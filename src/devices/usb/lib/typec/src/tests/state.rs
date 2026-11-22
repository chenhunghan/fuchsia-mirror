// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::cc::{Accessory, CcPinState, CcStatus, PowerRole, RpValue};
use crate::port::*;
use fidl_next_fuchsia_hardware_usb_connector::Orientation;

/// Verifies that `TypecState`, `DebounceDirection`, `DebounceTimer`, and `Phase::debounce_timer`
/// format labels and report state predicates.
#[fuchsia::test]
fn test_typec_state_predicates() {
    assert!(!TypecState::UnattachedSnk.is_attached());
    assert!(!TypecState::AttachWaitSnk.is_attached());
    assert!(TypecState::AttachedSnk.is_attached());
    assert!(TypecState::DebugAccessorySnk.is_attached());
    assert!(!TypecState::DebugAccessorySnk.is_unattached());
    assert!(!TypecState::AttachWaitSrc.is_attached());
    assert!(TypecState::AttachedSrc.is_attached());
    assert!(!TypecState::Disabled.is_attached());

    assert!(!TypecState::AttachedSnk.is_sourcing_vbus());
    assert!(!TypecState::DebugAccessorySnk.is_sourcing_vbus());
    assert!(TypecState::AttachedSrc.is_sourcing_vbus());

    assert_eq!(TypecState::AttachedSnk.power_role(), Some(PowerRole::Sink));
    assert_eq!(TypecState::DebugAccessorySnk.power_role(), Some(PowerRole::Sink));
    assert_eq!(TypecState::AttachedSrc.power_role(), Some(PowerRole::Source));
    assert_eq!(TypecState::AttachWaitSnk.power_role(), None);
    assert_eq!(TypecState::UnattachedSnk.power_role(), None);

    assert_eq!(DebounceDirection::Attach.as_str(), "attach");
    assert_eq!(DebounceDirection::Disconnect.as_str(), "disconnect");

    assert_eq!(DebounceTimer::CcAttach.as_str(), "cc_attach");
    assert_eq!(DebounceTimer::SnkExit.as_str(), "snk_exit");
    assert_eq!(DebounceTimer::SrcDisconnect.as_str(), "disconnect");
    assert_eq!(DebounceTimer::RpValueChange.as_str(), "rp_value_change");

    let cc = CcStatus::default();
    assert_eq!(Phase::Unattached.debounce_timer(), None);
    assert_eq!(
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
            .debounce_timer(),
        Some(DebounceTimer::CcAttach)
    );
    assert_eq!(
        Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::WaitingForVbus }
            .debounce_timer(),
        None
    );
    assert_eq!(
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingSnkExit }
            .debounce_timer(),
        Some(DebounceTimer::SnkExit)
    );
    assert_eq!(
        Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held }
            .debounce_timer(),
        None
    );
    assert_eq!(Phase::AttachedSrc { cc_status: cc, disconnecting: false }.debounce_timer(), None);
    assert_eq!(
        Phase::AttachedSrc { cc_status: cc, disconnecting: true }.debounce_timer(),
        Some(DebounceTimer::SrcDisconnect)
    );
    assert_eq!(
        Phase::AttachedSnk { cc_status: cc, rp: RpValue::Default, stage: SinkStage::Steady }
            .debounce_timer(),
        None
    );
    assert_eq!(
        Phase::AttachedSnk {
            cc_status: cc,
            rp: RpValue::Default,
            stage: SinkStage::DebouncingRpRise { candidate: cc },
        }
        .debounce_timer(),
        Some(DebounceTimer::RpValueChange)
    );
    assert_eq!(
        Phase::AttachedSnk {
            cc_status: cc,
            rp: RpValue::Rp3_0A,
            stage: SinkStage::DebouncingRpFall { floor: RpValue::Default },
        }
        .debounce_timer(),
        Some(DebounceTimer::RpValueChange)
    );

    for (state, label) in [
        (TypecState::UnattachedSnk, "Unattached.SNK"),
        (TypecState::AttachWaitSnk, "AttachWait.SNK"),
        (TypecState::AttachWaitSrc, "AttachWait.SRC"),
        (TypecState::AttachedSnk, "Attached.SNK"),
        (TypecState::AttachedSrc, "Attached.SRC"),
        (TypecState::DebugAccessorySnk, "DebugAccessory.SNK"),
        (TypecState::Disabled, "Disabled"),
    ] {
        assert_eq!(state.as_str(), label);
    }
}

