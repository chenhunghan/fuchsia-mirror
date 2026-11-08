// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host-side end-to-end tests for `ffx uart` against a QEMU emulator and
//! mock UNIX-socket UART targets.
//!
//! Layout:
//! - Tier 1: success flows (connect, disconnect, reconnect, list, probe).
//! - Tier 2: boundary, CLI validation, and metadata recovery cases.
//! - Tier 3: multi-step cross-feature sequences.
//! - Tier 4: emulator lifecycle workflows (reboot, parallel, already connected, stop).
//! - Tier 5: adversarial handshake frames.
//! - Tier 6: target identity drift.

mod harness;

use ffx_e2e_emu as _;
use harness::{
    MockDriverProcess, MockUartTarget, STALE_METADATA_JSON, TestEmulator, assert_success,
    connection_count, process_exists, wait_for_process_exit,
};
use std::os::unix::net::UnixListener;
use std::time::Duration;
use uart_driver_api::{ConnectionError, ConnectionStatus};

// ==================== TIER 1: Success Flows ====================

#[fuchsia::test]
async fn test_smoke_t1_connect_success() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;
    emu.assert_target_echo().await;
}

#[fuchsia::test]
async fn test_t1_connect_with_baud_rates() {
    let emu = TestEmulator::shared().await;
    for baud in ["9600", "38400", "57600", "115200"] {
        emu.reset().await;
        emu.connect_ok(&[&emu.emu_name, "--baud", baud]).await;
        emu.assert_connected(&emu.emu_name).await;
        emu.assert_target_echo().await;
    }
}

#[fuchsia::test]
async fn test_t1_connect_reconnect_when_disconnected() {
    let emu = TestEmulator::shared().await;
    emu.reconnect_target().await;
    emu.assert_target_echo().await;
}

#[fuchsia::test]
async fn test_smoke_t1_disconnect_success() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;
    emu.assert_target_echo().await;
    emu.disconnect_target().await;
}

#[fuchsia::test]
async fn test_smoke_t1_reconnect_success() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;
    let old_pid = emu.driver_pid(&emu.emu_name);

    emu.reconnect_target().await;
    assert_ne!(emu.driver_pid(&emu.emu_name), old_pid, "reconnect should restart the driver");
    emu.assert_target_echo().await;
}

#[fuchsia::test]
async fn test_t1_reconnect_by_id_and_with_baud() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    let first_pid = emu.driver_pid(&emu.emu_name);

    let id = emu.target_id(&emu.emu_name);
    emu.reconnect_ok(&[&id]).await;
    emu.assert_connected(&emu.emu_name).await;
    let second_pid = emu.driver_pid(&emu.emu_name);
    assert_ne!(second_pid, first_pid, "reconnect by ID should restart the driver");

    emu.reconnect_ok(&[&emu.emu_name, "--baud", "115200"]).await;
    emu.assert_connected(&emu.emu_name).await;
    assert_ne!(
        emu.driver_pid(&emu.emu_name),
        second_pid,
        "reconnect with --baud should restart the driver"
    );
    assert_eq!(emu.driver_baud(&emu.emu_name), Some(115200), "new baud should take effect");
}

#[fuchsia::test]
async fn test_t1_connect_and_disconnect_unix_socket() {
    let emu = TestEmulator::shared().await;
    let socket_path = emu.shared_data_dir.join("device_disc.sock");
    let _mock_target = MockUartTarget::spawn(&socket_path);
    let target = socket_path.to_string_lossy();

    emu.connect_ok(&[&target]).await;
    emu.assert_connected(&target).await;

    emu.disconnect_ok(&[&target]).await;
    emu.assert_disconnected(&target).await;
}

#[fuchsia::test]
async fn test_t1_list_empty() {
    let emu = TestEmulator::shared().await;
    let list = emu.list_connections().await;
    assert_eq!(connection_count(&list), 0, "list: {list:?}");
}

#[fuchsia::test]
async fn test_smoke_t1_list_one_connection() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    let output = emu.isolate.ffx(&["uart", "list"]).await.unwrap();
    assert_success(&output, "ffx uart list");
    let resolved = emu.resolved_target_path(&emu.emu_name);
    assert!(
        output.stdout.contains(&*resolved.to_string_lossy())
            || output.stdout.contains(&emu.emu_name),
        "list output: {}",
        output.stdout
    );
}

