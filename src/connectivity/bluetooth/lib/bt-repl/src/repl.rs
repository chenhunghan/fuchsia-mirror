// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use futures::channel::mpsc;
use futures::executor::block_on;
use futures::{Sink, SinkExt, Stream, StreamExt};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config, EditMode, Editor};
use std::future::Future;
use std::io::{self, Write};
use std::pin::pin;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;

use crate::helper::ReplHelper;
use crate::source::CommandSource;

/// Built-in command that prints help for all commands or for one command.
pub(crate) const HELP_COMMAND: &str = "help";
/// Built-in command that ends the REPL.
pub(crate) const EXIT_COMMAND: &str = "exit";
/// Built-in alias for [`EXIT_COMMAND`].
pub(crate) const QUIT_COMMAND: &str = "quit";
/// All commands handled by the REPL itself rather than by the [`ReplHandler`].
pub(crate) const BUILTIN_COMMANDS: [&str; 3] = [HELP_COMMAND, EXIT_COMMAND, QUIT_COMMAND];

/// Usage lines of the built-in commands, appended to the output of `help`.
const BUILTIN_USAGES: [&str; 3] = [
    "help <[command]> -- Print help for all commands, or for a single command",
    "exit -- Exit the REPL",
    "quit -- Exit the REPL",
];

/// Whether the REPL should keep reading commands after a command is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplControl {
    /// Read the next command.
    Continue,
    /// End the REPL, as if the user had entered `exit`.
    Exit,
}

/// Handles the commands entered into a REPL started with [`run`].
///
/// The built-in `help`, `exit` and `quit` commands, empty lines and
/// unrecognized commands are handled by the REPL and never reach the handler.
pub trait ReplHandler {
    /// The commands this handler accepts. Lines whose first word does not
    /// parse as a `Command` are reported to the user as invalid commands.
    type Command: FromStr;

    /// Handles `command`, entered with the whitespace-separated `args`.
    ///
    /// If this returns `Err`, the error is printed and the REPL reads the next
    /// command.
    fn handle(
        &mut self,
        command: Self::Command,
        args: &[&str],
    ) -> impl Future<Output = Result<ReplControl, Error>>;

    /// Called once when the REPL loop ends (when the user enters `exit` or
    /// `quit`, presses Ctrl-C or Ctrl-D, [`handle`](ReplHandler::handle)
    /// returns [`ReplControl::Exit`], or reading input fails). Not called if
    /// the terminal editor cannot be initialized or if the future returned by
    /// [`run`] is dropped before it completes.
    fn on_exit(&mut self) -> impl Future<Output = ()> {
        async {}
    }

    /// Text printed after the list of commands when the user enters `help`.
    fn help_footer(&self) -> Option<String> {
        None
    }
}

