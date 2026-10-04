// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Error types for the target-side FDomain UART driver.

use thiserror::Error;

/// Errors produced during target-side UART driver operation.
#[derive(Error, Debug)]
pub enum TargetDriverError {
    /// FIDL channel or proxy communication failure.
    #[error("FIDL communication error: {0}")]
    Fidl(#[from] fidl::Error),
    /// Non-OK status code returned by the serial hardware driver.
    #[error("Serial driver error status: {0}")]
    DriverStatus(i32),
    /// Empty byte vector returned during handshake read.
    #[error("Serial read returned empty data")]
    EmptyRead,
    /// End-of-file reached when reading frames from the serial device.
    #[error("EOF from serial driver")]
    Eof,
    /// Protocol negotiation frame decoding or validation failure.
    #[error("Handshake error: {0}")]
    Handshake(#[from] uart_fpl::HandshakeError),
    /// Host and target share no supported framing protocol version.
    #[error("No common protocol negotiated")]
    NoCommonProtocol,
    /// Filesystem or I/O failure while discovering serial devices.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Serial device directory entry path is not valid UTF-8.
    #[error("Non-UTF-8 serial device path")]
    NonUtf8Path,
    /// No usable serial ports found under `/dev/class/serial`.
    #[error("No serial ports found")]
    NoSerialPorts,
    /// Serial write failed after exhausting all retry attempts.
    #[error("Failed to write to serial after exhausting retry attempts")]
    WriteRetriesExhausted,
    /// Host sent a control reset or control channel close frame.
    #[error("Host requested protocol reset")]
    ResetRequested,
    /// Host initiated a new handshake with a different session ID.
    #[error("Session ID changed")]
    SessionIdChanged,
    /// Internal ACK notification channel closed unexpectedly.
    #[error("ACK channel closed")]
    AckChannelClosed,
    /// Frame encoding or validation failure.
    #[error("Frame error: {0}")]
    Frame(#[from] uart_fpl::FrameError),
    /// Go-Back-N retransmission attempts exceeded the protocol limit.
    #[error("Retransmission limit exceeded: {0}")]
    RetransmissionLimit(#[from] uart_fpl::RetransmissionLimitExceeded),
    /// Received an unexpected sequence number while advancing the receiver window.
    #[error("Unexpected sequence number: {0}")]
    UnexpectedSeq(#[from] uart_fpl::UnexpectedSeqError),
    /// Internal mpsc channel send failure between bridge tasks.
    #[error("Internal channel communication error: {0}")]
    ChannelSend(String),
    /// Failed to connect to the target RCS `Connector` FIDL protocol.
    #[error("Failed to connect to RCS Connector: {0}")]
    RcsConnect(String),
}

/// Alias for results returned by the target UART driver.
pub type Result<T, E = TargetDriverError> = std::result::Result<T, E>;