/// Verifies that `Machine` projects `Phase` variants onto `TypecState`, `orientation`,
/// `accessory`, and `uncommitted_debounce_direction`, and settles uncommitted debounce states on
/// `cancel_debounce`, `set_disabled`, and `reset_to_disconnected`.
#[fuchsia::test]
fn test_machine_phase_projections_and_lifecycle_controls() {
    let mut machine = Machine::new();
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);

    assert_eq!(machine.orientation(), None);
    assert_eq!(machine.accessory(), None);
    assert_eq!(machine.uncommitted_debounce_direction(), None);

    machine.cc_status = snk_dbg;
    machine.set_phase(Phase::AttachWait {
        role: PowerRole::Sink,
        stage: AttachWaitStage::DebouncingCc,
    });
    assert_eq!(machine.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(machine.orientation(), Some(Orientation::Normal));
    assert_eq!(machine.accessory(), None);
    assert_eq!(machine.uncommitted_debounce_direction(), Some(DebounceDirection::Attach));
    machine.sink_debug_accessory_mode = true;
    assert_eq!(machine.accessory(), Some(Accessory::DebugAccessorySnk));

    machine.set_phase(Phase::AttachWait {
        role: PowerRole::Sink,
        stage: AttachWaitStage::DebouncingSnkExit,
    });
    assert_eq!(machine.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(machine.orientation(), None);
    assert_eq!(machine.uncommitted_debounce_direction(), Some(DebounceDirection::Disconnect));

    machine.cc_status = src_cc2;
    machine.set_phase(Phase::AttachWait {
        role: PowerRole::Source,
        stage: AttachWaitStage::DebouncingCc,
    });
    assert_eq!(machine.typec_state(), TypecState::AttachWaitSrc);
    assert_eq!(machine.orientation(), Some(Orientation::Reverse));
    machine.cancel_debounce();
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(machine.uncommitted_debounce_direction(), None);
    assert_eq!(machine.telemetry.debounce_cancelled_count, 1);

    machine.cc_status = snk_cc1;
    machine.set_phase(Phase::AttachWait {
        role: PowerRole::Sink,
        stage: AttachWaitStage::DebouncingCc,
    });
    machine.reset_to_disconnected();
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.typec_state(), TypecState::UnattachedSnk);
    assert_eq!(machine.uncommitted_debounce_direction(), None);
    assert_eq!(machine.telemetry.debounce_cancelled_count, 2);

    machine.cc_status = CcStatus::default();
    machine.set_phase(Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true });
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);
    assert_eq!(machine.orientation(), Some(Orientation::Reverse));
    assert_eq!(machine.uncommitted_debounce_direction(), Some(DebounceDirection::Disconnect));
    machine.cancel_debounce();
    assert_eq!(machine.phase, Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false });
    assert_eq!(machine.visible_cc_status(), src_cc2);
    assert_eq!(machine.uncommitted_debounce_direction(), None);
    assert_eq!(machine.telemetry.debounce_cancelled_count, 3);

    assert!(machine.set_disabled(true));
    assert_eq!(machine.typec_state(), TypecState::Disabled);
    assert!(!machine.typec_state().is_unattached());
    assert!(!machine.typec_state().is_attached());
    assert_eq!(machine.visible_cc_status(), CcStatus::default());
    assert_eq!(machine.orientation(), None);
    assert_eq!(machine.accessory(), None);
    assert!(!machine.set_disabled(true));

    assert!(machine.set_disabled(false));
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);
    assert!(machine.typec_state().is_attached());
    assert!(!machine.typec_state().is_unattached());
    assert!(!machine.set_disabled(false));

    machine.reset_to_disconnected();
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.typec_state(), TypecState::UnattachedSnk);
    assert!(machine.typec_state().is_unattached());
    assert!(!machine.typec_state().is_attached());
    assert_eq!(machine.telemetry.debounce_cancelled_count, 3);

    let lone_src_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::Open);
    machine.cc_status = lone_src_ra;
    assert_eq!(machine.visible_cc_status(), lone_src_ra);
    assert!(machine.set_disabled(true));
    assert!(machine.set_disabled(false));
    assert_eq!(machine.visible_cc_status(), CcStatus::default());

    machine.set_phase(Phase::AttachedSnk {
        cc_status: snk_dbg,
        rp: RpValue::Rp3_0A,
        stage: SinkStage::Steady,
    });
    assert_eq!(machine.uncommitted_debounce_direction(), None);
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert!(machine.set_disabled(true));
    assert_eq!(machine.sink_rp_level(), None);
    assert!(machine.set_disabled(false));
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp3_0A));
}

