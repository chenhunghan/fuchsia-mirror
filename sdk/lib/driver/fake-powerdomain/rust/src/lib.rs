// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fake `fuchsia.hardware.powerdomain/Domain` server.
//!
//! Enable and disable requests are tracked as per-connection votes, like a real power domain
//! provider (see `fdf_resource_vote`): every connection owns its own counted vote (each `Enable`
//! increments it, each `Disable` decrements it, saturating at zero), attributed to the service
//! instance name passed to [`FakePowerDomain::serve`]. The domain is enabled while any vote is
//! held. When a connection closes while holding a vote, the vote is kept as an orphan and is
//! reclaimed by the next `Enable` on a new connection to the same instance.

use fdf_resource_vote::{BoolVoteSet, CloseBehavior, Vote};
use fidl_fuchsia_driver_framework as fdf;
use fidl_next::Responder;
use fidl_next_fuchsia_hardware_powerdomain::{self as fpowerdomain, DomainServerHandler};
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

struct FakePowerDomainState {
    votes: BoolVoteSet,
    enable_calls: u32,
    disable_calls: u32,
    enable_error: InjectedError,
    disable_error: InjectedError,
}

#[derive(Clone)]
pub struct FakePowerDomain {
    state: Arc<Mutex<FakePowerDomainState>>,
}

impl Default for FakePowerDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePowerDomain {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakePowerDomainState {
                votes: BoolVoteSet::new_bool("fake-powerdomain"),
                enable_calls: 0,
                disable_calls: 0,
                enable_error: InjectedError::None,
                disable_error: InjectedError::None,
            })),
        }
    }

    /// Whether the domain is enabled, i.e. whether any vote (live or orphaned) is held.
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

    pub fn connect(&self, scope: &fasync::ScopeHandle) -> fidl_next::Client<fpowerdomain::Domain> {
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        server_end.spawn_on(
            FakePowerDomainServer {
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
        fdf_component::ServiceOffer::<fpowerdomain::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                FakePowerDomainService {
                    state: self.state.clone(),
                    scope,
                    node_key: instance_name.to_string(),
                },
            )
            .build_zircon_offer_next()
    }
}

struct FakePowerDomainService {
    state: Arc<Mutex<FakePowerDomainState>>,
    scope: fasync::ScopeHandle,
    node_key: String,
}

impl fpowerdomain::ServiceHandler for FakePowerDomainService {
    fn domain(&self, server_end: fidl_next::ServerEnd<fpowerdomain::Domain>) {
        server_end.spawn_on(
            FakePowerDomainServer {
                state: self.state.clone(),
                vote: Vote::new(self.node_key.clone(), CloseBehavior::OrphanOnClose),
            },
            &self.scope,
        );
    }
}

/// Serves one connection and owns its vote.
struct FakePowerDomainServer {
    state: Arc<Mutex<FakePowerDomainState>>,
    vote: Vote,
}

impl Drop for FakePowerDomainServer {
    fn drop(&mut self) {
        // The connection closed. Closing an orphan-on-close vote never changes the aggregate.
        let mut state = self.state.lock();
        let plan = state.votes.prepare_close(&self.vote);
        state.votes.commit(plan);
    }
}

impl DomainServerHandler for FakePowerDomainServer {
    async fn enable(&mut self, responder: Responder<fpowerdomain::domain::Enable>) {
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

    async fn disable(&mut self, responder: Responder<fpowerdomain::domain::Disable>) {
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

    async fn is_enabled(&mut self, responder: Responder<fpowerdomain::domain::IsEnabled>) {
        let enabled = self.state.lock().votes.enabled();
        let _ = responder.respond(enabled).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_next::fuchsia::create_channel;

    /// Connects to `fake` the same way `serve` does, on a dedicated scope that ends once the
    /// connection has been torn down.
    fn connect(fake: &FakePowerDomain) -> (fidl_next::Client<fpowerdomain::Domain>, fasync::Scope) {
        let scope = fasync::Scope::new_with_name("fake-powerdomain-connection");
        let service = FakePowerDomainService {
            state: fake.state.clone(),
            scope: scope.to_handle(),
            node_key: "default".to_string(),
        };
        let (client_end, server_end) = create_channel::<fpowerdomain::Domain>();
        fpowerdomain::ServiceHandler::domain(&service, server_end);
        (client_end.spawn(), scope)
    }

    async fn enable(client: &fidl_next::Client<fpowerdomain::Domain>) {
        assert!(client.enable().await.expect("FIDL error").is_ok());
    }

    async fn disable(client: &fidl_next::Client<fpowerdomain::Domain>) {
        assert!(client.disable().await.expect("FIDL error").is_ok());
    }

    #[fuchsia::test]
    async fn ref_counting_and_error_injection() {
        let scope = fasync::Scope::new_with_name("test");
        let domain = FakePowerDomain::new();
        let client = domain.connect(&scope.to_handle());

        assert!(!domain.enabled());
        assert_eq!(domain.ref_count(), 0);
        assert!(!client.is_enabled().await.unwrap().unwrap().enabled);

        domain.fail_next_enable(zx::Status::BAD_STATE);
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::BAD_STATE));
        assert!(!domain.enabled());
        assert_eq!(domain.enable_calls(), 1);

        domain.set_enable_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(client.enable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert!(!domain.enabled());
        assert_eq!(domain.ref_count(), 0);
        assert_eq!(domain.enable_calls(), 3);

        domain.set_enable_error(None);
        client.enable().await.unwrap().unwrap();
        client.enable().await.unwrap().unwrap();
        assert!(domain.enabled());
        assert_eq!(domain.ref_count(), 2);
        assert_eq!(domain.enable_calls(), 5);
        assert!(client.is_enabled().await.unwrap().unwrap().enabled);

        domain.set_disable_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.disable().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(domain.ref_count(), 2);
        assert_eq!(domain.disable_calls(), 1);

        domain.set_disable_error(None);
        client.disable().await.unwrap().unwrap();
        assert!(domain.enabled());
        assert_eq!(domain.ref_count(), 1);

        domain.fail_next_disable(zx::Status::IO);
        assert_eq!(client.disable().await.unwrap(), Err(zx::Status::IO));
        assert_eq!(domain.ref_count(), 1);

        client.disable().await.unwrap().unwrap();
        assert!(!domain.enabled());
        assert_eq!(domain.ref_count(), 0);
        assert_eq!(domain.disable_calls(), 4);

        client.disable().await.unwrap().unwrap();
        assert!(!domain.enabled());
        assert_eq!(domain.ref_count(), 0);
        assert_eq!(domain.disable_calls(), 5);
    }

    #[fuchsia::test]
    async fn votes_are_counted_per_connection() {
        let fake = FakePowerDomain::new();
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
        let fake = FakePowerDomain::new();
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
        let fake = FakePowerDomain::new();
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
