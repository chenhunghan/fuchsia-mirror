// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Async transition runner and port state synchronization for [`crate::state::Machine`].

use crate::actor::{CcSampleMode, Command, PortActor, TimerKind, TimerSlot};
use crate::cc::{CcStatus, PowerRole};
use crate::port::DebounceDirection;
use crate::power::HardwareState;
use crate::state::{DebounceTimer, Effect, Event, Machine, Step, VbusAction};
use crate::typec::{Typec, TypecExt};
use anyhow::Result;
use fuchsia_async as fasync;
use futures::channel::oneshot;
use std::collections::VecDeque;

/// Pre-alert hardware fault state retried once an alert settles without a transition.
#[derive(Clone, Copy)]
struct AlertRetries {
    disarm_failed: bool,
    step_handled_discharge: bool,
}

impl AlertRetries {
    fn capture(state: &PortState) -> Self {
        Self {
            disarm_failed: state.runner.hw.vbus_discharge.is_unconfirmed_enabled(),
            step_handled_discharge: false,
        }
    }

    fn note_step(&mut self, step: Step) {
        self.step_handled_discharge = !step.is_settled_non_transition();
    }

    async fn finish<T: Typec>(self, actor: PortActor<'_, T>) {
        if self.disarm_failed && !self.step_handled_discharge {
            actor.retry_failed_disarm().await;
        }
    }
}

