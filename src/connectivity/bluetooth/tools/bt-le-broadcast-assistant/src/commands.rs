// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_broadcast_assistant::debug::AssistantCmd;
use bt_common::debug_command::CommandSet as UpstreamCommandSet;
use bt_repl::{CommandSource, Commands, gen_commands};
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

// `Cmd` is the declarative specification of all local commands that bt-le-broadcast-assistant
// accepts in addition to `AssistantCmd`.
gen_commands! {
    pub enum Cmd {
        Scan = (
            "scan",
            [],
            ["[duration]"],
            "Scan for scan delegators for a specified duration in seconds (default is 10)",
        ),
        SetPeerAddr = (
            "set-peer-addr",
            [],
            ["peer_id", "address", "Public|Random"],
            "Set the address for a given peer ID (only for --use-static-address mode).",
        ),
    }
}

/// Combined command enum dispatching either a local [`Cmd`] or an upstream [`AssistantCmd`].
#[derive(Debug, PartialEq)]
pub enum ReplCmd {
    Local(Cmd),
    Assistant(AssistantCmd),
}

impl FromStr for ReplCmd {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(cmd) = Cmd::from_str(s) {
            return Ok(Self::Local(cmd));
        }
        if let Ok(cmd) = AssistantCmd::from_str(s) {
            return Ok(Self::Assistant(cmd));
        }
        Err(())
    }
}

/// Adapts a [`bt_common::debug_command::CommandSet`] into a [`bt_repl::CommandSource`].
pub struct UpstreamCommands<C>(PhantomData<fn() -> C>);

impl<C> UpstreamCommands<C> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<C> Default for UpstreamCommands<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Clone for UpstreamCommands<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for UpstreamCommands<C> {}

impl<C> fmt::Debug for UpstreamCommands<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamCommands").finish()
    }
}

impl<C: UpstreamCommandSet + 'static> CommandSource for UpstreamCommands<C> {
    fn command_names(&self) -> Vec<String> {
        C::variants()
    }

    fn usage(&self, cmd: &str) -> Option<String> {
        let c = C::from_str(cmd).ok()?;
        Some(format!("{c} {}{}-- {}", c.flags(), c.arguments(), c.desc()))
    }

    fn hint(&self, cmd: &str) -> Option<String> {
        let c = C::from_str(cmd).ok()?;
        let hint = format!("{}{}", c.flags(), c.arguments());
        (!hint.is_empty()).then_some(hint)
    }
}

/// Creates the combined [`CommandSource`] for `bt-le-broadcast-assistant`.
pub fn command_source() -> impl CommandSource {
    (Commands::<Cmd>::new(), UpstreamCommands::<AssistantCmd>::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_repl::CommandSet as _;

    #[fuchsia::test]
    fn test_cmd_variants() {
        let variants = Cmd::all();
        assert_eq!(variants, vec![Cmd::Scan, Cmd::SetPeerAddr]);
    }

    #[fuchsia::test]
    fn test_cmd_from_str() {
        assert_eq!(Cmd::from_str("scan"), Ok(Cmd::Scan));
        assert_eq!(Cmd::from_str("set-peer-addr"), Ok(Cmd::SetPeerAddr));
        assert!(Cmd::from_str("invalid").is_err());

        assert_eq!(ReplCmd::from_str("scan"), Ok(ReplCmd::Local(Cmd::Scan)));
        assert_eq!(ReplCmd::from_str("connect"), Ok(ReplCmd::Assistant(AssistantCmd::Connect)));
        assert_eq!(ReplCmd::from_str("invalid"), Err(()));
    }

    #[fuchsia::test]
    fn test_command_source() {
        let upstream = UpstreamCommands::<AssistantCmd>::new().clone();
        assert_eq!(format!("{upstream:?}"), "UpstreamCommands");

        let source = command_source();
        let names = source.command_names();
        assert!(names.contains(&"scan".to_string()));
        assert!(names.contains(&"set-peer-addr".to_string()));
        assert!(names.contains(&"connect".to_string()));
        assert!(names.contains(&"set-broadcast-code".to_string()));

        assert!(!source.usages().is_empty());
        assert!(source.usage("scan").is_some());
        assert!(source.usage("connect").unwrap().starts_with("connect <peer_id> -- "));
        assert!(source.usage("unknown").is_none());

        assert_eq!(source.hint("scan"), Some("<[duration]> ".to_string()));
        assert_eq!(source.hint("connect"), Some("<peer_id> ".to_string()));
        assert_eq!(source.hint("info"), None);
    }
}
