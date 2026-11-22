// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

const CONCURRENT_INTERLEAVED_TASKS: usize = 16;
const RANDOM_CONFIG_COMBINATIONS: usize = 32;
const RANDOM_STEPS_PER_CONFIG: usize = 32;
const RANDOM_OVERLAP_WINDOW: usize = 4;
const RANDOM_CHOICE_COUNT: u64 = 8;
const RANDOM_VBUS_CHOICE_COUNT: u64 = 5;
const RANDOM_CC_ERR_PERIOD: u64 = 8;
const RANDOM_FAIL_ATTACH_PERIOD: u64 = 11;
const RANDOM_FAIL_POWER_WRITE_PERIOD: u64 = 13;
const RANDOM_SEED: u64 = 0x5eed_cafe_d00d_beef;

/// Verifies that dropping the driver mid-attach debounce frees the
/// driver immediately (timers hold only `Weak<T>`) and exits cleanly without leaking tasks.
#[fuchsia::test]
async fn test_driver_drop_mid_attach_debounce() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);

    let temp_tcpc = FakeTcpc::new(&scope, CcStatus::default());
    let weak = Arc::downgrade(&temp_tcpc);
    let gate_arc = Arc::clone(temp_tcpc.port.debounce_gate());
    let gate = gate_arc.lock().await;
    temp_tcpc.set_cc(snk_cc1);
    temp_tcpc.handle_cc_status(None).await.unwrap();
    drop(temp_tcpc);
    assert!(weak.upgrade().is_none(), "sleeping debounce timer must hold only Weak<T>");
    drop(gate);
    scope.on_no_tasks().await;
}

/// Verifies that an owned actor command future keeps the driver alive when the caller drops
/// its own `Arc` first, and releases it once completed.
#[fuchsia::test]
async fn test_owned_command_future_keeps_driver_alive_until_complete() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);

    let temp_tcpc = FakeTcpc::new(&scope, snk_cc1);
    let weak = Arc::downgrade(&temp_tcpc);
    let fut = temp_tcpc.sync_initial_state(None);
    drop(temp_tcpc);
    fut.await.unwrap();
    assert!(weak.upgrade().is_none());
    scope.on_no_tasks().await;
}

/// Verifies that cancelling the async scope mid-debounce clears the active timer
/// synchronously, waking `wait_for_debounce` without hanging.
#[fuchsia::test]
async fn test_scope_cancel_mid_debounce_clears_timer() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    let gate_arc = Arc::clone(tcpc.port.debounce_gate());
    let gate = gate_arc.lock().await;
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.is_busy());

    let mut idle = pin!(tcpc.wait_for_debounce());
    assert!(poll!(&mut idle).is_pending());
    scope.cancel().await;
    assert_eq!(poll!(&mut idle), Poll::Ready(()));
    assert!(!tcpc.is_busy());
    drop(gate);
}

/// Verifies that dropping the caller's `handle_cc_status` future while `read_cc_status` is
/// suspended still finishes on `port.scope` and arms the debounce timer so the port reaches
/// `Attached.SNK`.
#[fuchsia::test]
async fn test_drop_foreground_future_mid_callback_completes_on_scope() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
    {
        let mut cc_fut = pin!(tcpc.handle_cc_status(None));
        poll_until_oneshot(cc_fut.as_mut(), cc_started_rx).await;
    }
    // `cc_fut` has been dropped while `read_cc_status` is held; the step continues on `scope`.
    assert!(tcpc.is_busy());
    drop(cc_hold);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
}

