// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{GenericMessage, GenericNetlinkFamily};
use crate::power::{
    ContainerWakingStream, SuspendResumeManagerHandle, WakeupSourceOrigin,
    create_proxy_for_wake_events_counter,
};
use starnix_logging::{log_debug, log_error, log_warn};

use anyhow::{Context as _, Error, bail, format_err};
use async_trait::async_trait;
use fidl::endpoints::{Proxy as _, RequestStream};
use fidl_fuchsia_wlan_wlanix::{self as fidl_wlanix, Nl80211Message, Nl80211MessageArray};
use fuchsia_async as fasync;
use futures::channel::mpsc;
use futures::{FutureExt as _, select_biased};
use netlink::NETLINK_LOG_TAG;
use netlink::messaging::Sender;
use netlink_packet_core::buffer::NETLINK_HEADER_LEN;
use netlink_packet_core::{
    DoneMessage, ErrorMessage, NetlinkHeader, NetlinkMessage, NetlinkPayload,
};
use netlink_packet_utils::Emitable;
use starnix_sync::{LockDepMutex, Nl80211ProxyLock, Nl80211UnicastWakeLock};
use starnix_uapi::errors;
use std::sync::Arc;

const INITIAL_RECONNECT_BACKOFF: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);
const MAX_RECONNECT_BACKOFF: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(5_000);
/// Duration to hold a Starnix wakeup source after receiving an `Nl80211Multicast`
/// message carrying a `wake_lease` baton, giving userspace daemons (such as
/// `wificond` and `system_server`) time to read the netlink socket and acquire
/// their own wakelocks before the container can suspend again.
///
/// Matches Linux `bcmdhd`'s `CUSTOM_PNO_EVENT_LOCK_xTIME * DHD_PACKET_TIMEOUT_MS`
/// (`10 * 100ms = 1000ms`).
const MULTICAST_WAKE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);
/// Maximum duration to hold a Starnix wakeup source for an in-flight unicast
/// `nl80211` request before timing out, so a hung `wlanix` FIDL call cannot
/// block container suspension indefinitely.
const UNICAST_WAKE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);
const UNICAST_WAKE_NAME: &str = "nl80211-unicast";

type ConnectWlanixFn =
    Box<dyn Fn() -> Result<fidl_wlanix::WlanixProxy, Error> + Send + Sync + 'static>;
type ProxyWakeChannelFn =
    Box<dyn Fn(zx::Channel, String) -> (zx::Channel, zx::Counter) + Send + Sync + 'static>;

struct ReconnectState {
    next_backoff: zx::MonotonicDuration,
    last_connected: fasync::MonotonicInstant,
}

impl ReconnectState {
    fn new() -> Self {
        Self {
            next_backoff: zx::MonotonicDuration::ZERO,
            last_connected: fasync::MonotonicInstant::INFINITE_PAST,
        }
    }

    fn maybe_reset_backoff(&mut self) {
        if fasync::MonotonicInstant::now() - self.last_connected >= MAX_RECONNECT_BACKOFF {
            self.next_backoff = zx::MonotonicDuration::ZERO;
        }
    }

    fn record_connected(&mut self) {
        self.last_connected = fasync::MonotonicInstant::now();
    }

    async fn backoff(&mut self) {
        let delay = self.next_backoff;
        self.next_backoff = if delay == zx::MonotonicDuration::ZERO {
            INITIAL_RECONNECT_BACKOFF
        } else {
            std::cmp::min(delay * 2, MAX_RECONNECT_BACKOFF)
        };
        if delay > zx::MonotonicDuration::ZERO {
            fasync::Timer::new(fasync::MonotonicInstant::after(delay)).await;
        }
    }
}

struct Nl80211FamilyInner {
    proxy: LockDepMutex<fidl_wlanix::Nl80211Proxy, Nl80211ProxyLock>,
    reconnect_state: futures::lock::Mutex<ReconnectState>,
    connect_wlanix_fn: ConnectWlanixFn,
    proxy_wake_channel_fn: ProxyWakeChannelFn,
    suspend_resume_manager: SuspendResumeManagerHandle,
    in_flight_unicast_requests: LockDepMutex<usize, Nl80211UnicastWakeLock>,
}

/// Reference-counted RAII guard that keeps the `nl80211-unicast` Starnix wakeup
/// source active while one or more unicast `nl80211` requests are in flight.
struct UnicastWakeGuard<'a> {
    inner: &'a Nl80211FamilyInner,
    timed_out: bool,
}

impl<'a> UnicastWakeGuard<'a> {
    fn acquire(inner: &'a Nl80211FamilyInner) -> Self {
        let mut in_flight = inner.in_flight_unicast_requests.lock();
        if *in_flight == 0 {
            inner
                .suspend_resume_manager
                .activate_wakeup_source(WakeupSourceOrigin::WakeLock(UNICAST_WAKE_NAME.into()));
        }
        *in_flight += 1;
        Self { inner, timed_out: false }
    }

    fn timeout(mut self) {
        self.timed_out = true;
    }
}

impl Drop for UnicastWakeGuard<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.inner.in_flight_unicast_requests.lock();
        *in_flight -= 1;
        if *in_flight == 0 {
            let origin = WakeupSourceOrigin::WakeLock(UNICAST_WAKE_NAME.into());
            if self.timed_out {
                self.inner.suspend_resume_manager.timeout_wakeup_source(&origin);
            } else {
                self.inner.suspend_resume_manager.deactivate_wakeup_source(&origin);
            }
        }
    }
}

#[derive(Clone)]
pub struct Nl80211Family {
    inner: Arc<Nl80211FamilyInner>,
}

impl Nl80211Family {
    pub fn new(suspend_resume_manager: SuspendResumeManagerHandle) -> Result<Self, Error> {
        Self::with_dependencies(
            suspend_resume_manager,
            || {
                fuchsia_component::client::connect_to_protocol::<fidl_wlanix::WlanixMarker>()
                    .context("failed to connect to wlanix")
            },
            create_proxy_for_wake_events_counter,
        )
    }

    fn with_dependencies(
        suspend_resume_manager: SuspendResumeManagerHandle,
        connect_wlanix_fn: impl Fn() -> Result<fidl_wlanix::WlanixProxy, Error> + Send + Sync + 'static,
        proxy_wake_channel_fn: impl Fn(zx::Channel, String) -> (zx::Channel, zx::Counter)
        + Send
        + Sync
        + 'static,
    ) -> Result<Self, Error> {
        let wlanix_svc = connect_wlanix_fn()?;
        let nl80211_proxy = Self::connect_proxy(&wlanix_svc)?;
        Ok(Self {
            inner: Arc::new(Nl80211FamilyInner {
                proxy: LockDepMutex::new(nl80211_proxy),
                reconnect_state: futures::lock::Mutex::new(ReconnectState::new()),
                connect_wlanix_fn: Box::new(connect_wlanix_fn),
                proxy_wake_channel_fn: Box::new(proxy_wake_channel_fn),
                suspend_resume_manager,
                in_flight_unicast_requests: LockDepMutex::new(0),
            }),
        })
    }

