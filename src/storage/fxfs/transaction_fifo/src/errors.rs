// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use thiserror::Error;

/// Errors returned by [`crate::FifoReader`] and [`crate::FifoWriter`] operations.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FifoError {
    /// An underlying Zircon VMO or VMAR operation failed.
    #[error("VMO error: {0}")]
    Vmo(#[from] zx::Status),

    /// The circular buffer has no free slots; the caller should wait for room before retrying.
    #[error("FIFO is full; block until space is available")]
    Full,

    /// The VMO size, slot size, capacity, or serialized payload is invalid.
    #[error("Slot size or capacity mismatch/invalid")]
    InvalidConfiguration,

    /// Insufficient reserved metadata space is available for a non-borrowing transaction.
    #[error("Insufficient metadata space available")]
    NoSpace,

    /// Shared memory state was modified in violation of the FIFO protocol (peer tampering).
    #[error("Invalid FIFO or metadata space state (peer tampering detected)")]
    InvalidState,

    /// The FIFO has been closed or fused due to an unrecoverable error.
    #[error("FIFO is in an unrecoverable bad/fused state")]
    BadState,
}
