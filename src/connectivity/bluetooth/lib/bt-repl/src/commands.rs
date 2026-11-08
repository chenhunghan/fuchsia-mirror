// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt;
use std::str::FromStr;

/// Error returned when a string does not name a command in a [`CommandSet`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnknownCommand(pub String);

impl fmt::Display for UnknownCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\" is not a valid command", self.0)
    }
}

impl std::error::Error for UnknownCommand {}

/// A set of commands accepted by a REPL.
///
/// Each command has a name (its [`Display`](fmt::Display) and
/// [`FromStr`] representation), zero or more flags, zero or more positional
/// arguments and a description. Implementations are normally generated with
/// [`gen_commands!`](crate::gen_commands).
pub trait CommandSet: FromStr + fmt::Display + Sized {
    /// Returns every command in this set, in declaration order.
    fn all() -> Vec<Self>;

    /// Returns the positional arguments of this command, each wrapped in `<>`
    /// and followed by a space, e.g. `"<peer-id> <psm> "`.
    fn arguments(&self) -> &'static str;

    /// Returns the flags of this command, each wrapped in `[]` and followed by
    /// a space, e.g. `"[--verbose] "`.
    fn flags(&self) -> &'static str;

    /// Returns a description of what this command does.
    fn description(&self) -> &'static str;

    /// Returns the names of every command in this set, in declaration order.
    fn names() -> Vec<String> {
        Self::all().iter().map(ToString::to_string).collect()
    }

    /// Returns the one-line usage of this command:
    /// `name [flags] <arguments> -- description`.
    fn usage(&self) -> String {
        format!("{self} {}{}-- {}", self.flags(), self.arguments(), self.description())
    }
}

/// Generates an enum of REPL commands that implements [`CommandSet`],
/// [`Display`](std::fmt::Display) and [`FromStr`](std::str::FromStr).
///
/// Each variant is declared as
/// `Variant = ("name", [flags...], [arguments...], "description")`. Flags and
/// arguments must be string literals; the description may be any `&'static str`
/// expression. Attributes placed on individual variants are attached to the
/// enum variant declarations (intended for documentation comments, not
/// conditional compilation attributes such as `#[cfg]`). The built-in `help`,
/// `exit` and `quit` commands are provided by [`run`](crate::run) and do not
/// need to be declared here. The generated enum derives `Clone`, `Copy`,
/// `Debug`, `PartialEq`, `Eq` and `Hash`, so those traits must not be derived
/// again by the caller.
///
/// # Example
///
/// ```ignore
/// bt_repl::gen_commands! {
///     /// Commands accepted by the example tool.
///     pub enum Cmd {
///         Connect = ("connect", [], ["peer-id"], "Connect to a peer"),
///         Scan = ("scan", ["--active"], ["[duration]"], "Scan for peers"),
///     }
/// }
///
/// assert_eq!("connect".parse::<Cmd>(), Ok(Cmd::Connect));
/// assert_eq!(Cmd::Scan.usage(), "scan [--active] <[duration]> -- Scan for peers");
/// ```
#[macro_export]
macro_rules! gen_commands {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident = (
                    $command:literal,
                    [$($flag:literal),* $(,)?],
                    [$($argument:literal),* $(,)?],
                    $description:expr $(,)?
                )
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                $variant
            ),*
        }

        impl $crate::CommandSet for $name {
            fn all() -> ::std::vec::Vec<Self> {
                ::std::vec![$($name::$variant),*]
            }

            fn arguments(&self) -> &'static str {
                match *self {
                    $($name::$variant => ::std::concat!($("<", $argument, "> ",)*)),*
                }
            }

            fn flags(&self) -> &'static str {
                match *self {
                    $($name::$variant => ::std::concat!($("[", $flag, "] ",)*)),*
                }
            }

            fn description(&self) -> &'static str {
                match *self {
                    $($name::$variant => $description),*
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                let command = match *self {
                    $($name::$variant => $command),*
                };
                f.write_str(command)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::UnknownCommand;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                match s {
                    $($command => ::std::result::Result::Ok($name::$variant),)*
                    _ => ::std::result::Result::Err($crate::UnknownCommand(s.to_string())),
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::gen_commands! {
        enum TestCommand {
            Plain = ("plain", [], [], "No flags or arguments"),
            WithArguments = ("with-arguments", [], ["first", "[second]"], "Two arguments"),
            WithFlags = ("with-flags", ["-a", "--bee"], [], "Two flags"),
            WithBoth = ("with-both", ["-v"], ["peer-id"], "A flag and an argument"),
        }
    }

    #[fuchsia::test]
    fn all_and_names_are_in_declaration_order() {
        assert_eq!(
            TestCommand::all(),
            vec![
                TestCommand::Plain,
                TestCommand::WithArguments,
                TestCommand::WithFlags,
                TestCommand::WithBoth
            ]
        );
        assert_eq!(
            TestCommand::names(),
            vec!["plain", "with-arguments", "with-flags", "with-both"]
        );
    }

    #[fuchsia::test]
    fn display_and_from_str_round_trip() {
        for command in TestCommand::all() {
            assert_eq!(command.to_string().parse::<TestCommand>(), Ok(command));
        }
    }

    #[fuchsia::test]
    fn from_str_rejects_unknown_commands() {
        let error = "bogus".parse::<TestCommand>().expect_err("bogus is not a command");
        assert_eq!(error, UnknownCommand("bogus".to_string()));
        assert_eq!(error.to_string(), "\"bogus\" is not a valid command");
    }

    #[fuchsia::test]
    fn arguments_and_flags_are_formatted() {
        assert_eq!(TestCommand::Plain.arguments(), "");
        assert_eq!(TestCommand::Plain.flags(), "");
        assert_eq!(TestCommand::WithArguments.arguments(), "<first> <[second]> ");
        assert_eq!(TestCommand::WithFlags.flags(), "[-a] [--bee] ");
        assert_eq!(TestCommand::WithBoth.flags(), "[-v] ");
        assert_eq!(TestCommand::WithBoth.arguments(), "<peer-id> ");
    }

    #[fuchsia::test]
    fn usage_is_formatted() {
        assert_eq!(TestCommand::Plain.usage(), "plain -- No flags or arguments");
        assert_eq!(
            TestCommand::WithArguments.usage(),
            "with-arguments <first> <[second]> -- Two arguments"
        );
        assert_eq!(TestCommand::WithFlags.usage(), "with-flags [-a] [--bee] -- Two flags");
        assert_eq!(
            TestCommand::WithBoth.usage(),
            "with-both [-v] <peer-id> -- A flag and an argument"
        );
    }
}