#[fuchsia::test]
async fn test_smoke_t1_probe_direct_success() {
    let emu = TestEmulator::shared().await;
    emu.probe_ok(&[&emu.emu_name]).await;
}

#[fuchsia::test]
async fn test_t1_probe_with_baud_and_while_connected() {
    let emu = TestEmulator::shared().await;
    emu.probe_ok(&[&emu.emu_name, "--baud", "115200"]).await;

    emu.connect_target().await;
    emu.probe_ok(&[&emu.emu_name]).await;
}

// ==================== TIER 2: Boundary and Error Cases ====================

#[fuchsia::test]
async fn test_t2_cli_argument_validation() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;

    // Invalid baud rates and missing targets are rejected across subcommands.
    emu.connect_err(&[]).await;
    emu.disconnect_err(&[]).await;
    emu.reconnect_err(&[]).await;

    for bad_baud in ["0", "-115200"] {
        emu.connect_err(&[target, "--baud", bad_baud]).await;
        emu.reconnect_err(&[target, "--baud", bad_baud]).await;
    }
    emu.probe_err(&[target, "--baud", "0"]).await;

    // Nonexistent targets and unconfigured baud combinations fail.
    emu.connect_err(&["nonexistent-baud-target", "--baud", "9999999"]).await;
    emu.probe_err(&["nonexistent-probe-target", "--baud", "9999999"]).await;
}

#[fuchsia::test]
async fn test_t2_connect_and_disconnect_state_errors() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;

    // Disconnecting when not connected fails.
    emu.disconnect_err(&[target]).await;

    // Second connect without --reconnect fails.
    emu.connect_target().await;
    emu.connect_err(&[target]).await;
}

#[fuchsia::test]
async fn test_t2_nonexistent_and_offline_targets() {
    let emu = TestEmulator::shared().await;
    emu.connect_err(&["/nonexistent/socket/path"]).await;
    emu.connect_err(&["offline-device"]).await;
    emu.probe_err(&["/nonexistent/socket/probe"]).await;
}

#[fuchsia::test]
async fn test_t2_connect_recovers_from_stale_or_corrupt_files() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;

    // 1. Stale PID in metadata + leftover placeholder socket.
    emu.write_metadata(target, STALE_METADATA_JSON);
    emu.write_placeholder_socket(target);
    emu.connect_target().await;

    // 2. Orphaned socket without metadata.
    emu.reset().await;
    emu.write_placeholder_socket(target);
    emu.connect_target().await;

    // 3. Orphaned stale metadata without socket (both plain connect and --reconnect).
    emu.reset().await;
    emu.write_metadata(target, STALE_METADATA_JSON);
    emu.connect_target().await;

    emu.reset().await;
    emu.write_metadata(target, STALE_METADATA_JSON);
    emu.reconnect_target().await;

    // 4. Corrupt non-JSON metadata.
    emu.reset().await;
    emu.write_metadata(target, "not-valid-json");
    emu.connect_target().await;
}

#[fuchsia::test]
async fn test_t2_disconnect_cleans_up_stale_or_corrupt_files() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;

    // 1. Stale PID metadata + placeholder socket.
    emu.write_metadata(target, STALE_METADATA_JSON);
    emu.write_placeholder_socket(target);
    emu.disconnect_target().await;

    // 2. Corrupt JSON metadata.
    emu.write_metadata(target, "not-valid-json");
    emu.disconnect_target().await;
}

#[fuchsia::test]
async fn test_t2_connect_permission_denied_readonly_dir() {
    let emu = TestEmulator::shared().await;
    let _guard = emu.make_uart_dir_readonly();
    emu.connect_err(&[&emu.emu_name]).await;
}

#[fuchsia::test]
async fn test_t2_list_ignores_invalid_metadata_files() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    for (name, contents) in [
        ("corrupt-target", "corrupt-json"),
        ("corrupt-2", "invalid-json"),
        ("empty-target", ""),
        ("no-pid-target", r#"{"target": "no-pid-target"}"#),
        ("plain-target", "plain text content"),
    ] {
        emu.write_metadata(name, contents);
    }

    let list = emu.list_connections().await;
    assert!(
        emu.list_contains_target(&list, &emu.emu_name),
        "live connection missing from {list:?}"
    );
}

