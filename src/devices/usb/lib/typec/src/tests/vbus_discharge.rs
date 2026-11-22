// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::power::{UnattachedPowerPlan, UnattachedVbus};

const OPEN: CcStatus = CcStatus::new(CcPinState::Open, CcPinState::Open);
const SNK_CC1: CcStatus = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
const SRC_CC1: CcStatus = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
const SRC_CC2: CcStatus = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
const RA_RA: CcStatus = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);

/// Verifies that `handle_vbus_change` debounces `vSafe0V` when residual discharge is armed in
/// `Unattached` or `AttachWait.SRC`, holds the wake lease across the debounce, disarms VBUS
/// discharge before `on_vsafe0v` (calling `on_vsafe0v` only in `Unattached`), cancels the timer
/// when VBUS rises or on entering `AttachWait.SNK`, and starts none when discharge is disarmed.
#[fuchsia::test(logging = false)]
async fn test_handle_vbus_change_vsafe0v_debounce_and_cancellation() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);
    let arm_unattached_discharge = async |tcpc: &Arc<FakeTcpc>| {
        tcpc.set_cc(OPEN);
        tcpc.set_vsafe0v(false);
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(tcpc.is_vbus_discharge_armed());
        tcpc.events.lock().clear();
    };

    // 1. When VBUS is not at `vSafe0V` or discharge was never armed, no debounce starts.
    tcpc.set_vsafe0v(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_busy());
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_busy());
    assert!(tcpc.discharge_writes.lock().is_empty());
    assert!(tcpc.events.lock().is_empty());

    // 2. `vSafe0V` in `Unattached` with discharge armed holds the wake lease across the debounce,
    // re-verifies `is_vbus_safe0v()`, retries a transient disarm write failure once, and calls
    // `on_vsafe0v` only once the disarm write succeeds.
    arm_unattached_discharge(&tcpc).await;
    tcpc.set_vsafe0v(true);
    tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_vbus_discharge_armed());
    assert!(tcpc.events.lock().is_empty());
    tcpc.fail_discharge_write.store(false, Ordering::SeqCst);

    let gate = tcpc.port.debounce_gate().lock().await;
    let (lease, lease_peer) = zx::EventPair::create();
    tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    tcpc.handle_vbus_change(Some(lease)).await.unwrap();
    assert!(tcpc.is_busy());
    assert_eq!(tcpc.active_debounce_direction(), None);
    assert!(!is_lease_closed(&lease_peer));
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_busy());
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true, false]);
    assert_eq!(*tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
    tcpc.events.lock().clear();
    tcpc.discharge_writes.lock().clear();

    // 3. Transient `vSafe0V` that rises above 0.8V during debounce keeps discharge armed,
    // re-issues `write_vbus_discharge(true)` for a fresh edge on TCPCI `ForceDischarge`
    // controllers so the stopped bleed load resumes, and does not call `on_vsafe0v`.
    arm_unattached_discharge(&tcpc).await;
    assert!(tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    tcpc.set_vsafe0v(true);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(tcpc.is_busy());
    tcpc.set_vsafe0v(false);
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_vbus_discharge_armed());
    assert!(tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true, true]);
    assert!(tcpc.events.lock().is_empty());

    // 3b. A VBUS-change alert while VBUS is back above `vSafe0V` cancels a pending `vSafe0V`
    // timer immediately, re-arms discharge, and avoids passing the noise filter; an `is_vbus_safe0v`
    // error at expiry leaves discharge armed without calling `on_vsafe0v`.
    tcpc.set_vsafe0v(true);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.port.state.lock().pending_timer(), "vsafe0v");
    tcpc.set_vsafe0v_err_once("vsafe0v re-read failure");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.is_vbus_discharge_armed());
    assert!(tcpc.events.lock().is_empty());

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.port.state.lock().pending_timer(), "vsafe0v");
    tcpc.set_vsafe0v(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert!(tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true, true, true]);
    drop(gate);
    tcpc.wait_for_debounce().await;

    // 4. Entering `AttachWait.SNK` while the `vSafe0V` debounce is in flight disarms discharge
    // and cancels the `vSafe0V` timer so `on_vsafe0v` does not fire.
    tcpc.set_vsafe0v(true);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    assert!(!tcpc.is_vbus_discharge_armed());
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
    tcpc.reset_to_disconnected();
    tcpc.events.lock().clear();
    tcpc.discharge_writes.lock().clear();

    // 5. In `AttachWait.SRC` with residual discharge armed (e.g. unsupported `Ra/Ra` held in
    // `AttachWait.SRC`), `vSafe0V` disarms residual discharge without invoking `on_vsafe0v`.
    arm_unattached_discharge(&tcpc).await;
    tcpc.set_cc(RA_RA);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(tcpc.is_vbus_discharge_armed());
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true, false]);
    assert!(tcpc.events.lock().is_empty());
    tcpc.reset_to_disconnected();
    tcpc.discharge_writes.lock().clear();

    // 6. When discharge is not armed, no `vSafe0V` debounce starts in `AttachWait.SRC` (where
    // `vSafe0V` qualifies the attach instead), `Attached.SRC`, or `Disabled`.
    tcpc.set_vsafe0v(false);
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(!tcpc.is_busy());
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_busy());
    tcpc.port.state.lock().set_disabled(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert!(!tcpc.is_busy());
    assert!(!tcpc.events.lock().iter().any(|e| e.starts_with("vsafe0v")));
}

/// Verifies that duplicate `vSafe0V` alerts while a `vSafe0V` debounce is already running neither
/// reset the countdown nor drop an existing wake lease when called with `None`.
#[fuchsia::test]
fn test_duplicate_vsafe0v_alerts_preserve_countdown_and_wake_lease() {
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(50);
    const DUPLICATE_ALERT_AT: zx::BootDuration = zx::BootDuration::from_millis(20);
    const REMAINING: zx::BootDuration = zx::BootDuration::from_nanos(
        VSAFE0V_DEBOUNCE.into_nanos() - DUPLICATE_ALERT_AT.into_nanos(),
    );

    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_vsafe0v_debounce(VSAFE0V_DEBOUNCE));

    // Case 1: duplicate `handle_vbus_change(None)` keeps `lease1` open and fires at `t = 50ms`.
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    h.tcpc.set_vsafe0v(true);
    let (lease1, peer1) = zx::EventPair::create();
    h.vbus_alert(Some(lease1)).unwrap();

    assert!(!h.advance(DUPLICATE_ALERT_AT));
    h.vbus_alert(None).unwrap();
    assert!(!is_lease_closed(&peer1));

    h.advance_to_expiry(REMAINING);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
    h.tcpc.events.lock().clear();

    // Case 2: duplicate `handle_vbus_change(Some(lease2))` replaces `lease1` immediately without
    // extending the original deadline.
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    h.tcpc.set_vsafe0v(true);
    let (lease1, peer1) = zx::EventPair::create();
    h.vbus_alert(Some(lease1)).unwrap();

    assert!(!h.advance(DUPLICATE_ALERT_AT));
    let (lease2, peer2) = zx::EventPair::create();
    h.vbus_alert(Some(lease2)).unwrap();
    assert!(is_lease_closed(&peer1));
    assert!(!is_lease_closed(&peer2));

    assert!(h.advance(REMAINING));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
}

