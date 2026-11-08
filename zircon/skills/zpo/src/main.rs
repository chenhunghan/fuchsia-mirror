// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! zpo: the Zircon Profile Optimizer. The commands land in later changes; for
//! now this only prints help.

use clap::{CommandFactory, Parser};

#[derive(Parser)]
#[command(name = "zpo", version = zpo::VERSION, about = "Zircon Profile Optimizer (zpo)")]
struct Cli {}

fn main() {
    let _ = Cli::parse();
    Cli::command().print_help().expect("could not print help");
}
