// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Standalone binary entrypoint for the `ffx-uart` subtool.
//!
//! This binary provides dual-mode execution for the `ffx-uart` subtool. It allows the tool
//! to be executed either as a subcommand of the main `ffx` CLI (`ffx uart ...`) or directly
//! as a standalone binary (`ffx-uart ...`) on the developer's system `PATH`.
//!
//! Entry point execution is managed by [`fho::FfxTool::execute_tool`], which handles
//! environment initialization, argument parsing via Argh, and command dispatch.

use ffx_tool_uart::UartTool;
use fho::FfxTool;

#[fuchsia_async::run_singlethreaded]
async fn main() {
    UartTool::execute_tool().await
}