impl<T: Typec> PortActor<'_, T> {
    pub(crate) async fn handle_cc_status(self, mode: CcSampleMode) -> Result<()> {
        if self.state().machine.is_disabled() {
            return Ok(());
        }
        let mut retries = AlertRetries::capture(&self.state());
        let initial_sync = mode == CcSampleMode::InitialSync;

        let cc = self.read_cc_status().await?;
        let (vbus_ready, vbus_err) = if initial_sync {
            let role = {
                let state = self.state();
                if state.machine.is_disabled() {
                    return Ok(());
                }
                state.machine.initial_vbus_role(cc)
            };
            match self.is_vbus_ready(role).await {
                Ok(ready) => (ready, None),
                // Fall back to `vbus_ready = false` so a static partner still enters `AttachWait`
                // (`tCCDebounce`) instead of staying stranded in `Unattached`.
                Err(e) => {
                    log::warn!(
                        "Failed to read VBUS status during initial sync; debouncing attach: {e:?}"
                    );
                    (false, Some(e))
                }
            }
        } else {
            (false, None)
        };

        let step = {
            let mut state = self.state();
            if state.machine.is_disabled() {
                return Ok(());
            }
            if initial_sync {
                state.transition_initial(cc, vbus_ready)
            } else {
                state.transition(Event::CcChanged(cc))
            }
        };
        let res = self.execute_step(step, true).await;
        retries.note_step(step);
        retries.finish(self).await;
        match vbus_err {
            Some(e) => Err(e),
            None => res,
        }
    }

    pub(crate) async fn handle_vbus_change(self) -> Result<()> {
        let (mut retries, action) = {
            let state = self.state();
            (AlertRetries::capture(&state), state.machine.vbus_action())
        };
        let res = match action {
            VbusAction::QualifyAttach(role) => self.qualify_vbus_attach(role, &mut retries).await,
            VbusAction::CheckSinkDisconnect => self.check_sink_vbus_loss().await,
            VbusAction::NoTransition => self.poll_unattached_vbus().await,
        };
        retries.finish(self).await;
        res
    }

    /// Retries an unconfirmed-enabled disarm when an alert settles in a phase that must not keep
    /// VBUS discharge armed.
    async fn retry_failed_disarm(self) {
        let should_retry = {
            let state = self.state();
            state.runner.hw.vbus_discharge.is_unconfirmed_enabled()
                && state.machine.should_retry_failed_disarm()
        };
        if should_retry && let Err(e) = self.set_vbus_discharge(false).await {
            log::warn!("Failed to retry disarming VBUS discharge: {e:?}");
        }
    }

    /// Qualifies a pending `AttachWait.{SNK,SRC}` (`WaitingForVbus`) once its VBUS predicate holds,
    /// recording on `retries` whether `execute_step` handled discharge for a transition or attach.
    async fn qualify_vbus_attach(self, role: PowerRole, retries: &mut AlertRetries) -> Result<()> {
        let vbus_res = self.is_vbus_ready(Some(role)).await;
        if self.state().machine.vbus_action() != VbusAction::QualifyAttach(role) {
            return Ok(());
        }
        if !vbus_res? {
            if role == PowerRole::Source && self.state().cancel_vsafe0v_timer() {
                self.rearm_vbus_discharge_after_bounce().await;
            }
            return Ok(());
        }

        let cc = self.read_cc_status().await?;
        let (step, arm_vsafe0v) = {
            let mut state = self.state();
            if state.machine.vbus_action() != VbusAction::QualifyAttach(role) {
                return Ok(());
            }
            let step = state.transition(Event::VbusChanged { cc, vbus_ready: true });
            // If `WaitingForVbus` re-evaluated into `AttachWait.SRC` (e.g. CC pin swap or `Ra/Ra`)
            // while residual VBUS discharge remained armed, `vSafe0V` is already confirmed here.
            let arm_vsafe0v = role == PowerRole::Source
                && step == Step::Advanced(None)
                && state.can_disarm_vsafe0v();
            (step, arm_vsafe0v)
        };
        retries.note_step(step);
        self.execute_step(step, true).await?;
        if arm_vsafe0v {
            self.arm_timer(TimerKind::Vsafe0v);
        }
        Ok(())
    }

    /// Detaches an `Attached.SNK` or `DebugAccessory.SNK` session when VBUS falls below
    /// `vSinkDisconnect` (USB Type-C R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2).
    async fn check_sink_vbus_loss(self) -> Result<()> {
        let present_res = self.is_vbus_present().await;
        if self.state().machine.vbus_action() != VbusAction::CheckSinkDisconnect {
            return Ok(());
        }
        if present_res? {
            return Ok(());
        }

        let (cc, cc_err) = match self.read_cc_status().await {
            Ok(cc) => (cc, None),
            // VBUS loss detaches a Sink whatever CC reads (R2.5 §4.5.2.2.5.2), so an unreadable CC
            // must not keep it attached.
            Err(e) => {
                log::warn!("Failed to read CC status on Sink VBUS loss; detaching: {e:?}");
                (CcStatus::default(), Some(e))
            }
        };
        let step = {
            let mut state = self.state();
            if state.machine.vbus_action() != VbusAction::CheckSinkDisconnect {
                return Ok(());
            }
            state.transition(Event::VbusChanged { cc, vbus_ready: false })
        };
        let res = self.execute_step(step, true).await;
        match cc_err {
            Some(e) => Err(e),
            None => res,
        }
    }

    /// Closes an open or unknown sink path in `Unattached` or held `AttachWait.SNK` once live
    /// partner VBUS drops, and debounces `vSafe0V` while residual VBUS discharge is armed in
    /// `Unattached` or `AttachWait.SRC`.
    async fn poll_unattached_vbus(self) -> Result<()> {
        let mut sink_res = Ok(());
        let mut vbus_present = None;
        let mut vbus_present_failed = false;
        if self.state().has_deferred_open_sink() {
            match self.is_vbus_present().await {
                Ok(false) => {
                    vbus_present = Some(false);
                    if self.state().has_deferred_open_sink()
                        && let Err(e) = self.set_vbus_sink(false).await
                    {
                        log::error!("Failed to close VBUS sink path after VBUS loss: {e:?}");
                    }
                }
                Ok(true) => {
                    vbus_present = Some(true);
                }
                Err(e) => {
                    vbus_present_failed = true;
                    log::warn!("Failed to read VBUS presence for deferred sink close: {e:?}");
                    sink_res = Err(e);
                }
            }
        }

        if self.state().can_disarm_vsafe0v() {
            let safe0v_res = self.is_vbus_safe0v().await;
            if self.state().can_disarm_vsafe0v() {
                if safe0v_res? {
                    self.state().runner.hw.vbus_seen_below_present = true;
                    self.arm_timer(TimerKind::Vsafe0v);
                } else if self.state().cancel_vsafe0v_timer() {
                    self.rearm_vbus_discharge_after_bounce().await;
                } else if !vbus_present_failed {
                    self.check_unattached_discharge_live_vbus(vbus_present).await;
                }
            }
        }
        sink_res
    }

    pub(crate) async fn handle_timer_expired(
        self,
        kind: TimerKind,
        debounce_timer: Option<DebounceTimer>,
    ) {
        match kind {
            TimerKind::Debounce { allow_recovery } => {
                self.handle_debounce_expired(debounce_timer, allow_recovery).await;
            }
            TimerKind::Vsafe0v => self.handle_vsafe0v_expired().await,
            TimerKind::DischargeTimeout { allow_retry } => {
                self.handle_discharge_timeout(allow_retry).await;
            }
        }
    }

    async fn handle_debounce_expired(
        self,
        timer_kind: Option<DebounceTimer>,
        allow_recovery: bool,
    ) {
        let cc_res = if timer_kind == Some(DebounceTimer::RpValueChange) {
            retry_once("read CC status after Sink Rp debounce", || self.read_cc_status()).await
        } else {
            self.read_cc_status().await
        };
        let cc = match cc_res {
            Ok(cc) => cc,
            Err(e) if timer_kind == Some(DebounceTimer::CcAttach) => {
                self.handle_attach_debounce_read_error(allow_recovery, "CC", e).await;
                return;
            }
            Err(e) if timer_kind == Some(DebounceTimer::RpValueChange) => {
                log::warn!("Failed to read CC status after Sink Rp debounce retry: {e:?}");
                let step = self.state().settle_rp_debounce_read_error();
                // Settling a Sink `Rp` window only yields `Absorbed`, `Advanced(None)`, or
                // `NotifySinkRp`.
                let _ = self.execute_step(step, true).await;
                return;
            }
            Err(e) => {
                log::warn!(
                    "Failed to read CC status after disconnect debounce; treating as Open: {e:?}"
                );
                CcStatus::default()
            }
        };

        let vbus_role = {
            let state = self.state();
            if state.machine.is_disabled() {
                return;
            }
            state.machine.attach_debounce_vbus_role(cc)
        };
        let vbus_ready = match self.is_vbus_ready(vbus_role).await {
            Ok(v) => v,
            Err(e) => {
                self.handle_attach_debounce_read_error(allow_recovery, "VBUS", e).await;
                return;
            }
        };
        if vbus_role == Some(PowerRole::Source)
            && !vbus_ready
            && self.state().cancel_vsafe0v_timer()
        {
            self.rearm_vbus_discharge_after_bounce().await;
        }

        let step = {
            let mut state = self.state();
            if state.machine.is_disabled() {
                return;
            }
            state.transition(Event::DebounceExpired { cc, vbus_ready })
        };
        // Background timer callbacks log and roll back errors inside `execute_step`.
        let _ = self.execute_step(step, true).await;
    }

    /// Recovers from a transient I2C/SPMI read error at the end of `tCCDebounce` by returning to
    /// `Unattached` (`on_unattached`) and, when `allow_recovery` is `true`, performing at most one
    /// follow-up `read_cc_status()` check so a still-asserted static partner re-enters
    /// `AttachWait`.
    async fn handle_attach_debounce_read_error(
        self,
        allow_recovery: bool,
        what: &str,
        err: anyhow::Error,
    ) {
        {
            let mut state = self.state();
            if state.machine.is_disabled() {
                return;
            }
            log::error!("Failed to check {what} status after attach debounce: {err:?}");
            state.cancel_debounce();
        }
        self.recover_to_unattached(allow_recovery).await;
    }

    /// Runs `Unattached` handling after an attach-debounce read error reset the port to `Unattached`
    /// and, when `resample` is `true`, performs one `read_cc_status()` check so a still-present
    /// partner re-enters `AttachWait`.
    ///
    /// The partner may still be attached and supplying VBUS, so this neither closes the sink path
    /// (`COMMAND.DisableSinkVbus` `0x44` disables sinking power over VBUS and discharges VBUS to
    /// `vSafe0V`, TCPCI Rev 2.0 Ver 1.3 §4.4.8 Table 4-27) nor starts VBUS discharge.
    async fn recover_to_unattached(self, resample: bool) {
        if self.is_disabled() {
            return;
        }
        self.on_unattached().await;
        if resample && let Some(next_step) = self.resample_cc_in_unattached().await {
            // Resampling from `Unattached` only enters `AttachWait`, so no partner callback can
            // fail.
            let _ = self.execute_step(next_step, false).await;
            return;
        }
        self.arm_timer(TimerKind::Debounce { allow_recovery: false });
    }

    async fn handle_vsafe0v_expired(self) {
        match self.is_vbus_safe0v().await {
            Ok(true) if self.state().can_disarm_vsafe0v() => {
                if let Err(e) = self.complete_vsafe0v_discharge().await {
                    log::error!("Failed to disarm VBUS discharge at vSafe0V: {e:?}");
                }
            }
            Ok(true) => {}
            Ok(false) => {
                log::debug!("Transient vSafe0V detected; VBUS rose above vSafe0V during debounce");
                self.rearm_vbus_discharge_after_bounce().await;
            }
            Err(e) => {
                log::error!("Failed to re-verify vSafe0V after debounce: {e:?}");
            }
        }
    }

    /// Logs a failed timeout disarm and, on the first window (`allow_retry`), re-arms
    /// `discharge_timer` once (without holding a wake lease across the retry window, and without
    /// looping indefinitely on a dead bus).
    fn handle_discharge_timeout_disarm_error(
        self,
        allow_retry: bool,
        what: &str,
        err: anyhow::Error,
    ) {
        if allow_retry {
            log::warn!("Failed to {what}; re-arming discharge timeout once: {err:?}");
            self.without_wake_lease().arm_timer(TimerKind::DischargeTimeout { allow_retry: false });
        } else {
            log::error!("Failed to {what} after retry: {err:?}");
        }
    }

    /// Ends a VBUS discharge that has not reached `vSafe0V` within `discharge_timeout`.
    async fn handle_discharge_timeout(self, allow_retry: bool) {
        if !self.state().machine.allows_vsafe0v_disarm() {
            if self.state().runner.hw.vbus_discharge.is_enabled()
                && let Err(e) = self.disarm_vbus_discharge().await
            {
                self.handle_discharge_timeout_disarm_error(
                    allow_retry,
                    "retry disarming VBUS discharge",
                    e,
                );
            }
            return;
        }
        match self.is_vbus_safe0v().await {
            Ok(true) => {
                // VBUS reached `vSafe0V` without a completed `vSafe0V` debounce, so finish discharge
                // as the `vSafe0V` debounce would while forwarding any wake lease held across
                // discharge.
                if self.state().runner.hw.vbus_discharge.is_enabled()
                    && let Err(e) = self.complete_vsafe0v_discharge().await
                {
                    self.handle_discharge_timeout_disarm_error(
                        allow_retry,
                        "disarm VBUS discharge at vSafe0V",
                        e,
                    );
                }
                return;
            }
            Ok(false) => {
                self.state().cancel_vsafe0v_timer();
            }
            Err(e) => {
                log::warn!(
                    "Failed to read vSafe0V on discharge timeout; stopping discharge: {e:?}"
                );
            }
        }
        if !self.state().runner.hw.vbus_discharge.is_enabled() {
            return;
        }
        log::warn!(
            "VBUS did not reach vSafe0V within the discharge timeout; stopping VBUS discharge"
        );
        match self.disarm_vbus_discharge().await {
            Ok(_) => self.state().machine.telemetry.discharge_timeout_count += 1,
            Err(e) => self.handle_discharge_timeout_disarm_error(
                allow_retry,
                "stop VBUS discharge after the discharge timeout",
                e,
            ),
        }
    }

    /// Re-samples `CC_STATUS` once after `Unattached` handling so a partner still presenting its
    /// termination re-enters `AttachWait`.
    async fn resample_cc_in_unattached(self) -> Option<Step> {
        if self.is_disabled() {
            return None;
        }
        match self.read_cc_status().await {
            Ok(cc) => {
                let step = {
                    let mut state = self.state();
                    if state.machine.is_disabled() {
                        return None;
                    }
                    state.transition(Event::CcChanged(cc))
                };
                // `Unattached` handling already ran; only follow up when a partner entered
                // `AttachWait`.
                (!matches!(
                    step,
                    Step::Absorbed | Step::Advanced(Some(Effect::EnterUnattached { .. }))
                ))
                .then_some(step)
            }
            Err(e) => {
                log::warn!("Failed to re-sample CC status in Unattached: {e:?}");
                None
            }
        }
    }

    /// Samples the driver VBUS predicate required to qualify an attach in `role` (`is_vbus_safe0v`
    /// for [`PowerRole::Source`], `is_vbus_present` for [`PowerRole::Sink`]).
    async fn is_vbus_ready(self, role: Option<PowerRole>) -> Result<bool> {
        match role {
            Some(PowerRole::Source) => self.is_vbus_safe0v().await,
            Some(PowerRole::Sink) => self.is_vbus_present().await,
            None => Ok(false),
        }
    }

    /// Applies the hardware side effects of entering `AttachWait.{SNK,SRC}`.
    async fn enter_attach_wait(self, role: PowerRole) {
        // Normal detach and `Unattached` close the sink path already; `AttachWait.SNK` skips
        // `COMMAND.DisableSinkVbus` (`0x44`), which disables sinking VBUS and discharges VBUS to
        // `vSafe0V` (TCPCI Rev 2.0 Ver 1.3 §4.4.8 Table 4-27), so re-detection never discharges a
        // still-present Source partner's VBUS.
        if role == PowerRole::Source {
            if let Err(e) = self.set_vbus_sink(false).await {
                log::error!("Failed to close VBUS sink path on AttachWait.SRC: {e:?}");
            }
            // Apply a discharge load when entering `AttachWait.SRC` above `vSafe0V` so residual
            // VBUS reaches `vSafe0V` within `tVsafe0V` (USB Type-C R2.5 §4.5.2.2.8.1).
            self.arm_residual_vbus_discharge("on AttachWait.SRC").await;
        }
        // Do not clamp the VBUS a Source partner is about to supply.
        if role == PowerRole::Sink
            && let Err(e) = self.set_vbus_discharge(false).await
        {
            log::warn!("Failed to disarm VBUS discharge on AttachWait.SNK: {e:?}");
        }
    }

    /// Opens the sink path for a Sink and invokes [`Typec::on_partner_attached`], holding the
    /// partner if any step fails.
    async fn attach_partner(self, role: PowerRole) -> Result<()> {
        let res = async {
            // A cold-boot attach skips `AttachWait`, which normally disarms discharge. Disarm
            // strictly before sourcing VBUS (`Attached.SRC`); warn and continue on Sink attach so
            // a sticky disarm fault does not block charging against live partner VBUS.
            match role {
                PowerRole::Source => {
                    self.disarm_vbus_discharge().await?;
                    // Ensure the sink path is closed before committing `Attached.SRC` (TCPCI Rev 2.0
                    // Ver 1.3 §4.4.8 Table 4-27), retrying once on a transient bus error if
                    // `enter_attach_wait` failed to close it.
                    retry_once("close VBUS sink path before Source attach", || {
                        self.set_vbus_sink(false)
                    })
                    .await?;
                }
                PowerRole::Sink => {
                    if let Err(e) = self.disarm_vbus_discharge().await {
                        log::warn!("Failed to disarm VBUS discharge on Sink attach: {e:?}");
                    }
                    // Sink VBUS once attached as a Sink (TCPCI Rev 2.0 Ver 1.3 Figure 4-24).
                    retry_once("open VBUS sink path on Sink attach", || self.set_vbus_sink(true))
                        .await?;
                }
            }
            self.on_partner_attached(role, self.wake_lease).await
        }
        .await;
        if let Err(e) = res {
            log::error!("{role:?} attach failed: {e:?}");
            // Hold the partner in `AttachWait` until its CC terminations change, so the port
            // neither retries an attach that cannot complete nor loses a partner still presenting
            // its termination (R2.5 §4.5.2.2.4.2, §4.5.2.2.8.2).
            self.state().hold_failed_attach(role);
            // A failed Sink attach leaves the sink path as it is: closing it with
            // `COMMAND.DisableSinkVbus` (`0x44`) would also discharge VBUS to `vSafe0V` while the
            // Source still supplies it (TCPCI Rev 2.0 Ver 1.3 §4.4.8 Table 4-27). It closes once
            // VBUS drops or the partner detaches.
            return Err(e);
        }
        let mut state = self.state();
        state.with_machine(Machine::record_attached);
        state.runner.committed_callback = None;
        Ok(())
    }

    /// Applies `step` on the actor: dispatches hardware callbacks and arms any debounce timer
    /// requested by [`crate::state::Machine::debounce_timer`].
    ///
    /// On the background timer path, callback errors are logged and rolled back internally; caller
    /// paths receive the error through [`Result`].
    async fn execute_step(self, step: Step, allow_recovery: bool) -> Result<()> {
        let Step::Advanced(mut next_effect) = step else {
            return Ok(());
        };
        let mut res = Ok(());

        loop {
            if let Some(effect) = next_effect {
                match effect {
                    Effect::EnterAttachWait(role) => self.enter_attach_wait(role).await,
                    Effect::EnterUnattached { detached_from, resample_cc } => {
                        if detached_from.is_some() {
                            if let Err(e) = self.set_vbus_sink(false).await {
                                log::error!("Failed to close VBUS sink path on detach: {e:?}");
                            }
                            self.on_partner_detached(self.wake_lease).await;
                            let mut state = self.state();
                            state.runner.committed_callback = None;
                            if state.machine.is_disabled() {
                                return Ok(());
                            }
                        }
                        // `Unattached` closes any open sink path and arms residual discharge before
                        // `resample_cc_in_unattached` enters `AttachWait` if a partner termination
                        // is still present (`AttachWait.SNK` disarms it).
                        self.prepare_unattached(detached_from).await;
                        if resample_cc
                            && let Some(Step::Advanced(effect)) =
                                self.resample_cc_in_unattached().await
                        {
                            next_effect = effect;
                            continue;
                        }
                    }
                    Effect::EnterAttached(role) => {
                        res = self.attach_partner(role).await;
                    }
                    Effect::NotifySinkRp(rp) => {
                        self.on_sink_rp_changed(rp, self.wake_lease).await;
                    }
                }
            }
            self.arm_timer(TimerKind::Debounce { allow_recovery });
            return res;
        }
    }
}

