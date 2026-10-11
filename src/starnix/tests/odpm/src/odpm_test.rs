// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration tests for the Starnix Google ODPM module.

use std::os::unix::fs::PermissionsExt;

const IIO_DEVICE_DIR: &str = "/sys/bus/iio/devices/iio:device0";
const PLATFORM_DEVICE_DIR: &str = "/sys/devices/platform/cpm/cpm:ODPM/iio:device0";
const ENOSYS: i32 = 38;

#[test]
fn test_odpm_device_dir_exists() {
    assert!(std::fs::metadata(IIO_DEVICE_DIR).is_ok(), "{IIO_DEVICE_DIR} does not exist");
    assert!(std::fs::metadata(PLATFORM_DEVICE_DIR).is_ok(), "{PLATFORM_DEVICE_DIR} does not exist");
}

#[test]
fn test_odpm_iio_bus_symlink() {
    let target = std::fs::read_link(IIO_DEVICE_DIR).expect("failed to read IIO symlink");
    assert_eq!(target.to_str().unwrap(), "../../../devices/platform/cpm/cpm:ODPM/iio:device0");
}

#[test]
fn test_odpm_subsystem_symlink() {
    let target = std::fs::read_link(format!("{PLATFORM_DEVICE_DIR}/subsystem"))
        .expect("failed to read subsystem symlink");
    assert_eq!(target.to_str().unwrap(), "../../../../../bus/iio");
}

#[test]
fn test_odpm_name() {
    let name = std::fs::read_to_string(format!("{IIO_DEVICE_DIR}/name"))
        .expect("failed to read ODPM name file");
    assert_eq!(name, "cpm:ODPM\n");

    let platform_name = std::fs::read_to_string(format!("{PLATFORM_DEVICE_DIR}/name"))
        .expect("failed to read ODPM name file via platform path");
    assert_eq!(platform_name, "cpm:ODPM\n");
}

#[test]
fn test_odpm_enabled_rails() {
    let enabled_rails = std::fs::read_to_string(format!("{IIO_DEVICE_DIR}/enabled_rails"))
        .expect("failed to read ODPM enabled_rails file");
    assert_eq!(enabled_rails, "CH0[S1M_VDD_AMB]:amb\nCH1[S2M_VDD_CPU2]:cpu2\n");
}

#[test]
fn test_odpm_energy_value() {
    let energy_val = std::fs::read_to_string(format!("{IIO_DEVICE_DIR}/energy_value"))
        .expect("failed to read ODPM energy_value file");
    assert_eq!(
        energy_val,
        "t=1279315\nCH0(T=1279315)[S1M_VDD_AMB], 51899976\nCH1(T=1279315)[S2M_VDD_CPU2], 185487825\n"
    );
}

#[test]
fn test_odpm_enabled_rails_permissions() {
    let metadata = std::fs::metadata(format!("{IIO_DEVICE_DIR}/enabled_rails"))
        .expect("failed to get metadata for enabled_rails file");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o444);
}

#[test]
fn test_odpm_enabled_rails_write_fails() {
    let err = std::fs::write(format!("{IIO_DEVICE_DIR}/enabled_rails"), "test")
        .expect_err("write to enabled_rails should fail");
    assert_eq!(err.raw_os_error(), Some(ENOSYS));
}
