// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{RequestStream, ServerEnd};
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_function as fusb_function;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use zx::{Status, Vmo};

#[derive(Debug, Clone, PartialEq)]
pub enum MockEvent {
    AllocResources { interface_nums: Vec<u8>, endpoints: Vec<u8> },
    Configure { descriptors: Vec<u8> },
    Deconfigure,
    ConfigureEndpoint { ep_addr: u8 },
    DisableEndpoint { ep_addr: u8 },
    SetConfiguredSend { configured: bool },
    SetConfiguredReply { configured: bool },
    SetInterfaceSend { interface: u8, alt_setting: u8 },
    SetInterfaceReply { interface: u8, alt_setting: u8 },
    CancelAll { ep_addr: u8 },
}

#[derive(Debug)]
pub struct MockEndpointState {
    pub requests: Vec<fusb_request::Request>,
    pub vmos: HashMap<u64, Vmo>,
}

#[derive(Debug)]
pub struct MockEndpoint {
    pub addr: u8,
    pub direction: fusb_descriptor::EndpointDirection,
    pub state: Arc<Mutex<MockEndpointState>>,
    completion_tx: mpsc::UnboundedSender<Vec<fusb_endpoint::Completion>>,
    completion_rx: Option<mpsc::UnboundedReceiver<Vec<fusb_endpoint::Completion>>>,
    scope: fasync::Scope,
}

impl MockEndpoint {
    pub fn new(addr: u8, direction: fusb_descriptor::EndpointDirection) -> Self {
        let (completion_tx, completion_rx) = mpsc::unbounded();
        Self {
            addr,
            direction,
            state: Arc::new(Mutex::new(MockEndpointState {
                requests: Vec::new(),
                vmos: HashMap::new(),
            })),
            completion_tx,
            completion_rx: Some(completion_rx),
            scope: fasync::Scope::new_with_name("mock_endpoint"),
        }
    }

