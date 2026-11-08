// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake `fuchsia.hardware.power.charger.Service` that serves `Charger` and `Controller`.

use anyhow::Result;
use fidl::endpoints::{Responder as _, ServiceMarker as _};
use fidl_fuchsia_hardware_power_charger as fcharger;
use fuchsia_component::server as fserver;
use fuchsia_component_test::LocalComponentHandles;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::future::FutureExt as _;
use futures::stream::{SelectAll, StreamExt as _};
use std::sync::Arc;

/// Default maximum charge current (in microamps) reported by `FakeCharger`.
pub const DEFAULT_CHARGER_MAX_CURRENT_UA: u32 = 3_000_000;
/// Default maximum charge voltage (in microvolts) reported by `FakeCharger`.
pub const DEFAULT_CHARGER_MAX_VOLTAGE_UV: u32 = 4_400_000;
/// Service instance that `FakeCharger::new` publishes.
const DEFAULT_INSTANCE: &str = "default";
/// Model that `FakeCharger::default_usb_charging` reports.
const DEFAULT_CHARGER_MODEL: &str = "FakeCharger";

/// Requests from `FakeCharger` to the component serving it, handled in order.
enum Command {
    PublishInstance(String),
    Disconnect,
}

struct State {
    spec: fcharger::Spec,
    status: fcharger::Status,
    /// `Charger.Watch` waiting for the next status change.
    pending_watch: Option<fcharger::ChargerWatchResponder>,
    /// Whether `status` changed since the last `Watch` reply.
    dirty: bool,
    /// Wake lease to send with the next `Watch` reply.
    next_wake_lease: Option<zx::EventPair>,
    /// Related koid of the wake lease passed to the most recent `Watch`.
    last_received_watch_lease_koid: Option<zx::Koid>,
    /// Service instance that accepts connections. Earlier instances stay in `ServiceFs`, which
    /// can't remove them, but reject new connections. `None` after `disconnect()`.
    active_instance: Option<String>,
    commands: mpsc::UnboundedSender<Command>,
    /// Taken by the component serving this charger.
    command_receiver: Option<mpsc::UnboundedReceiver<Command>>,
    /// Controls applied through `Controller.SetControl` and `Controller.ClearControl`.
    controls: fcharger::ControlOptions,
    /// Notified whenever `controls` changes.
    controls_changed: Vec<oneshot::Sender<()>>,
}

impl State {
    /// Applies `update` to `controls` and notifies waiters.
    fn update_controls(&mut self, update: impl FnOnce(&mut fcharger::ControlOptions)) {
        update(&mut self.controls);
        for sender in self.controls_changed.drain(..) {
            let _ = sender.send(());
        }
    }

    /// Replies to `Watch` with the current status and the pending wake lease.
    fn reply_to_watch(&mut self, responder: fcharger::ChargerWatchResponder) {
        self.dirty = false;
        let _ = responder.send(Ok((&self.status, self.next_wake_lease.take())));
    }

    /// Drops the pending `Watch` without closing its channel, which `run_fake_charger` closes
    /// once `Controller` is closed.
    fn drop_pending_watch(&mut self) {
        if let Some(responder) = self.pending_watch.take() {
            responder.drop_without_shutdown();
        }
    }
}

/// Fake implementation of `fuchsia.hardware.power.charger.Service` (`Charger` and `Controller`).
#[derive(Clone)]
pub struct FakeCharger {
    state: Arc<Mutex<State>>,
}

impl FakeCharger {
    /// Creates a `FakeCharger` and publishes its `"default"` service instance.
    pub fn new(spec: fcharger::Spec, initial_status: fcharger::Status) -> Self {
        let charger = Self::new_unpublished(spec, initial_status);
        charger.publish_instance(DEFAULT_INSTANCE);
        charger
    }

    /// Creates a `FakeCharger` without publishing a service instance yet.
    fn new_unpublished(spec: fcharger::Spec, initial_status: fcharger::Status) -> Self {
        let (commands, command_receiver) = mpsc::unbounded();
        Self {
            state: Arc::new(Mutex::new(State {
                spec,
                status: initial_status,
                pending_watch: None,
                dirty: false,
                next_wake_lease: None,
                last_received_watch_lease_koid: None,
                active_instance: None,
                commands,
                command_receiver: Some(command_receiver),
                controls: fcharger::ControlOptions::default(),
                controls_changed: Vec::new(),
            })),
        }
    }

