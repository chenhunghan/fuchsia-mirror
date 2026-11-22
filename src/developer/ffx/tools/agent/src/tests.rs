// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use ffx_config::keys::{AGENT_LOCK_DIR, AGENT_TARGET_OWNERSHIP};
use ffx_config::test_env;
use ffx_writer::TestBuffers;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[fuchsia::test]
async fn test_claim_subcommand_success_and_idempotence() {
    let dir = tempdir().unwrap();
    let env = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "test-session-1")
        .build()
        .unwrap();

    let tool = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand { target: "id:DUT-123".to_string() }),
        },
        context: env.context.clone(),
    };

    let buffers = TestBuffers::default();
    let machine_writer = VerifiedMachineWriter::new_test(None, &buffers);
    tool.main(machine_writer).await.unwrap();

    let output_str = buffers.into_stdout_str();
    assert!(
        output_str.contains("Successfully claimed target 'id:DUT-123' (session: test-session-1).")
    );
    assert!(dir.path().join("DUT-123.lock").exists());

    // Idempotent re-claim by same session succeeds
    let tool_repeat = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand { target: "id:DUT-123".to_string() }),
        },
        context: env.context.clone(),
    };
    let buffers_repeat = TestBuffers::default();
    tool_repeat.main(VerifiedMachineWriter::new_test(None, &buffers_repeat)).await.unwrap();
    let out_repeat = buffers_repeat.into_stdout_str();
    assert!(out_repeat.contains("Successfully claimed target 'id:DUT-123'"));
}

#[fuchsia::test]
async fn test_claim_invalid_target_specifier() {
    let dir = tempdir().unwrap();
    let env = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "test-session")
        .build()
        .unwrap();

    let tool = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand {
                target: "DUT-RAW-SERIAL".to_string(),
            }),
        },
        context: env.context,
    };

    let buffers = TestBuffers::default();
    let err = tool.main(VerifiedMachineWriter::new_test(None, &buffers)).await.unwrap_err();
    assert!(err.to_string().contains("Expected format 'id:<serial>'"));
    assert!(!err.to_string().contains("I/O error"));
}

#[fuchsia::test]
async fn test_claim_contention_error_reporting() {
    let dir = tempdir().unwrap();
    let env_a = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "session-owner")
        .build()
        .unwrap();

    claim_target(&env_a.context, "CONFLICT-DUT", "session-owner").unwrap();

    let env_b = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "session-contender")
        .build()
        .unwrap();

    let tool = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand {
                target: "id:CONFLICT-DUT".to_string(),
            }),
        },
        context: env_b.context,
    };

    let buffers = TestBuffers::default();
    let err = tool.main(VerifiedMachineWriter::new_test(None, &buffers)).await.unwrap_err();
    let err_str = err.to_string();
    assert!(err_str.contains(
        "Target 'id:CONFLICT-DUT' is currently claimed by agent session 'session-owner'"
    ));
    assert!(err_str.contains("ffx agent release id:CONFLICT-DUT"));
}

#[fuchsia::test]
async fn test_release_subcommand_and_ownership_checks() {
    let dir = tempdir().unwrap();
    let env = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("ANTIGRAVITY_CONVERSATION_ID", "agent-release-test")
        .build()
        .unwrap();

    // 1. Releasing an unclaimed target errors cleanly
    let tool_unclaimed = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:DEV-A".to_string(),
                unsafe_release: false,
            }),
        },
        context: env.context.clone(),
    };
    let buffers = TestBuffers::default();
    let err =
        tool_unclaimed.main(VerifiedMachineWriter::new_test(None, &buffers)).await.unwrap_err();
    assert!(err.to_string().contains("Target 'id:DEV-A' is not claimed"));

    // 2. Claim target and release it as the owning agent
    claim_target(&env.context, "DEV-A", "agent-release-test").unwrap();
    let tool_release = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:DEV-A".to_string(),
                unsafe_release: false,
            }),
        },
        context: env.context.clone(),
    };
    let buffers_release = TestBuffers::default();
    tool_release.main(VerifiedMachineWriter::new_test(None, &buffers_release)).await.unwrap();
    assert!(buffers_release.into_stdout_str().contains("Released claim on 'id:DEV-A'."));
    assert!(!dir.path().join("DEV-A.lock").exists());
}

#[fuchsia::test]
async fn test_human_release_and_unsafe_override() {
    let dir = tempdir().unwrap();
    let env_human =
        test_env().runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap()).build().unwrap();

    // Human can release a target claimed by any agent session without --unsafe
    claim_target(&env_human.context, "SESSION-1-DUT", "session-1").unwrap();
    let tool_human = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:SESSION-1-DUT".to_string(),
                unsafe_release: false,
            }),
        },
        context: env_human.context.clone(),
    };
    let buffers_human = TestBuffers::default();
    tool_human.main(VerifiedMachineWriter::new_test(None, &buffers_human)).await.unwrap();
    assert!(buffers_human.into_stdout_str().contains("Released claim on 'id:SESSION-1-DUT'."));
    assert!(!dir.path().join("SESSION-1-DUT.lock").exists());

    // Target claimed by session-1
    claim_target(&env_human.context, "LOCKED-DUT", "session-1").unwrap();
    let lock_file = dir.path().join("LOCKED-DUT.lock");
    assert!(lock_file.exists());

    // Agent session-2 trying to release session-1's target without --unsafe fails
    let env_agent2 = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "session-2")
        .build()
        .unwrap();
    let tool_safe = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:LOCKED-DUT".to_string(),
                unsafe_release: false,
            }),
        },
        context: env_agent2.context.clone(),
    };
    let buffers_safe = TestBuffers::default();
    let err =
        tool_safe.main(VerifiedMachineWriter::new_test(None, &buffers_safe)).await.unwrap_err();
    assert!(err.to_string().contains("currently claimed by agent session 'session-1'"));

    // Agent session-2 with --unsafe succeeds
    let tool_unsafe = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:LOCKED-DUT".to_string(),
                unsafe_release: true,
            }),
        },
        context: env_agent2.context,
    };
    let buffers_unsafe = TestBuffers::default();
    tool_unsafe.main(VerifiedMachineWriter::new_test(None, &buffers_unsafe)).await.unwrap();
    assert!(buffers_unsafe.into_stdout_str().contains("Released claim on 'id:LOCKED-DUT'."));
    assert!(!lock_file.exists());
}

