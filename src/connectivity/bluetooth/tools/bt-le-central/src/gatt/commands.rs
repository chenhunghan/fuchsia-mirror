// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::gen_commands;

// `Cmd` is the declarative specification of all commands that the
// bt-le-central GATT REPL accepts.
gen_commands! {
    pub enum Cmd {
        List = ("list", [], [], "List discovered services"),
        Find = ("find", [], ["uuid"], "Find services"),
        Connect = ("connect", [], ["uuid", "index"], "Connect to a service"),
        ReadChr = ("read-chr", [], ["id"], "Read a characteristic"),
        ReadLongChr = (
            "read-long-chr",
            [],
            ["id", "offset", "max bytes"],
            "Read a long characteristic.",
        ),
        WriteChr = (
            "write-chr",
            ["-w"],
            ["id", "value"],
            "Write to a characteristic.\n\t\
             Use the -w flag to write without response.",
        ),
        WriteLongChr = (
            "write-long-chr",
            ["-r"],
            ["id", "offset", "value"],
            "Write to a long characteristic.\n\t\
             Use the -r flag for a reliable write.",
        ),
        ReadDesc = (
            "read-desc",
            [],
            ["id"],
            "Read a characteristic descriptor",
        ),
        ReadLongDesc = (
            "read-long-desc",
            [],
            ["id", "offset", "max bytes"],
            "Read a long characteristic descriptor",
        ),
        WriteDesc = (
            "write-desc",
            [],
            ["id", "value"],
            "Write to a characteristic descriptor",
        ),
        WriteLongDesc = (
            "write-long-desc",
            [],
            ["id", "offset", "value"],
            "Write to a long characteristic descriptor",
        ),
        ReadByType = (
            "read-by-type",
            [],
            ["uuid"],
            "Read a characteristic or descriptor by UUID",
        ),
        EnableNotify = (
            "enable-notify",
            [],
            ["id"],
            "Enable characteristic notifications",
        ),
        DisableNotify = (
            "disable-notify",
            [],
            ["id"],
            "Disable characteristic notifications",
        ),
        Pacs = ("pacs", [], [], "PACS subcommands"),
        Vol = ("vol", [], [], "VCS subcommands"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_repl::{CommandSet as _, CommandSource as _, Commands};

    #[fuchsia::test]
    fn test_gen_commands_macro() {
        let source = Commands::<Cmd>::new();
        assert!(source.command_names().contains(&"connect".to_string()));
        assert_eq!(Cmd::WriteChr.arguments(), "<id> <value> ");
        assert_eq!(Cmd::WriteLongDesc.flags(), "");
        assert_eq!(Cmd::WriteDesc.flags(), "");
        assert_eq!(Cmd::WriteLongChr.flags(), "[-r] ");
        assert_eq!(Cmd::WriteChr.flags(), "[-w] ");
        assert!(!source.usages().is_empty());
    }
}
