// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Subtool implementation for managing target claims by AI coding agents.
//!
//! Provides CLI subcommands to claim and release exclusive host-scoped target claims
//! for agent sessions.

mod args;

pub use args::{AgentCommand, AgentSubCommand, ClaimCommand, ReleaseCommand};

use discovery::query::TargetInfoQuery;
use ffx_agent::{
    AgentLockError, claim_target, is_invoked_by_agent, is_target_ownership_enabled, release_target,
    resolve_session_id,
};
use ffx_config::EnvironmentContext;
use ffx_writer::{ToolIO as _, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool, Result};
use schemars::JsonSchema;
use serde::Serialize;

/// Structured output schema emitted by `ffx agent`.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutput {
    /// Result of a target claim operation.
    Claim(ClaimOutput),
    /// Result of a target release operation.
    Release(ReleaseOutput),
}

/// Output data for `ffx agent claim`.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClaimOutput {
    /// Serial number of the target.
    pub serial: String,
    /// Canonical target specifier (`id:<serial>`).
    pub target: String,
    /// Session identity of the claiming agent.
    pub owner_identity: String,
}

/// Output data for `ffx agent release`.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseOutput {
    /// Serial number of the released target.
    pub serial: String,
    /// Canonical target specifier (`id:<serial>`).
    pub target: String,
}

impl std::fmt::Display for AgentOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claim(claim) => write!(
                f,
                "Successfully claimed target '{}' (session: {}).",
                claim.target, claim.owner_identity
            ),
            Self::Release(release) => write!(f, "Released claim on '{}'.", release.target),
        }
    }
}

/// Primary tool struct for `ffx agent`.
#[derive(FfxTool)]
#[target(None)]
pub struct AgentTool {
    #[command]
    cmd: AgentCommand,
    context: EnvironmentContext,
}

impl FfxMain for AgentTool {
    type Writer = VerifiedMachineWriter<AgentOutput>;

    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let output = match self.cmd.subcommand {
            AgentSubCommand::Claim(cmd) => handle_claim(&self.context, cmd)?,
            AgentSubCommand::Release(cmd) => handle_release(&self.context, cmd)?,
        };
        if matches!(output, AgentOutput::Claim(_)) && !is_target_ownership_enabled(&self.context) {
            writeln!(
                writer.stderr(),
                "Warning: claim recorded, but enforcement is disabled ({}=false).",
                ffx_config::keys::AGENT_TARGET_OWNERSHIP
            )
            .map_err(|e| fho::bug!(e))?;
        }
        writer.item(&output)?;
        Ok(())
    }
}

fn map_lock_err(err: AgentLockError) -> fho::Error {
    fho::Error::User(err.into())
}

fn parse_target_serial(target: &str) -> Result<String> {
    match TargetInfoQuery::try_from(target) {
        Ok(TargetInfoQuery::Id(serial)) if !serial.is_empty() => Ok(serial),
        _ => fho::return_user_error!(
            "Invalid target specifier '{target}'. Expected format 'id:<serial>' or 'serial:<serial>'."
        ),
    }
}

fn handle_claim(context: &EnvironmentContext, cmd: ClaimCommand) -> Result<AgentOutput> {
    let serial = parse_target_serial(&cmd.target)?;
    if !is_invoked_by_agent(context) {
        fho::return_user_error!(
            "`ffx agent claim` is intended for agent sessions; you can set $FFX_AGENT_SESSION_ID to claim manually, but this is not encouraged."
        );
    }
    let session_id = resolve_session_id(context).map_err(map_lock_err)?;
    claim_target(context, &serial, &session_id).map_err(map_lock_err)?;

    Ok(AgentOutput::Claim(ClaimOutput {
        target: format!("id:{serial}"),
        serial,
        owner_identity: session_id,
    }))
}

fn handle_release(context: &EnvironmentContext, cmd: ReleaseCommand) -> Result<AgentOutput> {
    let serial = parse_target_serial(&cmd.target)?;
    let session_id = if !cmd.unsafe_release && is_invoked_by_agent(context) {
        Some(resolve_session_id(context).map_err(map_lock_err)?)
    } else {
        None
    };

    release_target(context, &serial, session_id.as_deref()).map_err(map_lock_err)?;

    Ok(AgentOutput::Release(ReleaseOutput { target: format!("id:{serial}"), serial }))
}

#[cfg(test)]
mod tests;
