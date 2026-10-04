// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use user_copy_bindings as bindings;

/// Ensure that all addresses in the range `[vaddr, vaddr+len)` are accessible to the user. If any
/// address in this range is not accessible to the user, `vaddr` and `len` are set to `{0,0}`.
pub fn validate_user_accessible_range(vaddr: &mut usize, len: &mut usize) {
    // SAFETY: pointers are valid for reads and writes.
    unsafe {
        bindings::internal_validate_user_accessible_range(vaddr, len);
    }
}
