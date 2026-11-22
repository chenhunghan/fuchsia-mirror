// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::{BStr, BString, ByteSlice};
use line_editor::{Color, Editor, Hint};
use std::sync::Arc;

use crate::repl::{BUILTIN_COMMANDS, EXIT_COMMAND, HELP_COMMAND, QUIT_COMMAND};
use crate::source::CommandSource;

/// ANSI SGR color code for drawing inline hints in dark gray (bright black).
const HINT_COLOR_CODE: u8 = 90;

/// The [`line_editor`] helper shared by every REPL. Provides command and
/// argument completion, and inline argument hints.
pub(crate) struct ReplHelper<S> {
    source: Arc<S>,
}

impl<S> Clone for ReplHelper<S> {
    fn clone(&self) -> Self {
        Self { source: Arc::clone(&self.source) }
    }
}

impl<S: CommandSource> ReplHelper<S> {
    pub(crate) fn new(source: Arc<S>) -> Self {
        Self { source }
    }

    /// Registers this helper's completion and hint handlers on `editor`.
    pub(crate) fn install(self, editor: &mut Editor) {
        let completer = self.clone();
        editor.set_completion_handler(move |line: &BStr| {
            let line = line.to_str_lossy();
            completer.complete(&line).into_iter().map(BString::from).collect()
        });
        editor.set_hint_handler(move |line: &BStr| {
            let line = line.to_str_lossy();
            self.hint(&line).map(|hint| Hint::new(hint).with_color(Color::Custom(HINT_COLOR_CODE)))
        });
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

    /// Returns completion candidates for `line`.
    pub(crate) fn complete(&self, line: &str) -> Vec<String> {
        let components: Vec<&str> = line.split_whitespace().collect();
        let ends_with_space = line.ends_with(char::is_whitespace);

        // Complete the first argument once a command followed by whitespace, or a command and a
        // partial argument, has been entered. Otherwise complete the command name.
        match components.as_slice() {
            [] => self.command_names(),
            [typed] if !ends_with_space => {
                self.command_names().into_iter().filter(|name| name.starts_with(typed)).collect()
            }
            [command] => self.argument_candidates(command, ""),
            [command, partial] if !ends_with_space => self.argument_candidates(command, partial),
            _ => Vec::new(),
        }
    }

    /// Hints the flags and arguments of a fully typed command.
    pub(crate) fn hint(&self, line: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use line_editor::{ColumnWidth, Config, OperatingMode, TerminalMode};

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
        helper.complete(line)
    }

    fn hint<S: CommandSource>(helper: &ReplHelper<S>, line: &str) -> Option<String> {
        helper.hint(line)
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
    fn install_wires_completion_and_hints_into_editor() {
        let config = Config::default()
            .with_terminal_mode(TerminalMode::Tty)
            .with_term_name(Some("xterm-256color"))
            .with_column_width(ColumnWidth::Fixed(80));
        let mut editor = Editor::with_config(config);
        ReplHelper::new(Arc::new(peer_source())).install(&mut editor);

        // Type "con", Tab (completes to "connect"), space, "04",
        // Tab (completes to "connect 0456"), Enter.
        let input = b"con\t 04\t\n";
        let mut output = Vec::new();
        let line = editor
            .readline_from(
                &input[..],
                &mut output,
                OperatingMode::Interactive,
                "\x1b[34mbt>\x1b[0m ",
            )
            .expect("readline succeeds");
        assert_eq!(line, BString::from("connect 0456"));
        let rendered = String::from_utf8_lossy(&output);
        // Verify that the dark-gray hint for `connect` (`<peer-id> `) was
        // rendered to output, and that cursor positioning accounts for the 4
        // visible columns of "\x1b[34mbt>\x1b[0m ".
        assert!(rendered.contains("\x1b[0;90;49m <peer-id> \x1b[0m"));
        assert!(rendered.contains("\r\x1b[11C"));
    }
}
