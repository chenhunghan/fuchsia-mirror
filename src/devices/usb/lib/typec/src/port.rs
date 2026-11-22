// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Type-C port state container ([`TypecPort`]), public state enums ([`TypecState`],
//! [`DebounceDirection`]), and debounce timing configuration.

use crate::cc::PowerRole;
use crate::runner::PortState;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use std::fmt;

#[cfg(test)]
use futures::lock::Mutex as AsyncMutex;
#[cfg(test)]
use std::sync::{Arc, OnceLock};

/// Default `tCCDebounce` (170ms within the 100ms - 200ms window; R2.5 Table 4-34).
pub(crate) const DEFAULT_T_CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(170);

/// Default `tPDDebounce` (20ms within the 10ms - 20ms window; R2.5 Table 4-34).
pub(crate) const DEFAULT_T_PD_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);

/// Default `tSRCDisconnect` (0ms of the 0ms - 20ms window, so a Source removes VBUS immediately on
/// `SRC.Open`; R2.5 Table 4-34).
pub(crate) const DEFAULT_T_SRC_DISCONNECT: zx::BootDuration = zx::BootDuration::ZERO;

/// Default `tRpValueChange` (15ms within the 10ms - 20ms window; R2.5 Table 4-34).
pub(crate) const DEFAULT_T_RP_VALUE_CHANGE: zx::BootDuration = zx::BootDuration::from_millis(15);

/// Default safety bound on a VBUS discharge that never reaches `vSafe0V`: 650ms, matching
/// `tVBUSOFF` and `tVsafe0V` (R2.5 Table 4-32).
pub(crate) const DEFAULT_DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);

/// Default post-`vSafe0V` debounce duration (`0ms`; configurable via
/// [`TypecPort::with_vsafe0v_debounce`]).
pub(crate) const DEFAULT_VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::ZERO;

/// Connection state of a Type-C port, named as in USB Type-C R2.5 §4.5.2.2.
///
/// New spec states may be added, so drivers should use the query methods
/// ([`Self::power_role`], [`Self::is_attached`], [`Self::is_unattached`],
/// [`Self::is_sourcing_vbus`]) rather than match on variants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypecState {
    /// `Unattached.SNK`: presenting `Rd` (static, or as the autonomous DRP toggle seed;
    /// controller-managed DRP toggling is also reported here while searching).
    #[default]
    UnattachedSnk,
    /// `AttachWait.SNK`: debouncing a Source's `Rp` (`tCCDebounce`) and waiting for VBUS. Also
    /// holds an `Rp/Rp` Debug Accessory when Sink Debug Accessory Mode is disabled. A Source whose
    /// attach failed is held the same way.
    AttachWaitSnk,
    /// `Attached.SNK`: attached to a Source as a power Sink.
    AttachedSnk,
    /// `DebugAccessory.SNK`: attached to a DTS Source (`Rp/Rp`) with Sink Debug Accessory Mode
    /// enabled; [`crate::TypecExt::accessory`] returns
    /// [`Some(crate::Accessory::DebugAccessorySnk)`].
    DebugAccessorySnk,
    /// `AttachWait.SRC`: debouncing a Sink's `Rd` (`tCCDebounce`) and waiting for `vSafe0V`. Also
    /// holds an unsupported `Rd/Rd` or `Ra/Ra` partner until its terminations change.
    /// A partner whose attach failed is held the same way.
    AttachWaitSrc,
    /// `Attached.SRC`: attached to a Sink (R2.5 §4.5.2.2.9).
    AttachedSrc,
    /// `Disabled`: CC terminations removed (`ROLE_CONTROL = FORCE_OPEN`).
    Disabled,
}

impl TypecState {
    /// Returns the spec name of the state (used in Inspect and logs).
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UnattachedSnk => "Unattached.SNK",
            Self::AttachWaitSnk => "AttachWait.SNK",
            Self::AttachedSnk => "Attached.SNK",
            Self::DebugAccessorySnk => "DebugAccessory.SNK",
            Self::AttachWaitSrc => "AttachWait.SRC",
            Self::AttachedSrc => "Attached.SRC",
            Self::Disabled => "Disabled",
        }
    }

    /// Returns `true` in an `Unattached` state.
    pub const fn is_unattached(self) -> bool {
        match self {
            Self::UnattachedSnk => true,
            Self::AttachWaitSnk
            | Self::AttachedSnk
            | Self::DebugAccessorySnk
            | Self::AttachWaitSrc
            | Self::AttachedSrc
            | Self::Disabled => false,
        }
    }

    /// Returns `true` once a partner is attached (`Attached.SNK`, `DebugAccessory.SNK`, or
    /// `Attached.SRC`).
    pub const fn is_attached(self) -> bool {
        self.power_role().is_some()
    }

    /// Returns `true` if the port is in a state that sources VBUS (`Attached.SRC`).
    pub const fn is_sourcing_vbus(self) -> bool {
        match self {
            Self::AttachedSrc => true,
            Self::UnattachedSnk
            | Self::AttachWaitSnk
            | Self::AttachedSnk
            | Self::DebugAccessorySnk
            | Self::AttachWaitSrc
            | Self::Disabled => false,
        }
    }

    /// Returns the attached [`PowerRole`], or `None` while unattached, disabled, or debouncing.
    pub const fn power_role(self) -> Option<PowerRole> {
        match self {
            Self::AttachedSnk | Self::DebugAccessorySnk => Some(PowerRole::Sink),
            Self::AttachedSrc => Some(PowerRole::Source),
            Self::UnattachedSnk | Self::AttachWaitSnk | Self::AttachWaitSrc | Self::Disabled => {
                None
            }
        }
    }
}

