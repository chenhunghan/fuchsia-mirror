// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Driver integration traits for [`TypecPort`], split into three roles:
//! - [`PortController`]: hardware mechanism (CC and VBUS register operations), kept separate from
//!   policy so register-layer crates can implement it independently of board wiring.
//! - [`Typec`]: board and product policy (embedding [`TypecPort`] and handling attach, Sink `Rp`,
//!   detach, unattached, and `vSafe0V` notifications).
//! - [`TypecExt`]: blanket-implemented entry points and queries callers invoke on the driver,
//!   kept separate from [`Typec`] so drivers cannot override state-machine invariants.

use crate::actor::{Action, CcSampleMode, run_command};
use crate::cc::{Accessory, CcStatus, PowerRole, RpValue};
use crate::port::{DebounceDirection, TypecPort, TypecState};
use anyhow::Result;
use fidl_next_fuchsia_hardware_usb_connector as fconnector;
use log::debug;
use std::future::Future;
use std::sync::Arc;

/// Hardware operations a port controller performs for [`TypecPort`], in USB Type-C terms.
///
/// [`TypecPort`] drives these methods sequentially on the port actor (plus read-only
/// [`TypecExt::sample_attachable_role`]) without holding the port lock. Implementations should
/// retry transient bus errors internally when their transport requires it.
pub trait PortController: Send + Sync + 'static {
    /// Reads the instantaneous CC line state and returns the decoded [`CcStatus`]. May be called
    /// concurrently with actor turns through [`TypecExt::sample_attachable_role`].
    fn read_cc_status(&self) -> impl Future<Output = Result<CcStatus>> + Send;

    /// Reads whether VBUS is at `vSafe0V` (below 0.8 V).
    fn is_vbus_safe0v(&self) -> impl Future<Output = Result<bool>> + Send;

    /// Reads whether VBUS is present for a Sink: `true` once 5 V Default VBUS (`vSafe5V`) is
    /// detected on rise (`AttachWait.SNK -> Attached.SNK`, USB Type-C R2.5 §4.5.2.2.4.2; e.g.
    /// `> 4.0 V` in TCPCI Rev 2.0 Ver 1.3 Table 4-23) and `false` once VBUS falls below
    /// `vSinkDisconnect` (`0.8 V`–`3.67 V`, R2.5 Table 4-3; `< 3.5 V` in TCPCI) for VBUS-loss
    /// disconnect (`Attached.SNK -> Unattached.SNK`, §4.5.2.2.5.2).
    fn is_vbus_present(&self) -> impl Future<Output = Result<bool>> + Send;

    /// Starts or stops applying a load that discharges VBUS toward `vSafe0V`, returning whether
    /// software-controlled discharge is now armed (`Ok(false)` when disarmed or when the hardware
    /// lacks a controllable discharge load, such as TCPCI `DEVICE_CAPABILITIES_1.ForceDischarge =
    /// 0`).
    ///
    /// [`TypecPort`] arms discharge when entering `Unattached` or `AttachWait.SRC` (USB Type-C
    /// R2.5 §4.5.2.2.8.1) with residual VBUS above `vSafe0V`, re-arms on a `vSafe0V` bounce, and
    /// disarms on `AttachWait.SNK`, at attach, and once `vSafe0V` is debounced (or on
    /// [`TypecPort::with_discharge_timeout`]). On edge-triggered hardware like TCPCI
    /// `POWER_CONTROL.ForceDischarge` (TCPCI Rev 2.0 Ver 1.3 §4.4.5.4.3, §4.4.18.2),
    /// implementations must always produce a `0 -> 1` edge when `armed = true` (clearing the bit
    /// first if already set). [`TypecPort`] caches the last confirmed state, so drivers must not
    /// reset controller power-path registers directly.
    fn write_vbus_discharge(&self, _armed: bool) -> impl Future<Output = Result<bool>> + Send {
        async { Ok(false) }
    }

    /// Opens or closes the path from VBUS to the system load (TCPCI `COMMAND.SinkVbus` /
    /// `DisableSinkVbus`), returning whether a controller-managed sink path is now open
    /// (`Ok(false)` when closed or when VBUS sinking is managed externally).
    ///
    /// [`TypecPort`] opens the path before [`Typec::on_partner_attached`] on `Attached.SNK` and
    /// closes it on detach, `AttachWait.SRC`, and `Unattached`. Controllers whose `SinkVbus` gates
    /// power-path FETs must keep a hardware input-current ceiling `<= 500 mA` (Default USB Power,
    /// USB Type-C R2.5 Table 4-20) until `on_partner_attached` configures the committed `Rp` limit.
    /// Because `DisableSinkVbus` (`0x44`) may also discharge VBUS to `vSafe0V` (TCPCI Rev 2.0 Ver
    /// 1.3 §4.4.8 Table 4-27), [`TypecPort`] skips closing across `AttachWait.SNK`, held Sink
    /// attaches, and non-detach `Unattached` entries on live partner VBUS until VBUS drops.
    /// [`TypecPort`] caches the last confirmed state; defaults to `Ok(false)` when managed
    /// externally.
    fn write_vbus_sink(&self, _enable: bool) -> impl Future<Output = Result<bool>> + Send {
        async { Ok(false) }
    }
}