/// Verifies that `vbus_action` selects attach qualification or Sink disconnect monitoring only in
/// phases that transition on VBUS edges, and `NoTransition` otherwise (including while disabled).
#[fuchsia::test]
fn test_machine_vbus_action() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

    let mut machine = Machine::new();
    machine.sink_debug_accessory_mode = true;
    assert_eq!(machine.vbus_action(), VbusAction::NoTransition);

    machine.phase =
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc };
    assert_eq!(machine.vbus_action(), VbusAction::NoTransition);

    machine.phase =
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus };
    assert_eq!(machine.vbus_action(), VbusAction::QualifyAttach(PowerRole::Sink));

    machine.phase =
        Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::WaitingForVbus };
    assert_eq!(machine.vbus_action(), VbusAction::QualifyAttach(PowerRole::Source));

    machine.phase =
        Phase::AttachedSnk { cc_status: snk_cc1, rp: RpValue::Default, stage: SinkStage::Steady };
    assert_eq!(machine.vbus_action(), VbusAction::CheckSinkDisconnect);

    machine.phase =
        Phase::AttachedSnk { cc_status: snk_dbg, rp: RpValue::Rp3_0A, stage: SinkStage::Steady };
    assert_eq!(machine.vbus_action(), VbusAction::CheckSinkDisconnect);
    assert!(machine.set_disabled(true));
    assert_eq!(machine.vbus_action(), VbusAction::NoTransition);
    assert!(machine.set_disabled(false));

    machine.phase = Phase::AttachedSrc { cc_status: src_cc1, disconnecting: false };
    assert_eq!(machine.vbus_action(), VbusAction::NoTransition);
}

