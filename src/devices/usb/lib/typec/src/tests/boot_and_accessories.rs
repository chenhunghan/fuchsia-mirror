// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

/// Verifies that cold-boot `sync_initial_state` refreshes `cc_status`, handles idempotent
/// already-attached calls, and re-arms `on_unattached()` on unplug.
#[fuchsia::test]
async fn test_boot_sync_initial_state() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let tcpc = FakeTcpc::new(&scope, snk_dbg);

    // Calling `sync_initial_state` refreshes `cc_status` from hardware so orientation and
    // Sink Debug Accessory detection are populated.
    tcpc.sync_initial_state(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(tcpc.cc_status(), snk_dbg);
    assert_eq!(tcpc.accessory(), Some(Accessory::DebugAccessorySnk));
    assert!(tcpc.port.state.lock().machine.telemetry.last_updated_time > zx::BootInstant::ZERO);

    // Calling `sync_initial_state` while already attached to the same partner is an
    // idempotent no-op and preserves the existing attachment without duplicate callbacks.
    tcpc.events.lock().clear();
    tcpc.sync_initial_state(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    assert!(tcpc.events.lock().is_empty());

    // Resetting to disconnected clears connection state and CC states.
    tcpc.reset_to_disconnected();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);

    // If the partner unplugs (`Open`, `Looking4Connection == 0`) before `sync_initial_state`
    // runs, `on_unattached()` re-arms DRP toggling.
    tcpc.set_cc(snk_dbg);
    assert_eq!(tcpc.sample_attachable_role().await.unwrap(), Some(PowerRole::Sink));
    assert!(!tcpc.drp_toggling.load(Ordering::SeqCst));
    tcpc.set_cc(open);
    let before_unplug_boot = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unplug_boot + 1);
    assert!(tcpc.drp_toggling.load(Ordering::SeqCst));

    // Calling `sync_initial_state` in `AttachWait.SNK` when CC has gone `Open` debounces
    // `tPDDebounce` exit to `Unattached` and invokes `on_unattached()`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_dbg);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.set_cc(open);
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.sync_initial_state(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert!(!tcpc.is_busy());
}