    fn connect_proxy(
        wlanix_svc: &fidl_wlanix::WlanixProxy,
    ) -> Result<fidl_wlanix::Nl80211Proxy, Error> {
        let (nl80211_proxy, nl80211_server) = fidl::endpoints::create_proxy();
        wlanix_svc
            .get_nl80211(fidl_wlanix::WlanixGetNl80211Request {
                nl80211: Some(nl80211_server),
                ..Default::default()
            })
            .map_err(|status| format_err!("Failed to get Nl80211 server with status {}", status))?;
        Ok(nl80211_proxy)
    }

    async fn reconnect_proxy(&self) -> Result<fidl_wlanix::Nl80211Proxy, Error> {
        let wlanix_svc = (self.inner.connect_wlanix_fn)()?;
        let nl80211_proxy = Self::connect_proxy(&wlanix_svc)?;
        // Use a pipelined Wifi::GetState call as a two-way barrier to confirm that
        // the restarted wlanix instance has finished initializing and is serving FIDL
        // requests before exposing the new Nl80211Proxy to handle_message.
        let (wifi_proxy, wifi_server) = fidl::endpoints::create_proxy::<fidl_wlanix::WifiMarker>();
        wlanix_svc
            .get_wifi(fidl_wlanix::WlanixGetWifiRequest {
                wifi: Some(wifi_server),
                ..Default::default()
            })
            .map_err(|status| format_err!("Failed to get Wifi server with status {}", status))?;
        wifi_proxy.get_state().await.context("Failed to call Wifi::GetState on wlanix")?;
        Ok(nl80211_proxy)
    }

    /// Returns the current `Nl80211Proxy` only if the channel is open.
    /// Because `proxy` is only replaced after `reconnect` completes, `proxy.is_closed()`
    /// remains true throughout the entire reconnection and backoff window.
    fn active_proxy(&self) -> Option<fidl_wlanix::Nl80211Proxy> {
        let proxy = self.inner.proxy.lock().clone();
        (!proxy.is_closed()).then_some(proxy)
    }

    async fn reconnect(&self) -> fidl_wlanix::Nl80211Proxy {
        let mut reconnect_state = self.inner.reconnect_state.lock().await;
        // All multicast group tasks call `reconnect()` when the proxy closes, but
        // `reconnect_state` serializes them: only the first task to acquire the lock
        // sees `None` here and reconnects, while the remaining multicast groups see
        // the newly connected proxy in `active_proxy()` and return immediately.
        if let Some(proxy) = self.active_proxy() {
            return proxy;
        }

        reconnect_state.maybe_reset_backoff();
        loop {
            reconnect_state.backoff().await;

            match self.reconnect_proxy().await {
                Ok(new_proxy) if !new_proxy.is_closed() => {
                    *self.inner.proxy.lock() = new_proxy.clone();
                    reconnect_state.record_connected();
                    return new_proxy;
                }
                Ok(_) => {
                    log_warn!(
                        tag = NETLINK_LOG_TAG;
                        "Reconnected Nl80211 proxy was immediately closed, backing off"
                    );
                }
                Err(e) => {
                    log_warn!(
                        tag = NETLINK_LOG_TAG;
                        "Failed to reconnect Nl80211 proxy: {:?}, backing off",
                        e
                    );
                }
            }
        }
    }
}

fn send_error_response<S: Sender<GenericMessage>>(
    mut header: NetlinkHeader,
    errno: errors::ErrnoCode,
    sender: &mut S,
) {
    let mut buffer = [0; NETLINK_HEADER_LEN];
    header.emit(&mut buffer[..NETLINK_HEADER_LEN]);
    let mut error = ErrorMessage::default();
    error.code = std::num::NonZeroI32::new(-(errno.error_code() as i32));
    error.header = buffer.to_vec();
    header.flags = netlink_packet_core::NLM_F_CAPPED;
    let mut netlink_message = NetlinkMessage::new(header, NetlinkPayload::Error(error));
    netlink_message.finalize();
    sender.send(netlink_message, None);
}

fn fidl_message_to_netlink(
    header: NetlinkHeader,
    message: fidl_wlanix::Nl80211Message,
) -> Result<NetlinkMessage<GenericMessage>, Error> {
    // TODO(https://fxbug.dev/42079282): Deduplicate these calls with similar calls used by rtnetlink.
    let payload = match message {
        fidl_wlanix::Nl80211Message::Message(m) => {
            NetlinkPayload::InnerMessage(GenericMessage::Other {
                family: header.message_type,
                payload: m.payload,
            })
        }
        fidl_wlanix::Nl80211Message::Ack(_) => {
            let mut buffer = [0; NETLINK_HEADER_LEN];
            header.emit(&mut buffer[..NETLINK_HEADER_LEN]);
            let mut ack = ErrorMessage::default();
            // Netlink uses an error payload with no error code to indicate a
            // successful ack.
            ack.code = None;
            ack.header = buffer.to_vec();
            NetlinkPayload::Error(ack)
        }
        fidl_wlanix::Nl80211Message::Done(_) => {
            let done = DoneMessage::default();
            NetlinkPayload::Done(done)
        }
        fidl_wlanix::Nl80211Message::Error(e) => {
            let mut buffer = [0; NETLINK_HEADER_LEN];
            header.emit(&mut buffer[..NETLINK_HEADER_LEN]);
            let mut error = ErrorMessage::default();
            let error_code = match e.error_code {
                0 => bail!("Dropping nl80211 error message with error code 0"),
                code => zx::Status::err_from_raw(code),
            };
            error.code = std::num::NonZeroI32::new(
                errors::from_status_like_fdio!(error_code).code.error_code() as i32,
            );
            error.header = buffer.to_vec();
            NetlinkPayload::Error(error)
        }
        other => bail!("Dropping unsupported nl80211 netlink protocol response type: {:?}", other),
    };
    let mut netlink_message = NetlinkMessage::new(header, payload);
    netlink_message.finalize();
    Ok(netlink_message)
}

fn deserialize(vmo: zx::Vmo) -> Result<Vec<Nl80211Message>, anyhow::Error> {
    let value = vmo.read_to_vec(0, vmo.get_content_size()?)?;
    Ok(fidl::unpersist::<Nl80211MessageArray>(&value)?.messages)
}

