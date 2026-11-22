// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

/// Verifies that Source attach requires `vSafe0V` qualification (`AttachWait.SRC -> Attached.SRC`),
/// releases wake leases on deferral, re-verifies `CC_STATUS` in `handle_vbus_change`, and recovers
/// from transient `is_vbus_safe0v()` read failures.
#[fuchsia::test(logging = false)]
async fn test_source_vsafe0v_qualification_and_cc_recheck() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let src_cc2_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);

    // 1. Source CC debouncing with `is_vbus_safe0v() == Ok(false)` stays in `AttachWait.SRC` and
    // releases the CC interrupt wake lease (`peer_closed`), then commits when `vSafe0V` arrives.
    let (cc_lease, cc_lease_peer) = zx::EventPair::create();
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(Some(cc_lease)).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert!(
        cc_lease_peer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .is_ok()
    );

    // Same-pin updates (`SrcRd -> SrcRa + SrcRd` on `CC2`) while waiting for `vSafe0V` keep
    // `AttachWait.SRC` in `WaitingForVbus`.
    tcpc.set_cc(src_cc2_ra);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(tcpc.events.lock().is_empty());

    // Spurious `handle_vbus_change` while `is_vbus_safe0v() == Ok(false)` remains in
    // `AttachWait.SRC`, then `vSafe0V == true` commits `Attached.SRC` with its wake lease.
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    let (vbus_lease, _vbus_peer) = zx::EventPair::create();
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(Some(vbus_lease)).await.unwrap();
    assert!(tcpc.is_connected());
    assert!(tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=true"]);
    tcpc.reset_to_disconnected();
    tcpc.events.lock().clear();

    // 2. If CC drops to `Open` while waiting for `vSafe0V`, `handle_vbus_change` re-reads
    // `CC_STATUS` and transitions to `Unattached` via `on_unattached()` instead of enabling VBUS.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.set_cc(open);
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);

    // 3. If CC swaps pins (`CC1 -> CC2`) while waiting for `vSafe0V`, `handle_vbus_change`
    // restarts `tCCDebounce` on `CC2` and then attaches `Attached.SRC`.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_cc(src_cc2);
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
    tcpc.reset_to_disconnected();

    // 4. A transient `is_vbus_safe0v()` read error at `tCCDebounce` expiry when VBUS is already at
    // `0 V` recovers via `on_unattached` + a one-shot recovery `CC_STATUS` recheck to
    // `Attached.SRC` without waiting for a non-existent `vSafe0V` edge; a persistent error
    // terminates cleanly in `Unattached`.
    tcpc.events.lock().clear();
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(src_cc1);
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    // Hold `tCCDebounce` so the one-shot error is consumed at its expiry.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_vsafe0v_err_once("transient extended_status read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert!(tcpc.is_connected());
    assert!(tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    tcpc.reset_to_disconnected();

    tcpc.set_vsafe0v_err("persistent extended_status read error");
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.cc_status(), CcStatus::default());
    tcpc.reset_to_disconnected();

    // 5. A `handle_vbus_change` that samples `Open` in `read_cc_status` while a new `src_cc1`
    // replug with `vSafe0V == false` queues behind it returns to `Unattached` first and leaves the
    // new `AttachWait.SRC` session waiting for `vSafe0V` without sourcing VBUS.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

    tcpc.set_cc(open);
    tcpc.set_vsafe0v(true);
    let (cc_hold, cc_rx) = tcpc.cc_read.arm().await;
    let mut stale_vbus = pin!(tcpc.handle_vbus_change(None));
    poll_until_oneshot(stale_vbus.as_mut(), cc_rx).await;

    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    drop(cc_hold);
    let (vbus_res, cc_res) = futures::join!(stale_vbus, tcpc.handle_cc_status(None));
    vbus_res.unwrap();
    cc_res.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
}

