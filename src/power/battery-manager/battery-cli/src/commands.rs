// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::{BStr, BString, ByteSlice};
use line_editor::{Color, Editor, Hint};
use std::fmt;
use std::str::FromStr;

//TODO(https://fxbug.dev/42123687): Make the macro a library
/// Macro to generate a command enum and its impl.
macro_rules! gen_commands {
    ($name:ident {
        $($variant:ident = ($val:expr, [$($arg:expr),*], $help:expr)),*,
    }) => {
        /// Enum of all possible commands
        #[derive(PartialEq)]
        pub enum $name {
            $($variant),*
        }

        impl $name {
            /// Returns a list of the string representations of all variants
            pub fn variants() -> Vec<String> {
                vec![$( $val.to_string() ),*]
            }

            pub fn arguments(&self) -> &'static str {
                match self {
                    $(
                        $name::$variant => concat!($("<", $arg, "> ",)*)
                    ),*
                }
            }

            /// Help string for a given variant. The format is "command <arg>.. -- help message"
            #[allow(unused)]
            pub fn cmd_help(&self) -> &'static str {
                match self {
                    $(
                        $name::$variant => concat!($val, " ", $("<", $arg, "> ",)* "-- ", $help)
                    ),*
                }
            }

            /// Multiline help string for `$name` including usage of all variants.
            pub fn help_msg() -> &'static str {
                concat!("Commands:\n", $(
                    "\t", $val, " ", $("<", $arg, "> ",)* "-- ", $help, "\n"
                ),*)
            }

        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match *self {
                    $($name::$variant => write!(f, $val)),* ,
                }
            }
        }

        impl FromStr for $name {
            type Err = ();

            fn from_str(s: &str) -> Result<$name, ()> {
                match s {
                    $($val => Ok($name::$variant)),* ,
                    _ => Err(()),
                }
            }
        }

    }
}

gen_commands! {
    Command {
        Get = ("get", [],
            "Get all information of the battery state"),
        Set = ("set", ["<attribute1 value1> ... [attributeN valueN]", "--help"],
            "Set state of battery in one line | Print help message"),
        Reconnect = ("reconnect", [],
            "Connect the real battery and disconnect the simulator"),
        Disconnect = ("disconnect", [],
            "Disconnect the real battery and connect to the simulator"),
        Governor = ("governor", [
                "list | control | pause <governor> | resume <governor> | override ... | clear ...",
                "--help"
            ],
            "Inspect and control charge governors and override their votes"),
        Help = ("help", [], "Help message"),
        Exit = ("exit", [], "Exit/Close REPL"),
        Quit = ("quit", [], "Quit/Close REPL"),

    }
}

/// ANSI SGR color code for drawing inline hints in dark gray (bright black).
const HINT_COLOR_CODE: u8 = 90;

/// CmdHelper provides completion and inline hints for battery-simulator.
#[derive(Clone, Copy)]
pub struct CmdHelper;

impl CmdHelper {
    pub fn new() -> CmdHelper {
        CmdHelper
    }

    /// Registers this helper's completion and hint handlers on `editor`.
    pub fn install(self, editor: &mut Editor) {
        editor.set_completion_handler(move |line: &BStr| {
            let line = line.to_str_lossy();
            self.complete(&line).into_iter().map(BString::from).collect()
        });
        editor.set_hint_handler(move |line: &BStr| {
            let line = line.to_str_lossy();
            self.hint(&line).map(|hint| Hint::new(hint).with_color(Color::Custom(HINT_COLOR_CODE)))
        });
    }

    /// Returns completion candidates for `line`.
    pub fn complete(&self, line: &str) -> Vec<String> {
        Command::variants().into_iter().filter(|variant| variant.starts_with(line)).collect()
    }

    /// Returns an inline hint for commands with arguments.
    pub fn hint(&self, line: &str) -> Option<String> {
        let cmd: Command = line.trim().parse().ok()?;
        let args = cmd.arguments();
        if args.trim().is_empty() {
            return None;
        }
        let separator = if line.ends_with(' ') { "" } else { " " };
        Some(format!("{separator}{args}"))
    }
}

/// Represents either continuation or breaking out of a read-evaluate-print loop.
pub enum ReplControl {
    Break,
    Continue,
}

#[cfg(test)]
mod tests {
    use super::*;
    use line_editor::{ColumnWidth, Config, OperatingMode, TerminalMode};

    #[fuchsia::test]
    fn completes_commands_by_prefix() {
        let helper = CmdHelper::new();
        assert_eq!(helper.complete("re"), vec!["reconnect"]);
        assert_eq!(helper.complete("g"), vec!["get", "governor"]);
        assert!(helper.complete("unknown").is_empty());
    }

    #[fuchsia::test]
    fn hints_commands_with_arguments() {
        let helper = CmdHelper::new();
        assert_eq!(
            helper.hint("set"),
            Some(" <<attribute1 value1> ... [attributeN valueN]> <--help> ".to_string())
        );
        assert_eq!(
            helper.hint("set "),
            Some("<<attribute1 value1> ... [attributeN valueN]> <--help> ".to_string())
        );
        assert_eq!(helper.hint("get"), None);
        assert_eq!(helper.hint("exit"), None);
        assert_eq!(helper.hint("unknown"), None);
    }

    #[fuchsia::test]
    fn install_wires_completion_and_hints_into_editor() {
        let config = Config::default()
            .with_terminal_mode(TerminalMode::Tty)
            .with_term_name(Some("xterm-256color"))
            .with_column_width(ColumnWidth::Fixed(80));
        let mut editor = Editor::with_config(config);
        CmdHelper::new().install(&mut editor);

        let input = b"re\t\n";
        let mut output = Vec::new();
        let line = editor
            .readline_from(
                &input[..],
                &mut output,
                OperatingMode::Interactive,
                "\x1b[34mbattman>\x1b[0m ",
            )
            .expect("readline succeeds");
        assert_eq!(line, BString::from("reconnect"));
    }
}
