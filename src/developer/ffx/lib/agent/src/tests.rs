// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use assert_matches::assert_matches;
use ffx_config::environment::ExecutableKind;
use ffx_config::keys::{AGENT_LOCK_DIR, AGENT_TARGET_OWNERSHIP};
use ffx_config::{EnvironmentContext, TestEnv, test_env};
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::tempdir;

fn test_lock_env(lock_dir_path: &Path) -> TestEnv {
    test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .runtime_config(AGENT_LOCK_DIR, lock_dir_path.to_str().unwrap())
        .build()
        .unwrap()
}

#[fuchsia::test]
fn test_activation_criteria() {
    // 1. Default: feature flag disabled, non-agent environment -> inactive
    let env_default = test_env().build().unwrap();
    assert!(!is_target_ownership_enabled(&env_default.context));
    assert!(!is_invoked_by_agent(&env_default.context));
    assert!(!is_target_ownership_active(&env_default.context));

    // 2. Agent environment, but feature flag disabled -> inactive
    let env_agent_flag_off = test_env()
        .env_var("ANTIGRAVITY_AGENT", "1")
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "conv-123")
        .build()
        .unwrap();
    assert!(!is_target_ownership_enabled(&env_agent_flag_off.context));
    assert!(is_invoked_by_agent(&env_agent_flag_off.context));
    assert!(!is_target_ownership_active(&env_agent_flag_off.context));

    // 3. Feature flag enabled, but human invocation -> exempt / inactive
    let env_human_flag_on =
        test_env().runtime_config(AGENT_TARGET_OWNERSHIP, true).build().unwrap();
    assert!(is_target_ownership_enabled(&env_human_flag_on.context));
    assert!(!is_invoked_by_agent(&env_human_flag_on.context));
    assert!(!is_target_ownership_active(&env_human_flag_on.context));

    // 4. Feature flag enabled AND agent environment -> active
    let env_active = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("ANTIGRAVITY_AGENT", "1")
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "conv-123")
        .build()
        .unwrap();
    assert!(is_target_ownership_enabled(&env_active.context));
    assert!(is_invoked_by_agent(&env_active.context));
    assert!(is_target_ownership_active(&env_active.context));
    assert_eq!(resolve_session_id(&env_active.context).unwrap(), "conv-123");
}

#[fuchsia::test]
fn test_session_id_resolution_priority_and_missing_error() {
    // 1. $FFX_AGENT_SESSION_ID takes priority over $ANTIGRAVITY_CONVERSATION_ID
    let env_both = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("ANTIGRAVITY_AGENT", "1")
        .env_var("FFX_AGENT_SESSION_ID", "explicit-session-override")
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "agent-uuid-1234")
        .build()
        .unwrap();
    assert_eq!(resolve_session_id(&env_both.context).unwrap(), "explicit-session-override");

    // 2. Empty $FFX_AGENT_SESSION_ID falls back to $ANTIGRAVITY_CONVERSATION_ID
    let env_fallback = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("ANTIGRAVITY_AGENT", "1")
        .env_var("FFX_AGENT_SESSION_ID", "   ")
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "agent-uuid-1234")
        .build()
        .unwrap();
    assert_eq!(resolve_session_id(&env_fallback.context).unwrap(), "agent-uuid-1234");

    // 3. $CLAUDE_CODE_SESSION_ID is supported as fallback when Antigravity/explicit vars are unset
    let env_claude = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("CLAUDE_CODE_SESSION_ID", "claude-session-99")
        .build()
        .unwrap();
    assert_eq!(resolve_session_id(&env_claude.context).unwrap(), "claude-session-99");

    // 4. Agent environment (e.g., $GEMINI_CLI) with no session ID set returns fatal error
    let env_missing = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("GEMINI_CLI", "1")
        .build()
        .unwrap();
    let err = resolve_session_id(&env_missing.context).unwrap_err();
    assert_matches!(err, AgentLockError::MissingSessionId);
    assert!(err.to_string().contains("$FFX_AGENT_SESSION_ID"));
}

