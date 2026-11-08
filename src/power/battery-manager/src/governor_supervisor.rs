// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Spawns and supervises dynamic charge governor child components in the `#governors` collection,
//! and serves `fuchsia.power.battery.test.GovernorDebug` and `fuchsia.power.battery.test.Governor`.
//!
//! Each configured governor receives its own `fuchsia.hardware.power.charger.Controller`,
//! `fuchsia.hardware.power.battery.Battery`, and `fuchsia.hardware.power.charger.Charger`
//! connectors in `CreateChildArgs.additional_inputs` so that [`ChargeLimitArbiter`] and
//! [`Cache`] attribute votes and status connections to the governor's child name. If a
//! governor stops or fails to create/start, it is restarted with capped exponential backoff.
//! [`Governors`] reports each governor's state and pauses or resumes it: a paused governor is
//! destroyed, which withdraws its votes, until it is resumed.
//!
//! ```text
//! governor child ── Controller ──────────► ChargeLimitArbiter ──► charger driver
//!                ◄─ Battery / Charger ───  Cache
//! GovernorRunner ── lifecycle ──► Governors ◄── GovernorDebug / Governor (battery_cli)
//! ```

use crate::Backoff;
use crate::arbiter::{ChargeLimitArbiter, DEBUG_OVERRIDE_VOTER, GovernorVotes};
use crate::cache::Cache;
use fidl::endpoints::{DiscoverableProtocolMarker, Responder as _, ServerEnd};
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery_test as spower;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_component::runtime::{Connector, ConnectorReceiver, Dictionary};
use fuchsia_inspect::{self as inspect, NumericProperty as _, Property as _};
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use log::{error, info, warn};
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;

/// Initial delay before restarting a charge governor that stopped or failed to start.
const GOVERNOR_RESTART_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

const GOVERNORS_COLLECTION: &str = "governors";
const INSPECTOR_NODE_GOVERNORS: &str = "governors";
const INSPECTOR_PROP_GOVERNOR_URL: &str = "url";
const INSPECTOR_PROP_GOVERNOR_STATE: &str = "state";
const INSPECTOR_PROP_GOVERNOR_RESTARTS: &str = "restarts";

/// Name of `lifecycle` in Inspect.
fn lifecycle_name(lifecycle: spower::GovernorLifecycle) -> &'static str {
    match lifecycle {
        spower::GovernorLifecycle::Stopped => "stopped",
        spower::GovernorLifecycle::Running => "running",
        spower::GovernorLifecycle::Paused => "paused",
        _ => "unknown",
    }
}

/// What a [`PauseRequest`] asks of a governor.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PauseCommand {
    Pause,
    Resume,
}

/// A request to pause or resume a governor, whose `done` sender fires once it is applied.
struct PauseRequest {
    command: PauseCommand,
    done: oneshot::Sender<()>,
}

/// State of one supervised governor, shared between its supervision task and [`Governors`], and
/// mirrored in Inspect. Only the supervision task changes `lifecycle`.
struct SupervisedGovernor {
    url: String,
    lifecycle: Cell<spower::GovernorLifecycle>,
    restarts: Cell<u64>,
    /// Pause and resume requests for the supervision task.
    requests: mpsc::UnboundedSender<PauseRequest>,
    inspect_lifecycle: inspect::StringProperty,
    inspect_restarts: inspect::UintProperty,
    _inspect_node: inspect::Node,
}

impl SupervisedGovernor {
    /// Returns the governor and the receiver of its pause and resume requests, which its
    /// supervision task serves.
    fn new(
        url: String,
        inspect_node: inspect::Node,
    ) -> (Self, mpsc::UnboundedReceiver<PauseRequest>) {
        const INITIAL_LIFECYCLE: spower::GovernorLifecycle = spower::GovernorLifecycle::Stopped;
        inspect_node.record_string(INSPECTOR_PROP_GOVERNOR_URL, &url);
        let (requests, request_receiver) = mpsc::unbounded();
        let governor = Self {
            url,
            lifecycle: Cell::new(INITIAL_LIFECYCLE),
            restarts: Cell::new(0),
            requests,
            inspect_lifecycle: inspect_node
                .create_string(INSPECTOR_PROP_GOVERNOR_STATE, lifecycle_name(INITIAL_LIFECYCLE)),
            inspect_restarts: inspect_node.create_uint(INSPECTOR_PROP_GOVERNOR_RESTARTS, 0),
            _inspect_node: inspect_node,
        };
        (governor, request_receiver)
    }

    fn set_lifecycle(&self, lifecycle: spower::GovernorLifecycle) {
        self.lifecycle.set(lifecycle);
        self.inspect_lifecycle.set(lifecycle_name(lifecycle));
    }

    fn record_restart(&self) {
        self.restarts.set(self.restarts.get() + 1);
        self.inspect_restarts.add(1);
    }

    /// Pauses the governor, destroying it and withdrawing its votes, or resumes it. Returns once
    /// the governor is torn down, or once it is set to restart.
    async fn request(&self, command: PauseCommand) -> Result<(), zx::Status> {
        let (done, applied) = oneshot::channel();
        self.requests
            .unbounded_send(PauseRequest { command, done })
            .map_err(|_| zx::Status::INTERNAL)?;
        applied.await.map_err(|_| zx::Status::INTERNAL)
    }
}