#[async_trait]
impl<S: Sender<GenericMessage>> GenericNetlinkFamily<S> for Nl80211Family {
    fn name(&self) -> String {
        "nl80211".into()
    }

    fn multicast_groups(&self) -> Vec<String> {
        vec![
            "scan".to_string(),
            "regulatory".to_string(),
            "mlme".to_string(),
            "vendor".to_string(),
            "config".to_string(),
        ]
    }

    async fn stream_multicast_messages(
        &self,
        group: String,
        assigned_family_id: u16,
        message_sink: mpsc::UnboundedSender<NetlinkMessage<GenericMessage>>,
    ) {
        let counter_name = format!("nl80211-multicast-{group}");
        let origin = WakeupSourceOrigin::WakeLock(counter_name.clone());
        let mut proxy = self.inner.proxy.lock().clone();

        while !message_sink.is_closed() {
            let (client_end, server_end) = fidl::endpoints::create_endpoints();
            if let Err(e) = proxy.get_multicast(fidl_wlanix::Nl80211GetMulticastRequest {
                group: Some(group.clone()),
                multicast: Some(client_end),
                ..Default::default()
            }) {
                if !e.is_closed() {
                    log_error!(
                        tag = NETLINK_LOG_TAG;
                        "Failed to request multicast stream for {}: {}",
                        group,
                        e,
                    );
                    return;
                }
            } else {
                let (local_channel, counter) = (self.inner.proxy_wake_channel_fn)(
                    server_end.into_channel(),
                    counter_name.clone(),
                );
                let mut stream = ContainerWakingStream::new(
                    self.inner
                        .suspend_resume_manager
                        .add_message_counter(&counter_name, Some(counter)),
                    fidl_wlanix::Nl80211MulticastRequestStream::from_channel(
                        fidl::AsyncChannel::from_channel(local_channel),
                    ),
                );
                let mut on_closed = std::pin::pin!(proxy.on_closed().fuse());
                let mut wake_timeout = std::pin::pin!(futures::future::Fuse::terminated());
                let mut next_req = stream.next().fuse();
                loop {
                    select_biased! {
                        req = next_req => {
                            match req {
                                Some(Ok(fidl_wlanix::Nl80211MulticastRequest::Message {
                                    payload,
                                    ..
                                })) => {
                                    if let Some(message) = payload.message {
                                        let mut header = NetlinkHeader::default();
                                        header.message_type = assigned_family_id;
                                        match fidl_message_to_netlink(header, message) {
                                            Ok(m) => {
                                                if let Err(e) = message_sink.unbounded_send(m) {
                                                    log_error!(
                                                        tag = NETLINK_LOG_TAG;
                                                        "Failed to send multicast message to handler: {}",
                                                        e,
                                                    );
                                                    if e.is_disconnected() {
                                                        self.inner
                                                            .suspend_resume_manager
                                                            .deactivate_wakeup_source(&origin);
                                                        return;
                                                    }
                                                } else if payload.wake_lease.is_some() {
                                                    self.inner
                                                        .suspend_resume_manager
                                                        .activate_wakeup_source(origin.clone());
                                                    wake_timeout.set(
                                                        fasync::Timer::new(
                                                            fasync::MonotonicInstant::after(
                                                                MULTICAST_WAKE_TIMEOUT,
                                                            ),
                                                        )
                                                        .fuse(),
                                                    );
                                                }
                                            }
                                            Err(e) => log_error!(
                                                tag = NETLINK_LOG_TAG;
                                                "Failed to forward multicast message: {}",
                                                e,
                                            ),
                                        }
                                    }
                                }
                                Some(Ok(unexpected)) => {
                                    log_error!(
                                        tag = NETLINK_LOG_TAG;
                                        "Received unexpected message from multicast protocol: {:?}",
                                        unexpected,
                                    );
                                }
                                Some(Err(e)) => {
                                    log_warn!(
                                        tag = NETLINK_LOG_TAG;
                                        "Nl80211 multicast stream {} returned error: {}",
                                        group,
                                        e
                                    );
                                    break;
                                }
                                None => break,
                            }
                            drop(next_req);
                            next_req = stream.next().fuse();
                        }
                        _ = wake_timeout => {
                            self.inner.suspend_resume_manager.timeout_wakeup_source(&origin);
                        }
                        _ = on_closed => break,
                    }
                }
                self.inner.suspend_resume_manager.deactivate_wakeup_source(&origin);
            }

            // Wait for the parent Nl80211 proxy to close as the restart signal.
            // For unsupported multicast groups ("vendor", "config"), wlanix drops
            // the multicast channel immediately while keeping the parent proxy open.
            let _ = proxy.on_closed().await;
            if message_sink.is_closed() {
                return;
            }

            log_warn!(
                tag = NETLINK_LOG_TAG;
                "Nl80211 proxy closed for multicast group {}, reconnecting",
                group
            );
            proxy = self.reconnect().await;
        }
    }

    async fn handle_message(
        &self,
        mut netlink_header: NetlinkHeader,
        payload: Vec<u8>,
        sender: &mut S,
    ) {
        log_debug!(
            tag = NETLINK_LOG_TAG;
            "Received nl80211 netlink protocol message: {:?}  -- {:?}",
            netlink_header,
            payload
        );
        let Some(proxy) = self.active_proxy() else {
            log_warn!(
                tag = NETLINK_LOG_TAG;
                "Rejecting nl80211 message while wlanix connection is closed or reconnecting"
            );
            send_error_response(netlink_header, errors::EIO, sender);
            return;
        };
        let mut wake_guard = Some(UnicastWakeGuard::acquire(&self.inner));
        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message { payload });
        let mut message_fut = proxy.message_v2(&message).fuse();
        let mut wake_timeout = std::pin::pin!(
            fasync::Timer::new(fasync::MonotonicInstant::after(UNICAST_WAKE_TIMEOUT)).fuse()
        );
        let message_res = loop {
            select_biased! {
                res = message_fut => break res,
                _ = wake_timeout => {
                    log_warn!(
                        tag = NETLINK_LOG_TAG;
                        "Unicast nl80211 request timed out holding wakeup source after {:?}",
                        UNICAST_WAKE_TIMEOUT,
                    );
                    if let Some(guard) = wake_guard.take() {
                        guard.timeout();
                    }
                }
            }
        };
        let vmo = match message_res {
            Ok(Ok(value)) => value,
            Ok(Err(errno)) => {
                log_error!(tag = NETLINK_LOG_TAG; "Failed to read nl80211 message: {}", errno);
                let err = errors::from_status_like_fdio!(zx::Status::err_from_raw(errno)).code;
                send_error_response(netlink_header, err, sender);
                return;
            }
            Err(fidl_error) => {
                log_error!(
                    tag = NETLINK_LOG_TAG;
                    "Failed to read nl80211 message due to FIDL error: {:?}",
                    fidl_error
                );
                send_error_response(netlink_header, errors::EIO, sender);
                return;
            }
        };
        let responses = match deserialize(vmo) {
            Ok(value) => value,
            Err(error) => {
                log_error!(
                    tag = NETLINK_LOG_TAG;
                    "Failed to read nl80211 message from VMO: {:?}",
                    error
                );
                send_error_response(netlink_header, errors::EIO, sender);
                return;
            }
        };