/// Verifies that cold-boot `sync_initial_state` defers Source attachment in `AttachWait.SRC`
/// until `vSafe0V`, qualifies an existing `WaitingForVbus` Sink or Source phase on a second
/// `sync_initial_state` once VBUS is ready, and supersedes active `tCCDebounce` timers.
#[fuchsia::test]
async fn test_boot_attach_vsafe0v_deferral_and_supersede() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, src_cc1);

    // When `is_vbus_safe0v() == false` at the end of `tCCDebounce`, the port waits in
    // `AttachWait.SRC` (`WaitingForVbus`, no timer) until `handle_vbus_change` observes `vSafe0V`.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    tcpc.events.lock().clear();
    tcpc.sync_initial_state(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert!(!tcpc.is_connected());
    assert!(tcpc.events.lock().is_empty());
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=false"]);

    // A second `sync_initial_state` while already in `AttachWait.SRC (WaitingForVbus)` or
    // `AttachWait.SNK (WaitingForVbus)` qualifies the attach directly once VBUS is ready.
    tcpc.reset_to_disconnected();
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    tcpc.sync_initial_state(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_vsafe0v(true);
    tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    tcpc.reset_to_disconnected();
    tcpc.set_vbus_present(false);
    tcpc.set_cc(snk_cc1);
    tcpc.sync_initial_state(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    tcpc.set_vbus_present(true);
    tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    tcpc.reset_to_disconnected();
    tcpc.set_cc(snk_dbg);

    // Calling `sync_initial_state` while an uncommitted `tCCDebounce` timer is running in
    // `AttachWait.SNK` supersedes the timer and commits `Attached.SNK` immediately.
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.is_busy());
    tcpc.sync_initial_state(None).await.unwrap();
    drop(gate);
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    assert!(!tcpc.is_busy());
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
}

/// Verifies that cold-boot `sync_initial_state` serializes with concurrent `handle_vbus_change`
/// calls, rolls back failed `on_partner_attached` callbacks, and ignores calls while disabled.
#[fuchsia::test(logging = false)]
async fn test_boot_attach_cancellation_guards() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let tcpc = FakeTcpc::new(&scope, snk_dbg);

    // A `handle_vbus_change` queued while `sync_initial_state` is in `on_partner_attached`
    // waits for the cold-boot attach to finish before detaching on the actor.
    tcpc.events.lock().clear();
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    let mut boot_fut = pin!(tcpc.sync_initial_state(None));
    poll_until_oneshot(boot_fut.as_mut(), attach_started_rx).await;
    tcpc.set_cc(open);
    tcpc.set_vbus_present(false);
    let mut unplug_fut = pin!(tcpc.handle_vbus_change(None));
    assert!(poll!(&mut unplug_fut).is_pending());
    drop(attach_hold);
    boot_fut.await.unwrap();
    unplug_fut.await.unwrap();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false", "detached:lease=false"]);
    tcpc.set_vbus_present(true);

    // If `on_partner_attached` fails during `sync_initial_state`, the attach rolls back
    // (`attach_count` is unchanged) and the partner, whose `Rp` is still present, is held in
    // `AttachWait.SNK` (USB Type-C R2.5 §4.5.2.2.4.2).
    let attach_count = tcpc.port.state.lock().machine.telemetry.attach_count;
    tcpc.set_cc(snk_dbg);
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    assert!(tcpc.sync_initial_state(None).await.is_err());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.cc_status(), snk_dbg);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, attach_count);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.sink_attach_failure_count, 1);
    tcpc.fail_attach.store(false, Ordering::SeqCst);

    // Calling `sync_initial_state` while `is_disabled()` is a no-op and leaves the port in
    // `Disabled`.
    tcpc.reset_to_disconnected();
    tcpc.port.state.lock().set_disabled(true);
    tcpc.set_cc(snk_dbg);
    tcpc.events.lock().clear();
    tcpc.sync_initial_state(None).await.unwrap();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::Disabled);
    assert!(tcpc.events.lock().is_empty());
    tcpc.port.state.lock().set_disabled(false);
}

