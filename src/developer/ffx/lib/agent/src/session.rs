// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::AgentLockError;
use ffx_config::EnvironmentContext;

pub use agents::is_invoked_by_agent;

/// Default value for the `agent.target_ownership` feature flag.
pub const DEFAULT_TARGET_OWNERSHIP_ENABLED: bool = false;

/// Returns `true` if the `agent.target_ownership` feature flag is enabled in `env`.
pub fn is_target_ownership_enabled(env: &EnvironmentContext) -> bool {
    env.get::<bool, _>(ffx_config::keys::AGENT_TARGET_OWNERSHIP)
        .unwrap_or(DEFAULT_TARGET_OWNERSHIP_ENABLED)
}

/// Returns `true` if agent target ownership semantics are active for `env`.
pub fn is_target_ownership_active(env: &EnvironmentContext) -> bool {
    is_target_ownership_enabled(env) && agents::is_invoked_by_agent(env)
}

/// Resolves the calling agent's session ID from `$FFX_AGENT_SESSION_ID`,
/// `$ANTIGRAVITY_CONVERSATION_ID`, or `$CLAUDE_CODE_SESSION_ID` (in that order).
///
/// # Errors
///
/// Returns [`AgentLockError::MissingSessionId`] if none of the session ID
/// environment variables contain a non-empty value.
pub fn resolve_session_id(env: &EnvironmentContext) -> Result<String, AgentLockError> {
    agents::resolve_session_id(env).ok_or(AgentLockError::MissingSessionId)
}
