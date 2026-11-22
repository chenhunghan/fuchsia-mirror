// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! VBUS sink-path and discharge tracking and cached controller writes.

use crate::actor::{PortActor, TimerKind};
use crate::cc::PowerRole;
use crate::runner::retry_once;
use crate::typec::{Typec, TypecExt};
use anyhow::Result;

/// Cached hardware switch state for [`crate::PortController::write_vbus_discharge`] and
/// [`crate::PortController::write_vbus_sink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerPathState {
    /// No confirmed write yet, or an enable (`true`) write failed when not enabled.
    Unknown,
    /// Last confirmed write disabled/disarmed the path (`false`), or `write_vbus_discharge(true)`
    /// / `write_vbus_sink(true)` reported `Ok(false)` (unsupported or externally managed).
    Disabled,
    /// Active, or potentially still active after a write error or `vSafe0V` dip; `confirmed = false`
    /// forces the next write in either direction and schedules disarm retries.
    Enabled { confirmed: bool },
}

impl PowerPathState {
    pub(crate) const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    pub(crate) const fn is_unconfirmed_enabled(self) -> bool {
        matches!(self, Self::Enabled { confirmed: false })
    }

    pub(crate) const fn matches_confirmed(self, enabled: bool) -> bool {
        matches!(
            (self, enabled),
            (Self::Enabled { confirmed: true }, true) | (Self::Disabled, false)
        )
    }

    const fn after_write_error(self) -> Self {
        if self.is_enabled() { Self::Enabled { confirmed: false } } else { Self::Unknown }
    }
}

/// Cached power-path state controlled by [`crate::TypecPort`].
pub(crate) struct HardwareState {
    /// Cached state of [`crate::PortController::write_vbus_discharge`].
    pub(crate) vbus_discharge: PowerPathState,
    /// Cached state of [`crate::PortController::write_vbus_sink`].
    pub(crate) vbus_sink: PowerPathState,
    /// Whether VBUS has been observed below `vVBUSPresent` since the current VBUS discharge was
    /// armed, distinguishing external partner VBUS arrival in `Unattached` from our own decaying
    /// 5 V Source rail.
    pub(crate) vbus_seen_below_present: bool,
}

impl HardwareState {
    pub(crate) const fn new() -> Self {
        Self {
            vbus_discharge: PowerPathState::Unknown,
            vbus_sink: PowerPathState::Unknown,
            vbus_seen_below_present: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn reset(&mut self) {
        self.vbus_sink = PowerPathState::Unknown;
        self.vbus_discharge = PowerPathState::Unknown;
        self.vbus_seen_below_present = false;
    }
}

/// VBUS state observed on entering `Unattached`, used by [`UnattachedPowerPlan::decide`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnattachedVbus {
    /// Below `vSafe0V`.
    Safe0v,
    /// Above `vSafe0V` from an ended session or an already-confirmed residual discharge.
    Residual,
    /// Above `vSafe0V` and below `vVBUSPresent` on a non-detach entry with no confirmed discharge.
    SubPresentGap,
    /// Live 5 V partner VBUS on a non-detach entry with no confirmed discharge.
    PartnerLive,
}

/// Power-path actions [`PortActor::prepare_unattached`] takes when entering `Unattached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnattachedPowerPlan {
    /// Whether to close any open or unknown VBUS sink path (`write_vbus_sink(false)`).
    pub(crate) close_sink: bool,
    /// Whether VBUS discharge should be armed (`true`) or disarmed (`false`).
    pub(crate) arm_discharge: bool,
}