/// Waits for a request to `command` the governor and returns its `done` sender. Other requests,
/// which would not change its pause state, are acknowledged right away.
async fn next_request(
    requests: &mut mpsc::UnboundedReceiver<PauseRequest>,
    command: PauseCommand,
) -> oneshot::Sender<()> {
    loop {
        match requests.next().await {
            Some(request) if request.command == command => return request.done,
            Some(request) => {
                let _ = request.done.send(());
            }
            // The governor, which holds the sender, outlives its supervision task.
            None => future::pending::<()>().await,
        }
    }
}

/// Supervises the configured charge governors and serves `fuchsia.power.battery.test.GovernorDebug`
/// and `fuchsia.power.battery.test.Governor`.
pub(crate) struct Governors {
    governors: BTreeMap<String, Rc<SupervisedGovernor>>,
    arbiter: Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    _scope: fasync::Scope,
}

impl Governors {
    /// Spawns a supervision task for each configured governor URL and records its Inspect node
    /// under `governors/<child_name>`.
    pub(crate) fn spawn(
        governor_urls: &[String],
        arbiter: &Arc<ChargeLimitArbiter>,
        cache: Rc<Cache>,
        inspect_root: &inspect::Node,
    ) -> Rc<Self> {
        let realm = if governor_urls.is_empty() {
            None
        } else {
            match fclient::realm() {
                Ok(realm) => Some(realm),
                Err(e) => {
                    warn!("Failed to connect to Realm service; cannot spawn governors: {e:?}");
                    None
                }
            }
        };
        let scope = fasync::Scope::new();
        let mut governors = BTreeMap::new();
        if let Some(realm) = realm {
            inspect_root.record_child(INSPECTOR_NODE_GOVERNORS, |governors_node| {
                let mut used_names = HashSet::new();
                for (idx, url) in governor_urls.iter().enumerate() {
                    let child_name = governor_child_name(url, idx, &mut used_names);
                    let (governor, requests) = SupervisedGovernor::new(
                        url.clone(),
                        governors_node.create_child(&child_name),
                    );
                    let governor = Rc::new(governor);
                    governors.insert(child_name.clone(), governor.clone());
                    let runner = GovernorRunner::new(
                        realm.clone(),
                        arbiter.clone(),
                        cache.clone(),
                        child_name,
                        governor,
                    );
                    let _ = scope.spawn_local(runner.supervise(
                        scope.to_handle(),
                        requests,
                        GOVERNOR_RESTART_DELAY,
                    ));
                }
            });
        }
        Rc::new(Self { governors, arbiter: arbiter.clone(), cache, _scope: scope })
    }

    /// Serves `stream` until the client closes it, and each `Governor` it opens until that closes
    /// or `stream` ends.
    pub(crate) async fn serve_debug(
        self: Rc<Self>,
        mut stream: spower::GovernorDebugRequestStream,
    ) -> Result<(), fidl::Error> {
        // Dropped when `stream` ends, which stops serving the `Governor`s it opened.
        let opened = fasync::Scope::new();
        while let Some(request) = stream.try_next().await? {
            match request {
                spower::GovernorDebugRequest::GetState { responder } => {
                    responder.send(&self.state().await)?;
                }
                spower::GovernorDebugRequest::OpenGovernor { name, governor, responder } => {
                    let Some(supervised) = self.governors.get(&name).cloned() else {
                        responder.send(Err(zx::Status::NOT_FOUND.into_raw()))?;
                        continue;
                    };
                    let this = self.clone();
                    let _ = opened.spawn_local(async move {
                        if let Err(e) =
                            this.serve_governor(&name, &supervised, governor.into_stream()).await
                            && !e.is_closed()
                        {
                            warn!("Governor {name} failed: {e:?}");
                        }
                    });
                    responder.send(Ok(()))?;
                }
                spower::GovernorDebugRequest::SetControl { options, responder } => {
                    responder.send(self.arbiter.set_debug_override(&options).await)?;
                }
                spower::GovernorDebugRequest::ClearControl { options, responder } => {
                    responder.send(self.arbiter.clear_debug_override(&options).await)?;
                }
                spower::GovernorDebugRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("GovernorDebug: received unknown method ordinal: {ordinal}");
                }
            }
        }
        Ok(())
    }

    /// Serves governor `name`'s `Governor` `stream` until the client closes it. Closes it with an
    /// epitaph if the governor can no longer be paused or resumed.
    async fn serve_governor(
        &self,
        name: &str,
        governor: &SupervisedGovernor,
        mut stream: spower::GovernorRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.try_next().await? {
            match request {
                spower::GovernorRequest::GetState { responder } => {
                    let votes = self.arbiter.governor_votes(name).await;
                    responder.send(&self.governor_state(name, governor, votes))?;
                }
                spower::GovernorRequest::Pause { responder } => {
                    match governor.request(PauseCommand::Pause).await {
                        Ok(()) => responder.send()?,
                        Err(status) => responder.control_handle().shutdown_with_epitaph(status),
                    }
                }
                spower::GovernorRequest::Resume { responder } => {
                    match governor.request(PauseCommand::Resume).await {
                        Ok(()) => responder.send()?,
                        Err(status) => responder.control_handle().shutdown_with_epitaph(status),
                    }
                }
                spower::GovernorRequest::SetControl { options, responder } => {
                    responder.send(self.arbiter.set_governor_override(name, &options).await)?;
                }
                spower::GovernorRequest::ClearControl { options, responder } => {
                    responder.send(self.arbiter.clear_governor_override(name, &options).await)?;
                }
                spower::GovernorRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("Governor {name}: received unknown method ordinal: {ordinal}");
                }
            }
        }
        Ok(())
    }

    async fn state(&self) -> spower::ChargeControlState {
        let mut arbitration = self.arbiter.arbitration().await;
        let governors = self
            .governors
            .iter()
            .map(|(name, governor)| {
                let votes = arbitration.governors.remove(name).unwrap_or_default();
                self.governor_state(name, governor, votes)
            })
            .collect();
        spower::ChargeControlState {
            governors: Some(governors),
            overrides: Some(arbitration.debug_overrides),
            effective: Some(arbitration.effective),
            ..Default::default()
        }
    }

    fn governor_state(
        &self,
        name: &str,
        governor: &SupervisedGovernor,
        votes: GovernorVotes,
    ) -> spower::GovernorState {
        spower::GovernorState {
            sample_interval: self
                .cache
                .battery_sample_interval_vote(name)
                .map(zx::BootDuration::into_nanos),
            name: Some(name.to_owned()),
            url: Some(governor.url.clone()),
            lifecycle: Some(governor.lifecycle.get()),
            restarts: Some(governor.restarts.get()),
            votes: Some(votes.votes),
            overrides: Some(votes.overrides),
            ..Default::default()
        }
    }
}

