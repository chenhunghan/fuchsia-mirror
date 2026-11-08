// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hardware operations ([`PortController`]), port callbacks ([`Typec`]), and extension methods
//! ([`TypecExt`]) for [`TypecPort`].

use crate::cc::{Accessory, CcStatus, PowerRole, RpValue};
use crate::port::{DebounceDirection, TypecPort, TypecState};
use anyhow::Result;
use fidl_next_fuchsia_hardware_usb_connector as fconnector;
use log::debug;
use std::future::Future;

const INSPECTOR_PROP_IS_CONNECTED: &str = "is_connected";
const INSPECTOR_PROP_LAST_UPDATED_TIME_NS: &str = "last_updated_time_ns";
const INSPECTOR_PROP_CC1_STATE: &str = "cc1_state";
const INSPECTOR_PROP_CC2_STATE: &str = "cc2_state";
const INSPECTOR_PROP_TYPEC_STATE: &str = "typec_state";
const INSPECTOR_PROP_ACTIVE_DEBOUNCE_DIRECTION: &str = "active_debounce_direction";
const INSPECTOR_PROP_ATTACH_COUNT: &str = "attach_count";
const INSPECTOR_PROP_DISCONNECT_COUNT: &str = "disconnect_count";
const INSPECTOR_PROP_DEBOUNCE_CANCELLED_COUNT: &str = "debounce_cancelled_count";
const INSPECTOR_PROP_SINK_RP_LEVEL: &str = "sink_rp_level";

/// Hardware operations a port controller performs for [`TypecPort`], in USB Type-C terms.
pub trait PortController: Send + Sync + 'static {
    /// Reads the instantaneous CC line state and returns the decoded [`CcStatus`].
    fn read_cc_status(&self) -> impl Future<Output = Result<CcStatus>> + Send;

    /// Reads whether VBUS is at `vSafe0V` (below 0.8 V).
    fn is_vbus_safe0v(&self) -> impl Future<Output = Result<bool>> + Send;

    /// Reads whether VBUS is present for a Sink: `true` once 5 V Default VBUS (`vSafe5V`) is
    /// detected on rise (`AttachWait.SNK -> Attached.SNK`, USB Type-C R2.5 §4.5.2.2.4.2; e.g.
    /// `> 4.0 V` in TCPCI Rev 2.0 Ver 1.3 Table 4-23) and `false` once VBUS falls below
    /// `vSinkDisconnect` (`0.8 V`–`3.67 V`, R2.5 Table 4-3; `< 3.5 V` in TCPCI) for VBUS-loss
    /// disconnect (`Attached.SNK -> Unattached.SNK`, §4.5.2.2.5.2).
    fn is_vbus_present(&self) -> impl Future<Output = Result<bool>> + Send;
}

/// Port policy a driver implements to integrate [`TypecPort`]: the state container and the
/// callbacks the connection state machine invokes.
pub trait Typec: PortController {
    /// Returns a reference to the driver's [`TypecPort`] state container.
    fn typec(&self) -> &TypecPort;

    /// Invoked when the port returns to `Unattached`.
    fn on_unattached(&self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Invoked upon completing `tCCDebounce` and transitioning to `Attached.SNK`,
    /// `DebugAccessory.SNK` (distinguished via [`TypecExt::accessory`]), or `Attached.SRC`.
    fn on_partner_attached(
        &self,
        _role: PowerRole,
        _wake_lease: Option<&zx::EventPair>,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Invoked upon completing disconnect debounce and transitioning out of `Attached.*`.
    fn on_partner_detached(
        &self,
        _wake_lease: Option<&zx::EventPair>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Extension methods on [`Typec`] drivers providing lifecycle controls, state queries, and Inspect
/// telemetry export.
pub trait TypecExt: Typec {
    /// Returns the current [`TypecState`].
    fn typec_state(&self) -> TypecState {
        self.typec().state.lock().machine.typec_state()
    }

    /// Returns `true` if the port is attached (`Attached.SNK`, `DebugAccessory.SNK`, or
    /// `Attached.SRC`).
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

    /// Returns the `Rp` level the attached Source advertised when it attached; later `Rp` changes
    /// are not yet tracked. `None` unless attached as a Sink.
    fn sink_rp_level(&self) -> Option<RpValue> {
        self.typec().state.lock().machine.sink_rp_level()
    }

    /// Returns the most recent [`CcStatus`] sample processed by the state machine (`Open` while
    /// disabled).
    fn cc_status(&self) -> CcStatus {
        self.typec().state.lock().machine.visible_cc_status()
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
        let state = self.typec().state.lock();
        let machine = &state.machine;
        let typec_state = machine.typec_state();
        node.record_bool(INSPECTOR_PROP_IS_CONNECTED, typec_state.is_attached());
        node.record_int(
            INSPECTOR_PROP_LAST_UPDATED_TIME_NS,
            machine.telemetry.last_updated_time.into_nanos(),
        );
        let cc = machine.visible_cc_status();
        node.record_string(INSPECTOR_PROP_CC1_STATE, cc.cc1.as_str());
        node.record_string(INSPECTOR_PROP_CC2_STATE, cc.cc2.as_str());
        node.record_string(INSPECTOR_PROP_TYPEC_STATE, typec_state.as_str());
        let direction = state.active_direction().map_or("none", DebounceDirection::as_str);
        node.record_string(INSPECTOR_PROP_ACTIVE_DEBOUNCE_DIRECTION, direction);
        node.record_uint(INSPECTOR_PROP_ATTACH_COUNT, machine.telemetry.attach_count);
        node.record_uint(INSPECTOR_PROP_DISCONNECT_COUNT, machine.telemetry.disconnect_count);
        node.record_uint(
            INSPECTOR_PROP_DEBOUNCE_CANCELLED_COUNT,
            machine.telemetry.debounce_cancelled_count,
        );
        if let Some(rp) = machine.sink_rp_level() {
            node.record_string(INSPECTOR_PROP_SINK_RP_LEVEL, rp.as_str());
        }
    }

    /// Returns `true` if a debounce timer or async operation is currently in flight.
    fn is_busy(&self) -> bool {
        self.typec().state.lock().is_busy()
    }

    /// Returns the [`DebounceDirection`] of the currently active debounce task, if any.
    fn active_debounce_direction(&self) -> Option<DebounceDirection> {
        self.typec().state.lock().active_direction()
    }
}

impl<T: Typec> TypecExt for T {}