/// Verifies that `TypecPort` arms VBUS discharge after a detach (and after a bounced attach) unless
/// VBUS is already at `vSafe0V`, disarms it on `AttachWait` without redundant writes, and
/// invalidates cached confirmation when the hardware write fails.
#[fuchsia::test(logging = false)]
async fn test_vbus_discharge_policy() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);
    let attach = async |tcpc: &Arc<FakeTcpc>| {
        tcpc.set_cc(SNK_CC1);
        tcpc.handle_cc_status(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    };
    let detach = async |tcpc: &Arc<FakeTcpc>| {
        tcpc.set_cc(OPEN);
        tcpc.set_vbus_present(false);
        tcpc.handle_vbus_change(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        tcpc.set_vbus_present(true);
        assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    };

    // 1. `AttachWait.SNK` disarms discharge; detaching with residual VBUS arms it once and
    // `on_unattached` does not re-arm it.
    tcpc.set_vsafe0v(false);
    attach(&tcpc).await;
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false]);
    detach(&tcpc).await;
    assert!(tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false, true]);
    tcpc.discharge_writes.lock().clear();

    // 2. An attach that bounces back to `Unattached` with residual VBUS re-arms discharge.
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_vbus_discharge_armed());
    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert!(tcpc.typec_state().is_unattached());
    assert!(tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false, true]);
    tcpc.discharge_writes.lock().clear();

    // 3. Detaching with VBUS already at `vSafe0V` leaves discharge disarmed without redundant
    // `false` writes once discharge is already recorded as disarmed.
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert!(!tcpc.is_vbus_discharge_armed());
    tcpc.discharge_writes.lock().clear();
    attach(&tcpc).await;
    detach(&tcpc).await;
    assert!(!tcpc.is_vbus_discharge_armed());
    assert!(tcpc.discharge_writes.lock().is_empty());

    // 4. A failed arm write leaves `is_vbus_discharge_armed()` false while invalidating the prior
    // `Disabled` cache entry, so the next `AttachWait.SNK` still writes `false`.
    tcpc.set_vsafe0v(false);
    attach(&tcpc).await;
    tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    detach(&tcpc).await;
    assert!(!tcpc.is_vbus_discharge_armed());
    assert!(tcpc.discharge_writes.lock().is_empty());
    tcpc.sync_initial_state(None).await.unwrap();
    assert!(!tcpc.is_vbus_discharge_armed());
    tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    tcpc.discharge_writes.lock().clear();

    // 5. Sink VBUS loss with `Rp` still asserted detaches to `Unattached` (even if closing the
    // sink path fails), which starts discharge, and re-entering `AttachWait.SNK` stops it again
    // (or retries stopping it at attach if the `AttachWait.SNK` disarm transiently fails).
    attach(&tcpc).await;
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false]);
    tcpc.discharge_writes.lock().clear();
    tcpc.fail_sink_write.store(true, Ordering::SeqCst);
    tcpc.set_vbus_present(false);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.fail_sink_write.store(false, Ordering::SeqCst);
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true, false]);

    tcpc.set_vbus_present(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    detach(&tcpc).await;
    assert!(tcpc.is_vbus_discharge_armed());
    tcpc.discharge_writes.lock().clear();
    tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    attach(&tcpc).await;
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false]);
}

/// Verifies that recovering from a `CC_STATUS` read error at the end of `tCCDebounce`, while a
/// Source still supplies VBUS, neither closes the sink path (`DisableSinkVbus` would discharge that
/// VBUS) nor starts VBUS discharge, and re-detects the partner.
#[fuchsia::test(logging = false)]
async fn test_attach_read_error_recovery_keeps_live_vbus() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);
    tcpc.set_vsafe0v(false);
    tcpc.set_vbus_present(true);

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    tcpc.discharge_writes.lock().clear();
    tcpc.sink_writes.lock().clear();

    tcpc.set_cc_err_once("attach read error");
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert!(!tcpc.discharge_writes.lock().contains(&true));
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
}

/// Verifies that `with_discharge_timeout` stops an armed `Unattached` VBUS discharge when
/// `vSafe0V` never arrives (without extending the deadline across intermediate `AttachWait.SRC`
/// bounces) and increments `discharge_timeout_count`.
#[fuchsia::test(logging = false)]
fn test_unattached_discharge_timeout_stops_discharge() {
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const BOUNCE_AT: zx::BootDuration = zx::BootDuration::from_millis(200);
    const REMAINING: zx::BootDuration =
        zx::BootDuration::from_nanos(DISCHARGE_TIMEOUT.into_nanos() - BOUNCE_AT.into_nanos());

    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_discharge_timeout(DISCHARGE_TIMEOUT));
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");

    // Bouncing through `AttachWait.SRC` and back to `Unattached` while discharge stays armed keeps
    // the original `DISCHARGE_TIMEOUT` deadline instead of restarting a fresh 650ms window.
    assert!(!h.advance(BOUNCE_AT));
    h.cc_alert(RA_RA, None).unwrap();
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.advance_to_expiry(REMAINING);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 1);
}

/// Verifies that a discharge timeout finding VBUS already at `vSafe0V` disarms discharge (retrying
/// once on a transient error), forwards the cancelled `vSafe0V` debounce's wake lease to
/// `on_vsafe0v`, and calls `on_vsafe0v` once instead of counting a timeout.
#[fuchsia::test(logging = false)]
fn test_discharge_timeout_at_vsafe0v_calls_on_vsafe0v() {
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    // VBUS reaches `vSafe0V` this long before the discharge timeout, inside the debounce window.
    const VSAFE0V_LEAD: zx::BootDuration = zx::BootDuration::from_millis(50);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_discharge_timeout(DISCHARGE_TIMEOUT).with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
    });
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    h.tcpc.events.lock().clear();

    assert!(!h.advance(DISCHARGE_TIMEOUT - VSAFE0V_LEAD));
    h.tcpc.set_vsafe0v(true);
    let (lease, _peer) = zx::EventPair::create();
    h.vbus_alert(Some(lease)).unwrap();
    assert!(h.tcpc.is_busy());

    h.tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    assert!(h.advance(VSAFE0V_LEAD));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.is_busy());
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 0);

    // The dropped debounce never reports `vSafe0V` a second time.
    assert!(!h.advance(VSAFE0V_DEBOUNCE));
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
}

/// Verifies that stopping VBUS discharge on `discharge_timeout` after VBUS bounced above `vSafe0V`
/// cancels any in-flight `vSafe0V` debounce timer, releases its wake lease, clears `is_busy()`, and
/// resets `pending_timer()`.
#[fuchsia::test(logging = false)]
fn test_disarming_discharge_cancels_inflight_vsafe0v_timer() {
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const VSAFE0V_LEAD: zx::BootDuration = zx::BootDuration::from_millis(50);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_discharge_timeout(DISCHARGE_TIMEOUT).with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
    });
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();

    assert!(!h.advance(DISCHARGE_TIMEOUT - VSAFE0V_LEAD));
    h.tcpc.set_vsafe0v(true);
    let (lease, observer) = zx::EventPair::create();
    h.vbus_alert(Some(lease)).unwrap();
    assert!(h.tcpc.is_busy());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");

    // VBUS bounces above `vSafe0V` without another `handle_vbus_change` before `discharge_timeout`.
    h.tcpc.set_vsafe0v(false);
    assert!(h.advance(VSAFE0V_LEAD));

    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.is_busy());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert!(is_lease_closed(&observer));
}

/// Verifies that retaining an active `vSafe0V` timer preserves its existing wake lease when a
/// follow-up alert passes an un-duplicatable handle instead of clearing the slot's lease to `None`.
#[fuchsia::test(logging = false)]
fn test_vsafe0v_retain_preserves_existing_lease_when_duplication_fails() {
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);
    const HALF_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(10);

    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_vsafe0v_debounce(VSAFE0V_DEBOUNCE));
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();

    let (initial_lease, observer) = zx::EventPair::create();
    h.tcpc.set_vsafe0v(true);
    h.vbus_alert(Some(initial_lease)).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    assert!(!is_lease_closed(&observer));

    assert!(!h.advance(HALF_DEBOUNCE));
    let (raw_lease, _raw_peer) = zx::EventPair::create();
    let non_dup_lease = raw_lease.replace_handle(zx::Rights::TRANSFER).unwrap();
    h.tcpc.events.lock().clear();
    h.vbus_alert(Some(non_dup_lease)).unwrap();
    assert!(!is_lease_closed(&observer));

    h.advance_to_expiry(HALF_DEBOUNCE);
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=true"]);
    assert!(is_lease_closed(&observer));
}

/// Verifies that `Attached.SRC` will not commit while the VBUS sink path remains open: a transient
/// `write_vbus_sink(false)` failure on `AttachWait.SRC` is retried at attach, and a persistent
/// failure holds the partner in `AttachWait.SRC`.
#[fuchsia::test(logging = false)]
async fn test_source_attach_requires_closed_sink_path() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);

    // Hold a failed Sink attach so the sink path stays open in `AttachWait.SNK`.
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    tcpc.set_cc(SNK_CC1);
    assert!(tcpc.sync_initial_state(None).await.is_err());
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);

    // Persistent `write_vbus_sink(false)` failure holds the Source partner in `AttachWait.SRC`
    // and invalidates the cached `vbus_sink` state.
    tcpc.sink_writes.lock().clear();
    tcpc.set_vsafe0v(true);
    tcpc.fail_sink_write.store(true, Ordering::SeqCst);
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert!(tcpc.sink_writes.lock().is_empty());

    // Unplugging while `fail_sink_write_once` fails `Unattached`'s close still succeeds at Source
    // attach once the bus recovers.
    tcpc.fail_sink_write.store(false, Ordering::SeqCst);
    tcpc.fail_sink_write_once.store(true, Ordering::SeqCst);
    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    assert!(tcpc.sink_writes.lock().is_empty());
    tcpc.fail_sink_write_once.store(true, Ordering::SeqCst);
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
}