/// Runs `op`, logging at `warn!` and retrying once if the first attempt fails.
pub(crate) async fn retry_once<T, Fut>(action: &str, mut op: impl FnMut() -> Fut) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>> + Send,
{
    match op().await {
        Ok(val) => Ok(val),
        Err(err) => {
            log::warn!("Failed to {action}; retrying once: {err:?}");
            op().await
        }
    }
}

/// Async runner and debounce state paired with [`crate::state::Machine`] inside [`PortState`].
pub(crate) struct RunnerState {
    /// Sleeping debounce timer on `port.scope`.
    pub(crate) timer: TimerSlot,
    /// Sleeping `vSafe0V` debounce timer on `port.scope`.
    pub(crate) vsafe0v_timer: TimerSlot,
    /// Sleeping discharge timeout timer on `port.scope`, cleared when discharge stops.
    pub(crate) discharge_timer: TimerSlot,
    /// Cached hardware power-path state.
    pub(crate) hw: HardwareState,
    /// Direction of an in-flight committed `on_partner_attached` or `on_partner_detached` callback.
    pub(crate) committed_callback: Option<DebounceDirection>,
    /// Whether the sequential actor task is currently running on `port.scope`.
    pub(crate) actor_running: bool,
    /// Pending actor commands executed in FIFO order on `port.scope`.
    pub(crate) queue: VecDeque<Command>,
    /// Test-only [`TypecExt::wait_for_debounce`] waiters woken once [`PortState::is_busy`] becomes
    /// `false`; kept un-gated so downstream driver unit tests can link the standard crate target.
    idle_waiters: Vec<oneshot::Sender<()>>,
}

