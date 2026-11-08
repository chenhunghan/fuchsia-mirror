// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cc::*;
use fidl_next_fuchsia_hardware_usb_connector::Orientation;

/// Verifies that `RpValue`, `CcTermination`, and `RoleControl` default to the TCPCI power-on
/// values.
#[fuchsia::test]
fn test_defaults() {
    assert_eq!(RpValue::default(), RpValue::Default);
    assert_eq!(CcTermination::default(), CcTermination::Open);
    assert_eq!(RoleControl::default(), RoleControl::STATIC_RD);
    assert!(!RoleControl::STATIC_RD.drp);
    assert!(RoleControl::DRP_RD.drp);
}

/// Verifies that `CcPinState` defaults to `Open` and formats matching its static string labels.
#[fuchsia::test]
fn test_cc_state_formatting() {
    assert_eq!(CcPinState::default(), CcPinState::Open);
    assert_eq!(CcPinState::Open.rp_value(), None);
    assert_eq!(CcPinState::SnkDefault.rp_value(), Some(RpValue::Default));
    assert_eq!(CcPinState::SnkPower1_5.rp_value(), Some(RpValue::Rp1_5A));
    assert_eq!(CcPinState::SnkPower3_0.rp_value(), Some(RpValue::Rp3_0A));
    assert_eq!(CcPinState::SrcRa.rp_value(), None);
    assert_eq!(CcPinState::SrcRd.rp_value(), None);
    let states = [
        (CcPinState::Open, "Open"),
        (CcPinState::SnkDefault, "SNK.Default"),
        (CcPinState::SnkPower1_5, "SNK.Power1.5"),
        (CcPinState::SnkPower3_0, "SNK.Power3.0"),
        (CcPinState::SrcRa, "SRC.Ra"),
        (CcPinState::SrcRd, "SRC.Rd"),
    ];
    for (state, label) in states {
        assert_eq!(state.as_str(), label);
        assert_eq!(format!("{state}"), label);
    }
}

/// Verifies that `Looking4Connection` hides the CC pins and that `CcStatus` resolves
/// `PowerRole`, cable `Orientation`, and the partner's `Rp` level.
#[fuchsia::test]
fn test_cc_status_clamping_and_role_resolution() {
    assert_eq!(CcStatus::default(), CcStatus::new(CcPinState::Open, CcPinState::Open));

    let searching = CcStatus::LOOKING4CONNECTION;
    assert!(searching.looking4connection);
    assert_eq!(searching.cc1, CcPinState::Open);
    assert_eq!(searching.cc2, CcPinState::Open);
    assert_eq!(searching.power_role(), None);
    assert_eq!(searching.accessory(), None);
    assert_eq!(searching.orientation(), None);
    assert_eq!(searching.sink_rp_level(), None);

    for manual_searching in [
        CcStatus { cc1: CcPinState::SnkPower3_0, cc2: CcPinState::Open, looking4connection: true },
        CcStatus {
            cc1: CcPinState::SnkPower3_0,
            cc2: CcPinState::SnkPower1_5,
            looking4connection: true,
        },
        CcStatus { cc1: CcPinState::SrcRd, cc2: CcPinState::SrcRa, looking4connection: true },
    ] {
        assert_eq!(manual_searching.power_role(), None);
        assert_eq!(manual_searching.accessory(), None);
        assert_eq!(manual_searching.orientation(), None);
    }

    let snk_cc1 = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
    assert_eq!(snk_cc1.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_cc1.orientation(), Some(Orientation::Normal));
    assert_eq!(snk_cc1.sink_rp_level(), Some(RpValue::Rp3_0A));

    let snk_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SnkDefault);
    assert_eq!(snk_cc2.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_cc2.orientation(), Some(Orientation::Reverse));
    assert_eq!(snk_cc2.sink_rp_level(), Some(RpValue::Default));

    // A cable's `Ra` on the other pin does not change the Source role or orientation.
    let src_cc2_with_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRd);
    assert_eq!(src_cc2_with_ra.power_role(), Some(PowerRole::Source));
    assert_eq!(src_cc2_with_ra.orientation(), Some(Orientation::Reverse));
    assert_eq!(src_cc2_with_ra.sink_rp_level(), None);

    let src_cc2 = CcStatus::new(CcPinState::Open, CcPinState::SrcRd);
    let src_cc1_with_ra = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRa);
    assert_eq!(src_cc1_with_ra.power_role(), Some(PowerRole::Source));
    assert_eq!(src_cc1_with_ra.orientation(), Some(Orientation::Normal));
    assert!(src_cc2.shares_connected_cc_pin(src_cc2_with_ra));
    assert!(!src_cc2.shares_connected_cc_pin(src_cc1_with_ra));
    assert!(!snk_cc2.shares_connected_cc_pin(src_cc2));
}