/// Verifies that `handle_vbus_change` propagates `is_vbus_safe0v` and `read_cc_status` errors in
/// `AttachWait.SRC`, and commits on subsequent `handle_vbus_change` calls after transient errors.
#[fuchsia::test(logging = false)]
async fn test_handle_vbus_change_read_errors_and_stale_discards() {
    let scope = fasync::Scope::new();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, src_cc1);

    tcpc.set_vsafe0v(false);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

    // 1. Persistent `is_vbus_safe0v` and `read_cc_status` errors return `Err` and keep
    // `AttachWait.SRC`.
    tcpc.set_vsafe0v_err("vsafe0v bus error");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

    tcpc.set_vsafe0v(true);
    tcpc.set_cc_err("cc bus error");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_cc(src_cc1);

    // 2. Transient `is_vbus_safe0v` and `read_cc_status` errors return `Err` to the caller,
    // and subsequent `handle_vbus_change` calls commit `Attached.SRC`.
    tcpc.events.lock().clear();
    tcpc.set_vsafe0v_err_once("transient vsafe0v bus error");
    let (lease, _peer) = zx::EventPair::create();
    assert!(tcpc.handle_vbus_change(Some(lease)).await.is_err());
    let (lease, _peer) = zx::EventPair::create();
    tcpc.handle_vbus_change(Some(lease)).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=true"]);

    tcpc.reset_to_disconnected();
    tcpc.set_vsafe0v(false);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

    tcpc.events.lock().clear();
    tcpc.set_vsafe0v(true);
    tcpc.set_cc_err_once("transient cc bus error");
    let (lease, _peer) = zx::EventPair::create();
    assert!(tcpc.handle_vbus_change(Some(lease)).await.is_err());
    let (lease, _peer) = zx::EventPair::create();
    tcpc.handle_vbus_change(Some(lease)).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=true"]);
}

/// Verifies that `on_partner_attached` errors roll back the attach and hold the partner in
/// `AttachWait.SNK` or `AttachWait.SRC` until its CC terminations change.
#[fuchsia::test(logging = false)]
async fn test_on_partner_attached_error_rollback() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, open);

    // 1. `on_partner_attached(Source)` error during `handle_vbus_change` propagates `Err` and holds
    // the partner in `AttachWait.SRC` until its CC terminations change; unplugging it then invokes
    // `on_unattached`.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    tcpc.set_vsafe0v(true);
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.source_attach_failure_count, 1);
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached);
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);

    // 2. `on_partner_attached` error after `tCCDebounce` rolls back the attach and holds the
    // partner, whose `Rp` is still present, in `AttachWait.SNK` without `on_unattached` (USB Type-C
    // R2.5 §4.5.2.2.4.2).
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, 0);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.sink_attach_failure_count, 1);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    tcpc.reset_to_disconnected();

    // 3. A same-pin `CC_STATUS` interrupt queued while `on_partner_attached` is failing leaves the
    // partner held without incrementing `attach_count` while in flight; it attaches once replugged.
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    tcpc.events.lock().clear();
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    attach_started_rx.await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, 0);

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1_3a0);
    let mut rp_fut = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut rp_fut).is_pending());
    drop(attach_hold);
    rp_fut.await.unwrap();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, 0);

    tcpc.fail_attach.store(false, Ordering::SeqCst);
    drop(gate);
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.reset_to_disconnected();

    // 4. A failed attach after an `Rp` rise during `tCCDebounce` holds the partner without arming
    // the `tRpValueChange` timer, so the attach is neither retried nor reported as an `Rp` change.
    tcpc.events.lock().clear();
    let failures_before = tcpc.port.state.lock().machine.telemetry.sink_attach_failure_count;
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(snk_cc1_3a0);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert!(!tcpc.is_busy());
    assert_eq!(
        tcpc.port.state.lock().machine.telemetry.sink_attach_failure_count,
        failures_before + 1
    );
    assert!(tcpc.events.lock().is_empty());
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    tcpc.reset_to_disconnected();
}