/// Verifies that default attach, `AttachWait.SNK` exit, Sink VBUS-loss disconnect, and Source
/// disconnect debounce timer boundaries fire at exact fake-time instants.
#[fuchsia::test]
fn test_default_debounce_durations() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let one_ms = fasync::MonotonicDuration::from_millis(1);
    let attach_debounce = fasync::MonotonicDuration::from_nanos(DEFAULT_T_CC_DEBOUNCE.into_nanos());
    let snk_exit_debounce =
        fasync::MonotonicDuration::from_nanos(DEFAULT_T_PD_DEBOUNCE.into_nanos());

    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle()).with_discharge_timeout(zx::BootDuration::INFINITE),
        open,
    );
    assert!(tcpc.port.debounce_gate.get().is_none());

    // 1. Trigger Sink attach: 1ms before `DEFAULT_T_CC_DEBOUNCE` the port is still in
    // `AttachWait.SNK`; at `DEFAULT_T_CC_DEBOUNCE` it transitions to `Attached.SNK`.
    tcpc.set_cc(snk_cc1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_connected());

    exec.set_fake_time(exec.now() + attach_debounce - one_ms);
    assert!(!exec.wake_expired_timers());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_connected());

    exec.set_fake_time(exec.now() + one_ms);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    // 2. CC `Open` in `Attached.SNK` keeps the Sink attached while VBUS stays present; VBUS loss
    // detaches immediately (R2.5 §4.5.2.2.5.2), and `DEFAULT_T_PD_DEBOUNCE` (`tPDDebounce`) gates
    // exiting `AttachWait.SNK` when CC goes `Open` (§4.5.2.2.4.2).
    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.set_cc(open);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    tcpc.set_vbus_present(false);
    {
        let mut fut = pin!(tcpc.handle_vbus_change(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    tcpc.set_vbus_present(true);

    tcpc.set_cc(snk_cc1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.set_cc(open);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.port.state.lock().pending_timer(), "snk_exit");
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);

    exec.set_fake_time(exec.now() + snk_exit_debounce - one_ms);
    assert!(!exec.wake_expired_timers());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);

    exec.set_fake_time(exec.now() + one_ms);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);

    // 3. Attach Source (`Attached.SRC`) and verify `DEFAULT_T_SRC_DISCONNECT` = 0ms
    // detaches immediately without waiting `tPDDebounce`. Even at cold boot a Sink partner is
    // debounced for `tCCDebounce` before VBUS is sourced (R2.5 §4.5.2.2.8.2).
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(src_cc1);
    {
        let mut fut = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    exec.set_fake_time(exec.now() + attach_debounce);
    assert!(exec.wake_expired_timers());
    {
        let mut wait_fut = pin!(tcpc.wait_for_debounce());
        assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    tcpc.set_cc(open);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
}

/// Verifies that Sink Debug Accessory Mode is opt-in (`false` by default) and unsupported `Rd/Rd`
/// or `Ra/Ra` accessory terminations hold `AttachWait.SRC` without sourcing VBUS.
#[fuchsia::test]
async fn test_opt_in_sink_debug_and_unsupported_accessories() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_dbg = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRd);
    let audio_ra_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);

    let default_tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO)
            .with_discharge_timeout(zx::BootDuration::INFINITE),
        open,
    );
    default_tcpc.set_cc(snk_dbg);
    default_tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(default_tcpc.events.lock().is_empty());
    default_tcpc.reset_to_disconnected();
    default_tcpc.events.lock().clear();
    default_tcpc.set_cc(snk_dbg);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.wait_for_debounce().await;
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!default_tcpc.is_connected());
    assert_eq!(default_tcpc.accessory(), None);
    assert_eq!(default_tcpc.orientation(), None);
    assert!(default_tcpc.events.lock().is_empty());

    // Role change during `tCCDebounce` from `snk_cc1` to `audio_ra_ra` switches to and holds
    // `AttachWait.SRC`.
    default_tcpc.set_cc(open);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.wait_for_debounce().await;
    default_tcpc.events.lock().clear();
    let gate = default_tcpc.port.debounce_gate().lock().await;
    default_tcpc.set_cc(snk_cc1);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.set_cc(audio_ra_ra);
    drop(gate);
    default_tcpc.wait_for_debounce().await;
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(default_tcpc.events.lock().is_empty());
    default_tcpc.reset_to_disconnected();
    default_tcpc.events.lock().clear();

    // `AttachWait.SNK` exit (`tPDDebounce`) that re-samples `snk_cc1` at timer expiry keeps
    // `AttachWait.SNK` and resumes `tCCDebounce`.
    let gate = default_tcpc.port.debounce_gate().lock().await;
    default_tcpc.set_cc(snk_cc1);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.set_cc(open);
    default_tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(default_tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    default_tcpc.set_cc(snk_cc1);
    drop(gate);
    default_tcpc.wait_for_debounce().await;
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*default_tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
    default_tcpc.reset_to_disconnected();
    default_tcpc.events.lock().clear();
    default_tcpc.set_cc(snk_dbg);
    default_tcpc.handle_cc_status(None).await.unwrap();

    // `src_dbg` (`Rd/Rd`) and `audio_ra_ra` (`Ra/Ra`) hold `AttachWait.SRC` without sourcing VBUS.
    default_tcpc.set_cc(src_dbg);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.wait_for_debounce().await;
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSrc);
    default_tcpc.set_vsafe0v(true);
    default_tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!default_tcpc.is_connected());
    assert!(!default_tcpc.typec_state().is_sourcing_vbus());
    assert!(default_tcpc.events.lock().is_empty());

    default_tcpc.set_cc(open);
    default_tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(default_tcpc.typec_state(), TypecState::UnattachedSnk);

    default_tcpc.events.lock().clear();
    default_tcpc.set_cc(audio_ra_ra);
    default_tcpc.handle_cc_status(None).await.unwrap();
    default_tcpc.wait_for_debounce().await;
    assert_eq!(default_tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!default_tcpc.is_connected());
    assert!(!default_tcpc.typec_state().is_sourcing_vbus());
    assert!(default_tcpc.events.lock().is_empty());
}