/// Verifies that cancelling `port.scope` while idle clears `discharge_timer` so `pending_timer`
/// returns to `"none"`.
#[fuchsia::test]
fn test_scope_cancel_clears_discharge_timer() {
    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_discharge_timeout(DEFAULT_DISCHARGE_TIMEOUT));
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    h.cancel_scope();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
}

/// Verifies that a fresh port whose sink path has never been written explicitly closes the sink
/// path (`write_vbus_sink(false)`) before committing `Attached.SRC` (TCPCI Rev 2.0 Ver 1.3 Table
/// 4-27), retrying once at attach after a transient `AttachWait.SRC` close failure and holding the
/// partner if closing keeps failing.
#[fuchsia::test(logging = false)]
async fn test_fresh_source_attach_closes_unsynced_sink_path() {
    let scope = fasync::Scope::new();

    // Direct cold Source attach without an earlier `Unattached` or Sink session writes `false`.
    let fresh = FakeTcpc::new(&scope, OPEN);
    fresh.set_vsafe0v(true);
    fresh.set_cc(SRC_CC1);
    fresh.handle_cc_status(None).await.unwrap();
    fresh.wait_for_debounce().await;
    assert_eq!(fresh.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*fresh.sink_writes.lock(), vec![false]);

    // Transient failure in `AttachWait.SRC` is retried by `attach_partner`.
    let retry_fresh = FakeTcpc::new(&scope, OPEN);
    retry_fresh.set_vsafe0v(true);
    retry_fresh.fail_sink_write_once.store(true, Ordering::SeqCst);
    retry_fresh.set_cc(SRC_CC1);
    retry_fresh.handle_cc_status(None).await.unwrap();
    retry_fresh.wait_for_debounce().await;
    assert_eq!(retry_fresh.typec_state(), TypecState::AttachedSrc);
    assert_eq!(*retry_fresh.sink_writes.lock(), vec![false]);

    // Persistent failure holds the Source partner in `AttachWait.SRC` with no confirmed sink write.
    let held_fresh = FakeTcpc::new(&scope, OPEN);
    held_fresh.set_vsafe0v(true);
    held_fresh.fail_sink_write.store(true, Ordering::SeqCst);
    held_fresh.set_cc(SRC_CC1);
    held_fresh.handle_cc_status(None).await.unwrap();
    held_fresh.wait_for_debounce().await;
    assert_eq!(held_fresh.typec_state(), TypecState::AttachWaitSrc);
    assert!(held_fresh.port.state.lock().machine.is_partner_held());
    assert!(held_fresh.sink_writes.lock().is_empty());
}

/// Verifies that `discharge_timer` holds a duplicated wake lease across the bound, handles an
/// `is_vbus_safe0v` read error by stopping discharge, re-arms the bound once (releasing the
/// initial wake lease) when both disarm attempts fail, and stops re-arming if the retry window
/// also fails on a dead bus.
#[fuchsia::test(logging = false)]
fn test_discharge_timeout_error_paths_and_wake_lease() {
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);

    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_discharge_timeout(DISCHARGE_TIMEOUT));
    h.tcpc.set_vsafe0v(false);
    let (boot_lease, boot_observer) = zx::EventPair::create();
    h.sync(Some(boot_lease)).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    // `discharge_timer` keeps the duplicated wake lease alive until the timeout fires.
    assert!(!is_lease_closed(&boot_observer));

    // 1. Both disarm attempts fail at `DISCHARGE_TIMEOUT`: discharge stays armed, the initial
    // wake lease is released, `discharge_timeout_count` is not incremented yet, and the bound
    // re-arms once.
    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 0);
    assert!(is_lease_closed(&boot_observer));

    // 2. On the retry window, an `is_vbus_safe0v` read error still stops discharge once the bus
    // write recovers, without calling `on_vsafe0v`.
    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.events.lock().clear();
    h.tcpc.set_vsafe0v_err_once("vsafe0v read failure on timeout");
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 1);
    assert!(h.tcpc.events.lock().is_empty());

    // 3. When the bus remains dead across both the initial timeout and its single retry window,
    // `TypecPort` stops re-arming `discharge_timer` rather than looping indefinitely.
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);

    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");

    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 1);
}

/// Verifies that when `handle_vbus_change` observes `vSafe0V` in `AttachWait.SRC (WaitingForVbus)`
/// and `CC_STATUS` simultaneously re-evaluates into `AttachWait.SRC` (either held on `Ra/Ra` or
/// restarting `tCCDebounce` on a pin swap) while residual discharge is armed, `vsafe0v_timer`
/// starts with the wake lease and disarms discharge without calling `on_vsafe0v`.
#[fuchsia::test]
fn test_waiting_for_vbus_src_reevaluation_arms_vsafe0v_timer() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);

    for (next_cc, expected_timer_before_vsafe0v) in [(RA_RA, "vsafe0v"), (SRC_CC2, "cc_attach")] {
        let mut h = FakeTimeHarness::new(OPEN, |p| {
            p.with_cc_debounce(CC_DEBOUNCE).with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
        });
        h.tcpc.set_vsafe0v(false);
        h.sync(None).unwrap();
        assert!(h.tcpc.is_vbus_discharge_armed());

        // Enter `AttachWait.SRC (WaitingForVbus)` with residual VBUS discharge still armed.
        h.cc_alert(SRC_CC1, None).unwrap();
        assert!(h.advance(CC_DEBOUNCE));
        assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
        assert!(h.tcpc.is_vbus_discharge_armed());

        // `vSafe0V` arrives as `CC_STATUS` changes to `next_cc`.
        h.tcpc.events.lock().clear();
        h.tcpc.set_vsafe0v(true);
        h.tcpc.set_cc(next_cc);
        let (lease, observer) = zx::EventPair::create();
        h.vbus_alert(Some(lease)).unwrap();
        assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert_eq!(h.tcpc.port.state.lock().pending_timer(), expected_timer_before_vsafe0v);
        assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
        assert!(!is_lease_closed(&observer));

        assert!(h.advance(VSAFE0V_DEBOUNCE));
        assert!(!h.tcpc.is_vbus_discharge_armed());
        assert!(h.tcpc.events.lock().is_empty());
        if next_cc == RA_RA {
            assert!(is_lease_closed(&observer));
        }
    }
}

/// Verifies that entering `Unattached` without detaching from our own session (both cold-boot
/// `sync_initial_state` and a runtime `AttachWait.SNK` CC insertion bounce on a cable with live
/// 5 V VBUS) preserves an unknown sink path until VBUS drops and avoids arming VBUS discharge
/// against live partner VBUS.
#[fuchsia::test]
async fn test_unattached_without_detach_preserves_live_vbus() {
    let scope = fasync::Scope::new();

    // 1. Fresh port with `CC == Open` or `Looking4Connection` and live 5 V VBUS does not close the
    // unknown sink path or arm discharge while VBUS stays live, then closes it once VBUS drops.
    for boot_cc in [OPEN, CcStatus::LOOKING4CONNECTION] {
        let tcpc = FakeTcpc::new(&scope, boot_cc);
        tcpc.set_vbus_level(VbusLevel::Present);
        tcpc.sync_initial_state(None).await.unwrap();
        assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
        assert!(tcpc.sink_writes.lock().is_empty());
        assert_eq!(*tcpc.discharge_writes.lock(), vec![false]);
        assert!(!tcpc.is_vbus_discharge_armed());

        tcpc.handle_vbus_change(None).await.unwrap();
        assert!(tcpc.sink_writes.lock().is_empty());

        tcpc.set_vbus_level(VbusLevel::Safe0V);
        tcpc.handle_vbus_change(None).await.unwrap();
        assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
    }

    // 2. Runtime `AttachWait.SNK` CC bounce (`SnkDefault -> Open`) while live 5 V VBUS is already
    // on the connector (e.g. legacy Type-A-to-C cable whose VBUS pins mated before CC settled)
    // exits `tPDDebounce` to `Unattached` without arming discharge onto partner VBUS.
    let tcpc = FakeTcpc::new(&scope, OPEN);
    tcpc.set_vbus_level(VbusLevel::Present);
    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);

    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;

    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![false]);
    assert!(tcpc.sink_writes.lock().is_empty());
}

