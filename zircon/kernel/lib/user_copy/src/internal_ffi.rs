// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// TODO(https://fxbug.dev/576430218): Remove once C++ callers of `validate_user_accessible_range`
// are removed.
/// Ensure that all addresses in the range `[vaddr, vaddr+len)` are accessible to the user. If any
/// address in this range is not accessible to the user, `vaddr` and `len` are set to `{0,0}`.
// SAFETY: `rust_validate_user_accessible_range` is globally unique and its signature matches the
// `extern "C"` declaration in `<lib/user_copy/internal.h>`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_validate_user_accessible_range(vaddr: &mut usize, len: &mut usize) {
    super::internal::validate_user_accessible_range(vaddr, len);
}
