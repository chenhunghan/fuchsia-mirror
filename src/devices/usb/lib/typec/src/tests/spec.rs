// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for behavior the USB Type-C (R2.5) spec requires.

use super::*;
use CcPinState::{SnkDefault, SnkPower1_5, SnkPower3_0};

const SNK_CC1_3A0: CcStatus = CcStatus::new(CcPinState::SnkPower3_0, CcPinState::Open);
const SRC_CC1: CcStatus = CcStatus::new(CcPinState::SrcRd, CcPinState::Open);

/// Verifies that a DTS Source's current is decoded from its pair of `Rp` values (R2.5 §B.2.4.2.1
/// Table B-2), with any other pair meaning Default USB Power (§B.2.4.2.2.1).
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

/// Verifies that `RpValue::typec_current_ua` reports USB Type-C Current @ 1.5 A and 3.0 A, and
/// no fixed current for Default USB Power (R2.5 §4.6.2.1, Table 4-20).
#[fuchsia::test]
fn test_typec_current_per_table_4_20() {
    assert_eq!(RpValue::Default.typec_current_ua(), None);
    assert_eq!(RpValue::Rp1_5A.typec_current_ua(), Some(1_500_000));
    assert_eq!(RpValue::Rp3_0A.typec_current_ua(), Some(3_000_000));
}