/// Verifies that detaching from `Attached.SRC` while our own 5 V VBUS rail is still decaying above
/// `vSafe0V` arms VBUS discharge, preserves discharge and its original `discharge_timer` deadline
/// across an `AttachWait.SRC` CC contact bounce even while our decaying rail still reads
/// `VbusPresent` (`> 4.0 V`), and disarms discharge with `on_vsafe0v` once `vSafe0V` is debounced.
#[fuchsia::test]
fn test_source_detach_discharges_vbus_until_vsafe0v() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const BOUNCE_AT: zx::BootDuration = zx::BootDuration::from_millis(200);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(h.tcpc.vbus_sourced.load(Ordering::SeqCst));
    assert_eq!(*h.tcpc.sink_writes.lock(), vec![false]);

    // Unplug the Sink while our 5 V rail is still above `VbusPresent` (`> 4.0 V`): because
    // `detached_from == Some(PowerRole::Source)`, `prepare_unattached` arms VBUS discharge to bleed
    // our own rail down to `vSafe0V`.
    h.tcpc.events.lock().clear();
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    let (detach_lease, detach_observer) = zx::EventPair::create();
    h.cc_alert(OPEN, Some(detach_lease)).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(!h.tcpc.vbus_sourced.load(Ordering::SeqCst));
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert_eq!(*h.tcpc.events.lock(), vec!["detached:lease=true"]);
    assert!(!is_lease_closed(&detach_observer));

    // A CC contact bounce through `AttachWait.SRC` (`open -> src_cc1 -> open`) while our decaying
    // 5 V rail still reads `VbusPresent` (`> 4.0 V`) keeps discharge armed and preserves the
    // running `discharge_timer` wake lease rather than mistaking our own rail for partner VBUS.
    assert!(!h.advance(BOUNCE_AT));
    for bounce_cc in [SRC_CC1, OPEN] {
        h.cc_alert(bounce_cc, None).unwrap();
    }
    assert_eq!(h.tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(!is_lease_closed(&detach_observer));

    // Once VBUS decays below `vSafe0V`, `handle_vbus_change` debounces `vSafe0V`, disarms
    // discharge, clears `discharge_timer`, invokes `on_vsafe0v`, and releases both wake leases.
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    let (vsafe_lease, vsafe_observer) = zx::EventPair::create();
    h.vbus_alert(Some(vsafe_lease)).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    assert!(h.tcpc.port.state.lock().runner.discharge_timer.is_active());

    assert!(h.advance(VSAFE0V_DEBOUNCE));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(*h.tcpc.events.lock(), vec!["detached:lease=true", "vsafe0v:lease=true"]);
    assert!(is_lease_closed(&detach_observer));
    assert!(is_lease_closed(&vsafe_observer));
}

/// Verifies that failed `write_vbus_discharge` and `write_vbus_sink` writes invalidate cached
/// confirmation in both directions so subsequent opposite writes are never skipped.
#[fuchsia::test(logging = false)]
async fn test_power_path_write_error_invalidates_cache() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);

    // 1. A failed disarm from `Enabled { confirmed: true }` leaves `Enabled { confirmed: false }`,
    // which keeps `is_vbus_discharge_armed()` true and still re-issues
    // `write_vbus_discharge(true)` when bouncing back to `Unattached`.
    tcpc.set_vsafe0v(false);
    tcpc.sync_initial_state(None).await.unwrap();
    assert!(tcpc.is_vbus_discharge_armed());
    tcpc.discharge_writes.lock().clear();

    let gate = tcpc.port.debounce_gate().lock().await;
    tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.is_vbus_discharge_armed());
    assert!(tcpc.discharge_writes.lock().is_empty());

    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    drop(gate);
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
    assert!(tcpc.is_vbus_discharge_armed());
    assert_eq!(*tcpc.discharge_writes.lock(), vec![true]);

    // 2. Transient `write_vbus_sink(true)` failure retries once and attaches; persistent failure
    // invalidates `vbus_sink` to `Unknown` and holds the partner so `Unattached` at `vSafe0V`
    // still issues `write_vbus_sink(false)`.
    tcpc.set_vsafe0v(true);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
    tcpc.sink_writes.lock().clear();

    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.fail_sink_write_once.store(true, Ordering::SeqCst);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
    tcpc.sink_writes.lock().clear();

    tcpc.set_vbus_level(VbusLevel::Safe0V);
    tcpc.set_cc(OPEN);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
    tcpc.sink_writes.lock().clear();

    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.fail_sink_write.store(true, Ordering::SeqCst);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.fail_sink_write.store(false, Ordering::SeqCst);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Unknown);
    assert!(tcpc.sink_writes.lock().is_empty());

    tcpc.set_vbus_level(VbusLevel::Safe0V);
    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(*tcpc.sink_writes.lock(), vec![false]);

    // 3. Failed `write_vbus_sink(false)` from `Enabled { confirmed: true }` records
    // `Enabled { confirmed: false }` (so Inspect keeps `vbus_sink_enabled: true`), yet re-attaching
    // as a Sink still issues `write_vbus_sink(true)` and a subsequent successful close clears
    // `vbus_sink_enabled` to `false`.
    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.sink_writes.lock().clear();

    tcpc.fail_sink_write.store(true, Ordering::SeqCst);
    tcpc.set_vbus_level(VbusLevel::Safe0V);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    tcpc.fail_sink_write.store(false, Ordering::SeqCst);
    assert_eq!(
        tcpc.port.state.lock().runner.hw.vbus_sink,
        PowerPathState::Enabled { confirmed: false }
    );
    assert!(tcpc.sink_writes.lock().is_empty());
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        vbus_sink_enabled: true,
    });

    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);

    tcpc.set_vbus_level(VbusLevel::Safe0V);
    tcpc.set_cc(OPEN);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        vbus_sink_enabled: false,
    });
}

/// Verifies that a held Sink partner whose sink path is open (`vbus_sink == Enabled`) keeps its
/// sink path open across a CC dropout (`SNK.Open` for `tPDDebounce`) while partner VBUS stays live,
/// and closes the path once a later VBUS alert sees VBUS gone.
#[fuchsia::test(logging = false)]
fn test_held_sink_open_path_preserved_on_live_vbus_cc_dropout() {
    let mut h = FakeTimeHarness::new(SNK_CC1, |p| p.with_pd_debounce(DEFAULT_T_PD_DEBOUNCE));
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.tcpc.fail_attach.store(true, Ordering::SeqCst);
    assert!(h.sync(None).is_err());
    h.tcpc.fail_attach.store(false, Ordering::SeqCst);
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(h.tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(*h.tcpc.sink_writes.lock(), vec![true]);

    // Ground offset pulls CC to `SNK.Open` for `tPDDebounce` while 5 V VBUS remains live:
    // `Unattached` must not send `DisableSinkVbus` (`write_vbus_sink(false)`) or arm discharge.
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.advance(DEFAULT_T_PD_DEBOUNCE));

    assert_eq!(h.tcpc.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(*h.tcpc.sink_writes.lock(), vec![true]);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);

    // A VBUS alert while VBUS is still live leaves the sink path open; once VBUS drops, the next
    // VBUS alert closes it.
    h.vbus_alert(None).unwrap();
    assert_eq!(*h.tcpc.sink_writes.lock(), vec![true]);

    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(*h.tcpc.sink_writes.lock(), vec![true, false]);
}

/// Verifies that a held Sink partner whose sink path stayed open keeps that path while partner
/// VBUS stays live, closes it when a VBUS alert observes `vSafe0V` with `Rp` still asserted, and
/// remains held without re-attaching if VBUS returns.
#[fuchsia::test(logging = false)]
async fn test_held_sink_closes_sink_path_on_vbus_loss_with_rp_present() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, SNK_CC1);
    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.fail_attach.store(true, Ordering::SeqCst);
    assert!(tcpc.sync_initial_state(None).await.is_err());
    tcpc.fail_attach.store(false, Ordering::SeqCst);
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
    tcpc.events.lock().clear();

    // A VBUS alert while VBUS remains present leaves the open sink path untouched.
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(*tcpc.sink_writes.lock(), vec![true]);

    // When VBUS drops to `vSafe0V` while `Rp` stays present, the held Sink closes its sink path
    // without releasing the hold.
    tcpc.set_vbus_level(VbusLevel::Safe0V);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(*tcpc.sink_writes.lock(), vec![true, false]);
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(tcpc.port.state.lock().machine.is_partner_held());

    // VBUS returning on the held Sink does not re-attach or re-open the sink path.
    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.handle_vbus_change(None).await.unwrap();
    assert_eq!(*tcpc.sink_writes.lock(), vec![true, false]);
    assert!(!tcpc.is_connected());
    assert!(tcpc.events.lock().is_empty());
}