/// Verifies that cancelling the async scope while an expired timer task is queued on the actor
/// behind an in-flight command still clears the timer and wakes `wait_for_debounce`.
#[fuchsia::test]
fn test_scope_cancel_while_timer_queued_on_actor_clears_timer() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    // Start a real attach timer held on `debounce_gate`, then suspend a second `handle_cc_status`
    // inside `read_cc_status` while releasing `debounce_gate` so `TimerExpired` queues behind it.
    let mut setup_fut = pin!(async {
        let debounce_gate = tcpc.port.debounce_gate().lock().await;
        tcpc.handle_cc_status(None).await.unwrap();
        let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
        let mut cc_fut = pin!(tcpc.handle_cc_status(None));
        poll_until_oneshot(cc_fut.as_mut(), cc_started_rx).await;
        drop(debounce_gate);
        (cc_hold, cc_fut.await)
    });
    assert!(exec.run_until_stalled(&mut setup_fut).is_pending());
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert!(tcpc.port.state.lock().runner.timer.is_active());
    assert!(tcpc.port.state.lock().runner.actor_running);
    assert_eq!(tcpc.port.state.lock().runner.queue.len(), 1);

    let mut finish_fut = pin!(async {
        scope.cancel().await;
        assert!(!tcpc.is_busy());
        tcpc.wait_for_debounce().await;
        // Also verify that the actor retires when idle so `scope.on_no_tasks()` resolves while
        // `tcpc2` is still alive.
        let scope2 = fasync::Scope::new();
        let tcpc2 = FakeTcpc::new(&scope2, snk_cc1);
        tcpc2.handle_cc_status(None).await.unwrap();
        tcpc2.wait_for_debounce().await;
        scope2.on_no_tasks().await;
        assert!(tcpc2.is_connected());
    });
    assert!(exec.run_until_stalled(&mut finish_fut).is_ready());
}

/// Verifies that spawning a timer on a closed async scope immediately clears the runner's timer
/// when dropped, so the port goes idle instead of reporting a dead timer as busy forever.
#[fuchsia::test]
async fn test_timer_spawned_on_closed_scope_clears_synchronously() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    // Hold the actor in `read_cc_status`, close the scope, then let it spawn the attach timer.
    let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
    let mut cc_fut = pin!(tcpc.handle_cc_status(None));
    poll_until_oneshot(cc_fut.as_mut(), cc_started_rx).await;
    tcpc.port.scope.close();
    drop(cc_hold);
    cc_fut.await.unwrap();

    // Asserted before waiting so a dead installed timer fails fast instead of hanging.
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.wait_for_debounce().await;
}

/// Verifies that cancelling an active timer after `Action::TimerExpired` has already queued behind
/// an in-flight actor command invalidates the timer's generation ID so the actor discards it.
#[fuchsia::test]
fn test_cancelled_queued_timer_is_discarded_by_actor() {
    let mut exec = fasync::TestExecutor::new_with_fake_time();
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, src_cc1);

    let (debounce_gate, cc_hold, cc_started_rx) = {
        let mut setup_fut = pin!(async {
            let debounce_gate = tcpc.port.debounce_gate().lock().await;
            tcpc.handle_cc_status(None).await.unwrap();
            assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);

            tcpc.set_cc(open);
            let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
            (debounce_gate, cc_hold, cc_started_rx)
        });
        let Poll::Ready(triple) = exec.run_until_stalled(&mut setup_fut) else {
            panic!("setup should arm cc_read synchronously");
        };
        triple
    };
    let mut abort_fut = pin!(tcpc.handle_cc_status(None));
    assert!(exec.run_until_stalled(&mut abort_fut).is_pending());
    let mut rx_fut = pin!(cc_started_rx);
    assert!(exec.run_until_stalled(&mut rx_fut).is_ready());
    // Now release `debounce_gate` while `abort_fut` is already in flight on the actor so
    // `Command::TimerExpired` queues behind `abort_fut`.
    drop(debounce_gate);
    let _ = exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    assert!(tcpc.port.state.lock().runner.actor_running);
    assert_eq!(tcpc.port.state.lock().runner.queue.len(), 1);

    // Put `src_cc1` back on the wire before releasing `cc_hold`: `abort_fut` already sampled
    // `open` and cancels the timer token, so the queued `TimerExpired` must not re-read `src_cc1`
    // or attach.
    tcpc.set_cc(src_cc1);
    drop(cc_hold);
    assert_matches!(exec.run_until_stalled(&mut abort_fut), Poll::Ready(Ok(())));
    let mut wait_fut = pin!(tcpc.wait_for_debounce());
    assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
}

