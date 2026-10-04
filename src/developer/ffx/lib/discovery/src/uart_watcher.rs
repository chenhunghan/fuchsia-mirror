// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Passive filesystem watcher for discovering active UART target connections.
//!
//! Monitors the FFX shared UART runtime directory for daemon metadata files
//! (`ffx_uart_*.json`), emitting target discovery events as connections are
//! created, verified, or closed.

use crate::TargetEvent;
use crate::error::Error;
use crate::events::{TargetHandle, TargetState};
use crate::instance_watcher::{InstanceSource, InstanceWatcher, is_pid_running};
use addr::TargetAddr;
use futures::channel::mpsc::UnboundedSender;
use std::path::{Path, PathBuf};
use uart_driver_api::{ConnectionMetadata, ConnectionStatus, METADATA_FILE_EXTENSION};

const METADATA_FILE_PREFIX: &str = "ffx_uart_";

/// Watched-based discovery mechanism for UART target connections.
///
/// Monitors a dedicated directory (typically `<ffx_shared_data_path>/ffx_uart`)
/// for connection metadata files matching `ffx_uart_*.json`. Emits `TargetEvent::Added`
/// when a driver connection is established and alive, and `TargetEvent::Removed`
/// when it disconnects or is deleted.
pub struct UartWatcher {
    _watcher: InstanceWatcher,
}

impl std::fmt::Debug for UartWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UartWatcher").finish_non_exhaustive()
    }
}

fn uart_instance_id(path: &Path) -> Option<&str> {
    if path.extension() != Some(std::ffi::OsStr::new(METADATA_FILE_EXTENSION)) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if stem.starts_with(METADATA_FILE_PREFIX) && stem.len() > METADATA_FILE_PREFIX.len() {
        Some(stem)
    } else {
        None
    }
}

fn is_uart_metadata_file(path: &Path) -> bool {
    uart_instance_id(path).is_some()
}

