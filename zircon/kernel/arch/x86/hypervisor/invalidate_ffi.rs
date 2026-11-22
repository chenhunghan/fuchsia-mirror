// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::zx_paddr_t;

use super::invalidate::ept_pointer_from_pml4;

/// FFI entry point for `ept_pointer_from_pml4`.
#[unsafe(no_mangle)]
extern "C" fn rust_ept_pointer_from_pml4(ept_pml4: zx_paddr_t) -> u64 {
    ept_pointer_from_pml4(ept_pml4)
}