/// Verifies that `CC_STATUS` read errors during `sync_initial_state` return `Err` in `Unattached`,
/// Sink VBUS read errors return `Err` while falling back to `tCCDebounce` in `AttachWait.SNK`
/// (recovering on transient failure and settling in `Unattached.SNK` on persistent failure), and a
/// `vSafe0V` read error surfaces at the end of `tCCDebounce` and re-detects a Source partner.
#[fuchsia::test(logging = false)]
async fn test_boot_attach_read_errors_fail_cleanly() {
    let scope = fasync::Scope::new();
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, src_cc2);

    tcpc.set_cc_err("cc read error");
    assert!(tcpc.sync_initial_state(None).await.is_err());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(!tcpc.typec_state().is_sourcing_vbus());

    // A transient Sink VBUS read error during `sync_initial_state` returns `Err` to the caller
    // while arming `tCCDebounce` with the caller's wake lease, recovering to `Attached.SNK`.
    tcpc.events.lock().clear();
    tcpc.set_cc(snk_cc1);
    tcpc.set_vbus_present(true);
    tcpc.set_vbus_present_err_once("transient vbus read error");
    let gate = tcpc.port.debounce_gate().lock().await;
    let (lease, _peer) = zx::EventPair::create();
    assert!(tcpc.sync_initial_state(Some(lease)).await.is_err());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "cc_attach");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=true"]);
    tcpc.reset_to_disconnected();

    // A persistent Sink VBUS read error returns `Err`, enters `AttachWait.SNK`, and settles in
    // `Unattached.SNK` once `tCCDebounce` recovery ends.
    tcpc.set_cc(snk_cc1);
    tcpc.set_vbus_present_err("persistent vbus read error");
    assert!(tcpc.sync_initial_state(None).await.is_err());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    tcpc.set_vbus_present(true);

    tcpc.set_cc(src_cc2);
    tcpc.set_vsafe0v(true);
    let gate = tcpc.port.debounce_gate().lock().await;
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_vsafe0v_err_once("vsafe0v read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
}

/// Verifies that dropping an `sync_initial_state` future while a committed attach callback
/// is suspended on `port.scope` still completes the attach.
#[fuchsia::test]
async fn test_sync_initial_state_future_drop_completes_attach() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    // 1. Dropping `sync_initial_state` mid-`on_partner_attached` completes on `port.scope`.
    tcpc.events.lock().clear();
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    {
        let attach_fut = pin!(tcpc.sync_initial_state(None));
        poll_until_oneshot(attach_fut, attach_started_rx).await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
        assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));
    }
    drop(attach_hold);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);

    // 2. A Source attach at cold boot still runs `tCCDebounce` (R2.5 §4.5.2.2.8.2), so
    // `sync_initial_state` returns in `AttachWait.SRC` and `on_partner_attached` runs from the
    // debounce timer on `port.scope`, sourcing VBUS.
    tcpc.reset_to_disconnected();
    tcpc.events.lock().clear();
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(src_cc2);
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    tcpc.sync_initial_state(None).await.unwrap();
    attach_started_rx.await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));
    drop(attach_hold);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert!(tcpc.typec_state().is_sourcing_vbus());
    assert!(tcpc.vbus_sourced.load(Ordering::SeqCst));
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=false"]);
}

