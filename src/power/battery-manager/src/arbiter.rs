// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Multi-governor charging limit arbitration engine.
//!
//! [`ChargeLimitArbiter`] proxies [`fcharger::ControllerMarker`] for internal charge governors
//! (such as Multi-Step Charging and thermal governors). Each governor votes under its own name,
//! and may hold at most one `Controller` connection at a time:
//! - [`fcharger::ControllerRequest::SetControl`] casts the present (`Some`) fields as the
//!   governor's votes without clearing omitted (`None`) fields.
//! - [`fcharger::ControllerRequest::ClearControl`] withdraws the governor's votes for the fields
//!   present in the mask (or all fields when the mask is empty).
//! - Closing the connection withdraws all of the governor's votes.
//!
//! Debug overrides set through [`ChargeLimitArbiter::set_debug_override`] are cast by
//! [`DEBUG_OVERRIDE_VOTER`] and take precedence over every governor's votes on the fields they
//! set, until cleared. [`ChargeLimitArbiter::set_governor_override`] instead overrides one
//! governor's votes on chosen fields while it is connected, without changing the votes it casts,
//! so that it still competes with the other governors.
//!
//! Each field is arbitrated through its own [`Election`], and the vote that charges least wins:
//! fields where lower values charge less (`input_current_limit_ua`, `charge_current_limit_ua`,
//! `float_voltage_uv`, `top_off_duration`) take the minimum active vote, fields where higher values
//! charge less (`input_voltage_limit_uv`, `charge_term_current_ua`) take the maximum, and
//! `operating_mode` takes the highest-precedence active vote
//! (`Discharging > Passthrough > Charging`).
//! `OperatingMode::Otg` is rejected with [`fcharger::Error::NotSupported`] on this control path.
//!
//! Only changed effective fields are forwarded to the downstream charger driver: changed values
//! with `SetControl`, then withdrawn fields with `ClearControl`. Active votes are cached while no
//! driver is bound, including after its channel fails, and replayed when a driver connects; if the
//! driver rejects an update, the caller's previous votes are restored and the driver is re-synced.

use election::Election;
use fidl::endpoints::RequestStream;
use fidl_fuchsia_hardware_power_charger as fcharger;
use futures::StreamExt;
use futures::lock::Mutex;
use governor::{clear_vote_fields, diff_controls, merge_vote};
use log::{debug, error, info, warn};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Voter that casts debug overrides, which win over every governor's votes. No governor may vote
/// under this name.
pub(crate) const DEBUG_OVERRIDE_VOTER: &str = "debug_override";

/// One voter's votes and the overrides of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GovernorVotes {
    /// The votes the voter casts.
    pub(crate) votes: fcharger::ControlOptions,
    /// Overrides of its votes set through [`ChargeLimitArbiter::set_governor_override`].
    pub(crate) overrides: fcharger::ControlOptions,
}

impl GovernorVotes {
    /// Returns what the voter contributes to the elections: its votes, with its overrides applied
    /// while it is `connected`, so that a paused governor has no influence.
    fn contribution(&self, connected: bool) -> fcharger::ControlOptions {
        let mut votes = self.votes.clone();
        if connected {
            merge_vote(&mut votes, &self.overrides);
        }
        votes
    }
}

/// A snapshot of the arbitration: each governor's votes, the overrides of every governor's votes,
/// and the effective values they elect.
#[derive(Debug, PartialEq)]
pub(crate) struct Arbitration {
    /// Votes of the governors that cast any or have overrides, keyed by governor name.
    pub(crate) governors: BTreeMap<String, GovernorVotes>,
    /// Debug overrides set through [`ChargeLimitArbiter::set_debug_override`].
    pub(crate) debug_overrides: fcharger::ControlOptions,
    /// The values sent to the charger driver.
    pub(crate) effective: fcharger::ControlOptions,
}

const INSPECTOR_NODE_ELECTIONS: &str = "elections";
const INSPECTOR_PROP_OPERATING_MODE: &str = "operating_mode";

/// Invokes `$m!(field)` for every numeric limit field in [`fcharger::ControlOptions`] that the
/// arbiter supports, so adding a new limit updates every per-field operation together.
macro_rules! for_each_limit_field {
    ($m:ident) => {
        $m!(input_current_limit_ua);
        $m!(charge_current_limit_ua);
        $m!(float_voltage_uv);
        $m!(charge_term_current_ua);
        $m!(input_voltage_limit_uv);
        $m!(top_off_duration);
    };
}

/// Invokes `$m!(field)` for every supported field in [`fcharger::ControlOptions`] (`operating_mode`
/// and all numeric limit fields).
/// Keep it in step with `for_each_control_field!` in `//src/power/lib/governor`.
macro_rules! for_each_control_field {
    ($m:ident) => {
        $m!(operating_mode);
        for_each_limit_field!($m);
    };
}

/// Ranks [`fcharger::OperatingMode`] votes by precedence (`Discharging > Passthrough > Charging`).
fn cmp_operating_mode(a: &fcharger::OperatingMode, b: &fcharger::OperatingMode) -> Ordering {
    fn rank(mode: &fcharger::OperatingMode) -> u8 {
        match mode {
            fcharger::OperatingMode::Charging => 1,
            fcharger::OperatingMode::Passthrough => 2,
            fcharger::OperatingMode::Discharging => 3,
            _ => 0,
        }
    }
    rank(a).cmp(&rank(b))
}

/// Returns `options` with every supported field removed, so that a non-default result means
/// `options` sets a field the arbiter does not support.
fn unsupported_fields(options: &fcharger::ControlOptions) -> fcharger::ControlOptions {
    let mut rest = options.clone();
    macro_rules! strip {
        ($f:ident) => {
            rest.$f = None;
        };
    }
    for_each_control_field!(strip);
    rest
}

/// Validates `SetControl` options before mutating voter state.
fn validate_set_options(options: &fcharger::ControlOptions) -> Result<(), fcharger::Error> {
    if unsupported_fields(options) != fcharger::ControlOptions::default() {
        return Err(fcharger::Error::NotSupported);
    }
    if !matches!(
        options.operating_mode,
        None | Some(
            fcharger::OperatingMode::Charging
                | fcharger::OperatingMode::Passthrough
                | fcharger::OperatingMode::Discharging
        )
    ) {
        return Err(fcharger::Error::NotSupported);
    }
    // A negative duration can't be programmed, and as the minimum it would win every election.
    if options.top_off_duration.is_some_and(|duration| duration < 0) {
        return Err(fcharger::Error::InvalidArgs);
    }
    Ok(())
}

/// Validates `ClearControl` mask options before mutating voter state.
fn validate_clear_options(mask: &fcharger::ControlOptions) -> Result<(), fcharger::Error> {
    if unsupported_fields(mask) != fcharger::ControlOptions::default() {
        return Err(fcharger::Error::NotSupported);
    }
    Ok(())
}

/// Per-field [`Election`] tables keyed by governor name.
#[derive(Debug)]
struct ControlVotes {
    operating_mode: Election<String, fcharger::OperatingMode>,
    input_current_limit_ua: Election<String, u32>,
    charge_current_limit_ua: Election<String, u32>,
    float_voltage_uv: Election<String, u32>,
    charge_term_current_ua: Election<String, u32>,
    input_voltage_limit_uv: Election<String, u32>,
    top_off_duration: Election<String, i64>,
}

/// Makes [`DEBUG_OVERRIDE_VOTER`]'s votes on `election` win over every governor's.
fn overridable<V: Copy>(election: Election<String, V>) -> Election<String, V> {
    election.with_override_voter(DEBUG_OVERRIDE_VOTER.to_owned())
}