        // It's not valid to send multiple messages with the same sequence
        // number unless the multipart flag is set.
        if responses.len() > 1 {
            netlink_header.flags |= netlink_packet_core::NLM_F_MULTIPART;
        }
        for resp_message in responses {
            match fidl_message_to_netlink(netlink_header, resp_message) {
                Ok(resp) => sender.send(resp, None),
                Err(e) => {
                    log_error!(tag = NETLINK_LOG_TAG; "Failed to send response message: {}", e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::SuspendResumeManager;
    use crate::vfs::socket::socket_generic_netlink::test_utils::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_async::TestExecutor;
    use futures::{FutureExt, StreamExt, pin_mut};
    use starnix_sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Poll;
    use test_case::test_case;

    struct TestHarness {
        family: Nl80211Family,
        suspend_resume_manager: SuspendResumeManagerHandle,
        wlanix_rx: Mutex<mpsc::UnboundedReceiver<fidl_wlanix::WlanixRequestStream>>,
        connect_calls: Arc<AtomicUsize>,
        fail_connect: Arc<AtomicBool>,
    }

    impl TestHarness {
        fn new() -> (Self, fidl_wlanix::Nl80211RequestStream) {
            let (wlanix_tx, mut wlanix_rx) = mpsc::unbounded();
            let connect_calls = Arc::new(AtomicUsize::new(0));
            let connect_calls_clone = Arc::clone(&connect_calls);
            let fail_connect = Arc::new(AtomicBool::new(false));
            let fail_connect_clone = Arc::clone(&fail_connect);
            let suspend_resume_manager = Arc::new(SuspendResumeManager::default());

            let family = Nl80211Family::with_dependencies(
                suspend_resume_manager.clone(),
                move || {
                    connect_calls_clone.fetch_add(1, Ordering::SeqCst);
                    if fail_connect_clone.load(Ordering::SeqCst) {
                        return Err(format_err!("simulated connect_to_protocol error"));
                    }
                    let (wlanix_proxy, wlanix_stream) =
                        create_proxy_and_stream::<fidl_wlanix::WlanixMarker>();
                    wlanix_tx.unbounded_send(wlanix_stream).expect("wlanix_rx dropped");
                    Ok(wlanix_proxy)
                },
                |channel, _name| (channel, zx::Counter::create()),
            )
            .expect("Failed to create test family");

            let mut wlanix_stream =
                wlanix_rx.try_recv().expect("Missing initial WlanixRequestStream");
            let nl80211_stream = assert_matches!(
                wlanix_stream.next().now_or_never(),
                Some(Some(Ok(fidl_wlanix::WlanixRequest::GetNl80211 { payload, .. }))) => {
                    payload.nl80211.expect("Missing nl80211 server end").into_stream()
                }
            );
            (
                Self {
                    family,
                    suspend_resume_manager,
                    wlanix_rx: Mutex::new(wlanix_rx),
                    connect_calls,
                    fail_connect,
                },
                nl80211_stream,
            )
        }

        fn connect_calls(&self) -> usize {
            self.connect_calls.load(Ordering::SeqCst)
        }

        fn set_fail_connect(&self, fail: bool) {
            self.fail_connect.store(fail, Ordering::SeqCst);
        }

        async fn next_reconnect_request(
            &self,
        ) -> (fidl_wlanix::Nl80211RequestStream, fidl_wlanix::WifiGetStateResponder) {
            let mut wlanix_stream =
                self.wlanix_rx.lock().try_recv().expect("Missing WlanixRequestStream");
            let nl80211_stream = assert_matches!(
                wlanix_stream.next().await,
                Some(Ok(fidl_wlanix::WlanixRequest::GetNl80211 { payload, .. })) => {
                    payload.nl80211.expect("Missing nl80211 server end").into_stream()
                }
            );
            let mut wifi_stream = assert_matches!(
                wlanix_stream.next().await,
                Some(Ok(fidl_wlanix::WlanixRequest::GetWifi { payload, .. })) => {
                    payload.wifi.expect("Missing wifi server end").into_stream()
                }
            );
            let responder = assert_matches!(
                wifi_stream.next().await,
                Some(Ok(fidl_wlanix::WifiRequest::GetState { responder })) => responder
            );
            (nl80211_stream, responder)
        }

        async fn succeed_reconnect(&self) -> fidl_wlanix::Nl80211RequestStream {
            let (nl80211_stream, responder) = self.next_reconnect_request().await;
            responder
                .send(&fidl_wlanix::WifiGetStateResponse {
                    is_started: Some(false),
                    ..Default::default()
                })
                .expect("Failed to send GetState response");
            nl80211_stream
        }
    }

    #[test]
    fn test_fidl_message_to_netlink() {
        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 10;
        let fidl_message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });

        let netlink_message = fidl_message_to_netlink(header, fidl_message)
            .expect("Netlink message conversion failed");
        assert_eq!(netlink_message.header.message_type, 123);
        assert_eq!(netlink_message.header.sequence_number, 10);
        let payload = assert_matches!(
            netlink_message.payload,
            NetlinkPayload::InnerMessage(GenericMessage::Other {
                payload,
                ..
            }) => payload
        );
        assert_eq!(payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_fidl_ack_to_netlink() {
        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 10;
        let fidl_message = fidl_wlanix::Nl80211Message::Ack(fidl_wlanix::Ack);

        let netlink_message = fidl_message_to_netlink(header, fidl_message)
            .expect("Netlink message conversion failed");
        assert_eq!(netlink_message.header.message_type, netlink_packet_core::NLMSG_ERROR);
        assert_eq!(netlink_message.header.sequence_number, 10);
        assert_matches!(netlink_message.payload, NetlinkPayload::Error(_));
    }

    #[test]
    fn test_fidl_error_to_netlink() {
        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 10;
        let fidl_message = fidl_wlanix::Nl80211Message::Error(fidl_wlanix::Error {
            error_code: zx::sys::ZX_ERR_SHOULD_WAIT,
        });

        let netlink_message = fidl_message_to_netlink(header, fidl_message)
            .expect("Netlink message conversion failed");
        assert_eq!(netlink_message.header.message_type, netlink_packet_core::NLMSG_ERROR);
        assert_eq!(netlink_message.header.sequence_number, 10);
        let error = assert_matches!(netlink_message.payload, NetlinkPayload::Error(e) => e);
        assert_eq!(
            error.code.expect("Missing error code").get(),
            errors::EAGAIN.error_code() as i32
        );
    }

    #[test]
    fn test_get_multicast() {
        let mut exec = TestExecutor::new();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let (sender, mut receiver) = mpsc::unbounded();
        let mcast_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "test_group".to_string(),
                123,
                sender,
            );
        pin_mut!(mcast_fut);

        let next_mcast_recv = receiver.select_next_some();
        pin_mut!(next_mcast_recv);

        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        let multicast_req = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, ..})) => payload);
        assert_eq!(multicast_req.group, Some("test_group".to_string()));
        let client_proxy: fidl_wlanix::Nl80211MulticastProxy =
            multicast_req.multicast.expect("No client endpoint").into_proxy();
        assert_matches!(exec.run_until_stalled(&mut next_mcast_recv), Poll::Pending);

        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });
        client_proxy
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message),
                ..Default::default()
            })
            .expect("Failed to send multicast message");
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);

        assert_matches!(exec.run_until_stalled(&mut next_mcast_recv), Poll::Ready(_));
    }

    #[test]
    fn test_multicast_reconnects_on_proxy_closed_and_deduplicates() {
        let mut exec = TestExecutor::new();
        let (harness, mut stream1) = TestHarness::new();
        assert_eq!(harness.connect_calls(), 1);

        let (scan_sender, mut scan_receiver) = mpsc::unbounded();
        let scan_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                scan_sender,
            );
        pin_mut!(scan_fut);

        let (vendor_sender, _vendor_receiver) = mpsc::unbounded();
        let vendor_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "vendor".to_string(),
                123,
                vendor_sender,
            );
        pin_mut!(vendor_fut);

        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_matches!(exec.run_until_stalled(&mut vendor_fut), Poll::Pending);

        let req1_scan = assert_matches!(
            exec.run_until_stalled(&mut stream1.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let req1_vendor = assert_matches!(
            exec.run_until_stalled(&mut stream1.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let scan_client_1 = req1_scan.multicast.expect("No scan client").into_proxy();
        // Simulate wlanix dropping unsupported "vendor" multicast channel while keeping stream1 open.
        drop(req1_vendor.multicast);
        assert_matches!(exec.run_until_stalled(&mut vendor_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 1);

        // Simulate wlanix dropping the parent Nl80211 stream1 first (as in Lifecycle::Stop)
        // while keeping scan_client_1 alive in WifiState.
        drop(stream1);

        // Both multicast futures wake up on proxy.on_closed() and contend on reconnect_state.
        // Only the first calls connect_wlanix_fn and blocks on Wifi::GetState; the second waits
        // on reconnect_state and reuses the reconnected proxy.
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_matches!(exec.run_until_stalled(&mut vendor_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 2);

        let Poll::Ready(mut stream2) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.succeed_reconnect()))
        else {
            panic!("Expected succeed_reconnect to complete");
        };
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_matches!(exec.run_until_stalled(&mut vendor_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 2);
        drop(scan_client_1);

        let req2_scan = assert_matches!(
            exec.run_until_stalled(&mut stream2.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let req2_vendor = assert_matches!(
            exec.run_until_stalled(&mut stream2.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        assert_eq!(req2_scan.group, Some("scan".to_string()));
        assert_eq!(req2_vendor.group, Some("vendor".to_string()));

        let scan_client_2 = req2_scan.multicast.expect("No scan client 2").into_proxy();
        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![9, 8, 7, 6],
        });
        scan_client_2
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message),
                ..Default::default()
            })
            .expect("Failed to send scan multicast");

        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_matches!(
            exec.run_until_stalled(&mut scan_receiver.select_next_some()),
            Poll::Ready(_)
        );
    }

    #[test]
    fn test_handle_message_fast_fails_during_reconnect_and_succeeds_after() {
        let mut exec = TestExecutor::new();
        let (harness, mut stream1) = TestHarness::new();

        let (scan_sender, _scan_receiver) = mpsc::unbounded();
        let scan_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                scan_sender,
            );
        pin_mut!(scan_fut);
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        let req1_scan = assert_matches!(
            exec.run_until_stalled(&mut stream1.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        drop(req1_scan.multicast);
        drop(stream1);

        // Poll scan_fut so it detects proxy closure and blocks on the Wifi::GetState barrier.
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // While reconnect is waiting on Wifi::GetState, handle_message should fast-fail with EIO.
        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 42;
        let messages = Arc::new(Mutex::new(vec![]));
        let mut sender = TestSender { messages: messages.clone() };
        {
            let handle_fut = GenericNetlinkFamily::handle_message(
                &harness.family,
                header,
                vec![1, 2, 3, 4],
                &mut sender,
            );
            pin_mut!(handle_fut);
            assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        }
        assert_eq!(messages.lock().len(), 1);
        let err_msg = assert_matches!(
            &messages.lock()[0].payload,
            NetlinkPayload::Error(err) => err.clone()
        );
        assert_eq!(
            err_msg.code.expect("Missing error code").get(),
            -(errors::EIO.error_code() as i32)
        );
        messages.lock().clear();

        // Complete the Wifi::GetState barrier and verify handle_message now succeeds on stream2.
        let Poll::Ready(mut stream2) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.succeed_reconnect()))
        else {
            panic!("Expected succeed_reconnect to complete");
        };
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        let req2_scan = assert_matches!(
            exec.run_until_stalled(&mut stream2.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        assert_eq!(req2_scan.group.as_deref(), Some("scan"));
        let _server_end2_scan = req2_scan.multicast.expect("Missing multicast client end");

        let handle_fut = GenericNetlinkFamily::handle_message(
            &harness.family,
            header,
            vec![1, 2, 3, 4],
            &mut sender,
        );
        pin_mut!(handle_fut);
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);

        let responder = assert_matches!(
            exec.run_until_stalled(&mut stream2.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { message: _, responder })) => responder
        );
        let response_array = Nl80211MessageArray {
            messages: vec![fidl_wlanix::Nl80211Message::Ack(fidl_wlanix::Ack)],
        };
        let bytes = fidl::persist(&response_array).expect("Failed to persist response");
        let vmo = zx::Vmo::create(bytes.len() as u64).expect("Failed to create VMO");
        vmo.write(&bytes, 0).expect("Failed to write VMO");
        vmo.set_content_size(&(bytes.len() as u64)).expect("Failed to set VMO content size");
        responder.send(Ok(vmo)).expect("Failed to send response");

        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        assert_eq!(messages.lock().len(), 1);
        let ack_msg = assert_matches!(
            &messages.lock()[0].payload,
            NetlinkPayload::Error(ack) => ack.clone()
        );
        assert!(ack_msg.code.is_none());
    }

    #[test]
    fn test_multicast_reconnect_backoff() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, stream1) = TestHarness::new();
        drop(stream1);
        assert_eq!(harness.connect_calls(), 1);

        let (scan_sender, _scan_receiver) = mpsc::unbounded();
        let scan_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                scan_sender,
            );
        pin_mut!(scan_fut);

        // Initial reconnect attempt runs immediately (0ms delay); fail it by dropping GetState
        // so it arms the 100ms timer.
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 2);
        let Poll::Ready((_nl80211_stream, responder)) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.next_reconnect_request()))
        else {
            panic!("Expected next_reconnect_request to complete");
        };
        drop(responder);
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // 8 failed reconnect attempts (1 GetState drop + 6 connect Errs + 1 closed Nl80211 channel)
        // followed by a succeeding reconnect exercises:
        // 0ms -> 100ms -> 200ms -> 400ms -> 800ms -> 1600ms -> 3200ms -> 5000ms -> 5000ms (capped).
        let expected_delays_ms = [100, 200, 400, 800, 1600, 3200, 5000, 5000];
        harness.set_fail_connect(true);
        for (i, delay_ms) in expected_delays_ms.into_iter().enumerate() {
            if i == 6 {
                harness.set_fail_connect(false);
            }
            exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(delay_ms - 1));
            assert!(!exec.wake_expired_timers());
            assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
            assert_eq!(harness.connect_calls(), i + 2);

            exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(1));
            assert!(exec.wake_expired_timers());
            assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
            assert_eq!(harness.connect_calls(), i + 3);

            if i == 6 {
                let Poll::Ready((nl80211_stream, responder)) =
                    exec.run_until_stalled(&mut std::pin::pin!(harness.next_reconnect_request()))
                else {
                    panic!("Expected next_reconnect_request to complete");
                };
                drop(nl80211_stream);
                responder
                    .send(&fidl_wlanix::WifiGetStateResponse {
                        is_started: Some(false),
                        ..Default::default()
                    })
                    .expect("Failed to send GetState response");
                assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
            }
        }

        let Poll::Ready(mut stream_short_lived) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.succeed_reconnect()))
        else {
            panic!("Expected succeed_reconnect to complete");
        };
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        let req_short = assert_matches!(
            exec.run_until_stalled(&mut stream_short_lived.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        drop(req_short.multicast);

        // Dropping the connection before MAX_RECONNECT_BACKOFF (5s) has elapsed preserves the
        // 5000ms backoff rather than resetting to 0ms.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(1000));
        drop(stream_short_lived);
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 10);

        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(4999));
        assert!(!exec.wake_expired_timers());
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(1));
        assert!(exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 11);

        let Poll::Ready(mut stream_after_short_lived) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.succeed_reconnect()))
        else {
            panic!("Expected succeed_reconnect to complete");
        };
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        let req_after_short = assert_matches!(
            exec.run_until_stalled(&mut stream_after_short_lived.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        drop(req_after_short.multicast);

        // Staying connected for at least MAX_RECONNECT_BACKOFF (5s) resets the next reconnect
        // attempt back to 0ms.
        exec.set_fake_time(exec.now() + MAX_RECONNECT_BACKOFF);
        drop(stream_after_short_lived);
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        assert_eq!(harness.connect_calls(), 12);

        let Poll::Ready(mut stream_final) =
            exec.run_until_stalled(&mut std::pin::pin!(harness.succeed_reconnect()))
        else {
            panic!("Expected succeed_reconnect to complete");
        };
        assert_matches!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);
        let req_final = assert_matches!(
            exec.run_until_stalled(&mut stream_final.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        assert_eq!(req_final.group, Some("scan".to_string()));
    }

    #[test_case(Some(Err(zx::sys::ZX_ERR_NOT_SUPPORTED)), errors::EOPNOTSUPP; "fidl_error_status")]
    #[test_case(Some(Ok(&[0xff, 0xee, 0xdd])), errors::EIO; "invalid_fidl_vmo_bytes")]
    #[test_case(None, errors::EIO; "channel_closed")]
    fn test_handle_message_error_responses(
        server_response: Option<Result<&'static [u8], zx::sys::zx_status_t>>,
        expected_errno: errors::ErrnoCode,
    ) {
        let mut exec = TestExecutor::new();
        let (harness, mut stream) = TestHarness::new();

        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 7;
        header.flags = netlink_packet_core::NLM_F_REQUEST | netlink_packet_core::NLM_F_ACK;
        let mut expected_header_bytes = [0; NETLINK_HEADER_LEN];
        header.emit(&mut expected_header_bytes[..NETLINK_HEADER_LEN]);

        let messages = Arc::new(Mutex::new(vec![]));
        let mut sender = TestSender { messages: messages.clone() };

        {
            let handle_fut = GenericNetlinkFamily::handle_message(
                &harness.family,
                header,
                vec![1, 2, 3, 4],
                &mut sender,
            );
            pin_mut!(handle_fut);
            assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);

            match server_response {
                Some(Err(status)) => {
                    let responder = assert_matches!(
                        exec.run_until_stalled(&mut stream.select_next_some()),
                        Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 {
                            message: _,
                            responder,
                        })) => responder
                    );
                    responder.send(Err(status)).expect("Failed to send error");
                }
                Some(Ok(vmo_bytes)) => {
                    let responder = assert_matches!(
                        exec.run_until_stalled(&mut stream.select_next_some()),
                        Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 {
                            message: _,
                            responder,
                        })) => responder
                    );
                    let vmo =
                        zx::Vmo::create(vmo_bytes.len() as u64).expect("Failed to create VMO");
                    vmo.write(vmo_bytes, 0).expect("Failed to write VMO");
                    vmo.set_content_size(&(vmo_bytes.len() as u64))
                        .expect("Failed to set content size");
                    responder.send(Ok(vmo)).expect("Failed to send VMO");
                }
                None => {
                    drop(stream);
                }
            }
            assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        }

        assert_eq!(messages.lock().len(), 1);
        assert_eq!(messages.lock()[0].header.sequence_number, 7);
        assert_eq!(messages.lock()[0].header.flags, netlink_packet_core::NLM_F_CAPPED);
        let err_msg = assert_matches!(
            &messages.lock()[0].payload,
            NetlinkPayload::Error(err) => err.clone()
        );
        assert_eq!(
            err_msg.code.expect("Missing error code").get(),
            -(expected_errno.error_code() as i32)
        );
        assert_eq!(err_msg.header, expected_header_bytes);
    }

    // Verifies that receiving a multicast message with a `wake_lease` baton:
    // 1. Forwards the netlink message to the sink, activates the Starnix wakeup source, and
    //    immediately drops the `wake_lease` baton (`EVENTPAIR_PEER_CLOSED`).
    // 2. Resets the 1s `MULTICAST_WAKE_TIMEOUT` timer when a subsequent message with a
    //    `wake_lease` arrives before the first timeout expires.
    // 3. Deactivates the wakeup source once `MULTICAST_WAKE_TIMEOUT` elapses after the last
    //    `wake_lease`-bearing message.
    #[::fuchsia::test]
    fn test_multicast_activates_timed_wakeup_source_and_extends_on_subsequent_baton() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let (sender, mut receiver) = mpsc::unbounded();
        let mcast_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                sender,
            );
        pin_mut!(mcast_fut);

        // Start the multicast stream and verify no wakeup source is active initially.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(harness.suspend_resume_manager.lock().can_suspend());

        // Extract the `Nl80211Multicast` client endpoint sent to wlanix via `GetMulticast`.
        let multicast_req = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let client_proxy: fidl_wlanix::Nl80211MulticastProxy =
            multicast_req.multicast.expect("No client endpoint").into_proxy();

        // Send a multicast message carrying a `wake_lease` baton at t = 0ms.
        let (server_lease_1, client_lease_1) = zx::EventPair::create();
        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![1, 2, 3, 4],
        });
        client_proxy
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message.clone()),
                wake_lease: Some(client_lease_1),
                ..Default::default()
            })
            .expect("Failed to send multicast message");

        // Poll `mcast_fut` to process the message: the netlink payload should be queued on
        // `receiver`, `client_lease_1` should be dropped (`EVENTPAIR_PEER_CLOSED` observed on
        // `server_lease_1`), and the wakeup source should block suspension.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(receiver.try_recv().is_ok());
        assert_matches!(
            server_lease_1
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(signals) if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // Advance 400ms (still within the 1s timeout); the wakeup source remains active.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(400));
        assert!(!exec.wake_expired_timers());
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // Send a second multicast message with a new `wake_lease` baton at t = 400ms to extend
        // the wakeup timeout by another 1s (until t = 1400ms).
        let (server_lease_2, client_lease_2) = zx::EventPair::create();
        client_proxy
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message),
                wake_lease: Some(client_lease_2),
                ..Default::default()
            })
            .expect("Failed to send second multicast message");

        // Verify the second message is forwarded, its baton is dropped, and the wakeup source
        // remains active.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(receiver.try_recv().is_ok());
        assert_matches!(
            server_lease_2
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(signals) if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // At t = 1000ms (when the first message's timer would have expired, 600ms after the
        // second message), the extended timeout has not yet expired.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(600));
        assert!(!exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // At t = 1400ms (1s after the second message), the timeout expires and deactivates the
        // wakeup source, allowing the container to suspend again.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(400));
        assert!(exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    // Verifies that a multicast message without a `wake_lease` (`wake_lease: None`) is still
    // forwarded to the netlink sink, but does not activate a timed wakeup source.
    #[::fuchsia::test]
    fn test_multicast_without_wake_lease_does_not_activate_wakeup_source() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let (sender, mut receiver) = mpsc::unbounded();
        let mcast_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                sender,
            );
        pin_mut!(mcast_fut);

        // Start the multicast stream and obtain the `Nl80211Multicast` client proxy.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        let multicast_req = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let client_proxy: fidl_wlanix::Nl80211MulticastProxy =
            multicast_req.multicast.expect("No client endpoint").into_proxy();

        // Send a multicast message with `wake_lease: None`.
        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![1, 2, 3, 4],
        });
        client_proxy
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message),
                wake_lease: None,
                ..Default::default()
            })
            .expect("Failed to send multicast message");

        // The message is forwarded to `receiver`, and the container remains able to suspend.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(receiver.try_recv().is_ok());
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    // Verifies that if the multicast stream closes while a timed wakeup source is active, the
    // wakeup source is immediately deactivated rather than leaking or waiting for the 1s timer.
    #[::fuchsia::test]
    fn test_multicast_deactivates_wakeup_source_on_stream_close() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let (sender, mut receiver) = mpsc::unbounded();
        let mcast_fut =
            GenericNetlinkFamily::<TestSender<GenericMessage>>::stream_multicast_messages(
                &harness.family,
                "scan".to_string(),
                123,
                sender,
            );
        pin_mut!(mcast_fut);

        // Start the multicast stream and obtain the `Nl80211Multicast` client proxy.
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        let multicast_req = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::GetMulticast { payload, .. })) => payload
        );
        let client_proxy: fidl_wlanix::Nl80211MulticastProxy =
            multicast_req.multicast.expect("No client endpoint").into_proxy();

        // Send a message with a `wake_lease` baton to activate the timed wakeup source.
        let (_server_lease, client_lease) = zx::EventPair::create();
        let message = fidl_wlanix::Nl80211Message::Message(fidl_wlanix::Message {
            payload: vec![1, 2, 3, 4],
        });
        client_proxy
            .message(fidl_wlanix::Nl80211MulticastMessageRequest {
                message: Some(message),
                wake_lease: Some(client_lease),
                ..Default::default()
            })
            .expect("Failed to send multicast message");

        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(receiver.try_recv().is_ok());
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // Dropping the multicast client proxy closes the stream before the 1s timer expires,
        // which should immediately deactivate the wakeup source.
        drop(client_proxy);
        assert_matches!(exec.run_until_stalled(&mut mcast_fut), Poll::Pending);
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    fn serialize_messages_to_vmo(messages: Vec<Nl80211Message>) -> zx::Vmo {
        let bytes =
            fidl::persist(&Nl80211MessageArray { messages }).expect("Failed to persist messages");
        let vmo = zx::Vmo::create(bytes.len() as u64).expect("Failed to create VMO");
        vmo.write(&bytes, 0).expect("Failed to write VMO");
        vmo.set_content_size(&(bytes.len() as u64)).expect("Failed to set VMO content size");
        vmo
    }

    // Verifies that an in-flight unicast `handle_message` call activates the Starnix wakeup
    // source (blocking container suspension) until all reply messages are enqueued on the sender.
    #[::fuchsia::test]
    fn test_unicast_holds_wakeup_source_until_replies_queued() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        assert!(harness.suspend_resume_manager.lock().can_suspend());

        let mut header = NetlinkHeader::default();
        header.message_type = 123;
        header.sequence_number = 42;
        let messages = Arc::new(Mutex::new(vec![]));
        let mut sender = TestSender { messages: messages.clone() };

        let handle_fut = GenericNetlinkFamily::handle_message(
            &harness.family,
            header,
            vec![1, 2, 3, 4],
            &mut sender,
        );
        pin_mut!(handle_fut);

        // Polling `handle_fut` dispatches `MessageV2` to wlanix and holds the wakeup source.
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        let responder = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { responder, .. })) => responder
        );
        let vmo = serialize_messages_to_vmo(vec![
            Nl80211Message::Message(fidl_wlanix::Message { payload: vec![9, 8, 7, 6] }),
            Nl80211Message::Done(fidl_wlanix::Done),
        ]);
        responder.send(Ok(vmo)).expect("Failed to send VMO response");

        // Once `handle_fut` processes the reply and enqueues both messages, the wakeup source is
        // deactivated.
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        assert_eq!(messages.lock().len(), 2);
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    // Verifies that overlapping in-flight unicast requests reference-count the wakeup source so
    // completing the first request does not allow suspension while the second request is still
    // waiting for wlanix.
    #[::fuchsia::test]
    fn test_unicast_overlapping_requests_refcount_wakeup_source() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();

        let messages_1 = Arc::new(Mutex::new(vec![]));
        let mut sender_1 = TestSender { messages: messages_1.clone() };
        let messages_2 = Arc::new(Mutex::new(vec![]));
        let mut sender_2 = TestSender { messages: messages_2.clone() };

        let handle_fut_1 = GenericNetlinkFamily::handle_message(
            &harness.family,
            NetlinkHeader::default(),
            vec![1],
            &mut sender_1,
        );
        let handle_fut_2 = GenericNetlinkFamily::handle_message(
            &harness.family,
            NetlinkHeader::default(),
            vec![2],
            &mut sender_2,
        );
        pin_mut!(handle_fut_1);
        pin_mut!(handle_fut_2);

        // Start both requests concurrently; both are now waiting for `MessageV2` replies.
        assert_matches!(exec.run_until_stalled(&mut handle_fut_1), Poll::Pending);
        assert_matches!(exec.run_until_stalled(&mut handle_fut_2), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        let responder_1 = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { responder, .. })) => responder
        );
        let responder_2 = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { responder, .. })) => responder
        );

        // Complete the first request; the wakeup source must remain active for the second request.
        responder_1
            .send(Ok(serialize_messages_to_vmo(vec![Nl80211Message::Ack(fidl_wlanix::Ack)])))
            .expect("Failed to send first response");
        assert_matches!(exec.run_until_stalled(&mut handle_fut_1), Poll::Ready(()));
        assert_eq!(messages_1.lock().len(), 1);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // Complete the second request; now the wakeup source is released.
        responder_2
            .send(Ok(serialize_messages_to_vmo(vec![Nl80211Message::Ack(fidl_wlanix::Ack)])))
            .expect("Failed to send second response");
        assert_matches!(exec.run_until_stalled(&mut handle_fut_2), Poll::Ready(()));
        assert_eq!(messages_2.lock().len(), 1);
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    // Verifies that if an in-flight unicast request future is dropped (e.g. client socket closed)
    // or if the wlanix channel closes mid-request, the wakeup source is released.
    #[::fuchsia::test]
    fn test_unicast_releases_wakeup_source_on_drop_or_channel_close() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let messages = Arc::new(Mutex::new(vec![]));
        let mut sender = TestSender { messages };

        // 1. Dropping the `handle_message` future mid-flight releases the wakeup source.
        {
            let mut handle_fut = Box::pin(GenericNetlinkFamily::handle_message(
                &harness.family,
                NetlinkHeader::default(),
                vec![1, 2],
                &mut sender,
            ));
            assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
            assert!(!harness.suspend_resume_manager.lock().can_suspend());
            let _responder = assert_matches!(
                exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
                Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { responder, .. })) => responder
            );
        }
        assert!(harness.suspend_resume_manager.lock().can_suspend());

        // 2. Closing the wlanix `Nl80211` channel mid-request returns an error reply and releases
        //    the wakeup source.
        let handle_fut = GenericNetlinkFamily::handle_message(
            &harness.family,
            NetlinkHeader::default(),
            vec![3, 4],
            &mut sender,
        );
        pin_mut!(handle_fut);
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        drop(nl80211_stream);
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        assert!(harness.suspend_resume_manager.lock().can_suspend());
    }

    // Verifies that if wlanix does not reply within `UNICAST_WAKE_TIMEOUT` (5s), the safety timer
    // releases the wakeup source so a hung FIDL call cannot block suspension indefinitely, and a
    // late reply does not underflow the reference count.
    #[::fuchsia::test]
    fn test_unicast_safety_timeout_releases_wakeup_source() {
        let mut exec = TestExecutor::new_with_fake_time();
        let (harness, mut nl80211_stream) = TestHarness::new();
        let messages = Arc::new(Mutex::new(vec![]));
        let mut sender = TestSender { messages: messages.clone() };

        let handle_fut = GenericNetlinkFamily::handle_message(
            &harness.family,
            NetlinkHeader::default(),
            vec![1, 2, 3, 4],
            &mut sender,
        );
        pin_mut!(handle_fut);

        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        let responder = assert_matches!(
            exec.run_until_stalled(&mut nl80211_stream.select_next_some()),
            Poll::Ready(Ok(fidl_wlanix::Nl80211Request::MessageV2 { responder, .. })) => responder
        );

        // Just before `UNICAST_WAKE_TIMEOUT` (5s), the wakeup source is still held.
        exec.set_fake_time(
            exec.now() + UNICAST_WAKE_TIMEOUT - zx::MonotonicDuration::from_millis(1),
        );
        assert!(!exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
        assert!(!harness.suspend_resume_manager.lock().can_suspend());

        // At `UNICAST_WAKE_TIMEOUT` (5s), the timer fires and releases the wakeup source while
        // `handle_fut` remains pending waiting for `MessageV2`.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_millis(1));
        assert!(exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Pending);
        assert!(harness.suspend_resume_manager.lock().can_suspend());

        // When the late reply arrives, `handle_fut` completes normally without double-decrementing
        // `in_flight_unicast_requests`.
        responder
            .send(Ok(serialize_messages_to_vmo(vec![Nl80211Message::Ack(fidl_wlanix::Ack)])))
            .expect("Failed to send late response");
        assert_matches!(exec.run_until_stalled(&mut handle_fut), Poll::Ready(()));
        assert_eq!(messages.lock().len(), 1);
        assert!(harness.suspend_resume_manager.lock().can_suspend());
        assert_eq!(*harness.family.inner.in_flight_unicast_requests.lock(), 0);
    }
}
