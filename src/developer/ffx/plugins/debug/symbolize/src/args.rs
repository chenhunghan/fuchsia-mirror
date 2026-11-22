// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

/// Options for "ffx debug symbolize".
#[ffx_command()]
#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
#[argh(
    subcommand,
    name = "symbolize",
    description = "symbolize backtraces in markup format",
    short = 's',
    note = "Parses logs from stdin and converts symbolizer markup into human-readable
stack traces using local or remote debug symbols.

By default, passes --prettify-backtrace and --omit-module-lines to the
underlying symbolizer. Use --no-prettify to disable.

Arguments starting with '-' for the symbolizer must follow '--'.
To see available symbolizer flags, run: `ffx debug symbolize -- --help`",
    example = "Symbolize piped logs:
  cat raw_log.txt | ffx debug symbolize

Pass flags to the underlying symbolizer:
  ffx debug symbolize -- -s /path/to/symbols --verbose

Show underlying symbolizer help:
  ffx debug symbolize -- --help"
)]
pub struct SymbolizeCommand {
    /// start the authentication process.
    #[argh(switch)]
    pub auth: bool,

    /// do not prettify the backtraces.
    #[argh(switch)]
    pub no_prettify: bool,

    /// extra arguments passed to the symbolizer. Any arguments starting with "-" must be after a "--" separator.
    #[argh(positional)]
    pub symbolizer_args: Vec<String>,
}
