// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_reset::{self as freset, ResetServerHandler};
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

#[derive(Default)]
struct FakeResetState {
    asserted: bool,
    toggled: bool,
    assert_calls: u32,
    deassert_calls: u32,
    toggle_calls: u32,
    assert_error: InjectedError,
    deassert_error: InjectedError,
    toggle_error: InjectedError,
}

#[derive(Clone, Default)]
pub struct FakeReset {
    state: Arc<Mutex<FakeResetState>>,
}

impl FakeReset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn asserted(&self) -> bool {
        self.state.lock().asserted
    }

    /// Returns and clears the current toggled state.
    pub fn take_toggled(&mut self) -> bool {
        std::mem::replace(&mut self.state.lock().toggled, false)
    }

    pub fn assert_calls(&self) -> u32 {
        self.state.lock().assert_calls
    }

    pub fn deassert_calls(&self) -> u32 {
        self.state.lock().deassert_calls
    }

    /// Returns the total number of `Toggle` and `ToggleWithTimeout` calls received.
    pub fn toggle_calls(&self) -> u32 {
        self.state.lock().toggle_calls
    }

    /// Sets a sticky `Assert` error (`None` also clears any pending [`Self::fail_next_assert`]).
    pub fn set_assert_error(&self, error: Option<zx::Status>) {
        self.state.lock().assert_error = InjectedError::from_option(error);
    }

    /// Fails the next `Assert` call with `status` (sharing a slot with [`Self::set_assert_error`]).
    pub fn fail_next_assert(&self, status: zx::Status) {
        self.state.lock().assert_error = InjectedError::Once(status);
    }

    /// Sets a sticky `Deassert` error (`None` also clears any pending
    /// [`Self::fail_next_deassert`]).
    pub fn set_deassert_error(&self, error: Option<zx::Status>) {
        self.state.lock().deassert_error = InjectedError::from_option(error);
    }

    /// Fails the next `Deassert` call with `status` (sharing a slot with
    /// [`Self::set_deassert_error`]).
    pub fn fail_next_deassert(&self, status: zx::Status) {
        self.state.lock().deassert_error = InjectedError::Once(status);
    }

    /// Sets a sticky error for `Toggle` and `ToggleWithTimeout` (`None` also clears any pending
    /// [`Self::fail_next_toggle`]).
    pub fn set_toggle_error(&self, error: Option<zx::Status>) {
        self.state.lock().toggle_error = InjectedError::from_option(error);
    }

    /// Fails the next `Toggle` or `ToggleWithTimeout` call with `status` (sharing a slot with
    /// [`Self::set_toggle_error`]).
    pub fn fail_next_toggle(&self, status: zx::Status) {
        self.state.lock().toggle_error = InjectedError::Once(status);
    }

    pub fn connect(&self, scope: &fasync::ScopeHandle) -> fidl_next::Client<freset::Reset> {
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        server_end.spawn_on(FakeResetServer { state: self.state.clone() }, scope);
        client_end.spawn_on(scope)
    }

    pub fn serve<O: ServiceObjTrait>(
        &self,
        service_fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        fdf_component::ServiceOffer::<freset::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                FakeResetService { state: self.state.clone(), scope },
            )
            .build_zircon_offer_next()
    }
}

struct FakeResetService {
    state: Arc<Mutex<FakeResetState>>,
    scope: fasync::ScopeHandle,
}

impl freset::ServiceHandler for FakeResetService {
    fn reset(&self, server_end: fidl_next::ServerEnd<freset::Reset>) {
        server_end.spawn_on(FakeResetServer { state: self.state.clone() }, &self.scope);
    }
}

struct FakeResetServer {
    state: Arc<Mutex<FakeResetState>>,
}

impl ResetServerHandler for FakeResetServer {
    async fn assert(&mut self, responder: Responder<freset::reset::Assert>) {
        let result = {
            let mut state = self.state.lock();
            state.assert_calls += 1;
            match state.assert_error.take() {
                Some(status) => Err(status),
                None => {
                    state.asserted = true;
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn deassert(&mut self, responder: Responder<freset::reset::Deassert>) {
        let result = {
            let mut state = self.state.lock();
            state.deassert_calls += 1;
            match state.deassert_error.take() {
                Some(status) => Err(status),
                None => {
                    state.asserted = false;
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn toggle(&mut self, responder: Responder<freset::reset::Toggle>) {
        let result = {
            let mut state = self.state.lock();
            state.toggle_calls += 1;
            match state.toggle_error.take() {
                Some(status) => Err(status),
                None => {
                    state.toggled = true;
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn toggle_with_timeout(
        &mut self,
        _request: Request<freset::reset::ToggleWithTimeout>,
        responder: Responder<freset::reset::ToggleWithTimeout>,
    ) {
        let result = {
            let mut state = self.state.lock();
            state.toggle_calls += 1;
            match state.toggle_error.take() {
                Some(status) => Err(status),
                None => {
                    state.toggled = true;
                    Ok(())
                }
            }
        };
        let _ = responder.respond_with(result).await;
    }

    async fn status(&mut self, responder: Responder<freset::reset::Status>) {
        let asserted = self.state.lock().asserted;
        let _ = responder.respond(asserted).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn call_tracking_and_error_injection() {
        let scope = fasync::Scope::new_with_name("test");
        let mut reset = FakeReset::new();
        let client = reset.connect(&scope.to_handle());

        reset.fail_next_assert(zx::Status::IO);
        assert_eq!(client.assert().await.unwrap(), Err(zx::Status::IO));
        assert!(!reset.asserted());
        assert_eq!(reset.assert_calls(), 1);

        reset.set_assert_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.assert().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(client.assert().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert!(!reset.asserted());
        assert_eq!(reset.assert_calls(), 3);

        reset.set_assert_error(None);
        client.assert().await.unwrap().unwrap();
        assert!(reset.asserted());
        assert!(client.status().await.unwrap().unwrap().asserted);
        assert_eq!(reset.assert_calls(), 4);

        reset.set_deassert_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.deassert().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(client.deassert().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert!(reset.asserted());
        assert_eq!(reset.deassert_calls(), 2);

        reset.set_deassert_error(None);
        reset.fail_next_deassert(zx::Status::IO);
        assert_eq!(client.deassert().await.unwrap(), Err(zx::Status::IO));
        assert!(reset.asserted());
        assert_eq!(reset.deassert_calls(), 3);

        client.deassert().await.unwrap().unwrap();
        assert!(!reset.asserted());
        assert_eq!(reset.deassert_calls(), 4);

        reset.fail_next_toggle(zx::Status::BAD_STATE);
        assert_eq!(client.toggle().await.unwrap(), Err(zx::Status::BAD_STATE));
        assert!(!reset.take_toggled());
        assert_eq!(reset.toggle_calls(), 1);

        reset.set_toggle_error(Some(zx::Status::TIMED_OUT));
        assert_eq!(client.toggle().await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(client.toggle_with_timeout(1_000).await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert!(!reset.take_toggled());
        assert_eq!(reset.toggle_calls(), 3);

        reset.set_toggle_error(None);
        client.toggle().await.unwrap().unwrap();
        assert!(reset.take_toggled());
        assert!(!reset.take_toggled());
        assert_eq!(reset.toggle_calls(), 4);

        client.toggle_with_timeout(1_000).await.unwrap().unwrap();
        assert!(reset.take_toggled());
        assert_eq!(reset.toggle_calls(), 5);
    }
}
