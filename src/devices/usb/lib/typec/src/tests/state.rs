// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cc::{Accessory, CcPinState, CcStatus, PowerRole, RpValue};
use crate::port::*;
use crate::state::*;
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
    assert_eq!(
        Phase::Attached { role: PowerRole::Source, cc_status: cc, stage: AttachedStage::Steady }
            .debounce_timer(),
        None
    );
    assert_eq!(
        Phase::Attached {
            role: PowerRole::Source,
            cc_status: cc,
            stage: AttachedStage::Disconnecting,
        }
        .debounce_timer(),
        Some(DebounceTimer::SrcDisconnect)
    );
    assert_eq!(
        Phase::Attached {
            role: PowerRole::Sink,
            cc_status: cc,
            stage: AttachedStage::Disconnecting,
        }
        .debounce_timer(),
        None
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
    machine.set_phase(Phase::Attached {
        role: PowerRole::Source,
        cc_status: src_cc2,
        stage: AttachedStage::Disconnecting,
    });
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);
    assert_eq!(machine.orientation(), Some(Orientation::Reverse));
    assert_eq!(machine.uncommitted_debounce_direction(), Some(DebounceDirection::Disconnect));
    machine.cancel_debounce();
    assert_eq!(
        machine.phase,
        Phase::Attached {
            role: PowerRole::Source,
            cc_status: src_cc2,
            stage: AttachedStage::Steady
        }
    );
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

    machine.set_phase(Phase::Attached {
        role: PowerRole::Sink,
        cc_status: snk_dbg,
        stage: AttachedStage::Disconnecting,
    });
    assert_eq!(machine.uncommitted_debounce_direction(), None);

    machine.set_phase(Phase::Attached {
        role: PowerRole::Sink,
        cc_status: snk_dbg,
        stage: AttachedStage::Steady,
    });
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert!(machine.set_disabled(true));
    assert_eq!(machine.sink_rp_level(), None);
    assert!(machine.set_disabled(false));
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Rp3_0A));
}

/// Verifies that `vbus_change_target` selects the role and polarity only in phases that react to
/// VBUS edges (`WaitingForVbus` and `Attached.SNK` / `DebugAccessory.SNK`), and returns `None`
/// while disabled.
#[fuchsia::test]
fn test_machine_vbus_change_target() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

    let mut machine = Machine::new();
    machine.sink_debug_accessory_mode = true;
    assert_eq!(machine.vbus_change_target(), None);

    machine.phase =
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc };
    assert_eq!(machine.vbus_change_target(), None);

    machine.phase =
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus };
    assert_eq!(
        machine.vbus_change_target(),
        Some(VbusTarget { role: PowerRole::Sink, expect_ready: true })
    );

    machine.phase =
        Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::WaitingForVbus };
    assert_eq!(
        machine.vbus_change_target(),
        Some(VbusTarget { role: PowerRole::Source, expect_ready: true })
    );

    machine.phase =
        Phase::Attached { role: PowerRole::Sink, cc_status: snk_cc1, stage: AttachedStage::Steady };
    assert_eq!(
        machine.vbus_change_target(),
        Some(VbusTarget { role: PowerRole::Sink, expect_ready: false })
    );

    machine.phase =
        Phase::Attached { role: PowerRole::Sink, cc_status: snk_dbg, stage: AttachedStage::Steady };
    assert_eq!(
        machine.vbus_change_target(),
        Some(VbusTarget { role: PowerRole::Sink, expect_ready: false })
    );
    assert!(machine.set_disabled(true));
    assert_eq!(machine.vbus_change_target(), None);
    assert!(machine.set_disabled(false));

    machine.phase = Phase::Attached {
        role: PowerRole::Source,
        cc_status: src_cc1,
        stage: AttachedStage::Steady,
    };
    assert_eq!(machine.vbus_change_target(), None);
}

