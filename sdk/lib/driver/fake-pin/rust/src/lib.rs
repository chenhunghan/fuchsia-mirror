// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_pin::{self as fpin, PinStatesServerHandler};
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use fuchsia_sync::Mutex;
use std::sync::Arc;

struct FakePinStatesState {
    current_state: String,
}

pub struct FakePinStates {
    state: Arc<Mutex<FakePinStatesState>>,
}

impl Default for FakePinStates {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePinStates {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(FakePinStatesState { current_state: String::new() })) }
    }

    pub fn current_state(&self) -> String {
        self.state.lock().current_state.clone()
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
        self.state.lock().current_state = request.payload().name;
        let _ = responder.respond(()).await;
    }
}
