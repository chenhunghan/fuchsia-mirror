// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_pin::{self as fpin, PinStatesServerHandler};
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use fuchsia_sync::Mutex;
use std::collections::{HashMap, HashSet};
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
struct FakePinStatesState {
    current_state: String,
    supported_states: Option<HashSet<String>>,
    select_calls: u32,
    state_select_calls: HashMap<String, u32>,
    select_error: InjectedError,
    state_errors: HashMap<String, zx::Status>,
}

#[derive(Clone, Default)]
pub struct FakePinStates {
    state: Arc<Mutex<FakePinStatesState>>,
}

impl FakePinStates {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a fake pin controller that only accepts the given state names, returning
    /// `zx::Status::NOT_FOUND` for any other state.
    pub fn with_states<I, S>(states: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let supported_states = Some(states.into_iter().map(Into::into).collect());
        Self {
            state: Arc::new(Mutex::new(FakePinStatesState {
                supported_states,
                ..Default::default()
            })),
        }
    }

    pub fn current_state(&self) -> String {
        self.state.lock().current_state.clone()
    }

    pub fn select_calls(&self) -> u32 {
        self.state.lock().select_calls
    }

    pub fn state_select_calls(&self, name: &str) -> u32 {
        self.state.lock().state_select_calls.get(name).copied().unwrap_or(0)
    }

    /// Sets a sticky `SelectState` error across all states (`None` also clears any pending
    /// [`Self::fail_next_select_state`]).
    pub fn set_select_state_error(&self, error: Option<zx::Status>) {
        self.state.lock().select_error = InjectedError::from_option(error);
    }

    /// Fails the next `SelectState` call with `status` (sharing a slot with
    /// [`Self::set_select_state_error`]).
    pub fn fail_next_select_state(&self, status: zx::Status) {
        self.state.lock().select_error = InjectedError::Once(status);
    }

    /// Injects or clears an error returned whenever `name` is selected.
    pub fn set_state_error(&self, name: impl Into<String>, error: Option<zx::Status>) {
        let mut state = self.state.lock();
        match error {
            Some(status) => {
                state.state_errors.insert(name.into(), status);
            }
            None => {
                state.state_errors.remove(&name.into());
            }
        }
    }

    pub fn connect(&self, scope: &fasync::ScopeHandle) -> fidl_next::Client<fpin::PinStates> {
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        server_end.spawn_on(FakePinStatesServer { state: self.state.clone() }, scope);
        client_end.spawn_on(scope)
    }

    pub fn serve<O: ServiceObjTrait>(
        &self,
        service_fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        fdf_component::ServiceOffer::<fpin::PinStatesService>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                FakePinStatesService { state: self.state.clone(), scope },
            )
            .build_zircon_offer_next()
    }
}

struct FakePinStatesService {
    state: Arc<Mutex<FakePinStatesState>>,
    scope: fasync::ScopeHandle,
}

impl fpin::PinStatesServiceHandler for FakePinStatesService {
    fn device(&self, server_end: fidl_next::ServerEnd<fpin::PinStates>) {
        server_end.spawn_on(FakePinStatesServer { state: self.state.clone() }, &self.scope);
    }
}

struct FakePinStatesServer {
    state: Arc<Mutex<FakePinStatesState>>,
}

impl PinStatesServerHandler for FakePinStatesServer {
    async fn select_state(
        &mut self,
        request: Request<fpin::pin_states::SelectState>,
        responder: Responder<fpin::pin_states::SelectState>,
    ) {
        let name = request.payload().name;
        let result = {
            let mut state = self.state.lock();
            state.select_calls += 1;
            *state.state_select_calls.entry(name.clone()).or_insert(0) += 1;
            if let Some(status) = state.select_error.take() {
                Err(status)
            } else if let Some(&status) = state.state_errors.get(&name) {
                Err(status)
            } else if state
                .supported_states
                .as_ref()
                .is_some_and(|supported| !supported.contains(&name))
            {
                Err(zx::Status::NOT_FOUND)
            } else {
                state.current_state = name;
                Ok(())
            }
        };
        let _ = responder.respond_with(result).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn configurable_states_and_error_injection() {
        let scope = fasync::Scope::new_with_name("test");
        let pins = FakePinStates::with_states(["default"]);
        let client = pins.connect(&scope.to_handle());

        client.select_state("default").await.unwrap().unwrap();
        assert_eq!(pins.current_state(), "default");
        assert_eq!(pins.select_calls(), 1);
        assert_eq!(pins.state_select_calls("default"), 1);

        assert_eq!(client.select_state("sleep").await.unwrap(), Err(zx::Status::NOT_FOUND));
        assert_eq!(pins.current_state(), "default");
        assert_eq!(pins.state_select_calls("sleep"), 1);

        pins.set_state_error("sleep", Some(zx::Status::IO));
        assert_eq!(client.select_state("sleep").await.unwrap(), Err(zx::Status::IO));
        pins.set_state_error("sleep", None);

        pins.fail_next_select_state(zx::Status::BAD_STATE);
        assert_eq!(client.select_state("default").await.unwrap(), Err(zx::Status::BAD_STATE));
        client.select_state("default").await.unwrap().unwrap();

        pins.set_select_state_error(Some(zx::Status::UNAVAILABLE));
        assert_eq!(client.select_state("default").await.unwrap(), Err(zx::Status::UNAVAILABLE));
        pins.set_select_state_error(None);
        client.select_state("default").await.unwrap().unwrap();
    }
}