/// Verifies that `read_cc_status` errors after attach debounce fall back to `Unattached` and after
/// disconnect debounce detach `Attached.SRC` so VBUS is shut off.
#[fuchsia::test(logging = false)]
async fn test_debounce_bus_error_abort_and_disconnect() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, open);

    // 1. A persistent `read_cc_status` error at the end of `tCCDebounce` falls back to
    // `TypecState::UnattachedSnk` and invokes `on_unattached()` once without looping.
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc_err("i2c/spmi read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert!(!tcpc.is_busy());

    // 2. `Attached.SRC` detaches on `read_cc_status` errors at the end of disconnect debounce so
    // VBUS is shut off (`!is_sourcing_vbus()`).
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_sourcing_vbus());
    tcpc.events.lock().clear();
    let src_gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc_err("dead bus while sourcing vbus");
    drop(src_gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert!(!tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false"]);
}

/// Verifies that `allow_recovery = false` bounds attach-debounce read-error recovery to a single
/// retry: if the first `tCCDebounce` expiry fails `read_cc_status`, its recovery resample re-arms
/// `tCCDebounce` once, and a second expiry read error terminates in `Unattached.SNK` without
/// re-sampling a third time.
#[fuchsia::test(logging = false)]
fn test_attach_debounce_recovery_stops_after_one_retry() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new_with_port(TypecPort::new(scope.to_handle()), snk_cc1);
    let cc_debounce = fasync::MonotonicDuration::from_nanos(DEFAULT_T_CC_DEBOUNCE.into_nanos());

    let mut start_fut = pin!(tcpc.handle_cc_status(None));
    assert_matches!(exec.run_until_stalled(&mut start_fut), Poll::Ready(Ok(())));
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "cc_attach");

    // 1st `tCCDebounce` expiry fails `read_cc_status`, runs `on_unattached`, resamples `snk_cc1`,
    // and re-arms `tCCDebounce` with `allow_recovery = false`.
    tcpc.set_cc_err_once("first expiry transient bus error");
    exec.set_fake_time(exec.now() + cc_debounce);
    assert!(exec.wake_expired_timers());
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), 1);
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "cc_attach");

    // 2nd `tCCDebounce` expiry fails `read_cc_status` while `snk_cc1` is still on the wire;
    // `allow_recovery = false` stops recovery in `Unattached.SNK` with no third resample.
    tcpc.set_cc_err_once("second expiry transient bus error");
    exec.set_fake_time(exec.now() + cc_debounce);
    assert!(exec.wake_expired_timers());
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), 2);
}

/// Verifies that committed `on_partner_attached` and `on_partner_detached` callbacks serialize
/// concurrent actor commands, and that disabling suppresses post-detach `on_unattached()`.
#[fuchsia::test]
async fn test_committed_callback_serialization() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);

    // 1. Holding `on_partner_attached` after `commit_attached` commits `Attached.SNK` serializes
    // redundant `handle_cc_status` calls behind the callback.
    tcpc.events.lock().clear();
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    attach_started_rx.await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));
    let mut same_fut = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut same_fut).is_pending());
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));
    drop(attach_hold);
    same_fut.await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);

    // Switch to `Attached.SRC` so `CC_STATUS` `Open` triggers a background `tSRCDisconnect` detach.
    tcpc.set_cc(src_cc1);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.set_vbus_present(true);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    // 2. Holding `on_partner_detached` after the detach commits queues concurrent `Open` and
    // replug `handle_cc_status` calls behind the detach callback.
    tcpc.events.lock().clear();
    let (detach_hold, detach_started_rx) = tcpc.detach.arm().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    detach_started_rx.await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    let mut open_fut = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut open_fut).is_pending());

    tcpc.set_cc(src_cc1);
    let mut replug_fut = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut replug_fut).is_pending());
    drop(detach_hold);
    open_fut.await.unwrap();
    replug_fut.await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false", "attached:Source:lease=false"]);

    // 3. Disabling the port while `on_partner_detached` is running transitions to `Disabled` and
    // suppresses post-detach `on_unattached()`.
    tcpc.events.lock().clear();
    let before = tcpc.unattached_count.load(Ordering::SeqCst);
    let (detach_hold, detach_started_rx) = tcpc.detach.arm().await;
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(src_cc2);
    drop(gate);
    detach_started_rx.await.unwrap();
    tcpc.port.state.lock().set_disabled(true);
    tcpc.drp_toggling.store(false, Ordering::SeqCst);
    assert_eq!(tcpc.typec_state(), TypecState::Disabled);
    drop(detach_hold);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::Disabled);
    assert!(!tcpc.drp_toggling.load(Ordering::SeqCst));
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false"]);
}