/// Verifies that entering `AttachWait.SNK` cancels any pending `vSafe0V` timer and releases its
/// wake lease even when `write_vbus_discharge(false)` fails (`Enabled { confirmed: false }`), so an
/// already-queued `TimerExpired { kind: Vsafe0v }` drops as stale without firing `on_vsafe0v`.
#[fuchsia::test(logging = false)]
fn test_queued_vsafe0v_expiry_clears_slot_after_failed_disarm() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(15);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE).with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
    });
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.set_vsafe0v(true);
    let (vsafe_lease, observer) = zx::EventPair::create();
    h.vbus_alert(Some(vsafe_lease)).unwrap();
    assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());

    // Pause `handle_cc_status` inside `read_cc_status` while `Unattached` still permits `vSafe0V`
    // disarm, then wake `vsafe0v_timer` so `Command::TimerExpired { kind: Vsafe0v }` queues before
    // `AttachWait.SNK` cancels the slot.
    let tcpc = Arc::clone(&h.tcpc);
    let (cc_hold, cc_started) = h.run(tcpc.cc_read.arm());
    h.tcpc.set_cc(SNK_CC1);
    h.tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    let tcpc_cc = Arc::clone(&h.tcpc);
    let mut cc_fut = pin!(async move { tcpc_cc.handle_cc_status(None).await });
    assert_matches!(h.exec.run_until_stalled(&mut cc_fut), Poll::Pending);
    assert_matches!(h.exec.run_until_stalled(&mut pin!(cc_started)), Poll::Ready(Ok(())));

    assert!(h.advance(VSAFE0V_DEBOUNCE));
    assert_matches!(h.exec.run_until_stalled(&mut cc_fut), Poll::Pending);

    // Releasing `cc_hold` enters `AttachWait.SNK` (cancelling `vsafe0v_timer` even though disarm
    // fails), so popping the queued `Vsafe0v` expiry sees `take_if_id == None`.
    drop(cc_hold);
    assert_matches!(h.exec.run_until_stalled(&mut cc_fut), Poll::Ready(Ok(())));
    h.flush();

    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    assert!(h.tcpc.events.lock().is_empty());
    assert!(is_lease_closed(&observer));
}

/// Verifies that `UnattachedPowerPlan::decide` closes the sink path and disarms discharge at
/// `vSafe0V`, preserves the sink path and disarms discharge on live partner VBUS, and closes the
/// sink path while arming discharge for residual session/confirmed discharge VBUS or the
/// `vSafe0V..vVBUSPresent` gap.
#[fuchsia::test]
fn test_unattached_power_plan_truth_table() {
    assert_eq!(
        UnattachedPowerPlan::decide(UnattachedVbus::Safe0v),
        UnattachedPowerPlan { close_sink: true, arm_discharge: false },
    );
    assert_eq!(
        UnattachedPowerPlan::decide(UnattachedVbus::PartnerLive),
        UnattachedPowerPlan { close_sink: false, arm_discharge: false },
    );
    for vbus in [UnattachedVbus::Residual, UnattachedVbus::SubPresentGap] {
        assert_eq!(
            UnattachedPowerPlan::decide(vbus),
            UnattachedPowerPlan { close_sink: true, arm_discharge: true },
        );
    }
}

