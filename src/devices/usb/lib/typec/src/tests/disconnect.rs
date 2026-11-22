// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

/// Verifies that `Typec` debounces Sink and Source attach/disconnect, role switches, wake leases,
/// and Inspect properties.
#[fuchsia::test]
async fn test_sink_and_source_debounce() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
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
    });

    // 1. Start Sink attach held at debounce gate, then bounce to Open (`tPDDebounce` exit from
    // `AttachWait.SNK`) before attach debounce expires.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));
    assert!(tcpc.is_busy());

    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert!(tcpc.typec_state().is_unattached());

    // 2. Stable Sink attach with wake lease -> Attached.SNK.
    let (lease, _peer) = zx::EventPair::create();
    tcpc.set_cc(snk);
    tcpc.handle_cc_status(Some(lease)).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.attached_power_role(), Some(PowerRole::Sink));
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Normal));

    // 3. `Attached.SNK` ignores CC `Open` while VBUS stays present (R2.5 §4.5.2.2.5.2).
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert!(!tcpc.is_busy());
    assert!(tcpc.is_connected());
    tcpc.set_cc(snk);

    // 4. Role reversal from `Attached.SNK` to Source (`SrcRa` on CC1, `SrcRd` on CC2): VBUS loss
    // detaches `Attached.SNK` to `Unattached` (R2.5 §4.5.2.2.5.2), after which `CC_STATUS`
    // attaches `Attached.SRC`.
    tcpc.set_vbus_present(false);
    tcpc.set_cc(src);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert!(tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.attached_power_role(), Some(PowerRole::Source));
    assert_eq!(tcpc.accessory(), None);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));

    // Transient disconnect bounce in `Attached.SRC` that recovers during `tSRCDisconnect`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    tcpc.set_cc(src);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(!tcpc.is_busy());
    assert!(tcpc.is_connected());
    drop(gate);

    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        is_connected: true,
        last_updated_time_ns: AnyProperty,
        cc1_state: "SRC.Ra",
        cc2_state: "SRC.Rd",
        typec_state: "Attached.SRC",
        active_debounce_direction: "none",
        attach_count: 2u64,
        disconnect_count: 1u64,
        debounce_cancelled_count: 2u64,
        vbus_discharge_armed: false,
        vbus_sink_enabled: false,
        pending_timer: "none",
        rp_change_count: 0u64,
        sink_attach_failure_count: 0u64,
        source_attach_failure_count: 0u64,
    });

    // 5. Confirmed disconnect -> Unattached.
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.attached_power_role(), None);
    assert_eq!(
        *tcpc.events.lock(),
        vec![
            "attached:Sink:lease=true",
            "detached:lease=false",
            "attached:Source:lease=false",
            "detached:lease=false",
        ]
    );
}

/// Verifies that same-pin `Rp` updates keep `tCCDebounce` running, `Attached.SNK` preserves its
/// committed orientation across CC noise while VBUS stays present, and VBUS loss re-debounces on a
/// swapped pin or DTS `Rp` pair.
#[fuchsia::test]
async fn test_attached_sink_same_pin_and_pin_swap_debounce() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    let tcpc = FakeTcpc::new(&scope, open);

    // 1. Same-pin interrupt (`SnkDefault -> SnkPower3_0` on `CC1`) during `tCCDebounce` keeps the
    // existing attach debounce running; the rise commits after `tRpValueChange`.
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(snk_cc1_3a0);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.events.lock().is_empty());
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.cc_status(), snk_cc1_3a0);

    // Same-pin dynamic Rp update while attached as Sink emits `on_sink_rp_changed` once the new
    // level survives `tRpValueChange`.
    tcpc.events.lock().clear();
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Default:lease=false"]);
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(tcpc.cc_status(), snk_cc1);

    // 2. While VBUS stays present, noisy `Ra/Ra` or opposite-pin `snk_cc2` samples keep
    // `Attached.SNK` at its committed `Normal` orientation (R2.5 §4.5.2.2.5.1-2).
    let ra_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);
    tcpc.set_cc(ra_ra);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Normal));
    tcpc.set_cc(snk_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Normal));

    // 3. VBUS loss detaches `CC1` and re-debounces attach on `CC2` once VBUS returns.
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));

    // 4. Dynamic `Rp` change on an attached Sink Debug Accessory
    // (`SnkPower3_0/SnkDefault -> SnkPower1_5/SnkDefault`) emits `on_sink_rp_changed(Rp1_5A)`.
    let snk_dbg_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let snk_dbg_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::SnkDefault);
    tcpc.set_cc(snk_dbg_3a0);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.accessory(), Some(Accessory::DebugAccessorySnk));
    tcpc.events.lock().clear();
    tcpc.set_cc(snk_dbg_1a5);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Rp1_5A:lease=false"]);
}

