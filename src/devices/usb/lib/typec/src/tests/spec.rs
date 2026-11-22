// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for behavior the USB Type-C (R2.5) spec requires.

use super::*;
use CcPinState::{SnkDefault, SnkPower1_5, SnkPower3_0};

const SNK_CC1: CcStatus = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
const SNK_CC1_3A0: CcStatus = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
const SRC_CC1: CcStatus = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
const OPEN: CcStatus = CcStatus::new(CcPinState::Open, CcPinState::Open);
/// A DTS Source advertising Default USB Power (R2.5 Table B-2: `Rp` for 3 A and 1.5 A).
const DTS_DEFAULT: CcStatus = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkPower1_5);
/// Creates a zero-debounce fake entered into `Unattached`.
async fn unattached(scope: &fasync::Scope) -> Arc<FakeTcpc> {
    let tcpc = FakeTcpc::new(scope, OPEN);
    tcpc.sync_initial_state(None).await.unwrap();
    tcpc
}

/// Drives a CC change to `cc` and waits for any debounce to finish.
async fn settle(tcpc: &Arc<FakeTcpc>, cc: CcStatus) {
    tcpc.set_cc(cc);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
}

/// Verifies that a DTS Source's current is decoded from its pair of `Rp` values (R2.5 §B.2.4.2.1
/// Table B-2), with any other pair meaning Default USB Power (§B.2.4.2.2.1).
///
/// Spec obligation: `typec:B.2.4.2.1:dts-current-table-b2`.
#[fuchsia::test]
fn test_dts_source_current_per_table_b2() {
    for (cc1, cc2, expected) in [
        (SnkPower3_0, SnkPower1_5, RpValue::Default),
        (SnkPower1_5, SnkPower3_0, RpValue::Default),
        (SnkPower1_5, SnkDefault, RpValue::Rp1_5A),
        (SnkPower3_0, SnkDefault, RpValue::Rp3_0A),
        (SnkDefault, SnkPower3_0, RpValue::Rp3_0A),
        (SnkPower3_0, SnkPower3_0, RpValue::Default),
    ] {
        assert_eq!(CcStatus::new(cc1, cc2).sink_rp_level(), Some(expected), "{cc1:?}/{cc2:?}");
    }
    assert_eq!(SNK_CC1_3A0.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert_eq!(SRC_CC1.sink_rp_level(), None);
}

/// Verifies that both `Attached.SNK` and `DebugAccessory.SNK` survive CC pins dropping their `Rp`
/// while VBUS stays present and exit only on VBUS loss (R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2).
///
/// Spec obligations: `typec:4.5.2.2.5.2:cc-monitor-optional`, `typec:4.5.2.2.19.2:debugaccessory-snk-exit-vbus`.
#[fuchsia::test]
async fn test_debug_accessory_exits_only_on_vbus_loss() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    settle(&tcpc, SNK_CC1).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.events.lock().clear();

    settle(&tcpc, OPEN).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert!(tcpc.events.lock().is_empty());
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());
    tcpc.set_vbus_present(true);

    settle(&tcpc, DTS_DEFAULT).await;
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    tcpc.events.lock().clear();

    settle(&tcpc, OPEN).await;
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    settle(&tcpc, CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open)).await;
    assert_eq!(tcpc.typec_state(), TypecState::DebugAccessorySnk);
    assert!(tcpc.events.lock().is_empty());

    // VBUS loss detaches to `Unattached.SNK`; the `Rp` still on CC1 then re-enters
    // `AttachWait.SNK`.
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(tcpc.events.lock().iter().filter(|e| e.starts_with("detached")).count(), 1);
}

/// Verifies that a failed Sink attach holds the partner in `AttachWait.SNK` (R2.5 §4.5.2.2.4.2)
/// without loading the VBUS it still supplies: neither a discharge nor `DisableSinkVbus`, which
/// also discharges (TCPCI Table 4-27), until the partner leaves.
#[fuchsia::test(logging = false)]
async fn test_failed_sink_attach_holds_partner_without_discharge() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    tcpc.discharge_writes.lock().clear();
    tcpc.sink_writes.lock().clear();
    tcpc.fail_attach.store(true, Ordering::SeqCst);

    settle(&tcpc, SNK_CC1).await;

    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert!(!tcpc.discharge_writes.lock().contains(&true));
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
    {
        let telemetry = &tcpc.port.state.lock().machine.telemetry;
        assert_eq!((telemetry.sink_attach_failure_count, telemetry.attach_count), (1, 0));
    }

    // Unplugging releases the held partner and closes the sink path.
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    settle(&tcpc, OPEN).await;
    assert!(tcpc.typec_state().is_unattached());
    assert_eq!(*tcpc.sink_writes.lock(), vec![true, false]);
}

