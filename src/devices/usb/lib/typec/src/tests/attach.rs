// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

/// Verifies that attach debounce handles cancellation, role/pin swaps during `tCCDebounce`, and
/// `on_partner_attached` failure rollback.
#[fuchsia::test(logging = false)]
async fn test_attach_debounce_cancellation_and_pin_swap() {
    let scope = fasync::Scope::new();
    let open_looking = CcStatus::LOOKING4CONNECTION;
    let open = CcStatus::default();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    let src = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);

    let tcpc = FakeTcpc::new(&scope, open_looking);

    // `Looking4Connection == 1` in `Unattached` is a no-op; `Looking4Connection == 0` invokes
    // `on_unattached()` to re-arm `COMMAND.Look4Connection`.
    let before_ts = tcpc.port.state.lock().machine.telemetry.last_updated_time;
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached);
    assert_eq!(tcpc.port.state.lock().machine.telemetry.last_updated_time, before_ts);
    assert!(!tcpc.is_busy());

    tcpc.drp_toggling.store(false, Ordering::SeqCst);
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert!(tcpc.drp_toggling.load(Ordering::SeqCst));
    assert_eq!(tcpc.port.state.lock().machine.telemetry.last_updated_time, before_ts);
    assert!(!tcpc.is_busy());

    // 1. Source attach debounce aborted via `Open` while in flight returns immediately to
    // `Unattached`, and the sleeping debounce task holds only `Weak<T>` (no `Arc` reference cycle).
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(src);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(Arc::strong_count(&tcpc), 1);
    tcpc.set_cc(open);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.cc_status(), CcStatus::default());
    assert_eq!(tcpc.orientation(), None);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());

    // 2. Attach debounce aborted when CC drops to Open during `tCCDebounce` without an IRQ.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.set_cc(open);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_connected());
    assert!(tcpc.typec_state().is_unattached());

    // 3. Role change (`Snk -> Src`) during `tCCDebounce` without an IRQ restarts `AttachWait`
    // in-place for the new role.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(src);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert!(tcpc.typec_state().is_sourcing_vbus());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        is_connected: true,
        last_updated_time_ns: AnyProperty,
        cc1_state: "SRC.Ra",
        cc2_state: "SRC.Rd",
        typec_state: "Attached.SRC",
        active_debounce_direction: "none",
        attach_count: 1u64,
        disconnect_count: 0u64,
        debounce_cancelled_count: 3u64,
        vbus_discharge_armed: false,
        vbus_sink_enabled: false,
        pending_timer: "none",
        rp_change_count: 0u64,
        sink_attach_failure_count: 0u64,
        source_attach_failure_count: 0u64,
    });
    tcpc.reset_to_disconnected();

    // 4. Active CC pin swap (`CC1 -> CC2`) during `tCCDebounce` restarts `tCCDebounce` for the
    // new pin and attaches on `CC2`.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(snk_cc2);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
    tcpc.reset_to_disconnected();

    // 5. An `on_partner_attached` error holds the partner, whose `Rp` is still present, in
    // `AttachWait.SNK` (USB Type-C R2.5 §4.5.2.2.4.2) rather than retrying or returning to
    // `Unattached`.
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached);
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    tcpc.reset_to_disconnected();

    // The superseded attach task releases its wake lease; the replacement task holds the new one.
    tcpc.events.lock().clear();
    let gate = tcpc.port.debounce_gate().lock().await;
    let (lease1, lease1_peer) = zx::EventPair::create();
    let (lease2, lease2_peer) = zx::EventPair::create();
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(Some(lease1)).await.unwrap();
    tcpc.set_cc(snk_cc2);
    tcpc.handle_cc_status(Some(lease2)).await.unwrap();
    assert!(tcpc.events.lock().is_empty());
    fasync::OnSignals::new(&lease1_peer, zx::Signals::EVENTPAIR_PEER_CLOSED).await.unwrap();
    assert_eq!(
        lease2_peer
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST),
        zx::WaitResult::TimedOut(zx::Signals::empty())
    );
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.orientation(), Some(fconnector::Orientation::Reverse));
    tcpc.reset_to_disconnected();
}

/// Verifies that `CC_STATUS` read errors propagate while enabled, discard mid-read when disabled,
/// and recover via a follow-up `CC_STATUS` check on transient post-`tCCDebounce` failure.
#[fuchsia::test(logging = false)]
async fn test_cc_read_errors_propagate_and_recover() {
    let scope = fasync::Scope::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let tcpc = FakeTcpc::new(&scope, CcStatus::default());

    tcpc.set_cc_err("cc read error");
    assert!(tcpc.handle_cc_status(None).await.is_err());
    assert!(tcpc.sample_attachable_role().await.is_err());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);

    // A `handle_cc_status` call while disabled returns `Ok(())` without reading `CC_STATUS`.
    tcpc.port.state.lock().set_disabled(true);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::Disabled);

    // Both successful and failing `sample_attachable_role` reads that complete after
    // `set_disabled(true)` return `Ok(None)`.
    tcpc.port.state.lock().set_disabled(false);
    tcpc.set_cc(snk_cc1);
    let (hold, started_rx) = tcpc.cc_read.arm().await;
    let mut query = pin!(tcpc.sample_attachable_role());
    poll_until_oneshot(query.as_mut(), started_rx).await;
    tcpc.port.state.lock().set_disabled(true);
    drop(hold);
    assert_eq!(query.await.unwrap(), None);

    tcpc.port.state.lock().set_disabled(false);
    tcpc.set_cc_err("cc query error before disable");
    let (hold, started_rx) = tcpc.cc_read.arm().await;
    let mut query = pin!(tcpc.sample_attachable_role());
    poll_until_oneshot(query.as_mut(), started_rx).await;
    tcpc.port.state.lock().set_disabled(true);
    drop(hold);
    assert_eq!(query.await.unwrap(), None);

    // A transient post-`tCCDebounce` `CC_STATUS` read error runs `on_unattached` and schedules
    // a one-shot recovery `CC_STATUS` recheck preserving the caller's wake lease, so a static
    // partner still plugged in attaches with that wake lease without needing a new IRQ edge.
    tcpc.port.state.lock().set_disabled(false);
    tcpc.events.lock().clear();
    tcpc.set_cc(snk_cc1);
    let gate = tcpc.port.debounce_gate().lock().await;
    let (lease, _lease_peer) = zx::EventPair::create();
    tcpc.handle_cc_status(Some(lease)).await.unwrap();
    let before_unattached = tcpc.unattached_count.load(Ordering::SeqCst);
    tcpc.set_cc_err_once("transient cc read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.unattached_count.load(Ordering::SeqCst), before_unattached + 1);
    assert!(tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=true"]);
    tcpc.reset_to_disconnected();

    // A persistent `CC_STATUS` read error terminates cleanly in `Unattached` with `Open`
    // `cc_status` without looping.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(snk_cc1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc_err("persistent cc read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_connected());
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(tcpc.cc_status(), CcStatus::default());
}
