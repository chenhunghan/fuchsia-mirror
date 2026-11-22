// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

/// Verifies that Inspect records the `Rp` level the attached Source advertises only while attached
/// as a Sink (including `DebugAccessory.SNK`).
#[fuchsia::test]
async fn test_inspect_sink_rp_level() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

    tcpc.set_cc(snk_cc1_1a5);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp1_5A));
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        typec_state: "Attached.SNK",
        sink_rp_level: "1.5A",
    });

    tcpc.set_cc(snk_dbg);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        typec_state: "DebugAccessory.SNK",
        sink_rp_level: "3.0A",
    });

    tcpc.set_cc(src_cc1);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.sink_rp_level(), None);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        is_connected: true,
        last_updated_time_ns: AnyProperty,
        cc1_state: "SRC.Rd",
        cc2_state: "Open",
        typec_state: "Attached.SRC",
        active_debounce_direction: "none",
        attach_count: 3u64,
        disconnect_count: 2u64,
        debounce_cancelled_count: 0u64,
        vbus_discharge_armed: false,
        vbus_sink_enabled: false,
        pending_timer: "none",
        rp_change_count: 0u64,
        sink_attach_failure_count: 0u64,
        source_attach_failure_count: 0u64,
    });
    // `contains` tolerates the other keys, so check separately that a Source session records no
    // `Rp` level.
    let hierarchy = fuchsia_inspect::reader::read(&inspector).await.expect("read Inspect");
    assert!(hierarchy.get_property("sink_rp_level").is_none());
}

/// Verifies that Inspect reports a Sink session's open sink path, its committed `Rp` level, a
/// pending `tRpValueChange` debounce, and the reported `Rp` change.
#[fuchsia::test]
async fn test_inspect_sink_session() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());
    tcpc.set_cc(CcStatus::new(CcPinState::SnkDefault, CcPinState::Open));
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open));
    tcpc.handle_cc_status(None).await.unwrap();
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        typec_state: "Attached.SNK",
        sink_rp_level: "Default",
        vbus_sink_enabled: true,
        pending_timer: "rp_value_change",
        rp_change_count: 0u64,
    });

    drop(gate);
    tcpc.wait_for_debounce().await;
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        sink_rp_level: "3.0A",
        pending_timer: "none",
        rp_change_count: 1u64,
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
    assert_eq!(default_port.config.vsafe0v_debounce, DEFAULT_VSAFE0V_DEBOUNCE);
    assert_eq!(default_port.config.discharge_timeout, DEFAULT_DISCHARGE_TIMEOUT);
    assert_eq!(default_port.config.rp_value_change, DEFAULT_T_RP_VALUE_CHANGE);
    assert!(!default_port.state.lock().machine.sink_debug_accessory_mode);

    const CUSTOM_SRC_DISCONNECT_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(5);
    const CUSTOM_VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(15);
    const CUSTOM_DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(100);
    let custom_port = TypecPort::new(scope.to_handle())
        .with_cc_debounce(zx::BootDuration::ZERO)
        .with_pd_debounce(zx::BootDuration::ZERO)
        .with_src_disconnect(CUSTOM_SRC_DISCONNECT_DEBOUNCE)
        .with_vsafe0v_debounce(CUSTOM_VSAFE0V_DEBOUNCE)
        .with_discharge_timeout(CUSTOM_DISCHARGE_TIMEOUT)
        .with_rp_value_change(zx::BootDuration::ZERO)
        .with_sink_debug_accessory_mode(true);
    assert_eq!(custom_port.config.cc_debounce, zx::BootDuration::ZERO);
    assert_eq!(custom_port.config.pd_debounce, zx::BootDuration::ZERO);
    assert_eq!(custom_port.config.src_disconnect, CUSTOM_SRC_DISCONNECT_DEBOUNCE);
    assert_eq!(custom_port.config.vsafe0v_debounce, CUSTOM_VSAFE0V_DEBOUNCE);
    assert_eq!(custom_port.config.discharge_timeout, CUSTOM_DISCHARGE_TIMEOUT);
    assert_eq!(custom_port.config.rp_value_change, zx::BootDuration::ZERO);
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
        vbus_discharge_armed: false,
        vbus_sink_enabled: false,
        pending_timer: "none",
        rp_change_count: 0u64,
        sink_attach_failure_count: 0u64,
        source_attach_failure_count: 0u64,
        discharge_timeout_count: 0u64,
    });

    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.attached_power_role(), None);
    assert_eq!(tcpc.orientation(), None);
    assert_eq!(tcpc.accessory(), None);
    assert_eq!(tcpc.sink_rp_level(), None);
    assert!(!tcpc.is_vbus_safe0v().await.unwrap());
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

    let minimal = Arc::new(MinimalTcpc {
        port: TypecPort::new(scope.to_handle()).with_vsafe0v_debounce(zx::BootDuration::ZERO),
        cc: Mutex::new(snk_dbg),
        vsafe0v: AtomicBool::new(false),
    });
    assert_eq!(minimal.sample_attachable_role().await.unwrap(), None);
    assert!(!minimal.write_vbus_discharge(true).await.unwrap());
    assert!(!minimal.write_vbus_sink(true).await.unwrap());
    assert!(!minimal.is_vbus_discharge_armed());
    *minimal.cc.lock() = CcStatus::default();
    let (minimal_lease, minimal_lease_peer) = zx::EventPair::create();
    minimal.sync_initial_state(Some(minimal_lease)).await.unwrap();
    assert!(!minimal.is_vbus_discharge_armed());
    assert_eq!(minimal.port.state.lock().pending_timer(), "none");
    assert!(is_lease_closed(&minimal_lease_peer));
    minimal.vsafe0v.store(true, Ordering::SeqCst);
    minimal.handle_vbus_change(None).await.unwrap();
    minimal.wait_for_debounce().await;
    assert!(!minimal.is_vbus_discharge_armed());
    minimal.on_sink_rp_changed(RpValue::Default, None).await;
    minimal.on_unattached().await;
    minimal.on_vsafe0v(None).await;
    minimal.wait_for_debounce().await;
    assert!(!minimal.is_disabled());
    assert_eq!(minimal.typec_state(), TypecState::UnattachedSnk);
}

struct MinimalTcpc {
    port: TypecPort,
    cc: Mutex<CcStatus>,
    vsafe0v: AtomicBool,
}

impl PortController for MinimalTcpc {
    async fn read_cc_status(&self) -> Result<CcStatus> {
        Ok(*self.cc.lock())
    }
    async fn is_vbus_safe0v(&self) -> Result<bool> {
        Ok(self.vsafe0v.load(Ordering::SeqCst))
    }
    async fn is_vbus_present(&self) -> Result<bool> {
        Ok(false)
    }
}

impl Typec for MinimalTcpc {
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
