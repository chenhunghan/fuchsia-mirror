// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use thiserror::Error;

/// Errors returned by agent session identity resolution and target lockfile operations.
#[derive(Debug, Error)]
pub enum AgentLockError {
    /// Agent environment was detected, but no valid session identifier could be resolved.
    #[error(
        "Agent environment detected, but no agent session ID was found. \
         Please set $FFX_AGENT_SESSION_ID to a unique session identifier."
    )]
    MissingSessionId,

    /// Target serial number is empty or contains invalid path characters (`/` or `\0`).
    #[error("Invalid target serial number '{0}'.")]
    InvalidTarget(String),

    /// Target is not currently claimed.
    #[error("Target 'id:{0}' is not claimed. Claim it first via `ffx agent claim id:{0}`.")]
    NotClaimed(String),

    /// Target is claimed by another agent session.
    #[error(
        "Target 'id:{serial}' is currently claimed by agent session '{owner}'.\n\
         Please try communicating with the owning agent, or ask the user to run `ffx agent release id:{serial}`."
    )]
    ClaimedByOther {
        /// Serial number of the target.
        serial: String,
        /// Session ID of the conflicting owning agent.
        owner: String,
    },

    /// I/O error occurred during lockfile operations.
    #[error("I/O error accessing agent lockfile: {0}")]
    Io(#[from] std::io::Error),
}