/// Verifies every `(Phase, Event)` arm of `Machine::transition` in isolation via a transition
/// table, asserting resulting `Phase`, `Step`, and telemetry counter deltas.
#[fuchsia::test]
fn test_transition_table_arms() {
    let open = CcStatus::default();
    let searching = CcStatus::LOOKING4CONNECTION;
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let ra_cc1 = CcStatus::new(CcPinState::SrcRa, CcPinState::Open);
    let ra_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRa);
    let ra_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);
    let src_dbg = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRd);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let snk_dbg_1a5 = CcStatus::new(CcPinState::SnkPower1_5, CcPinState::SnkDefault);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Deltas {
        disconnect: u64,
        cancelled: u64,
    }

    impl Deltas {
        const NONE: Self = Self { disconnect: 0, cancelled: 0 };
    }

    struct Case {
        initial: Phase,
        sink_debug: bool,
        event: Event,
        next: Phase,
        step: Step,
        deltas: Deltas,
    }

    let cases = [
        // --- Phase::Unattached ---
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::Unattached,
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(snk_cc1),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(ra_cc1),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(ra_ra),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            step: Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            step: Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: true,
            event: Event::CcChanged(snk_dbg),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink))),
            deltas: Deltas::NONE,
        },
        // --- Phase::AttachWait (DebouncingCc) ---
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc2),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_dbg),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(ra_cc1),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: snk_cc1, vbus_ready: true },
            next: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            step: Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: snk_cc1, vbus_ready: false },
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: snk_cc2, vbus_ready: true },
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: src_cc1, vbus_ready: false },
            next: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::WaitingForVbus,
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        // --- Phase::AttachWait (WaitingForVbus) ---
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: src_cc1, vbus_ready: true },
            next: Phase::AttachedSrc { cc_status: src_cc1, disconnecting: false },
            step: Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::CcChanged(src_cc1),
            next: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            step: Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc1, vbus_ready: false },
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc1, vbus_ready: true },
            next: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            step: Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink))),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc2, vbus_ready: true },
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        // --- Phase::AttachWait (DebouncingSnkExit & Held) ---
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: open, vbus_ready: false },
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: snk_cc1, vbus_ready: false },
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(snk_dbg_1a5),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingSnkExit,
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(src_cc1),
            next: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc,
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: None,
                resample_cc: false,
            })),
            deltas: Deltas::NONE,
        },
        // --- Phase::AttachedSnk & Phase::AttachedSrc ---
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::DebouncingRpRise { candidate: snk_cc1_3a0 },
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1_3a0,
                rp: RpValue::Rp3_0A,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1),
            next: Phase::AttachedSnk {
                cc_status: snk_cc1_3a0,
                rp: RpValue::Rp3_0A,
                stage: SinkStage::DebouncingRpFall { floor: RpValue::Default },
            },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: open, vbus_ready: true },
            next: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc1, vbus_ready: false },
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: Some(PowerRole::Sink),
                resample_cc: true,
            })),
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: open, vbus_ready: false },
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: Some(PowerRole::Sink),
                resample_cc: false,
            })),
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachedSnk {
                cc_status: snk_cc1,
                rp: RpValue::Default,
                stage: SinkStage::Steady,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSnk {
                cc_status: snk_dbg,
                rp: RpValue::Rp3_0A,
                stage: SinkStage::Steady,
            },
            sink_debug: true,
            event: Event::CcChanged(open),
            next: Phase::AttachedSnk {
                cc_status: snk_dbg,
                rp: RpValue::Rp3_0A,
                stage: SinkStage::Steady,
            },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::CcChanged(ra_cc2),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::CcChanged(ra_cc1),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            step: Step::Advanced(None),
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            sink_debug: false,
            event: Event::CcChanged(src_cc2),
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            sink_debug: false,
            event: Event::DebounceExpired { cc: src_cc2, vbus_ready: false },
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            step: Step::Advanced(None),
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            sink_debug: false,
            event: Event::DebounceExpired { cc: open, vbus_ready: false },
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: Some(PowerRole::Source),
                resample_cc: false,
            })),
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: true },
            sink_debug: false,
            event: Event::DebounceExpired { cc: src_cc1, vbus_ready: false },
            next: Phase::Unattached,
            step: Step::Advanced(Some(Effect::EnterUnattached {
                detached_from: Some(PowerRole::Source),
                resample_cc: true,
            })),
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            sink_debug: false,
            event: Event::VbusChanged { cc: open, vbus_ready: false },
            next: Phase::AttachedSrc { cc_status: src_cc2, disconnecting: false },
            step: Step::Absorbed,
            deltas: Deltas::NONE,
        },
    ];

    for (idx, case) in cases.iter().enumerate() {
        let mut machine = Machine::new();
        machine.phase = case.initial;
        machine.cc_status = match case.initial {
            Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
            } => snk_cc1,
            Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held } => snk_dbg,
            Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::DebouncingCc | AttachWaitStage::WaitingForVbus,
            } => src_cc1,
            Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held } => ra_ra,
            _ => open,
        };
        machine.sink_debug_accessory_mode = case.sink_debug;
        let step = machine.transition(case.event);
        assert_eq!(step, case.step, "table case {idx} transition mismatch");
        assert_eq!(machine.phase, case.next, "table case {idx} phase mismatch");
        assert_eq!(machine.telemetry.attach_count, 0, "table case {idx} attach_count mismatch");
        assert_eq!(
            Deltas {
                disconnect: machine.telemetry.disconnect_count,
                cancelled: machine.telemetry.debounce_cancelled_count,
            },
            case.deltas,
            "table case {idx} telemetry delta mismatch"
        );
    }
}