/// Returns the child name for the governor at `url` (its manifest name, such as `msc` for
/// `...#meta/msc.cm`), suffixed with its index if another governor already uses that name or it
/// is [`DEBUG_OVERRIDE_VOTER`], which governors cannot vote under.
fn governor_child_name(url: &str, idx: usize, used: &mut HashSet<String>) -> String {
    let base_name = url
        .split_once('#')
        .and_then(|(_, fragment)| fragment.strip_prefix("meta/"))
        .and_then(|s| s.strip_suffix(".cm"))
        .unwrap_or("governor");
    let name = if used.contains(base_name) || base_name == DEBUG_OVERRIDE_VOTER {
        format!("{base_name}_{idx}")
    } else {
        base_name.to_owned()
    };
    used.insert(name.clone());
    name
}

/// Runs one configured governor as `child_name` in the `#governors` collection.
struct GovernorRunner {
    realm: fcomponent::RealmProxy,
    arbiter: Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    child_name: String,
    child_decl: fdecl::Child,
    child_ref: fdecl::ChildRef,
    governor: Rc<SupervisedGovernor>,
}

impl GovernorRunner {
    fn new(
        realm: fcomponent::RealmProxy,
        arbiter: Arc<ChargeLimitArbiter>,
        cache: Rc<Cache>,
        child_name: String,
        governor: Rc<SupervisedGovernor>,
    ) -> Self {
        let child_decl = fdecl::Child {
            name: Some(child_name.clone()),
            url: Some(governor.url.clone()),
            startup: Some(fdecl::StartupMode::Lazy),
            environment: None,
            ..Default::default()
        };
        let child_ref = fdecl::ChildRef {
            name: child_name.clone(),
            collection: Some(GOVERNORS_COLLECTION.into()),
        };
        Self { realm, arbiter, cache, child_name, child_decl, child_ref, governor }
    }

    /// Runs the governor forever, restarting it with capped exponential backoff starting at
    /// `restart_delay` whenever it stops or cannot be created or started, and serves its pause and
    /// resume `requests`.
    ///
    /// Each run serves the governor's inputs on its own child of `scope`. A pause ends the run,
    /// aborts those servers, destroys the child, and withdraws its votes before it is acknowledged,
    /// so that a paused governor holds no votes.
    async fn supervise(
        self,
        scope: fasync::ScopeHandle,
        mut requests: mpsc::UnboundedReceiver<PauseRequest>,
        restart_delay: zx::MonotonicDuration,
    ) {
        let child_name = &self.child_name;
        let mut backoff = Backoff::new(restart_delay);
        loop {
            let run_scope = scope.new_child();
            let paused = {
                let inputs_scope = run_scope.to_handle();
                let mut run = pin!(self.run(&inputs_scope, &mut backoff).fuse());
                let mut pause = pin!(next_request(&mut requests, PauseCommand::Pause).fuse());
                futures::select! {
                    never = run => match never {},
                    done = pause => done,
                }
            };
            run_scope.abort().await;
            self.destroy_child().await;
            self.arbiter.withdraw(child_name).await;
            self.governor.set_lifecycle(spower::GovernorLifecycle::Paused);
            info!("Paused charge governor {child_name}");
            let _ = paused.send(());

            let resumed = next_request(&mut requests, PauseCommand::Resume).await;
            self.governor.set_lifecycle(spower::GovernorLifecycle::Stopped);
            info!("Resumed charge governor {child_name}");
            let _ = resumed.send(());
        }
    }

    /// Runs the governor, serving its inputs on `scope`, and restarts it after `backoff` whenever
    /// it stops or cannot be created or started. Never returns.
    async fn run(&self, scope: &fasync::ScopeHandle, backoff: &mut Backoff) -> Infallible {
        loop {
            self.run_once(scope).await;
            self.destroy_child().await;
            self.governor.record_restart();
            backoff.wait().await;
        }
    }

    /// Destroys the governor child component if it exists.
    async fn destroy_child(&self) {
        let child_name = &self.child_name;
        match self.realm.destroy_child(&self.child_ref).await {
            Ok(Ok(())) | Ok(Err(fcomponent::Error::InstanceNotFound)) => {}
            Ok(Err(e)) => {
                warn!("Failed to destroy charge governor {child_name}: {e:?}");
            }
            Err(e) => {
                warn!("FIDL transport error destroying charge governor {child_name}: {e:?}");
            }
        }
    }