#[fuchsia::test]
async fn test_t2_probe_mock_driver_states() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;
    let driver = MockDriverProcess::spawn();

    // 1. Driver currently connecting.
    emu.write_metadata_for_mock_driver(target, &driver, ConnectionStatus::Connecting);
    let res = emu.probe_err(&[target]).await;
    assert!(res.stderr.contains("currently connecting"), "stderr: {}", res.stderr);

    // 2. Driver reporting a connection error.
    emu.write_metadata_for_mock_driver(
        target,
        &driver,
        ConnectionStatus::Error(ConnectionError::TtyConfigureFailed {
            error: "Line dropped / framing error".to_string(),
        }),
    );
    let res = emu.probe_err(&[target]).await;
    assert!(res.stderr.contains("reporting error"), "stderr: {}", res.stderr);
}

#[fuchsia::test]
async fn test_t2_disconnect_and_probe_when_emulator_stopped() {
    let emu = TestEmulator::shared().await;
    let target = &emu.emu_name;
    emu.connect_target().await;
    emu.stop().await;

    // Driver may already have exited on EOF and cleaned up; either way no files remain.
    let _ = emu.disconnect(&[target]).await;
    emu.assert_disconnected(target).await;

    // Probing a stopped emulator fails.
    emu.probe_err(&[target]).await;
}

// ==================== TIER 3: Cross-feature sequences ====================

#[fuchsia::test]
async fn test_t3_reconnect_replacing_crashed_daemon() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    // Overwrite live metadata with a dead PID to simulate a crashed driver.
    emu.write_metadata(&emu.emu_name, STALE_METADATA_JSON);
    emu.reconnect_target().await;
}

#[fuchsia::test]
async fn test_t3_multiple_connect_list_disconnect() {
    let emu = TestEmulator::shared().await;
    let target1 = &emu.emu_name;
    let socket_path = emu.shared_data_dir.join("other_device.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
    let target2 = socket_path.to_string_lossy();

    emu.connect_ok(&[target1]).await;
    let _ = emu.connect(&[&target2]).await;
    let list = emu.list_connections().await;
    assert!(connection_count(&list) >= 2, "list: {list:?}");
    emu.assert_connected(target1).await;
    emu.assert_connected(&target2).await;

    emu.disconnect_target().await;
    emu.assert_connected(&target2).await;

    emu.disconnect_ok(&[&target2]).await;
    emu.assert_disconnected(&target2).await;
}

#[fuchsia::test]
async fn test_t3_connect_probe_reconnect_disconnect() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;
    emu.probe_ok(&[&emu.emu_name]).await;
    emu.reconnect_target().await;
    emu.probe_ok(&[&emu.emu_name]).await;
    emu.disconnect_target().await;
}

// ==================== TIER 4: Emulator lifecycle workflows ====================

#[fuchsia::test]
async fn test_t4_emulator_boot_and_echo() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    let list_out = emu.isolate.ffx(&["target", "list", &emu.emu_name]).await.unwrap();
    assert_success(&list_out, "ffx target list");
    assert!(list_out.stdout.contains(&emu.emu_name));

    assert!(
        emu.wait_for_target_echo(Duration::from_secs(60)).await,
        "Target did not respond to echo within 60 seconds"
    );
    emu.disconnect_target().await;
}

#[fuchsia::test]
async fn test_t4_multiple_emulators_in_parallel() {
    let emu1 = TestEmulator::private("t4_parallel1").await;
    let emu2 = TestEmulator::private("t4_parallel2").await;
    emu1.reset().await;

    emu1.connect_ok(&[&emu1.emu_name]).await;

    // Point emu2's ffx at emu1's shared data directory so both drivers are listed together.
    let output = emu2
        .isolate
        .ffx(&["config", "set", "shared_data", &emu1.shared_data_dir.to_string_lossy()])
        .await
        .unwrap();
    assert_success(&output, "ffx config set shared_data");

    emu2.connect_ok(&[&emu2.emu_name]).await;

    emu1.assert_connected(&emu1.emu_name).await;
    emu1.assert_connected(&emu2.emu_name).await;

    emu1.disconnect_ok(&[&emu1.emu_name]).await;
    emu2.disconnect_ok(&[&emu2.emu_name]).await;
}

#[fuchsia::test]
async fn test_t4_reboot_target_during_connection() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;

    emu.restart().await;
    assert!(
        emu.wait_for_target_echo(Duration::from_secs(60)).await,
        "Target did not respond to echo after reboot within 60 seconds"
    );
    emu.disconnect_target().await;
}

