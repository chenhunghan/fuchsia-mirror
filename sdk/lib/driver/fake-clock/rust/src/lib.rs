// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fake `fuchsia.hardware.clock/Clock` server.
//!
//! Enable and disable requests are tracked as per-connection votes, like a real clock provider
//! (see `fdf_resource_vote`): every connection owns its own counted vote (each `Enable` increments
//! it, each `Disable` decrements it, saturating at zero), attributed to the service instance name
//! passed to [`FakeClock::serve`]. The clock is enabled while any vote is held. When a connection
//! closes while holding a vote, the vote is kept as an orphan and is reclaimed by the next
//! `Enable` on a new connection to the same instance.

use fdf_resource_vote::{BoolVoteSet, CloseBehavior, Vote};
use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_clock::{self as fclock, ClockServerHandler};
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use fuchsia_sync::Mutex;
use std::sync::Arc;

/// One-shot or sticky injected error occupying a single shared slot.
#[derive(Clone, Copy, Default)]
enum InjectedError {
    #[default]
    None,
    Once(zx::Status),
    Always(zx::Status),
}

impl InjectedError {
    fn from_option(error: Option<zx::Status>) -> Self {
        match error {
            Some(status) => Self::Always(status),
            None => Self::None,
        }
    }

    fn take(&mut self) -> Option<zx::Status> {
        match *self {
            Self::None => None,
            Self::Once(status) => {
                *self = Self::None;
                Some(status)
            }
            Self::Always(status) => Some(status),
        }
    }
}

struct FakeClockState {
    votes: BoolVoteSet,
    enable_calls: u32,
    disable_calls: u32,
    rate: u64,
    rate_locked: bool,
    enable_error: InjectedError,
    disable_error: InjectedError,
    get_rate_error: InjectedError,
}

#[derive(Clone)]
pub struct FakeClock {
    state: Arc<Mutex<FakeClockState>>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeClockState {
                votes: BoolVoteSet::new_bool("fake-clock"),
                enable_calls: 0,
                disable_calls: 0,
                rate: 0,
                rate_locked: false,
                enable_error: InjectedError::None,
                disable_error: InjectedError::None,
                get_rate_error: InjectedError::None,
            })),
        }
    }

    /// Whether the clock is enabled, i.e. whether any vote (live or orphaned) is held.
    pub fn enabled(&self) -> bool {
        self.state.lock().votes.enabled()
    }

    /// Returns the total active vote count across all connections (including orphaned votes).
    pub fn ref_count(&self) -> u32 {
        self.state.lock().votes.entries().filter(|e| e.request).map(|e| e.count).sum()
    }

    /// The number of orphaned votes, i.e. votes held by connections that have closed.
    pub fn orphan_count(&self) -> usize {
        self.state.lock().votes.orphan_count()
    }

    pub fn enable_calls(&self) -> u32 {
        self.state.lock().enable_calls
    }

    pub fn disable_calls(&self) -> u32 {
        self.state.lock().disable_calls
    }

    pub fn rate(&self) -> u64 {
        self.state.lock().rate
    }

    pub fn set_rate(&self, hz: u64) {
        self.state.lock().rate = hz;
    }

    /// When `true`, `SetRate` requests succeed without changing the clock rate.
    pub fn set_rate_locked(&self, locked: bool) {
        self.state.lock().rate_locked = locked;
    }

    /// Sets a sticky `Enable` error (`None` also clears any pending [`Self::fail_next_enable`]).
    pub fn set_enable_error(&self, error: Option<zx::Status>) {
        self.state.lock().enable_error = InjectedError::from_option(error);
    }

    /// Fails the next `Enable` call with `status` (sharing a slot with [`Self::set_enable_error`]).
    pub fn fail_next_enable(&self, status: zx::Status) {
        self.state.lock().enable_error = InjectedError::Once(status);
    }

    /// Sets a sticky `Disable` error (`None` also clears any pending [`Self::fail_next_disable`]).
    /// A failed `Disable` keeps the connection's vote unchanged, matching the provider contract.
    pub fn set_disable_error(&self, error: Option<zx::Status>) {
        self.state.lock().disable_error = InjectedError::from_option(error);
    }

    /// Fails the next `Disable` call with `status`, keeping the connection's vote unchanged
    /// (sharing a slot with [`Self::set_disable_error`]).
    pub fn fail_next_disable(&self, status: zx::Status) {
        self.state.lock().disable_error = InjectedError::Once(status);
    }

    /// Sets a sticky `GetRate` error (`None` also clears any pending [`Self::fail_next_get_rate`]).
    pub fn set_get_rate_error(&self, error: Option<zx::Status>) {
        self.state.lock().get_rate_error = InjectedError::from_option(error);
    }

    /// Fails the next `GetRate` call with `status` (sharing a slot with
    /// [`Self::set_get_rate_error`]).
    pub fn fail_next_get_rate(&self, status: zx::Status) {
        self.state.lock().get_rate_error = InjectedError::Once(status);
    }

    pub fn connect(&self, scope: &fasync::ScopeHandle) -> fidl_next::Client<fclock::Clock> {
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        server_end.spawn_on(
            FakeClockServer {
                state: self.state.clone(),
                vote: Vote::new("default", CloseBehavior::OrphanOnClose),
            },
            scope,
        );
        client_end.spawn_on(scope)
    }

    pub fn serve<O: ServiceObjTrait>(
        &self,
        service_fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        fdf_component::ServiceOffer::<fclock::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                FakeClockService {
                    state: self.state.clone(),
                    scope,
                    node_key: instance_name.to_string(),
                },
            )
            .build_zircon_offer_next()
    }
}