/// Verifies that same-pin recovery cancels an uncommitted `Attached.SRC` disconnect debounce, and
/// that single-sample CC glitches in `Attached.SRC` are filtered without restarting the timer.
#[fuchsia::test(logging = false)]
async fn test_disconnect_debounce_error_recovery() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    // 1. Same-pin recovery during an uncommitted `Attached.SRC` disconnect debounce cancels the
    // timer and keeps `Attached.SRC` with the committed `cc_status`.
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.cc_status(), src_cc1);
    assert!(tcpc.events.lock().is_empty());
    drop(gate);
    tcpc.wait_for_debounce().await;
    tcpc.reset_to_disconnected();

    // 2. Single-sample CC glitches in `Attached.SRC` debounce for `tSRCDisconnect` without
    // resetting the timer on intermediate noise and stay attached when `src_cc2` recovers.
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    let timer_id = tcpc.port.state.lock().runner.timer.id().unwrap();
    // Noisy sample on `src_cc1` during disconnect window keeps the same timer running.
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.port.state.lock().runner.timer.id(), Some(timer_id));
    // Original `src_cc2` recovers before disconnect timer expires.
    tcpc.set_cc(src_cc2);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(tcpc.events.lock().is_empty());
}

/// Verifies that `Attached.SRC` disconnect bounce recovery on the same pin stays attached and
/// recovery on a different pin (`CC1 -> Open -> CC2`) detaches `CC1` before attaching `CC2`.
#[fuchsia::test]
async fn test_disconnect_debounce_recovery_and_pin_swap() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc1_ra = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRa);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    // 1. Disconnect bounce recovery (`SrcRd/Open -> Open -> SrcRd/SrcRa`) via interrupt cancels the
    // disconnect debounce and keeps the port connected in `Attached.SRC`.
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    tcpc.set_cc(src_cc1_ra);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert!(tcpc.events.lock().is_empty());
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.cc_status(), src_cc1_ra);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Normal));

    // 2. Disconnect bounce recovery (`SrcRd/Open -> Open -> SrcRd/Open`) detected at timer
    // re-sample (without an intermediate IRQ) keeps the port attached in `Attached.SRC`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(src_cc1);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(tcpc.events.lock().is_empty());

    // 3. Disconnect bounce where CC recovers on a different pin (`CC1 -> Open -> CC2`) at timer
    // re-sample detaches `CC1` and debounces attach on `CC2` once VBUS is at `vSafe0V`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(src_cc2);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));

    // 4. Disconnect bounce interrupted by a pin-swap IRQ (`CC2 -> Open -> CC1`) forces full
    // detach teardown before attaching on `CC1`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Normal));
}

/// Verifies that when an expired disconnect timer queues behind an in-flight same-pin recovery
/// `handle_cc_status`, cancelling the timer marks its token cancelled so the queued expiry cannot
/// detach the port even if CC bounces back to `Open`.
#[fuchsia::test]
fn test_cancelled_queued_disconnect_timer_is_discarded() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, src_cc1);

    let (gate, cc_hold, cc_started_rx) = {
        let mut setup_fut = pin!(async {
            tcpc.handle_cc_status(None).await.unwrap();
            tcpc.wait_for_debounce().await;
            assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
            tcpc.events.lock().clear();

            let gate = tcpc.port.debounce_gate().lock().await;
            tcpc.set_cc(open);
            tcpc.handle_cc_status(None).await.unwrap();
            assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));

            tcpc.set_cc(src_cc1);
            let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
            (gate, cc_hold, cc_started_rx)
        });
        let Poll::Ready(triple) = exec.run_until_stalled(&mut setup_fut) else {
            panic!("setup should attach, start disconnect debounce, and arm cc_read synchronously");
        };
        triple
    };

    let mut recover_fut = pin!(tcpc.handle_cc_status(None));
    assert!(exec.run_until_stalled(&mut recover_fut).is_pending());
    assert!(exec.run_until_stalled(&mut pin!(cc_started_rx)).is_ready());
    drop(gate);
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert!(tcpc.port.state.lock().runner.actor_running);
    assert_eq!(tcpc.port.state.lock().runner.queue.len(), 1);

    tcpc.set_cc(open);
    drop(cc_hold);
    assert_matches!(exec.run_until_stalled(&mut recover_fut), Poll::Ready(Ok(())));
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(tcpc.events.lock().is_empty(), "a cancelled disconnect timer must not detach");
}

