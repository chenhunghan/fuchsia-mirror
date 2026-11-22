// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use async_trait::async_trait;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_system as fsystem;
use log::warn;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PowerLevel {
    Off = 0,
    Suspend = 1,
    Active = 2,
}

impl From<PowerLevel> for u8 {
    fn from(level: PowerLevel) -> Self {
        level as u8
    }
}

#[must_use = "a WakeLease keeps the system awake while held and must be retained or explicitly dropped"]
#[derive(Debug)]
pub struct WakeLease {
    _token: fsystem::LeaseToken,
}

impl WakeLease {
    pub fn from_token(token: fsystem::LeaseToken) -> Self {
        Self { _token: token }
    }

    /// Returns a duplicate handle to the underlying wake lease token so one-way FIDL senders can
    /// pass a wake baton to recipients without prematurely releasing the lease.
    pub fn duplicate_token(&self) -> Option<fsystem::LeaseToken> {
        self._token.duplicate_handle(zx::Rights::SAME_RIGHTS).ok()
    }

    /// Constructs a WakeLease from an underlying token.
    /// This is intended strictly for use in test environments.
    #[doc(hidden)]
    pub fn from_token_for_test(token: fsystem::LeaseToken) -> Self {
        Self { _token: token }
    }
}

/// A token representing the startup suspend blocker registration.
///
/// An InitSuspendBlocker holds a startup wake lease preventing system suspend
/// until initialization finishes. The only way to obtain the underlying
/// SuspendBlockerRequestStream to serve suspend blocker requests is to consume
/// this struct via `into_stream_dropping_init_lease`, ensuring that the initial
/// wake lease is not accidentally held across the FIDL serve loop.
#[must_use = "an InitSuspendBlocker holds a startup wake lease that must be consumed via into_stream_dropping_init_lease"]
pub struct InitSuspendBlocker {
    lease: WakeLease,
    stream: fsystem::SuspendBlockerRequestStream,
}

impl std::fmt::Debug for InitSuspendBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitSuspendBlocker").field("lease", &self.lease).finish()
    }
}

impl InitSuspendBlocker {
    pub fn new(lease: WakeLease, stream: fsystem::SuspendBlockerRequestStream) -> Self {
        Self { lease, stream }
    }

    /// Consumes this struct, drops the startup wake lease, and returns the request stream.
    pub fn into_stream_dropping_init_lease(self) -> fsystem::SuspendBlockerRequestStream {
        drop(self.lease);
        self.stream
    }
}

pub trait SyncPowerManager: Send + Sync + std::fmt::Debug {
    #[must_use = "a WakeLease keeps the system awake while held and must be retained or explicitly dropped"]
    fn take_wake_lease(&self, name: &str) -> Option<WakeLease>;
}

impl SyncPowerManager for () {
    fn take_wake_lease(&self, _name: &str) -> Option<WakeLease> {
        None
    }
}

const ACQUIRE_WAKE_LEASE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

#[derive(Debug)]
pub struct DeviceSyncPowerManager {
    proxy: Option<fsystem::ActivityGovernorSynchronousProxy>,
}

impl DeviceSyncPowerManager {
    pub fn new(proxy: Option<fsystem::ActivityGovernorSynchronousProxy>) -> Self {
        Self { proxy }
    }
}

impl SyncPowerManager for DeviceSyncPowerManager {
    fn take_wake_lease(&self, name: &str) -> Option<WakeLease> {
        let activity_governor = self.proxy.as_ref()?;
        let deadline = zx::MonotonicInstant::after(ACQUIRE_WAKE_LEASE_TIMEOUT);
        match activity_governor.acquire_wake_lease(name, deadline) {
            Ok(Ok(token)) => Some(WakeLease { _token: token }),
            Ok(Err(e)) => {
                warn!("Failed to acquire wake lease {}: {:?}", name, e);
                None
            }
            Err(e) => {
                warn!("FIDL error when acquiring wake lease {}: {:?}", name, e);
                None
            }
        }
    }
}

#[async_trait]
pub trait PowerManager: Send + Sync {
    #[must_use = "a WakeLease keeps the system awake while held and must be retained or explicitly dropped"]
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease>;
    async fn power_element_lease(
        &self,
        lease_name: &str,
        dependency_token: fbroker::DependencyToken,
        dependency_level: PowerLevel,
    ) -> Result<fbroker::LeaseToken, Error>;
    async fn register_suspend_blocker(&self, name: &str) -> Result<InitSuspendBlocker, Error>;
}