/// Verifies that a failed `SinkVbus` write holds the partner in `AttachWait.SNK` without calling
/// `on_partner_attached`, increments `sink_attach_failure_count`, and does not open the sink
/// path.
#[fuchsia::test(logging = false)]
async fn test_failed_sink_vbus_write_holds_partner() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    tcpc.events.lock().clear();
    tcpc.sink_writes.lock().clear();
    tcpc.fail_sink_write.store(true, Ordering::SeqCst);

    settle(&tcpc, SNK_CC1).await;

    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.sink_writes.lock().is_empty());
    assert!(!tcpc.events.lock().iter().any(|e| e.starts_with("attached:")));
    let state = tcpc.port.state.lock();
    assert!(state.machine.is_partner_held());
    assert_eq!(
        (state.machine.telemetry.sink_attach_failure_count, state.machine.telemetry.attach_count),
        (1, 0)
    );
}

/// Verifies that an `Rp` level rise during `tCCDebounce` is reported only after it has been
/// stable for `tRpValueChange` (R2.5 §4.5.2.3.1.2), not at attach, and holds the caller's wake
/// lease across both `on_partner_attached` and `on_sink_rp_changed`.
///
/// Spec obligation: `typec:4.5.2.3:rp-change-trpvaluechange`.
#[fuchsia::test]
fn test_rp_change_during_cc_debounce_honors_rp_value_change() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(10);
    const RP_CHANGE: zx::BootDuration = zx::BootDuration::from_millis(20);
    const HALF_DEBOUNCE: fasync::MonotonicDuration = fasync::MonotonicDuration::from_millis(5);
    const BEFORE_RP_END: fasync::MonotonicDuration = fasync::MonotonicDuration::from_millis(19);
    const ONE_MS: fasync::MonotonicDuration = fasync::MonotonicDuration::from_millis(1);

    let mut exec = fasync::TestExecutor::new_with_fake_time();
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(CC_DEBOUNCE)
            .with_pd_debounce(zx::BootDuration::ZERO)
            .with_rp_value_change(RP_CHANGE)
            .with_sink_debug_accessory_mode(true),
        OPEN,
    );
    {
        let mut init = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut init), Poll::Ready(Ok(())));
    }
    tcpc.events.lock().clear();

    let (lease, observer) = zx::EventPair::create();
    tcpc.set_cc(SNK_CC1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(Some(lease)));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    exec.set_fake_time(exec.now() + HALF_DEBOUNCE);
    assert!(!exec.wake_expired_timers());
    tcpc.set_cc(SNK_CC1_3A0);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }

    // `tCCDebounce` commits `Default` and starts `tRpValueChange`, keeping `lease` open.
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    exec.set_fake_time(exec.now() + HALF_DEBOUNCE);
    assert!(exec.wake_expired_timers());
    {
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert!(exec.run_until_stalled(&mut idle).is_pending());
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=true"]);
    assert_eq!(
        observer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result(),
        Err(zx::Status::TIMED_OUT)
    );

    exec.set_fake_time(exec.now() + BEFORE_RP_END);
    assert!(!exec.wake_expired_timers());
    {
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert!(exec.run_until_stalled(&mut idle).is_pending());
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(
        observer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result(),
        Err(zx::Status::TIMED_OUT)
    );

    exec.set_fake_time(exec.now() + ONE_MS);
    assert!(exec.wake_expired_timers());
    {
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert_matches!(exec.run_until_stalled(&mut idle), Poll::Ready(()));
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=true", "sink_rp:Rp3_0A:lease=true"]);
    assert!(
        observer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .is_ok()
    );
}

/// Verifies that an `Rp` level drop during `tCCDebounce` attaches at the lower level without
/// reporting the stale higher one: a Sink starts in `PowerDefault.SNK` and never draws more than
/// the current advertisement (R2.5 §4.5.2.3, §4.5.2.3.2.1, §4.5.2.3.3.1).
#[fuchsia::test]
async fn test_rp_drop_during_cc_debounce_attaches_at_lower_level() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(SNK_CC1_3A0);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;

    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(tcpc.port.state.lock().machine.debounce_timer(), None);
    assert!(!tcpc.is_busy());
}

/// Verifies that entering `Unattached` syncs the sink path closed, and that a Sink attach opens
/// and a detach closes the sink path (TCPCI `SinkVbus` / `DisableSinkVbus`).
///
/// Spec obligation: `tcpci:4.4.8:look4connection-and-sinkvbus`.
#[fuchsia::test]
async fn test_sink_attach_opens_and_detach_closes_sink_path() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false]);

    settle(&tcpc, SNK_CC1).await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false, true]);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false, true, false]);
}

