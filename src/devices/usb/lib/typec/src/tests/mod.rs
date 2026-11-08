// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod cc;
mod spec;
mod state;

use crate::port::{DEFAULT_T_CC_DEBOUNCE, DEFAULT_T_PD_DEBOUNCE, DEFAULT_T_SRC_DISCONNECT};
use crate::state::{Event, Transition};
use crate::*;
use anyhow::Result;
use diagnostics_assertions::{AnyProperty, assert_data_tree};
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use std::sync::Arc;

/// A port controller fake.
struct FakeTcpc {
    port: TypecPort,
    cc: Mutex<Result<CcStatus, &'static str>>,
}

impl FakeTcpc {
    fn new(scope: &fasync::Scope, initial_cc: CcStatus) -> Arc<Self> {
        Arc::new(Self {
            port: TypecPort::new(scope.to_handle())
                .with_cc_debounce(zx::BootDuration::ZERO)
                .with_pd_debounce(zx::BootDuration::ZERO)
                .with_sink_debug_accessory_mode(true),
            cc: Mutex::new(Ok(initial_cc)),
        })
    }

    fn set_cc(&self, cc: CcStatus) {
        *self.cc.lock() = Ok(cc);
    }
}

impl PortController for FakeTcpc {
    async fn read_cc_status(&self) -> Result<CcStatus> {
        (*self.cc.lock()).map_err(|msg| anyhow::anyhow!("{msg}"))
    }

    async fn is_vbus_safe0v(&self) -> Result<bool> {
        Ok(true)
    }

    async fn is_vbus_present(&self) -> Result<bool> {
        Ok(true)
    }
}

impl Typec for FakeTcpc {
    fn typec(&self) -> &TypecPort {
        &self.port
    }

    async fn on_partner_attached(
        &self,
        _role: PowerRole,
        _wake_lease: Option<&zx::EventPair>,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_partner_detached(&self, _wake_lease: Option<&zx::EventPair>) {}
}

/// Verifies that Inspect records the `Rp` level the attached Source advertises only while attached
/// as a Sink (including `DebugAccessory.SNK`).
#[fuchsia::test]
async fn test_inspect_sink_rp_level() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

    {
        let mut state = tcpc.port.state.lock();
        assert_eq!(state.machine.transition(Event::CcChanged(snk_cc1_1a5)), Transition::Moved);
        assert_eq!(
            state.machine.transition(Event::DebounceExpired { cc: snk_cc1_1a5, vbus_ready: true }),
            Transition::Moved
        );
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp1_5A));
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        typec_state: "Attached.SNK",
        sink_rp_level: "1.5A",
    });

    {
        let mut state = tcpc.port.state.lock();
        assert_eq!(
            state.machine.transition(Event::VbusChanged { cc: snk_dbg, vbus_ready: false }),
            Transition::Moved
        );
        assert_eq!(state.machine.transition(Event::CcChanged(snk_dbg)), Transition::Moved);
        assert_eq!(
            state.machine.transition(Event::DebounceExpired { cc: snk_dbg, vbus_ready: true }),
            Transition::Moved
        );
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        typec_state: "DebugAccessory.SNK",
        sink_rp_level: "3.0A",
    });

    {
        let mut state = tcpc.port.state.lock();
        assert_eq!(
            state.machine.transition(Event::VbusChanged { cc: src_cc1, vbus_ready: false }),
            Transition::Moved
        );
        assert_eq!(state.machine.transition(Event::CcChanged(src_cc1)), Transition::Moved);
        assert_eq!(
            state.machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
            Transition::Moved
        );
    }
    assert_eq!(tcpc.sink_rp_level(), None);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: {
        is_connected: true,
        last_updated_time_ns: AnyProperty,
        cc1_state: "SRC.Rd",
        cc2_state: "Open",
        typec_state: "Attached.SRC",
        active_debounce_direction: "none",
        attach_count: 3u64,
        disconnect_count: 2u64,
        debounce_cancelled_count: 0u64,
    });
}

