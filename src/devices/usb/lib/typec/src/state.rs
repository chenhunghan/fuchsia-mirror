// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronous USB Type-C connection state machine (USB Type-C R2.5, Section 4.5.2).
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "wired to the async port runner in the follow-up commit")
)]

use crate::cc::{Accessory, CcPinState, CcStatus, PowerRole, RpValue};
use crate::port::{DebounceDirection, TypecState};
use fidl_next_fuchsia_hardware_usb_connector as fconnector;

/// Connection phase of [`Machine`]; projected onto [`TypecState`] by [`Machine::typec_state`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Port is unattached or actively DRP-toggling (`Looking4Connection`).
    #[default]
    Unattached,
    /// `AttachWait.{SNK,SRC}`: qualifying a partner termination before committing `Attached`.
    AttachWait { role: PowerRole, stage: AttachWaitStage },
    /// `Attached.{SNK,SRC}` or `DebugAccessory.SNK`.
    Attached { role: PowerRole, cc_status: CcStatus, stage: AttachedStage },
}

impl Phase {
    /// Returns the [`DebounceTimer`] active in `self`, if any.
    pub(crate) const fn debounce_timer(self) -> Option<DebounceTimer> {
        match self {
            Self::AttachWait { stage: AttachWaitStage::DebouncingCc, .. } => {
                Some(DebounceTimer::CcAttach)
            }
            Self::AttachWait { stage: AttachWaitStage::DebouncingSnkExit, .. } => {
                Some(DebounceTimer::SnkExit)
            }
            Self::Attached {
                role: PowerRole::Source, stage: AttachedStage::Disconnecting, ..
            } => Some(DebounceTimer::SrcDisconnect),
            _ => None,
        }
    }
}

/// Sub-stage within [`Phase::Attached`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedStage {
    /// Partner is steadily attached.
    Steady,
    /// Debouncing a CC disconnect (`tSRCDisconnect`).
    Disconnecting,
}