/// Verifies that `sink_rp_level` reports the committed `Rp` level only while attached as a Sink,
/// and a new level only once it has been stable for `tRpValueChange` (R2.5 §4.5.2.3).
#[fuchsia::test]
async fn test_sink_rp_level_reports_committed_level() {
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;
    assert_eq!(tcpc.sink_rp_level(), None);

    settle(&tcpc, SNK_CC1).await;
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(SNK_CC1_3A0);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));

    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.sink_rp_level(), None);

    tcpc.set_vbus_present(true);
    settle(&tcpc, OPEN).await;
    settle(&tcpc, SRC_CC1).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.sink_rp_level(), None);
}

/// Verifies that an `Rp` fall commits within a single `tRpValueChange` window even when ground
/// offset under load makes CC chatter between `SNK.Default`, `SNK.Open`, and `SNK.Power1.5` faster
/// than `tRpValueChange` (R2.5 §4.5.2.3.2.2, §4.5.2.3.3.2, Table 4-32 `tSinkAdj`), and that VBUS
/// loss during an active `tRpValueChange` window detaches without firing `on_sink_rp_changed`.
#[fuchsia::test]
fn test_rp_fall_chatter_and_vbus_loss_cancellation() {
    const SNK_CC1_1A5: CcStatus = CcStatus::new(SnkPower1_5, CcPinState::Open);

    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let rp_window = fasync::MonotonicDuration::from_nanos(DEFAULT_T_RP_VALUE_CHANGE.into_nanos());
    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO),
        SNK_CC1_3A0,
    );
    tcpc.set_vbus_level(VbusLevel::Present);
    {
        let mut init = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut init), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    tcpc.events.lock().clear();

    // 1. Chatter across a `tRpValueChange` window (`Default -> Open -> 1.5A -> Default -> Open`)
    // still commits `Default` at `rp_window` even when CC reads `Open` at expiry.
    let samples = [SNK_CC1, OPEN, SNK_CC1_1A5, SNK_CC1, OPEN];
    let step = fasync::MonotonicDuration::from_nanos(rp_window.into_nanos() / samples.len() as i64);
    for (idx, sample) in samples.into_iter().enumerate() {
        tcpc.set_cc(sample);
        {
            let mut fut = pin!(tcpc.handle_cc_status(None));
            assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
        }
        let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
        exec.set_fake_time(exec.now() + step);
        assert_eq!(exec.wake_expired_timers(), idx + 1 == samples.len());
    }
    {
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert_matches!(exec.run_until_stalled(&mut idle), Poll::Ready(()));
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Default:lease=false"]);

    // 2. VBUS loss while `tRpValueChange` (`Default -> 3.0A`) is in flight detaches immediately
    // and cancels the `Rp` timer so `on_sink_rp_changed` never fires after `on_partner_detached`.
    tcpc.events.lock().clear();
    tcpc.set_cc(SNK_CC1_3A0);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert_eq!(tcpc.port.state.lock().pending_timer(), "rp_value_change");

    tcpc.set_cc(OPEN);
    tcpc.set_vbus_level(VbusLevel::Safe0V);
    {
        let mut fut = pin!(tcpc.handle_vbus_change(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    exec.set_fake_time(exec.now() + rp_window);
    assert!(!exec.wake_expired_timers());
    {
        let mut idle = pin!(tcpc.wait_for_debounce());
        assert_matches!(exec.run_until_stalled(&mut idle), Poll::Ready(()));
    }
    assert!(tcpc.typec_state().is_unattached());
    assert_eq!(*tcpc.events.lock(), vec!["detached:lease=false"]);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.rp_change_count, 1);
}

/// Verifies that a new `Attached.SNK` `Rp` level is reported exactly when the default
/// `tRpValueChange` debounce expires.
#[fuchsia::test]
fn test_default_rp_value_change() {
    const ONE_MS: fasync::MonotonicDuration = fasync::MonotonicDuration::from_millis(1);

    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let rp_debounce = fasync::MonotonicDuration::from_nanos(DEFAULT_T_RP_VALUE_CHANGE.into_nanos());

    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO),
        SNK_CC1,
    );
    {
        let mut boot = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut boot), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.events.lock().clear();

    tcpc.set_cc(SNK_CC1_3A0);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());

    exec.set_fake_time(exec.now() + rp_debounce - ONE_MS);
    assert!(!exec.wake_expired_timers());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    assert!(tcpc.events.lock().is_empty());

    exec.set_fake_time(exec.now() + ONE_MS);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Rp3_0A:lease=false"]);
}