/// Verifies every `(Phase, Event)` arm of `Machine::transition` in isolation via a transition
/// table, asserting resulting `Phase`, `Transition`, and telemetry counter deltas.
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
        attach: u64,
        disconnect: u64,
        cancelled: u64,
    }

    impl Deltas {
        const NONE: Self = Self { attach: 0, disconnect: 0, cancelled: 0 };
    }

    struct Case {
        initial: Phase,
        sink_debug: bool,
        event: Event,
        next: Phase,
        transition: Transition,
        deltas: Deltas,
    }

    let cases = [
        // --- Phase::Unattached ---
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::Unattached,
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(snk_cc1),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(ra_cc1),
            next: Phase::Unattached,
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(ra_ra),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Unattached,
            sink_debug: true,
            event: Event::CcChanged(snk_dbg),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            transition: Transition::Moved,
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
            transition: Transition::Absorbed,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::DebouncingCc,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: snk_cc1, vbus_ready: true },
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Moved,
            deltas: Deltas { attach: 1, ..Deltas::NONE },
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Absorbed,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Source,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: src_cc1, vbus_ready: true },
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Moved,
            deltas: Deltas { attach: 1, ..Deltas::NONE },
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
            transition: Transition::Moved,
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
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc1, vbus_ready: true },
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Moved,
            deltas: Deltas { attach: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait {
                role: PowerRole::Sink,
                stage: AttachWaitStage::WaitingForVbus,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc2, vbus_ready: true },
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            transition: Transition::Moved,
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
            transition: Transition::Absorbed,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
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
            transition: Transition::Moved,
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(snk_dbg_1a5),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc },
            transition: Transition::Moved,
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
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            transition: Transition::Absorbed,
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
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::AttachWait { role: PowerRole::Source, stage: AttachWaitStage::Held },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Unattached,
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        // --- Phase::Attached ---
        Case {
            initial: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(snk_cc1_3a0),
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: open, vbus_ready: true },
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: snk_cc1, vbus_ready: false },
            next: Phase::Unattached,
            transition: Transition::Moved,
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_cc1,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_dbg,
                stage: AttachedStage::Steady,
            },
            sink_debug: true,
            event: Event::CcChanged(open),
            next: Phase::Attached {
                role: PowerRole::Sink,
                cc_status: snk_dbg,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(src_dbg),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(ra_cc2),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(ra_cc1),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(searching),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            transition: Transition::Moved,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            sink_debug: false,
            event: Event::CcChanged(open),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            transition: Transition::Absorbed,
            deltas: Deltas::NONE,
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            sink_debug: false,
            event: Event::CcChanged(src_cc2),
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Moved,
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: src_cc2, vbus_ready: false },
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Moved,
            deltas: Deltas { cancelled: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Disconnecting,
            },
            sink_debug: false,
            event: Event::DebounceExpired { cc: open, vbus_ready: false },
            next: Phase::Unattached,
            transition: Transition::Moved,
            deltas: Deltas { disconnect: 1, ..Deltas::NONE },
        },
        Case {
            initial: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            sink_debug: false,
            event: Event::VbusChanged { cc: open, vbus_ready: false },
            next: Phase::Attached {
                role: PowerRole::Source,
                cc_status: src_cc2,
                stage: AttachedStage::Steady,
            },
            transition: Transition::Absorbed,
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
        let transition = machine.transition(case.event);
        assert_eq!(transition, case.transition, "table case {idx} transition mismatch");
        assert_eq!(machine.phase, case.next, "table case {idx} phase mismatch");
        assert_eq!(
            Deltas {
                attach: machine.telemetry.attach_count,
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
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Transition::Moved);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: true }),
        Transition::Moved
    );
    assert_eq!(machine.sink_rp_level(), Some(RpValue::Default));

    assert_eq!(
        machine.transition(Event::VbusChanged { cc: snk_cc1_3a0, vbus_ready: false }),
        Transition::Moved
    );
    assert_eq!(machine.phase, Phase::Unattached);
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1_3a0)), Transition::Moved);
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::DebouncingCc }
    );

    let mut machine = Machine::new();
    assert_eq!(machine.transition(Event::CcChanged(src_cc1)), Transition::Moved);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Transition::Moved
    );
    assert_eq!(machine.typec_state(), TypecState::AttachedSrc);
}

/// Verifies that `DebugAccessory.SNK` reports its state, stays attached across a CC change to
/// `Open`, and detaches only on `VbusChanged`.
#[fuchsia::test]
fn test_debug_accessory_snk_exits_only_on_vbus_loss() {
    let snk_dbg = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    let open = CcStatus::default();
    let attached =
        Phase::Attached { role: PowerRole::Sink, cc_status: snk_dbg, stage: AttachedStage::Steady };

    let mut machine = Machine::new();
    machine.sink_debug_accessory_mode = true;
    assert_eq!(machine.transition(Event::CcChanged(snk_dbg)), Transition::Moved);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_dbg, vbus_ready: true }),
        Transition::Moved
    );
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);
    assert_eq!(machine.accessory(), Some(Accessory::DebugAccessorySnk));

    assert_eq!(machine.transition(Event::CcChanged(open)), Transition::Absorbed);
    assert_eq!(machine.phase, attached);
    assert_eq!(machine.typec_state(), TypecState::DebugAccessorySnk);

    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Transition::Moved
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
    assert_eq!(disabled.transition(Event::CcChanged(snk_cc1)), Transition::Absorbed);
    assert_eq!(
        disabled.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(
        disabled.transition(Event::VbusChanged { cc: snk_cc1, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(disabled.phase, Phase::Unattached);
    assert_eq!(disabled.cc_status, open);
    assert_eq!(disabled.telemetry.attach_count, 0);

    let mut machine = Machine::new();
    machine.cc_status = snk_cc1;
    assert_eq!(machine.transition(Event::CcChanged(searching)), Transition::Absorbed);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);

    let mut machine = Machine::new();
    assert_eq!(machine.transition(Event::CcChanged(snk_cc1)), Transition::Moved);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: snk_cc1, vbus_ready: false }),
        Transition::Moved
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus }
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.phase,
        Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::WaitingForVbus }
    );
    assert_eq!(machine.cc_status, snk_cc1);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: snk_cc1, vbus_ready: true }),
        Transition::Moved
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(machine.cc_status, snk_cc1);

    // An `Rp/Rp` Debug Accessory with Sink Debug Accessory Mode off is held in `AttachWait.SNK`.
    let held = Phase::AttachWait { role: PowerRole::Sink, stage: AttachWaitStage::Held };
    let mut machine = Machine::new();
    assert_eq!(machine.transition(Event::CcChanged(snk_dbg)), Transition::Moved);
    assert_eq!(machine.phase, held);
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: true }),
        Transition::Absorbed
    );
    assert_eq!(machine.cc_status, snk_dbg);
    assert_eq!(machine.transition(Event::CcChanged(snk_dbg)), Transition::Absorbed);
    assert_eq!(machine.phase, held);

    let mut machine = Machine::new();
    assert_eq!(machine.transition(Event::CcChanged(src_cc1)), Transition::Moved);
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: src_cc1, vbus_ready: true }),
        Transition::Moved
    );
    assert_eq!(
        machine.transition(Event::VbusChanged { cc: open, vbus_ready: false }),
        Transition::Absorbed
    );
    assert_eq!(
        machine.transition(Event::DebounceExpired { cc: open, vbus_ready: false }),
        Transition::Absorbed
    );
    assert_eq!(machine.cc_status, src_cc1);
}
