// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronous USB Type-C connection state machine (USB Type-C R2.5, Section 4.5.2).

mod sink;

use self::sink::{SinkAttachSample, SinkStage};
use crate::cc::{Accessory, CcPinState, CcStatus, PowerRole, RpValue};
use crate::port::{DebounceDirection, TypecState};
use fidl_next_fuchsia_hardware_usb_connector as fconnector;

/// Connection phase of [`Machine`]; projected onto [`TypecState`] by [`Machine::typec_state`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    /// Port is unattached or actively DRP-toggling (`Looking4Connection`).
    #[default]
    Unattached,
    /// `AttachWait.{SNK,SRC}`: qualifying a partner termination before committing `Attached`.
    AttachWait { role: PowerRole, stage: AttachWaitStage },
    /// `Attached.SNK` or `DebugAccessory.SNK`: `cc_status` fixes the attach-time CC pins
    /// (`orientation` and `accessory`), while `rp` tracks the committed Sink `Rp` advertisement.
    AttachedSnk { cc_status: CcStatus, rp: RpValue, stage: SinkStage },
    /// `Attached.SRC`: `cc_status` fixes the attach-time CC pins (`orientation`), while
    /// `disconnecting` tracks an active `tSRCDisconnect` debounce.
    AttachedSrc { cc_status: CcStatus, disconnecting: bool },
}

impl Phase {
    /// Returns the [`DebounceTimer`] active in `self`, if any.
    const fn debounce_timer(self) -> Option<DebounceTimer> {
        match self {
            Self::AttachWait { stage: AttachWaitStage::DebouncingCc, .. } => {
                Some(DebounceTimer::CcAttach)
            }
            Self::AttachWait { stage: AttachWaitStage::DebouncingSnkExit, .. } => {
                Some(DebounceTimer::SnkExit)
            }
            Self::AttachedSrc { disconnecting: true, .. } => Some(DebounceTimer::SrcDisconnect),
            Self::AttachedSnk {
                stage: SinkStage::DebouncingRpRise { .. } | SinkStage::DebouncingRpFall { .. },
                ..
            } => Some(DebounceTimer::RpValueChange),
            _ => None,
        }
    }
}

/// Sub-stage within [`Phase::AttachWait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachWaitStage {
    /// Debouncing `tCCDebounce` before qualifying VBUS and entering `Attached`.
    DebouncingCc,
    /// `tCCDebounce` has expired; waiting only for VBUS qualification (`vVBUSPresent` for Sink,
    /// `vSafe0V` for Source) before entering `Attached`.
    WaitingForVbus,
    /// `AttachWait.SNK` debouncing `tPDDebounce` after CC open before returning to `Unattached`.
    DebouncingSnkExit,
    /// Holding a partner in `AttachWait.{SNK,SRC}` without an active debounce timer until its CC
    /// terminations change: `SRC.Ra/SRC.Ra` or an unsupported Debug Accessory (`SRC.Rd/SRC.Rd`, or
    /// `Rp/Rp` with `DebugAccessory.SNK` disabled).
    /// A partner whose attach failed is held the same way.
    Held,
}

/// Synchronous USB Type-C connection state machine (USB Type-C R2.5, Section 4.5.2).
pub(crate) struct Machine {
    /// Most recent `CC_STATUS` sample; use [`Self::visible_cc_status`] to report `Open` while
    /// disabled.
    pub(crate) cc_status: CcStatus,
    /// Whether port CC terminations are currently disabled.
    pub(crate) disabled: bool,
    /// Current connection lifecycle phase; [`Self::typec_state`] projects it onto [`TypecState`].
    phase: Phase,
    /// Whether an `Rp/Rp` Sink Debug Accessory (e.g. SuzyQ) may enter `Attached.SNK`.
    pub(crate) sink_debug_accessory_mode: bool,
    /// In `AttachWait.SNK`, the earliest sample at the lowest `Rp` level seen since `tCCDebounce`
    /// started; [`Self::commit_attached`] commits both its CC pins and `Rp` when `Rp` rose during
    /// attach qualification and debounces the higher sample for `tRpValueChange` (R2.5 §4.5.2.3).
    sink_attach_base: Option<SinkAttachSample>,
    /// Inspect-only counters and timestamp.
    pub(crate) telemetry: Telemetry,
}