/// Verifies that a `read_cc_status` error when `tRpValueChange` expires retries once and still
/// commits a pending `Rp` fall when both reads fail, so the Sink lowers its draw within `tSinkAdj`
/// without overwriting `cc_status` (R2.5 §4.5.2.3.3.2).
#[fuchsia::test(logging = false)]
fn test_rp_fall_commits_on_expiry_read_error() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let rp_debounce = fasync::MonotonicDuration::from_nanos(DEFAULT_T_RP_VALUE_CHANGE.into_nanos());

    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO),
        SNK_CC1_3A0,
    );
    {
        let mut boot = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut boot), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    tcpc.events.lock().clear();

    tcpc.set_cc(SNK_CC1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    assert_eq!(tcpc.port.state.lock().pending_timer(), "rp_value_change");
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));

    let reads_before = tcpc.cc_read_count.load(Ordering::SeqCst);
    tcpc.set_cc_err("persistent rp fall read error");
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    exec.set_fake_time(exec.now() + rp_debounce);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert_eq!(tcpc.cc_read_count.load(Ordering::SeqCst), reads_before + 2);
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.cc_status(), SNK_CC1);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Default:lease=false"]);
}

/// Verifies that a transient `read_cc_status` error when `tRpValueChange` expires on an `Rp` rise
/// retries immediately once (preserving the caller's wake lease) and commits the rise, while a
/// persistent read error drops the uncommitted rise and keeps the committed level.
#[fuchsia::test(logging = false)]
fn test_rp_rise_expiry_read_error_retries_once() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
    let scope = fasync::Scope::new();
    let rp_debounce = fasync::MonotonicDuration::from_nanos(DEFAULT_T_RP_VALUE_CHANGE.into_nanos());

    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO),
        SNK_CC1,
    );
    {
        let mut boot = pin!(tcpc.sync_initial_state(None));
        assert_matches!(exec.run_until_stalled(&mut boot), Poll::Ready(Ok(())));
    }
    tcpc.events.lock().clear();

    // 1. Transient expiry read error retries immediately once and commits `Rp3_0A` with the wake
    // lease open.
    let (lease, _observer) = zx::EventPair::create();
    tcpc.set_cc(SNK_CC1_3A0);
    {
        let mut fut = pin!(tcpc.handle_cc_status(Some(lease)));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    let reads_before = tcpc.cc_read_count.load(Ordering::SeqCst);
    tcpc.set_cc_err_once("first rp rise expiry read error");
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    exec.set_fake_time(exec.now() + rp_debounce);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert_eq!(tcpc.cc_read_count.load(Ordering::SeqCst), reads_before + 2);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert_eq!(*tcpc.events.lock(), vec!["sink_rp:Rp3_0A:lease=true"]);

    // 2. Persistent expiry read error fails both reads and keeps `Default`.
    tcpc.set_cc(SNK_CC1);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    exec.set_fake_time(exec.now() + rp_debounce);
    assert!(exec.wake_expired_timers());
    {
        let mut wait_fut = pin!(tcpc.wait_for_debounce());
        assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    }
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    tcpc.events.lock().clear();

    tcpc.set_cc(SNK_CC1_3A0);
    {
        let mut fut = pin!(tcpc.handle_cc_status(None));
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
    }
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    let reads_before = tcpc.cc_read_count.load(Ordering::SeqCst);
    tcpc.set_cc_err("persistent rp rise read error");
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
    exec.set_fake_time(exec.now() + rp_debounce);
    assert!(exec.wake_expired_timers());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert_eq!(tcpc.cc_read_count.load(Ordering::SeqCst), reads_before + 2);
    assert_eq!(tcpc.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(tcpc.cc_status(), SNK_CC1_3A0);
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert!(tcpc.events.lock().is_empty());
}

/// Verifies that `RpValue::typec_current_ua` reports USB Type-C Current @ 1.5 A and 3.0 A, and
/// no fixed current for Default USB Power (R2.5 §4.6.2.1, Table 4-20).
///
/// Spec obligations: `typec:4.6.2.1:typec-current-values`, `typec:4.6.2.1:default-rp-follow-usb-spec`.
#[fuchsia::test]
fn test_typec_current_per_table_4_20() {
    assert_eq!(RpValue::Default.typec_current_ua(), None);
    assert_eq!(RpValue::Rp1_5A.typec_current_ua(), Some(1_500_000));
    assert_eq!(RpValue::Rp3_0A.typec_current_ua(), Some(3_000_000));
}

/// Verifies that default USB Type-C timing constants fall within R2.5 Table 4-32 and Table 4-34
/// normative bounds.
#[fuchsia::test]
fn test_default_timing_constants_within_spec_bounds() {
    // Spec obligations: `typec:4.11.2:tccdebounce`, `typec:4.11.2:tpddebounce`,
    // `typec:4.11.2:trpvaluechange`, `typec:4.11.2:table4-34-tsrcdisconnect`,
    // `typec:4.11.2:table4-32-tvsafe0v`.
    const T_CC_DEBOUNCE_MIN: zx::BootDuration = zx::BootDuration::from_millis(100);
    const T_CC_DEBOUNCE_MAX: zx::BootDuration = zx::BootDuration::from_millis(200);
    const T_PD_DEBOUNCE_MIN: zx::BootDuration = zx::BootDuration::from_millis(10);
    const T_PD_DEBOUNCE_MAX: zx::BootDuration = zx::BootDuration::from_millis(20);
    const T_RP_CHANGE_MIN: zx::BootDuration = zx::BootDuration::from_millis(10);
    const T_RP_CHANGE_MAX: zx::BootDuration = zx::BootDuration::from_millis(20);
    const T_SRC_DISCONNECT_MAX: zx::BootDuration = zx::BootDuration::from_millis(20);
    const T_VSAFE0V_MAX: zx::BootDuration = zx::BootDuration::from_millis(650);

    assert!((T_CC_DEBOUNCE_MIN..=T_CC_DEBOUNCE_MAX).contains(&DEFAULT_T_CC_DEBOUNCE));
    assert!((T_PD_DEBOUNCE_MIN..=T_PD_DEBOUNCE_MAX).contains(&DEFAULT_T_PD_DEBOUNCE));
    assert!((T_RP_CHANGE_MIN..=T_RP_CHANGE_MAX).contains(&DEFAULT_T_RP_VALUE_CHANGE));
    assert!((zx::BootDuration::ZERO..=T_SRC_DISCONNECT_MAX).contains(&DEFAULT_T_SRC_DISCONNECT));
    assert_eq!(DEFAULT_DISCHARGE_TIMEOUT, T_VSAFE0V_MAX);
}

/// Verifies that `AttachWait.SRC` closes the sink path, arms a VBUS discharge load when entered
/// above `vSafe0V` without sourcing VBUS, bounds runaway discharge at `tVsafe0V` (`650 ms`), and
/// enters `Attached.SRC` only once both `tCCDebounce` and `vSafe0V` hold.
#[fuchsia::test]
fn test_attach_wait_src_loads_residual_vbus_per_4_5_2_2_8_1() {
    // Spec obligations: `typec:4.5.2.2.8.1:attachwait-src-load-vbus`,
    // `typec:4.5.2.2.8.1:attachwait-src-no-vbus-vconn`,
    // `typec:4.5.2.2.8.2:attachwait-src-to-attached-src`,
    // `typec:4.5.2.2.8.2:attachwait-src-vsafe0v-timeout`, `typec:4.11.2:table4-32-tvsafe0v`.
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(DEFAULT_T_CC_DEBOUNCE).with_discharge_timeout(DEFAULT_DISCHARGE_TIMEOUT)
    });
    h.sync(None).unwrap();
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();

    // Residual VBUS above `vSafe0V` on entering `AttachWait.SRC` arms discharge, never sources
    // VBUS across `tCCDebounce`, holds the wake lease until `tVsafe0V`, and clears `vbus_sink`.
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    let (lease, observer) = zx::EventPair::create();
    h.cc_alert(SRC_CC1, Some(lease)).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(true));
    assert_eq!(*h.tcpc.hw_sink.lock(), Some(false));
    assert!(!h.tcpc.vbus_sourced.load(Ordering::SeqCst));
    assert!(!is_lease_closed(&observer));

    h.advance_to_expiry(DEFAULT_T_CC_DEBOUNCE);
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!h.tcpc.vbus_sourced.load(Ordering::SeqCst));
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(!is_lease_closed(&observer));

    // At `DEFAULT_DISCHARGE_TIMEOUT` (`tVsafe0V` = 650 ms), discharge stops cleanly without calling
    // `on_vsafe0v` and releases the timeout wake lease.
    let remaining_to_vsafe0v = DEFAULT_DISCHARGE_TIMEOUT - DEFAULT_T_CC_DEBOUNCE;
    h.advance_to_expiry(remaining_to_vsafe0v);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 1);
    assert!(h.tcpc.events.lock().is_empty());
    assert!(is_lease_closed(&observer));

    // Once VBUS reaches `vSafe0V`, `AttachWait.SRC` transitions to `Attached.SRC`.
    h.tcpc.set_vsafe0v(true);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(h.tcpc.vbus_sourced.load(Ordering::SeqCst));
    h.tcpc.assert_tcpci_invariants();

    // Detaching at `vSafe0V` returns to `Unattached.SNK`; a second partner entering `AttachWait.SRC`
    // above `vSafe0V` disarms discharge when `vSafe0V` arrives before attaching.
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.tcpc.typec_state().is_unattached());
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    h.tcpc.set_vsafe0v(true);
    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    h.advance_to_expiry(DEFAULT_T_CC_DEBOUNCE);
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(*h.tcpc.hw_sink.lock(), Some(false));
    h.cancel_scope();
}

