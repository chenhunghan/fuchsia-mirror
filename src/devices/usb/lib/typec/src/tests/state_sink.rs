// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::SinkStage;
use crate::cc::{CcPinState, CcStatus, PowerRole, RpValue};
use crate::state::{DebounceTimer, Effect, Event, Machine, Phase, Step};
use fidl_next_fuchsia_hardware_usb_connector::Orientation;

/// Verifies that Sink `Rp` rises require every sample across `tRpValueChange` to match `candidate`
/// (`SNK.Open` or a different higher level cancels or restarts the window, while expiry commits and
/// increments `rp_change_count`).
#[fuchsia::test]
fn test_sink_rp_rise_debounce() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let open = CcStatus::default();
    let attached = |rp, stage| Phase::AttachedSnk { cc_status: snk_cc1, rp, stage };
    let rise = |candidate| SinkStage::DebouncingRpRise { candidate };

    let mut machine = Machine::new();
    machine.cc_status = snk_cc1;
    machine.set_phase(attached(RpValue::Default, SinkStage::Steady));
    assert_eq!(machine.settle_rp_debounce_read_error(), Step::Absorbed);

    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Default, rise(snk_cc1_1a5)));
    assert_eq!(machine.settle_rp_debounce_read_error(), Step::Advanced(None));
    assert_eq!(machine.visible_cc_status(), snk_cc1_1a5);
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));

    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(machine.debounce_timer(), Some(DebounceTimer::RpValueChange));
    assert_eq!(machine.uncommitted_debounce_direction(), None);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Absorbed);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Default, rise(snk_cc1_3a0)));
    assert_eq!(machine.transition(Event::CcChanged(open)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));

    // A different higher level seen only at expiry restarts the rise window instead of committing.
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(machine.phase, attached(RpValue::Default, rise(snk_cc1_3a0)));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp3_0A)))
    );
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, SinkStage::Steady));
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert_eq!(machine.telemetry.rp_change_count, 1);
}

/// Verifies that Sink `Rp` falls cancel only when CC returns `>= committed`, keep the timer running
/// across `SNK.Open` chatter and lower-level steps so `tSinkAdj` is never starved, and re-arm
/// `DebouncingRpRise` when expiry reads an intermediate level above `floor` (R2.5 §4.5.2.3.2.2,
/// §4.5.2.3.3.2, Table 4-32).
#[fuchsia::test]
fn test_sink_rp_fall_debounce_and_chatter() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let open = CcStatus::default();
    let attached = |rp, stage| Phase::AttachedSnk { cc_status: snk_cc1_3a0, rp, stage };
    let rise = |candidate| SinkStage::DebouncingRpRise { candidate };
    let fall = |floor| SinkStage::DebouncingRpFall { floor };

    let mut machine = Machine::new();
    machine.cc_status = snk_cc1_3a0;
    machine.set_phase(attached(RpValue::Rp3_0A, SinkStage::Steady));

    // Returning `>= committed` (via `CcChanged` or at `DebounceExpired`) cancels the fall.
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, fall(RpValue::Default)));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, SinkStage::Steady));
    assert_eq!(machine.debounce_timer(), None);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, SinkStage::Steady));

    // Stepping `3.0A -> 1.5A -> Default` and flapping `SNK.Open` lowers `floor` without restarting
    // the timer (`Step::Absorbed`), and commits `floor` even if CC reads `Open` at expiry.
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, fall(RpValue::Rp1_5A)));
    assert_eq!(machine.transition(Event::CcChanged(open)), Step::Absorbed);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Absorbed);
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, fall(RpValue::Default)));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Absorbed);
    assert_eq!(machine.phase, attached(RpValue::Rp3_0A, fall(RpValue::Default)));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Default)))
    );
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(machine.telemetry.rp_change_count, 1);

    // If a fall dipped to `Default` (`floor`) and expiry reads `1.5A` (`floor < 1.5A < 3.0A`),
    // `floor` commits immediately and `1.5A` enters `DebouncingRpRise`.
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp3_0A)))
    );
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_1a5, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Default)))
    );
    assert_eq!(machine.phase, attached(RpValue::Default, rise(snk_cc1_1a5)));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_1a5, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp1_5A)))
    );
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp1_5A));

    // Cross-stage switches from committed `1.5A`: a pending rise (`3.0A`) switches directly into
    // `DebouncingRpFall` on `Default`, and a pending fall (`Default`) switches directly into
    // `DebouncingRpRise` on `3.0A` (both via `CcChanged` and at `DebounceExpired`).
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp1_5A, rise(snk_cc1_3a0)));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp1_5A, fall(RpValue::Default)));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(machine.phase, attached(RpValue::Rp1_5A, rise(snk_cc1_3a0)));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp1_5A, fall(RpValue::Default)));
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    assert_eq!(machine.phase, attached(RpValue::Rp1_5A, rise(snk_cc1_3a0)));

    // `RpValue`'s derived `Ord` matches increasing pull-up strength, and an opposite-pin `Rp` at
    // fall expiry is treated as lost `Rp` on the monitored pin (`floor` commits).
    assert!(RpValue::Default < RpValue::Rp1_5A && RpValue::Rp1_5A < RpValue::Rp3_0A);
    let snk_cc2_3a0 = CcStatus::new(CcPinState::Open, CcPinState::SnkPower3_0);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc2_3a0, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Default)))
    );
    assert_eq!(machine.phase, attached(RpValue::Default, SinkStage::Steady));
}

