// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow::{self, Borrowed, Owned};
use std::sync::Arc;

use crate::repl::{BUILTIN_COMMANDS, EXIT_COMMAND, HELP_COMMAND, QUIT_COMMAND};
use crate::source::CommandSource;

/// Escape sequence that starts drawing hints in dark gray.
const HINT_COLOR: &str = "\x1b[90m";
/// Escape sequence that resets the text color.
const RESET_COLOR: &str = "\x1b[0m";

/// The rustyline [`Helper`] shared by every REPL. Provides command and
/// argument completion, argument hints, and hint highlighting.
pub(crate) struct ReplHelper<S> {
    source: Arc<S>,
}

impl<S: CommandSource> ReplHelper<S> {
    pub(crate) fn new(source: Arc<S>) -> Self {
        Self { source }
    }

    /// Returns the names of all commands, including the built-in commands.
    fn command_names(&self) -> Vec<String> {
        let mut names = self.source.command_names();
        for builtin in BUILTIN_COMMANDS {
            if !names.iter().any(|name| name == builtin) {
                names.push(builtin.to_string());
            }
        }
        names
    }

    /// Returns completion candidates for the argument of `command`.
    fn argument_candidates(&self, command: &str, partial: &str) -> Vec<String> {
        let candidates = match command {
            HELP_COMMAND => self.command_names(),
            EXIT_COMMAND | QUIT_COMMAND => Vec::new(),
            _ => self.source.complete_argument(command, partial),
        };
        candidates
            .into_iter()
            .filter(|candidate| candidate.starts_with(partial))
            .map(|candidate| format!("{command} {candidate}"))
            .collect()
    }
}

impl<S: CommandSource> Completer for ReplHelper<S> {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        _pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<String>), ReadlineError> {
        let components: Vec<&str> = line.split_whitespace().collect();
        let ends_with_space = line.ends_with(char::is_whitespace);

        // Complete the first argument once a command followed by whitespace, or a command and a
        // partial argument, has been entered. Otherwise complete the command name.
        let candidates = match components.as_slice() {
            [] => self.command_names(),
            [typed] if !ends_with_space => {
                self.command_names().into_iter().filter(|name| name.starts_with(typed)).collect()
            }
            [command] => self.argument_candidates(command, ""),
            [command, partial] if !ends_with_space => self.argument_candidates(command, partial),
            _ => Vec::new(),
        };
        Ok((0, candidates))
    }
}

impl<S: CommandSource> Hinter for ReplHelper<S> {
    type Hint = String;

    /// Hints the flags and arguments of a fully typed command.
    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        let trimmed = line.trim();
        let hint = match trimmed {
            HELP_COMMAND => "<[command]> ".to_string(),
            EXIT_COMMAND | QUIT_COMMAND => return None,
            _ => self.source.hint(trimmed)?,
        };
        if hint.trim().is_empty() {
            return None;
        }
        let separator = if line.ends_with(' ') { "" } else { " " };
        Some(format!("{separator}{hint}"))
    }
}

impl<S: CommandSource> Highlighter for ReplHelper<S> {
    /// Draws hints in a different color to distinguish them from user input.
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if hint.trim().is_empty() {
            Borrowed(hint)
        } else {
            Owned(format!("{HINT_COLOR}{hint}{RESET_COLOR}"))
        }
    }
}

impl<S: CommandSource> Validator for ReplHelper<S> {}

impl<S: CommandSource> Helper for ReplHelper<S> {}

#[cfg(test)]
mod tests {
    use rustyline::history::DefaultHistory;

    use super::*;
    use crate::commands::CommandSet;
    use crate::source::Commands;

    crate::gen_commands! {
        enum TestCommand {
            Connect = ("connect", [], ["peer-id"], "Connect to a peer"),
            Disconnect = ("disconnect", [], ["peer-id"], "Disconnect from a peer"),
            Scan = ("scan", ["--active"], ["[duration]"], "Scan for peers"),
            List = ("list", [], [], "List peers"),
        }
    }

    fn peer_source() -> impl CommandSource {
        Commands::<TestCommand>::new().with_arg_completer(|command, _partial| match command {
            TestCommand::Connect | TestCommand::Disconnect => {
                vec!["0123".to_string(), "0456".to_string()]
            }
            TestCommand::Scan | TestCommand::List => Vec::new(),
        })
    }

