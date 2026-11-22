// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Argument definitions for the `ffx agent` tool.

use argh::{ArgsInfo, FromArgs};

/// Root command structure for `ffx agent`.
#[derive(ArgsInfo, FromArgs, Clone, Debug, PartialEq, Eq)]
#[argh(
    subcommand,
    name = "agent",
    description = "Manage Fuchsia target ownership claims for AI agent sessions."
)]
pub struct AgentCommand {
    #[argh(subcommand)]
    pub subcommand: AgentSubCommand,
}

/// Available subcommands for `ffx agent`.
#[derive(ArgsInfo, FromArgs, Clone, PartialEq, Eq, Debug)]
#[argh(subcommand)]
pub enum AgentSubCommand {
    /// Claim exclusive ownership of a target.
    Claim(ClaimCommand),
    /// Release target claim ownership.
    Release(ReleaseCommand),
}

/// Claim exclusive ownership of a Fuchsia target for this agent session.
#[derive(ArgsInfo, FromArgs, Clone, Debug, PartialEq, Eq)]
#[argh(
    subcommand,
    name = "claim",
    description = "Claim exclusive ownership of a Fuchsia target for this agent session."
)]
pub struct ClaimCommand {
    /// target specifier in `id:<serial>` or `serial:<serial>` format
    #[argh(positional)]
    pub target: String,
}

/// Release an agent session claim on a Fuchsia target.
#[derive(ArgsInfo, FromArgs, Clone, Debug, PartialEq, Eq)]
#[argh(
    subcommand,
    name = "release",
    description = "Release an agent session claim on a Fuchsia target."
)]
pub struct ReleaseCommand {
    /// target specifier in `id:<serial>` or `serial:<serial>` format
    #[argh(positional)]
    pub target: String,

    /// force release unconditionally without verifying ownership
    #[argh(switch, long = "unsafe")]
    pub unsafe_release: bool,
}
