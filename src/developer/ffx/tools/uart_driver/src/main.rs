// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_build_version::build_info;
use ffx_uart_host_driver::run;

#[fuchsia_async::run_singlethreaded]
async fn main() {
    // Retain version and build metadata symbols required by the build system
    // to prevent the linker from dead-stripping them.
    let _build_info = build_info();
    run().await;
}