/// Verifies that `CcStatus::accessory` decodes `DebugAccessorySnk` and
/// `UnorientedDebugAccessorySrc`, while `power_role` and `orientation` return `None` for unsupported
/// accessory and mixed states.
#[fuchsia::test]
fn test_accessory_and_unsupported_terminations() {
    let snk_cc1 = CcStatus::new(CcPinState::SnkDefault, CcPinState::Open);
    assert_eq!(snk_cc1.accessory(), None);

    let snk_dbg_normal = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::SnkDefault);
    assert_eq!(snk_dbg_normal.accessory(), Some(Accessory::DebugAccessorySnk));
    assert_eq!(snk_dbg_normal.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_dbg_normal.orientation(), Some(Orientation::Normal));
    assert_eq!(snk_dbg_normal.sink_rp_level(), Some(RpValue::Rp3_0A));
    assert!(!snk_dbg_normal.shares_connected_cc_pin(snk_cc1));

    let snk_dbg_reverse = CcStatus::new(CcPinState::SnkDefault, CcPinState::SnkPower1_5);
    assert_eq!(snk_dbg_reverse.accessory(), Some(Accessory::DebugAccessorySnk));
    assert_eq!(snk_dbg_reverse.power_role(), Some(PowerRole::Sink));
    assert_eq!(snk_dbg_reverse.orientation(), Some(Orientation::Reverse));
    assert_eq!(snk_dbg_reverse.sink_rp_level(), Some(RpValue::Rp1_5A));
    assert!(snk_dbg_normal.shares_connected_cc_pin(snk_dbg_reverse));

    // Unoriented Sink Debug Accessory with CC1 == CC2 -> orientation is `None`.
    for state in [CcPinState::SnkDefault, CcPinState::SnkPower1_5, CcPinState::SnkPower3_0] {
        let unoriented = CcStatus::new(state, state);
        assert_eq!(unoriented.accessory(), Some(Accessory::DebugAccessorySnk));
        assert_eq!(unoriented.power_role(), Some(PowerRole::Sink));
        assert_eq!(unoriented.orientation(), None);
    }

    // TODO(https://fxbug.dev/575661321): Support `UnorientedDebugAccessory.SRC` (`SRC.Rd/SRC.Rd`)
    // in power_role / orientation.
    let src_dbg = CcStatus::new(CcPinState::SrcRd, CcPinState::SrcRd);
    assert_eq!(src_dbg.accessory(), Some(Accessory::UnorientedDebugAccessorySrc));
    assert_eq!(src_dbg.power_role(), None);
    assert_eq!(src_dbg.orientation(), None);

    // `SRC.Ra/SRC.Ra` is not an accessory in R2.5 and never attaches; single-pin `SRC.Ra`
    // (dangling active cable) is neither `is_ra_ra` nor an attachable partner (R2.5 §4.5.2.2.7.2).
    let ra_ra = CcStatus::new(CcPinState::SrcRa, CcPinState::SrcRa);
    assert!(ra_ra.is_ra_ra());
    assert_eq!(ra_ra.accessory(), None);
    assert_eq!(ra_ra.power_role(), None);
    assert_eq!(ra_ra.orientation(), None);
    for single_ra in [
        CcStatus::new(CcPinState::SrcRa, CcPinState::Open),
        CcStatus::new(CcPinState::Open, CcPinState::SrcRa),
    ] {
        assert!(!single_ra.is_ra_ra());
        assert_eq!(single_ra.accessory(), None);
        assert_eq!(single_ra.power_role(), None);
        assert_eq!(single_ra.orientation(), None);
    }

    let src_cc1 = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);
    assert!(src_dbg.keeps_attached_source(src_cc1));
    assert!(CcStatus::new(CcPinState::SrcRa, CcPinState::Open).keeps_attached_source(src_cc1));
    assert!(!CcStatus::new(CcPinState::Open, CcPinState::SrcRa).keeps_attached_source(src_cc1));

    // Invalid mixed Sink/Source states return `None`.
    for invalid in [
        CcStatus::new(CcPinState::SnkDefault, CcPinState::SrcRd),
        CcStatus::new(CcPinState::SnkDefault, CcPinState::SrcRa),
    ] {
        assert_eq!(invalid.accessory(), None);
        assert_eq!(invalid.power_role(), None);
        assert_eq!(invalid.orientation(), None);
    }
}