    pub fn start(
        &mut self,
        server_end: ServerEnd<fusb_endpoint::EndpointMarker>,
        global_state: Option<Arc<Mutex<MockUsbFunctionState>>>,
    ) {
        let state = self.state.clone();
        let mut completion_rx = self.completion_rx.take().expect("Already started");
        let mut stream = server_end.into_stream();
        let control_handle = stream.control_handle();
        let ep_addr = self.addr;

        // Spawn completion forwarder on the endpoint's Scope so dropping MockEndpoint cancels it
        // and drops its ControlHandle clone.
        let ch = control_handle.clone();
        let _ = self.scope.spawn(async move {
            while let Some(completion) = completion_rx.next().await {
                if let Err(e) = ch.send_on_completion(completion) {
                    log::error!("Failed to send completion: {:?}", e);
                    break;
                }
            }
        });

        let completion_tx_clone = self.completion_tx.clone();
        // Spawn request handler on the endpoint's Scope so dropping MockEndpoint cancels it and
        // closes the underlying Zircon channel ServerEnd.
        let _ = self.scope.spawn(async move {
            while let Ok(Some(request)) = stream.try_next().await {
                match request {
                    fusb_endpoint::EndpointRequest::GetInfo { responder } => {
                        let _ = responder.send(Err(Status::NOT_SUPPORTED.into_raw()));
                    }
                    fusb_endpoint::EndpointRequest::GetAlignmentInfo { responder } => {
                        let _ = responder.send(Err(Status::NOT_SUPPORTED.into_raw()));
                    }
                    fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                        let mut vmos = Vec::new();
                        let mut state_lock = state.lock();
                        for info in vmo_ids {
                            let id = info.id.unwrap();
                            let size = info.size.unwrap();
                            let vmo = zx::Vmo::create(size).unwrap();
                            let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                            state_lock.vmos.insert(id, vmo);
                            vmos.push(fusb_endpoint::VmoHandle {
                                id: Some(id),
                                vmo: Some(dup),
                                ..Default::default()
                            });
                        }
                        let _ = responder.send(vmos);
                    }
                    fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                        let mut state_lock = state.lock();
                        for id in vmo_ids {
                            state_lock.vmos.remove(&id);
                        }
                        let _ = responder.send(&[], &[]);
                    }
                    fusb_endpoint::EndpointRequest::QueueRequests { req, control_handle: _ } => {
                        let is_configured = if let Some(ref gs) = global_state {
                            let gs_lock = gs.lock();
                            gs_lock.configured_endpoints.contains_key(&ep_addr)
                        } else {
                            true
                        };

                        if !is_configured {
                            // Endpoint is disabled! Return with IO_NOT_PRESENT to prevent re-queuing
                            let completions: Vec<fusb_endpoint::Completion> = req
                                .into_iter()
                                .map(|r| fusb_endpoint::Completion {
                                    request: Some(r),
                                    status: Some(zx::Status::IO_NOT_PRESENT.into_raw()),
                                    transfer_size: Some(0),
                                    ..Default::default()
                                })
                                .collect();
                            let _ = completion_tx_clone.unbounded_send(completions);
                        } else {
                            // Store in-flight requests for manual completion or cancellation
                            let mut state_lock = state.lock();
                            state_lock.requests.extend(req);
                        }
                    }
                    fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                        let (hold, fail_status) = if let Some(ref gs) = global_state {
                            let mut gs_lock = gs.lock();
                            gs_lock.events.push(MockEvent::CancelAll { ep_addr });
                            if gs_lock.hold_cancel_all {
                                if let Some(ref sender) = gs_lock.held_cancel_notifier {
                                    let _ = sender.unbounded_send(ep_addr);
                                }
                            }
                            (gs_lock.hold_cancel_all, gs_lock.fail_cancel_all_status)
                        } else {
                            (false, None)
                        };

                        if hold {
                            if let Some(ref gs) = global_state {
                                let mut gs_lock = gs.lock();
                                gs_lock.held_cancel_responders.push((ep_addr, responder));
                            }
                            continue;
                        }

                        let mut state_lock = state.lock();
                        let requests = std::mem::take(&mut state_lock.requests);
                        let completions: Vec<fusb_endpoint::Completion> = requests
                            .into_iter()
                            .map(|req| fusb_endpoint::Completion {
                                request: Some(req),
                                status: Some(zx::Status::CANCELED.into_raw()),
                                transfer_size: Some(0),
                                ..Default::default()
                            })
                            .collect();
                        if !completions.is_empty() {
                            let _ = completion_tx_clone.unbounded_send(completions);
                        }
                        if let Some(status) = fail_status {
                            let _ = responder.send(Err(status.into_raw()));
                        } else {
                            let _ = responder.send(Ok(()));
                        }
                    }
                }
            }
        });
    }

    pub fn cancel_all(&self) {
        self.complete_all_with_status(Status::CANCELED);
    }

    pub fn fail_all(&self, status: Status) {
        self.complete_all_with_status(status);
    }

    fn complete_all_with_status(&self, status: Status) {
        let mut state_lock = self.state.lock();
        let requests = std::mem::take(&mut state_lock.requests);
        let completions: Vec<fusb_endpoint::Completion> = requests
            .into_iter()
            .map(|req| fusb_endpoint::Completion {
                request: Some(req),
                status: Some(status.into_raw()),
                transfer_size: Some(0),
                ..Default::default()
            })
            .collect();
        if !completions.is_empty() {
            let _ = self.completion_tx.unbounded_send(completions);
        }
    }
}

#[derive(Debug)]
pub struct MockUsbFunctionState {
    pub interface_client: Option<fusb_function::UsbFunctionInterfaceProxy>,
    interface_ready_notifiers: Vec<oneshot::Sender<fusb_function::UsbFunctionInterfaceProxy>>,
    pub endpoints: HashMap<u8, MockEndpoint>,
    pub configured_endpoints: HashMap<u8, fusb_function::EndpointConfiguration>,
    endpoints_configured_notifiers: Vec<oneshot::Sender<()>>,
    pub events: Vec<MockEvent>,
    pub fail_configure_status: Option<Status>,
    configure_attempt_notifiers: Vec<oneshot::Sender<()>>,
    active_function_channels: usize,
    pub function_channel_closed: bool,
    function_closed_notifiers: Vec<oneshot::Sender<()>>,
    pub fail_configure_endpoint_nth: Option<(usize, Status)>,
    pub fail_disable_endpoint_status: Option<Status>,
    close_function_txs: Vec<oneshot::Sender<oneshot::Sender<()>>>,
    pub fail_cancel_all_status: Option<Status>,
    pub hold_cancel_all: bool,
    pub held_cancel_responders: Vec<(u8, fusb_endpoint::EndpointCancelAllResponder)>,
    pub held_cancel_notifier: Option<mpsc::UnboundedSender<u8>>,