/// Verifies that `cancel_debounce` settles a pending Sink `Rp` window to `Steady` while keeping the
/// committed `Rp` level and leaving the most recent `cc_status` sample untouched.
#[fuchsia::test]
fn test_sink_rp_cancel_debounce_preserves_cc_status() {
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let open = CcStatus::default();

    let mut machine = Machine::new();
    machine.cc_status = snk_cc1_1a5;
    machine.set_phase(Phase::AttachedSnk {
        cc_status: snk_cc1_1a5,
        rp: RpValue::Rp1_5A,
        stage: SinkStage::Steady,
    });

    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Advanced(None));
    machine.cancel_debounce();
    assert_eq!(
        machine.phase,
        Phase::AttachedSnk {
            cc_status: snk_cc1_1a5,
            rp: RpValue::Rp1_5A,
            stage: SinkStage::Steady,
        }
    );
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp1_5A));
    assert_eq!(machine.visible_cc_status(), snk_cc1_3a0);
    assert_eq!(machine.telemetry.debounce_cancelled_count, 0);

    assert_eq!(machine.transition(Event::CcChanged(open)), Step::Absorbed);
    assert_eq!(machine.visible_cc_status(), open);
    machine.cancel_debounce();
    assert_eq!(machine.visible_cc_status(), open);
}