/// Verifies that holding `on_partner_detached` during a pin-swap detach (`Attached.SRC`
/// `CC1 -> CC2`) serializes concurrent `handle_cc_status` calls behind the follow-up attach.
#[fuchsia::test]
async fn test_role_switch_detach_serialization() {
    let scope = fasync::Scope::new();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, src_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    tcpc.events.lock().clear();
    let (detach_hold, detach_started_rx) = tcpc.detach.arm().await;
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    detach_started_rx.await.unwrap();
    assert!(tcpc.is_busy());
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));

    let mut concurrent_irq = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut concurrent_irq).is_pending());
    drop(detach_hold);
    concurrent_irq.await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false", "attached:Source:lease=false"]);
}

/// Verifies that Sink attach requires VBUS presence qualification (`AttachWait.SNK ->
/// Attached.SNK`) through `is_vbus_present()`, and VBUS loss while attached detaches the Sink
/// partner.
#[fuchsia::test(logging = false)]
async fn test_sink_vbus_attach_qualification_and_vbus_loss_disconnect() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);

    let tcpc = FakeTcpc::new(&scope, open);

    // 1. Sink CC debouncing with `is_vbus_present() == Ok(false)` stays in `AttachWait.SNK`
    // (`WaitingForVbus`), releasing wake leases, then commits when VBUS presence arrives.
    let (cc_lease, cc_lease_peer) = zx::EventPair::create();
    tcpc.set_vbus_present(false);
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(Some(cc_lease)).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_connected());
    assert!(
        cc_lease_peer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .is_ok()
    );

    // Spurious `handle_vbus_change` while `is_vbus_present() == Ok(false)` remains in
    // `AttachWait.SNK`.
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);

    // VBUS presence arriving via `handle_vbus_change` commits `Attached.SNK`.
    let (vbus_lease, _vbus_peer) = zx::EventPair::create();
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(Some(vbus_lease)).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=true"]);
    tcpc.reset_to_disconnected();
    tcpc.events.lock().clear();

    // 2. Cold-boot attach with `is_vbus_present() == false` debounces `tCCDebounce` first
    // (R2.5 §4.5.2.2.4.2), then waits in `AttachWait.SNK` for VBUS.
    tcpc.set_vbus_present(false);
    tcpc.set_cc(snk_cc1);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.sync_initial_state(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "cc_attach");
    assert!(!tcpc.is_connected());
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    // 3. In `Attached.SNK`, VBUS loss while CC is still asserted detaches immediately to
    // `Unattached` (`on_unattached`), then re-enters `AttachWait.SNK` and waits for VBUS after
    // `tCCDebounce`.
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);

    // VBUS restored while still in `WaitingForVbus` re-attaches `Attached.SNK`.
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    // 4. In `Attached.SNK`, VBUS loss when CC has dropped to `Open` detaches and returns to
    // `Unattached` (`on_unattached`).
    tcpc.set_vbus_present(false);
    tcpc.set_cc(open);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 2);

    // 5. While waiting in `AttachWait.SNK` for VBUS (`WaitingForVbus`), a CC `Open` bounce enters
    // `tPDDebounce` (`DebounceDirection::Disconnect`) instead of dropping immediately to
    // `Unattached`.
    tcpc.set_cc(snk_cc1);
    tcpc.set_vbus_present(false);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());

    // 6. Sink Debug Accessory (`Rp/Rp`) is also gated on `is_vbus_present()` for attach and
    // detaches on VBUS loss.
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    tcpc.set_cc(snk_dbg);
    tcpc.set_vbus_present(false);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_connected());
    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(tcpc.accessory(), Some(Accessory::DebugAccessorySnk));
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.reset_to_disconnected();

    // 7. Read errors: persistent and transient `is_vbus_present()` errors in `handle_vbus_change`
    // return `Err`, and a subsequent `handle_vbus_change` call commits `Attached.SNK`; an error
    // while already in `Attached.SNK` propagates `Err` without detaching.
    tcpc.set_cc(snk_cc1);
    tcpc.set_vbus_present(false);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.set_vbus_present_err("persistent vbus read error");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    tcpc.set_vbus_present(true);
    tcpc.set_vbus_present_err_once("transient vbus read error");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    assert!(tcpc.handle_vbus_change(None).await.is_ok());
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    tcpc.set_vbus_present_err_once("transient vbus read error while attached");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
}

