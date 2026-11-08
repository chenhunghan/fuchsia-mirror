// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Harness for integration tests that run battery-manager in a realm with fake drivers, a fake
//! charger, a fake system activity governor, and fake time.

mod fake_charger;
mod fake_sag;
mod realm;
mod wait;

pub use fake_charger::{
    DEFAULT_CHARGER_MAX_CURRENT_UA, DEFAULT_CHARGER_MAX_VOLTAGE_UV, FakeCharger,
};
pub use fake_sag::LeaseEvent;
pub use fidl_fuchsia_testing;
pub use fidl_test_hardwarepowercontrol;
pub use realm::{
    FidlRouteMode, battery_manager_moniker, setup_realm, setup_realm_with_charger,
    setup_realm_with_config,
};
pub use wait::{wait_for_battery_info, wait_for_inspect_match, wait_for_matching_battery_info};
