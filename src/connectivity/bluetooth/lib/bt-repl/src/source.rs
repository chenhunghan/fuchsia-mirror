// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt;
use std::marker::PhantomData;

use crate::commands::CommandSet;

/// Supplies the command names, usage, hints and argument completions used by
/// the REPL editor and by the built-in `help` command.
///
/// Most tools use [`Commands`], which implements this trait for any
/// [`CommandSet`]. Tools that complete arguments can use
/// [`Commands::with_arg_completer`] or implement `CommandSource` directly. Two
/// sources can be combined with a tuple: `(A, B)` implements `CommandSource`,
/// with commands in `A` taking precedence over commands with the same name in
/// `B`.
pub trait CommandSource: Send + Sync + 'static {
    /// Returns the names of all commands provided by this source.
    fn command_names(&self) -> Vec<String>;

    /// Returns the usage line of every command provided by this source.
    fn usages(&self) -> Vec<String> {
        self.command_names().into_iter().filter_map(|name| self.usage(&name)).collect()
    }

    /// Returns the usage line of `command`, or `None` if this source does not
    /// provide `command`.
    fn usage(&self, command: &str) -> Option<String>;

    /// Returns the hint shown after `command` has been typed: its flags
    /// followed by its arguments. Returns `None` if this source does not
    /// provide `command` or if `command` takes no flags or arguments.
    fn hint(&self, command: &str) -> Option<String>;

    /// Returns completion candidates for the argument of `command`.
    ///
    /// `partial` is the part of the argument typed so far. Candidates must be
    /// bare argument values (e.g. `"0123"`, not `"connect 0123"`); the editor
    /// filters candidates that start with `partial` and prepends `command`
    /// automatically.
    fn complete_argument(&self, _command: &str, _partial: &str) -> Vec<String> {
        Vec::new()
    }
}

/// A [`CommandSource`] for the commands of a [`CommandSet`].
pub struct Commands<C>(PhantomData<fn() -> C>);

impl<C> Commands<C> {
    /// Creates a source for the commands of `C`.
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// Returns a [`CommandSource`] that completes the first argument of
    /// commands in `C` with `completer`.
    ///
    /// `completer` is called with the parsed command and the part of the
    /// argument typed so far, and must return bare argument values without the
    /// leading command name.
    pub fn with_arg_completer<F>(self, completer: F) -> WithArgCompleter<C, F>
    where
        C: CommandSet + 'static,
        F: Fn(C, &str) -> Vec<String> + Send + Sync + 'static,
    {
        WithArgCompleter { commands: self, completer }
    }
}

impl<C> Default for Commands<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Clone for Commands<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Commands<C> {}

impl<C> fmt::Debug for Commands<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Commands<{}>", std::any::type_name::<C>())
    }
}

impl<C: CommandSet + 'static> CommandSource for Commands<C> {
    fn command_names(&self) -> Vec<String> {
        C::names()
    }

    fn usages(&self) -> Vec<String> {
        C::all().iter().map(CommandSet::usage).collect()
    }

    fn usage(&self, command: &str) -> Option<String> {
        command.parse::<C>().ok().map(|command| command.usage())
    }

    fn hint(&self, command: &str) -> Option<String> {
        let command = command.parse::<C>().ok()?;
        let hint = format!("{}{}", command.flags(), command.arguments());
        (!hint.is_empty()).then_some(hint)
    }
}

/// A [`CommandSource`] for the commands of a [`CommandSet`] that uses a
/// closure to complete command arguments.
///
/// Created by [`Commands::with_arg_completer`].
pub struct WithArgCompleter<C, F> {
    commands: Commands<C>,
    completer: F,
}

impl<C, F: Clone> Clone for WithArgCompleter<C, F> {
    fn clone(&self) -> Self {
        Self { commands: self.commands, completer: self.completer.clone() }
    }
}

impl<C, F: Copy> Copy for WithArgCompleter<C, F> {}

impl<C, F> fmt::Debug for WithArgCompleter<C, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WithArgCompleter<{}>", std::any::type_name::<C>())
    }
}

impl<C, F> CommandSource for WithArgCompleter<C, F>
where
    C: CommandSet + 'static,
    F: Fn(C, &str) -> Vec<String> + Send + Sync + 'static,
{
    fn command_names(&self) -> Vec<String> {
        self.commands.command_names()
    }

    fn usage(&self, command: &str) -> Option<String> {
        self.commands.usage(command)
    }

    fn hint(&self, command: &str) -> Option<String> {
        self.commands.hint(command)
    }

    fn complete_argument(&self, command: &str, partial: &str) -> Vec<String> {
        match command.parse::<C>() {
            Ok(command) => (self.completer)(command, partial),
            Err(_) => Vec::new(),
        }
    }
}