impl Default for ControlVotes {
    fn default() -> Self {
        Self {
            operating_mode: overridable(Election::new(cmp_operating_mode)),
            input_current_limit_ua: overridable(Election::min()),
            charge_current_limit_ua: overridable(Election::min()),
            float_voltage_uv: overridable(Election::min()),
            charge_term_current_ua: overridable(Election::max()),
            input_voltage_limit_uv: overridable(Election::max()),
            top_off_duration: overridable(Election::min()),
        }
    }
}

impl ControlVotes {
    /// Returns `voter`'s current votes across all fields.
    #[cfg(test)]
    fn votes_of(&self, voter: &str) -> fcharger::ControlOptions {
        let mut votes = fcharger::ControlOptions::default();
        macro_rules! get {
            ($f:ident) => {
                votes.$f = self.$f.vote(voter);
            };
        }
        for_each_control_field!(get);
        votes
    }

    /// Replaces `voter`'s votes on every field with `votes` (`None` withdraws).
    fn replace(&self, voter: &str, votes: &fcharger::ControlOptions) {
        macro_rules! set {
            ($f:ident) => {
                self.$f.set_vote(voter.to_owned(), votes.$f);
            };
        }
        for_each_control_field!(set);
    }

    /// Returns the effective (winning) value of every field across all voters.
    fn effective(&self) -> fcharger::ControlOptions {
        let mut effective = fcharger::ControlOptions::default();
        macro_rules! effective {
            ($f:ident) => {
                effective.$f = self.$f.result();
            };
        }
        for_each_control_field!(effective);
        effective
    }

    /// Records each field's [`Election`] under `node/elections`.
    fn record_inspect(&self, node: &fuchsia_inspect::Node) {
        node.record_child(INSPECTOR_NODE_ELECTIONS, |elections_node| {
            self.operating_mode.record_inspect_with(
                elections_node,
                INSPECTOR_PROP_OPERATING_MODE,
                |mode, node, key| node.record_string(key, format!("{mode:?}")),
            );
            macro_rules! record {
                ($f:ident) => {
                    self.$f.record_inspect(elections_node, stringify!($f));
                };
            }
            for_each_limit_field!(record);
        });
    }
}

/// Clears the fields of `options` that `mask` selects, or every field if `mask` is empty, as
/// `ClearControl` does.
fn clear_masked(options: &mut fcharger::ControlOptions, mask: &fcharger::ControlOptions) {
    if *mask == fcharger::ControlOptions::default() {
        *options = fcharger::ControlOptions::default();
    } else {
        clear_vote_fields(options, mask);
    }
}

#[derive(Debug, Default)]
struct ArbiterState {
    /// Per-field votes keyed by voter: what each voter contributes.
    votes: ControlVotes,
    /// Each voter's votes and overrides. Overrides are kept across reconnections.
    voters: BTreeMap<String, GovernorVotes>,
    /// Governors that currently hold a `Controller` connection.
    connected: BTreeSet<String>,
    /// Constraints last acknowledged by the downstream driver.
    applied: fcharger::ControlOptions,
    driver: Option<fcharger::ControllerProxy>,
}

impl ArbiterState {
    /// Synchronizes the downstream hardware driver with the current effective limits.
    ///
    /// Sends changed values with `set_control`, then withdraws fields with `clear_control`, so that
    /// no field is left unconstrained while the others change. `set_control` may itself relax a
    /// limit, for example by raising it.
    async fn sync_driver(&mut self) -> Result<(), fcharger::Error> {
        let desired = self.votes.effective();
        let Some(driver) = self.driver.clone() else {
            debug!("ChargeLimitArbiter: cached effective vote (no driver connected): {desired:?}");
            return Ok(());
        };

        let (to_set, to_clear) = diff_controls(&self.applied, &desired);

        if to_set != fcharger::ControlOptions::default() {
            info!("ChargeLimitArbiter: applying SetControl to driver: {to_set:?}");
            // Record the in-flight fields before awaiting so a `withdraw` after `serve_controller`
            // is cancelled mid-call still clears them from the driver; restored if rejected.
            let prev_applied = self.applied.clone();
            merge_vote(&mut self.applied, &to_set);
            let result = driver.set_control(&to_set).await;
            self.handle_driver_result("SetControl", prev_applied, result)?;
        }

        if to_clear != fcharger::ControlOptions::default() {
            info!("ChargeLimitArbiter: applying ClearControl to driver: {to_clear:?}");
            let prev_applied = self.applied.clone();
            clear_vote_fields(&mut self.applied, &to_clear);
            let result = driver.clear_control(&to_clear).await;
            self.handle_driver_result("ClearControl", prev_applied, result)?;
        }

        Ok(())
    }

    /// Maps a driver response to the arbiter's result, restoring `prev_applied` if the driver
    /// rejected the request or dropping the driver on any FIDL transport error.
    fn handle_driver_result(
        &mut self,
        method: &str,
        prev_applied: fcharger::ControlOptions,
        result: Result<Result<(), fcharger::Error>, fidl::Error>,
    ) -> Result<(), fcharger::Error> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                warn!("Hardware charger driver rejected {method}: {err:?}");
                self.applied = prev_applied;
                Err(err)
            }
            Err(fidl_err) => {
                error!("FIDL error calling charger driver {method}: {fidl_err:?}");
                self.driver = None;
                self.applied = fcharger::ControlOptions::default();
                Err(fcharger::Error::Io)
            }
        }
    }

    /// Stores `voter`'s votes and overrides, forgetting it once both are empty, and updates its
    /// contribution to the elections.
    fn store(&mut self, voter: &str, entry: GovernorVotes) {
        self.votes.replace(voter, &entry.contribution(self.connected.contains(voter)));
        if entry == GovernorVotes::default() {
            self.voters.remove(voter);
        } else {
            self.voters.insert(voter.to_owned(), entry);
        }
    }

    /// Forgets `governor`'s connection, withdraws its votes, and syncs the driver. Its
    /// overrides, if any, are kept for its next connection.
    async fn withdraw(&mut self, governor: &str) {
        debug!("ChargeLimitArbiter: governor {governor} disconnected");
        self.connected.remove(governor);
        let overrides =
            self.voters.get(governor).map(|voter| voter.overrides.clone()).unwrap_or_default();
        self.store(governor, GovernorVotes { overrides, ..Default::default() });
        if let Err(e) = self.sync_driver().await {
            warn!(
                "ChargeLimitArbiter: failed to sync driver after governor {governor} \
                 disconnected: {e:?}"
            );
        }
    }

    /// Applies `update` to `voter`'s votes and overrides and syncs the driver, restoring both if
    /// the driver rejects the new effective values. If the driver's channel fails, the new votes
    /// stay cached, as when no driver is bound, and are replayed when one binds.
    async fn update_votes(
        &mut self,
        voter: &str,
        update: impl FnOnce(&mut GovernorVotes),
    ) -> Result<(), fcharger::Error> {
        let prev = self.voters.get(voter).cloned().unwrap_or_default();
        let mut next = prev.clone();
        update(&mut next);
        self.store(voter, next);
        if let Err(err) = self.sync_driver().await {
            if self.driver.is_none() {
                return Ok(());
            }
            self.store(voter, prev);
            // The failed sync may have partially applied the new effective values (e.g. a
            // `SetControl` relaxing one field succeeded before a `ClearControl` failed), so
            // re-sync to bring the driver back in line with the restored votes.
            if let Err(e) = self.sync_driver().await {
                warn!("ChargeLimitArbiter: failed to restore driver after rollback: {e:?}");
            }
            return Err(err);
        }
        Ok(())
    }
}

/// Arbitrates `fuchsia.hardware.power.charger.Controller` votes across governors and forwards
/// the effective configuration to the hardware charger driver.
pub(crate) struct ChargeLimitArbiter {
    state: Mutex<ArbiterState>,
}

impl ChargeLimitArbiter {
    pub(crate) fn new() -> Self {
        Self { state: Mutex::new(ArbiterState::default()) }
    }