/// Verifies that at `tCCDebounce` expiry in `AttachWait.SRC`, a `read_cc_status` error preserves
/// an active `vsafe0v_timer` so it still disarms discharge after `vsafe0v_debounce`, whereas
/// sampling `is_vbus_safe0v() == false` cancels `vsafe0v_timer` and releases its wake lease
/// immediately.
#[fuchsia::test(logging = false)]
fn test_attach_wait_src_expiry_vsafe0v_timer_handling() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(40);
    const ALERT_AT: zx::BootDuration = zx::BootDuration::from_millis(80);
    const CC_EXPIRY_REMAINING: zx::BootDuration =
        zx::BootDuration::from_nanos(CC_DEBOUNCE.into_nanos() - ALERT_AT.into_nanos());
    const VSAFE0V_REMAINING: zx::BootDuration = zx::BootDuration::from_nanos(
        VSAFE0V_DEBOUNCE.into_nanos() - CC_EXPIRY_REMAINING.into_nanos(),
    );

    let stage_src_debounce_with_vsafe0v = || {
        let mut h = FakeTimeHarness::new(OPEN, |p| {
            p.with_cc_debounce(CC_DEBOUNCE).with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
        });
        h.tcpc.set_vsafe0v(false);
        h.sync(None).unwrap();
        h.cc_alert(SRC_CC1, None).unwrap();
        assert!(!h.advance(ALERT_AT));

        h.tcpc.set_vsafe0v(true);
        let (lease, observer) = zx::EventPair::create();
        h.vbus_alert(Some(lease)).unwrap();
        assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
        (h, observer)
    };

    // 1. A `read_cc_status` error on `tCCDebounce` expiry stays in states permitting `vSafe0V`
    // disarm across `on_unattached` recovery, so `vsafe0v_timer` and its wake lease remain active
    // and still disarm discharge at `VSAFE0V_DEBOUNCE`.
    {
        let (mut recovery, observer) = stage_src_debounce_with_vsafe0v();

        recovery.tcpc.set_cc_err_once("transient cc read error on attach debounce");
        recovery.advance_to_expiry(CC_EXPIRY_REMAINING);
        assert_eq!(recovery.tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert!(recovery.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
        assert!(recovery.tcpc.is_vbus_discharge_armed());
        assert!(!is_lease_closed(&observer));

        recovery.advance_to_expiry(VSAFE0V_REMAINING);
        assert!(!recovery.tcpc.is_vbus_discharge_armed());
        assert!(!recovery.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
        assert!(is_lease_closed(&observer));
    }

    // 2. Sampling `is_vbus_safe0v() == false` at `tCCDebounce` expiry cancels any in-flight
    // `vsafe0v_timer` immediately and releases its wake lease without disarming discharge.
    {
        let (mut bounced, observer) = stage_src_debounce_with_vsafe0v();

        // VBUS rises back above `vSafe0V` before `tCCDebounce` expires without a `VbusChange` alert.
        bounced.tcpc.set_vsafe0v(false);
        bounced.advance_to_expiry(CC_EXPIRY_REMAINING);
        assert_eq!(bounced.tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert!(!bounced.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
        assert!(bounced.tcpc.is_vbus_discharge_armed());
        assert!(!bounced.tcpc.is_busy());
        assert!(is_lease_closed(&observer));
        assert!(!bounced.advance(VSAFE0V_REMAINING));
        assert!(bounced.tcpc.is_vbus_discharge_armed());
    }
}

/// Verifies that `with_discharge_timeout(zx::BootDuration::ZERO)` fires immediately rather than
/// disabling the safety bound.
#[fuchsia::test]
fn test_zero_discharge_timeout_fires_immediately() {
    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_discharge_timeout(zx::BootDuration::ZERO));
    h.tcpc.set_vsafe0v(false);
    h.sync(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 1);
}

/// Verifies that `Unattached` VBUS read-error fallbacks arm discharge when `is_vbus_safe0v()`
/// fails without live VBUS, assume live VBUS (never arming discharge) when `is_vbus_present()`
/// fails, and still run the `vSafe0V` debounce after an `is_vbus_present()` failure in
/// `poll_unattached_vbus`.
#[fuchsia::test(logging = false)]
async fn test_unattached_vbus_read_error_fallbacks() {
    let scope = fasync::Scope::new();

    // 1. `is_vbus_safe0v()` error in `prepare_unattached` closes the sink path and arms discharge.
    {
        let tcpc = FakeTcpc::new(&scope, OPEN);
        tcpc.set_vbus_level(VbusLevel::Safe0V);
        tcpc.set_vsafe0v_err_once("transient vsafe0v read error on unattached");
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(tcpc.is_vbus_discharge_armed());
        assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
        assert_eq!(*tcpc.discharge_writes.lock(), vec![true]);
    }

    // 2a. `is_vbus_present()` error in `prepare_unattached` with `vbus_sink == Disabled` assumes
    // live partner VBUS and does not arm discharge.
    {
        let tcpc = FakeTcpc::new(&scope, OPEN);
        tcpc.set_vbus_level(VbusLevel::Safe0V);
        tcpc.sync_initial_state(None).await.unwrap();
        assert_eq!(*tcpc.sink_writes.lock(), vec![false]);
        assert!(!tcpc.is_vbus_discharge_armed());
        tcpc.discharge_writes.lock().clear();
        tcpc.sink_writes.lock().clear();

        tcpc.set_vbus_level(VbusLevel::Present);
        tcpc.set_vbus_present_err_once("transient vbus_present read error on unattached");
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(!tcpc.is_vbus_discharge_armed());
        assert!(tcpc.sink_writes.lock().is_empty());
        assert!(tcpc.discharge_writes.lock().is_empty());
    }

    // 2b. `is_vbus_present()` error in `prepare_unattached` when a held Sink (`Enabled`) drops
    // CC to `Open` preserves `vbus_sink == Enabled` and does not arm discharge.
    {
        let tcpc = FakeTcpc::new(&scope, SNK_CC1);
        tcpc.set_vbus_level(VbusLevel::Present);
        tcpc.fail_attach.store(true, Ordering::SeqCst);
        assert!(tcpc.sync_initial_state(None).await.is_err());
        tcpc.fail_attach.store(false, Ordering::SeqCst);
        assert!(tcpc.port.state.lock().machine.is_partner_held());
        assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
        tcpc.discharge_writes.lock().clear();
        tcpc.sink_writes.lock().clear();

        tcpc.set_cc(OPEN);
        tcpc.set_vbus_present_err_once("transient vbus_present read error on cc dropout");
        tcpc.handle_cc_status(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::UnattachedSnk);
        assert!(!tcpc.is_vbus_discharge_armed());
        assert!(tcpc.sink_writes.lock().is_empty());
        assert!(tcpc.discharge_writes.lock().is_empty());
    }

    // 3. `is_vbus_present()` error in `poll_unattached_vbus` when `vbus_sink == Unknown` and
    // discharge is armed returns `Err` while still running the `vSafe0V` debounce to disarm.
    {
        let tcpc = FakeTcpc::new(&scope, OPEN);
        tcpc.set_vbus_level(VbusLevel::Gap);
        tcpc.fail_sink_write_once.store(true, Ordering::SeqCst);
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(tcpc.sink_writes.lock().is_empty());
        assert!(tcpc.is_vbus_discharge_armed());
        tcpc.events.lock().clear();

        tcpc.set_vbus_level(VbusLevel::Safe0V);
        tcpc.set_vbus_present_err_once("transient vbus_present read error in poll_unattached_vbus");
        assert!(tcpc.handle_vbus_change(None).await.is_err());
        tcpc.wait_for_debounce().await;
        assert!(!tcpc.is_vbus_discharge_armed());
        assert_eq!(*tcpc.events.lock(), vec!["vsafe0v:lease=false"]);
    }
}

/// Verifies that persistent `write_vbus_discharge(false)` failure warns and commits Sink attach
/// (retrying disarm on later alerts) while holding Source attach until `vSafe0V` clears disarm.
#[fuchsia::test(logging = false)]
async fn test_persistent_discharge_disarm_failure_on_attach() {
    let scope = fasync::Scope::new();

    // Runtime `Gap -> Attached.SNK` warns on disarm failure, commits Sink attach, and clears
    // unconfirmed-enabled discharge via `retry_failed_disarm` on the next alert once writes recover.
    {
        let tcpc = FakeTcpc::new(&scope, OPEN);
        tcpc.set_vbus_level(VbusLevel::Gap);
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(tcpc.is_vbus_discharge_armed());
        tcpc.sink_writes.lock().clear();
        tcpc.discharge_writes.lock().clear();
        tcpc.events.lock().clear();

        tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
        tcpc.set_vbus_level(VbusLevel::Present);
        tcpc.set_cc(SNK_CC1);
        tcpc.handle_cc_status(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
        assert!(tcpc.is_connected());
        assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
        assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
        assert!(tcpc.is_vbus_discharge_armed());
        {
            let state = tcpc.port.state.lock();
            assert_eq!(state.machine.telemetry.attach_count, 1);
            assert_eq!(state.machine.telemetry.sink_attach_failure_count, 0);
        }

        tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
        tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
        tcpc.handle_cc_status(None).await.unwrap();
        assert!(tcpc.is_vbus_discharge_armed());

        tcpc.handle_vbus_change(None).await.unwrap();
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
        assert!(!tcpc.is_vbus_discharge_armed());
        assert_eq!(tcpc.discharge_writes.lock().last(), Some(&false));
    }

    // Cold-boot Sink attach with VBUS present still commits `Attached.SNK` when disarm fails.
    {
        let tcpc = FakeTcpc::new(&scope, SNK_CC1);
        tcpc.set_vbus_level(VbusLevel::Present);
        tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
        tcpc.sync_initial_state(None).await.unwrap();
        assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
        assert!(tcpc.is_connected());
        assert_eq!(*tcpc.sink_writes.lock(), vec![true]);
        assert_eq!(*tcpc.events.lock(), vec!["attached:Sink:lease=false"]);
        let state = tcpc.port.state.lock();
        assert_eq!(state.machine.telemetry.attach_count, 1);
        assert_eq!(state.machine.telemetry.sink_attach_failure_count, 0);
    }

    // Source attach while VBUS discharge is armed holds in `AttachWait.SRC` if disarm keeps
    // failing, then clears unconfirmed-enabled discharge once `vSafe0V` polling succeeds.
    {
        let tcpc = FakeTcpc::new(&scope, OPEN);
        tcpc.set_vbus_level(VbusLevel::Gap);
        tcpc.sync_initial_state(None).await.unwrap();
        assert!(tcpc.is_vbus_discharge_armed());
        tcpc.sink_writes.lock().clear();
        tcpc.discharge_writes.lock().clear();
        tcpc.events.lock().clear();

        tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
        tcpc.set_vbus_level(VbusLevel::Safe0V);
        tcpc.set_cc(SRC_CC1);
        tcpc.handle_cc_status(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert!(tcpc.port.state.lock().machine.is_partner_held());
        assert!(!tcpc.is_connected());
        assert!(tcpc.is_vbus_discharge_armed());
        assert!(!tcpc.vbus_sourced.load(Ordering::SeqCst));
        assert!(tcpc.sink_writes.lock().is_empty());
        assert!(tcpc.events.lock().is_empty());
        {
            let state = tcpc.port.state.lock();
            assert_eq!(state.machine.telemetry.attach_count, 0);
            assert_eq!(state.machine.telemetry.source_attach_failure_count, 1);
        }

        tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
        tcpc.handle_vbus_change(None).await.unwrap();
        tcpc.wait_for_debounce().await;
        assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
        assert!(tcpc.port.state.lock().machine.is_partner_held());
        assert!(!tcpc.is_vbus_discharge_armed());
        assert_eq!(tcpc.discharge_writes.lock().last(), Some(&false));
    }
}

/// Verifies that a controller whose `write_vbus_discharge(true)` returns `Ok(false)` (unsupported)
/// stays disarmed across `Unattached` and `AttachWait.SRC`, arms no discharge timer, and releases
/// the wake lease immediately.
#[fuchsia::test]
async fn test_unsupported_vbus_discharge_stays_disarmed_without_holding_lease() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new_with_port(
        TypecPort::new(scope.to_handle())
            .with_cc_debounce(zx::BootDuration::ZERO)
            .with_vsafe0v_debounce(zx::BootDuration::ZERO),
        OPEN,
    );
    tcpc.supports_vbus_discharge.store(false, Ordering::SeqCst);
    tcpc.set_vsafe0v(false);

    let (unattached_lease, unattached_observer) = zx::EventPair::create();
    tcpc.sync_initial_state(Some(unattached_lease)).await.unwrap();
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert!(is_lease_closed(&unattached_observer));

    let (wait_lease, wait_observer) = zx::EventPair::create();
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_cc_status(Some(wait_lease)).await.unwrap();
    assert!(!tcpc.port.state.lock().runner.discharge_timer.is_active());
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!tcpc.is_vbus_discharge_armed());
    assert_eq!(tcpc.port.state.lock().pending_timer(), "none");
    assert!(is_lease_closed(&wait_observer));
    assert!(tcpc.discharge_writes.lock().is_empty());
}

/// Verifies that when `vSafe0V` arrives via `handle_vbus_change(None)` while `discharge_timer`
/// holds a wake lease from Source detach, disarming discharge forwards `discharge_timer`'s wake
/// lease into `on_vsafe0v` and releases it once `on_vsafe0v` finishes.
#[fuchsia::test]
fn test_discharge_timer_wake_lease_forwarded_to_on_vsafe0v() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSrc);

    h.tcpc.events.lock().clear();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    let (detach_lease, detach_observer) = zx::EventPair::create();
    h.cc_alert(OPEN, Some(detach_lease)).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(!is_lease_closed(&detach_observer));

    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    assert!(!is_lease_closed(&detach_observer));

    assert!(h.advance(VSAFE0V_DEBOUNCE));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(*h.tcpc.events.lock(), vec!["detached:lease=true", "vsafe0v:lease=true"]);
    assert!(is_lease_closed(&detach_observer));
}

/// Verifies that if unconfirmed-enabled discharge carries `discharge_timer` into `Attached.SNK` on
/// live 5 V partner VBUS, `handle_discharge_timeout` retries disarming without sampling
/// `is_vbus_safe0v()` or incrementing `discharge_timeout_count`.
#[fuchsia::test(logging = false)]
fn test_discharge_timeout_in_attached_snk_skips_vsafe0v_and_counter() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE).with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");

    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSnk);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.set_vsafe0v_err("must not sample is_vbus_safe0v in Attached.SNK");
    assert!(h.advance(DISCHARGE_TIMEOUT - CC_DEBOUNCE));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(h.tcpc.port.state.lock().machine.telemetry.discharge_timeout_count, 0);
}