    /// Creates and starts the governor and waits until its `ExecutionController` reports `OnStop`
    /// (or closes).
    async fn run_once(&self, scope: &fasync::ScopeHandle) {
        let child_name = &self.child_name;
        let url = &self.governor.url;
        let (comp_ctrl_proxy, comp_ctrl_server) =
            fidl::endpoints::create_proxy::<fcomponent::ControllerMarker>();
        let inputs = create_governor_inputs(
            scope,
            self.arbiter.clone(),
            self.cache.clone(),
            child_name.clone(),
        )
        .await;
        let child_args = fcomponent::CreateChildArgs {
            controller: Some(comp_ctrl_server),
            additional_inputs: Some(inputs.handle),
            ..Default::default()
        };

        info!(
            "Spawning charge governor {child_name} ({url}) into collection #{GOVERNORS_COLLECTION}"
        );
        let collection_ref = fdecl::CollectionRef { name: GOVERNORS_COLLECTION.into() };
        match self.realm.create_child(&collection_ref, &self.child_decl, child_args).await {
            Ok(Ok(())) => {}
            Ok(Err(fcomponent::Error::InstanceAlreadyExists)) => {
                warn!("Charge governor {child_name} already exists; destroying before retry");
                return;
            }
            Ok(Err(e)) => {
                error!("Failed to spawn charge governor {child_name} ({url}): {e:?}");
                return;
            }
            Err(e) => {
                error!("FIDL transport error spawning charge governor {child_name} ({url}): {e:?}");
                return;
            }
        }

        let (exec_proxy, exec_server) =
            fidl::endpoints::create_proxy::<fcomponent::ExecutionControllerMarker>();
        match comp_ctrl_proxy.start(fcomponent::StartChildArgs::default(), exec_server).await {
            Ok(Ok(())) => {
                info!("Successfully started charge governor {child_name}");
                self.governor.set_lifecycle(spower::GovernorLifecycle::Running);
                let mut events = exec_proxy.take_event_stream();
                while let Some(event) = events.next().await {
                    if let Ok(fcomponent::ExecutionControllerEvent::OnStop { stopped_payload }) =
                        event
                    {
                        warn!(
                            "Charge governor {child_name} stopped ({stopped_payload:?}); \
                             restarting after backoff"
                        );
                        break;
                    }
                }
                self.governor.set_lifecycle(spower::GovernorLifecycle::Stopped);
            }
            Ok(Err(e)) => {
                warn!("Failed to start charge governor {child_name} ({url}): {e:?}");
            }
            Err(e) => {
                warn!("FIDL transport error starting charge governor {child_name} ({url}): {e:?}");
            }
        }
    }
}

/// Creates the input dictionary for governor `child_name`, holding:
/// - a `fuchsia.hardware.power.charger.Controller` connector whose connections `arbiter` serves
///   as `child_name`'s votes;
/// - `fuchsia.hardware.power.battery.Battery` and `fuchsia.hardware.power.charger.Charger`
///   connectors whose connections `cache` serves from battery-manager's cached status.
async fn create_governor_inputs(
    scope: &fasync::ScopeHandle,
    arbiter: Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    child_name: String,
) -> Dictionary {
    let (connector, receiver) = Connector::new().await;
    let inputs = Dictionary::new().await;
    inputs.insert(fcharger::ControllerMarker::PROTOCOL_NAME, connector).await;
    let _ = scope.spawn_local(serve_governor_controller(arbiter, child_name.clone(), receiver));
    let (battery_connector, battery_receiver) = Connector::new().await;
    inputs.insert(fbattery::BatteryMarker::PROTOCOL_NAME, battery_connector).await;
    let (charger_connector, charger_receiver) = Connector::new().await;
    inputs.insert(fcharger::ChargerMarker::PROTOCOL_NAME, charger_connector).await;
    let _ = scope.spawn_local(serve_governor_cache(
        cache,
        child_name,
        battery_receiver,
        charger_receiver,
    ));
    inputs
}

/// Serves each `Controller` connection that governor `child_name` opens through its connector,
/// voting under `child_name`. Runs until the connector is dropped along with the governor.
async fn serve_governor_controller(
    arbiter: Arc<ChargeLimitArbiter>,
    child_name: String,
    receiver: ConnectorReceiver,
) {
    receiver
        .for_each_concurrent(None, |channel| {
            let arbiter = arbiter.clone();
            let child_name = child_name.clone();
            async move {
                let stream = ServerEnd::<fcharger::ControllerMarker>::new(channel).into_stream();
                if let Err(e) = arbiter.serve_controller(child_name.clone(), stream).await {
                    error!("Charger Controller for governor {child_name} failed: {e:?}");
                }
            }
        })
        .await;
}