/// Verifies that entering `Unattached.SNK` without a prior detach while partner VBUS is live
/// preserves any sink path, skips VBUS discharge, and requires `tPDDebounce` on `SNK.Open`.
#[fuchsia::test]
fn test_non_detach_unattached_on_live_partner_vbus_preserves_sink_and_skips_discharge() {
    // Spec obligations: `tcpci:4.4.8:look4connection-and-sinkvbus`,
    // `typec:4.5.2.2.4.2:attachwait-snk-open-tpddebounce`, `typec:4.11.2:tpddebounce`.
    const ONE_MS: zx::BootDuration = zx::BootDuration::from_millis(1);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(DEFAULT_T_CC_DEBOUNCE).with_pd_debounce(DEFAULT_T_PD_DEBOUNCE)
    });
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.sync(None).unwrap();
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.sink_writes.lock().is_empty());
    assert!(!h.tcpc.discharge_writes.lock().contains(&true));

    // Entering `AttachWait.SNK` then seeing `SNK.Open` stays in `AttachWait.SNK` until
    // `tPDDebounce` (20 ms), and exits to `Unattached.SNK` without disabling SinkVbus or clamping
    // the still-present live VBUS.
    h.cc_alert(SNK_CC1, None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.sink_writes.lock().clear();

    h.cc_alert(OPEN, None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!h.advance(DEFAULT_T_PD_DEBOUNCE - ONE_MS));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(h.advance(ONE_MS));
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.discharge_writes.lock().contains(&true));
    assert!(h.tcpc.sink_writes.lock().is_empty());
    h.cancel_scope();
}

/// Verifies that cable orientation is fixed on entry to `Attached.SNK` from the `SNK.Rp` pin (and
/// stays fixed if CC noise occurs under load) and on entry to `Attached.SRC` from the `SRC.Rd` pin.
#[fuchsia::test]
async fn test_orientation_fixed_on_entry_to_attached_snk_and_src() {
    // Spec obligations: `typec:4.5.2.2.5.1:orientation-on-entry`,
    // `typec:4.5.2.2.9.1:attached-src-orientation-on-entry`.
    let scope = fasync::Scope::new();
    let tcpc = unattached(&scope).await;

    // 1. `Attached.SNK` fixes orientation on entry and retains it across `SNK.Open` under load.
    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkPower1_5);
    settle(&tcpc, snk_cc2).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
    settle(&tcpc, OPEN).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));

    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());

    // 2. `Attached.SRC` determines orientation from `SRC.Rd` even with `SRC.Ra` on the peer pin.
    tcpc.set_vsafe0v(true);
    let src_rd_cc2_ra_cc1 = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);
    settle(&tcpc, src_rd_cc2_ra_cc1).await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
}