impl UnattachedPowerPlan {
    /// Chooses whether `Unattached` closes the sink path and arms VBUS discharge:
    /// - [`UnattachedVbus::Safe0v`]: VBUS is already below `vSafe0V`, so close any sink path and
    ///   disarm discharge.
    /// - [`UnattachedVbus::PartnerLive`]: entering `Unattached` without a prior detach or confirmed
    ///   discharge while 5 V partner VBUS is present (e.g. dead-battery sinking, a held Sink whose
    ///   CC glitched `SNK.Open` under load, or a legacy Type-A-to-C cable whose VBUS mated before
    ///   CC settled) preserves the sink path and disarms discharge rather than clamping live
    ///   partner VBUS.
    /// - [`UnattachedVbus::Residual`] | [`UnattachedVbus::SubPresentGap`]: close the sink path and
    ///   arm discharge to bleed residual or sub-`vVBUSPresent` VBUS to `vSafe0V`.
    pub(crate) const fn decide(vbus: UnattachedVbus) -> Self {
        match vbus {
            UnattachedVbus::Safe0v => Self { close_sink: true, arm_discharge: false },
            UnattachedVbus::PartnerLive => Self { close_sink: false, arm_discharge: false },
            UnattachedVbus::Residual | UnattachedVbus::SubPresentGap => {
                Self { close_sink: true, arm_discharge: true }
            }
        }
    }
}