/// Board and product policy a driver implements to integrate [`TypecPort`]: the state container
/// and the callbacks the connection state machine invokes on transitions.
///
/// Callbacks run sequentially on the port actor without holding the port lock and should complete
/// in bounded time so queued transitions and Spec response windows (such as `tSinkAdj`) are not
/// delayed. Sync [`TypecExt`] queries (which already reflect the candidate attachment during
/// [`Self::on_partner_attached`], rolling back to `AttachWait` on `Err`) and
/// [`TypecExt::sample_attachable_role`] are safe inside them; awaiting same-port actor commands
/// (`handle_*`, `sync_initial_state`) or [`TypecExt::wait_for_debounce`] inside a callback
/// (directly or via another task) deadlocks the actor; spawn an independent task if a follow-up
/// command is needed.
pub trait Typec: PortController {
    /// Returns a reference to the driver's [`TypecPort`] state container.
    fn typec(&self) -> &TypecPort;

    /// Invoked when the port returns to `Unattached`, after updating the sink path and VBUS
    /// discharge (see [`TypecExt::is_vbus_discharge_armed`]; attach-debounce read-error recovery
    /// leaves both unchanged so live partner VBUS is not disturbed).
    fn on_unattached(&self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Invoked on transitioning to `Attached.SNK`, `DebugAccessory.SNK` (distinguished via
    /// [`TypecExt::accessory`]), or `Attached.SRC`: after `tCCDebounce`, or immediately for a
    /// Source partner already attached with VBUS during [`TypecExt::sync_initial_state`]. For
    /// [`PowerRole::Sink`], read the initial committed `Rp` advertisement via
    /// [`TypecExt::sink_rp_level`]; later changes arrive via [`Self::on_sink_rp_changed`].
    ///
    /// On `Err`, the port holds the partner in `AttachWait` until its CC terminations change
    /// (including across VBUS power cycles while `Rp` stays asserted) and does not call
    /// [`Self::on_partner_detached`], so retry transient bus errors internally and undo any
    /// partial setup before returning `Err` for an unrecoverable fault. For [`PowerRole::Sink`],
    /// the sink path stays open while partner VBUS remains present (closing once VBUS drops or the
    /// partner detaches), so keep a safe hardware input-current ceiling.
    fn on_partner_attached(
        &self,
        _role: PowerRole,
        _wake_lease: Option<&zx::EventPair>,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Invoked when the partner Source changes its `Rp` current advertisement (`Default`,
    /// `Rp1_5A`, `Rp3_0A`) after attaching as a Sink (`Attached.SNK`, or `DebugAccessory.SNK`
    /// whose level comes from its `Rp` pair per USB Type-C R2.5 Table B-2), once the new level has
    /// been debounced for `tRpValueChange` (§4.5.2.3, §4.6.2.1). Read the attach-time level via
    /// [`TypecExt::sink_rp_level`] inside [`Self::on_partner_attached`]; because the new level is
    /// already committed when this runs, handle or retry any current-limit update failure
    /// internally.
    fn on_sink_rp_changed(
        &self,
        _rp: RpValue,
        _wake_lease: Option<&zx::EventPair>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Invoked upon completing disconnect debounce and transitioning out of `Attached.*`.
    fn on_partner_detached(
        &self,
        _wake_lease: Option<&zx::EventPair>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Informational notification invoked in `Unattached` once an armed VBUS discharge disarms at
    /// `vSafe0V` (after [`TypecPort::with_vsafe0v_debounce`], or when
    /// [`TypecPort::with_discharge_timeout`] samples `vSafe0V`).
    ///
    /// Not invoked when `Unattached` is entered already at `vSafe0V`, when
    /// [`PortController::write_vbus_discharge`] reports `Ok(false)` (unsupported), or when the
    /// discharge timeout stops discharge above `vSafe0V`, so do not gate connection detection solely
    /// on this hook.
    fn on_vsafe0v(&self, _wake_lease: Option<&zx::EventPair>) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Blanket-implemented driver API on [`Typec`] types, split into two execution modes:
/// - **Actor commands (`self: &Arc<Self>`):** serialized in FIFO order on `port.scope` so
///   callbacks never overlap and dropping a caller's future does not abort an in-flight
///   transition. Do not `await` these from inside [`Typec`] callbacks. Interrupt loops should
///   `.await` `handle_*` before acknowledging the hardware IRQ so bursts coalesce in the
///   controller's alert register rather than queuing in software.
/// - **Queries and Inspect (`&self`):** lock-only state snapshots (plus read-only
///   [`Self::sample_attachable_role`]), safe from any context including [`Typec`] callbacks.
pub trait TypecExt: Typec {
    /// Waits for any in-flight debounce timer or async operation to complete. Intended for
    /// deterministic synchronization in unit tests rather than production driver control flow.
    fn wait_for_debounce(self: &Arc<Self>) -> impl Future<Output = ()> + Send + use<Self>
    where
        Self: Sized,
    {
        let this = Arc::clone(self);
        async move {
            loop {
                let rx = this.typec().state.lock().register_idle_waiter();
                let Some(rx) = rx else {
                    break;
                };
                // Sender dropped without sending if the port scope closed while waiting.
                let _ = rx.await;
            }
        }
    }

    /// Processes a hardware `CC_STATUS` alert interrupt.
    fn handle_cc_status(
        self: &Arc<Self>,
        wake_lease: Option<zx::EventPair>,
    ) -> impl Future<Output = Result<()>> + Send + use<Self>
    where
        Self: Sized,
    {
        run_command(Arc::clone(self), wake_lease, Action::CcStatus(CcSampleMode::Alert))
    }

    /// Processes a hardware VBUS status change interrupt:
    /// - In `AttachWait.SRC`: qualifies Source attach once VBUS drops to 0 V (`vSafe0V`), or
    ///   debounces `vSafe0V` to disarm residual VBUS discharge while `tCCDebounce` or a hold is
    ///   active.
    /// - In `AttachWait.SNK`: qualifies Sink attach once [`PortController::is_vbus_present`]
    ///   reports VBUS, or closes a held Sink's open sink path once VBUS drops.
    /// - In `Attached.SNK` / `DebugAccessory.SNK`: immediately detaches the port if VBUS falls
    ///   below `vSinkDisconnect` (returning any `CC_STATUS` read error after completing the
    ///   detach).
    /// - In `Unattached`: closes an open sink path preserved on live VBUS once VBUS drops, disarms
    ///   VBUS discharge if live external VBUS arrives after VBUS was already below `vVBUSPresent`
    ///   (or after a failed disarm), and if residual VBUS discharge is armed at `vSafe0V`, starts a
    ///   [`TypecPort::with_vsafe0v_debounce`] timer that re-verifies `vSafe0V`, stops VBUS
    ///   discharge, and invokes [`Typec::on_vsafe0v`].
    fn handle_vbus_change(
        self: &Arc<Self>,
        wake_lease: Option<zx::EventPair>,
    ) -> impl Future<Output = Result<()>> + Send + use<Self>
    where
        Self: Sized,
    {
        run_command(Arc::clone(self), wake_lease, Action::VbusChange)
    }

    /// Synchronizes port state once during driver startup with initial hardware `CC_STATUS`, when a
    /// partner may already be attached (e.g. at cold boot). Commits `Attached.SNK` /
    /// `DebugAccessory.SNK` from `Unattached` or a pending `AttachWait.SNK` without `tCCDebounce`
    /// when a VBUS-supplying Source partner is present (otherwise debouncing `tCCDebounce`); a Sink
    /// partner still gets `tCCDebounce` (R2.5 §4.5.2.2.8.2); with no partner, runs
    /// [`Typec::on_unattached`]. Callers should use [`Self::handle_cc_status`] and
    /// [`Self::handle_vbus_change`] at runtime so `tCCDebounce` is enforced.
    fn sync_initial_state(
        self: &Arc<Self>,
        wake_lease: Option<zx::EventPair>,
    ) -> impl Future<Output = Result<()>> + Send + use<Self>
    where
        Self: Sized,
    {
        run_command(Arc::clone(self), wake_lease, Action::CcStatus(CcSampleMode::InitialSync))
    }

    /// Returns the current [`TypecState`], including an in-flight [`Typec::on_partner_attached`]
    /// candidate (rolling back to `AttachWait` on `Err`).
    fn typec_state(&self) -> TypecState {
        self.typec().state.lock().machine.typec_state()
    }

    /// Returns `true` if the port is attached (`Attached.SNK`, `DebugAccessory.SNK`, or
    /// `Attached.SRC`), including an in-flight [`Typec::on_partner_attached`] candidate (rolling
    /// back to `AttachWait` on `Err`).
    fn is_connected(&self) -> bool {
        self.typec_state().is_attached()
    }

    /// Returns the attached [`PowerRole`] if currently in `Attached.SNK`, `DebugAccessory.SNK`, or
    /// `Attached.SRC`.
    fn attached_power_role(&self) -> Option<PowerRole> {
        self.typec_state().power_role()
    }

    /// Resolves the Type-C cable plug [`fconnector::Orientation`], or `None` when unattached,
    /// disabled, or unoriented.
    fn orientation(&self) -> Option<fconnector::Orientation> {
        self.typec().state.lock().machine.orientation()
    }

    /// Returns the active [`Accessory`] mode, or `None` when disabled or not in accessory mode.
    fn accessory(&self) -> Option<Accessory> {
        self.typec().state.lock().machine.accessory()
    }

    /// Returns the `Rp` level the attached Source advertises, as last reported through
    /// [`Typec::on_sink_rp_changed`] or committed at attach; `None` unless attached as a Sink.
    fn sink_rp_level(&self) -> Option<RpValue> {
        self.typec().state.lock().machine.sink_rp_level()
    }

    /// Returns the most recent [`CcStatus`] sample processed by the state machine (`Open` while
    /// disabled).
    fn cc_status(&self) -> CcStatus {
        self.typec().state.lock().machine.visible_cc_status()
    }

    /// Returns `true` if VBUS discharge is armed, or if disarming through
    /// [`PortController::write_vbus_discharge`] failed.
    fn is_vbus_discharge_armed(&self) -> bool {
        self.typec().state.lock().runner.hw.vbus_discharge.is_enabled()
    }

    /// Returns `true` if the port's CC terminations are currently disabled.
    fn is_disabled(&self) -> bool {
        self.typec().state.lock().machine.is_disabled()
    }

    /// Read-only hardware query that samples `CC_STATUS` via [`PortController::read_cc_status`] and
    /// returns the instantaneous attachable [`PowerRole`] without mutating [`TypecPort`] state.
    fn sample_attachable_role(&self) -> impl Future<Output = Result<Option<PowerRole>>> + Send {
        async {
            if self.is_disabled() {
                return Ok(None);
            }
            let read_res = self.read_cc_status().await;
            let (cc_status, role) = {
                let state = self.typec().state.lock();
                if state.machine.is_disabled() {
                    return Ok(None);
                }
                let cc_status = read_res?;
                (cc_status, state.machine.attachable_role(cc_status))
            };
            if cc_status.looking4connection {
                debug!(
                    "Type-C port is searching for a connection (Looking4Connection); treating as disconnected"
                );
            }
            Ok(role)
        }
    }

    /// Records the port's Type-C state and debounce telemetry properties onto `node`.
    fn record_typec_inspect(&self, node: &fuchsia_inspect::Node) {
        crate::inspect::record(self.typec(), node);
    }

    /// Returns `true` if a debounce timer or async operation is currently in flight.
    fn is_busy(&self) -> bool {
        self.typec().state.lock().is_busy()
    }

    /// Returns the [`DebounceDirection`] of an active attach or disconnect debounce, if any
    /// (`None` during a Sink `tRpValueChange` window, while [`Self::is_busy`] is `true`).
    fn active_debounce_direction(&self) -> Option<DebounceDirection> {
        self.typec().state.lock().active_direction()
    }
}

impl<T: Typec> TypecExt for T {}