/// Verifies that a cold-boot `sync_initial_state` while the TCPC is DRP-toggling still runs
/// `on_unattached`, and that a cold-boot Sink attach (which skips `AttachWait`) stops VBUS
/// discharge before `on_partner_attached`.
#[fuchsia::test]
async fn test_cold_boot_unattached_and_sink_attach_disarm_discharge() {
    let scope = fasync::Scope::new();

    let toggling = FakeTcpc::new(&scope, CcStatus::LOOKING4CONNECTION);
    toggling.sync_initial_state(None).await.unwrap();
    assert_eq!(toggling.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(toggling.unattached_count.load(Ordering::SeqCst), 1);

    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let unsynced = FakeTcpc::new(&scope, snk_cc1);
    unsynced.sync_initial_state(None).await.unwrap();
    assert_eq!(unsynced.typec_state(), TypecState::AttachedSnk);
    assert!(!unsynced.is_vbus_discharge_armed());
    assert_eq!(*unsynced.discharge_writes.lock(), vec![false]);

    let attached = FakeTcpc::new(&scope, CcStatus::default());
    attached.set_vsafe0v(false);
    attached.sync_initial_state(None).await.unwrap();
    assert!(attached.is_vbus_discharge_armed());
    attached.set_cc(snk_cc1);
    attached.sync_initial_state(None).await.unwrap();
    assert_eq!(attached.typec_state(), TypecState::AttachedSnk);
    assert!(!attached.is_vbus_discharge_armed());
    assert_eq!(*attached.discharge_writes.lock(), vec![true, false]);
}

/// Verifies that each debounce uses its own configured duration: `tCCDebounce` for attach,
/// `tPDDebounce` for `AttachWait.SNK` exit, `tSRCDisconnect` for a Source disconnect, and
/// `tRpValueChange` for a Sink `Rp` change.
#[fuchsia::test]
fn test_custom_debounce_durations_select_by_phase() {
    const ATTACH: zx::BootDuration = zx::BootDuration::from_millis(150);
    const SNK_EXIT: zx::BootDuration = zx::BootDuration::from_millis(15);
    const SOURCE_DISCONNECT: zx::BootDuration = zx::BootDuration::from_millis(10);
    const RP_CHANGE: zx::BootDuration = zx::BootDuration::from_millis(12);
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(ATTACH)
            .with_pd_debounce(SNK_EXIT)
            .with_src_disconnect(SOURCE_DISCONNECT)
            .with_rp_value_change(RP_CHANGE),
        open,
    );

    // Drives `cc` and checks the transition happens exactly `duration` later.
    let step = |exec: &mut fasync::TestExecutor,
                cc: CcStatus,
                duration: zx::BootDuration,
                before: TypecState,
                after| {
        tcpc.set_cc(cc);
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
        let one_ms = fasync::MonotonicDuration::from_millis(1);
        let duration = fasync::MonotonicDuration::from_nanos(duration.into_nanos());
        let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
        exec.set_fake_time(exec.now() + duration - one_ms);
        exec.wake_expired_timers();
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert!(exec.run_until_stalled(&mut idle).is_pending());
        assert_eq!(tcpc.typec_state(), before);
        exec.set_fake_time(exec.now() + one_ms);
        assert!(exec.wake_expired_timers());
        assert_matches!(exec.run_until_stalled(&mut idle), Poll::Ready(()));
        assert_eq!(tcpc.typec_state(), after);
    };

    // Enter `AttachWait.SNK` without letting `ATTACH` expire, then time `SNK_EXIT` (`tPDDebounce`).
    tcpc.set_cc(snk_cc1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    step(&mut exec, open, SNK_EXIT, TypecState::AttachWaitSnk, TypecState::UnattachedSnk);

    step(&mut exec, snk_cc1, ATTACH, TypecState::AttachWaitSnk, TypecState::AttachedSnk);
    step(&mut exec, snk_cc1_3a0, RP_CHANGE, TypecState::AttachedSnk, TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock().last().unwrap(), "sink_rp:Rp3_0A:lease=false");

    // Detach Sink via VBUS loss, then check `ATTACH` and `SOURCE_DISCONNECT` on `Attached.SRC`.
    tcpc.set_cc(open);
    tcpc.set_vbus_present(false);
    {
        let mut fut = pin!(tcpc.handle_vbus_change(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    tcpc.set_vbus_present(true);

    step(&mut exec, src_cc1, ATTACH, TypecState::AttachWaitSrc, TypecState::AttachedSrc);
    step(&mut exec, open, SOURCE_DISCONNECT, TypecState::AttachedSrc, TypecState::UnattachedSnk);
}
