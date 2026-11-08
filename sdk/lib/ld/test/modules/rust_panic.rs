// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(unexpected_cfgs)]

use core::panic::PanicInfo;

#[panic_handler]
fn rust_panic(_: &PanicInfo<'_>) -> ! {
    // TODO(https://fxbug.dev/581545068): Remove after Rust rolls past
    // https://github.com/rust-lang/rust/pull/164731.
    #[cfg(any(
        __rust_toolchain = "IUASEg9BqcUUJMfq-eGpRh9fao7ko9ynyHIJQBr_ctoC",
        __rust_toolchain = "3J8xwTtZSE8drVnCKFEYqgJvdDPoFkFWvvOIkOP90FEC",
        __rust_toolchain = "MkV7w5I7Lg47BvuHm7cburh-jAL2hnh1SpaFNLD48VgC",
        __rust_toolchain = "QWQMruskCovB4lgj1Osipz8TNY1-Pww0I4no7He9CBkC",
    ))]
    core::process::abort_immediate();

    #[cfg(not(any(
        __rust_toolchain = "IUASEg9BqcUUJMfq-eGpRh9fao7ko9ynyHIJQBr_ctoC",
        __rust_toolchain = "3J8xwTtZSE8drVnCKFEYqgJvdDPoFkFWvvOIkOP90FEC",
        __rust_toolchain = "MkV7w5I7Lg47BvuHm7cburh-jAL2hnh1SpaFNLD48VgC",
        __rust_toolchain = "QWQMruskCovB4lgj1Osipz8TNY1-Pww0I4no7He9CBkC",
    )))]
    core::process::crash();
}