    /// Status of a charger that is charging from USB in its fast-charge phase.
    pub fn default_usb_charging_status() -> fcharger::Status {
        fcharger::Status {
            online: Some(true),
            source_type: Some(fcharger::SourceType::Usb),
            operating_mode: Some(fcharger::OperatingMode::Charging),
            charge_phase: Some(fcharger::ChargePhase::Fast),
            ..Default::default()
        }
    }

    /// Creates a `FakeCharger` published as `"default"` and fast-charging from USB.
    pub fn default_usb_charging() -> Self {
        let charger = Self::default_usb_charging_unpublished();
        charger.publish_instance(DEFAULT_INSTANCE);
        charger
    }

    /// Creates an unpublished `FakeCharger` configured to fast-charge from USB.
    pub fn default_usb_charging_unpublished() -> Self {
        let spec = fcharger::Spec {
            max_charge_current_ua: Some(DEFAULT_CHARGER_MAX_CURRENT_UA),
            max_charge_voltage_uv: Some(DEFAULT_CHARGER_MAX_VOLTAGE_UV),
            model: Some(DEFAULT_CHARGER_MODEL.to_string()),
            ..Default::default()
        };
        Self::new_unpublished(spec, Self::default_usb_charging_status())
    }

    /// Publishes a charger service instance named `instance_name`, which then is the only
    /// instance that accepts connections.
    pub fn publish_instance(&self, instance_name: &str) {
        let mut state = self.state.lock();
        state.active_instance = Some(instance_name.to_string());
        let _ = state.commands.unbounded_send(Command::PublishInstance(instance_name.to_string()));
    }

    /// Updates the charger's status and notifies any pending `Watch` caller.
    pub fn set_status(&self, status: fcharger::Status) {
        self.set_status_with_wake_lease(status, None);
    }

    /// Updates the charger's status with an optional `wake_lease` and notifies any pending `Watch`.
    pub fn set_status_with_wake_lease(
        &self,
        status: fcharger::Status,
        wake_lease: Option<zx::EventPair>,
    ) {
        let mut state = self.state.lock();
        state.status = status;
        state.next_wake_lease = wake_lease;
        match state.pending_watch.take() {
            Some(responder) => state.reply_to_watch(responder),
            None => state.dirty = true,
        }
    }

    /// Closes all active `Charger` and `Controller` connections, stops accepting new ones until
    /// the next `publish_instance`, and resets recorded controls.
    pub fn disconnect(&self) {
        let mut state = self.state.lock();
        state.active_instance = None;
        // Status changes from here on must not reach the connections being closed.
        state.drop_pending_watch();
        state.update_controls(|controls| *controls = fcharger::ControlOptions::default());
        let _ = state.commands.unbounded_send(Command::Disconnect);
    }

    /// Returns the related koid of the wake lease passed to the most recent `Charger.Watch` call.
    pub fn last_received_watch_lease_related_koid(&self) -> Option<zx::Koid> {
        self.state.lock().last_received_watch_lease_koid
    }

    /// Waits until the controls applied through `Controller` equal `expected`.
    pub async fn wait_for_controls(&self, expected: &fcharger::ControlOptions) {
        let _ = self.wait_for_controls_matching(|controls| controls == expected).await;
    }

    /// Waits until the controls applied through `Controller` satisfy `predicate`, and returns
    /// them.
    pub async fn wait_for_controls_matching(
        &self,
        mut predicate: impl FnMut(&fcharger::ControlOptions) -> bool,
    ) -> fcharger::ControlOptions {
        loop {
            let changed = {
                let mut state = self.state.lock();
                if predicate(&state.controls) {
                    return state.controls.clone();
                }
                let (sender, receiver) = oneshot::channel();
                state.controls_changed.push(sender);
                receiver
            };
            let _ = changed.await;
        }
    }

    fn handle_controller_request(&self, request: fcharger::ControllerRequest) {
        match request {
            fcharger::ControllerRequest::SetControl { options, responder } => {
                self.state.lock().update_controls(|c| governor::merge_vote(c, &options));
                let _ = responder.send(Ok(()));
            }
            fcharger::ControllerRequest::ClearControl { options, responder } => {
                self.state.lock().update_controls(|c| governor::clear_vote_fields(c, &options));
                let _ = responder.send(Ok(()));
            }
            _ => panic!("FakeCharger: unimplemented Controller method"),
        }
    }

