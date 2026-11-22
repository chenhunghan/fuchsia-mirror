// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration test for the Starnix `battery_manager` module with the real
//! Fuchsia `battery-manager` component and a fake battery driver service.

use std::time::{Duration, Instant};

const BATTERY_DIR: &str = "/sys/class/power_supply/battery";
const USB_DIR: &str = "/sys/class/power_supply/usb";
const AC_DIR: &str = "/sys/class/power_supply/ac";
const WIRELESS_DIR: &str = "/sys/class/power_supply/wireless";
const POLL_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn read_sysfs(directory: &str, attribute: &str) -> String {
    let path = format!("{directory}/{attribute}");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
}

/// Polls a sysfs file until it matches `expected`. Use this where the value depends on a
/// `BatteryInfoWatcher` update from `battery-manager` that may not have arrived yet.
fn wait_for_sysfs_value(path: &str, expected: &str) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            if value == expected {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {path} to equal {expected:?} (last value: {:?})",
            std::fs::read_to_string(path)
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[test]
fn test_battery_manager_capacity_override_and_reconnect() {
    let capacity_path = format!("{BATTERY_DIR}/capacity");

    // 1. Wait for initial telemetry from fake_battery -> battery-manager -> Starnix sysfs.
    wait_for_sysfs_value(&capacity_path, "50\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "type"), "Battery\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "present"), "1\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "capacity_level"), "Normal\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "status"), "Charging\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "health"), "Good\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "voltage_now"), "4100000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "charge_counter"), "2000000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "charge_full"), "4000000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "charge_full_design"), "4200000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "temp"), "250\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "time_to_full_now"), "-22\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "technology"), "Li-ion\n");

    assert_eq!(read_sysfs(USB_DIR, "type"), "USB\n");
    assert_eq!(read_sysfs(USB_DIR, "online"), "1\n");
    assert_eq!(read_sysfs(USB_DIR, "present"), "1\n");
    assert_eq!(read_sysfs(USB_DIR, "usb_type"), "Unknown SDP [DCP] CDP\n");
    assert_eq!(read_sysfs(AC_DIR, "type"), "Mains\n");
    assert_eq!(read_sysfs(AC_DIR, "online"), "1\n");
    assert_eq!(read_sysfs(WIRELESS_DIR, "type"), "Wireless\n");
    assert_eq!(read_sysfs(WIRELESS_DIR, "online"), "0\n");
    assert_eq!(read_sysfs(WIRELESS_DIR, "present"), "0\n");

    // 2. Write fake capacity (77%) to sysfs. The override is applied before `write()` returns, so
    //    it is readable immediately, and the other attributes keep the real driver telemetry.
    std::fs::write(&capacity_path, "77\n").expect("failed to write simulated capacity");
    assert_eq!(read_sysfs(BATTERY_DIR, "capacity"), "77\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "status"), "Charging\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "health"), "Good\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "voltage_now"), "4100000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "charge_counter"), "2000000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "charge_full"), "4000000\n");
    assert_eq!(read_sysfs(BATTERY_DIR, "temp"), "250\n");
    assert_eq!(read_sysfs(AC_DIR, "online"), "1\n");

    // 3. Write a negative value (-1) to clear the override and restore the driver level (50%).
    //    Poll here: the update that `battery-manager` sent for the 77% override can still be in
    //    flight, and it briefly sets capacity back to 77 until the reconnect update arrives.
    std::fs::write(&capacity_path, "-1\n").expect("failed to write reconnect sentinel");
    wait_for_sysfs_value(&capacity_path, "50\n");
}