/// Reads and validates UART connection metadata from a given file path.
///
/// Returns `Some(TargetHandle)` if the file contains valid JSON metadata, the
/// associated driver daemon process is actively running (`is_pid_running`), and
/// the connection status is `Connected`. Returns `None` if the driver process
/// has terminated or the target is in a non-connected state, causing the watcher
/// to emit a `Removed` event for the target.
fn read_target_from_metadata_file(path: &Path) -> Option<TargetHandle> {
    if !is_uart_metadata_file(path) {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let meta = serde_json::from_str::<ConnectionMetadata>(&content).ok()?;

    if !is_pid_running(meta.pid) {
        return None;
    }

    if meta.status != ConnectionStatus::Connected {
        return None;
    }

    Some(TargetHandle {
        node_name: meta.nodename,
        state: TargetState::Product {
            addrs: vec![TargetAddr::Uart(meta.target)],
            serial: meta.serial,
        },
        manual: false,
    })
}

/// Implementation of [`InstanceSource`] for discovering targets from UART driver metadata.
///
/// Extracts target identifiers and reads [`TargetHandle`] definitions from
/// JSON connection files matching `ffx_uart_<hash>.json`.
#[derive(Debug)]
pub struct UartSource;

impl InstanceSource for UartSource {
    fn instance_id_from_path(&self, root: &Path, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(root).ok()?;
        if rel.parent() != Some(Path::new("")) {
            return None;
        }
        uart_instance_id(rel).map(str::to_owned)
    }

    fn read_target_handle(&self, _root: &Path, path: &Path) -> Option<TargetHandle> {
        read_target_from_metadata_file(path)
    }
}

impl UartWatcher {
    /// Creates a new `UartWatcher` that watches the specified `socket_dir`.
    pub(crate) fn new(
        socket_dir: PathBuf,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self, Error> {
        let watcher = InstanceWatcher::new(socket_dir, sender, UartSource, |path, err| {
            Error::UartWatcher { path, err }
        })?;
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::num::NonZeroU32;
    use uart_driver_api::UartProtocol;

    fn write_test_meta(
        path: &Path,
        pid: u32,
        status: ConnectionStatus,
        nodename: Option<String>,
        serial: Option<String>,
    ) {
        let meta = ConnectionMetadata {
            pid,
            target: "/dev/ttyUSB0".to_string(),
            status,
            id: Some("0123456789abcdef".to_string()),
            baud: NonZeroU32::new(115200),
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename,
            serial,
        };
        let content = serde_json::to_string(&meta).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[fuchsia::test]
    async fn test_uart_watcher_initial_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let meta_path = root.join("ffx_uart_0123456789abcdef.json");

        write_test_meta(
            &meta_path,
            std::process::id(),
            ConnectionStatus::Connected,
            Some("initial-target".to_string()),
            Some("SN-12345".to_string()),
        );

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _watcher = UartWatcher::new(root, tx).expect("create watcher");

        let event = rx.next().await.expect("received event");
        match event {
            TargetEvent::Added(handle) => {
                assert_eq!(handle.node_name.as_deref(), Some("initial-target"));
                assert_eq!(
                    handle.state,
                    TargetState::Product {
                        addrs: vec![TargetAddr::Uart("/dev/ttyUSB0".to_string())],
                        serial: Some("SN-12345".to_string()),
                    }
                );
            }
            _ => panic!("expected Added event, got: {:?}", event),
        }
    }

    async fn assert_watcher_event(
        rx: &mut futures::channel::mpsc::UnboundedReceiver<TargetEvent>,
        expected_nodename: &str,
        is_add: bool,
    ) {
        let event = rx.next().await.expect("received event");
        match (event, is_add) {
            (TargetEvent::Added(handle), true) => {
                assert_eq!(handle.node_name.as_deref(), Some(expected_nodename));
            }
            (TargetEvent::Removed(handle), false) => {
                assert_eq!(handle.node_name.as_deref(), Some(expected_nodename));
            }
            (other, _) => panic!("Unexpected event for {expected_nodename}: {other:?}"),
        }
    }

    #[fuchsia::test]
    async fn test_uart_watcher_drain_and_watch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let meta_path = root.join("ffx_uart_0123456789abcdef.json");

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _watcher = UartWatcher::new(root, tx).expect("create watcher");

        // Dynamically add a connected target file
        write_test_meta(
            &meta_path,
            std::process::id(),
            ConnectionStatus::Connected,
            Some("dynamic-target".to_string()),
            Some("SN-DYN".to_string()),
        );
        assert_watcher_event(&mut rx, "dynamic-target", true).await;

        // Delete the file
        std::fs::remove_file(&meta_path).unwrap();
        assert_watcher_event(&mut rx, "dynamic-target", false).await;
    }

    #[fuchsia::test]
    async fn test_uart_watcher_status_change_to_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let meta_path = root.join("ffx_uart_0123456789abcdef.json");

        write_test_meta(
            &meta_path,
            std::process::id(),
            ConnectionStatus::Connected,
            Some("failing-target".to_string()),
            None,
        );

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _watcher = UartWatcher::new(root, tx).expect("create watcher");

        assert_watcher_event(&mut rx, "failing-target", true).await;

        // Update status to Connecting
        write_test_meta(
            &meta_path,
            std::process::id(),
            ConnectionStatus::Connecting,
            Some("failing-target".to_string()),
            None,
        );

        assert_watcher_event(&mut rx, "failing-target", false).await;
    }

    #[fuchsia::test]
    async fn test_uart_watcher_ignores_socket_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let meta_path = root.join("ffx_uart_0123456789abcdef.json");
        let sock_path = root.join("ffx_uart_0123456789abcdef.sock");

        // Write both .sock and .json files
        std::fs::write(&sock_path, b"").unwrap();
        write_test_meta(
            &meta_path,
            std::process::id(),
            ConnectionStatus::Connected,
            Some("socket-test-target".to_string()),
            Some("SN-SOCK".to_string()),
        );

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _watcher = UartWatcher::new(root, tx).expect("create watcher");

        // Exactly one Added event should be emitted
        assert_watcher_event(&mut rx, "socket-test-target", true).await;

        // Removing the .sock file should NOT emit a Removed event
        std::fs::remove_file(&sock_path).unwrap();

        // Adding another target confirms no intermediate Removed event was queued
        let meta2_path = temp.path().join("ffx_uart_fedcba9876543210.json");
        write_test_meta(
            &meta2_path,
            std::process::id(),
            ConnectionStatus::Connected,
            Some("second-target".to_string()),
            None,
        );
        assert_watcher_event(&mut rx, "second-target", true).await;
    }

    #[fuchsia::test]
    fn test_stopped_or_connecting_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let meta_path = temp.path().join("ffx_uart_test.json");

        // Dead PID
        write_test_meta(&meta_path, 0, ConnectionStatus::Connected, None, None);
        assert!(read_target_from_metadata_file(&meta_path).is_none());

        // Connecting status
        write_test_meta(&meta_path, std::process::id(), ConnectionStatus::Connecting, None, None);
        assert!(read_target_from_metadata_file(&meta_path).is_none());
    }

    #[test]
    fn test_uart_instance_id() {
        assert_eq!(uart_instance_id(Path::new("ffx_uart_1234.json")), Some("ffx_uart_1234"));
        assert_eq!(
            uart_instance_id(Path::new("/some/dir/ffx_uart_1234.json")),
            Some("ffx_uart_1234")
        );
        // Empty ID after prefix should be rejected
        assert_eq!(uart_instance_id(Path::new("ffx_uart_.json")), None);
        // Wrong prefix
        assert_eq!(uart_instance_id(Path::new("other_1234.json")), None);
        // Wrong extension
        assert_eq!(uart_instance_id(Path::new("ffx_uart_1234.sock")), None);
        assert_eq!(uart_instance_id(Path::new("ffx_uart_1234.txt")), None);
    }
}