/// Verifies that attaching commits the attach-time `Rp` level, `VbusChanged` detaches an
/// `Attached.SNK` partner into `Unattached`, a fresh `CcChanged` sample re-enters `AttachWait.SNK`,
/// and Source attach reaches `Attached.SRC`.
#[fuchsia::test]
fn test_vbus_loss_and_commit_lifecycle() {
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);

    let mut machine = Machine::new();
    assert!(machine.allows_vsafe0v_disarm());
    assert!(machine.allows_on_vsafe0v());
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert!(!machine.allows_vsafe0v_disarm());
    assert!(!machine.allows_on_vsafe0v());
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert!(!machine.allows_vsafe0v_disarm());
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Default));

    assert_eq!(
        machine.transition(Event::VbusChanged { cc: snk_cc1_3a0, vbus_ready: false }),
        Step::Advanced(Some(Effect::EnterUnattached {
            detached_from: Some(PowerRole::Sink),
            resample_cc: true,
        }))
    );
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.cc_status, CcStatus::default());
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1_3a0)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
    );

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(src_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert!(machine.allows_vsafe0v_disarm());
    assert!(!machine.allows_on_vsafe0v());
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
    );
    assert!(!machine.allows_vsafe0v_disarm());
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);
}

/// Verifies that `DebugAccessory.SNK` reports its state, stays attached across a CC change to
/// `Open`, and detaches only on `VbusChanged`.
#[fuchsia::test]
fn test_debug_accessory_snk_exits_only_on_vbus_loss() {
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let open = CcStatus::default();
    let attached =
        Phase::AttachedSnk { cc_status: snk_dbg, rp: RpValue::Rp3_0A, stage: SinkStage::Steady };

    let mut machine = Machine::new();
    machine.sink_debug_accessory_mode = true;
    assert_eq!(
        machine.transition(Event::CcChanged(snk_dbg)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_dbg, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(machine.accessory(), Some(Accessory::DebugAccessorySnk));

    assert_eq!(machine.transition(Event::CcChanged(open)), Step::Absorbed);
    assert_eq!(machine.phase, attached);
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);

    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Step::Advanced(Some(Effect::EnterUnattached {
            detached_from: Some(PowerRole::Sink),
            resample_cc: false,
        }))
    );
    assert_eq!(machine.phase, Phase::Unattached);
}