#[fuchsia::test]
async fn test_t4_connect_when_already_connected() {
    let emu = TestEmulator::shared().await;
    emu.connect_target().await;
    let driver_pid = emu.driver_pid(&emu.emu_name);

    let res = emu.connect_err(&[&emu.emu_name]).await;
    assert!(res.stderr.contains("Already connected"), "stderr: {}", res.stderr);
    assert_eq!(emu.driver_pid(&emu.emu_name), driver_pid, "existing driver should be untouched");
    emu.assert_connected(&emu.emu_name).await;
}

#[fuchsia::test]
async fn test_t4_driver_exits_on_emulator_stop() {
    let emu = TestEmulator::private("t4_driver_exit").await;
    emu.connect_target().await;

    let driver_pid = emu.driver_pid(&emu.emu_name);
    assert!(process_exists(driver_pid), "Driver process should be running");

    emu.stop().await;

    assert!(
        wait_for_process_exit(driver_pid, Duration::from_secs(10)),
        "Driver (PID {driver_pid}) failed to exit after emulator stopped"
    );
    emu.assert_disconnected(&emu.emu_name).await;
}

// ==================== TIER 5: Adversarial handshakes ====================

async fn assert_connect_rejects_bad_handshake<F>(socket_name: &str, mutate_response: F)
where
    F: FnOnce(Vec<u8>) -> Vec<u8> + Send + 'static,
{
    let emu = TestEmulator::shared().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let socket_path = temp_dir.path().join(socket_name);
    let _mock_target = MockUartTarget::spawn_with(&socket_path, mutate_response);
    let target = socket_path.to_string_lossy().into_owned();

    emu.connect_err(&[&target]).await;

    let list = emu.list_connections().await;
    let entry = emu
        .find_connection(&list, &target)
        .unwrap_or_else(|| panic!("Expected target {target} to be in list: {list:?}"));
    let status = entry.get("status").and_then(|s| s.as_str());
    assert_ne!(status, Some("Connected"), "target {target} should not be Connected: {list:?}");

    emu.disconnect_ok(&[&target]).await;
    emu.assert_disconnected(&target).await;
}

#[fuchsia::test]
async fn test_t5_connect_adversarial_bad_handshake_sync() {
    assert_connect_rejects_bad_handshake("bad_sync.sock", |mut frame| {
        frame[0] = 0x99;
        frame[1] = 0x99;
        frame
    })
    .await;
}

#[fuchsia::test]
async fn test_t5_connect_adversarial_bad_handshake_checksum() {
    assert_connect_rejects_bad_handshake("bad_checksum.sock", |mut frame| {
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        frame
    })
    .await;
}

// ==================== TIER 6: Target identity drift ====================

#[fuchsia::test]
async fn test_t6_driver_cleanup_on_target_id_drift() {
    let emu = TestEmulator::shared().await;

    let temp_dir = tempfile::tempdir().unwrap();
    let mock_socket_1 = temp_dir.path().join("mock_1.sock");
    let mock_socket_2 = temp_dir.path().join("mock_2.sock");
    let symlink_path = temp_dir.path().join("drift_target.sock");
    let _mock_target_1 = MockUartTarget::spawn(&mock_socket_1);
    let _mock_target_2 = MockUartTarget::spawn(&mock_socket_2);

    // Connect through a symlink that initially resolves to mock target 1.
    std::os::unix::fs::symlink(&mock_socket_1, &symlink_path).unwrap();
    let target = symlink_path.to_string_lossy().into_owned();
    emu.connect_ok(&[&target]).await;
    emu.assert_connected(&target).await;
    let old_driver_pid = emu.driver_pid(&target);
    assert!(process_exists(old_driver_pid), "Driver process should be running");

    // Re-point the symlink at mock target 2 and reconnect: the old driver must be
    // replaced by a new one under the same (symlink-derived) target ID.
    std::fs::remove_file(&symlink_path).unwrap();
    std::os::unix::fs::symlink(&mock_socket_2, &symlink_path).unwrap();
    emu.reconnect_ok(&[&target]).await;

    assert!(
        wait_for_process_exit(old_driver_pid, Duration::from_secs(5)),
        "Old driver (PID {old_driver_pid}) failed to terminate on reconnect after target drift"
    );
    emu.assert_connected(&target).await;
    assert_ne!(emu.driver_pid(&target), old_driver_pid, "New driver should have a new PID");
}