/// Verifies that cancelling the async scope before a committed attach runs returns `Err` and
/// leaves the port `Unattached` and idle without sourcing VBUS.
#[fuchsia::test(logging = false)]
async fn test_scope_closed_before_committed_attach_returns_err_and_idles() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
    let mut attach_fut = pin!(tcpc.sync_initial_state(None));
    poll_until_oneshot(attach_fut.as_mut(), cc_started_rx).await;
    scope.cancel().await;
    drop(cc_hold);
    assert!(attach_fut.await.is_err());

    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.active_debounce_direction(), None);

    // Calling `sync_initial_state` on an already-closed scope also returns `Err` and leaves
    // the port idle.
    assert!(tcpc.sync_initial_state(None).await.is_err());
    assert!(!tcpc.is_busy());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
}

/// Verifies that dropping the driver mid-disconnect debounce frees the
/// driver immediately (timers hold only `Weak<T>`) and exits cleanly without leaking tasks.
#[fuchsia::test]
async fn test_driver_drop_mid_disconnect_debounce() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

    let temp_tcpc = FakeTcpc::new(&scope, src_cc1);
    let weak = Arc::downgrade(&temp_tcpc);
    temp_tcpc.handle_cc_status(None).await.unwrap();
    temp_tcpc.wait_for_debounce().await;
    assert_eq!(temp_tcpc.typec_state(), TypecState::AttachedSrc);
    let gate_arc = Arc::clone(temp_tcpc.port.debounce_gate());
    let gate = gate_arc.lock().await;
    temp_tcpc.set_cc(open);
    temp_tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(temp_tcpc.active_debounce_direction(), Some(DebounceDirection::Disconnect));
    drop(temp_tcpc);
    assert!(weak.upgrade().is_none(), "sleeping debounce timer must hold only Weak<T>");
    drop(gate);
    scope.on_no_tasks().await;
}

/// Verifies that re-entrant queries during callbacks succeed and that active
/// cable `Ra -> Open` transitions after enabling `VCONN` keep `Attached.SRC` connected.
#[fuchsia::test]
async fn test_reentrant_callbacks_and_active_cable_ra() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_with_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);
    let src_after_vconn_enabled = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);
    tcpc.set_vsafe0v(true);
    tcpc.reentrant_query_in_callbacks.store(true, Ordering::SeqCst);

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(src_with_ra);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));

    // When `EnableVconn = 1` disconnects the `CC1` (`Ra`) comparator (`CC1 = Open, CC2 = SrcRd`),
    // `shares_connected_cc_pin` keeps `Attached.SRC` connected without false disconnect debounce.
    tcpc.set_cc(src_after_vconn_enabled);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    // Detaching exercises re-entrant `sample_attachable_role` inside `on_partner_detached`.
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
}

/// Verifies that a queued CC status change waits for an in-flight attach callback
/// before starting the disconnect debounce, so committed callbacks never overlap.
#[fuchsia::test]
async fn test_queued_cc_change_waits_for_attach_callback() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let tcpc = FakeTcpc::new(&scope, open);

    tcpc.events.lock().clear();
    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(src_cc2);
    tcpc.handle_cc_status(None).await.unwrap();
    attach_started_rx.await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);

    tcpc.set_cc(open);
    let mut disconnect_cc_fut = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut disconnect_cc_fut).is_pending());
    drop(attach_hold);
    disconnect_cc_fut.await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Source:lease=false", "detached:lease=false"]);
}