impl Machine {
    pub(crate) const fn new() -> Self {
        Self {
            cc_status: CcStatus::new(CcPinState::Open, CcPinState::Open),
            disabled: false,
            phase: Phase::Unattached,
            sink_debug_accessory_mode: false,
            sink_attach_base: None,
            telemetry: Telemetry::new(),
        }
    }

    /// Projects current phase and disabled state onto the public [`TypecState`].
    pub(crate) const fn typec_state(&self) -> TypecState {
        if self.disabled {
            return TypecState::Disabled;
        }
        match self.phase {
            Phase::Unattached => TypecState::UnattachedSnk,
            Phase::AttachWait { role: PowerRole::Sink, .. } => TypecState::AttachWaitSnk,
            Phase::AttachWait { role: PowerRole::Source, .. } => TypecState::AttachWaitSrc,
            Phase::AttachedSnk { cc_status, .. } if self.is_debug_accessory(cc_status) => {
                TypecState::DebugAccessorySnk
            }
            Phase::AttachedSnk { .. } => TypecState::AttachedSnk,
            Phase::AttachedSrc { .. } => TypecState::AttachedSrc,
        }
    }

    /// Returns `true` if `cc` is a DTS Source (`Rp/Rp`) that may enter `DebugAccessory.SNK`.
    const fn is_debug_accessory(&self, cc: CcStatus) -> bool {
        self.sink_debug_accessory_mode
            && matches!(cc.accessory(), Some(Accessory::DebugAccessorySnk))
    }

    pub(crate) const fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Returns `true` while a partner is held in `AttachWait` until its terminations change.
    #[cfg(test)]
    pub(crate) const fn is_partner_held(&self) -> bool {
        matches!(self.phase, Phase::AttachWait { stage: AttachWaitStage::Held, .. })
    }

