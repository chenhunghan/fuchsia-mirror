// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::gen_commands;

// `Cmd` is the declarative specification of all commands that
// bt-map-mce-tool accepts.
gen_commands! {
    pub enum Cmd {
        ListAllMasInstances = (
            "list",
            [],
            [],
            "lists all the message access service instances available at \
             this remote MSE peer",
        ),
        TurnOnNotifications = (
            "turn-on-notifications",
            [],
            ["[repo_uid]"],
            "registers to receive notifications from the specified repo. \
             If `repo_uid` isn't specified, will receive notifications \
             from all known MAS instances",
        ),
        TurnOffNotifications = (
            "turn-off-notifications",
            [],
            [],
            "unregister for notifications from the connected remote MSE peer",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_repl::{CommandSet as _, CommandSource as _, Commands};

    #[fuchsia::test]
    fn test_gen_commands_macro() {
        let source = Commands::<Cmd>::new();
        assert!(source.command_names().contains(&"turn-on-notifications".to_string()));
        assert_eq!(Cmd::ListAllMasInstances.arguments(), "");
        assert_eq!(Cmd::TurnOnNotifications.arguments(), "<[repo_uid]> ");
        assert_eq!(Cmd::TurnOffNotifications.arguments(), "");
        assert!(!source.usages().is_empty());
    }

    #[fuchsia::test]
    fn test_completer() {
        let source = Commands::<Cmd>::new();
        let names = source.command_names();
        assert!(names.iter().any(|c| c.starts_with("li") && c == "list"));
        assert!(names.iter().any(|c| c.starts_with("turn-on") && c == "turn-on-notifications"));
    }
}