/// Verifies that `CC_STATUS` and VBUS commands queued while a callback is in flight execute
/// sequentially in FIFO order on `port.scope` (even if an intermediate caller drops its future),
/// hand wake leases to spawned debounce timers, and return independent per-turn results.
#[fuchsia::test]
async fn test_fifo_queued_commands_and_wake_leases() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    tcpc.handle_cc_status(None).await.unwrap();
    attach_started_rx.await.unwrap();

    let reads_before = tcpc.cc_read_count.load(Ordering::SeqCst);
    let (cc_lease_1, cc_peer_1) = zx::EventPair::create();
    let (cc_lease_2, cc_peer_2) = zx::EventPair::create();
    let (vbus_lease, vbus_peer) = zx::EventPair::create();

    let mut cc_1 = pin!(tcpc.handle_cc_status(Some(cc_lease_1)));
    assert!(poll!(&mut cc_1).is_pending());

    // Dropping a queued caller's future leaves its step queued on `port.scope` until the actor
    // drains it.
    {
        let mut dropped_cc = pin!(tcpc.handle_cc_status(Some(cc_lease_2)));
        assert!(poll!(&mut dropped_cc).is_pending());
    }

    tcpc.set_cc(snk_cc1_3a0);
    let mut vbus_1 = pin!(tcpc.handle_vbus_change(Some(vbus_lease)));
    assert!(poll!(&mut vbus_1).is_pending());

    for peer in [&cc_peer_1, &cc_peer_2, &vbus_peer] {
        assert_eq!(
            peer.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
            zx::WaitResult::TimedOut(zx::Signals::empty())
        );
    }
    assert!(tcpc.port.state.lock().runner.actor_running);
    assert_eq!(tcpc.port.state.lock().runner.queue.len(), 3);

    let debounce_gate = tcpc.port.debounce_gate().lock().await;
    drop(attach_hold);
    let (r1, v1) = futures::join!(cc_1, vbus_1);
    r1.unwrap();
    v1.unwrap();
    assert_eq!(tcpc.cc_read_count.load(Ordering::SeqCst), reads_before + 2);
    assert_eq!(
        cc_peer_1.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
        zx::WaitResult::TimedOut(zx::Signals::empty())
    );
    for peer in [&cc_peer_2, &vbus_peer] {
        assert!(
            peer.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
                .to_result()
                .is_ok()
        );
    }

    drop(debounce_gate);
    tcpc.wait_for_debounce().await;
    assert!(
        cc_peer_1
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
            .is_ok()
    );
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false", "sink_rp:Rp3_0A:lease=true"]);

    // Queued callers each run their own hardware read and observe their own `Result`.
    let (cc_hold, cc_started_rx) = tcpc.cc_read.arm().await;
    let mut in_flight = pin!(tcpc.handle_cc_status(None));
    poll_until_oneshot(in_flight.as_mut(), cc_started_rx).await;
    let mut err_turn = pin!(tcpc.handle_cc_status(None));
    let mut ok_turn = pin!(tcpc.handle_cc_status(None));
    assert!(poll!(&mut err_turn).is_pending());
    assert!(poll!(&mut ok_turn).is_pending());
    assert_eq!(tcpc.port.state.lock().runner.queue.len(), 2);

    tcpc.set_cc_err_once("queued cc read error");
    drop(cc_hold);
    in_flight.await.unwrap();
    let (e1, r2) = futures::join!(err_turn, ok_turn);
    assert!(e1.unwrap_err().to_string().contains("queued cc read error"));
    r2.unwrap();
}

/// Verifies that interleaved concurrent async operations preserve internal state safety
/// and callback non-overlap without deadlocks.
#[fuchsia::test]
async fn test_concurrent_interleaved_operations() {
    let scope = fasync::Scope::new();
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open);
    tcpc.set_vsafe0v(true);
    tcpc.yield_in_callbacks.store(true, Ordering::SeqCst);

    let stress_samples = [snk_cc1, src_cc2, open];
    let mut futs = Vec::new();
    for i in 0..CONCURRENT_INTERLEAVED_TASKS {
        let t = tcpc.clone();
        futs.push(async move {
            let cc = stress_samples[i % stress_samples.len()];
            t.set_cc(cc);
            t.handle_cc_status(None).await.unwrap();
            t.handle_vbus_change(None).await.unwrap();
            let _ = t.sample_attachable_role().await.unwrap();
            let _ = format!("{:?}", t.typec());
        });
    }
    futures::future::join_all(futs).await;
    tcpc.set_cc(open);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.set_vbus_present(true);
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.active_callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
}