/// Direction of an active Type-C debounce operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DebounceDirection {
    /// Debouncing a Sink or Source attachment (`tCCDebounce`).
    Attach,
    /// Debouncing an `AttachWait.SNK` exit (`tPDDebounce`), a Source CC disconnect
    /// (`tSRCDisconnect`), or an in-flight detach callback.
    Disconnect,
}

impl DebounceDirection {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Disconnect => "disconnect",
        }
    }
}

/// Timing of a [`TypecPort`], named after the USB Type-C R2.5 Table 4-34 parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PortConfig {
    /// `tCCDebounce`, before `AttachWait.* -> Attached.*`.
    pub(crate) cc_debounce: zx::BootDuration,
    /// `tPDDebounce`, before `AttachWait.SNK` exits on `SNK.Open`.
    pub(crate) pd_debounce: zx::BootDuration,
    /// `tSRCDisconnect`, before `Attached.SRC` exits on `SRC.Open`.
    pub(crate) src_disconnect: zx::BootDuration,
    /// `tRpValueChange`, before reporting a new `Attached.SNK` `Rp` level.
    pub(crate) rp_value_change: zx::BootDuration,
    /// Post-`vSafe0V` debounce applied by [`crate::TypecExt::handle_vbus_change`] before
    /// re-verifying [`crate::PortController::is_vbus_safe0v`] and disarming VBUS discharge.
    pub(crate) vsafe0v_debounce: zx::BootDuration,
    /// Bound on a VBUS discharge that never reaches `vSafe0V` (`INFINITE` disables it).
    pub(crate) discharge_timeout: zx::BootDuration,
}

/// USB Type-C Port state container embedded in a driver implementing [`crate::Typec`].
///
/// Holds the synchronized [`crate::CcStatus`], [`TypecState`], debounce configuration, and
/// runner state; all state operations are accessed via [`crate::TypecExt`].
pub struct TypecPort {
    /// Scope on which the port actor and debounce timers are spawned.
    pub(crate) scope: fasync::ScopeHandle,
    pub(crate) config: PortConfig,
    pub(crate) state: Mutex<PortState>,
    #[cfg(test)]
    pub(crate) debounce_gate: OnceLock<Arc<AsyncMutex<()>>>,
}

impl fmt::Debug for TypecPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TypecPort");
        let last_updated_time = if let Some(state) = self.state.try_lock() {
            let typec_state = state.machine.typec_state();
            debug
                .field("cc_status", &state.machine.visible_cc_status())
                .field("typec_state", &typec_state)
                .field("attached_power_role", &typec_state.power_role())
                .field("is_connected", &typec_state.is_attached())
                .field("is_sourcing_vbus", &typec_state.is_sourcing_vbus())
                .field("vbus_discharge_armed", &state.runner.hw.vbus_discharge.is_enabled())
                .field("vbus_seen_below_present", &state.runner.hw.vbus_seen_below_present)
                .field("vbus_sink_enabled", &state.runner.hw.vbus_sink.is_enabled())
                .field("is_disabled", &state.machine.is_disabled())
                .field("sink_debug_accessory_mode", &state.machine.sink_debug_accessory_mode);
            Some(state.machine.telemetry.last_updated_time)
        } else {
            debug.field("state", &"<locked>");
            None
        };
        debug
            .field("cc_debounce", &self.config.cc_debounce)
            .field("pd_debounce", &self.config.pd_debounce)
            .field("src_disconnect", &self.config.src_disconnect)
            .field("rp_value_change", &self.config.rp_value_change)
            .field("vsafe0v_debounce", &self.config.vsafe0v_debounce)
            .field("discharge_timeout", &self.config.discharge_timeout);
        if let Some(last_updated_time) = last_updated_time {
            debug.field("last_updated_time", &last_updated_time);
        }
        debug.finish()
    }
}

