// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::{zx_paddr_t, zx_vaddr_t};

use super::invalidate::{InvEpt, InvVpid, ept_pointer_from_pml4, invept, invvpid};

// TODO(https://fxbug.dev/576430432): Remove `rust_invvpid` and `rust_invept` once their C++
// callers are ported to Rust.

/// FFI entry point for `invvpid`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_invvpid(invalidation: InvVpid, vpid: u16, address: zx_vaddr_t) {
    invvpid(invalidation, vpid, address)
}

/// FFI entry point for `invept`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_invept(invalidation: InvEpt, eptp: u64) {
    invept(invalidation, eptp)
}

/// FFI entry point for `ept_pointer_from_pml4`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ept_pointer_from_pml4(ept_pml4: zx_paddr_t) -> u64 {
    ept_pointer_from_pml4(ept_pml4)
}