pub struct DevicePowerManager {
    proxies: Option<(fsystem::ActivityGovernorProxy, fbroker::TopologyProxy)>,
}

impl DevicePowerManager {
    pub fn new(proxies: Option<(fsystem::ActivityGovernorProxy, fbroker::TopologyProxy)>) -> Self {
        Self { proxies }
    }
}

#[async_trait]
impl PowerManager for DevicePowerManager {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease> {
        let (activity_governor, _) = self.proxies.as_ref()?;
        match activity_governor.acquire_wake_lease(name).await {
            Ok(Ok(token)) => Some(WakeLease { _token: token }),
            Ok(Err(e)) => {
                warn!("Failed to acquire wake lease {}: {:?}", name, e);
                None
            }
            Err(e) => {
                warn!("FIDL error when acquiring wake lease {}: {:?}", name, e);
                None
            }
        }
    }

    async fn power_element_lease(
        &self,
        lease_name: &str,
        dependency_token: fbroker::DependencyToken,
        dependency_level: PowerLevel,
    ) -> Result<fbroker::LeaseToken, Error> {
        let (_, power_broker) =
            self.proxies.as_ref().ok_or_else(|| anyhow::anyhow!("Power broker not available"))?;

        let dep = fbroker::LeaseDependency {
            requires_token: Some(dependency_token),
            requires_level: Some(dependency_level as u8),
            ..Default::default()
        };
        let (lease_token_local, lease_token_server) = zx::EventPair::create();
        let schema = fbroker::LeaseSchema {
            lease_token: Some(lease_token_server),
            lease_name: Some(lease_name.to_string()),
            dependencies: Some(vec![dep]),
            ..Default::default()
        };
        power_broker
            .lease(schema)
            .await?
            .map_err(|e| anyhow::anyhow!("Power broker returned error: {:?}", e))?;
        Ok(lease_token_local)
    }

