// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A read-evaluate-print loop (REPL) shared by the Bluetooth command-line
//! tools.
//!
//! A tool declares its commands with [`gen_commands!`], implements
//! [`ReplHandler`] to handle them, and calls [`run`]. The REPL provides line
//! editing and history, command and argument completion, argument hints, and
//! the built-in `help`, `exit` and `quit` commands, and handles empty lines,
//! invalid commands and command errors the same way for every tool.
//!
//! # Example
//!
//! ```ignore
//! use anyhow::Error;
//! use bt_repl::{Commands, ReplControl, ReplHandler};
//!
//! bt_repl::gen_commands! {
//!     pub enum Cmd {
//!         Connect = ("connect", [], ["peer-id"], "Connect to a peer"),
//!         Peers = ("peers", [], [], "List known peers"),
//!     }
//! }
//!
//! struct Handler;
//!
//! impl ReplHandler for Handler {
//!     type Command = Cmd;
//!
//!     async fn handle(&mut self, command: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
//!         match command {
//!             Cmd::Connect => println!("connecting to {:?}", args.first()),
//!             Cmd::Peers => println!("no peers"),
//!         }
//!         Ok(ReplControl::Continue)
//!     }
//! }
//!
//! #[fuchsia::main]
//! async fn main() -> Result<(), Error> {
//!     bt_repl::run("tool> ", Commands::<Cmd>::new(), Handler).await
//! }
//! ```

mod commands;
mod helper;
mod repl;
mod source;

pub use commands::{CommandSet, UnknownCommand};
pub use repl::{ReplControl, ReplHandler, run};
pub use source::{CommandSource, Commands, WithArgCompleter};
