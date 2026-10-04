// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Invocation analytics and telemetry helpers for `ffx-uart-driver`.
//!
//! Provides event recording for tracking driver execution modes, serial transport
//! configuration, and tool usage across developer workflows.

use analytics::{GA4Value, add_custom_event};
use std::collections::BTreeMap;

/// Target category classification for telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetType {
    /// Physical serial TTY character device (e.g., `/dev/ttyUSB*`, `/dev/ttyACM*`, `/dev/pts/*`).
    Tty,
    /// UNIX domain stream socket (e.g., QEMU virtual serial port socket).
    Socket,
    /// Network TCP serial endpoint (e.g., DHUB `tcp:<host>:<port>`).
    Tcp,
    /// Any other custom or unclassified target endpoint string.
    Other,
}

impl TargetType {
    /// Returns the telemetry tag string for this target type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tty => "tty",
            Self::Socket => "socket",
            Self::Tcp => "tcp",
            Self::Other => "other",
        }
    }
}

/// Returns true if the string contains the substring "tty" where the character
/// immediately preceding "tty" (if any) is not alphabetic.
fn has_non_alphabetic_prefixed_tty(target_lower: &str) -> bool {
    for (idx, _) in target_lower.match_indices("tty") {
        if idx == 0 {
            return true;
        }
        if let Some(prev) = target_lower[..idx].chars().next_back() {
            if !prev.is_alphabetic() {
                return true;
            }
        }
    }
    false
}

/// Classifies a target specification string into a telemetry-safe categorical endpoint type
/// without including raw filesystem paths or network hostnames.
pub fn classify_target(target: &str) -> TargetType {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("tcp:") {
        TargetType::Tcp
    } else if lower.ends_with(".sock") || lower.contains("socket") {
        TargetType::Socket
    } else if has_non_alphabetic_prefixed_tty(&lower)
        || lower.contains("/dev/pts/")
        || lower.starts_with("/dev/serial/")
        || lower.starts_with("/dev/cu.")
    {
        TargetType::Tty
    } else {
        TargetType::Other
    }
}

/// Emits a custom analytics event noting that the `ffx-uart-driver` tool has been invoked.
///
/// Records the execution mode (`launcher` vs `daemon`), target endpoint classification,
/// baud rate, connection parameters, and invoking tool context (e.g. `fx`).
pub async fn emit_uart_driver_invoked_event(
    command: &crate::UartDriverCommand,
    invoker: Option<&str>,
) {
    let mode = if command.background { "launcher" } else { "daemon" };
    let target_type = classify_target(&command.target);

    let mut custom_dimensions = BTreeMap::new();
    custom_dimensions.insert("mode", GA4Value::from(mode));
    custom_dimensions.insert("target_type", GA4Value::from(target_type.as_str()));
    custom_dimensions.insert("baud", GA4Value::from(command.baud.get() as u64));
    custom_dimensions.insert("has_custom_socket", GA4Value::from(command.socket.is_some()));
    custom_dimensions.insert("no_retry", GA4Value::from(command.no_retry));
    if let Some(invoker) = invoker {
        custom_dimensions.insert("invoker", GA4Value::from(invoker));
    }

    let _ = add_custom_event(Some("ffx_uart_driver"), Some(mode), None, custom_dimensions).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_classify_target() {
        assert_eq!(classify_target("/dev/ttyUSB0"), TargetType::Tty);
        assert_eq!(classify_target("/dev/TTYUSB0"), TargetType::Tty);
        assert_eq!(classify_target("/dev/ttyACM1"), TargetType::Tty);
        assert_eq!(classify_target("/dev/pts/4"), TargetType::Tty);
        assert_eq!(classify_target("/dev/PTS/4"), TargetType::Tty);
        assert_eq!(classify_target("/dev/tty.usbserial-110"), TargetType::Tty);
        assert_eq!(classify_target("/dev/cu.usbserial-110"), TargetType::Tty);
        assert_eq!(
            classify_target("/dev/serial/by-id/usb-FTDI_FT232R_USB_UART-if00-port0"),
            TargetType::Tty
        );
        assert_eq!(
            classify_target("/dev/serial/by-path/pci-0000:00:14.0-usb-0:2:1.0-port0"),
            TargetType::Tty
        );
        assert_eq!(classify_target("tcp:127.0.0.1:8023"), TargetType::Tcp);
        assert_eq!(classify_target("tcp:myhost.corp.google.com:9000"), TargetType::Tcp);
        assert_eq!(classify_target("/tmp/qemu_serial.sock"), TargetType::Socket);
        assert_eq!(classify_target("/tmp/custom_socket"), TargetType::Socket);
        assert_eq!(classify_target("/home/user/tests/tty_tests/qemu.sock"), TargetType::Socket);
        assert_eq!(classify_target("ttyUSB0"), TargetType::Tty);
        assert_eq!(classify_target("/dev/usb-tty0"), TargetType::Tty);
        assert_eq!(classify_target("/dev/usb_tty0"), TargetType::Tty);
        assert_eq!(classify_target("/tmp/nutty"), TargetType::Other);
        assert_eq!(classify_target("/home/user/pretty"), TargetType::Other);
        assert_eq!(classify_target("/var/log/entity.log"), TargetType::Other);
        assert_eq!(classify_target("/home/user/pretty/ttyUSB0"), TargetType::Tty);
        assert_eq!(classify_target("custom_device"), TargetType::Other);
    }

    #[fuchsia::test]
    async fn test_emit_uart_driver_invoked_event() {
        let cmd1 = crate::UartDriverCommand {
            background: true,
            log_dir: None,
            target: "/dev/ttyUSB0".to_string(),
            socket: None,
            baud: std::num::NonZeroU32::new(115200).unwrap(),
            no_retry: false,
        };
        emit_uart_driver_invoked_event(&cmd1, Some("fx")).await;

        let cmd2 = crate::UartDriverCommand {
            background: false,
            log_dir: None,
            target: "tcp:localhost:8023".to_string(),
            socket: Some(std::path::PathBuf::from("/tmp/custom.sock")),
            baud: std::num::NonZeroU32::new(1_000_000).unwrap(),
            no_retry: true,
        };
        emit_uart_driver_invoked_event(&cmd2, None).await;
    }
}