/// Sub-stage within [`Phase::AttachWait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachWaitStage {
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
    pub(crate) phase: Phase,
    /// Whether an `Rp/Rp` Sink Debug Accessory (e.g. SuzyQ) may enter `Attached.SNK`.
    pub(crate) sink_debug_accessory_mode: bool,
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
            Phase::Attached { role: PowerRole::Sink, cc_status, .. }
                if self.is_debug_accessory(cc_status) =>
            {
                TypecState::DebugAccessorySnk
            }
            Phase::Attached { role: PowerRole::Sink, .. } => TypecState::AttachedSnk,
            Phase::Attached { role: PowerRole::Source, .. } => TypecState::AttachedSrc,
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
            | Phase::Attached {
                role: PowerRole::Source,
                stage: AttachedStage::Disconnecting,
                ..
            } => Some(DebounceDirection::Disconnect),
            _ => None,
        }
    }

    pub(crate) fn set_phase(&mut self, next_phase: Phase) {
        let prev_state = self.typec_state();
        self.phase = next_phase;
        if self.typec_state() != prev_state {
            self.telemetry.last_updated_time = zx::BootInstant::get();
        }
    }

    /// Returns the `CC_STATUS` that drives [`Self::orientation`] and [`Self::accessory`].
    fn active_cc(&self) -> Option<CcStatus> {
        if self.disabled {
            return None;
        }
        match self.phase {
            Phase::Attached { cc_status, .. } => Some(cc_status),
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
    /// `DebugAccessory.SNK`; `None` unless attached as a Sink.
    pub(crate) fn sink_rp_level(&self) -> Option<RpValue> {
        match self.phase {
            Phase::Attached { role: PowerRole::Sink, cc_status, .. } if !self.disabled => {
                cc_status.sink_rp_level()
            }
            _ => None,
        }
    }

    /// Settles `phase` when cancelling an uncommitted debounce (`AttachedStage::Disconnecting`
    /// returns to `AttachedStage::Steady`; `AttachWait` returns to `Unattached`).
    #[cfg(test)]
    pub(crate) fn cancel_debounce(&mut self) {
        if self.uncommitted_debounce_direction().is_some() {
            self.telemetry.debounce_cancelled_count += 1;
        }
        match self.phase {
            Phase::Attached { role, cc_status, stage: AttachedStage::Disconnecting } => {
                self.cc_status = cc_status;
                self.set_phase(Phase::Attached { role, cc_status, stage: AttachedStage::Steady });
            }
            Phase::Attached { stage: AttachedStage::Steady, .. } => {}
            Phase::Unattached => {
                self.cc_status = CcStatus::default();
            }
            Phase::AttachWait { .. } => {
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
        self.telemetry.last_updated_time = zx::BootInstant::get();
        true
    }

    /// Resets the port to disconnected (`disabled` flag preserved) and cancels any uncommitted
    /// debounce.
    #[cfg(test)]
    pub(crate) fn reset_to_disconnected(&mut self) {
        if self.uncommitted_debounce_direction().is_some() {
            self.telemetry.debounce_cancelled_count += 1;
        }
        self.cc_status = CcStatus::default();
        self.set_phase(Phase::Unattached);
    }

    /// Advances the state machine on `event` by dispatching on the current [`Phase`].
    pub(crate) fn transition(&mut self, event: Event) -> Transition {
        if self.ignores_event(event) {
            return Transition::Absorbed;
        }
        let cc = event.cc();
        let prev_cc = std::mem::replace(&mut self.cc_status, cc);

        match self.phase {
            Phase::Unattached => self.enter_attach_wait(None, cc),
            Phase::AttachWait { role, stage } => {
                self.attach_wait_transition(role, stage, event, prev_cc)
            }
            Phase::Attached { role, cc_status: committed_cc, stage } => {
                self.attached_transition(role, committed_cc, stage, event)
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
                self.vbus_change_target(),
                Some(target) if target.expect_ready == vbus_ready
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
    ) -> Transition {
        match (stage, event) {
            // Keep the `tPDDebounce` exit timer running while CC remains Open/toggling.
            (AttachWaitStage::DebouncingSnkExit, Event::CcChanged(cc))
                if self.classify(cc).is_none() =>
            {
                Transition::Absorbed
            }
            // `tPDDebounce` elapsed with CC still Open (USB Type-C R2.5 §4.5.2.2.4.2).
            (AttachWaitStage::DebouncingSnkExit, Event::DebounceExpired { cc, .. })
                if self.classify(cc).is_none() =>
            {
                self.set_phase(Phase::Unattached);
                Transition::Moved
            }
            // A held partner stays held until its terminations change.
            (AttachWaitStage::Held, Event::CcChanged(cc))
                if matches!(
                    self.classify(cc),
                    Some(ClassifiedPartner { role: r, attachable: false }) if r == role
                ) =>
            {
                Transition::Absorbed
            }
            // Same-pin `Rp` value or `Ra` change does not reset `tCCDebounce` or VBUS wait.
            (
                AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
                Event::CcChanged(cc),
            ) if self.matches_partner(role, prev_cc, cc) => Transition::Absorbed,
            // `tCCDebounce` elapsed: commit if VBUS is qualified (`vSafe0V` / `vVBUSPresent`),
            // otherwise wait in `AttachWait` for `Event::VbusChanged`.
            (AttachWaitStage::DebouncingCc, Event::DebounceExpired { cc, vbus_ready })
                if self.matches_partner(role, prev_cc, cc) =>
            {
                if vbus_ready {
                    self.commit_attached(role, cc)
                } else {
                    self.set_phase(Phase::AttachWait {
                        role,
                        stage: AttachWaitStage::WaitingForVbus,
                    });
                    Transition::Moved
                }
            }
            // VBUS became qualified after `tCCDebounce` had already completed.
            (AttachWaitStage::WaitingForVbus, Event::VbusChanged { cc, .. })
                if self.matches_partner(role, prev_cc, cc) =>
            {
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

    fn attached_transition(
        &mut self,
        role: PowerRole,
        committed_cc: CcStatus,
        stage: AttachedStage,
        event: Event,
    ) -> Transition {
        match event {
            // Partner still present on the same CC pin(s): cancel any pending Source disconnect
            // debounce and keep the `Rp` level committed at attach.
            // TODO(https://fxbug.dev/575661321): Report `Rp` changes once stable for
            // `tRpValueChange` (R2.5 §4.5.2.3).
            Event::CcChanged(cc) | Event::DebounceExpired { cc, .. }
                if self.keeps_attached_partner(role, committed_cc, cc) =>
            {
                if stage == AttachedStage::Disconnecting {
                    self.telemetry.debounce_cancelled_count += 1;
                    self.set_phase(Phase::Attached {
                        role,
                        cc_status: committed_cc,
                        stage: AttachedStage::Steady,
                    });
                    Transition::Moved
                } else {
                    Transition::Absorbed
                }
            }
            // `Attached.SNK` and `DebugAccessory.SNK` exit only on VBUS loss (`vSinkDisconnect`,
            // R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2); once the runner re-samples CC in
            // `Unattached.SNK`, a still-asserted `Rp` re-enters `AttachWait.SNK` and `tCCDebounce`.
            Event::VbusChanged { .. } if role == PowerRole::Sink => {
                self.telemetry.disconnect_count += 1;
                self.set_phase(Phase::Unattached);
                Transition::Moved
            }
            // Without USB PD, CC changes while VBUS remains present (e.g. ground offset pulling
            // Default `Rp` into `SNK.Open` under load) do not detach a Sink (R2.5 §4.5.2.2.5.2,
            // §4.5.2.2.19.2); the committed `Rp` level stays in effect.
            Event::CcChanged(_) if role == PowerRole::Sink => Transition::Absorbed,
            // Keep the running `tSRCDisconnect` timer across non-matching CC bounces.
            Event::CcChanged(_) if stage == AttachedStage::Disconnecting => Transition::Absorbed,
            // Source partner `Rd` lost on the active CC pin: start `tSRCDisconnect` (R2.5
            // §4.5.2.2.9.2).
            Event::CcChanged(_) => {
                self.set_phase(Phase::Attached {
                    role,
                    cc_status: committed_cc,
                    stage: AttachedStage::Disconnecting,
                });
                Transition::Moved
            }
            // `tSRCDisconnect` elapsed without `Rd` returning: commit detach.
            Event::DebounceExpired { .. } if stage == AttachedStage::Disconnecting => {
                self.telemetry.disconnect_count += 1;
                self.set_phase(Phase::Unattached);
                Transition::Moved
            }
            _ => Transition::Absorbed,
        }
    }

    /// Transitions from `current_wait_role` (`None` when in `Unattached`) into `AttachWait` or the
    /// appropriate `Open` exit path for `cc`.
    fn enter_attach_wait(
        &mut self,
        current_wait_role: Option<PowerRole>,
        cc: CcStatus,
    ) -> Transition {
        match self.classify(cc) {
            Some(ClassifiedPartner { role, attachable }) => {
                let stage =
                    if attachable { AttachWaitStage::DebouncingCc } else { AttachWaitStage::Held };
                self.set_phase(Phase::AttachWait { role, stage });
            }
            None => match current_wait_role {
                // `AttachWait.SNK -> Unattached.SNK` requires `tPDDebounce` (R2.5 §4.5.2.2.4.2).
                Some(PowerRole::Sink) => {
                    self.set_phase(Phase::AttachWait {
                        role: PowerRole::Sink,
                        stage: AttachWaitStage::DebouncingSnkExit,
                    });
                }
                // `AttachWait.SRC` exits immediately on `SRC.Open`, a DRP to `Unattached.SNK` (R2.5
                // §4.5.2.2.8.2). In `Unattached`, `Looking4Connection == 0` with `Open` or lone
                // `SRC.Ra` means autonomous DRP toggling halted and must be re-armed (`Moved`).
                Some(PowerRole::Source) | None => {
                    self.set_phase(Phase::Unattached);
                }
            },
        }
        Transition::Moved
    }

    /// Commits an `Attached.{SNK,SRC}` state transition.
    fn commit_attached(&mut self, role: PowerRole, cc: CcStatus) -> Transition {
        self.cc_status = cc;
        self.set_phase(Phase::Attached { role, cc_status: cc, stage: AttachedStage::Steady });
        self.telemetry.attach_count += 1;
        Transition::Moved
    }

    /// Returns the [`VbusTarget`] that VBUS change notifications should sample in the current
    /// phase, or `None` if the current phase does not react to VBUS changes.
    pub(crate) const fn vbus_change_target(&self) -> Option<VbusTarget> {
        if self.disabled {
            return None;
        }
        match self.phase {
            Phase::AttachWait { role, stage: AttachWaitStage::WaitingForVbus } => {
                Some(VbusTarget { role, expect_ready: true })
            }
            Phase::Attached { role: PowerRole::Sink, .. } => {
                Some(VbusTarget { role: PowerRole::Sink, expect_ready: false })
            }
            _ => None,
        }
    }

    /// Returns `true` when `cc` is an attachable partner in `role` sharing the same connected CC
    /// pin(s) as `prev_cc`.
    fn matches_partner(&self, role: PowerRole, prev_cc: CcStatus, cc: CcStatus) -> bool {
        !cc.looking4connection
            && self.attachable_role(cc) == Some(role)
            && prev_cc.shares_connected_cc_pin(cc)
    }

    /// Returns `true` when `cc` keeps an `Attached.{SNK,SRC}` partner on its committed CC pin(s)
    /// (for `Attached.SRC`, monitoring only the single connected CC pin for `SRC.Open` per R2.5
    /// §4.5.2.2.9.1–2).
    fn keeps_attached_partner(
        &self,
        role: PowerRole,
        committed_cc: CcStatus,
        cc: CcStatus,
    ) -> bool {
        match role {
            PowerRole::Sink => self.matches_partner(role, committed_cc, cc),
            PowerRole::Source => cc.keeps_attached_source(committed_cc),
        }
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
}

/// Partner role and whether its terminations can reach `Attached.*` rather than staying `Held`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClassifiedPartner {
    role: PowerRole,
    attachable: bool,
}

/// VBUS condition that triggers an [`Event::VbusChanged`] transition in the current [`Phase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VbusTarget {
    /// Power role whose VBUS predicate (`is_vbus_present` or `is_vbus_safe0v`) is sampled.
    pub(crate) role: PowerRole,
    /// Whether the phase waits for the predicate to become `true` (attach qualification) or `false`
    /// (`Attached.SNK` VBUS loss).
    pub(crate) expect_ready: bool,
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
    /// Sink, `vSafe0V` for Source), matched against [`Machine::vbus_change_target`].
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
}

impl DebounceTimer {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CcAttach => "cc_attach",
            Self::SnkExit => "snk_exit",
            Self::SrcDisconnect => "disconnect",
        }
    }
}

/// Outcome of a [`Machine`] transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum Transition {
    /// Event absorbed in the current stage; keep any running debounce timer untouched.
    Absorbed,
    /// State or debounce window moved; cancel any running debounce timer, arm a fresh timer for
    /// [`Phase::debounce_timer`], and apply `(prev_phase, next_phase)` in the runner.
    Moved,
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
}

impl Telemetry {
    const fn new() -> Self {
        Self {
            last_updated_time: zx::BootInstant::ZERO,
            attach_count: 0,
            disconnect_count: 0,
            debounce_cancelled_count: 0,
        }
    }
}