    /// Returns `true` while a Sink partner is held in `AttachWait.SNK` until its terminations
    /// change.
    pub(crate) const fn is_held_sink(&self) -> bool {
        !self.disabled
            && matches!(
                self.phase,
                Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held }
            )
    }

    /// Returns the externally visible `CC_STATUS` (`Open` while disabled).
    pub(crate) fn visible_cc_status(&self) -> CcStatus {
        if self.disabled { CcStatus::default() } else { self.cc_status }
    }

    /// Returns the [`DebounceDirection`] of an uncommitted debounce phase in [`Self::phase`].
    pub(crate) const fn uncommitted_debounce_direction(&self) -> Option<DebounceDirection> {
        match self.phase {
            Phase::AttachWait { stage: AttachWaitStage::DebouncingCc, .. } => {
                Some(DebounceDirection::Attach)
            }
            Phase::AttachWait { stage: AttachWaitStage::DebouncingSnkExit, .. }
            | Phase::AttachedSrc { disconnecting: true, .. } => Some(DebounceDirection::Disconnect),
            _ => None,
        }
    }

    /// Returns the [`DebounceTimer`] active in the current phase, if any.
    pub(crate) const fn debounce_timer(&self) -> Option<DebounceTimer> {
        self.phase.debounce_timer()
    }

    fn set_phase(&mut self, next_phase: Phase) {
        self.phase = next_phase;
        if !matches!(
            next_phase,
            Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
            }
        ) {
            self.sink_attach_base = None;
        }
    }

    /// Returns the `CC_STATUS` that drives [`Self::orientation`] and [`Self::accessory`].
    fn active_cc(&self) -> Option<CcStatus> {
        if self.disabled {
            return None;
        }
        match self.phase {
            Phase::AttachedSnk { cc_status, .. } | Phase::AttachedSrc { cc_status, .. } => {
                Some(cc_status)
            }
            Phase::AttachWait {
                stage: AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
                ..
            } => Some(self.cc_status),
            Phase::Unattached
            | Phase::AttachWait {
                stage: AttachWaitStage::DebouncingSnkExit | AttachWaitStage::Held,
                ..
            } => None,
        }
    }

    /// Plug orientation of the committed attachment or debounce candidate.
    pub(crate) fn orientation(&self) -> Option<fconnector::Orientation> {
        self.active_cc()?.orientation()
    }

    /// Supported accessory of the committed attachment or debounce candidate.
    pub(crate) fn accessory(&self) -> Option<Accessory> {
        match self.active_cc()?.accessory() {
            Some(Accessory::DebugAccessorySnk) if self.sink_debug_accessory_mode => {
                Some(Accessory::DebugAccessorySnk)
            }
            Some(Accessory::DebugAccessorySnk | Accessory::UnorientedDebugAccessorySrc) | None => {
                None
            }
        }
    }

    /// `Rp` level the attached Source advertises, as last committed in `Attached.SNK` or
    /// `DebugAccessory.SNK`: a new level counts only once stable for `tRpValueChange` (R2.5
    /// §4.5.2.3). `None` unless attached as a Sink.
    pub(crate) fn sink_rp_level(&self) -> Option<RpValue> {
        match self.phase {
            Phase::AttachedSnk { rp, .. } if !self.disabled => Some(rp),
            _ => None,
        }
    }

    /// Increments `attach_count` once `on_partner_attached` succeeds.
    pub(crate) fn record_attached(&mut self) {
        self.telemetry.attach_count += 1;
    }

    /// Rolls back a failed `Attached` commit for `role` and, if the port is still enabled in
    /// `Attached(role)`, holds the partner in `AttachWait` until its terminations change; returns
    /// whether it entered the hold.
    pub(crate) fn hold_failed_attach(&mut self, role: PowerRole) -> bool {
        let failures = match role {
            PowerRole::Sink => &mut self.telemetry.sink_attach_failure_count,
            PowerRole::Source => &mut self.telemetry.source_attach_failure_count,
        };
        *failures += 1;
        let is_attached_in_role = matches!(
            (role, self.phase),
            (PowerRole::Sink, Phase::AttachedSnk { .. })
                | (PowerRole::Source, Phase::AttachedSrc { .. })
        );
        if is_attached_in_role && !self.disabled {
            self.set_phase(Phase::AttachWait { role, stage: AttachWaitStage::Held });
            return true;
        }
        false
    }

    /// Enters `Phase::Unattached`, clearing any pre-`Unattached` partner termination from
    /// [`Self::cc_status`] and reporting whether one was present (`resample_cc`).
    fn enter_unattached(&mut self, detached_from: Option<PowerRole>) -> Step {
        let resample_cc = self.classify(self.cc_status).is_some();
        if resample_cc {
            self.cc_status = CcStatus::default();
        }
        self.set_phase(Phase::Unattached);
        Step::Advanced(Some(Effect::EnterUnattached { detached_from, resample_cc }))
    }

    /// Settles `phase` when cancelling an uncommitted debounce (`AttachedSrc { disconnecting: true
    /// }` or `SinkStage::DebouncingRp*` returns to steady attachment while keeping the committed
    /// `Rp` level; `Unattached` and `AttachWait` clear `cc_status` in `Unattached`).
    pub(crate) fn cancel_debounce(&mut self) {
        if self.uncommitted_debounce_direction().is_some() {
            self.telemetry.debounce_cancelled_count += 1;
        }
        match self.phase {
            Phase::AttachedSrc { cc_status, .. } => {
                self.cc_status = cc_status;
                self.set_phase(Phase::AttachedSrc { cc_status, disconnecting: false });
            }
            Phase::AttachedSnk {
                cc_status,
                rp,
                stage: SinkStage::DebouncingRpRise { .. } | SinkStage::DebouncingRpFall { .. },
            } => {
                self.set_sink_stage(cc_status, rp, SinkStage::Steady);
            }
            Phase::AttachedSnk { stage: SinkStage::Steady, .. } => {}
            Phase::Unattached | Phase::AttachWait { .. } => {
                self.cc_status = CcStatus::default();
                self.set_phase(Phase::Unattached);
            }
        }
    }

    /// Updates disabled state, returning `true` when changed.
    #[cfg(test)]
    pub(crate) fn set_disabled(&mut self, disabled: bool) -> bool {
        if self.disabled == disabled {
            return false;
        }
        if disabled {
            self.cancel_debounce();
        }
        self.disabled = disabled;
        true
    }

    /// Resets the port to disconnected (`disabled` flag preserved) and cancels any uncommitted
    /// debounce.
    pub(crate) fn reset_to_disconnected(&mut self) {
        if self.uncommitted_debounce_direction().is_some() {
            self.telemetry.debounce_cancelled_count += 1;
        }
        self.cc_status = CcStatus::default();
        self.set_phase(Phase::Unattached);
    }

    /// Advances the state machine on `event` by dispatching on the current [`Phase`].
    pub(crate) fn transition(&mut self, event: Event) -> Step {
        if self.ignores_event(event) {
            return Step::Absorbed;
        }
        let cc = event.cc();
        let prev_cc = std::mem::replace(&mut self.cc_status, cc);

        match self.phase {
            Phase::Unattached => self.enter_attach_wait(None, cc),
            Phase::AttachWait { role, stage } => {
                self.attach_wait_transition(role, stage, event, prev_cc)
            }
            Phase::AttachedSnk { cc_status: committed_cc, rp: committed_rp, stage } => {
                self.sink_attached_transition(committed_cc, committed_rp, stage, event)
            }
            Phase::AttachedSrc { cc_status: committed_cc, disconnecting } => {
                self.source_attached_transition(committed_cc, disconnecting, event)
            }
        }
    }

    /// Returns `true` when the current [`Phase`] ignores `event` without updating
    /// [`Self::cc_status`] or touching any running debounce timer.
    const fn ignores_event(&self, event: Event) -> bool {
        if self.disabled {
            return true;
        }
        match event {
            Event::DebounceExpired { .. } => self.phase.debounce_timer().is_none(),
            Event::VbusChanged { vbus_ready, .. } => !matches!(
                (self.vbus_action(), vbus_ready),
                (VbusAction::QualifyAttach(_), true) | (VbusAction::CheckSinkDisconnect, false)
            ),
            // Ignore intermediate hardware DRP toggle samples (`Looking4Connection == 1`) while
            // `Unattached`.
            Event::CcChanged(cc) => {
                matches!(self.phase, Phase::Unattached) && cc.looking4connection
            }
        }
    }

    fn attach_wait_transition(
        &mut self,
        role: PowerRole,
        stage: AttachWaitStage,
        event: Event,
        prev_cc: CcStatus,
    ) -> Step {
        match (stage, event) {
            // Keep the `tPDDebounce` exit timer running while CC remains Open/toggling.
            (AttachWaitStage::DebouncingSnkExit, Event::CcChanged(cc))
                if self.classify(cc).is_none() =>
            {
                Step::Absorbed
            }
            // `tPDDebounce` elapsed with CC still Open (USB Type-C R2.5 §4.5.2.2.4.2).
            (AttachWaitStage::DebouncingSnkExit, Event::DebounceExpired { cc, .. })
                if self.classify(cc).is_none() =>
            {
                self.enter_unattached(None)
            }
            // A held partner stays held until its terminations change.
            (AttachWaitStage::Held, Event::CcChanged(cc))
                if self.matches_partner(role, prev_cc, cc)
                    || matches!(
                        self.classify(cc),
                        Some(ClassifiedPartner { role: r, attachable: false }) if r == role
                    ) =>
            {
                Step::Absorbed
            }
            // Same-pin `Rp` value or `Ra` change does not reset `tCCDebounce` or VBUS wait; a
            // new `Rp` level must still be stable for `tRpValueChange` once attached.
            (
                AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
                Event::CcChanged(cc),
            ) if self.matches_partner(role, prev_cc, cc) => {
                self.note_sink_attach_sample(role, prev_cc, cc);
                Step::Absorbed
            }
            // `tCCDebounce` elapsed: commit if VBUS is qualified (`vSafe0V` / `vVBUSPresent`),
            // otherwise wait in `AttachWait` for `Event::VbusChanged`.
            (AttachWaitStage::DebouncingCc, Event::DebounceExpired { cc, vbus_ready })
                if self.matches_partner(role, prev_cc, cc) =>
            {
                self.note_sink_attach_sample(role, prev_cc, cc);
                if vbus_ready {
                    self.commit_attached(role, cc)
                } else {
                    self.set_phase(Phase::AttachWait {
                        role,
                        stage: AttachWaitStage::WaitingForVbus,
                    });
                    Step::Advanced(None)
                }
            }
            // VBUS became qualified after `tCCDebounce` had already completed.
            (AttachWaitStage::WaitingForVbus, Event::VbusChanged { cc, .. })
                if self.matches_partner(role, prev_cc, cc) =>
            {
                self.note_sink_attach_sample(role, prev_cc, cc);
                self.commit_attached(role, cc)
            }
            // Partner role, CC pin, or Open state changed: re-evaluate `AttachWait`.
            (
                _,
                Event::CcChanged(cc)
                | Event::DebounceExpired { cc, .. }
                | Event::VbusChanged { cc, .. },
            ) => {
                if self.uncommitted_debounce_direction().is_some() {
                    self.telemetry.debounce_cancelled_count += 1;
                }
                self.enter_attach_wait(Some(role), cc)
            }
        }
    }

    fn source_attached_transition(
        &mut self,
        committed_cc: CcStatus,
        disconnecting: bool,
        event: Event,
    ) -> Step {
        match event {
            // Partner `Rd` still present on the monitored CC pin: cancel any pending
            // `tSRCDisconnect` debounce.
            Event::CcChanged(cc) | Event::DebounceExpired { cc, .. }
                if cc.keeps_attached_source(committed_cc) =>
            {
                if disconnecting {
                    self.telemetry.debounce_cancelled_count += 1;
                    self.set_phase(Phase::AttachedSrc {
                        cc_status: committed_cc,
                        disconnecting: false,
                    });
                    Step::Advanced(None)
                } else {
                    Step::Absorbed
                }
            }
            // Keep the running `tSRCDisconnect` timer across non-matching CC bounces.
            Event::CcChanged(_) if disconnecting => Step::Absorbed,
            // Source partner `Rd` lost on the active CC pin: start `tSRCDisconnect` (R2.5
            // §4.5.2.2.9.2).
            Event::CcChanged(_) => {
                self.set_phase(Phase::AttachedSrc { cc_status: committed_cc, disconnecting: true });
                Step::Advanced(None)
            }
            // `tSRCDisconnect` elapsed without `Rd` returning: commit detach.
            Event::DebounceExpired { .. } if disconnecting => {
                self.telemetry.disconnect_count += 1;
                self.enter_unattached(Some(PowerRole::Source))
            }
            _ => Step::Absorbed,
        }
    }

    /// Transitions from `current_wait_role` (`None` when in `Unattached`) into `AttachWait` or the
    /// appropriate `Open` exit path for `cc`.
    fn enter_attach_wait(&mut self, current_wait_role: Option<PowerRole>, cc: CcStatus) -> Step {
        match self.classify(cc) {
            Some(ClassifiedPartner { role, attachable }) => {
                let stage =
                    if attachable { AttachWaitStage::DebouncingCc } else { AttachWaitStage::Held };
                self.set_phase(Phase::AttachWait { role, stage });
                // A new `tCCDebounce` starts, so an `Rp` change seen before it is already settled.
                self.sink_attach_base = None;
                if current_wait_role == Some(role) {
                    Step::Advanced(None)
                } else {
                    Step::Advanced(Some(Effect::EnterAttachWait(role)))
                }
            }
            None => match current_wait_role {
                // `AttachWait.SNK -> Unattached.SNK` requires `tPDDebounce` (R2.5 §4.5.2.2.4.2).
                Some(PowerRole::Sink) => {
                    self.set_phase(Phase::AttachWait {
                        role: PowerRole::Sink,
                        stage: AttachWaitStage::DebouncingSnkExit,
                    });
                    Step::Advanced(None)
                }
                // `AttachWait.SRC` exits immediately on `SRC.Open`, a DRP to `Unattached.SNK` (R2.5
                // §4.5.2.2.8.2). In `Unattached`, `Looking4Connection == 0` with `Open` or lone
                // `SRC.Ra` means autonomous DRP toggling halted and must be re-armed.
                Some(PowerRole::Source) | None => self.enter_unattached(None),
            },
        }
    }

    /// Commits an `Attached.{SNK,SRC}` state transition.
    ///
    /// A Sink commits both the CC pins (`orientation`) and `Rp` from the earliest lowest-`Rp`
    /// sample seen during `AttachWait.SNK`: a rise debounces the higher sample for
    /// `tRpValueChange` (R2.5 §4.5.2.3), an equal-`Rp` DTS pin-pair step keeps the initial
    /// orientation, and a drop commits the lower sample at once.
    fn commit_attached(&mut self, role: PowerRole, cc: CcStatus) -> Step {
        let sink_base = self.sink_attach_base.take();
        match role {
            PowerRole::Sink => {
                let Some(cc_rp) = cc.sink_rp_level() else {
                    debug_assert!(false, "attachable Sink CC must have an Rp level: {cc:?}");
                    return self.enter_attach_wait(Some(PowerRole::Sink), cc);
                };
                let (cc_status, rp, stage) = match sink_base {
                    Some(base) if base.rp < cc_rp => {
                        (base.cc_status, base.rp, SinkStage::DebouncingRpRise { candidate: cc })
                    }
                    Some(base) if base.rp == cc_rp => (base.cc_status, cc_rp, SinkStage::Steady),
                    _ => (cc, cc_rp, SinkStage::Steady),
                };
                self.set_phase(Phase::AttachedSnk { cc_status, rp, stage });
                Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
            }
            PowerRole::Source => {
                self.set_phase(Phase::AttachedSrc { cc_status: cc, disconnecting: false });
                Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
            }
        }
    }

    /// Returns the [`VbusAction`] that VBUS change notifications should run in the current phase.
    pub(crate) const fn vbus_action(&self) -> VbusAction {
        if self.disabled {
            return VbusAction::NoTransition;
        }
        match self.phase {
            Phase::AttachWait { role, stage: AttachWaitStage::WaitingForVbus } => {
                VbusAction::QualifyAttach(role)
            }
            Phase::AttachedSnk { .. } => VbusAction::CheckSinkDisconnect,
            _ => VbusAction::NoTransition,
        }
    }

    /// Returns `true` when `cc` is an attachable partner in `role` sharing the same connected CC
    /// pin(s) as `prev_cc`.
    fn matches_partner(&self, role: PowerRole, prev_cc: CcStatus, cc: CcStatus) -> bool {
        !cc.looking4connection
            && self.attachable_role(cc) == Some(role)
            && prev_cc.shares_connected_cc_pin(cc)
    }

    /// Returns the [`PowerRole`] `cc` may attach as on this port.
    pub(crate) fn attachable_role(&self, cc: CcStatus) -> Option<PowerRole> {
        cc.attachable_role(self.sink_debug_accessory_mode)
    }

    /// Classifies `cc` into an attachable or held [`ClassifiedPartner`], or `None` when open or
    /// toggling (`Looking4Connection`).
    fn classify(&self, cc: CcStatus) -> Option<ClassifiedPartner> {
        if cc.looking4connection {
            return None;
        }
        if let Some(role) = self.attachable_role(cc) {
            return Some(ClassifiedPartner { role, attachable: true });
        }
        // `SRC.Ra/SRC.Ra` enters `AttachWait.SRC` (R2.5 §4.5.2.2.7.2) but never attaches.
        let unattachable_role = if cc.is_ra_ra() {
            Some(PowerRole::Source)
        } else {
            match cc.accessory() {
                Some(Accessory::DebugAccessorySnk) => Some(PowerRole::Sink),
                Some(Accessory::UnorientedDebugAccessorySrc) => Some(PowerRole::Source),
                None => None,
            }
        };
        unattachable_role.map(|role| ClassifiedPartner { role, attachable: false })
    }

    /// Advances the state machine on initial hardware sync (`sync_initial_state`): commits
    /// `Attached.SNK` directly from `Unattached` or a pending `AttachWait.SNK` when a
    /// VBUS-supplying Source partner is present (partner and VBUS were already settled prior to
    /// driver startup), debounces `tCCDebounce` when VBUS is absent, qualifies an existing
    /// `WaitingForVbus` stage once VBUS is ready, and still enters `Unattached` when DRP toggling
    /// (`Looking4Connection`) left the port unattached so `on_unattached` runs.
    pub(crate) fn transition_initial(&mut self, cc: CcStatus, vbus_ready: bool) -> Step {
        if self.disabled {
            return Step::Absorbed;
        }
        if vbus_ready
            && self.phase == Phase::Unattached
            && !cc.looking4connection
            && self.attachable_role(cc) == Some(PowerRole::Sink)
        {
            debug_assert!(self.sink_attach_base.is_none());
            self.cc_status = cc;
            return self.commit_attached(PowerRole::Sink, cc);
        }
        let mut step = self.transition(Event::CcChanged(cc));
        // Only cold-boot `Unattached` (handled above) or an already-pending `AttachWait.SNK`
        // bypasses `tCCDebounce`: switching from `AttachWait.SRC` must run `enter_attach_wait`
        // and `tCCDebounce` before attaching.
        if vbus_ready && matches!(step, Step::Absorbed | Step::Advanced(None)) {
            if matches!(
                self.phase,
                Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
            ) {
                step = self.transition(Event::DebounceExpired { cc, vbus_ready });
            } else if matches!(
                self.phase,
                Phase::AttachWait { stage: AttachWaitStage::WaitingForVbus, .. }
            ) {
                step = self.transition(Event::VbusChanged { cc, vbus_ready });
            }
        }
        if self.phase == Phase::Unattached && step == Step::Absorbed {
            self.cc_status = cc;
            step = self.enter_unattached(None);
        }
        step
    }

    /// Returns the VBUS predicate role to pre-sample during [`Self::transition_initial`].
    ///
    /// A fresh Sink partner always debounces `tCCDebounce` first (R2.5 §4.5.2.2.8.2), so `vSafe0V`
    /// is pre-sampled only when the port is already waiting in `AttachWait.SRC` (`WaitingForVbus`).
    pub(crate) fn initial_vbus_role(&self, cc: CcStatus) -> Option<PowerRole> {
        match self.attachable_role(cc) {
            Some(PowerRole::Source)
                if !matches!(
                    self.phase,
                    Phase::AttachWait {
                        role: PowerRole::Source,
                        stage: AttachWaitStage::WaitingForVbus,
                    }
                ) =>
            {
                None
            }
            role => role,
        }
    }

    /// Returns the [`PowerRole`] whose VBUS check must be sampled to qualify `cc` when
    /// `tCCDebounce` expires in `AttachWait`, or `None` if `cc` does not match the current
    /// attach candidate.
    pub(crate) fn attach_debounce_vbus_role(&self, cc: CcStatus) -> Option<PowerRole> {
        match self.phase {
            Phase::AttachWait { role, stage: AttachWaitStage::DebouncingCc }
                if self.matches_partner(role, self.cc_status, cc) =>
            {
                Some(role)
            }
            _ => None,
        }
    }

    /// Returns `true` if the current phase permits disarming an armed VBUS discharge once
    /// `vSafe0V` is debounced (`Unattached` or `AttachWait.SRC`, where residual VBUS from a prior
    /// session may still be decaying toward `vSafe0V`).
    pub(crate) const fn allows_vsafe0v_disarm(&self) -> bool {
        !self.disabled
            && matches!(
                self.phase,
                Phase::Unattached | Phase::AttachWait { role: PowerRole::Source, .. }
            )
    }

    /// Returns `true` when the current phase must not keep VBUS discharge armed and is not
    /// debouncing a disconnect back to `Unattached`.
    pub(crate) const fn should_retry_failed_disarm(&self) -> bool {
        !self.disabled
            && !self.allows_vsafe0v_disarm()
            && !matches!(self.uncommitted_debounce_direction(), Some(DebounceDirection::Disconnect))
    }

    /// Returns `true` if the current phase permits invoking [`crate::Typec::on_vsafe0v`]
    /// (`Unattached` only).
    pub(crate) const fn allows_on_vsafe0v(&self) -> bool {
        !self.disabled && matches!(self.phase, Phase::Unattached)
    }
}