    next_interface_num: u8,
    next_in_ep_num: u8,
    next_out_ep_num: u8,
    next_string_index: u8,
}

#[derive(Debug)]
pub struct MockUsbFunction {
    pub state: Arc<Mutex<MockUsbFunctionState>>,
    scope: fasync::Scope,
}

impl MockUsbFunction {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockUsbFunctionState {
                interface_client: None,
                interface_ready_notifiers: Vec::new(),
                endpoints: HashMap::new(),
                configured_endpoints: HashMap::new(),
                endpoints_configured_notifiers: Vec::new(),
                events: Vec::new(),
                fail_configure_status: None,
                configure_attempt_notifiers: Vec::new(),
                active_function_channels: 0,
                function_channel_closed: false,
                function_closed_notifiers: Vec::new(),
                fail_configure_endpoint_nth: None,
                fail_disable_endpoint_status: None,
                close_function_txs: Vec::new(),
                fail_cancel_all_status: None,
                hold_cancel_all: false,
                held_cancel_responders: Vec::new(),
                held_cancel_notifier: None,
                next_interface_num: 0,
                next_in_ep_num: 1,
                next_out_ep_num: 1,
                next_string_index: 1,
            })),
            scope: fasync::Scope::new_with_name("mock_usb_function"),
        }
    }

    pub async fn close_function_server(&self) {
        let txs: Vec<_> = self.state.lock().close_function_txs.drain(..).collect();
        for tx in txs {
            let (done_tx, done_rx) = oneshot::channel();
            if tx.send(done_tx).is_ok() {
                let _ = done_rx.await;
            }
        }
    }

    pub fn release_held_cancel_all(&self) {
        let mut state_lock = self.state.lock();
        state_lock.hold_cancel_all = false;
        state_lock.held_cancel_notifier = None;
        for ep in state_lock.endpoints.values() {
            ep.cancel_all();
        }
        let responders = std::mem::take(&mut state_lock.held_cancel_responders);
        for (_, responder) in responders {
            let _ = responder.send(Ok(()));
        }
    }

    pub fn release_one_held_cancel_all(&self) -> Option<u8> {
        let mut state_lock = self.state.lock();
        let (ep_addr, responder) = state_lock.held_cancel_responders.pop()?;
        if let Some(ep) = state_lock.endpoints.get(&ep_addr) {
            ep.cancel_all();
        }
        let _ = responder.send(Ok(()));
        Some(ep_addr)
    }

    pub fn release_held_cancel_all_except(&self, keep_ep_addr: u8) {
        let mut state_lock = self.state.lock();
        let responders = std::mem::take(&mut state_lock.held_cancel_responders);
        let mut kept = Vec::new();
        for (ep_addr, responder) in responders {
            if ep_addr == keep_ep_addr {
                kept.push((ep_addr, responder));
            } else {
                if let Some(ep) = state_lock.endpoints.get(&ep_addr) {
                    ep.cancel_all();
                }
                let _ = responder.send(Ok(()));
            }
        }
        state_lock.held_cancel_responders = kept;
    }

    pub async fn wait_for_held_cancel_all(&self, hold_start_idx: usize) {
        let (tx, mut rx) = mpsc::unbounded();
        let already_held = {
            let mut state = self.state.lock();
            state.held_cancel_notifier = Some(tx);
            state.events[hold_start_idx..]
                .iter()
                .any(|e| matches!(e, MockEvent::CancelAll { .. }) && state.hold_cancel_all)
        };

        if !already_held {
            let _ = rx.next().await;
        }

        let mut state = self.state.lock();
        state.held_cancel_notifier = None;
    }
    pub async fn wait_for_endpoints_configured(&self, expected_endpoints: &[u8]) {
        loop {
            let rx = {
                let mut state = self.state.lock();
                if expected_endpoints
                    .iter()
                    .all(|addr| state.configured_endpoints.contains_key(addr))
                {
                    return;
                }
                let (tx, rx) = oneshot::channel();
                state.endpoints_configured_notifiers.push(tx);
                rx
            };
            let _ = rx.await;
        }
    }

    pub async fn wait_for_configure_attempt(&self) {
        let rx = {
            let mut state = self.state.lock();
            if state.events.iter().any(|e| matches!(e, MockEvent::Configure { .. })) {
                return;
            }
            let (tx, rx) = oneshot::channel();
            state.configure_attempt_notifiers.push(tx);
            rx
        };
        rx.await.expect("UsbFunction channel closed before Configure was attempted");
    }

    pub async fn wait_for_function_closed(&self) {
        let rx = {
            let mut state = self.state.lock();
            if state.function_channel_closed {
                return;
            }
            let (tx, rx) = oneshot::channel();
            state.function_closed_notifiers.push(tx);
            rx
        };
        let _ = rx.await;
    }
    pub async fn wait_for_interface(&self) -> fusb_function::UsbFunctionInterfaceProxy {
        let rx = {
            let mut state = self.state.lock();
            if let Some(proxy) = &state.interface_client {
                return proxy.clone();
            }
            let (tx, rx) = oneshot::channel();
            state.interface_ready_notifiers.push(tx);
            rx
        };
        rx.await.expect("UsbFunction channel closed before Configure provided UsbFunctionInterface")
    }

    pub fn start(&self, server_end: ServerEnd<fusb_function::UsbFunctionMarker>) {
        let state = self.state.clone();
        let mut stream = server_end.into_stream();
        let (close_tx, mut close_rx) = oneshot::channel::<oneshot::Sender<()>>();
        {
            let mut state_lock = state.lock();
            state_lock.close_function_txs.push(close_tx);
            state_lock.active_function_channels += 1;
            state_lock.function_channel_closed = false;
        }

        let _ = self.scope.spawn(async move {
            loop {
                let request = futures::select! {
                    req = stream.try_next().fuse() => req,
                    done_tx = close_rx => {
                        if let Ok(done_tx) = done_tx {
                            drop(stream);
                            let _ = done_tx.send(());
                            break;
                        }
                        continue;
                    }
                };
                let Ok(Some(request)) = request else {
                    break;
                };
                match request {
                    fusb_function::UsbFunctionRequest::AllocResources {
                        interface_count,
                        endpoints,
                        strings,
                        responder,
                    } => {
                        let mut state_lock = state.lock();
                        let mut interface_nums = Vec::new();
                        for _ in 0..interface_count {
                            interface_nums.push(state_lock.next_interface_num);
                            state_lock.next_interface_num += 1;
                        }

                        let mut string_indices = Vec::new();
                        for _ in strings {
                            string_indices.push(state_lock.next_string_index);
                            state_lock.next_string_index += 1;
                        }

                        let mut endpoint_addrs = Vec::new();
                        for ep_res in endpoints {
                            let dir = ep_res.direction;
                            let addr = match dir {
                                fusb_descriptor::EndpointDirection::In => {
                                    let a = state_lock.next_in_ep_num | 0x80;
                                    state_lock.next_in_ep_num += 1;
                                    a
                                }
                                fusb_descriptor::EndpointDirection::Out => {
                                    let a = state_lock.next_out_ep_num;
                                    state_lock.next_out_ep_num += 1;
                                    a
                                }
                            };
                            endpoint_addrs.push(addr);

                            let mut ep = MockEndpoint::new(addr, dir);
                            ep.start(ep_res.endpoint, Some(state.clone()));
                            state_lock.endpoints.insert(addr, ep);
                        }

                        state_lock.events.push(MockEvent::AllocResources {
                            interface_nums: interface_nums.clone(),
                            endpoints: endpoint_addrs.clone(),
                        });
                        let _ =
                            responder.send(Ok((&interface_nums, &endpoint_addrs, &string_indices)));
                    }
                    fusb_function::UsbFunctionRequest::ConnectToEndpoint {
                        ep_addr,
                        ep,
                        responder,
                    } => {
                        let mut state_lock = state.lock();
                        if state_lock.endpoints.contains_key(&ep_addr) {
                            let _ = responder.send(Err(Status::ALREADY_BOUND.into_raw()));
                        } else {
                            // Determine direction from address (MSB is IN)
                            let dir = if (ep_addr & 0x80) != 0 {
                                fusb_descriptor::EndpointDirection::In
                            } else {
                                fusb_descriptor::EndpointDirection::Out
                            };
                            let mut mock_ep = MockEndpoint::new(ep_addr, dir);
                            mock_ep.start(ep, Some(state.clone()));
                            state_lock.endpoints.insert(ep_addr, mock_ep);
                            let _ = responder.send(Ok(()));
                        }
                    }
                    fusb_function::UsbFunctionRequest::Configure {
                        configuration,
                        iface,
                        responder,
                    } => {
                        let mut state_lock = state.lock();
                        state_lock.events.push(MockEvent::Configure { descriptors: configuration });
                        for tx in state_lock.configure_attempt_notifiers.drain(..) {
                            let _ = tx.send(());
                        }
                        if let Some(status) = state_lock.fail_configure_status {
                            drop(iface);
                            let _ = responder.send(Err(status.into_raw()));
                            continue;
                        }
                        let proxy = iface.into_proxy();
                        state_lock.interface_client = Some(proxy.clone());
                        for tx in state_lock.interface_ready_notifiers.drain(..) {
                            let _ = tx.send(proxy.clone());
                        }
                        let _ = responder.send(Ok(()));
                    }
                    fusb_function::UsbFunctionRequest::Deconfigure { responder } => {
                        let mut state_lock = state.lock();
                        state_lock.interface_client = None;
                        state_lock.configured_endpoints.clear();
                        for ep in state_lock.endpoints.values() {
                            ep.cancel_all();
                        }
                        state_lock.events.push(MockEvent::Deconfigure);
                        let _ = responder.send(Ok(()));
                    }
                    fusb_function::UsbFunctionRequest::ConfigureEndpoint {
                        endpoint_address,
                        endpoint_configuration,
                        responder,
                    } => {
                        let mut state_lock = state.lock();
                        if let Some((nth, status)) = state_lock.fail_configure_endpoint_nth {
                            if nth <= 1 {
                                state_lock.fail_configure_endpoint_nth = None;
                                let _ = responder.send(Err(status.into_raw()));
                                continue;
                            } else {
                                state_lock.fail_configure_endpoint_nth = Some((nth - 1, status));
                            }
                        }
                        state_lock
                            .configured_endpoints
                            .insert(endpoint_address, endpoint_configuration);
                        state_lock
                            .events
                            .push(MockEvent::ConfigureEndpoint { ep_addr: endpoint_address });
                        for tx in state_lock.endpoints_configured_notifiers.drain(..) {
                            let _ = tx.send(());
                        }
                        let _ = responder.send(Ok(()));
                    }
                    fusb_function::UsbFunctionRequest::DisableEndpoint {
                        endpoint_address,
                        responder,
                    } => {
                        let mut state_lock = state.lock();
                        state_lock.configured_endpoints.remove(&endpoint_address);
                        if !state_lock.hold_cancel_all {
                            if let Some(ep) = state_lock.endpoints.get(&endpoint_address) {
                                ep.cancel_all();
                            }
                        }
                        state_lock
                            .events
                            .push(MockEvent::DisableEndpoint { ep_addr: endpoint_address });
                        if let Some(status) = state_lock.fail_disable_endpoint_status {
                            let _ = responder.send(Err(status.into_raw()));
                        } else {
                            let _ = responder.send(Ok(()));
                        }
                    }
                    fusb_function::UsbFunctionRequest::EndpointSetStall {
                        endpoint_address,
                        responder,
                    } => {
                        let state_lock = state.lock();
                        if let Some(ep) = state_lock.endpoints.get(&endpoint_address) {
                            ep.fail_all(Status::IO_REFUSED);
                        }
                        let _ = responder.send(Ok(()));
                    }
                    fusb_function::UsbFunctionRequest::EndpointClearStall {
                        endpoint_address: _,
                        responder,
                    } => {
                        let _ = responder.send(Ok(()));
                    }
                }
            }
            let mut state_lock = state.lock();
            state_lock.active_function_channels =
                state_lock.active_function_channels.saturating_sub(1);
            if state_lock.active_function_channels == 0 {
                state_lock.interface_ready_notifiers.clear();
                state_lock.configure_attempt_notifiers.clear();
                state_lock.function_channel_closed = true;
                for tx in state_lock.function_closed_notifiers.drain(..) {
                    let _ = tx.send(());
                }
            }
        });
    }
}
