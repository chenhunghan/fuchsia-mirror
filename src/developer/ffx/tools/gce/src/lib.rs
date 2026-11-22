// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fho::subtool_suite::{FfxSubtoolSuite, Subtool, SubtoolBox, SubtoolSuite};

mod args;
mod subtools;

pub use args::{
    GceCommand, GceSubCommand, ListCommand, SerialCommand, ShowCommand, StartCommand, StopCommand,
};
pub use subtools::{ListTool, SerialTool, ShowTool, StartTool, StopTool};

pub struct GceSuite;

impl SubtoolSuite for GceSuite {
    type Command = GceCommand;

    fn new_subtool(cmd: GceCommand) -> Box<dyn SubtoolBox> {
        match cmd.subcommand {
            GceSubCommand::Start(cmd) => Subtool::<StartTool>::new(cmd),
            GceSubCommand::List(cmd) => Subtool::<ListTool>::new(cmd),
            GceSubCommand::Show(cmd) => Subtool::<ShowTool>::new(cmd),
            GceSubCommand::Serial(cmd) => Subtool::<SerialTool>::new(cmd),
            GceSubCommand::Stop(cmd) => Subtool::<StopTool>::new(cmd),
        }
    }
}

pub type GceSuiteTool = FfxSubtoolSuite<GceSuite>;

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;

    #[fuchsia::test]
    fn test_parse_start_command() {
        let cmd = GceCommand::from_args(
            &["gce"],
            &["start", "--name", "my-vm", "--machine-type", "e2-standard-8", "--serial"],
        )
        .expect("parsed start");
        match cmd.subcommand {
            GceSubCommand::Start(s) => {
                assert_eq!(s.name.as_deref(), Some("my-vm"));
                assert_eq!(s.machine_type.as_deref(), Some("e2-standard-8"));
                assert!(s.serial);
            }
            _ => panic!("expected Start subcommand"),
        }
    }

    #[fuchsia::test]
    fn test_parse_list_command() {
        let cmd = GceCommand::from_args(&["gce"], &["list", "--zone", "us-east1-c"])
            .expect("parsed list");
        match cmd.subcommand {
            GceSubCommand::List(l) => {
                assert_eq!(l.zone.as_deref(), Some("us-east1-c"));
            }
            _ => panic!("expected List subcommand"),
        }
    }

    #[fuchsia::test]
    fn test_parse_show_command() {
        let cmd = GceCommand::from_args(&["gce"], &["show", "test-vm"]).expect("parsed show");
        match cmd.subcommand {
            GceSubCommand::Show(s) => {
                assert_eq!(s.name, "test-vm");
            }
            _ => panic!("expected Show subcommand"),
        }
    }

    #[fuchsia::test]
    fn test_parse_serial_command() {
        let cmd =
            GceCommand::from_args(&["gce"], &["serial", "test-vm", "--follow", "--port", "1"])
                .expect("parsed serial");
        match cmd.subcommand {
            GceSubCommand::Serial(s) => {
                assert_eq!(s.name, "test-vm");
                assert!(s.follow);
                assert_eq!(s.port, 1);
            }
            _ => panic!("expected Serial subcommand"),
        }
    }

    #[fuchsia::test]
    fn test_parse_stop_command() {
        let cmd =
            GceCommand::from_args(&["gce"], &["stop", "my-vm", "--keep"]).expect("parsed stop");
        match cmd.subcommand {
            GceSubCommand::Stop(s) => {
                assert_eq!(s.name, "my-vm");
                assert!(s.keep);
            }
            _ => panic!("expected Stop subcommand"),
        }
    }

    #[fuchsia::test]
    fn test_parse_stop_command_requires_name() {
        assert!(GceCommand::from_args(&["gce"], &["stop"]).is_err());
    }
}