/// Verifies that `Typec::on_unattached` fires on async transitions from `AttachWait.*` into
/// `TypecState::UnattachedSnk`.
#[fuchsia::test]
async fn test_on_unattached_hook_on_attach_aborts() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);

    // 1. `AttachWait.SRC` immediate abort on `SRC.Open` -> `on_unattached`.
    let before = tcpc.unattached_count.load(Ordering::SeqCst);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(tcpc.events.lock().is_empty());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 1);

    // 2. `AttachWait.SNK` exit after `tPDDebounce` on `SNK.Open` -> `on_unattached`.
    let before = tcpc.unattached_count.load(Ordering::SeqCst);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());
    assert!(tcpc.events.lock().is_empty());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 1);

    // 3. `AttachWait.SNK` abort when CC is sampled `Open` at the end of `tCCDebounce` without an
    // intermediate IRQ -> `on_unattached`.
    let before = tcpc.unattached_count.load(Ordering::SeqCst);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(open);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());
    assert!(tcpc.events.lock().is_empty());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 1);
}

/// Verifies that an active-pin swap (`Attached.SRC` CC1 -> CC2) and a Sink-to-Source role switch
/// pass through `Unattached` (`on_unattached`), and wake leases stay alive across `handle_cc_status`.
#[fuchsia::test]
async fn test_on_unattached_hook_on_role_switch_and_detach() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let tcpc = FakeTcpc::new(&scope, open);

    // 1a. Active CC pin swap (`src_cc1 -> src_cc2`) runs `on_partner_detached` and
    // `on_unattached`, then re-enters `AttachWait.SRC` from the disconnect sample.
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    tcpc.events.lock().clear();
    let before = tcpc.unattached_count.load(Ordering::SeqCst);

    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 1);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false", "attached:Source:lease=false"]);
    tcpc.events.lock().clear();

    // Detach Source, attach `snk_cc1`, then test Sink-to-Source role switch via VBUS loss.
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 2);
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.events.lock().clear();

    // 1b. Sink-to-Source role switch: VBUS loss detaches `Attached.SNK` to `Unattached`
    // (`on_unattached`), after which `src_cc2` enters `AttachWait.SRC -> Attached.SRC`.
    tcpc.set_vbus_present(false);
    tcpc.set_cc(src_cc2);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 3);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false", "attached:Source:lease=false"]);
    tcpc.events.lock().clear();

    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false"]);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before + 4);

    // 2. When `Ra/Ra` enters `AttachWait.SRC` (held without `tCCDebounce` timer), `wake_lease` is
    // held across `handle_cc_status` and dropped upon completion.
    let audio_tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO)
            .with_discharge_timeout(zx::BootDuration::INFINITE),
        CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa),
    );
    let (cc_hold, cc_started_rx) = audio_tcpc.cc_read.arm().await;
    let (lease, lease_peer) = zx::EventPair::create();
    let mut cc_fut = pin!(audio_tcpc.handle_cc_status(Some(lease)));
    poll_until_oneshot(cc_fut.as_mut(), cc_started_rx).await;
    assert_eq!(
        lease_peer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
        zx::WaitResult::TimedOut(zx::Signals::empty())
    );
    drop(cc_hold);
    cc_fut.await.unwrap();
    assert!(
        lease_peer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .is_ok()
    );
}

/// Verifies that the wake lease from a disconnect event reaches `on_partner_detached`, and
/// follows an immediate re-attach from the disconnect sample through `AttachWait` into
/// `on_partner_attached`.
#[fuchsia::test]
async fn test_disconnect_wake_lease_handoff() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());

    // Sink unplug (VBUS loss): the VBUS interrupt's lease is handed to `on_partner_detached`.
    tcpc.events.lock().clear();
    let (lease, _lease_peer) = zx::EventPair::create();
    tcpc.set_cc(open);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(Some(lease)).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=true"]);
    tcpc.set_vbus_present(true);

    // Source pin swap (`src_cc1 -> src_cc2`): the same lease is held through
    // `on_partner_detached` and `AttachWait.SRC` into `on_partner_attached`.
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    tcpc.events.lock().clear();
    let (lease, _lease_peer) = zx::EventPair::create();
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(Some(lease)).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=true", "attached:Source:lease=true"]);
}