/// Verifies that a `vSafe0V` bounce re-arms VBUS discharge across `AttachWait.SRC` VBUS
/// qualification, Source `tCCDebounce` expiry (`vbus_ready == false`), and a failed re-arm write
/// (keeping `Enabled { confirmed: false }` and `discharge_timer` active until VBUS settles at
/// `vSafe0V`).
#[fuchsia::test(logging = false)]
fn test_vsafe0v_bounce_rearms_across_attach_wait_src_and_write_error() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    const VSAFE0V_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(200);

    // 1. `qualify_vbus_attach(PowerRole::Source)` bounce in `AttachWait.SRC` (`WaitingForVbus`).
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_vsafe0v_debounce(VSAFE0V_DEBOUNCE)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    h.tcpc.discharge_writes.lock().clear();

    // `vSafe0V` while `CC` switches to `RA_RA` keeps `AttachWait.SRC (WaitingForVbus)` and arms
    // `vsafe0v_timer`; bouncing back to `Gap` cancels `vsafe0v_timer` in `qualify_vbus_attach` and
    // re-arms discharge.
    h.tcpc.set_cc(RA_RA);
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    assert!(!h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);

    // 2. Source `tCCDebounce` expiry (`handle_debounce_expired` with `!vbus_ready`) cancels an
    // active `vSafe0V` timer and re-arms discharge.
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.cc_alert(OPEN, None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.cc_alert(SRC_CC1, None).unwrap();
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    assert!(!h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    h.tcpc.discharge_writes.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(!h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);

    // 3. Failed re-arm write in `AttachWait.SRC` (`RA_RA`) records `Enabled { confirmed: false }`,
    // leaves `is_vbus_discharge_armed()` true and `discharge_timer` active without attempting a
    // `false` write, and later disarms cleanly once VBUS settles at `vSafe0V`.
    h.tcpc.set_cc(RA_RA);
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.port.state.lock().runner.discharge_timer.is_active());
    assert!(!h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));
    assert!(h.tcpc.discharge_writes.lock().is_empty());

    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.port.state.lock().runner.vsafe0v_timer.is_active());
    assert!(h.advance(VSAFE0V_DEBOUNCE));
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_discharge, PowerPathState::Disabled);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);

    // 4a. If VBUS rises all the way to live 5 V partner VBUS (`VbusLevel::Present`) while
    // `vsafe0v_timer` is sleeping in `Unattached`, `rearm_vbus_discharge_after_bounce` disarms
    // VBUS discharge (without invoking `on_vsafe0v`) so level-triggered hardware removes its load.
    h.tcpc.set_cc(OPEN);
    h.tcpc.reset_to_disconnected();
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert!(h.tcpc.events.lock().is_empty());

    // 4b. In `AttachWait.SRC` (`RA_RA`), a `Safe0V -> Present` bounce skips re-arming without
    // disarming the existing discharge state.
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(h.tcpc.is_vbus_discharge_armed());
    h.tcpc.set_cc(RA_RA);
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    h.tcpc.discharge_writes.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.discharge_writes.lock().is_empty());
    assert!(!h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));

    // Return to `Unattached` with VBUS in the `Gap` band so Step 5 starts with discharge armed.
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.is_vbus_discharge_armed());

    // 5. An `is_vbus_present()` read failure during a `vSafe0V` bounce assumes live VBUS and skips
    // re-arming discharge so a Source plugging in concurrently is not clamped.
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");
    h.tcpc.discharge_writes.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.tcpc.set_vbus_present_err_once("transient vbus_present read error on bounce");
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(h.tcpc.discharge_writes.lock().is_empty());
    assert!(!h.tcpc.hw_discharge_bleeding.load(Ordering::SeqCst));

    // 6. If a prior timeout retry exhaustion left unconfirmed-enabled discharge with no active
    // deadline, a subsequent bounce re-arm (even if `write_vbus_discharge(true)` fails) arms a
    // fresh `discharge_timeout` that expires and stops discharge.
    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");

    h.tcpc.fail_discharge_write_once.store(true, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");

    // Exhaust the newly armed deadline again, then verify a succeeding bounce re-arm also arms a
    // fresh `discharge_timeout` that disarms on expiry.
    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "vsafe0v");

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: true }
    );
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    assert!(h.advance(DISCHARGE_TIMEOUT));
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
}

/// Verifies that if `write_vbus_discharge(false)` returns `Ok(true)` (still armed), `TypecPort`
/// records `PowerPathState::Enabled { confirmed: false }`, preserves `discharge_timeout` without
/// invoking `on_vsafe0v`, and recovers to `Disabled` once a subsequent disarm returns `Ok(false)`.
#[fuchsia::test(logging = false)]
fn test_disarm_reporting_still_armed_records_unconfirmed_enabled() {
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);
    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_discharge_timeout(DISCHARGE_TIMEOUT));
    h.tcpc.set_vsafe0v(false);
    h.tcpc.discharge_stuck_on.store(true, Ordering::SeqCst);

    h.sync(None).unwrap();
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: true }
    );
    assert!(h.tcpc.port.state.lock().runner.discharge_timer.is_active());

    h.tcpc.set_vsafe0v(true);
    h.vbus_alert(None).unwrap();
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.port.state.lock().runner.discharge_timer.is_active());
    assert!(h.tcpc.events.lock().is_empty());

    h.tcpc.discharge_stuck_on.store(false, Ordering::SeqCst);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_discharge, PowerPathState::Disabled);
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.port.state.lock().runner.discharge_timer.is_active());
    assert_eq!(*h.tcpc.events.lock(), vec!["vsafe0v:lease=false"]);
}

/// Verifies that when disarming VBUS discharge fails on entering `AttachWait.SNK`
/// (`Enabled { confirmed: false }`) and CC later drops out (`tPDDebounce` -> `Unattached`) while
/// 5 V partner VBUS remains live, `Unattached` re-checks `is_vbus_present()`, preserves any open
/// or unknown sink path, and retries disarming (including across follow-up `vbus_alert` calls if
/// the `Unattached` disarm itself fails) instead of clamping live partner VBUS.
#[fuchsia::test(logging = false)]
fn test_failed_disarm_cc_dropout_on_live_vbus_preserves_sink_and_retries_disarm() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const PD_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(20);

    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE).with_pd_debounce(PD_DEBOUNCE)
    });

    // 1. Failed disarm on entering `AttachWait.SNK` followed by CC dropout while VBUS is `Present`
    // and `vbus_sink == Unknown`.
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.tcpc.fail_sink_write_once.store(true, Ordering::SeqCst);
    h.sync(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Unknown);
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.sink_writes.lock().clear();
    h.tcpc.discharge_writes.lock().clear();
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.advance(PD_DEBOUNCE));
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.sink_writes.lock().is_empty());
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Unknown);
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert!(!h.tcpc.is_vbus_discharge_armed());

    // 2. Held Sink with open sink path (`Enabled { confirmed: true }`) and unconfirmed discharge:
    // CC dropout on live 5 V preserves the open sink path and retries disarming.
    h.tcpc.reset_to_disconnected();
    h.tcpc.set_cc(OPEN);
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    h.tcpc.fail_attach.store(true, Ordering::SeqCst);
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert!(h.tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_sink,
        PowerPathState::Enabled { confirmed: true }
    );
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.fail_attach.store(false, Ordering::SeqCst);
    h.tcpc.sink_writes.lock().clear();
    h.tcpc.discharge_writes.lock().clear();
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.advance(PD_DEBOUNCE));
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.sink_writes.lock().is_empty());
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_sink,
        PowerPathState::Enabled { confirmed: true }
    );
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert!(!h.tcpc.is_vbus_discharge_armed());

    // 3. If the `Unattached` disarm itself fails on live VBUS (`Enabled { confirmed: false }`),
    // `vbus_seen_below_present` stays `false` while follow-up `vbus_alert` calls keep retrying
    // until it succeeds (reusing a failed `has_deferred_open_sink()` presence read rather than
    // issuing a second bus read on the same alert).
    h.tcpc.reset_to_disconnected();
    h.tcpc.set_cc(OPEN);
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.sync(None).unwrap();
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Unknown);

    h.tcpc.fail_sink_write.store(true, Ordering::SeqCst);
    h.cc_alert(SRC_CC1, None).unwrap();
    h.cc_alert(OPEN, None).unwrap();
    h.tcpc.fail_sink_write.store(false, Ordering::SeqCst);
    assert!(h.tcpc.typec_state().is_unattached());
    assert_eq!(h.tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Unknown);
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.port.state.lock().runner.hw.vbus_seen_below_present);

    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.sink_writes.lock().clear();
    h.tcpc.discharge_writes.lock().clear();
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.advance(PD_DEBOUNCE));
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(!h.tcpc.port.state.lock().runner.hw.vbus_seen_below_present);
    assert!(h.tcpc.sink_writes.lock().is_empty());

    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.set_vbus_present_err_once("transient vbus_present read error");
    assert!(h.vbus_alert(None).is_err());
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
}