#[fuchsia::test]
async fn test_machine_output_format() {
    let dir = tempdir().unwrap();
    let env = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "session-machine")
        .build()
        .unwrap();

    // 1. Claim machine output
    let tool_claim = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand {
                target: "serial:MACHINE-DUT".to_string(),
            }),
        },
        context: env.context.clone(),
    };

    let buffers_claim = TestBuffers::default();
    let machine_writer =
        VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &buffers_claim);
    tool_claim.main(machine_writer).await.unwrap();

    let json_claim_str = buffers_claim.into_stdout_str();
    let val_claim: serde_json::Value = serde_json::from_str(&json_claim_str).unwrap();
    VerifiedMachineWriter::<AgentOutput>::verify_schema(&val_claim)
        .expect("Claim JSON must match schema");
    assert_eq!(val_claim["claim"]["serial"], "MACHINE-DUT");
    assert_eq!(val_claim["claim"]["target"], "id:MACHINE-DUT");
    assert_eq!(val_claim["claim"]["owner_identity"], "session-machine");

    // 2. Release machine output
    let tool_release = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Release(ReleaseCommand {
                target: "id:MACHINE-DUT".to_string(),
                unsafe_release: false,
            }),
        },
        context: env.context,
    };
    let buffers_release = TestBuffers::default();
    tool_release
        .main(VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &buffers_release))
        .await
        .unwrap();
    let json_release_str = buffers_release.into_stdout_str();
    let val_release: serde_json::Value = serde_json::from_str(&json_release_str).unwrap();
    VerifiedMachineWriter::<AgentOutput>::verify_schema(&val_release)
        .expect("Release JSON must match schema");
    assert_eq!(val_release["release"]["serial"], "MACHINE-DUT");
    assert_eq!(val_release["release"]["target"], "id:MACHINE-DUT");
}

#[fuchsia::test]
async fn test_claim_non_agent_error() {
    let dir = tempdir().unwrap();
    let env =
        test_env().runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap()).build().unwrap();

    let tool = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand { target: "id:HUMAN-DUT".to_string() }),
        },
        context: env.context,
    };

    let buffers = TestBuffers::default();
    let err = tool.main(VerifiedMachineWriter::new_test(None, &buffers)).await.unwrap_err();
    assert!(
        err.to_string().contains(
            "`ffx agent claim` is intended for agent sessions; you can set $FFX_AGENT_SESSION_ID to claim manually, but this is not encouraged."
        )
    );
    assert!(!dir.path().join("HUMAN-DUT.lock").exists());
}

#[fuchsia::test]
async fn test_claim_enforcement_disabled_warning() {
    let dir = tempdir().unwrap();

    // 1. Default (agent.target_ownership=false) emits a warning on stderr
    let env_disabled = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .env_var("FFX_AGENT_SESSION_ID", "session-warn")
        .build()
        .unwrap();
    let tool_disabled = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand { target: "id:WARN-DUT".to_string() }),
        },
        context: env_disabled.context,
    };
    let buffers_disabled = TestBuffers::default();
    tool_disabled.main(VerifiedMachineWriter::new_test(None, &buffers_disabled)).await.unwrap();
    let (stdout_disabled, stderr_disabled) = buffers_disabled.into_strings();
    assert!(stdout_disabled.contains("Successfully claimed target 'id:WARN-DUT'"));
    assert!(stderr_disabled.contains(
        "Warning: claim recorded, but enforcement is disabled (agent.target_ownership=false)."
    ));

    // 2. Enabled (agent.target_ownership=true) does not emit a warning on stderr
    let env_enabled = test_env()
        .runtime_config(AGENT_LOCK_DIR, dir.path().to_str().unwrap())
        .runtime_config(AGENT_TARGET_OWNERSHIP, true)
        .env_var("FFX_AGENT_SESSION_ID", "session-no-warn")
        .build()
        .unwrap();
    let tool_enabled = AgentTool {
        cmd: AgentCommand {
            subcommand: AgentSubCommand::Claim(ClaimCommand {
                target: "id:ENFORCED-DUT".to_string(),
            }),
        },
        context: env_enabled.context,
    };
    let buffers_enabled = TestBuffers::default();
    tool_enabled.main(VerifiedMachineWriter::new_test(None, &buffers_enabled)).await.unwrap();
    let (stdout_enabled, stderr_enabled) = buffers_enabled.into_strings();
    assert!(stdout_enabled.contains("Successfully claimed target 'id:ENFORCED-DUT'"));
    assert_eq!(stderr_enabled, "");
}