/// Runs a REPL that reads commands with `prompt` and passes them to `handler`.
///
/// `source` provides completion, hints and the help text for the commands
/// accepted by `handler`. The REPL behaves the same way for every tool:
///
///   * Empty lines are ignored.
///   * `help` prints the usage of every command, followed by
///     [`ReplHandler::help_footer`]. `help <command>` prints the usage of one
///     command.
///   * Lines whose first word is not a command print an error and the REPL
///     continues.
///   * Errors returned by [`ReplHandler::handle`] are printed and the REPL
///     continues.
///   * `exit`, `quit`, Ctrl-C and Ctrl-D call [`ReplHandler::on_exit`] and
///     end the REPL.
///
/// Input is read by a dedicated thread, because reading a line blocks. The
/// next line is read only after the previous command has been handled, so
/// output from a command is printed before the next prompt. The thread is
/// joined before this function returns. If the returned future is dropped
/// before it completes, the thread remains blocked on `stdin` until the next
/// newline or process exit, so `run` should not be called again after being
/// cancelled.
///
/// # Errors
///
/// Returns an error if the terminal cannot be configured or read from, or if
/// the input thread panics.
pub async fn run<S, H>(prompt: &'static str, source: S, mut handler: H) -> Result<(), Error>
where
    S: CommandSource,
    H: ReplHandler,
{
    let source = Arc::new(source);
    let config = Config::builder()
        .auto_add_history(true)
        .history_ignore_space(true)
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .build();
    let mut editor: Editor<ReplHelper<S>, DefaultHistory> = Editor::with_config(config)?;
    editor.set_helper(Some(ReplHelper::new(source.clone())));

    let (input_thread, lines, acks) = spawn_input_thread(prompt, editor);
    dispatch_loop(&*source, &mut handler, lines, acks, &mut io::stdout()).await;
    input_thread.join().map_err(|_| format_err!("REPL input thread panicked"))?
}

/// Spawns a thread that reads lines with `editor` and sends them to the
/// returned `Receiver`. After sending a line, the thread waits for an
/// acknowledgement on the returned `Sender` before reading the next one.
///
/// The thread exits when the user presses Ctrl-C or Ctrl-D, when reading
/// fails, or when either channel is closed.
fn spawn_input_thread<S: CommandSource>(
    prompt: &'static str,
    mut editor: Editor<ReplHelper<S>, DefaultHistory>,
) -> (thread::JoinHandle<Result<(), Error>>, mpsc::Receiver<String>, mpsc::Sender<()>) {
    let (mut line_sender, line_receiver) = mpsc::channel(0);
    let (ack_sender, mut ack_receiver) = mpsc::channel(0);

    let input_thread = thread::spawn(move || -> Result<(), Error> {
        block_on(async move {
            loop {
                match editor.readline(prompt) {
                    Ok(line) => {
                        if line_sender.send(line).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(ReadlineError::Eof | ReadlineError::Interrupted) => return Ok(()),
                    Err(error) => return Err(Error::from(error).context("failed to read input")),
                }
                // Wait until the command has been handled so that its output is printed before
                // the next prompt.
                if ack_receiver.next().await.is_none() {
                    return Ok(());
                }
            }
        })
    });
    (input_thread, line_receiver, ack_sender)
}

/// Handles each line from `lines` and sends an acknowledgement to `acks` once
/// the line has been handled. Returns when the REPL ends.
async fn dispatch_loop<S, H, W>(
    source: &S,
    handler: &mut H,
    lines: impl Stream<Item = String>,
    acks: impl Sink<()>,
    out: &mut W,
) where
    S: CommandSource,
    H: ReplHandler,
    W: Write,
{
    let mut lines = pin!(lines);
    let mut acks = pin!(acks);
    while let Some(line) = lines.next().await {
        if dispatch_line(source, handler, &line, out).await == ReplControl::Exit {
            break;
        }
        let _ = out.flush();
        if acks.send(()).await.is_err() {
            break;
        }
    }
    handler.on_exit().await;
}

/// Handles a single line of input.
async fn dispatch_line<S, H, W>(source: &S, handler: &mut H, line: &str, out: &mut W) -> ReplControl
where
    S: CommandSource,
    H: ReplHandler,
    W: Write,
{
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else {
        return ReplControl::Continue;
    };
    let args: Vec<&str> = words.collect();

    match command {
        HELP_COMMAND => {
            let help = match args.as_slice() {
                [] => help_all(source, handler.help_footer()),
                [command] => usage(source, command),
                [_, _, ..] => usage(source, HELP_COMMAND),
            };
            write_line(out, &help);
            ReplControl::Continue
        }
        EXIT_COMMAND | QUIT_COMMAND => ReplControl::Exit,
        _ => match command.parse::<H::Command>() {
            Ok(command) => match handler.handle(command, &args).await {
                Ok(control) => control,
                Err(error) => {
                    write_line(out, &format!("Error: {error:#}"));
                    ReplControl::Continue
                }
            },
            Err(_) => {
                write_line(out, &invalid_command(command));
                ReplControl::Continue
            }
        },
    }
}

/// Returns the usage of every command, followed by `footer`.
fn help_all<S: CommandSource>(source: &S, footer: Option<String>) -> String {
    let mut help = String::from("Commands:");
    for name in source.command_names() {
        if !BUILTIN_COMMANDS.contains(&name.as_str())
            && let Some(usage) = source.usage(&name)
        {
            help.push_str("\n\t");
            help.push_str(&usage);
        }
    }
    for usage in BUILTIN_USAGES {
        help.push_str("\n\t");
        help.push_str(usage);
    }
    if let Some(footer) = footer {
        help.push_str("\n\n");
        help.push_str(&footer);
    }
    help
}

/// Returns the usage of `command`.
fn usage<S: CommandSource>(source: &S, command: &str) -> String {
    let builtin_usage = BUILTIN_COMMANDS
        .iter()
        .zip(BUILTIN_USAGES.iter())
        .find(|(builtin, _)| **builtin == command)
        .map(|(_, usage)| usage.to_string());
    builtin_usage.or_else(|| source.usage(command)).unwrap_or_else(|| invalid_command(command))
}

/// Returns the message printed when `command` is not a valid command.
fn invalid_command(command: &str) -> String {
    format!("\"{command}\" is not a valid command. Type '{HELP_COMMAND}' for a list of commands.")
}

/// Writes `text` and a newline to `out`.
fn write_line<W: Write>(out: &mut W, text: &str) {
    // Nothing useful can be done if writing REPL output fails.
    let _ = writeln!(out, "{text}");
}

#[cfg(test)]
mod tests {
    use futures::stream;

    use super::*;
    use crate::source::Commands;

    crate::gen_commands! {
        enum TestCommand {
            Ping = ("ping", [], [], "Reply with pong"),
            Echo = ("echo", ["-n"], ["text"], "Print text"),
            Fail = ("fail", [], [], "Always fails"),
            Stop = ("stop", [], [], "End the REPL from the handler"),
        }
    }

    #[derive(Default)]
    struct TestHandler {
        handled: Vec<(TestCommand, Vec<String>)>,
        exit_count: usize,
        footer: Option<String>,
    }

    impl ReplHandler for TestHandler {
        type Command = TestCommand;

        async fn handle(
            &mut self,
            command: TestCommand,
            args: &[&str],
        ) -> Result<ReplControl, Error> {
            self.handled.push((command, args.iter().map(ToString::to_string).collect()));
            match command {
                TestCommand::Fail => Err(format_err!("root cause").context("failed on purpose")),
                TestCommand::Stop => Ok(ReplControl::Exit),
                TestCommand::Ping | TestCommand::Echo => Ok(ReplControl::Continue),
            }
        }

        async fn on_exit(&mut self) {
            self.exit_count += 1;
        }

        fn help_footer(&self) -> Option<String> {
            self.footer.clone()
        }
    }

    /// The result of feeding lines to `dispatch_loop`.
    struct Outcome {
        output: String,
        ack_count: usize,
    }

    async fn dispatch(handler: &mut TestHandler, lines: &[&str]) -> Outcome {
        let lines = stream::iter(lines.iter().map(ToString::to_string).collect::<Vec<_>>());
        let (ack_sender, ack_receiver) = mpsc::unbounded();
        let mut output = Vec::new();
        dispatch_loop(&Commands::<TestCommand>::new(), handler, lines, ack_sender, &mut output)
            .await;
        let ack_count = ack_receiver.count().await;
        Outcome { output: String::from_utf8(output).expect("output is UTF-8"), ack_count }
    }

    #[fuchsia::test]
    async fn passes_commands_and_arguments_to_handler() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["ping", "  echo   -n hello  "]).await;
        assert_eq!(
            handler.handled,
            vec![
                (TestCommand::Ping, vec![]),
                (TestCommand::Echo, vec!["-n".to_string(), "hello".to_string()]),
            ]
        );
        assert_eq!(outcome.output, "");
        assert_eq!(outcome.ack_count, 2);
    }

    #[fuchsia::test]
    async fn ignores_empty_lines() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["", "   ", "ping"]).await;
        assert_eq!(handler.handled, vec![(TestCommand::Ping, vec![])]);
        assert_eq!(outcome.output, "");
        assert_eq!(outcome.ack_count, 3);
    }

    #[fuchsia::test]
    async fn reports_invalid_commands_and_continues() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["bogus arg", "ping"]).await;
        assert_eq!(handler.handled, vec![(TestCommand::Ping, vec![])]);
        assert_eq!(
            outcome.output,
            "\"bogus\" is not a valid command. Type 'help' for a list of commands.\n"
        );
        assert_eq!(outcome.ack_count, 2);
    }

    #[fuchsia::test]
    async fn reports_handler_errors_and_continues() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["fail", "ping"]).await;
        assert_eq!(handler.handled, vec![(TestCommand::Fail, vec![]), (TestCommand::Ping, vec![])]);
        assert_eq!(outcome.output, "Error: failed on purpose: root cause\n");
        assert_eq!(outcome.ack_count, 2);
        // `on_exit` runs once, when the input ends, rather than when the handler fails.
        assert_eq!(handler.exit_count, 1);
    }

    #[fuchsia::test]
    async fn exit_and_quit_end_the_repl() {
        for exit_command in [EXIT_COMMAND, QUIT_COMMAND] {
            let mut handler = TestHandler::default();
            let outcome = dispatch(&mut handler, &["ping", exit_command, "ping"]).await;
            assert_eq!(handler.handled, vec![(TestCommand::Ping, vec![])]);
            assert_eq!(handler.exit_count, 1);
            assert_eq!(outcome.ack_count, 1);
        }
    }

    #[fuchsia::test]
    async fn handler_can_end_the_repl() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["stop", "ping"]).await;
        assert_eq!(handler.handled, vec![(TestCommand::Stop, vec![])]);
        assert_eq!(handler.exit_count, 1);
        assert_eq!(outcome.ack_count, 0);
    }

    #[fuchsia::test]
    async fn closed_ack_channel_ends_the_repl() {
        let mut handler = TestHandler::default();
        let lines = stream::iter(vec!["ping".to_string(), "ping".to_string()]);
        let (ack_sender, ack_receiver) = mpsc::unbounded();
        drop(ack_receiver);
        let mut output = Vec::new();
        dispatch_loop(
            &Commands::<TestCommand>::new(),
            &mut handler,
            lines,
            ack_sender,
            &mut output,
        )
        .await;
        assert_eq!(handler.handled, vec![(TestCommand::Ping, vec![])]);
        assert_eq!(handler.exit_count, 1);
    }

    #[fuchsia::test]
    async fn end_of_input_ends_the_repl() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(&mut handler, &["ping"]).await;
        assert_eq!(handler.exit_count, 1);
        assert_eq!(outcome.ack_count, 1);
    }

    #[fuchsia::test]
    async fn help_lists_all_commands_without_duplicating_builtins() {
        crate::gen_commands! {
            enum OverlappingCommand {
                Ping = ("ping", [], [], "Reply with pong"),
                Help = ("help", [], [], "Custom help that is shadowed"),
            }
        }
        let mut handler = TestHandler::default();
        let lines = stream::iter(vec!["help".to_string()]);
        let (ack_sender, _ack_receiver) = mpsc::unbounded();
        let mut output = Vec::new();
        dispatch_loop(
            &Commands::<OverlappingCommand>::new(),
            &mut handler,
            lines,
            ack_sender,
            &mut output,
        )
        .await;
        assert_eq!(
            String::from_utf8(output).expect("output is UTF-8"),
            "Commands:\n\
             \tping -- Reply with pong\n\
             \thelp <[command]> -- Print help for all commands, or for a single command\n\
             \texit -- Exit the REPL\n\
             \tquit -- Exit the REPL\n"
        );
        assert!(handler.handled.is_empty());
    }

    #[fuchsia::test]
    async fn help_includes_footer() {
        let mut handler =
            TestHandler { footer: Some("See README.md".to_string()), ..Default::default() };
        let outcome = dispatch(&mut handler, &["help"]).await;
        assert!(outcome.output.ends_with("\tquit -- Exit the REPL\n\nSee README.md\n"));
    }

    #[fuchsia::test]
    async fn help_for_a_single_command_and_extra_args() {
        let mut handler = TestHandler::default();
        let outcome = dispatch(
            &mut handler,
            &["help echo", "help help", "help exit", "help quit", "help bogus", "help echo extra"],
        )
        .await;
        assert_eq!(
            outcome.output,
            "echo [-n] <text> -- Print text\n\
             help <[command]> -- Print help for all commands, or for a single command\n\
             exit -- Exit the REPL\n\
             quit -- Exit the REPL\n\
             \"bogus\" is not a valid command. Type 'help' for a list of commands.\n\
             help <[command]> -- Print help for all commands, or for a single command\n"
        );
        assert_eq!(outcome.ack_count, 6);
    }
}