impl<A: CommandSource, B: CommandSource> CommandSource for (A, B) {
    fn command_names(&self) -> Vec<String> {
        let mut names = self.0.command_names();
        for name in self.1.command_names() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    fn usage(&self, command: &str) -> Option<String> {
        self.0.usage(command).or_else(|| self.1.usage(command))
    }

    fn hint(&self, command: &str) -> Option<String> {
        if self.0.usage(command).is_some() { self.0.hint(command) } else { self.1.hint(command) }
    }

    fn complete_argument(&self, command: &str, partial: &str) -> Vec<String> {
        if self.0.usage(command).is_some() {
            self.0.complete_argument(command, partial)
        } else {
            self.1.complete_argument(command, partial)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::gen_commands! {
        enum FirstCommand {
            Shared = ("shared", [], [], "Shared from first set"),
            Connect = ("connect", [], ["peer-id"], "Connect to a peer"),
        }
    }

    crate::gen_commands! {
        enum SecondCommand {
            Shared = ("shared", ["--second"], ["ignored-arg"], "Shared from second set"),
            Scan = ("scan", ["--active"], ["[duration]"], "Scan for peers"),
        }
    }

    fn peer_source() -> impl CommandSource {
        Commands::<FirstCommand>::new().with_arg_completer(|command, _partial| match command {
            FirstCommand::Connect => vec!["0123".to_string(), "4567".to_string()],
            FirstCommand::Shared => Vec::new(),
        })
    }

    #[fuchsia::test]
    fn commands_source_reports_names_and_usages() {
        let source = Commands::<SecondCommand>::new();
        assert_eq!(source.command_names(), vec!["shared", "scan"]);
        assert_eq!(
            source.usages(),
            vec![
                "shared [--second] <ignored-arg> -- Shared from second set".to_string(),
                "scan [--active] <[duration]> -- Scan for peers".to_string(),
            ]
        );
        assert_eq!(
            source.usage("scan"),
            Some("scan [--active] <[duration]> -- Scan for peers".to_string())
        );
        assert_eq!(source.usage("bogus"), None);
        assert!(format!("{source:?}").starts_with("Commands<"));
    }

    #[fuchsia::test]
    fn commands_source_hints_flags_and_arguments() {
        let source = Commands::<SecondCommand>::new();
        assert_eq!(source.hint("scan"), Some("[--active] <[duration]> ".to_string()));
        assert_eq!(source.hint("shared"), Some("[--second] <ignored-arg> ".to_string()));
        assert_eq!(source.hint("bogus"), None);
        assert_eq!(source.complete_argument("scan", ""), Vec::<String>::new());
    }

    #[fuchsia::test]
    fn with_arg_completer_completes_parsed_commands() {
        let source = peer_source();
        assert_eq!(source.command_names(), vec!["shared", "connect"]);
        assert_eq!(source.hint("connect"), Some("<peer-id> ".to_string()));
        assert_eq!(source.complete_argument("connect", ""), vec!["0123", "4567"]);
        assert_eq!(source.complete_argument("shared", ""), Vec::<String>::new());
        assert_eq!(source.complete_argument("bogus", ""), Vec::<String>::new());
        let with_completer = Commands::<FirstCommand>::new().with_arg_completer(|_, _| Vec::new());
        let cloned = with_completer;
        assert!(format!("{cloned:?}").starts_with("WithArgCompleter<"));
    }

    #[fuchsia::test]
    fn combined_source_merges_and_shadows_commands() {
        let source = (Commands::<FirstCommand>::new(), Commands::<SecondCommand>::new());
        assert_eq!(source.command_names(), vec!["shared", "connect", "scan"]);
        assert_eq!(
            source.usages(),
            vec![
                "shared -- Shared from first set".to_string(),
                "connect <peer-id> -- Connect to a peer".to_string(),
                "scan [--active] <[duration]> -- Scan for peers".to_string(),
            ]
        );
        assert_eq!(source.usage("shared"), Some("shared -- Shared from first set".to_string()));
        assert_eq!(
            source.usage("connect"),
            Some("connect <peer-id> -- Connect to a peer".to_string())
        );
        assert_eq!(
            source.usage("scan"),
            Some("scan [--active] <[duration]> -- Scan for peers".to_string())
        );
        // `FirstCommand::Shared` takes no arguments and must shadow `SecondCommand::Shared`
        // rather than falling back to the second set's hint.
        assert_eq!(source.hint("shared"), None);
        assert_eq!(source.hint("connect"), Some("<peer-id> ".to_string()));
        assert_eq!(source.hint("scan"), Some("[--active] <[duration]> ".to_string()));
    }

    #[fuchsia::test]
    fn combined_source_merges_and_shadows_argument_completions() {
        let second_with_completer = Commands::<SecondCommand>::new().with_arg_completer(
            |command, _partial| match command {
                SecondCommand::Shared => vec!["from-second".to_string()],
                SecondCommand::Scan => vec!["10".to_string()],
            },
        );
        let source = (peer_source(), second_with_completer);
        assert_eq!(source.complete_argument("connect", ""), vec!["0123", "4567"]);
        assert_eq!(source.complete_argument("scan", ""), vec!["10"]);
        // `FirstCommand::Shared` shadows `SecondCommand::Shared`.
        assert_eq!(source.complete_argument("shared", ""), Vec::<String>::new());
    }
}
