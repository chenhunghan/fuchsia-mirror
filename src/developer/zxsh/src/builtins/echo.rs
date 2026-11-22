// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::{EXIT_SUCCESS, ShellState};
use crate::string::process_escape_bytes;
use bstr::{BString, ByteSlice};
use std::io::{Read, Write};

pub fn builtin_echo(
    args: &[BString],
    _env: &mut ShellState,
    _stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    _stderr: &mut dyn Write,
) -> i32 {
    let (nonl, positional_args) = if args.first().map(|a| a.as_bytes()) == Some(b"-n") {
        (true, &args[1..])
    } else {
        (false, args)
    };

    for (i, arg) in positional_args.iter().enumerate() {
        if i > 0 {
            let _ = stdout.write_all(b" ");
        }
        let (processed, halt) = process_escape_bytes(arg.as_bytes());
        let _ = stdout.write_all(&processed);
        if halt {
            return EXIT_SUCCESS;
        }
    }

    if !nonl {
        let _ = stdout.write_all(b"\n");
    }

    EXIT_SUCCESS
}