impl RunnerState {
    pub(crate) const fn new() -> Self {
        Self {
            timer: TimerSlot::new(),
            vsafe0v_timer: TimerSlot::new(),
            discharge_timer: TimerSlot::new(),
            hw: HardwareState::new(),
            committed_callback: None,
            actor_running: false,
            queue: VecDeque::new(),
            idle_waiters: Vec::new(),
        }
    }

    fn clear_timers(&mut self) {
        self.timer.clear();
        self.vsafe0v_timer.clear();
        self.discharge_timer.clear();
    }
}

/// Synchronized state of a [`crate::TypecPort`]: [`Machine`] paired with its async [`RunnerState`].
pub(crate) struct PortState {
    pub(crate) machine: Machine,
    pub(crate) runner: RunnerState,
}

impl PortState {
    pub(crate) const fn new() -> Self {
        Self { machine: Machine::new(), runner: RunnerState::new() }
    }

    /// Mutates [`Self::machine`] and stamps `telemetry.last_updated_time` from the executor's boot
    /// clock whenever [`crate::TypecState`] changes.
    fn with_machine<R>(&mut self, f: impl FnOnce(&mut Machine) -> R) -> R {
        let prev = self.machine.typec_state();
        let out = f(&mut self.machine);
        if self.machine.typec_state() != prev {
            self.machine.telemetry.last_updated_time = fasync::BootInstant::now().into_zx();
        }
        out
    }