/// Verifies that state machine invariants and non-overlapping callbacks hold across seeded
/// randomized CC, `vSafe0V`, and error sequences.
#[fuchsia::test(logging = false)]
async fn test_randomized_state_machine_invariants() {
    let scope = fasync::Scope::new();
    let cc_samples = [
        CcStatus::default(),
        CcStatus::LOOKING4CONNECTION,
        CcStatus::new(CcPinState::SnkDefault, CcPinState::Open),
        CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open),
        CcStatus::new(CcPinState::Open, CcPinState::SnkPower1_5),
        CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault),
        CcStatus::new(CcPinState::SrcRd, CcPinState::Open),
        CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd),
        CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRd),
        CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRa),
        CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa),
    ];

    let mut rng = RANDOM_SEED;
    let mut next_u64 = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for cfg_idx in 0..RANDOM_CONFIG_COMBINATIONS {
        let port = TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_pd_debounce(zx::BootDuration::ZERO)
            .with_rp_value_change(zx::BootDuration::ZERO)
            .with_discharge_timeout(zx::BootDuration::INFINITE)
            .with_sink_debug_accessory_mode(cfg_idx & 1 != 0);
        let tcpc = FakeTcpc::new_with_port(port, CcStatus::LOOKING4CONNECTION);
        tcpc.yield_in_callbacks.store(true, Ordering::SeqCst);

        for step in 0..RANDOM_STEPS_PER_CONFIG {
            let choice = next_u64() % RANDOM_CHOICE_COUNT;
            let cc_choice = cc_samples[(next_u64() as usize) % cc_samples.len()];
            if next_u64() % RANDOM_CC_ERR_PERIOD == 0 {
                tcpc.set_cc_err("simulated cc read error");
            } else {
                tcpc.set_cc(cc_choice);
            }
            match next_u64() % RANDOM_VBUS_CHOICE_COUNT {
                0 => {
                    tcpc.set_vsafe0v(true);
                    tcpc.set_vbus_present(true);
                }
                1 => tcpc.set_vsafe0v(false),
                2 => tcpc.set_vbus_present(false),
                3 => tcpc.set_vsafe0v_err("simulated vsafe0v read error"),
                _ => tcpc.set_vbus_present_err("simulated vbus_present read error"),
            }
            tcpc.fail_attach.store(next_u64() % RANDOM_FAIL_ATTACH_PERIOD == 0, Ordering::SeqCst);
            tcpc.fail_sink_write
                .store(next_u64() % RANDOM_FAIL_POWER_WRITE_PERIOD == 0, Ordering::SeqCst);
            tcpc.fail_discharge_write
                .store(next_u64() % RANDOM_FAIL_POWER_WRITE_PERIOD == 0, Ordering::SeqCst);

            match choice {
                0..=3 => {
                    let _ = tcpc.handle_cc_status(None).await;
                }
                4..=5 => {
                    let _ = tcpc.handle_vbus_change(None).await;
                }
                6 => {
                    if tcpc.typec_state() == TypecState::UnattachedSnk && !tcpc.is_busy() {
                        let _ = tcpc.sync_initial_state(None).await;
                    } else {
                        let _ = tcpc.handle_cc_status(None).await;
                    }
                }
                _ => {
                    tcpc.wait_for_debounce().await;
                    let _ = tcpc.sync_initial_state(None).await;
                }
            }

            tcpc.assert_tcpci_invariants();

            if step % RANDOM_OVERLAP_WINDOW == RANDOM_OVERLAP_WINDOW - 1
                || step == RANDOM_STEPS_PER_CONFIG - 1
            {
                tcpc.wait_for_debounce().await;
                assert!(!tcpc.is_busy());
                assert_eq!(tcpc.active_callbacks.load(Ordering::SeqCst), 0);
                assert_eq!(tcpc.active_debounce_direction(), None);
            }
        }
    }
}

/// Verifies that cancelling the async scope while `on_partner_attached` is suspended rolls back
/// the uncommitted attach to `Unattached` and idles the port with `attach_count == 0`.
#[fuchsia::test]
async fn test_scope_cancel_mid_attach_callback_resets_uncommitted_attach() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, snk_cc1);

    let (attach_hold, attach_started_rx) = tcpc.attach.arm().await;
    let mut attach_fut = pin!(tcpc.sync_initial_state(None));
    poll_until_oneshot(attach_fut.as_mut(), attach_started_rx).await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, 0);

    scope.cancel().await;
    drop(attach_hold);
    assert!(attach_fut.await.is_err());

    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.attach_count, 0);
}