/// Partner role and whether its terminations can reach `Attached.*` rather than staying `Held`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClassifiedPartner {
    role: PowerRole,
    attachable: bool,
}

/// Action [`crate::runner`] should perform on a VBUS change notification in the current [`Phase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VbusAction {
    /// `AttachWait.{SNK,SRC}` has completed `tCCDebounce` and waits for VBUS qualification
    /// (`is_vbus_present` for Sink, `is_vbus_safe0v` for Source).
    QualifyAttach(PowerRole),
    /// `Attached.SNK` / `DebugAccessory.SNK` monitors for VBUS falling below `vSinkDisconnect`.
    CheckSinkDisconnect,
    /// Phase does not transition on VBUS changes (`runner` checks only deferred sink-path cleanup
    /// and residual `vSafe0V` discharge disarm).
    NoTransition,
}

/// State machine transition events. Each carries the latest `CC_STATUS` sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// `CC_STATUS` changed.
    CcChanged(CcStatus),
    /// The running debounce timer expired; `vbus_ready` is whether VBUS is qualified for the
    /// `AttachWait` role (`vVBUSPresent` for Sink, `vSafe0V` for Source).
    DebounceExpired { cc: CcStatus, vbus_ready: bool },
    /// VBUS crossed a threshold; `vbus_ready` is the role's VBUS predicate (`vVBUSPresent` for
    /// Sink, `vSafe0V` for Source), matched against [`Machine::vbus_action`].
    VbusChanged { cc: CcStatus, vbus_ready: bool },
}