    /// Handles `request` on a `Charger` connection. The first `Watch` on each connection replies
    /// immediately; later ones wait for a status change.
    fn handle_charger_request(&self, request: fcharger::ChargerRequest, first_watch: bool) {
        let mut state = self.state.lock();
        match request {
            fcharger::ChargerRequest::GetSpec { responder } => {
                let _ = responder.send(Ok(&state.spec));
            }
            fcharger::ChargerRequest::GetStatus { responder } => {
                let _ = responder.send(Ok(&state.status));
            }
            fcharger::ChargerRequest::ConfigureWatch { options, responder } => {
                let _ = responder.send(Ok(&options));
            }
            fcharger::ChargerRequest::Watch { lease, responder } => {
                if let Some(info) = lease.and_then(|lease| lease.basic_info().ok()) {
                    state.last_received_watch_lease_koid = Some(info.related_koid);
                }
                if first_watch || state.dirty {
                    state.reply_to_watch(responder);
                } else {
                    state.pending_watch = Some(responder);
                }
            }
            _ => panic!("FakeCharger: unimplemented Charger method"),
        }
    }
}

/// Next thing for `run_fake_charger` to handle.
enum Event {
    Command(Command),
    /// A connection request to a service instance, or `None` once the outgoing directory closes.
    ServiceRequest(Option<(String, fcharger::ServiceRequest)>),
    ControllerRequest(fcharger::ControllerRequest),
    ChargerRequest {
        request: fcharger::ChargerRequest,
        first_watch: bool,
    },
}

/// Serves `charger` as a local component until its outgoing directory closes.
pub(crate) async fn run_fake_charger(
    handles: LocalComponentHandles,
    charger: FakeCharger,
) -> Result<()> {
    let mut commands = charger
        .state
        .lock()
        .command_receiver
        .take()
        .expect("run_fake_charger called more than once");
    let mut fs = fserver::ServiceFs::new();
    // Create the service directory up front, so that `Service::open(...).watch()` observes
    // instances published later.
    let _ = fs.dir("svc").dir(fcharger::ServiceMarker::SERVICE_NAME);
    fs.serve_connection(handles.outgoing_dir)?;

    let mut controllers = SelectAll::<fcharger::ControllerRequestStream>::new();
    let mut chargers = SelectAll::new();
    loop {
        // Commands go first, so that instances published before the component started exist
        // before any request is served, and commands apply in the order the test issued them.
        let event = futures::select_biased! {
            command = commands.select_next_some() => Event::Command(command),
            request = fs.next().fuse() => Event::ServiceRequest(request),
            request = controllers.select_next_some() => {
                Event::ControllerRequest(request.expect("failed to serve Controller"))
            },
            (request, first_watch) = chargers.select_next_some() => {
                Event::ChargerRequest { request, first_watch }
            },
        };
        match event {
            Event::Command(Command::PublishInstance(instance_name)) => {
                fs.dir("svc").add_fidl_service_instance(
                    instance_name.clone(),
                    move |request: fcharger::ServiceRequest| (instance_name.clone(), request),
                );
            }
            Event::Command(Command::Disconnect) => {
                // Close Controller before Charger, so that battery-manager sees its Controller
                // closed as soon as Charger.Watch fails, and finds the charger again rather than
                // reconnecting to this instance.
                controllers.clear();
                charger.state.lock().drop_pending_watch();
                chargers.clear();
            }
            Event::ServiceRequest(None) => break,
            Event::ServiceRequest(Some((instance_name, request))) => {
                if charger.state.lock().active_instance.as_ref() != Some(&instance_name) {
                    continue;
                }
                match request {
                    fcharger::ServiceRequest::Charger(stream) => {
                        // Tag each request with whether it is the connection's first `Watch`.
                        chargers.push(stream.scan(false, |watched, request| {
                            let request = request.expect("failed to serve Charger");
                            let is_watch =
                                matches!(request, fcharger::ChargerRequest::Watch { .. });
                            let first_watch = is_watch && !*watched;
                            *watched |= is_watch;
                            futures::future::ready(Some((request, first_watch)))
                        }));
                    }
                    fcharger::ServiceRequest::Controller(stream) => controllers.push(stream),
                }
            }
            Event::ControllerRequest(request) => charger.handle_controller_request(request),
            Event::ChargerRequest { request, first_watch } => {
                charger.handle_charger_request(request, first_watch)
            }
        }
    }
    Ok(())
}