    /// Sets or clears the downstream hardware charger driver `Controller` proxy.
    ///
    /// When a new proxy is bound, any active governor votes are immediately replayed to the
    /// driver.
    pub(crate) async fn set_driver_controller(&self, proxy: Option<fcharger::ControllerProxy>) {
        let mut state = self.state.lock().await;
        state.driver = proxy;
        state.applied = fcharger::ControlOptions::default();
        if state.driver.is_some()
            && let Err(e) = state.sync_driver().await
        {
            warn!("ChargeLimitArbiter: failed to replay cached votes to charger driver: {e:?}");
        }
    }

    /// Records the arbiter's per-field elections under `node`.
    pub(crate) async fn record_inspect(&self, node: &fuchsia_inspect::Node) {
        self.state.lock().await.votes.record_inspect(node);
    }

    /// Returns every governor's votes and overrides, the debug overrides of every governor's votes,
    /// and the effective values, read together.
    pub(crate) async fn arbitration(&self) -> Arbitration {
        let state = self.state.lock().await;
        let governors = state
            .voters
            .iter()
            .filter(|(voter, _)| *voter != DEBUG_OVERRIDE_VOTER)
            .map(|(governor, votes)| (governor.clone(), votes.clone()))
            .collect();
        let debug_overrides = state
            .voters
            .get(DEBUG_OVERRIDE_VOTER)
            .map(|voter| voter.votes.clone())
            .unwrap_or_default();
        Arbitration { governors, debug_overrides, effective: state.votes.effective() }
    }

    /// Returns `governor`'s votes and overrides.
    pub(crate) async fn governor_votes(&self, governor: &str) -> GovernorVotes {
        self.state.lock().await.voters.get(governor).cloned().unwrap_or_default()
    }
}

/// Governor votes, cast through each governor's `Controller` connection.
impl ChargeLimitArbiter {
    /// Serves `governor`'s `Controller` request stream, maintaining its votes until disconnect.
    ///
    /// A governor may hold only one connection at a time: while `governor` is connected, a second
    /// connection is closed with a `ZX_ERR_ALREADY_BOUND` epitaph before any request is served.
    ///
    /// If this future is dropped before the stream ends, the caller must [`Self::withdraw`] the
    /// governor, as the governor supervisor does when it tears a governor down.
    pub(crate) async fn serve_controller(
        self: Arc<Self>,
        governor: String,
        mut stream: fcharger::ControllerRequestStream,
    ) -> Result<(), fidl::Error> {
        if governor == DEBUG_OVERRIDE_VOTER {
            warn!("ChargeLimitArbiter: rejecting Controller connection from reserved {governor}");
            stream.control_handle().shutdown_with_epitaph(zx::Status::ACCESS_DENIED);
            return Ok(());
        }
        {
            let mut state = self.state.lock().await;
            if !state.connected.insert(governor.clone()) {
                warn!(
                    "ChargeLimitArbiter: rejecting duplicate Controller connection from {governor}"
                );
                stream.control_handle().shutdown_with_epitaph(zx::Status::ALREADY_BOUND);
                return Ok(());
            }
            // Overrides vote for the governor as soon as it connects.
            if let Some(entry) = state.voters.get(&governor).cloned() {
                state.store(&governor, entry);
                if let Err(e) = state.sync_driver().await {
                    warn!(
                        "ChargeLimitArbiter: failed to sync driver after {governor} connected: \
                         {e:?}"
                    );
                }
            }
        }
        debug!("ChargeLimitArbiter: governor {governor} connected");

        let mut stream_err = None;
        while let Some(item) = stream.next().await {
            let request = match item {
                Ok(req) => req,
                Err(e) => {
                    stream_err = Some(e);
                    break;
                }
            };

            match request {
                fcharger::ControllerRequest::SetControl { options, responder } => {
                    let result = self.handle_set_control(&governor, &options).await;
                    let _ = responder.send(result);
                }
                fcharger::ControllerRequest::ClearControl { options, responder } => {
                    let result = self.handle_clear_control(&governor, &options).await;
                    let _ = responder.send(result);
                }
                fcharger::ControllerRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("ChargeLimitArbiter: received unknown method ordinal: {ordinal}");
                }
            }
        }

        self.withdraw(&governor).await;
        stream_err.map_or(Ok(()), Err)
    }

    /// Forgets `governor`'s connection, if any, withdraws its votes, and syncs the driver.
    pub(crate) async fn withdraw(&self, governor: &str) {
        self.state.lock().await.withdraw(governor).await;
    }

    async fn handle_set_control(
        &self,
        governor: &str,
        options: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        validate_set_options(options)?;
        if *options == fcharger::ControlOptions::default() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state.update_votes(governor, |entry| merge_vote(&mut entry.votes, options)).await
    }

    async fn handle_clear_control(
        &self,
        governor: &str,
        mask: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        validate_clear_options(mask)?;
        let mut state = self.state.lock().await;
        state.update_votes(governor, |entry| clear_masked(&mut entry.votes, mask)).await
    }
}

/// Debug overrides, set through `GovernorDebug` over every governor's votes and through `Governor`
/// over one governor's votes.
impl ChargeLimitArbiter {
    /// Overrides `governor`'s votes on the fields `options` sets, as `SetControl` would, until
    /// cleared; the overrides apply while it is connected and leave the votes it casts unchanged.
    pub(crate) async fn set_governor_override(
        &self,
        governor: &str,
        options: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        validate_set_options(options)?;
        let mut state = self.state.lock().await;
        state.update_votes(governor, |entry| merge_vote(&mut entry.overrides, options)).await
    }

    /// Clears `governor`'s overrides of the fields in `mask`, or of every field if `mask` is
    /// empty, restoring the votes it casts on them.
    pub(crate) async fn clear_governor_override(
        &self,
        governor: &str,
        mask: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        validate_clear_options(mask)?;
        let mut state = self.state.lock().await;
        state.update_votes(governor, |entry| clear_masked(&mut entry.overrides, mask)).await
    }

    /// Overrides the present fields of `options` over every governor's votes until they are
    /// cleared with [`Self::clear_debug_override`]. Accepts the same options as `SetControl`.
    pub(crate) async fn set_debug_override(
        &self,
        options: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        self.handle_set_control(DEBUG_OVERRIDE_VOTER, options).await
    }