    async fn register_suspend_blocker(&self, name: &str) -> Result<InitSuspendBlocker, Error> {
        let (activity_governor, _) = self
            .proxies
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Activity governor not available"))?;
        let (suspend_blocker_client, suspend_blocker_requests) =
            fidl::endpoints::create_request_stream::<fsystem::SuspendBlockerMarker>();
        let payload = fsystem::ActivityGovernorRegisterSuspendBlockerRequest {
            suspend_blocker: Some(suspend_blocker_client),
            name: Some(name.to_string()),
            ..Default::default()
        };
        match activity_governor.register_suspend_blocker(payload).await {
            Ok(Ok(token)) => {
                Ok(InitSuspendBlocker::new(WakeLease::from_token(token), suspend_blocker_requests))
            }
            Ok(Err(e)) => Err(anyhow::anyhow!("RegisterSuspendBlocker error: {:?}", e)),
            Err(e) => Err(anyhow::anyhow!("FIDL error: {:?}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fuchsia_async as fasync;
    use futures::StreamExt;

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_disabled_take_wake_lease() {
        let pm = DevicePowerManager::new(None);
        assert!(pm.take_wake_lease("test-lease").await.is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_disabled_power_element_lease() {
        let pm = DevicePowerManager::new(None);
        let token = zx::Event::create();
        assert!(pm.power_element_lease("test-lease", token, PowerLevel::Suspend).await.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_disabled_register_suspend_blocker() {
        let pm = DevicePowerManager::new(None);
        assert!(pm.register_suspend_blocker("test-blocker").await.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_take_wake_lease_success() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, _topology_server) =
            fidl::endpoints::create_proxy::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((proxy, topology_proxy)));

        let server_fut = async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fsystem::ActivityGovernorRequest::AcquireWakeLease { name, responder } => {
                        assert_eq!(name, "test-wake-lease");
                        let (_server_token, client_token) = fsystem::LeaseToken::create();
                        let _ = responder.send(Ok(client_token));
                    }
                    other => panic!("Unexpected request: {:?}", other),
                }
            }
        };
        let client_fut = async move {
            let lease = pm.take_wake_lease("test-wake-lease").await;
            assert!(lease.is_some());
        };
        futures::join!(server_fut, client_fut);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_take_wake_lease_error() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, _topology_server) =
            fidl::endpoints::create_proxy::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((proxy, topology_proxy)));

        let server_fut = async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fsystem::ActivityGovernorRequest::AcquireWakeLease { name, responder } => {
                        assert_eq!(name, "test-wake-lease");
                        let _ = responder.send(Err(fsystem::AcquireWakeLeaseError::Internal));
                    }
                    other => panic!("Unexpected request: {:?}", other),
                }
            }
        };
        let client_fut = async move {
            let lease = pm.take_wake_lease("test-wake-lease").await;
            assert!(lease.is_none());
        };
        futures::join!(server_fut, client_fut);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_register_suspend_blocker_success() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, _topology_server) =
            fidl::endpoints::create_proxy::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((proxy, topology_proxy)));
        let server_token_and_proxy = std::sync::Arc::new(std::sync::Mutex::new(None));

        let server_fut = {
            let server_token_and_proxy = server_token_and_proxy.clone();
            async move {
                while let Some(Ok(req)) = stream.next().await {
                    match req {
                        fsystem::ActivityGovernorRequest::RegisterSuspendBlocker {
                            payload,
                            responder,
                        } => {
                            assert_eq!(payload.name.as_deref(), Some("test-blocker"));
                            let blocker_proxy = payload
                                .suspend_blocker
                                .expect("suspend blocker client end present")
                                .into_proxy();
                            let (server_token, client_token) = fsystem::LeaseToken::create();
                            *server_token_and_proxy.lock().unwrap() =
                                Some((server_token, blocker_proxy));
                            let _ = responder.send(Ok(client_token));
                        }
                        other => panic!("Unexpected request: {:?}", other),
                    }
                }
            }
        };
        let client_fut = async move {
            let blocker = pm
                .register_suspend_blocker("test-blocker")
                .await
                .expect("register_suspend_blocker succeeds");
            let (server_token, blocker_proxy) =
                server_token_and_proxy.lock().unwrap().take().expect("server token was recorded");
            // The wake lease is held while InitSuspendBlocker is alive.
            assert_matches!(
                server_token.wait_one(
                    zx::Signals::EVENTPAIR_PEER_CLOSED,
                    zx::MonotonicInstant::INFINITE_PAST
                ),
                zx::WaitResult::TimedOut(_)
            );

            // Consuming InitSuspendBlocker drops the wake lease immediately.
            let mut stream = blocker.into_stream_dropping_init_lease();
            assert_matches!(
                server_token.wait_one(
                    zx::Signals::EVENTPAIR_PEER_CLOSED,
                    zx::MonotonicInstant::INFINITE_PAST
                ),
                zx::WaitResult::Ok(_)
            );

            // The returned request stream remains fully functional for suspend blocker requests.
            let client_call_fut = async {
                blocker_proxy.before_suspend().await.expect("before_suspend succeeds");
            };
            let stream_handler_fut = async {
                let req = stream
                    .next()
                    .await
                    .expect("stream receives request")
                    .expect("valid FIDL request");
                match req {
                    fsystem::SuspendBlockerRequest::BeforeSuspend { responder } => {
                        responder.send().expect("send before_suspend response");
                    }
                    other => panic!("Unexpected request on stream: {:?}", other),
                }
            };
            futures::join!(client_call_fut, stream_handler_fut);
        };
        futures::join!(server_fut, client_fut);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_register_suspend_blocker_error() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, _topology_server) =
            fidl::endpoints::create_proxy::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((proxy, topology_proxy)));

        let server_fut = async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fsystem::ActivityGovernorRequest::RegisterSuspendBlocker {
                        payload,
                        responder,
                    } => {
                        assert_eq!(payload.name.as_deref(), Some("test-blocker"));
                        let _ = responder.send(Err(fsystem::RegisterSuspendBlockerError::Internal));
                    }
                    other => panic!("Unexpected request: {:?}", other),
                }
            }
        };
        let client_fut = async move {
            let blocker = pm.register_suspend_blocker("test-blocker").await;
            assert_matches!(blocker, Err(_));
        };
        futures::join!(server_fut, client_fut);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_power_element_lease_success() {
        let (ag_proxy, _ag_server) =
            fidl::endpoints::create_proxy::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((ag_proxy, topology_proxy)));

        let server_fut = async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fbroker::TopologyRequest::Lease { responder, .. } => {
                        let _ = responder.send(Ok(()));
                    }
                    other => panic!("Unexpected request: {:?}", other),
                }
            }
        };
        let client_fut = async move {
            let token = zx::Event::create();
            let lease = pm.power_element_lease("test-lease", token, PowerLevel::Suspend).await;
            assert_matches!(lease, Ok(_));
        };
        futures::join!(server_fut, client_fut);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_power_manager_power_element_lease_error() {
        let (ag_proxy, _ag_server) =
            fidl::endpoints::create_proxy::<fsystem::ActivityGovernorMarker>();
        let (topology_proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbroker::TopologyMarker>();
        let pm = DevicePowerManager::new(Some((ag_proxy, topology_proxy)));

        let server_fut = async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fbroker::TopologyRequest::Lease { responder, .. } => {
                        let _ = responder.send(Err(fbroker::LeaseError::Internal));
                    }
                    other => panic!("Unexpected request: {:?}", other),
                }
            }
        };
        let client_fut = async move {
            let token = zx::Event::create();
            let lease = pm.power_element_lease("test-lease", token, PowerLevel::Suspend).await;
            assert_matches!(lease, Err(_));
        };
        futures::join!(server_fut, client_fut);
    }

    #[test]
    fn test_sync_power_manager_disabled_take_wake_lease() {
        let pm = DeviceSyncPowerManager::new(None);
        assert!(pm.take_wake_lease("test-lease").is_none());
        let unit_pm = ();
        assert!(unit_pm.take_wake_lease("test-lease").is_none());
    }

    #[test]
    fn test_sync_power_manager_take_wake_lease_success() {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fsystem::ActivityGovernorMarker>();
        let proxy = client_end.into_sync_proxy();
        let pm = DeviceSyncPowerManager::new(Some(proxy));

        let thread_handle = std::thread::spawn(move || {
            let mut exec = fasync::LocalExecutor::default();
            let mut stream = server_end.into_stream();
            exec.run_singlethreaded(async move {
                if let Some(Ok(req)) = stream.next().await {
                    match req {
                        fsystem::ActivityGovernorRequest::AcquireWakeLease { name, responder } => {
                            assert_eq!(name, "test-sync-wake-lease");
                            let (_server_token, client_token) = fsystem::LeaseToken::create();
                            let _ = responder.send(Ok(client_token));
                        }
                        other => panic!("Unexpected request: {:?}", other),
                    }
                }
            });
        });

        let lease = pm.take_wake_lease("test-sync-wake-lease");
        assert!(lease.is_some());
        thread_handle.join().unwrap();
    }

    #[test]
    fn test_sync_power_manager_take_wake_lease_error() {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fsystem::ActivityGovernorMarker>();
        let proxy = client_end.into_sync_proxy();
        let pm = DeviceSyncPowerManager::new(Some(proxy));

        let thread_handle = std::thread::spawn(move || {
            let mut exec = fasync::LocalExecutor::default();
            let mut stream = server_end.into_stream();
            exec.run_singlethreaded(async move {
                if let Some(Ok(req)) = stream.next().await {
                    match req {
                        fsystem::ActivityGovernorRequest::AcquireWakeLease { name, responder } => {
                            assert_eq!(name, "test-sync-wake-lease");
                            let _ = responder.send(Err(fsystem::AcquireWakeLeaseError::Internal));
                        }
                        other => panic!("Unexpected request: {:?}", other),
                    }
                }
            });
        });

        let lease = pm.take_wake_lease("test-sync-wake-lease");
        assert!(lease.is_none());
        thread_handle.join().unwrap();
    }

    #[test]
    fn test_wake_lease_duplicate_token_keeps_peer_open() {
        let (server_token, client_token) = fsystem::LeaseToken::create();
        let lease = WakeLease::from_token(client_token);
        let duplicated = lease.duplicate_token().expect("duplicate_token should succeed");

        assert_eq!(
            duplicated.basic_info().expect("duplicated info").related_koid,
            server_token.koid().expect("server koid")
        );

        // Dropping the original `WakeLease` must not close the peer while the duplicate is held.
        drop(lease);
        assert_eq!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(zx::Signals::empty())
        );

        drop(duplicated);
        assert_matches!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::Ok(signals) if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
    }
}