/// Serves each `Battery` and `Charger` connection that governor `child_name` opens through its
/// connectors from `cache`. Runs until the connectors are dropped along with the governor.
async fn serve_governor_cache(
    cache: Rc<Cache>,
    child_name: String,
    battery_receiver: ConnectorReceiver,
    charger_receiver: ConnectorReceiver,
) {
    let cache = &*cache;
    let child_name = child_name.as_str();
    let battery = battery_receiver.for_each_concurrent(None, |channel| async move {
        let stream = ServerEnd::<fbattery::BatteryMarker>::new(channel).into_stream();
        if let Err(e) = cache.serve_battery(child_name, stream).await
            && !e.is_closed()
        {
            warn!("Battery connection for governor {child_name} failed: {e:?}");
        }
    });
    let charger = charger_receiver.for_each_concurrent(None, |channel| async move {
        let stream = ServerEnd::<fcharger::ChargerMarker>::new(channel).into_stream();
        if let Err(e) = cache.serve_charger(child_name, stream).await
            && !e.is_closed()
        {
            warn!("Charger connection for governor {child_name} failed: {e:?}");
        }
    });
    futures::join!(battery, charger);
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async::TestExecutor;
    use std::task::Poll;

    /// Verifies that `governor_child_name` extracts the manifest stem and suffixes duplicate
    /// names, and the reserved debug override voter name, with their index.
    #[fuchsia::test]
    fn test_governor_child_name_uses_manifest_name_and_dedupes() {
        let mut used = HashSet::new();
        assert_eq!(
            governor_child_name("fuchsia-pkg://fuchsia.com/msc#meta/msc.cm", 0, &mut used),
            "msc"
        );
        assert_eq!(
            governor_child_name("fuchsia-boot:///thermal#meta/thermal.cm", 1, &mut used),
            "thermal"
        );
        assert_eq!(
            governor_child_name("fuchsia-pkg://fuchsia.com/msc2#meta/msc.cm", 2, &mut used),
            "msc_2"
        );
        assert_eq!(governor_child_name("fuchsia-pkg://fuchsia.com/bad", 3, &mut used), "governor");
        assert_eq!(
            governor_child_name("fuchsia-pkg://fuchsia.com/o#meta/debug_override.cm", 4, &mut used),
            "debug_override_4"
        );
    }

    /// Verifies that `Governors::spawn` is a no-op and does not record a `governors` Inspect node
    /// when `governor_urls` is empty.
    #[fuchsia::test]
    async fn test_spawn_empty_is_noop() {
        let inspector = inspect::Inspector::default();
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let cache = Rc::new(Cache::default());
        let governors = Governors::spawn(&[], &arbiter, cache, inspector.root());
        assert_data_tree!(inspector, root: {});
        assert_eq!(governors.state().await.governors, Some(vec![]));
    }

    /// Verifies that `GovernorRunner::supervise` creates and starts a child governor via `Realm`,
    /// sets `state = "running"` in Inspect while running, and destroys the child, sets
    /// `state = "stopped"`, increments `restarts`, and retries when `OnStop` fires.
    #[fuchsia::test]
    fn test_supervise_governor_starts_tracks_inspect_and_restarts_on_stop() {
        let mut exec = TestExecutor::new();
        let inspector = inspect::Inspector::default();
        let (governor, _arbiter, mut realm_stream, _scope) = start_supervised_governor(&inspector);
        let exec_ctrl_server = expect_governor_started(&mut exec, &mut realm_stream, &governor);
        assert_governor_inspect(&mut exec, &inspector, "running", 0);

        let (_, exec_control_handle) = exec_ctrl_server.into_stream_and_control_handle();
        exec_control_handle
            .send_on_stop(&fcomponent::StoppedPayload {
                status: Some(zx::Status::INTERNAL.into_raw()),
                exit_code: None,
                ..Default::default()
            })
            .unwrap();

        exec.run_singlethreaded(async {
            // Expect DestroyChild after OnStop.
            expect_destroy_child(&mut realm_stream).await;

            // Expect the next CreateChild attempt, which proves `state: "stopped"` and
            // `restarts: 1` have been recorded.
            match realm_stream.next().await.unwrap().unwrap() {
                fcomponent::RealmRequest::CreateChild { decl, .. } => {
                    assert_eq!(decl.name.as_deref(), Some(GOVERNOR_A));
                }
                other => panic!("unexpected RealmRequest: {other:?}"),
            }
        });
        assert_governor_inspect(&mut exec, &inspector, "stopped", 1);
    }

    /// Verifies that pausing a running governor returns only once its child is destroyed and its
    /// votes are withdrawn, that per-governor overrides are kept while paused without contributing
    /// and re-applied once the governor resumes and reconnects, and that duplicate pause/resume
    /// requests are acknowledged immediately.
    #[fuchsia::test(logging = false)]
    fn test_supervise_governor_pause_and_resume() {
        let mut exec = TestExecutor::new();
        let inspector = inspect::Inspector::default();
        let (governor, arbiter, mut realm_stream, _scope) = start_supervised_governor(&inspector);
        let _exec_ctrl_server = expect_governor_started(&mut exec, &mut realm_stream, &governor);
        let (controller, controller_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let _controller_task = fasync::Task::local(
            arbiter.clone().serve_controller(GOVERNOR_A.to_string(), controller_stream).map(|_| ()),
        );
        let votes = fcc(TEST_FCC_MID_UA);
        let overrides = fcc(TEST_FCC_LOW_UA);
        exec.run_singlethreaded(async {
            // Resuming an already-running governor is a no-op.
            assert_eq!(governor.request(PauseCommand::Resume).await, Ok(()));
            assert_eq!(controller.set_control(&votes).await.unwrap(), Ok(()));
            assert_eq!(arbiter.set_governor_override(GOVERNOR_A, &overrides).await, Ok(()));
            assert_eq!(
                arbiter.arbitration().await.governors.get(GOVERNOR_A),
                Some(&GovernorVotes { votes: votes.clone(), overrides: overrides.clone() })
            );
            assert_eq!(arbiter.arbitration().await.effective, overrides);

            let (paused, ()) = futures::join!(
                governor.request(PauseCommand::Pause),
                expect_destroy_child(&mut realm_stream)
            );
            assert_eq!(paused, Ok(()));
            // While paused, the governor's votes are withdrawn and its override does not
            // contribute, but the override is retained for its next connection.
            assert_eq!(
                arbiter.arbitration().await.governors.get(GOVERNOR_A),
                Some(&GovernorVotes { overrides: overrides.clone(), ..Default::default() })
            );
            assert_eq!(arbiter.arbitration().await.effective, fcharger::ControlOptions::default());
            // Pausing an already-paused governor is a no-op.
            assert_eq!(governor.request(PauseCommand::Pause).await, Ok(()));
        });
        // Once it acknowledges the pause, the supervision task waits for a resume, so it cannot
        // recreate the child.
        assert!(
            exec.run_until_stalled(&mut realm_stream.next()).is_pending(),
            "paused governor was recreated"
        );
        assert_eq!(governor.lifecycle.get(), spower::GovernorLifecycle::Paused);
        assert_eq!(governor.restarts.get(), 0);
        assert_governor_inspect(&mut exec, &inspector, "paused", 0);

        assert_eq!(exec.run_singlethreaded(governor.request(PauseCommand::Resume)), Ok(()));
        let _exec_ctrl_server = expect_governor_started(&mut exec, &mut realm_stream, &governor);
        assert_governor_inspect(&mut exec, &inspector, "running", 0);

        // When the resumed governor reconnects and casts votes, its retained override applies.
        let (controller, controller_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let _controller_task = fasync::Task::local(
            arbiter.clone().serve_controller(GOVERNOR_A.to_string(), controller_stream).map(|_| ()),
        );
        exec.run_singlethreaded(async {
            assert_eq!(controller.set_control(&votes).await.unwrap(), Ok(()));
            assert_eq!(arbiter.arbitration().await.effective, overrides);
        });
    }

    /// Verifies that pausing a governor while it is `Stopped` in backoff after a failed creation
    /// attempt cancels the backoff, ignores `InstanceNotFound` on `DestroyChild`, transitions to
    /// `Paused`, and does not retry until resumed.
    #[fuchsia::test(logging = false)]
    fn test_supervise_governor_pause_while_stopped_in_backoff() {
        let mut exec = TestExecutor::new();
        let inspector = inspect::Inspector::default();
        let (governor, _arbiter, mut realm_stream, _scope) =
            start_supervised_governor_with_delay(&inspector, GOVERNOR_RESTART_DELAY);

        exec.run_singlethreaded(async {
            match realm_stream.next().await.unwrap().unwrap() {
                fcomponent::RealmRequest::CreateChild { responder, .. } => {
                    responder.send(Err(fcomponent::Error::Internal)).unwrap();
                }
                other => panic!("unexpected RealmRequest: {other:?}"),
            }
            match realm_stream.next().await.unwrap().unwrap() {
                fcomponent::RealmRequest::DestroyChild { responder, .. } => {
                    responder.send(Err(fcomponent::Error::InstanceNotFound)).unwrap();
                }
                other => panic!("unexpected RealmRequest: {other:?}"),
            }
        });
        let _: Poll<()> = exec.run_until_stalled(&mut future::pending());
        assert_eq!(governor.lifecycle.get(), spower::GovernorLifecycle::Stopped);
        assert_governor_inspect(&mut exec, &inspector, "stopped", 1);

        exec.run_singlethreaded(async {
            let (paused, ()) = futures::join!(governor.request(PauseCommand::Pause), async {
                match realm_stream.next().await.unwrap().unwrap() {
                    fcomponent::RealmRequest::DestroyChild { responder, .. } => {
                        responder.send(Err(fcomponent::Error::InstanceNotFound)).unwrap();
                    }
                    other => panic!("unexpected RealmRequest: {other:?}"),
                }
            });
            assert_eq!(paused, Ok(()));
        });
        assert!(
            exec.run_until_stalled(&mut realm_stream.next()).is_pending(),
            "paused governor was recreated"
        );
        assert_eq!(governor.lifecycle.get(), spower::GovernorLifecycle::Paused);
        assert_governor_inspect(&mut exec, &inspector, "paused", 1);

        assert_eq!(exec.run_singlethreaded(governor.request(PauseCommand::Resume)), Ok(()));
        let _exec_ctrl_server = expect_governor_started(&mut exec, &mut realm_stream, &governor);
        assert_governor_inspect(&mut exec, &inspector, "running", 1);
    }

    /// Verifies that `GetState` reports each governor's sample interval, votes, and overrides, set
    /// through its `Governor`, with the overrides of every governor's votes and the values they
    /// elect, that clearing each override restores the governor's votes, and that validation
    /// errors propagate through both protocols.
    #[fuchsia::test]
    async fn test_state_reports_votes_and_overrides_at_both_scopes() {
        const SAMPLE_INTERVAL_NANOS: i64 = 5_000_000_000;
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let cache = Rc::new(Cache::default());
        let (battery, battery_stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::BatteryMarker>();
        let _battery_task = fasync::Task::local({
            let cache = cache.clone();
            async move { cache.serve_battery(GOVERNOR_A, battery_stream).await.unwrap() }
        });
        let _ = battery
            .configure_watch(&fbattery::WatchOptions {
                max_interval: Some(SAMPLE_INTERVAL_NANOS),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        let (controller, controller_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let _controller_task = fasync::Task::local(
            arbiter.clone().serve_controller(GOVERNOR_A.to_string(), controller_stream).map(|_| ()),
        );
        let (proxy, _task) = connect_debug_with_cache(unsupervised_governor(), arbiter, cache);
        let (governor, governor_server) = fidl::endpoints::create_proxy::<spower::GovernorMarker>();
        assert_eq!(proxy.open_governor(GOVERNOR_A, governor_server).await.unwrap(), Ok(()));
        assert_eq!(controller.set_control(&fcc(TEST_FCC_HIGH_UA)).await.unwrap(), Ok(()));

        let invalid = fcharger::ControlOptions { top_off_duration: Some(-1), ..Default::default() };
        assert_eq!(
            governor.set_control(&invalid).await.unwrap(),
            Err(fcharger::Error::InvalidArgs)
        );
        assert_eq!(proxy.set_control(&invalid).await.unwrap(), Err(fcharger::Error::InvalidArgs));

        assert_eq!(governor.set_control(&fcc(TEST_FCC_LOW_UA)).await.unwrap(), Ok(()));
        assert_eq!(proxy.set_control(&fcc(TEST_FCC_MID_UA)).await.unwrap(), Ok(()));
        let expected_state = spower::GovernorState {
            name: Some(GOVERNOR_A.to_string()),
            url: Some(GOVERNOR_A_URL.to_string()),
            lifecycle: Some(spower::GovernorLifecycle::Stopped),
            restarts: Some(0),
            sample_interval: Some(SAMPLE_INTERVAL_NANOS),
            votes: Some(fcc(TEST_FCC_HIGH_UA)),
            overrides: Some(fcc(TEST_FCC_LOW_UA)),
            ..Default::default()
        };
        assert_eq!(governor.get_state().await.unwrap(), expected_state);
        assert_eq!(
            proxy.get_state().await.unwrap(),
            spower::ChargeControlState {
                governors: Some(vec![expected_state]),
                overrides: Some(fcc(TEST_FCC_MID_UA)),
                effective: Some(fcc(TEST_FCC_MID_UA)),
                ..Default::default()
            }
        );

        let all = fcharger::ControlOptions::default();
        assert_eq!(proxy.clear_control(&all).await.unwrap(), Ok(()));
        assert_eq!(proxy.get_state().await.unwrap().effective, Some(fcc(TEST_FCC_LOW_UA)));
        assert_eq!(governor.clear_control(&all).await.unwrap(), Ok(()));
        assert_eq!(proxy.get_state().await.unwrap().effective, Some(fcc(TEST_FCC_HIGH_UA)));
    }

    /// Verifies that opening an unknown governor fails with `ZX_ERR_NOT_FOUND`, and that no
    /// governors are reported when none are configured.
    #[fuchsia::test(logging = false)]
    async fn test_unknown_governor() {
        let (proxy, _task) = connect_debug(BTreeMap::new(), Arc::new(ChargeLimitArbiter::new()));
        assert_eq!(proxy.get_state().await.unwrap().governors, Some(vec![]));
        let (_governor, governor_server) =
            fidl::endpoints::create_proxy::<spower::GovernorMarker>();
        assert_eq!(
            proxy.open_governor(GOVERNOR_A, governor_server).await.unwrap(),
            Err(zx::Status::NOT_FOUND.into_raw())
        );
    }

    /// Verifies that a `Governor` stops being served once the `GovernorDebug` connection that
    /// opened it closes.
    #[fuchsia::test]
    async fn test_governor_closes_with_its_governor_debug_connection() {
        let (proxy, task) =
            connect_debug(unsupervised_governor(), Arc::new(ChargeLimitArbiter::new()));
        let (governor, governor_server) = fidl::endpoints::create_proxy::<spower::GovernorMarker>();
        assert_eq!(proxy.open_governor(GOVERNOR_A, governor_server).await.unwrap(), Ok(()));
        assert!(governor.get_state().await.is_ok());

        drop(proxy);
        task.await;
        assert!(governor.get_state().await.unwrap_err().is_closed());
    }

    const GOVERNOR_A: &str = "governor_a";
    const GOVERNOR_A_URL: &str = "fuchsia-pkg://fuchsia.com/governor_a#meta/governor_a.cm";
    /// Charge current limits (in microamps) that the tests vote for and override with.
    const TEST_FCC_HIGH_UA: u32 = 2_000_000;
    const TEST_FCC_MID_UA: u32 = 1_000_000;
    const TEST_FCC_LOW_UA: u32 = 500_000;

    fn fcc(ua: u32) -> fcharger::ControlOptions {
        fcharger::ControlOptions { charge_current_limit_ua: Some(ua), ..Default::default() }
    }

    fn unsupervised_governor() -> BTreeMap<String, Rc<SupervisedGovernor>> {
        let inspector = inspect::Inspector::default();
        let (governor, _requests) = SupervisedGovernor::new(
            GOVERNOR_A_URL.to_owned(),
            inspector.root().create_child(GOVERNOR_A),
        );
        BTreeMap::from([(GOVERNOR_A.to_owned(), Rc::new(governor))])
    }

    fn connect_debug(
        governors: BTreeMap<String, Rc<SupervisedGovernor>>,
        arbiter: Arc<ChargeLimitArbiter>,
    ) -> (spower::GovernorDebugProxy, fasync::Task<()>) {
        connect_debug_with_cache(governors, arbiter, Rc::new(Cache::default()))
    }

    fn connect_debug_with_cache(
        governors: BTreeMap<String, Rc<SupervisedGovernor>>,
        arbiter: Arc<ChargeLimitArbiter>,
        cache: Rc<Cache>,
    ) -> (spower::GovernorDebugProxy, fasync::Task<()>) {
        let governors =
            Rc::new(Governors { governors, arbiter, cache, _scope: fasync::Scope::new() });
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<spower::GovernorDebugMarker>();
        let task = fasync::Task::local(async move {
            governors.serve_debug(stream).await.expect("GovernorDebug failed");
        });
        (proxy, task)
    }

    /// Asserts that `inspector` records governor `GOVERNOR_A` in `state` after `restarts` restarts.
    fn assert_governor_inspect(
        exec: &mut TestExecutor,
        inspector: &inspect::Inspector,
        state: &'static str,
        restarts: u64,
    ) {
        exec.run_singlethreaded(async {
            assert_data_tree!(inspector, root: {
                var GOVERNOR_A: {
                    url: GOVERNOR_A_URL,
                    state: state,
                    restarts: restarts,
                }
            });
        });
    }

    /// Starts supervising `GOVERNOR_A` against a fake `Realm`, recording Inspect under its name,
    /// and returns its arbiter.
    fn start_supervised_governor(
        inspector: &inspect::Inspector,
    ) -> (
        Rc<SupervisedGovernor>,
        Arc<ChargeLimitArbiter>,
        fcomponent::RealmRequestStream,
        fasync::Scope,
    ) {
        start_supervised_governor_with_delay(inspector, zx::MonotonicDuration::ZERO)
    }

    fn start_supervised_governor_with_delay(
        inspector: &inspect::Inspector,
        restart_delay: zx::MonotonicDuration,
    ) -> (
        Rc<SupervisedGovernor>,
        Arc<ChargeLimitArbiter>,
        fcomponent::RealmRequestStream,
        fasync::Scope,
    ) {
        let (governor, requests) = SupervisedGovernor::new(
            GOVERNOR_A_URL.to_string(),
            inspector.root().create_child(GOVERNOR_A),
        );
        let governor = Rc::new(governor);
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let (realm_proxy, realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcomponent::RealmMarker>();
        let scope = fasync::Scope::new();
        let runner = GovernorRunner::new(
            realm_proxy,
            arbiter.clone(),
            Rc::new(Cache::default()),
            GOVERNOR_A.to_string(),
            governor.clone(),
        );
        let _ = scope.spawn_local(runner.supervise(scope.to_handle(), requests, restart_delay));
        (governor, arbiter, realm_stream, scope)
    }

    /// Answers `CreateChild` and `Controller.Start` for `GOVERNOR_A`, checks that `governor` then
    /// reports that it is running, and returns its `ExecutionController` server end.
    fn expect_governor_started(
        exec: &mut TestExecutor,
        realm_stream: &mut fcomponent::RealmRequestStream,
        governor: &SupervisedGovernor,
    ) -> ServerEnd<fcomponent::ExecutionControllerMarker> {
        // Creating the governor's inputs waits on component_manager, so answer the requests on a
        // running executor.
        let exec_ctrl_server = exec.run_singlethreaded(answer_create_and_start(realm_stream));
        // Handling the `Start` reply needs no further IPC, so it completes before the executor
        // stalls.
        let _: Poll<()> = exec.run_until_stalled(&mut future::pending());
        assert_eq!(governor.lifecycle.get(), spower::GovernorLifecycle::Running);
        exec_ctrl_server
    }

    /// Answers `CreateChild` and `Controller.Start` for `GOVERNOR_A` and returns its
    /// `ExecutionController` server end.
    async fn answer_create_and_start(
        realm_stream: &mut fcomponent::RealmRequestStream,
    ) -> ServerEnd<fcomponent::ExecutionControllerMarker> {
        let comp_ctrl_server = match realm_stream.next().await.unwrap().unwrap() {
            fcomponent::RealmRequest::CreateChild { collection, decl, args, responder } => {
                assert_eq!(collection.name, GOVERNORS_COLLECTION);
                assert_eq!(decl.name.as_deref(), Some(GOVERNOR_A));
                assert_eq!(decl.url.as_deref(), Some(GOVERNOR_A_URL));
                assert!(args.additional_inputs.is_some());
                responder.send(Ok(())).unwrap();
                args.controller.unwrap()
            }
            other => panic!("unexpected RealmRequest: {other:?}"),
        };
        let mut comp_ctrl_stream = comp_ctrl_server.into_stream();
        let exec_ctrl_server = match comp_ctrl_stream.next().await.unwrap().unwrap() {
            fcomponent::ControllerRequest::Start { execution_controller, responder, .. } => {
                responder.send(Ok(())).unwrap();
                execution_controller
            }
            other => panic!("unexpected ControllerRequest: {other:?}"),
        };
        exec_ctrl_server
    }

    async fn expect_destroy_child(realm_stream: &mut fcomponent::RealmRequestStream) {
        match realm_stream.next().await.unwrap().unwrap() {
            fcomponent::RealmRequest::DestroyChild { child, responder } => {
                assert_eq!(child.name, GOVERNOR_A);
                assert_eq!(child.collection.as_deref(), Some(GOVERNORS_COLLECTION));
                responder.send(Ok(())).unwrap();
            }
            other => panic!("unexpected RealmRequest: {other:?}"),
        }
    }
}