/// Verifies that an event the current phase ignores leaves `phase`, `cc_status`, and telemetry
/// unchanged across disabled, `Unattached`, `DebouncingCc`, `WaitingForVbus`, `Held`, and
/// `Attached.{SNK,SRC}` states.
#[fuchsia::test]
fn test_ignored_event_keeps_cc_status() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let searching = CcStatus::LOOKING4CONNECTION;
    let open = CcStatus::default();

    let mut disabled = Machine::new();
    assert!(disabled.set_disabled(true));
    assert_eq!(disabled.transition(Event::CcChanged(snk_cc1)), Step::Absorbed);
    assert_eq!(
        disabled.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(
        disabled.transition(Event::VbusChanged { cc: snk_cc1, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(disabled.transition_initial(snk_cc1, true), Step::Absorbed);
    assert_eq!(disabled.phase, Phase::Unattached);
    assert_eq!(disabled.cc_status, open);
    assert_eq!(disabled.telemetry.attach_count, 0);

    let mut machine = Machine::new();
    machine.cc_status = snk_cc1;
    assert_eq!(machine.transition(Event::CcChanged(searching)), Step::Absorbed);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(snk_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus }
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Step::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Step::Absorbed
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus }
    );
    assert_eq!(machine.cc_status, snk_cc1);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: snk_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);

    // An `Rp/Rp` Debug Accessory with Sink Debug Accessory Mode off is held in `AttachWait.SNK`.
    let held = Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held };
    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(snk_dbg)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(machine.phase, held);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Step::Absorbed
    );
    assert_eq!(machine.cc_status, snk_dbg);
    assert_eq!(machine.transition(Event::CcChanged(snk_dbg)), Step::Absorbed);
    assert_eq!(machine.phase, held);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(src_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Step::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Step::Absorbed
    );
    assert_eq!(machine.cc_status, src_cc1);
}

/// Verifies that `Machine::transition_initial` skips `tCCDebounce` for a Source partner only when
/// VBUS is already present (debouncing `tCCDebounce` otherwise), keeps `tCCDebounce` for a fresh
/// Sink partner, qualifies an existing `WaitingForVbus` stage, and upgrades `Looking4Connection` in
/// `Unattached` to `Step::Advanced(Some(Effect::EnterUnattached))`.
#[fuchsia::test]
fn test_transition_initial() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let searching = CcStatus::LOOKING4CONNECTION;

    // 1. Initial-sync Sink with VBUS ready commits `Attached.SNK` immediately, and an idempotent
    // repeat while already attached is absorbed.
    let mut machine = Machine::new();
    assert_eq!(machine.initial_vbus_role(snk_cc1), Some(PowerRole::Sink));
    assert_eq!(
        machine.transition_initial(snk_cc1, true),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(
        machine.phase,
        Phase::AttachedSnk { cc_status: snk_cc1, rp: RpValue::Default, stage: SinkStage::Steady }
    );
    assert_eq!(machine.debounce_timer(), None);
    assert_eq!(machine.transition_initial(snk_cc1, true), Step::Absorbed);

    // 2. Initial-sync Sink without VBUS debounces `tCCDebounce` (`DebouncingCc`) per R2.5
    // §4.5.2.2.4.2; a follow-up `transition_initial` once VBUS is ready supersedes either
    // `DebouncingCc` or `WaitingForVbus` into `Attached.SNK`.
    let mut machine = Machine::new();
    assert_eq!(
        machine.transition_initial(snk_cc1, false),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
    );
    assert_eq!(machine.debounce_timer(), Some(DebounceTimer::CcAttach));
    assert_eq!(
        machine.transition_initial(snk_cc1, true),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.typec_state(), TypecState::AttachedSnk);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition_initial(snk_cc1, false),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus }
    );
    assert_eq!(machine.debounce_timer(), None);
    assert_eq!(
        machine.transition_initial(snk_cc1, true),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.typec_state(), TypecState::AttachedSnk);

    // 3. Initial-sync Source partner still debounces `tCCDebounce` first, then qualifies `vSafe0V`
    // once in `WaitingForVbus`.
    let mut machine = Machine::new();
    assert_eq!(machine.initial_vbus_role(src_cc1), None);
    assert_eq!(
        machine.transition_initial(src_cc1, false),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::DebouncingCc }
    );
    assert_eq!(machine.debounce_timer(), Some(DebounceTimer::CcAttach));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: false }),
        Step::Advanced(None)
    );
    assert_eq!(machine.initial_vbus_role(src_cc1), Some(PowerRole::Source));
    assert_eq!(
        machine.transition_initial(src_cc1, true),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
    );
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);

    // 4. Initial-sync `Looking4Connection` in `Unattached` returns
    // `Step::Advanced(Some(Effect::EnterUnattached { .. }))` so `on_unattached` runs.
    let mut machine = Machine::new();
    assert_eq!(
        machine.transition_initial(searching, false),
        Step::Advanced(Some(Effect::EnterUnattached { detached_from: None, resample_cc: false }))
    );
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.cc_status, searching);

    // 5. Switching from `AttachWait.SRC` to a Source partner with VBUS present enters
    // `AttachWait.SNK` and debounces `tCCDebounce` rather than bypassing `enter_attach_wait`.
    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(src_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert_eq!(
        machine.transition_initial(snk_cc1, true),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Sink)))
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
    );
    assert_eq!(machine.debounce_timer(), Some(DebounceTimer::CcAttach));
}