impl Event {
    const fn cc(self) -> CcStatus {
        match self {
            Self::CcChanged(cc)
            | Self::DebounceExpired { cc, .. }
            | Self::VbusChanged { cc, .. } => cc,
        }
    }
}

/// Debounce timer window reported by [`Phase::debounce_timer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DebounceTimer {
    /// `tCCDebounce` before `AttachWait.* -> Attached.*`.
    CcAttach,
    /// `tPDDebounce` before `AttachWait.SNK -> Unattached.SNK`.
    SnkExit,
    /// `tSRCDisconnect` before `Attached.SRC -> Unattached`.
    SrcDisconnect,
    /// `tRpValueChange` before reporting a new `Attached.SNK` `Rp` level.
    RpValueChange,
}

impl DebounceTimer {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CcAttach => "cc_attach",
            Self::SnkExit => "snk_exit",
            Self::SrcDisconnect => "disconnect",
            Self::RpValueChange => "rp_value_change",
        }
    }
}

/// Outcome of feeding an [`Event`] into [`Machine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum Step {
    /// Event was ignored or absorbed without resetting the running debounce timer (sub-stage
    /// bounds such as `SinkStage::DebouncingRpFall::floor` may tighten).
    Absorbed,
    /// Phase updated: run `Option<Effect>` on the runner, then sync the debounce timer to
    /// [`Machine::debounce_timer`].
    Advanced(Option<Effect>),
}