    fn complete<S: CommandSource>(helper: &ReplHelper<S>, line: &str) -> Vec<String> {
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, candidates) =
            Completer::complete(helper, line, line.len(), &context).expect("completion succeeds");
        assert_eq!(start, 0);
        candidates
    }

    fn hint<S: CommandSource>(helper: &ReplHelper<S>, line: &str) -> Option<String> {
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        Hinter::hint(helper, line, line.len(), &context)
    }

    #[fuchsia::test]
    fn completes_command_names_by_prefix() {
        let helper = ReplHelper::new(Arc::new(Commands::<TestCommand>::new()));
        assert_eq!(complete(&helper, "con"), vec!["connect"]);
        assert_eq!(complete(&helper, "  con"), vec!["connect"]);
        assert_eq!(complete(&helper, "s"), vec!["scan"]);
        assert_eq!(complete(&helper, "bogus"), Vec::<String>::new());
    }

    #[fuchsia::test]
    fn completes_builtin_commands_without_duplicates() {
        crate::gen_commands! {
            enum OverlappingCommand {
                Help = ("help", [], [], "Custom help"),
                Ping = ("ping", [], [], "Ping"),
            }
        }
        let helper = ReplHelper::new(Arc::new(Commands::<OverlappingCommand>::new()));
        assert_eq!(complete(&helper, "he"), vec!["help"]);
        assert_eq!(complete(&helper, "ex"), vec!["exit"]);
        assert_eq!(complete(&helper, "q"), vec!["quit"]);
        assert_eq!(complete(&helper, ""), vec!["help", "ping", "exit", "quit"]);
    }

    #[fuchsia::test]
    fn completes_command_names_after_help() {
        let helper = ReplHelper::new(Arc::new(Commands::<TestCommand>::new()));
        assert_eq!(complete(&helper, "help dis"), vec!["help disconnect"]);
        assert_eq!(complete(&helper, "help q"), vec!["help quit"]);
        assert_eq!(
            complete(&helper, "help ").len(),
            TestCommand::all().len() + BUILTIN_COMMANDS.len()
        );
    }

    #[fuchsia::test]
    fn completes_arguments_from_source() {
        let helper = ReplHelper::new(Arc::new(peer_source()));
        assert_eq!(complete(&helper, "connect "), vec!["connect 0123", "connect 0456"]);
        assert_eq!(complete(&helper, "connect 04"), vec!["connect 0456"]);
        assert_eq!(complete(&helper, "connect 9"), Vec::<String>::new());
        assert_eq!(complete(&helper, "list "), Vec::<String>::new());
    }

    #[fuchsia::test]
    fn does_not_complete_past_the_first_argument() {
        let helper = ReplHelper::new(Arc::new(peer_source()));
        assert_eq!(complete(&helper, "connect 0123 "), Vec::<String>::new());
    }

    #[fuchsia::test]
    fn hints_flags_and_arguments() {
        let helper = ReplHelper::new(Arc::new(Commands::<TestCommand>::new()));
        assert_eq!(hint(&helper, "connect"), Some(" <peer-id> ".to_string()));
        assert_eq!(hint(&helper, "  connect"), Some(" <peer-id> ".to_string()));
        assert_eq!(hint(&helper, "connect "), Some("<peer-id> ".to_string()));
        assert_eq!(hint(&helper, "scan"), Some(" [--active] <[duration]> ".to_string()));
        assert_eq!(hint(&helper, "help"), Some(" <[command]> ".to_string()));
        assert_eq!(hint(&helper, "help "), Some("<[command]> ".to_string()));
    }

    #[fuchsia::test]
    fn does_not_hint_unknown_or_partial_commands() {
        let helper = ReplHelper::new(Arc::new(Commands::<TestCommand>::new()));
        assert_eq!(hint(&helper, "list"), None);
        assert_eq!(hint(&helper, "exit"), None);
        assert_eq!(hint(&helper, "quit"), None);
        assert_eq!(hint(&helper, "conn"), None);
        assert_eq!(hint(&helper, "connect 0123"), None);
        assert_eq!(hint(&helper, ""), None);
    }

    #[fuchsia::test]
    fn builtins_shadow_custom_hints_and_argument_completions() {
        crate::gen_commands! {
            enum ShadowedBuiltinCommand {
                Help = ("help", ["--custom"], ["topic"], "Shadowed help"),
                Exit = ("exit", ["--force"], ["code"], "Shadowed exit"),
                Quit = ("quit", ["--force"], ["code"], "Shadowed quit"),
            }
        }
        let source = Commands::<ShadowedBuiltinCommand>::new()
            .with_arg_completer(|_, _| vec!["shadowed-arg".to_string()]);
        let helper = ReplHelper::new(Arc::new(source));
        assert_eq!(hint(&helper, "help"), Some(" <[command]> ".to_string()));
        assert_eq!(hint(&helper, "exit"), None);
        assert_eq!(hint(&helper, "quit"), None);
        assert!(complete(&helper, "exit ").is_empty());
        assert!(complete(&helper, "quit ").is_empty());
    }

    #[fuchsia::test]
    fn highlights_non_empty_hints() {
        let helper = ReplHelper::new(Arc::new(Commands::<TestCommand>::new()));
        assert_eq!(helper.highlight_hint(" <peer-id> "), "\x1b[90m <peer-id> \x1b[0m");
        assert_eq!(helper.highlight_hint("  "), "  ");
    }
}
