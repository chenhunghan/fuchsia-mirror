// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Binary entry point for the target-side FDomain UART driver daemon.

use fdomain_uart_driver_lib::{Result, run_driver};

#[fuchsia::main(logging_tags = ["fdomain_uart_driver"])]
async fn main() -> Result<()> {
    run_driver().await
}