#[fuchsia::test]
fn test_lock_dir_bypasses_isolate_dir_and_supports_config_override() {
    let fake_run_user = tempdir().unwrap();
    let fake_tmp = tempdir().unwrap();

    assert_eq!(
        lock::default_lock_dir_with_bases(1000, fake_run_user.path(), fake_tmp.path()),
        fake_tmp.path().join("ffx-agent-locks-1000")
    );
    std::fs::create_dir_all(fake_run_user.path().join("1000")).unwrap();
    assert_eq!(
        lock::default_lock_dir_with_bases(1000, fake_run_user.path(), fake_tmp.path()),
        fake_run_user.path().join("1000/ffx/agent-locks")
    );

    let env_unconfigured = test_env().build().unwrap();
    let expected_default = lock_dir(&env_unconfigured.context);
    let isolate_data_path = env_unconfigured.context.get_data_path().unwrap();
    assert!(!expected_default.starts_with(&isolate_data_path));

    let env_strict =
        EnvironmentContext::strict(ExecutableKind::MainFfx, Default::default()).unwrap();
    assert_eq!(lock_dir(&env_strict), expected_default);

    let custom_dir = tempdir().unwrap();
    let env_override = test_env()
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .runtime_config(AGENT_LOCK_DIR, custom_dir.path().to_str().unwrap())
        .build()
        .unwrap();
    assert_eq!(lock_dir(&env_override.context), custom_dir.path());

    let mut strict_args = ffx_config::ConfigMap::new();
    strict_args.insert(
        "agent".to_string(),
        serde_json::json!({ "lock_dir": custom_dir.path().to_str().unwrap() }),
    );
    let env_strict_configured =
        EnvironmentContext::strict(ExecutableKind::MainFfx, strict_args).unwrap();
    assert_eq!(lock_dir(&env_strict_configured), custom_dir.path());
}

#[fuchsia::test]
fn test_serial_validation() {
    let dir = tempdir().unwrap();
    let env = test_lock_env(dir.path());

    for bad_serial in ["", "foo/bar", "foo\0bar"] {
        assert_matches!(
            claim_target(&env.context, bad_serial, "session-a"),
            Err(AgentLockError::InvalidTarget(_))
        );
    }

    // Vendor serials with colons, dashes, dots, or arbitrary length are accepted verbatim
    claim_target(&env.context, "vendor:123-456.789", "session-a").unwrap();
    verify_target_ownership(&env.context, "vendor:123-456.789", "session-a").unwrap();
    release_target(&env.context, "vendor:123-456.789", Some("session-a")).unwrap();
}

#[fuchsia::test]
fn test_claim_verify_and_release_lifecycle() {
    let dir = tempdir().unwrap();
    let env = test_lock_env(dir.path());

    // Initially unclaimed -> verify_target_ownership and release_target fail with NotClaimed
    assert_matches!(
        verify_target_ownership(&env.context, "12345ABC", "session-a"),
        Err(AgentLockError::NotClaimed(ref s)) if s == "12345ABC"
    );
    assert_matches!(
        release_target(&env.context, "12345ABC", Some("session-a")),
        Err(AgentLockError::NotClaimed(ref s)) if s == "12345ABC"
    );
    assert_matches!(
        claim_target(&env.context, "12345ABC", "   "),
        Err(AgentLockError::MissingSessionId)
    );

    // Claim target for session-a creates `<serial>.lock` containing the session ID
    claim_target(&env.context, "12345ABC", "session-a").unwrap();
    let on_disk_path = dir.path().join("12345ABC.lock");
    assert_eq!(std::fs::read_to_string(&on_disk_path).unwrap(), "session-a");

    // verify_target_ownership succeeds for session-a and fails for session-b with ClaimedByOther
    verify_target_ownership(&env.context, "12345ABC", "session-a").unwrap();
    assert_matches!(
        verify_target_ownership(&env.context, "12345ABC", "session-b"),
        Err(AgentLockError::ClaimedByOther { ref serial, ref owner })
            if serial == "12345ABC" && owner == "session-a"
    );

    // Re-claiming by the same session ("session-a") succeeds idempotently
    claim_target(&env.context, "12345ABC", "session-a").unwrap();

    // Claiming by another session ("session-b") fails with ClaimedByOther
    assert_matches!(
        claim_target(&env.context, "12345ABC", "session-b"),
        Err(AgentLockError::ClaimedByOther { ref serial, ref owner })
            if serial == "12345ABC" && owner == "session-a"
    );

    // Agent "session-b" cannot release "session-a"'s claim
    assert_matches!(
        release_target(&env.context, "12345ABC", Some("session-b")),
        Err(AgentLockError::ClaimedByOther { ref serial, ref owner })
            if serial == "12345ABC" && owner == "session-a"
    );

    // Release by owning session ("session-a") succeeds and removes the lockfile
    release_target(&env.context, "12345ABC", Some("session-a")).unwrap();
    assert!(!on_disk_path.exists());

    // Unconditional release (`None`) releases any session's claim and fails with NotClaimed when absent
    claim_target(&env.context, "EM-999", "subagent-1").unwrap();
    release_target(&env.context, "EM-999", None).unwrap();
    assert_matches!(
        release_target(&env.context, "EM-999", None),
        Err(AgentLockError::NotClaimed(ref s)) if s == "EM-999"
    );
}
