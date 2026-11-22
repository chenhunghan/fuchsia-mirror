// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Agent session identity resolution and host-scoped target claim lockfile management.
//!
//! This crate binds Fuchsia target devices by serial number to AI coding agent sessions via
//! `<serial>.lock` files stored in `/run/user/<euid>/ffx/agent-locks` (falling back to
//! `/tmp/ffx-agent-locks-<euid>`, or overridden via `agent.lock_dir`).

mod error;
mod lock;
mod session;

pub use error::AgentLockError;
pub use lock::{LOCK_FILE_SUFFIX, claim_target, lock_dir, release_target, verify_target_ownership};
pub use session::{
    DEFAULT_TARGET_OWNERSHIP_ENABLED, is_invoked_by_agent, is_target_ownership_active,
    is_target_ownership_enabled, resolve_session_id,
};

#[cfg(test)]
mod tests;