impl<T: Typec> PortActor<'_, T> {
    /// Prepares `Unattached`: applies [`UnattachedPowerPlan::decide`] to the sink path and VBUS
    /// discharge, bounds armed discharge with `discharge_timeout`, and invokes
    /// [`Typec::on_unattached`].
    pub(crate) async fn prepare_unattached(self, detached_from: Option<PowerRole>) {
        if self.is_disabled() {
            return;
        }
        let detached = detached_from.is_some();
        let already_discharging = self.state().runner.hw.vbus_discharge.matches_confirmed(true);
        let safe0v = self.is_vbus_safe0v().await.unwrap_or_else(|e| {
            log::warn!("Failed to read vSafe0V; assuming VBUS is above vSafe0V: {e:?}");
            false
        });
        let vbus = if safe0v {
            UnattachedVbus::Safe0v
        } else if detached || already_discharging {
            UnattachedVbus::Residual
        } else if self.is_vbus_present().await.unwrap_or_else(|e| {
            log::warn!(
                "Failed to read VBUS presence on Unattached entry; assuming live VBUS: {e:?}"
            );
            true
        }) {
            UnattachedVbus::PartnerLive
        } else {
            UnattachedVbus::SubPresentGap
        };
        let plan = UnattachedPowerPlan::decide(vbus);
        if plan.close_sink
            && let Err(e) = self.set_vbus_sink(false).await
        {
            log::error!("Failed to close VBUS sink path on Unattached: {e:?}");
        }
        if let Err(e) = self.set_vbus_discharge(plan.arm_discharge).await {
            let action = if plan.arm_discharge { "arm" } else { "disarm" };
            log::warn!("Failed to {action} VBUS discharge: {e:?}");
        }
        match detached_from {
            Some(PowerRole::Source) => {
                self.state().runner.hw.vbus_seen_below_present = false;
            }
            Some(PowerRole::Sink) if self.is_vbus_discharge_armed() => {
                self.state().runner.hw.vbus_seen_below_present = true;
            }
            None if vbus == UnattachedVbus::SubPresentGap && self.is_vbus_discharge_armed() => {
                self.state().runner.hw.vbus_seen_below_present = true;
            }
            Some(PowerRole::Sink) | None => {}
        }
        if self.is_vbus_discharge_armed() {
            self.arm_timer(TimerKind::DischargeTimeout { allow_retry: true });
        }
        self.on_unattached().await;
    }

    /// Arms residual VBUS discharge when VBUS is above `vSafe0V` and bounds it with
    /// `discharge_timeout`. Returns whether discharge is now armed.
    pub(crate) async fn arm_residual_vbus_discharge(self, context: &str) -> bool {
        let safe0v = self.is_vbus_safe0v().await.unwrap_or_else(|e| {
            log::warn!("Failed to read vSafe0V {context}; assuming VBUS is above vSafe0V: {e:?}");
            false
        });
        if safe0v {
            return false;
        }
        if let Err(e) = self.set_vbus_discharge(true).await {
            log::warn!("Failed to arm VBUS discharge {context}: {e:?}");
        }
        if self.is_vbus_discharge_armed() {
            self.arm_timer(TimerKind::DischargeTimeout { allow_retry: true });
            return true;
        }
        false
    }

    /// Disarms VBUS discharge at `vSafe0V` and, if still in `Unattached`, invokes
    /// [`Typec::on_vsafe0v`] with any cancelled timer wake lease or the actor turn's lease.
    pub(crate) async fn complete_vsafe0v_discharge(self) -> Result<()> {
        let timer_lease = self.disarm_vbus_discharge().await?;
        if self.state().machine.allows_on_vsafe0v() {
            self.on_vsafe0v(timer_lease.as_ref().or(self.wake_lease)).await;
        }
        Ok(())
    }

    /// Disarms VBUS discharge, retrying once on a transient bus error, and returns any wake lease
    /// held by a cancelled `vSafe0V` debounce timer.
    pub(crate) async fn disarm_vbus_discharge(self) -> Result<Option<zx::EventPair>> {
        retry_once("disarm VBUS discharge", || self.set_vbus_discharge(false)).await
    }

    async fn disarm_discharge_on_live_vbus(self, context: &str) {
        if self.state().can_disarm_unattached_discharge()
            && let Err(e) = self.disarm_vbus_discharge().await
        {
            log::warn!("Failed to disarm VBUS discharge {context}: {e:?}");
        }
    }

    /// Updates sub-`vVBUSPresent` tracking while VBUS discharge is armed in `Unattached` without an
    /// active `vSafe0V` timer, and disarms discharge if 5 V partner VBUS is present after VBUS was
    /// observed below `vVBUSPresent` or after a prior disarm failed.
    pub(crate) async fn check_unattached_discharge_live_vbus(self, vbus_present: Option<bool>) {
        if !self.state().can_disarm_unattached_discharge() {
            return;
        }
        let present_res = match vbus_present {
            Some(present) => Ok(present),
            None => self.is_vbus_present().await,
        };
        match present_res {
            Ok(false) => {
                let mut state = self.state();
                if state.can_disarm_unattached_discharge() {
                    state.runner.hw.vbus_seen_below_present = true;
                }
            }
            Ok(true) => {
                let should_disarm = {
                    let state = self.state();
                    state.runner.hw.vbus_seen_below_present
                        || state.runner.hw.vbus_discharge.is_unconfirmed_enabled()
                };
                if should_disarm {
                    self.disarm_discharge_on_live_vbus("on live VBUS in Unattached").await;
                }
            }
            // Keep an already-bounded bleed running across a transient read fault rather than
            // stopping residual discharge on an unreadable bus.
            Err(e) => {
                log::warn!("Failed to read VBUS presence while discharging in Unattached: {e:?}");
            }
        }
    }

    /// Re-issues `write_vbus_discharge(true)` when VBUS bounces back above `vSafe0V` after a
    /// `vSafe0V` dip: TCPCI `ForceDischarge` controllers stop their discharge circuit below
    /// `VBUS_STOP_DISCHARGE_THRESHOLD` (or `vSafe0V`) without clearing
    /// `POWER_CONTROL.ForceDischarge` and require a new `0 -> 1` edge to resume discharging
    /// (TCPCI Rev 2.0 Ver 1.3 §4.4.18.2, §4.4.5.4.3).
    ///
    /// If `is_vbus_present()` sees 5 V partner VBUS after VBUS already dipped to `vSafe0V`,
    /// disarms discharge in `Unattached` (so level-triggered discharge hardware also removes its
    /// load) and skips re-arming in `AttachWait.SRC`. An `is_vbus_present()` read error assumes
    /// live VBUS and skips re-arming so a Source plugging in concurrently is not clamped.
    pub(crate) async fn rearm_vbus_discharge_after_bounce(self) {
        if !self.state().can_disarm_vsafe0v() {
            return;
        }
        match self.is_vbus_present().await {
            Ok(false) => {
                self.state().runner.hw.vbus_seen_below_present = true;
            }
            Ok(true) => {
                self.disarm_discharge_on_live_vbus("on live VBUS after vSafe0V bounce").await;
                return;
            }
            Err(e) => {
                log::warn!(
                    "Failed to read VBUS presence on vSafe0V bounce; skipping discharge re-arm: {e:?}"
                );
                return;
            }
        }
        if !self.state().can_disarm_vsafe0v() {
            return;
        }
        // Edge-triggered `ForceDischarge` hardware stops below `vSafe0V`, so hardware discharge is
        // no longer confirmed active until `write_vbus_discharge(true)` runs again.
        self.state().runner.hw.vbus_discharge = PowerPathState::Enabled { confirmed: false };
        if let Err(e) = self.set_vbus_discharge(true).await {
            log::warn!("Failed to re-arm VBUS discharge after vSafe0V bounce: {e:?}");
        }
        if self.is_vbus_discharge_armed() {
            // Keep any existing `discharge_timer` lease without extending an unrelated turn wake
            // lease across a `vSafe0V` bounce if `discharge_timer` already expired.
            self.without_wake_lease().arm_timer(TimerKind::DischargeTimeout { allow_retry: true });
        }
    }

    /// Arms or disarms VBUS discharge through [`crate::PortController::write_vbus_discharge`] and
    /// records whether hardware discharge is active, invalidating the cached confirmation on error
    /// and returning any wake lease held by cancelled `vSafe0V` or discharge-timeout timers.
    pub(crate) async fn set_vbus_discharge(self, armed: bool) -> Result<Option<zx::EventPair>> {
        if self.state().runner.hw.vbus_discharge.matches_confirmed(armed) {
            return Ok(None);
        }
        let was_enabled = self.state().runner.hw.vbus_discharge.is_enabled();
        let now_armed = match self.write_vbus_discharge(armed).await {
            Ok(true) if !armed => {
                self.state().runner.hw.vbus_discharge =
                    PowerPathState::Enabled { confirmed: false };
                return Err(anyhow::anyhow!(
                    "write_vbus_discharge(false) reported VBUS discharge still armed"
                ));
            }
            Ok(now_armed) => armed && now_armed,
            Err(e) => {
                let mut state = self.state();
                // Mark failed writes from an enabled state as unconfirmed enabled for recovery;
                // keep `Unknown` when not previously enabled so `is_vbus_discharge_armed()` stays
                // false.
                state.runner.hw.vbus_discharge = state.runner.hw.vbus_discharge.after_write_error();
                return Err(e);
            }
        };
        let mut state = self.state();
        if !now_armed || !was_enabled {
            state.runner.hw.vbus_seen_below_present = false;
        }
        state.runner.hw.vbus_discharge = if now_armed {
            PowerPathState::Enabled { confirmed: true }
        } else {
            PowerPathState::Disabled
        };
        let lease = if !now_armed {
            let discharge_lease = state.runner.discharge_timer.take_wake_lease_and_clear();
            state.take_vsafe0v_lease().or(discharge_lease)
        } else {
            None
        };
        Ok(lease)
    }

    /// Opens or closes the sink path through [`crate::PortController::write_vbus_sink`] and records
    /// whether a controller-managed sink path is open, marking failed closes from an open state as
    /// unconfirmed enabled so `vbus_sink_enabled` stays `true` and neither direction is skipped on
    /// retry.
    pub(crate) async fn set_vbus_sink(self, enable: bool) -> Result<()> {
        if self.state().runner.hw.vbus_sink.matches_confirmed(enable) {
            return Ok(());
        }
        let now_enabled = match self.write_vbus_sink(enable).await {
            Ok(true) if !enable => {
                self.state().runner.hw.vbus_sink = PowerPathState::Enabled { confirmed: false };
                return Err(anyhow::anyhow!(
                    "write_vbus_sink(false) reported VBUS sink path still enabled"
                ));
            }
            Ok(now_enabled) => enable && now_enabled,
            Err(e) => {
                let mut state = self.state();
                state.runner.hw.vbus_sink = state.runner.hw.vbus_sink.after_write_error();
                return Err(e);
            }
        };
        self.state().runner.hw.vbus_sink = if now_enabled {
            PowerPathState::Enabled { confirmed: true }
        } else {
            PowerPathState::Disabled
        };
        Ok(())
    }
}
