// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use ffx_build_version::build_info;
use ffx_usb_host_driver::run;

fn main() {
    // There are some symbols that the build system uses to examine FFX plugin
    // binaries and get metadata from them. Calling this here prevents them from
    // being garbage collected by the linker.
    let _build_info = build_info();
    // SAFETY: Called at process startup before any background threads or async
    // executors are started, so it is safe to fork the process.
    unsafe { run() };
}