/// Verifies that `hold_failed_attach` rolls back failed Sink and Source attaches into
/// `AttachWaitStage::Held` without ever incrementing `attach_count`, `record_attached` increments
/// `attach_count` on success, and `enter_unattached` reports `resample_cc` while clearing stale
/// partner terminations from `cc_status`.
#[fuchsia::test]
fn test_hold_failed_attach_and_unattached_resample() {
    let open = CcStatus::default();
    let ra_cc1 = CcStatus::new(CcPinState::SrcRa, CcPinState::Open);
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_cc1_3a0 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let src_cc1_ra = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRa);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition_initial(snk_cc1, true),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    assert_eq!(machine.telemetry.attach_count, 0);
    assert!(machine.hold_failed_attach(PowerRole::Sink));
    assert!(machine.is_partner_held());
    assert_eq!(machine.typec_state(), TypecState::AttachWaitSnk);
    assert_eq!(machine.telemetry.attach_count, 0);
    assert_eq!(machine.telemetry.sink_attach_failure_count, 1);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Step::Absorbed);
    assert!(machine.is_partner_held());
    assert_eq!(machine.transition(Event::CcChanged(snk_cc2)), Step::Advanced(None));
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc2, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Sink)))
    );
    machine.record_attached();
    assert_eq!(machine.telemetry.attach_count, 1);

    let mut machine = Machine::new();
    assert_eq!(
        machine.transition(Event::CcChanged(src_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
    );
    assert!(machine.hold_failed_attach(PowerRole::Source));
    assert!(machine.is_partner_held());
    assert_eq!(machine.telemetry.source_attach_failure_count, 1);
    assert_eq!(machine.transition(Event::CcChanged(src_cc1_ra)), Step::Absorbed);
    assert!(machine.is_partner_held());
    assert_eq!(
        machine.transition(Event::CcChanged(open)),
        Step::Advanced(Some(Effect::EnterUnattached { detached_from: None, resample_cc: false }))
    );
    assert_eq!(machine.phase, Phase::Unattached);

    // Lone `SRC.Ra` enters `Unattached` with `resample_cc: false` and preserves `ra_cc1`.
    assert_eq!(
        machine.transition(Event::CcChanged(ra_cc1)),
        Step::Advanced(Some(Effect::EnterUnattached { detached_from: None, resample_cc: false }))
    );
    assert_eq!(machine.cc_status, ra_cc1);

    // Detaching when a partner termination (`src_cc2` on the opposite pin) is already present sets
    // `resample_cc: true` and clears `cc_status` to `Open` until the runner resamples `CC_STATUS`.
    assert_eq!(
        machine.transition(Event::CcChanged(src_cc1)),
        Step::Advanced(Some(Effect::EnterAttachWait(PowerRole::Source)))
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Step::Advanced(Some(Effect::EnterAttached(PowerRole::Source)))
    );
    assert_eq!(machine.transition(Event::CcChanged(open)), Step::Advanced(None));
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc2, vbus_ready: false }),
        Step::Advanced(Some(Effect::EnterUnattached {
            detached_from: Some(PowerRole::Source),
            resample_cc: true,
        }))
    );
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.cc_status, open);
}