/// Verifies that `TypecPort` configures default debounce durations, read-only status queries, and
/// default `Typec` trait hooks.
#[fuchsia::test]
async fn test_port_config_and_minimal_trait_impl() {
    let scope = fasync::Scope::new();
    let default_port = TypecPort::new(scope.to_handle());
    assert_eq!(default_port.config.cc_debounce, DEFAULT_T_CC_DEBOUNCE);
    assert_eq!(default_port.config.pd_debounce, DEFAULT_T_PD_DEBOUNCE);
    assert_eq!(default_port.config.src_disconnect, DEFAULT_T_SRC_DISCONNECT);
    assert!(!default_port.state.lock().machine.sink_debug_accessory_mode);

    const CUSTOM_SRC_DISCONNECT_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(5);
    let custom_port = TypecPort::new(scope.to_handle())
        .with_cc_debounce(zx::BootDuration::ZERO)
        .with_pd_debounce(zx::BootDuration::ZERO)
        .with_src_disconnect(CUSTOM_SRC_DISCONNECT_DEBOUNCE)
        .with_sink_debug_accessory_mode(true);
    assert_eq!(custom_port.config.cc_debounce, zx::BootDuration::ZERO);
    assert_eq!(custom_port.config.pd_debounce, zx::BootDuration::ZERO);
    assert_eq!(custom_port.config.src_disconnect, CUSTOM_SRC_DISCONNECT_DEBOUNCE);
    assert!(custom_port.state.lock().machine.sink_debug_accessory_mode);
    assert!(format!("{default_port:?}").contains("TypecPort"));
    {
        // Format from another thread: a same-thread `try_lock` while holding the lock is reported
        // as a lock cycle when `fuchsia_sync` lock-cycle detection is enabled (debug builds).
        let _locked = default_port.state.lock();
        let formatted = std::thread::scope(|s| s.spawn(|| format!("{default_port:?}")).join());
        assert!(formatted.unwrap().contains("<locked>"));
    }

    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let searching = CcStatus::LOOKING4CONNECTION;
    let tcpc = FakeTcpc::new(&scope, snk_dbg);

    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: {
        is_connected: false,
        last_updated_time_ns: 0i64,
        cc1_state: "Open",
        cc2_state: "Open",
        typec_state: "Unattached.SNK",
        active_debounce_direction: "none",
        attach_count: 0u64,
        disconnect_count: 0u64,
        debounce_cancelled_count: 0u64,
    });

    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.attached_power_role(), None);
    assert_eq!(tcpc.orientation(), None);
    assert_eq!(tcpc.accessory(), None);
    assert_eq!(tcpc.sink_rp_level(), None);
    assert!(tcpc.is_vbus_safe0v().await.unwrap());
    assert!(tcpc.is_vbus_present().await.unwrap());
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.last_updated_time, zx::BootInstant::ZERO);
    assert_eq!(tcpc.sample_attachable_role().await.unwrap(), Some(PowerRole::Sink));
    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);

    tcpc.set_cc(CcStatus::new(CcPinState::SrcRd, CcPinState::Open));
    assert_eq!(tcpc.sample_attachable_role().await.unwrap(), Some(PowerRole::Source));
    tcpc.set_cc(searching);
    assert_eq!(tcpc.sample_attachable_role().await.unwrap(), None);

    struct MinimalTcpc(TypecPort, CcStatus);
    impl PortController for MinimalTcpc {
        async fn read_cc_status(&self) -> Result<CcStatus> {
            Ok(self.1)
        }
        async fn is_vbus_safe0v(&self) -> Result<bool> {
            Ok(true)
        }
        async fn is_vbus_present(&self) -> Result<bool> {
            Ok(true)
        }
    }
    impl Typec for MinimalTcpc {
        fn typec(&self) -> &TypecPort {
            &self.0
        }
        async fn on_partner_attached(
            &self,
            _role: PowerRole,
            _wake_lease: Option<&zx::EventPair>,
        ) -> Result<()> {
            Ok(())
        }
        async fn on_partner_detached(&self, _wake_lease: Option<&zx::EventPair>) {}
    }
    let minimal = Arc::new(MinimalTcpc(TypecPort::new(scope.to_handle()), snk_dbg));
    assert_eq!(minimal.sample_attachable_role().await.unwrap(), None);
    minimal.on_unattached().await;
    assert!(!minimal.is_disabled());
    assert_eq!(minimal.typec_state(), TypecState::UnattachedSnk);
}