/// Verifies that VBUS rising during a cold-socket Sink attach debounce (CC attaches first, VBUS
/// follows) leaves the debounce running, so the attach completes without a cancelled debounce.
#[fuchsia::test]
async fn test_vbus_present_during_attach_debounce_keeps_debounce() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());
    tcpc.set_vbus_present(false);

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));

    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.active_debounce_direction(), Some(DebounceDirection::Attach));

    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    let telemetry = &tcpc.port.state.lock().machine.telemetry;
    assert_eq!(telemetry.attach_count, 1);
    assert_eq!(telemetry.debounce_cancelled_count, 0);
}

/// Verifies that Sink VBUS loss detaches the partner even when `CC_STATUS` cannot be read (R2.5
/// §4.5.2.2.5.2), and that the read error is still returned.
#[fuchsia::test(logging = false)]
async fn test_sink_vbus_loss_detaches_despite_cc_read_error() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

    tcpc.set_vbus_present(false);
    tcpc.set_cc_err_once("simulated CC_STATUS read failure");
    assert!(tcpc.handle_vbus_change(None).await.is_err());
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert!(tcpc.events.lock().iter().any(|e| e.starts_with("detached")));
    assert_eq!(tcpc.port.state.lock().machine.telemetry.disconnect_count, 1);
}

/// Verifies that when VBUS drops below `vSinkDisconnect` before CC opens during a physical unplug,
/// re-sampling fresh `CC_STATUS` after `on_partner_detached` and `on_unattached` avoids entering a
/// phantom `AttachWait.SNK` or invoking `on_unattached` twice.
#[fuchsia::test]
async fn test_sink_vbus_loss_resamples_fresh_cc_after_detach() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let searching = CcStatus::LOOKING4CONNECTION;
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, open);

    for post_detach_cc in [searching, open] {
        tcpc.set_vbus_present(true);
        tcpc.set_cc(snk_cc1);
        tcpc.handle_cc_status(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);

        let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
        let (detach_hold, detach_rx) = tcpc.detach.arm().await;
        tcpc.set_vbus_present(false);
        // `raw_cc` is still `snk_cc1` when `handle_vbus_change` samples pre-detach `CC_STATUS`,
        // then clears to `searching` or `open` while `on_partner_detached` runs.
        let mut vbus_fut = pin!(tcpc.handle_vbus_change(None));
        poll_until_oneshot(vbus_fut.as_mut(), detach_rx).await;
        tcpc.set_cc(post_detach_cc);
        drop(detach_hold);
        vbus_fut.await.unwrap();

        assert!(!tcpc.is_busy());
        assert!(!tcpc.is_connected());
        assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
        assert_eq!(tcpc.cc_status(), open);
        assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    }
}