impl Step {
    /// Whether this step stayed in the current phase (or only updated Sink `Rp`) rather than
    /// running `enter_attach_wait`, `attach_partner`, or `EnterUnattached`.
    pub(crate) const fn is_settled_non_transition(self) -> bool {
        matches!(self, Self::Absorbed | Self::Advanced(None | Some(Effect::NotifySinkRp(_))))
    }
}

/// Runner entry or notification hook emitted by [`Step::Advanced`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Entered `Unattached` (`detached_from` carries the ended `Attached.*` role on detach;
    /// `resample_cc` when the triggering sample still carried a partner termination).
    EnterUnattached { detached_from: Option<PowerRole>, resample_cc: bool },
    /// Entered `AttachWait.{SNK,SRC}` for `role` from `Unattached` or the opposite `AttachWait`
    /// role.
    EnterAttachWait(PowerRole),
    /// Committed `Attached.{SNK,SRC}` for `role`.
    EnterAttached(PowerRole),
    /// Confirmed a new `Rp` current advertisement while attached as a Sink.
    NotifySinkRp(RpValue),
}

/// Inspect-only telemetry: transitions update it but never branch on it.
pub(crate) struct Telemetry {
    /// Time of the last [`TypecState`] change.
    pub(crate) last_updated_time: zx::BootInstant,
    /// Committed `Attached.*` transitions.
    pub(crate) attach_count: u64,
    /// Confirmed detaches from `Attached.*`.
    pub(crate) disconnect_count: u64,
    /// Debounce timers cancelled or superseded before committing.
    pub(crate) debounce_cancelled_count: u64,
    /// Debounced Sink `Rp` level changes reported through `on_sink_rp_changed`.
    pub(crate) rp_change_count: u64,
    /// Sink attaches whose sink path or `on_partner_attached` failed, holding the partner.
    pub(crate) sink_attach_failure_count: u64,
    /// Source attaches whose discharge disarm, sink path close, or `on_partner_attached` failed,
    /// holding the partner.
    pub(crate) source_attach_failure_count: u64,
    /// Times `vSafe0V` did not arrive within the discharge timeout.
    pub(crate) discharge_timeout_count: u64,
}

impl Telemetry {
    const fn new() -> Self {
        Self {
            last_updated_time: zx::BootInstant::ZERO,
            attach_count: 0,
            disconnect_count: 0,
            debounce_cancelled_count: 0,
            rp_change_count: 0,
            sink_attach_failure_count: 0,
            source_attach_failure_count: 0,
            discharge_timeout_count: 0,
        }
    }
}

// Mounted inside `state` so transition-table tests can inspect private `Phase` directly.
#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