/// Verifies that in `DebugAccessory.SNK`, `Rp` rises require exact pin-pair stability, attach-time
/// `orientation()` comes from the committed pre-rise sample (matching steps just before and just
/// after `tCCDebounce`), and post-attach DTS `Rp` changes never flip `orientation()` (R2.5
/// §B.2.4.2.2.2, §B.2.4.2.4.2, §B.2.6.1.1).
#[fuchsia::test]
fn test_dts_sink_rp_changes_preserve_attached_orientation() {
    let dts_3a0_normal = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let dts_3a0_reverse = CcStatus::new(CcPinState::SnkDefault, CcPinState::SnkPower3_0);
    let dts_1a5_normal = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::SnkDefault);
    let dts_1a5_reverse = CcStatus::new(CcPinState::SnkDefault, CcPinState::SnkPower1_5);
    let dts_def_equal = CcStatus::new(CcPinState::SnkDefault, CcPinState::SnkDefault);
    let dts_def_mixed = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkPower1_5);

    // 1. A DTS stepping `Default/Default -> 3.0/Default` just before `tCCDebounce` expires gives
    // the same `orientation()` (`None`) and `Rp` progression (`Default -> 3.0A`) as stepping just
    // after `tCCDebounce` expires, and an un-debounced `Rp/Rp` glitch at expiry never flips an
    // oriented (`3.0/1.5`, `Normal`) attachment.
    let mut step_before = Machine::new();
    step_before.sink_debug_accessory_mode = true;
    assert_eq!(
        step_before.transition(Event::CcChanged(dts_def_equal)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(step_before.transition(Event::CcChanged(dts_3a0_normal)), Step::Absorbed);
    assert_eq!(
        step_before.transition(Event::DebounceExpired { cc: dts_3a0_normal, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(step_before.orientation(), None);
    assert_eq!(step_before.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(
        step_before.transition(Event::DebounceExpired { cc: dts_3a0_normal, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp3_0A)))
    );

    let mut step_after = Machine::new();
    step_after.sink_debug_accessory_mode = true;
    assert_eq!(
        step_after.transition(Event::CcChanged(dts_def_equal)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        step_after.transition(Event::DebounceExpired { cc: dts_def_equal, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(step_after.orientation(), None);
    assert_eq!(step_after.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(step_after.transition(Event::CcChanged(dts_3a0_normal)), Step::Advanced(None));
    assert_eq!(
        step_after.transition(Event::DebounceExpired { cc: dts_3a0_normal, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp3_0A)))
    );
    assert_eq!(step_before.orientation(), step_after.orientation());
    assert_eq!(step_before.orientation(), None);
    assert_eq!(step_before.sink_rp_level(), step_after.sink_rp_level());
    assert_eq!(step_before.sink_rp_level(), Some(RpValue::Rp3_0A));

    let mut glitch_at_attach = Machine::new();
    glitch_at_attach.sink_debug_accessory_mode = true;
    assert_eq!(
        glitch_at_attach.transition(Event::CcChanged(dts_def_mixed)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        glitch_at_attach
            .transition(Event::DebounceExpired { cc: dts_3a0_reverse, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(glitch_at_attach.orientation(), Some(Orientation::Normal));
    assert_eq!(glitch_at_attach.sink_rp_level(), Some(RpValue::Default));
    assert_eq!(
        glitch_at_attach
            .transition(Event::DebounceExpired { cc: dts_def_mixed, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(glitch_at_attach.orientation(), Some(Orientation::Normal));
    assert_eq!(glitch_at_attach.sink_rp_level(), Some(RpValue::Default));

    // Equal-`Rp` DTS pin-pair changes at `tCCDebounce` expiry (`Default/Default -> 3.0/1.5` or
    // `1.5/Default -> Default/1.5`) preserve the pre-step orientation, matching post-attach steps.
    let mut equal_rp_step = Machine::new();
    equal_rp_step.sink_debug_accessory_mode = true;
    assert_eq!(
        equal_rp_step.transition(Event::CcChanged(dts_def_equal)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        equal_rp_step.transition(Event::DebounceExpired { cc: dts_def_mixed, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(equal_rp_step.orientation(), None);
    assert_eq!(equal_rp_step.sink_rp_level(), Some(RpValue::Default));

    let mut equal_rp_glitch = Machine::new();
    equal_rp_glitch.sink_debug_accessory_mode = true;
    assert_eq!(
        equal_rp_glitch.transition(Event::CcChanged(dts_1a5_normal)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        equal_rp_glitch
            .transition(Event::DebounceExpired { cc: dts_1a5_reverse, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(equal_rp_glitch.orientation(), Some(Orientation::Normal));
    assert_eq!(equal_rp_glitch.sink_rp_level(), Some(RpValue::Rp1_5A));

    // 2. Post-attach DTS rises require exact pin-pair stability across `tRpValueChange`, and
    // neither equal-`Rp` steps nor reversed `Rp/Rp` updates alter the attach-time `orientation()`.
    let mut dts_machine = Machine::new();
    dts_machine.sink_debug_accessory_mode = true;
    dts_machine.cc_status = dts_1a5_normal;
    dts_machine.set_phase(Phase::AttachedSnk {
        cc_status: dts_1a5_normal,
        rp: RpValue::Rp1_5A,
        stage: SinkStage::Steady,
    });
    assert_eq!(dts_machine.orientation(), Some(Orientation::Normal));
    assert_eq!(dts_machine.transition(Event::CcChanged(dts_1a5_reverse)), Step::Absorbed);
    assert_eq!(dts_machine.orientation(), Some(Orientation::Normal));

    assert_eq!(dts_machine.transition(Event::CcChanged(dts_3a0_normal)), Step::Advanced(None));
    assert_eq!(dts_machine.transition(Event::CcChanged(dts_3a0_reverse)), Step::Advanced(None));
    assert_eq!(
        dts_machine.phase,
        Phase::AttachedSnk {
            cc_status: dts_1a5_normal,
            rp: RpValue::Rp1_5A,
            stage: SinkStage::DebouncingRpRise { candidate: dts_3a0_reverse },
        }
    );
    assert_eq!(
        dts_machine.transition(Event::DebounceExpired { cc: dts_3a0_reverse, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Rp3_0A)))
    );
    assert_eq!(dts_machine.orientation(), Some(Orientation::Normal));

    assert_eq!(dts_machine.transition(Event::CcChanged(dts_def_equal)), Step::Advanced(None));
    assert_eq!(dts_machine.transition(Event::CcChanged(dts_def_mixed)), Step::Absorbed);
    assert_eq!(
        dts_machine.transition(Event::DebounceExpired { cc: dts_def_equal, vbus_ready: false }),
        Step::Advanced(Some(Effect::NotifySinkRp(RpValue::Default)))
    );
    assert_eq!(dts_machine.orientation(), Some(Orientation::Normal));
}

/// Verifies that an `Rp` rise first seen in `DebounceExpired` or `VbusChanged`, with no earlier
/// `CcChanged`, still commits the earlier sample and debounces the new one for `tRpValueChange`.
#[fuchsia::test]
fn test_rp_rise_at_attach_without_cc_changed_is_debounced() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let debouncing_rp = |cc_status, rp, candidate| Phase::AttachedSnk {
        cc_status,
        rp,
        stage: SinkStage::DebouncingRpRise { candidate },
    };

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_1a5, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.phase, debouncing_rp(snk_cc1, RpValue::Default, snk_cc1_1a5));

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: snk_cc1_1a5, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.phase, debouncing_rp(snk_cc1, RpValue::Default, snk_cc1_1a5));
}

/// Verifies that `Rp` stepping `Default -> 1.5A -> 3.0A` during `tCCDebounce` commits `Default`,
/// the lowest level seen, rather than the unsettled 1.5A.
#[fuchsia::test]
fn test_rp_steps_during_cc_debounce_commit_lowest_level() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_1a5)), Step::Absorbed);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Absorbed);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1_3a0, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Default));
}