    /// Returns whether a debounce timer or async operation is currently in flight.
    pub(crate) fn is_busy(&self) -> bool {
        self.runner.timer.is_active()
            || self.runner.vsafe0v_timer.is_active()
            || self.runner.actor_running
    }

    /// Names the sleeping timer, for Inspect: an attach, `AttachWait.SNK` exit, or disconnect
    /// debounce, a Sink `Rp` change debounce, a `vSafe0V` debounce, or the discharge timeout.
    pub(crate) fn pending_timer(&self) -> &'static str {
        if self.runner.timer.is_active() {
            self.machine.debounce_timer().map_or("none", DebounceTimer::as_str)
        } else if self.runner.vsafe0v_timer.is_active() {
            "vsafe0v"
        } else if self.runner.discharge_timer.is_active() {
            "discharge_timeout"
        } else {
            "none"
        }
    }

    /// Returns the [`DebounceDirection`] of an attach or disconnect debounce in flight: a sleeping
    /// timer, pending uncommitted work, or a committed callback (`None` during a Sink
    /// `tRpValueChange` window).
    pub(crate) fn active_direction(&self) -> Option<DebounceDirection> {
        if !self.is_busy() {
            return None;
        }
        self.runner.committed_callback.or_else(|| self.machine.uncommitted_debounce_direction())
    }

    /// Cancels any uncommitted attach, disconnect, or Sink `Rp` change debounce.
    fn cancel_debounce(&mut self) {
        self.with_machine(Machine::cancel_debounce);
        self.cancel_timer();
    }

    /// Enables or disables the port and cancels any uncommitted debounce and (when disabling) any
    /// pending `vSafe0V` timer.
    #[cfg(test)]
    pub(crate) fn set_disabled(&mut self, disabled: bool) {
        if self.with_machine(|m| m.set_disabled(disabled)) {
            self.cancel_timer();
            if disabled {
                self.cancel_vsafe0v_timer();
            }
        }
    }

    /// Resets the port to disconnected (`disabled` flag preserved), clears tracked sink/discharge
    /// state, and cancels any pending timers.
    #[cfg(test)]
    pub(crate) fn reset_to_disconnected(&mut self) {
        self.with_machine(Machine::reset_to_disconnected);
        self.runner.hw.reset();
        self.runner.clear_timers();
        self.wake_if_idle();
    }

    pub(crate) fn can_disarm_vsafe0v(&self) -> bool {
        self.runner.hw.vbus_discharge.is_enabled() && self.machine.allows_vsafe0v_disarm()
    }

    pub(crate) fn can_disarm_unattached_discharge(&self) -> bool {
        self.can_disarm_vsafe0v() && self.machine.allows_on_vsafe0v()
    }

    fn has_deferred_open_sink(&self) -> bool {
        !self.runner.hw.vbus_sink.matches_confirmed(false)
            && (self.machine.typec_state().is_unattached() || self.machine.is_held_sink())
    }

    /// Registers a waiter woken once [`Self::is_busy`] becomes `false`.
    pub(crate) fn register_idle_waiter(&mut self) -> Option<oneshot::Receiver<()>> {
        if self.is_busy() {
            self.runner.idle_waiters.retain(|tx| !tx.is_canceled());
            let (tx, rx) = oneshot::channel();
            self.runner.idle_waiters.push(tx);
            Some(rx)
        } else {
            None
        }
    }

    pub(crate) fn wake_if_idle(&mut self) {
        if !self.is_busy() {
            for waiter in self.runner.idle_waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }

    /// Cancels and drops any sleeping debounce timer.
    pub(crate) fn cancel_timer(&mut self) {
        self.runner.timer.clear();
        self.wake_if_idle();
    }

    /// Cancels and drops any sleeping `vSafe0V` debounce timer, returning whether one was active.
    pub(crate) fn cancel_vsafe0v_timer(&mut self) -> bool {
        let was_active = self.runner.vsafe0v_timer.is_active();
        self.runner.vsafe0v_timer.clear();
        self.wake_if_idle();
        was_active
    }

    /// Cancels any sleeping `vSafe0V` debounce timer, returning any wake lease it held.
    pub(crate) fn take_vsafe0v_lease(&mut self) -> Option<zx::EventPair> {
        let lease = self.runner.vsafe0v_timer.take_wake_lease_and_clear();
        self.wake_if_idle();
        lease
    }

    /// Pops the next pending [`Command`], or retires `actor_running` and wakes idle waiters when
    /// the queue has drained.
    pub(crate) fn pop_or_retire_actor(&mut self) -> Option<Command> {
        let next = self.runner.queue.pop_front();
        if next.is_none() {
            self.runner.actor_running = false;
            self.runner.committed_callback = None;
            self.wake_if_idle();
        }
        next
    }

    /// Resets runner state (rolling back any uncommitted attach callback) and wakes idle waiters
    /// when `port.scope` drops the actor task.
    pub(crate) fn reset_on_scope_drop(&mut self) {
        if self.runner.committed_callback == Some(DebounceDirection::Attach) {
            self.machine.reset_to_disconnected();
        }
        self.runner.actor_running = false;
        self.runner.queue.clear();
        self.runner.clear_timers();
        self.runner.committed_callback = None;
        self.wake_if_idle();
    }

    /// Applies the timer cancellation and committed-callback state changes for `step` under the
    /// same lock acquisition as the state machine transition.
    fn apply_step(&mut self, step: Step) -> Step {
        if step == Step::Absorbed {
            return Step::Absorbed;
        }
        self.cancel_timer();
        if !self.machine.allows_vsafe0v_disarm() {
            self.cancel_vsafe0v_timer();
        }
        match step {
            Step::Advanced(Some(Effect::EnterAttached(_))) => {
                self.runner.committed_callback = Some(DebounceDirection::Attach);
            }
            Step::Advanced(Some(Effect::EnterUnattached { detached_from: Some(_), .. })) => {
                self.runner.committed_callback = Some(DebounceDirection::Disconnect);
            }
            Step::Absorbed
            | Step::Advanced(
                None
                | Some(
                    Effect::EnterAttachWait(_)
                    | Effect::NotifySinkRp(_)
                    | Effect::EnterUnattached { detached_from: None, .. },
                ),
            ) => {}
        }
        step
    }

    /// Transitions [`Self::machine`] on `event` and atomically prepares [`Self::runner`].
    fn transition(&mut self, event: Event) -> Step {
        let outcome = self.with_machine(|m| m.transition(event));
        self.apply_step(outcome)
    }

    /// Runs [`crate::state::Machine::transition_initial`] and atomically prepares [`Self::runner`].
    fn transition_initial(&mut self, cc: CcStatus, vbus_ready: bool) -> Step {
        let outcome = self.with_machine(|m| m.transition_initial(cc, vbus_ready));
        self.apply_step(outcome)
    }

    /// Settles a persistent `tRpValueChange` expiry read error in [`Self::machine`] and atomically
    /// updates [`Self::runner`].
    fn settle_rp_debounce_read_error(&mut self) -> Step {
        let outcome = self.with_machine(Machine::settle_rp_debounce_read_error);
        self.apply_step(outcome)
    }

    /// Holds a partner whose attach callbacks failed in `AttachWait` until its CC terminations
    /// change.
    fn hold_failed_attach(&mut self, role: PowerRole) {
        self.runner.committed_callback = None;
        if self.with_machine(|m| m.hold_failed_attach(role)) {
            self.cancel_timer();
        }
    }
}