struct FakeClockService {
    state: Arc<Mutex<FakeClockState>>,
    scope: fasync::ScopeHandle,
    node_key: String,
}

impl fclock::ServiceHandler for FakeClockService {
    fn clock(&self, server_end: fidl_next::ServerEnd<fclock::Clock>) {
        server_end.spawn_on(
            FakeClockServer {
                state: self.state.clone(),
                vote: Vote::new(self.node_key.clone(), CloseBehavior::OrphanOnClose),
            },
            &self.scope,
        );
    }
}

/// Serves one connection and owns its vote.
struct FakeClockServer {
    state: Arc<Mutex<FakeClockState>>,
    vote: Vote,
}

impl Drop for FakeClockServer {
    fn drop(&mut self) {
        // The connection closed. Closing an orphan-on-close vote never changes the aggregate.
        let mut state = self.state.lock();
        let plan = state.votes.prepare_close(&self.vote);
        state.votes.commit(plan);
    }
}

impl ClockServerHandler for FakeClockServer {
    async fn enable(&mut self, responder: Responder<fclock::clock::Enable>) {
        let result = {
            let mut state = self.state.lock();
            state.enable_calls += 1;
            match state.enable_error.take() {
                Some(status) => Err(status),
                None => {
                    let plan = state.votes.prepare_enable(&self.vote);
                    state.votes.commit(plan);
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn disable(&mut self, responder: Responder<fclock::clock::Disable>) {
        let result = {
            let mut state = self.state.lock();
            state.disable_calls += 1;
            match state.disable_error.take() {
                Some(status) => Err(status),
                None => {
                    let plan = state.votes.prepare_disable(&self.vote);
                    state.votes.commit(plan);
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn is_enabled(&mut self, responder: Responder<fclock::clock::IsEnabled>) {
        let enabled = self.state.lock().votes.enabled();
        let _ = responder.respond(enabled).await;
    }

    async fn set_rate(
        &mut self,
        request: Request<fclock::clock::SetRate>,
        responder: Responder<fclock::clock::SetRate>,
    ) {
        {
            let mut state = self.state.lock();
            if !state.rate_locked {
                state.rate = request.payload().hz;
            }
        }
        let _ = responder.respond(()).await;
    }

    async fn query_supported_rate(
        &mut self,
        _request: Request<fclock::clock::QuerySupportedRate>,
        responder: Responder<fclock::clock::QuerySupportedRate>,
    ) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn get_rate(&mut self, responder: Responder<fclock::clock::GetRate>) {
        let result = {
            let mut state = self.state.lock();
            match state.get_rate_error.take() {
                Some(status) => Err(status),
                None => Ok(fclock::natural::ClockGetRateResponse { hz: state.rate }),
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn set_input(
        &mut self,
        _request: Request<fclock::clock::SetInput>,
        responder: Responder<fclock::clock::SetInput>,
    ) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn get_num_inputs(&mut self, responder: Responder<fclock::clock::GetNumInputs>) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn get_input(&mut self, responder: Responder<fclock::clock::GetInput>) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn get_properties(&mut self, responder: Responder<fclock::clock::GetProperties>) {
        let _ = responder
            .respond(fclock::natural::ClockGetPropertiesResponse {
                id: 0,
                name: "clock".to_string(),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_next::fuchsia::create_channel;

    /// Connects to `fake` the same way `serve` does, on a dedicated scope that ends once the
    /// connection has been torn down.
    fn connect(fake: &FakeClock) -> (fidl_next::Client<fclock::Clock>, fasync::Scope) {
        let scope = fasync::Scope::new_with_name("fake-clock-connection");
        let service = FakeClockService {
            state: fake.state.clone(),
            scope: scope.to_handle(),
            node_key: "default".to_string(),
        };
        let (client_end, server_end) = create_channel::<fclock::Clock>();
        fclock::ServiceHandler::clock(&service, server_end);
        (client_end.spawn(), scope)
    }

    async fn enable(client: &fidl_next::Client<fclock::Clock>) {
        assert!(client.enable().await.expect("FIDL error").is_ok());
    }

    async fn disable(client: &fidl_next::Client<fclock::Clock>) {
        assert!(client.disable().await.expect("FIDL error").is_ok());
    }

    #[fuchsia::test]
    async fn ref_counting_and_counters() {
        let scope = fasync::Scope::new_with_name("test");
        let clock = FakeClock::new();
        let client = clock.connect(&scope.to_handle());

        assert!(!clock.enabled());
        assert_eq!(clock.ref_count(), 0);
        assert!(!client.is_enabled().await.unwrap().unwrap().enabled);

        client.enable().await.unwrap().unwrap();
        client.enable().await.unwrap().unwrap();
        assert!(clock.enabled());
        assert_eq!(clock.ref_count(), 2);
        assert_eq!(clock.enable_calls(), 2);
        assert!(client.is_enabled().await.unwrap().unwrap().enabled);

        client.disable().await.unwrap().unwrap();
        assert!(clock.enabled());
        assert_eq!(clock.ref_count(), 1);
        assert_eq!(clock.disable_calls(), 1);

        client.disable().await.unwrap().unwrap();
        assert!(!clock.enabled());
        assert_eq!(clock.ref_count(), 0);
        assert_eq!(clock.disable_calls(), 2);

        client.disable().await.unwrap().unwrap();
        assert!(!clock.enabled());
        assert_eq!(clock.ref_count(), 0);
        assert_eq!(clock.disable_calls(), 3);
    }

    #[fuchsia::test]
    async fn error_injection_keeps_reference_on_failed_disable() {
        let scope = fasync::Scope::new_with_name("test");
        let clock = FakeClock::new();
        let client = clock.connect(&scope.to_handle());

        clock.fail_next_enable(zx::Status::IO);
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::IO));
        assert_eq!(clock.ref_count(), 0);
        assert_eq!(clock.enable_calls(), 1);

        clock.set_enable_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(clock.ref_count(), 0);
        assert_eq!(clock.enable_calls(), 3);

        clock.set_enable_error(None);
        client.enable().await.unwrap().unwrap();
        assert_eq!(clock.ref_count(), 1);
        assert_eq!(clock.enable_calls(), 4);

        clock.set_disable_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.disable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(clock.ref_count(), 1);
        assert_eq!(clock.disable_calls(), 1);

        clock.set_disable_error(None);
        clock.fail_next_disable(zx::Status::BAD_STATE);
        assert_eq!(client.disable().await.unwrap(), Err(zx::Status::BAD_STATE));
        assert_eq!(clock.ref_count(), 1);

        client.disable().await.unwrap().unwrap();
        assert_eq!(clock.ref_count(), 0);
    }

    #[fuchsia::test]
    async fn rate_locking_and_get_rate_errors() {
        let scope = fasync::Scope::new_with_name("test");
        let clock = FakeClock::new();
        clock.set_rate(200_000_000);
        let client = clock.connect(&scope.to_handle());

        assert_eq!(client.get_rate().await.unwrap().unwrap().hz, 200_000_000);

        clock.set_rate_locked(true);
        client.set_rate(100_000_000).await.unwrap().unwrap();
        assert_eq!(clock.rate(), 200_000_000);

        clock.set_rate_locked(false);
        client.set_rate(100_000_000).await.unwrap().unwrap();
        assert_eq!(clock.rate(), 100_000_000);

        clock.fail_next_get_rate(zx::Status::IO);
        assert_eq!(client.get_rate().await.unwrap(), Err(zx::Status::IO));
        assert_eq!(client.get_rate().await.unwrap().unwrap().hz, 100_000_000);

        clock.set_get_rate_error(Some(zx::Status::UNAVAILABLE));
        assert_eq!(client.get_rate().await.unwrap(), Err(zx::Status::UNAVAILABLE));
        clock.set_get_rate_error(None);
        assert_eq!(client.get_rate().await.unwrap().unwrap().hz, 100_000_000);
    }

    #[fuchsia::test]
    async fn votes_are_counted_per_connection() {
        let fake = FakeClock::new();
        let (client, _scope) = connect(&fake);
        assert!(!fake.enabled());

        enable(&client).await;
        enable(&client).await;
        disable(&client).await;
        assert!(fake.enabled());

        disable(&client).await;
        assert!(!fake.enabled());

        // Disabling without a vote is a no-op.
        disable(&client).await;
        assert!(!fake.enabled());
    }

    #[fuchsia::test]
    async fn connections_vote_independently() {
        let fake = FakeClock::new();
        let (client1, _scope1) = connect(&fake);
        let (client2, _scope2) = connect(&fake);

        enable(&client1).await;
        disable(&client2).await;
        assert!(fake.enabled());

        disable(&client1).await;
        assert!(!fake.enabled());
    }

    #[fuchsia::test]
    async fn closed_connection_orphans_vote_until_reclaimed() {
        let fake = FakeClock::new();
        let (client, scope) = connect(&fake);
        enable(&client).await;
        drop(client);
        scope.join().await;
        assert!(fake.enabled());
        assert_eq!(fake.orphan_count(), 1);

        let (client, _scope) = connect(&fake);
        enable(&client).await;
        assert_eq!(fake.orphan_count(), 0);
        disable(&client).await;
        assert!(!fake.enabled());
    }
}