impl TypecPort {
    /// Creates a new [`TypecPort`] with USB Type-C timing (`tCCDebounce` 170ms, `tPDDebounce`
    /// 20ms, `tSRCDisconnect` 0ms, `tRpValueChange` 15ms), a 650ms discharge bound, no `vSafe0V`
    /// debounce, and `DebugAccessory.SNK` disabled.
    ///
    /// Cancelling `scope` aborts in-flight runner tasks and timers without invoking detach
    /// callbacks (whereas closing `scope` blocks new actor spawns and timers while letting an
    /// in-flight actor finish), so drivers shutting down must disarm VBUS discharge, close or
    /// restore the sink path, and place hardware in a safe state directly.
    pub fn new(scope: fasync::ScopeHandle) -> Self {
        Self {
            scope,
            config: PortConfig {
                cc_debounce: DEFAULT_T_CC_DEBOUNCE,
                pd_debounce: DEFAULT_T_PD_DEBOUNCE,
                src_disconnect: DEFAULT_T_SRC_DISCONNECT,
                rp_value_change: DEFAULT_T_RP_VALUE_CHANGE,
                vsafe0v_debounce: DEFAULT_VSAFE0V_DEBOUNCE,
                discharge_timeout: DEFAULT_DISCHARGE_TIMEOUT,
            },
            state: Mutex::new(PortState::new()),
            #[cfg(test)]
            debounce_gate: OnceLock::new(),
        }
    }

    /// Returns the gate that holds debounce timers in flight while it is locked.
    #[cfg(test)]
    pub(crate) fn debounce_gate(&self) -> &Arc<AsyncMutex<()>> {
        self.debounce_gate.get_or_init(|| Arc::new(AsyncMutex::new(())))
    }

    /// Configures `tCCDebounce` (100ms - 200ms; defaults to 170ms).
    ///
    /// Out-of-range durations (such as `0ms`) are permitted for deterministic unit testing.
    pub const fn with_cc_debounce(mut self, t_cc_debounce: zx::BootDuration) -> Self {
        self.config.cc_debounce = t_cc_debounce;
        self
    }

    /// Configures `tPDDebounce` (10ms - 20ms; defaults to 20ms), used when `AttachWait.SNK` exits
    /// on `SNK.Open` (R2.5 §4.5.2.2.4.2).
    ///
    /// Out-of-range durations (such as `0ms`) are permitted for deterministic unit testing.
    pub const fn with_pd_debounce(mut self, t_pd_debounce: zx::BootDuration) -> Self {
        self.config.pd_debounce = t_pd_debounce;
        self
    }

    /// Configures `tSRCDisconnect` (0ms - 20ms; defaults to 0ms), used when detaching from
    /// `Attached.SRC` (R2.5 §4.5.2.2.9.2).
    ///
    /// Out-of-range durations (such as `> 20ms`) are permitted for deterministic unit testing.
    pub const fn with_src_disconnect(mut self, t_src_disconnect: zx::BootDuration) -> Self {
        self.config.src_disconnect = t_src_disconnect;
        self
    }

    /// Configures `tRpValueChange` (10ms - 20ms; defaults to 15ms), used before reporting a Sink
    /// `Rp` change via [`crate::Typec::on_sink_rp_changed`] (R2.5 §4.5.2.3).
    ///
    /// Out-of-range durations (such as `0ms`) are permitted for deterministic unit testing.
    pub const fn with_rp_value_change(mut self, t_rp_value_change: zx::BootDuration) -> Self {
        self.config.rp_value_change = t_rp_value_change;
        self
    }

    /// Configures a post-`vSafe0V` debounce duration (defaults to `0ms`) applied by
    /// [`crate::TypecExt::handle_vbus_change`] before re-verifying
    /// [`crate::PortController::is_vbus_safe0v`], disarming VBUS discharge, and (in `Unattached`)
    /// invoking [`crate::Typec::on_vsafe0v`].
    ///
    /// This is a noise filter for the `vSafe0V` comparator, not a Type-C timing parameter.
    pub const fn with_vsafe0v_debounce(mut self, vsafe0v_debounce: zx::BootDuration) -> Self {
        self.config.vsafe0v_debounce = vsafe0v_debounce;
        self
    }

    /// Configures how long VBUS discharge may run without reaching `vSafe0V` (defaults to 650ms,
    /// matching `tVBUSOFF` and `tVsafe0V` in R2.5 Table 4-32 as a safety bound). On expiry
    /// [`TypecPort`] stops discharge.
    ///
    /// `zx::BootDuration::INFINITE` disables the bound, which unit tests use to keep discharge
    /// armed without a real-time timer.
    pub const fn with_discharge_timeout(mut self, discharge_timeout: zx::BootDuration) -> Self {
        self.config.discharge_timeout = discharge_timeout;
        self
    }

    /// Enables or disables `DebugAccessory.SNK` for a DTS Source (`Rp/Rp`, e.g. a SuzyQ debug
    /// cable; disabled by default).
    pub fn with_sink_debug_accessory_mode(self, enabled: bool) -> Self {
        self.state.lock().machine.sink_debug_accessory_mode = enabled;
        self
    }
}
