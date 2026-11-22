// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use memalloc::Range;

unsafe extern "C" {
    pub(super) fn cpp_phys_handoff_memory(count: &mut usize) -> *const Range;
    pub(super) fn cpp_phys_handoff_efi_memory_attributes(size: &mut usize) -> *const u8;
}