    /// Clears the debug overrides for the fields present in `mask`, or every debug override if
    /// `mask` is empty.
    pub(crate) async fn clear_debug_override(
        &self,
        mask: &fcharger::ControlOptions,
    ) -> Result<(), fcharger::Error> {
        self.handle_clear_control(DEBUG_OVERRIDE_VOTER, mask).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async as fasync;
    use std::sync::Mutex as StdMutex;

    const MSC: &str = "msc";
    const THERMAL: &str = "thermal";
    /// Governor name for tests that don't model a specific governor.
    const GOVERNOR_A: &str = "governor_a";

    const TEST_ICL_HIGH_UA: u32 = 1_500_000;
    const TEST_ICL_LOW_UA: u32 = 500_000;
    const TEST_FCC_HIGH_UA: u32 = 2_000_000;
    const TEST_FCC_MID_UA: u32 = 1_500_000;
    const TEST_FCC_LOW_UA: u32 = 800_000;
    const TEST_FV_HIGH_UV: u32 = 4_400_000;
    const TEST_FV_MID_UV: u32 = 4_350_000;
    const TEST_FV_LOW_UV: u32 = 4_100_000;
    const TEST_ITERM_HIGH_UA: u32 = 200_000;
    const TEST_ITERM_LOW_UA: u32 = 100_000;
    const TEST_VIN_HIGH_UV: u32 = 4_600_000;
    const TEST_TOP_OFF_LONG_NANOS: i64 = 600_000_000_000;
    const TEST_TOP_OFF_SHORT_NANOS: i64 = 30_000_000_000;
    const TEST_VIN_LOW_UV: u32 = 4_400_000;
    const UNKNOWN_OPERATING_MODE_RAW: u32 = 99;

    #[derive(Clone, Debug, PartialEq)]
    enum DriverCall {
        Set(fcharger::ControlOptions),
        Clear(fcharger::ControlOptions),
    }

    fn spawn_fake_driver(
        calls: Arc<StdMutex<Vec<DriverCall>>>,
        next_error: Arc<StdMutex<Option<fcharger::Error>>>,
    ) -> (fcharger::ControllerProxy, fasync::Task<()>) {
        spawn_scripted_driver(calls, move |_| next_error.lock().unwrap().take().map_or(Ok(()), Err))
    }

    /// Spawns a fake driver that records every call and answers it with `respond(call)`.
    fn spawn_scripted_driver(
        calls: Arc<StdMutex<Vec<DriverCall>>>,
        mut respond: impl FnMut(&DriverCall) -> Result<(), fcharger::Error> + 'static,
    ) -> (fcharger::ControllerProxy, fasync::Task<()>) {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let task = fasync::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fcharger::ControllerRequest::SetControl { options, responder } => {
                        let call = DriverCall::Set(options);
                        let result = respond(&call);
                        calls.lock().unwrap().push(call);
                        let _ = responder.send(result);
                    }
                    fcharger::ControllerRequest::ClearControl { options, responder } => {
                        let call = DriverCall::Clear(options);
                        let result = respond(&call);
                        calls.lock().unwrap().push(call);
                        let _ = responder.send(result);
                    }
                    fcharger::ControllerRequest::_UnknownMethod { .. } => {}
                }
            }
        });
        (proxy, task)
    }

    fn connect_client(
        arbiter: &Arc<ChargeLimitArbiter>,
        governor: &str,
    ) -> (fcharger::ControllerProxy, fasync::Task<()>) {
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let arbiter = arbiter.clone();
        let governor = governor.to_owned();
        let task = fasync::Task::local(async move {
            let _ = arbiter.serve_controller(governor, stream).await;
        });
        (proxy, task)
    }

    async fn new_arbiter_with_driver() -> (
        Arc<ChargeLimitArbiter>,
        Arc<StdMutex<Vec<DriverCall>>>,
        Arc<StdMutex<Option<fcharger::Error>>>,
        fasync::Task<()>,
    ) {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let next_err = Arc::new(StdMutex::new(None));
        let (driver_proxy, driver_task) = spawn_fake_driver(calls.clone(), next_err.clone());
        arbiter.set_driver_controller(Some(driver_proxy)).await;
        (arbiter, calls, next_err, driver_task)
    }

    fn drain(calls: &StdMutex<Vec<DriverCall>>) -> Vec<DriverCall> {
        calls.lock().unwrap().drain(..).collect()
    }

    /// Returns the [`GovernorVotes`] of a governor that casts `votes` and has no overrides.
    fn cast(votes: fcharger::ControlOptions) -> GovernorVotes {
        GovernorVotes { votes, overrides: fcharger::ControlOptions::default() }
    }

    /// Verifies that a governor's overrides win over the votes it casts on the fields they set
    /// while the governor still competes with the others, that the governor keeps voting
    /// underneath, and that clearing the overrides restores its current votes.
    #[fuchsia::test]
    async fn test_governor_override_wins_until_cleared() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (governor, _governor_task) = connect_client(&arbiter, GOVERNOR_A);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        let fcc = |ua| fcharger::ControlOptions {
            charge_current_limit_ua: Some(ua),
            ..Default::default()
        };
        assert_eq!(governor.set_control(&fcc(TEST_FCC_HIGH_UA)).await.unwrap(), Ok(()));
        assert_eq!(thermal.set_control(&fcc(TEST_FCC_MID_UA)).await.unwrap(), Ok(()));
        let _ = drain(&calls);

        // Overriding the governor's vote with a lower one makes it win against thermal.
        assert_eq!(arbiter.set_governor_override(GOVERNOR_A, &fcc(TEST_FCC_LOW_UA)).await, Ok(()));
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc(TEST_FCC_LOW_UA))]);
        assert_eq!(
            arbiter.arbitration().await.governors.get(GOVERNOR_A),
            Some(&GovernorVotes { votes: fcc(TEST_FCC_HIGH_UA), overrides: fcc(TEST_FCC_LOW_UA) })
        );

        // The governor keeps voting; its votes on the overridden field are recorded but not
        // applied.
        let fv = fcharger::ControlOptions {
            float_voltage_uv: Some(TEST_FV_MID_UV),
            ..fcc(TEST_FCC_MID_UA)
        };
        assert_eq!(governor.set_control(&fv).await.unwrap(), Ok(()));
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );

        // Clearing the override restores the governor's current vote, which ties thermal's.
        assert_eq!(
            arbiter.clear_governor_override(GOVERNOR_A, &fcharger::ControlOptions::default()).await,
            Ok(())
        );
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc(TEST_FCC_MID_UA))]);
        assert_eq!(arbiter.arbitration().await.governors.get(GOVERNOR_A), Some(&cast(fv)));
    }

    /// Verifies that a governor's overrides apply only while the governor is connected, and are
    /// kept across reconnections.
    #[fuchsia::test]
    async fn test_governor_override_applies_while_connected() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let fcc = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            ..Default::default()
        };
        assert_eq!(arbiter.set_governor_override(GOVERNOR_A, &fcc).await, Ok(()));
        assert_eq!(drain(&calls), vec![]);

        for _ in 0..2 {
            let (client, client_task) = connect_client(&arbiter, GOVERNOR_A);
            // A request round trip ensures that the connection is being served.
            assert_eq!(
                client.set_control(&fcharger::ControlOptions::default()).await.unwrap(),
                Ok(())
            );
            assert_eq!(drain(&calls), vec![DriverCall::Set(fcc.clone())]);

            drop(client);
            client_task.await;
            assert_eq!(drain(&calls), vec![DriverCall::Clear(fcc.clone())]);
            assert_eq!(
                arbiter
                    .arbitration()
                    .await
                    .governors
                    .get(GOVERNOR_A)
                    .map(|votes| votes.overrides.clone()),
                Some(fcc.clone())
            );
        }
    }

    /// Verifies that governor overrides are validated like governor votes.
    #[fuchsia::test(logging = false)]
    async fn test_governor_override_rejects_invalid_requests() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let otg = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Otg),
            ..Default::default()
        };
        assert_eq!(
            arbiter.set_governor_override(GOVERNOR_A, &otg).await,
            Err(fcharger::Error::NotSupported)
        );
        assert_eq!(drain(&calls), vec![]);
        assert_eq!(arbiter.arbitration().await.governors, BTreeMap::new());
    }

    /// Verifies that debug overrides win over every governor's votes on the fields they set, last
    /// until cleared, and that clearing them restores the governors' arbitration.
    #[fuchsia::test]
    async fn test_debug_override_wins_until_cleared() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, GOVERNOR_A);
        let fcc = |ua| fcharger::ControlOptions {
            charge_current_limit_ua: Some(ua),
            ..Default::default()
        };
        let governor_votes = fcharger::ControlOptions {
            float_voltage_uv: Some(TEST_FV_MID_UV),
            ..fcc(TEST_FCC_LOW_UA)
        };
        assert_eq!(client.set_control(&governor_votes).await.unwrap(), Ok(()));
        let _ = drain(&calls);

        // An override above the governor's minimum still wins, and leaves other fields alone.
        assert_eq!(arbiter.set_debug_override(&fcc(TEST_FCC_HIGH_UA)).await, Ok(()));
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc(TEST_FCC_HIGH_UA))]);
        assert_eq!(
            arbiter.arbitration().await,
            Arbitration {
                governors: BTreeMap::from([(GOVERNOR_A.to_owned(), cast(governor_votes.clone()))]),
                debug_overrides: fcc(TEST_FCC_HIGH_UA),
                effective: fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_MID_UV),
                    ..fcc(TEST_FCC_HIGH_UA)
                },
            }
        );

        // Governor votes on an overridden field do not change the effective value.
        assert_eq!(client.set_control(&fcc(TEST_FCC_MID_UA)).await.unwrap(), Ok(()));
        assert_eq!(drain(&calls), vec![]);

        assert_eq!(
            arbiter.clear_debug_override(&fcharger::ControlOptions::default()).await,
            Ok(())
        );
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc(TEST_FCC_MID_UA))]);
        assert_eq!(
            arbiter.arbitration().await.debug_overrides,
            fcharger::ControlOptions::default()
        );
    }

    /// Verifies that debug overrides are validated like governor votes.
    #[fuchsia::test(logging = false)]
    async fn test_debug_override_rejects_otg() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let otg = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Otg),
            ..Default::default()
        };
        assert_eq!(arbiter.set_debug_override(&otg).await, Err(fcharger::Error::NotSupported));
        assert_eq!(drain(&calls), vec![]);
    }

    /// Verifies that no governor can vote under the debug override voter's name.
    #[fuchsia::test(logging = false)]
    async fn test_rejects_controller_named_debug_override_voter() {
        let (arbiter, _calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, client_task) = connect_client(&arbiter, DEBUG_OVERRIDE_VOTER);
        client_task.await;
        let err = client
            .set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_matches!(
            err,
            fidl::Error::ClientChannelClosed { epitaph, .. } if epitaph == zx::Status::ACCESS_DENIED
        );
    }

    /// Verifies that partial `SetControl` updates preserve omitted fields and only send changed
    /// fields to the driver, and that `ClearControl` clears masked fields (or all fields when the
    /// mask is empty).
    #[fuchsia::test]
    async fn test_single_voter_partial_update_and_selective_clear() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);

        // 1. Set FCC only.
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                ..Default::default()
            })]
        );

        // 2. Partial SetControl with FV only preserves FCC and sends only the FV delta to driver.
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_MID_UV),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );

        // 3. Selective ClearControl on FCC clears only FCC on the driver while keeping FV active.
        assert_eq!(
            client
                .clear_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(0),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Clear(fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                ..Default::default()
            })]
        );

        // 4. Empty ClearControl clears the remaining FV vote.
        assert_eq!(
            client.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Clear(fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );
    }

    /// Verifies that across multiple voters each field selects the vote that charges least (the
    /// minimum, or the maximum for `input_voltage_limit_uv` and `charge_term_current_ua`),
    /// `operating_mode` the highest-precedence vote, and that only changed effective fields are
    /// forwarded to the driver.
    #[fuchsia::test]
    async fn test_multi_voter_arbitration_all_fields() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);

        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                input_current_limit_ua: Some(TEST_ICL_HIGH_UA),
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                charge_term_current_ua: Some(TEST_ITERM_LOW_UA),
                input_voltage_limit_uv: Some(TEST_VIN_LOW_UV),
                top_off_duration: Some(TEST_TOP_OFF_LONG_NANOS),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        drain(&calls);

        // thermal votes tighter on mode, ICL, FCC, ITERM, VIN (higher), and top-off, and looser on
        // FV.
        assert_eq!(
            thermal
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    input_current_limit_ua: Some(TEST_ICL_LOW_UA),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    float_voltage_uv: Some(TEST_FV_HIGH_UV),
                    charge_term_current_ua: Some(TEST_ITERM_HIGH_UA),
                    input_voltage_limit_uv: Some(TEST_VIN_HIGH_UV),
                    top_off_duration: Some(TEST_TOP_OFF_SHORT_NANOS),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );

        // Only the fields whose effective value changed should be sent to the driver.
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Passthrough),
                input_current_limit_ua: Some(TEST_ICL_LOW_UA),
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                charge_term_current_ua: Some(TEST_ITERM_HIGH_UA),
                input_voltage_limit_uv: Some(TEST_VIN_HIGH_UV),
                top_off_duration: Some(TEST_TOP_OFF_SHORT_NANOS),
                ..Default::default()
            })]
        );
    }

    /// Verifies that `SetControl` rejects `OperatingMode::Otg` and unknown operating modes with
    /// `NotSupported` without calling the driver.
    #[fuchsia::test]
    async fn test_rejects_otg_and_unknown_operating_mode() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);

        for bad_mode in [
            fcharger::OperatingMode::Otg,
            fcharger::OperatingMode::from_primitive_allow_unknown(UNKNOWN_OPERATING_MODE_RAW),
        ] {
            let res = client
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(bad_mode),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(res, Err(fcharger::Error::NotSupported));
        }

        assert!(calls.lock().unwrap().is_empty());
    }

    /// Verifies that closing a governor's connection withdraws all of its votes on the driver and
    /// allows the governor to reconnect afterward.
    #[fuchsia::test]
    async fn test_voter_disconnect_clears_votes_on_driver() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, msc_task) = connect_client(&arbiter, MSC);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                float_voltage_uv: Some(TEST_FV_LOW_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        drain(&calls);

        drop(msc);
        msc_task.await;

        assert_eq!(
            drain(&calls),
            vec![DriverCall::Clear(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                float_voltage_uv: Some(TEST_FV_LOW_UV),
                ..Default::default()
            })]
        );

        // The governor can reconnect once its previous connection has closed.
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
    }

    /// Verifies that a second concurrent connection under the same governor name is closed with
    /// `ZX_ERR_ALREADY_BOUND` without disturbing the active connection's votes.
    #[fuchsia::test]
    async fn test_rejects_duplicate_connection_from_same_governor() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (first, _first_task) = connect_client(&arbiter, MSC);
        assert_eq!(
            first
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        drain(&calls);

        // A second connection under the same governor name is closed with ALREADY_BOUND and does
        // not disturb the first connection's votes.
        let (second, second_task) = connect_client(&arbiter, MSC);
        second_task.await;
        let err = second
            .set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_matches!(
            err,
            fidl::Error::ClientChannelClosed { epitaph, .. } if epitaph == zx::Status::ALREADY_BOUND
        );
        assert!(calls.lock().unwrap().is_empty());

        assert_eq!(
            arbiter.state.lock().await.votes.effective().charge_current_limit_ua,
            Some(TEST_FCC_MID_UA)
        );
    }

    /// Verifies that votes cast before a charger driver is bound are cached and replayed when the
    /// driver connects.
    #[fuchsia::test]
    async fn test_late_binding_replays_cached_votes_to_driver() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let (client, _client_task) = connect_client(&arbiter, MSC);

        // Vote before any hardware driver is connected.
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    float_voltage_uv: Some(TEST_FV_MID_UV),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );

        let calls = Arc::new(StdMutex::new(Vec::new()));
        let next_err = Arc::new(StdMutex::new(None));
        let (driver_proxy, _driver_task) = spawn_fake_driver(calls.clone(), next_err);
        arbiter.set_driver_controller(Some(driver_proxy)).await;

        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );
    }

    /// Verifies that when the driver rejects `SetControl`, the caller's previous votes are
    /// restored.
    #[fuchsia::test]
    async fn test_driver_rejection_rolls_back_voter_state() {
        let (arbiter, _calls, next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);

        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );

        // Next driver SetControl fails with InvalidArgs -> governor's previous FCC vote is kept.
        *next_err.lock().unwrap() = Some(fcharger::Error::InvalidArgs);
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Err(fcharger::Error::InvalidArgs)
        );

        let state = arbiter.state.lock().await;
        assert_eq!(state.votes.votes_of(MSC).charge_current_limit_ua, Some(TEST_FCC_MID_UA));
        assert_eq!(state.votes.effective().charge_current_limit_ua, Some(TEST_FCC_MID_UA));
        assert_eq!(state.applied.charge_current_limit_ua, Some(TEST_FCC_MID_UA));
    }

    /// Verifies that `record_inspect` exposes each field's `effective`, `overridden`, and
    /// per-governor `votes` under `elections`.
    #[fuchsia::test]
    async fn test_record_inspect_keys_votes_by_governor_name() {
        let (arbiter, _calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);

        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            thermal
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );

        let inspector = fuchsia_inspect::Inspector::default();
        arbiter.record_inspect(inspector.root()).await;
        assert_data_tree!(inspector, root: {
            elections: {
                operating_mode: {
                    result: "Passthrough",
                    overridden: false,
                    votes: { msc: "Charging", thermal: "Passthrough" },
                },
                input_current_limit_ua: { overridden: false, votes: {} },
                charge_current_limit_ua: {
                    result: u64::from(TEST_FCC_LOW_UA),
                    overridden: false,
                    votes: {
                        msc: u64::from(TEST_FCC_HIGH_UA),
                        thermal: u64::from(TEST_FCC_LOW_UA),
                    },
                },
                float_voltage_uv: {
                    result: u64::from(TEST_FV_MID_UV),
                    overridden: false,
                    votes: { msc: u64::from(TEST_FV_MID_UV) },
                },
                charge_term_current_ua: { overridden: false, votes: {} },
                input_voltage_limit_uv: { overridden: false, votes: {} },
                top_off_duration: { overridden: false, votes: {} },
            },
        });
    }

    /// Verifies that `validate_set_options` and `validate_clear_options` accept all supported
    /// fields, and that `validate_clear_options` permits any `operating_mode` value as a mask.
    #[fuchsia::test]
    fn test_validation_accepts_every_supported_field() {
        let all_fields = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Charging),
            input_current_limit_ua: Some(TEST_ICL_HIGH_UA),
            charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
            float_voltage_uv: Some(TEST_FV_HIGH_UV),
            charge_term_current_ua: Some(TEST_ITERM_HIGH_UA),
            input_voltage_limit_uv: Some(TEST_VIN_HIGH_UV),
            top_off_duration: Some(TEST_TOP_OFF_SHORT_NANOS),
            ..Default::default()
        };
        assert_eq!(unsupported_fields(&all_fields), fcharger::ControlOptions::default());
        assert_eq!(validate_set_options(&all_fields), Ok(()));
        assert_eq!(validate_clear_options(&all_fields), Ok(()));

        // `ClearControl` only inspects field presence, so a mode that `SetControl` rejects is
        // still a valid mask entry.
        let otg_mask = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Otg),
            ..Default::default()
        };
        assert_eq!(validate_set_options(&otg_mask), Err(fcharger::Error::NotSupported));
        assert_eq!(validate_clear_options(&otg_mask), Ok(()));

        // A zero top-off duration is valid; a negative one is not, but still masks the field.
        let top_off = |nanos| fcharger::ControlOptions {
            top_off_duration: Some(nanos),
            ..Default::default()
        };
        assert_eq!(validate_set_options(&top_off(0)), Ok(()));
        assert_eq!(validate_set_options(&top_off(-1)), Err(fcharger::Error::InvalidArgs));
        assert_eq!(validate_clear_options(&top_off(-1)), Ok(()));
    }

    /// Verifies that a `SetControl` request rejected during validation leaves existing votes and
    /// driver state untouched.
    #[fuchsia::test]
    async fn test_rejected_set_control_leaves_votes_unchanged() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        let initial = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            ..Default::default()
        };
        assert_eq!(client.set_control(&initial).await.unwrap(), Ok(()));
        drain(&calls);

        // The supported FCC field must not be applied when the request is rejected as a whole.
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Otg),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Err(fcharger::Error::NotSupported)
        );

        assert!(calls.lock().unwrap().is_empty());
        let state = arbiter.state.lock().await;
        assert_eq!(state.votes.votes_of(MSC), initial);
        assert_eq!(state.votes.effective(), initial);
        assert_eq!(state.applied, initial);
    }

    /// Verifies that `ClearControl` only inspects field presence (`Some` vs `None`) and ignores the
    /// inner values of mask fields.
    #[fuchsia::test]
    async fn test_clear_control_mask_ignores_field_values() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Discharging),
                    charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        drain(&calls);

        // `Otg` is not settable, but as a mask entry only its presence matters.
        assert_eq!(
            client
                .clear_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Otg),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Clear(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                ..Default::default()
            })]
        );
        assert_eq!(
            arbiter.state.lock().await.votes.votes_of(MSC),
            fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                ..Default::default()
            }
        );
    }

    /// Verifies that when the driver rejects `ClearControl`, the caller's votes are restored so the
    /// clear can be retried.
    #[fuchsia::test]
    async fn test_driver_rejects_clear_control_rolls_back_voter_state() {
        let (arbiter, calls, next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        let votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            float_voltage_uv: Some(TEST_FV_MID_UV),
            ..Default::default()
        };
        assert_eq!(client.set_control(&votes).await.unwrap(), Ok(()));
        drain(&calls);

        let fv_mask = fcharger::ControlOptions { float_voltage_uv: Some(0), ..Default::default() };
        // Only presence counts in a clear, so the arbiter forwards the applied FV.
        let fv_clear = fcharger::ControlOptions {
            float_voltage_uv: Some(TEST_FV_MID_UV),
            ..Default::default()
        };
        *next_err.lock().unwrap() = Some(fcharger::Error::Io);
        assert_eq!(client.clear_control(&fv_mask).await.unwrap(), Err(fcharger::Error::Io));
        assert_eq!(drain(&calls), vec![DriverCall::Clear(fv_clear.clone())]);

        {
            let state = arbiter.state.lock().await;
            assert_eq!(state.votes.votes_of(MSC), votes);
            assert_eq!(state.votes.effective(), votes);
            assert_eq!(state.applied, votes);
        }

        // The governor's FV constraint was retained, so the clear can be retried.
        assert_eq!(client.clear_control(&fv_mask).await.unwrap(), Ok(()));
        assert_eq!(drain(&calls), vec![DriverCall::Clear(fv_clear)]);
    }

    /// Verifies that if `SetControl` succeeds on the driver before `ClearControl` fails during the
    /// same update, rolling back the voter's state also re-syncs the driver back to the restored
    /// votes.
    #[fuchsia::test]
    async fn test_driver_rejects_clear_after_partial_apply_restores_driver() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let clear_err = Arc::new(StdMutex::new(None));
        let (driver_proxy, _driver_task) = spawn_scripted_driver(calls.clone(), {
            let clear_err = clear_err.clone();
            move |call| match call {
                DriverCall::Set(_) => Ok(()),
                DriverCall::Clear(_) => clear_err.lock().unwrap().take().map_or(Ok(()), Err),
            }
        });
        arbiter.set_driver_controller(Some(driver_proxy)).await;

        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        let thermal_votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            float_voltage_uv: Some(TEST_FV_LOW_UV),
            ..Default::default()
        };
        assert_eq!(thermal.set_control(&thermal_votes).await.unwrap(), Ok(()));
        drain(&calls);

        // Withdrawing all thermal votes relaxes FCC to msc's vote (SetControl, succeeds) and
        // clears FV (ClearControl, fails). The rollback must restore thermal's FCC on the driver.
        *clear_err.lock().unwrap() = Some(fcharger::Error::Io);
        assert_eq!(
            thermal.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Err(fcharger::Error::Io)
        );
        assert_eq!(
            drain(&calls),
            vec![
                DriverCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    ..Default::default()
                }),
                DriverCall::Clear(fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_LOW_UV),
                    ..Default::default()
                }),
                DriverCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                }),
            ]
        );

        let state = arbiter.state.lock().await;
        assert_eq!(state.votes.votes_of(THERMAL), thermal_votes);
        assert_eq!(state.votes.effective(), thermal_votes);
        assert_eq!(state.applied, thermal_votes);
    }

    /// Verifies that when the driver's channel closes, the request that finds it closed drops the
    /// driver proxy but succeeds, its vote cached like any vote cast while no driver is bound.
    #[fuchsia::test]
    async fn test_driver_channel_closed_drops_driver_and_caches_votes() {
        let (arbiter, calls, _next_err, driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        let fcc_vote = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            ..Default::default()
        };
        let fv_vote = fcharger::ControlOptions {
            float_voltage_uv: Some(TEST_FV_MID_UV),
            ..Default::default()
        };
        assert_eq!(client.set_control(&fcc_vote).await.unwrap(), Ok(()));
        drain(&calls);

        // Close the driver's server end.
        let _ = driver_task.abort().await;

        assert_eq!(client.set_control(&fv_vote).await.unwrap(), Ok(()));
        let state = arbiter.state.lock().await;
        assert!(state.driver.is_none());
        assert_eq!(state.applied, fcharger::ControlOptions::default());
        assert_eq!(
            state.votes.votes_of(MSC),
            fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_MID_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            }
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    /// Verifies that unbinding the driver caches vote updates while unbound and replays only the
    /// current effective controls when a new driver binds.
    #[fuchsia::test]
    async fn test_unbinding_driver_caches_votes_and_rebinding_replays_effective_controls() {
        let (arbiter, old_calls, _next_err, _old_driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        drain(&old_calls);

        arbiter.set_driver_controller(None).await;

        // Votes changed while unbound are cached and never reach the old driver.
        assert_eq!(
            thermal
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            msc.clear_control(&fcharger::ControlOptions {
                float_voltage_uv: Some(0),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert!(old_calls.lock().unwrap().is_empty());

        let new_calls = Arc::new(StdMutex::new(Vec::new()));
        let (new_driver, _new_driver_task) =
            spawn_fake_driver(new_calls.clone(), Arc::new(StdMutex::new(None)));
        arbiter.set_driver_controller(Some(new_driver)).await;

        // Only the current effective controls are replayed; the withdrawn FV vote is not.
        let expected = fcharger::ControlOptions {
            operating_mode: Some(fcharger::OperatingMode::Passthrough),
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            ..Default::default()
        };
        assert_eq!(drain(&new_calls), vec![DriverCall::Set(expected.clone())]);
        assert!(old_calls.lock().unwrap().is_empty());
        assert_eq!(arbiter.state.lock().await.applied, expected);
    }

    /// Verifies that an empty `SetControl` call succeeds without mutating votes or calling the
    /// driver.
    #[fuchsia::test]
    async fn test_empty_set_control_is_noop() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        let votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            ..Default::default()
        };
        assert_eq!(client.set_control(&votes).await.unwrap(), Ok(()));
        drain(&calls);

        assert_eq!(client.set_control(&fcharger::ControlOptions::default()).await.unwrap(), Ok(()));

        assert!(calls.lock().unwrap().is_empty());
        let state = arbiter.state.lock().await;
        assert_eq!(state.votes.votes_of(MSC), votes);
        assert_eq!(state.applied, votes);
    }

    /// Verifies `Discharging > Passthrough > Charging` precedence across voters and fallback when
    /// higher-precedence votes relax or clear.
    #[fuchsia::test]
    async fn test_operating_mode_precedence_and_fallback() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);

        // msc votes Charging, thermal votes Passthrough -> Passthrough wins.
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            thermal
                .set_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Passthrough),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        drain(&calls);

        // msc escalates to Discharging -> Discharging beats Passthrough.
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Discharging),
                ..Default::default()
            })]
        );

        // msc relaxes back to Charging -> falls back to thermal's Passthrough vote.
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Passthrough),
                ..Default::default()
            })]
        );

        // thermal clears its mode vote -> falls back to msc's Charging vote.
        assert_eq!(
            thermal
                .clear_control(&fcharger::ControlOptions {
                    operating_mode: Some(fcharger::OperatingMode::Charging),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                operating_mode: Some(fcharger::OperatingMode::Charging),
                ..Default::default()
            })]
        );
    }

    /// Verifies that a negative `top_off_duration` is rejected with `InvalidArgs` without casting
    /// any of the request's votes or calling the driver.
    #[fuchsia::test]
    async fn test_rejects_negative_top_off_duration() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (client, _client_task) = connect_client(&arbiter, MSC);
        let initial = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            ..Default::default()
        };
        assert_eq!(client.set_control(&initial).await.unwrap(), Ok(()));
        drain(&calls);

        assert_eq!(
            client
                .set_control(&fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    top_off_duration: Some(-1),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Err(fcharger::Error::InvalidArgs)
        );
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(arbiter.state.lock().await.votes.votes_of(MSC), initial);
    }

    /// Verifies that withdrawing the winning vote falls back to the remaining vote with a
    /// `SetControl`, on fields that take the maximum (`charge_term_current_ua`,
    /// `input_voltage_limit_uv`) and on `top_off_duration`, which takes the minimum.
    #[fuchsia::test]
    async fn test_withdrawing_winning_vote_falls_back_to_remaining_vote() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        let msc_votes = fcharger::ControlOptions {
            charge_term_current_ua: Some(TEST_ITERM_LOW_UA),
            input_voltage_limit_uv: Some(TEST_VIN_LOW_UV),
            top_off_duration: Some(TEST_TOP_OFF_LONG_NANOS),
            ..Default::default()
        };
        assert_eq!(msc.set_control(&msc_votes).await.unwrap(), Ok(()));
        assert_eq!(
            thermal
                .set_control(&fcharger::ControlOptions {
                    charge_term_current_ua: Some(TEST_ITERM_HIGH_UA),
                    input_voltage_limit_uv: Some(TEST_VIN_HIGH_UV),
                    top_off_duration: Some(TEST_TOP_OFF_SHORT_NANOS),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Ok(())
        );
        drain(&calls);

        assert_eq!(
            thermal.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Ok(())
        );
        assert_eq!(drain(&calls), vec![DriverCall::Set(msc_votes)]);
    }

    /// Verifies that withdrawing one of two equal winning votes leaves the effective value, and
    /// so the driver, unchanged.
    #[fuchsia::test]
    async fn test_withdrawing_one_of_equal_votes_leaves_driver_alone() {
        let (arbiter, calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        let votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_MID_UA),
            ..Default::default()
        };
        assert_eq!(msc.set_control(&votes).await.unwrap(), Ok(()));
        assert_eq!(thermal.set_control(&votes).await.unwrap(), Ok(()));
        drain(&calls);

        assert_eq!(
            thermal.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Ok(())
        );
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(arbiter.state.lock().await.votes.effective(), votes);
    }

    /// Verifies that if the driver rejects the replay when it binds, the votes stay cached and
    /// the next sync sends everything the driver has not accepted.
    #[fuchsia::test]
    async fn test_rejected_replay_keeps_votes_and_resends_on_next_sync() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let fcc_vote = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            ..Default::default()
        };
        assert_eq!(msc.set_control(&fcc_vote).await.unwrap(), Ok(()));

        let calls = Arc::new(StdMutex::new(Vec::new()));
        let next_err = Arc::new(StdMutex::new(Some(fcharger::Error::InvalidArgs)));
        let (driver_proxy, _driver_task) = spawn_fake_driver(calls.clone(), next_err);
        arbiter.set_driver_controller(Some(driver_proxy)).await;
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc_vote.clone())]);
        {
            let state = arbiter.state.lock().await;
            assert_eq!(state.votes.votes_of(MSC), fcc_vote);
            assert_eq!(state.applied, fcharger::ControlOptions::default());
        }

        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );
    }

    /// Verifies that when the driver rejects the update a disconnect causes, the governor's votes
    /// are still withdrawn, the driver keeps the values it last accepted, and the next sync sends
    /// the change again.
    #[fuchsia::test]
    async fn test_driver_rejection_during_disconnect_still_withdraws_votes() {
        let (arbiter, calls, next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, thermal_task) = connect_client(&arbiter, THERMAL);
        let fcc = |ua| fcharger::ControlOptions {
            charge_current_limit_ua: Some(ua),
            ..Default::default()
        };
        assert_eq!(msc.set_control(&fcc(TEST_FCC_HIGH_UA)).await.unwrap(), Ok(()));
        assert_eq!(thermal.set_control(&fcc(TEST_FCC_LOW_UA)).await.unwrap(), Ok(()));
        drain(&calls);

        *next_err.lock().unwrap() = Some(fcharger::Error::Io);
        drop(thermal);
        thermal_task.await;
        assert_eq!(drain(&calls), vec![DriverCall::Set(fcc(TEST_FCC_HIGH_UA))]);
        {
            let state = arbiter.state.lock().await;
            assert_eq!(state.votes.votes_of(THERMAL), fcharger::ControlOptions::default());
            assert_eq!(state.applied, fcc(TEST_FCC_LOW_UA));
        }

        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![DriverCall::Set(fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_MID_UV),
                ..Default::default()
            })]
        );
    }

    /// Verifies that a transport error on `ClearControl` after `SetControl` succeeded in the same
    /// update drops the driver but still withdraws the caller's votes, so the replay to the next
    /// driver doesn't carry them.
    #[fuchsia::test]
    async fn test_transport_error_on_clear_after_set_keeps_votes_withdrawn() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (driver_proxy, mut driver_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        let _driver_task = fasync::Task::local({
            let calls = calls.clone();
            async move {
                while let Some(Ok(request)) = driver_stream.next().await {
                    match request {
                        fcharger::ControllerRequest::SetControl { options, responder } => {
                            calls.lock().unwrap().push(DriverCall::Set(options));
                            let _ = responder.send(Ok(()));
                        }
                        // Closing the channel instead of answering is a transport error.
                        fcharger::ControllerRequest::ClearControl { options, .. } => {
                            calls.lock().unwrap().push(DriverCall::Clear(options));
                            return;
                        }
                        fcharger::ControllerRequest::_UnknownMethod { .. } => {}
                    }
                }
            }
        });
        arbiter.set_driver_controller(Some(driver_proxy)).await;

        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        let thermal_votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            float_voltage_uv: Some(TEST_FV_LOW_UV),
            ..Default::default()
        };
        assert_eq!(thermal.set_control(&thermal_votes).await.unwrap(), Ok(()));
        drain(&calls);

        assert_eq!(
            thermal.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Ok(())
        );
        assert_eq!(
            drain(&calls),
            vec![
                DriverCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    ..Default::default()
                }),
                DriverCall::Clear(fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_LOW_UV),
                    ..Default::default()
                }),
            ]
        );
        let state = arbiter.state.lock().await;
        assert!(state.driver.is_none());
        assert_eq!(state.applied, fcharger::ControlOptions::default());
        assert_eq!(state.votes.votes_of(THERMAL), fcharger::ControlOptions::default());
    }

    /// Verifies that if the re-sync after a rollback also fails, the caller gets the original
    /// error, its votes are restored, and the driver keeps the values it last accepted.
    #[fuchsia::test]
    async fn test_failed_resync_after_rollback_returns_original_error() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let fail = Arc::new(StdMutex::new(false));
        let (driver_proxy, _driver_task) = spawn_scripted_driver(calls.clone(), {
            let fail = fail.clone();
            move |call| match (*fail.lock().unwrap(), call) {
                (false, _) => Ok(()),
                (true, DriverCall::Set(options))
                    if options.charge_current_limit_ua == Some(TEST_FCC_HIGH_UA) =>
                {
                    Ok(())
                }
                (true, DriverCall::Clear(_)) => Err(fcharger::Error::Io),
                (true, DriverCall::Set(_)) => Err(fcharger::Error::Internal),
            }
        });
        arbiter.set_driver_controller(Some(driver_proxy)).await;

        let (msc, _msc_task) = connect_client(&arbiter, MSC);
        let (thermal, _thermal_task) = connect_client(&arbiter, THERMAL);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        let thermal_votes = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            float_voltage_uv: Some(TEST_FV_LOW_UV),
            ..Default::default()
        };
        assert_eq!(thermal.set_control(&thermal_votes).await.unwrap(), Ok(()));
        drain(&calls);

        // Withdrawing thermal relaxes FCC (accepted), then fails to clear FV; the re-sync that
        // would restore thermal's FCC fails too.
        *fail.lock().unwrap() = true;
        assert_eq!(
            thermal.clear_control(&fcharger::ControlOptions::default()).await.unwrap(),
            Err(fcharger::Error::Io)
        );
        assert_eq!(
            drain(&calls),
            vec![
                DriverCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                    ..Default::default()
                }),
                DriverCall::Clear(fcharger::ControlOptions {
                    float_voltage_uv: Some(TEST_FV_LOW_UV),
                    ..Default::default()
                }),
                DriverCall::Set(fcharger::ControlOptions {
                    charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
                    ..Default::default()
                }),
            ]
        );
        let state = arbiter.state.lock().await;
        assert_eq!(state.votes.votes_of(THERMAL), thermal_votes);
        assert_eq!(
            state.applied,
            fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                float_voltage_uv: Some(TEST_FV_LOW_UV),
                ..Default::default()
            }
        );
    }

    /// Verifies that a governor's votes disappear from Inspect once it disconnects.
    #[fuchsia::test]
    async fn test_record_inspect_drops_votes_after_disconnect() {
        let (arbiter, _calls, _next_err, _driver_task) = new_arbiter_with_driver().await;
        let (msc, msc_task) = connect_client(&arbiter, MSC);
        assert_eq!(
            msc.set_control(&fcharger::ControlOptions {
                charge_current_limit_ua: Some(TEST_FCC_HIGH_UA),
                ..Default::default()
            })
            .await
            .unwrap(),
            Ok(())
        );
        drop(msc);
        msc_task.await;

        let inspector = fuchsia_inspect::Inspector::default();
        arbiter.record_inspect(inspector.root()).await;
        assert_data_tree!(inspector, root: {
            elections: contains {
                charge_current_limit_ua: { overridden: false, votes: {} },
            },
        });
    }

    /// Verifies that if `serve_controller` is cancelled while a `SetControl` call to the driver is
    /// in flight (as when a governor is paused mid-vote), the subsequent `withdraw` still clears
    /// the in-flight field on the driver.
    #[fuchsia::test]
    async fn test_cancelled_set_control_followed_by_withdraw_clears_driver() {
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let (driver_proxy, mut driver_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcharger::ControllerMarker>();
        arbiter.set_driver_controller(Some(driver_proxy)).await;

        let (msc, msc_task) = connect_client(&arbiter, MSC);
        let vote = fcharger::ControlOptions {
            charge_current_limit_ua: Some(TEST_FCC_LOW_UA),
            ..Default::default()
        };
        let _set_fut = msc.set_control(&vote);

        // Wait until `serve_controller` has sent `SetControl` to the driver and is suspended
        // waiting for the reply, then cancel `serve_controller` before answering.
        let set_responder = match driver_stream.next().await.unwrap().unwrap() {
            fcharger::ControllerRequest::SetControl { options, responder } => {
                assert_eq!(options, vote);
                responder
            }
            other => panic!("unexpected driver request: {other:?}"),
        };
        drop(msc_task);
        let _ = set_responder.send(Ok(()));

        let withdraw = arbiter.withdraw(MSC);
        let clear = async {
            match driver_stream.next().await.unwrap().unwrap() {
                fcharger::ControllerRequest::ClearControl { options, responder } => {
                    assert_eq!(options, vote);
                    responder.send(Ok(())).unwrap();
                }
                other => panic!("unexpected driver request: {other:?}"),
            }
        };
        futures::join!(withdraw, clear);
        assert_eq!(arbiter.state.lock().await.applied, fcharger::ControlOptions::default());
    }
}
