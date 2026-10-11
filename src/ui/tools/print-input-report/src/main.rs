// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::commands::get_descriptor::GetDescriptorArgs;
use crate::commands::list::ListArgs;
use crate::commands::read::ReadArgs;
use anyhow::Result;
use argh::FromArgs;

mod commands;
mod common;
mod descriptor_types;
mod indented_serializer;
mod input_report_types;
#[cfg(test)]
mod testing;

#[derive(FromArgs, Debug)]
/// A tool to dump input reports from input devices.
struct Args {
    #[argh(subcommand)]
    subcommand: Subcommands,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum Subcommands {
    List(ListArgs),
    GetDescriptor(GetDescriptorArgs),
    Read(ReadArgs),
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let args: Args = argh::from_env();

    match args.subcommand {
        Subcommands::List(list_args) => commands::list::run(list_args).await,
        Subcommands::GetDescriptor(descriptor_args) => descriptor_args.run().await,
        Subcommands::Read(read_args) => read_args.run().await,
    }
}
