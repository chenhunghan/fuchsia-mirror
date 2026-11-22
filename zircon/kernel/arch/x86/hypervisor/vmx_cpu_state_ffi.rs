// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::vmx_cpu_state::{broadcast_invept, vmx_ept_supports_large_pages};

/// FFI entry point for `broadcast_invept`.
#[unsafe(no_mangle)]
extern "C" fn rust_broadcast_invept(eptp: u64) {
    broadcast_invept(eptp)
}

/// FFI entry point for `vmx_ept_supports_large_pages`.
#[unsafe(no_mangle)]
extern "C" fn rust_vmx_ept_supports_large_pages() -> bool {
    vmx_ept_supports_large_pages()
}