/// Verifies that when VBUS discharge is armed in `Unattached` in the `vSafe0V..vVBUSPresent` gap
/// (or after a Sink VBUS-loss detach into the gap, or after a Source-detach rail decays below
/// `vVBUSPresent`) and 5 V partner VBUS arrives without dipping to `vSafe0V`, `handle_vbus_change`
/// disarms discharge without firing `on_vsafe0v`, while keeping an initial `> 4 V` Source-detach
/// decay or transient `is_vbus_present()` read error armed.
#[fuchsia::test(logging = false)]
fn test_live_vbus_arrival_during_gap_discharge_disarms_without_vsafe0v_dip() {
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);
    const DISCHARGE_TIMEOUT: zx::BootDuration = zx::BootDuration::from_millis(650);

    // 1. Direct `Gap -> Present` arrival without dipping to `vSafe0V`.
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "discharge_timeout");
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert!(h.tcpc.events.lock().is_empty());

    // 2. Detaching from `Attached.SRC` at `Present` keeps discharge armed until VBUS drops below
    // `vVBUSPresent`, then disarms if external `Present` VBUS arrives without dipping to `vSafe0V`.
    drop(h);
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.sync(None).unwrap();
    h.cc_alert(SRC_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSrc);
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.cc_alert(OPEN, None).unwrap();
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);

    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);

    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert!(h.tcpc.events.lock().is_empty());

    // 3. Detaching from `Attached.SNK` on VBUS loss directly into `Gap` marks
    // `vbus_seen_below_present` so external `Present` VBUS arriving without a `vSafe0V` dip
    // disarms discharge.
    drop(h);
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Safe0V);
    h.sync(None).unwrap();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachedSnk);
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();

    h.tcpc.set_cc(OPEN);
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.typec_state().is_unattached());
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.port.state.lock().runner.hw.vbus_seen_below_present);
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![true]);

    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.hw_discharge.lock(), Some(false));
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert!(h.tcpc.events.lock().is_empty());

    // 4. An `is_vbus_present()` error in `check_unattached_discharge_live_vbus` keeps the bounded
    // bleed armed and retries on the next alert.
    drop(h);
    let mut h = FakeTimeHarness::new(OPEN, |p| {
        p.with_cc_debounce(CC_DEBOUNCE)
            .with_src_disconnect(zx::BootDuration::ZERO)
            .with_discharge_timeout(DISCHARGE_TIMEOUT)
    });
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.port.state.lock().runner.hw.vbus_seen_below_present);
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.events.lock().clear();

    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.tcpc.set_vbus_present_err_once("transient vbus_present error during gap discharge");
    h.vbus_alert(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());
    assert!(h.tcpc.discharge_writes.lock().is_empty());

    h.vbus_alert(None).unwrap();
    assert!(!h.tcpc.is_vbus_discharge_armed());
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert!(h.tcpc.events.lock().is_empty());
}

/// Verifies that when `handle_vbus_change` in `AttachWait.SNK` (`WaitingForVbus`) sees CC flip
/// pins and restarts `tCCDebounce` (`Step::Advanced(None)`), it still retries a prior failed VBUS
/// discharge disarm.
#[fuchsia::test(logging = false)]
fn test_waiting_for_vbus_cc_redebounce_retries_failed_disarm() {
    const SNK_CC2: CcStatus = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    const CC_DEBOUNCE: zx::BootDuration = zx::BootDuration::from_millis(100);

    let mut h = FakeTimeHarness::new(OPEN, |p| p.with_cc_debounce(CC_DEBOUNCE));
    h.tcpc.set_vbus_level(VbusLevel::Gap);
    h.sync(None).unwrap();
    assert!(h.tcpc.is_vbus_discharge_armed());

    h.tcpc.fail_discharge_write.store(true, Ordering::SeqCst);
    h.cc_alert(SNK_CC1, None).unwrap();
    assert!(h.advance(CC_DEBOUNCE));
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "none");
    assert_eq!(
        h.tcpc.port.state.lock().runner.hw.vbus_discharge,
        PowerPathState::Enabled { confirmed: false }
    );

    h.tcpc.fail_discharge_write.store(false, Ordering::SeqCst);
    h.tcpc.discharge_writes.lock().clear();
    h.tcpc.set_cc(SNK_CC2);
    h.tcpc.set_vbus_level(VbusLevel::Present);
    h.vbus_alert(None).unwrap();
    assert_eq!(h.tcpc.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(h.tcpc.port.state.lock().pending_timer(), "cc_attach");
    assert_eq!(*h.tcpc.discharge_writes.lock(), vec![false]);
    assert!(!h.tcpc.is_vbus_discharge_armed());
}

/// Verifies that `write_vbus_sink` returning `Ok(false)` (externally managed) records
/// `PowerPathState::Disabled` (`vbus_sink_enabled: false` in Inspect) and skips redundant closes,
/// while `write_vbus_sink(false)` returning `Ok(true)` (still open) records
/// `PowerPathState::Enabled { confirmed: false }` (`vbus_sink_enabled: true`) and holds `Attached.SRC`.
#[fuchsia::test(logging = false)]
async fn test_write_vbus_sink_unsupported_and_stuck_open() {
    let scope = fasync::Scope::new();
    let tcpc = FakeTcpc::new(&scope, OPEN);

    // 1. Externally managed sink path (`Ok(false)`): Sink attach commits with `Disabled` cache and
    // `vbus_sink_enabled: false`, detach skips redundant `write_vbus_sink(false)`, and Source
    // attach commits normally.
    tcpc.supports_vbus_sink.store(false, Ordering::SeqCst);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    assert_eq!(tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Disabled);
    assert!(tcpc.sink_writes.lock().is_empty());
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        vbus_sink_enabled: false,
    });

    tcpc.set_vsafe0v(true);
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert!(tcpc.sink_writes.lock().is_empty());

    // 2. Managed sink path whose close reports `Ok(true)` (still open): records
    // `Enabled { confirmed: false }`, keeps `vbus_sink_enabled: true`, and holds the Source
    // partner in `AttachWait.SRC` until a later close succeeds.
    tcpc.reset_to_disconnected();
    tcpc.set_vbus_level(VbusLevel::Present);
    tcpc.set_cc(SNK_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSnk);
    tcpc.sink_writes.lock().clear();

    tcpc.sink_stuck_open.store(true, Ordering::SeqCst);
    tcpc.set_vsafe0v(true);
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_vbus_change(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachWaitSrc);
    assert!(tcpc.port.state.lock().machine.is_partner_held());
    assert_eq!(tcpc.port.state.lock().machine.telemetry.source_attach_failure_count, 1);
    assert_eq!(
        tcpc.port.state.lock().runner.hw.vbus_sink,
        PowerPathState::Enabled { confirmed: false }
    );
    assert_eq!(*tcpc.sink_writes.lock(), vec![false; 5]);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        vbus_sink_enabled: true,
    });

    tcpc.sink_stuck_open.store(false, Ordering::SeqCst);
    tcpc.sink_writes.lock().clear();
    tcpc.set_cc(OPEN);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.set_cc(SRC_CC1);
    tcpc.handle_cc_status(None).await.unwrap();
    tcpc.wait_for_debounce().await;
    assert_eq!(tcpc.typec_state(), TypecState::AttachedSrc);
    assert_eq!(tcpc.port.state.lock().runner.hw.vbus_sink, PowerPathState::Disabled);
    let inspector = fuchsia_inspect::Inspector::default();
    tcpc.record_typec_inspect(inspector.root());
    assert_data_tree!(inspector, root: contains {
        vbus_sink_enabled: false,
    });
}
