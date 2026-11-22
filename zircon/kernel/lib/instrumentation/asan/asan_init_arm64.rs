// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 KASAN shadow initialization stubs.

// TODO(https://fxbug.dev/379891035): Implement me in physboot!

/// Adds the virtual region defined by `[start, start + size)` to the regions
/// instrumented by asan. After calling this function, users can call
/// `asan_poison_shadow` on the bytes in the newly added region.
///
/// # Safety
///
/// The caller must ensure that `size > 0` and `[start, start + size)` is a valid kernel virtual
/// address range within the KASAN shadow-mapped kernel address space.
#[sanitize(address = "off")]
pub unsafe fn asan_map_shadow_for(_start: usize, _size: usize) {}

/// KASAN initialization after the PMM is available.
///
/// # Safety
///
/// Must only be called once during single-threaded early boot (`LK_INIT_LEVEL_VM_PREHEAP`).
#[sanitize(address = "off")]
pub(super) unsafe fn arch_asan_early_init() {}

/// KASAN initialization after the VM/kernel_aspace are available and we can update kernel mappings.
///
/// # Safety
///
/// Must only be called once during single-threaded boot (`LK_INIT_LEVEL_VM`).
#[sanitize(address = "off")]
pub(super) unsafe fn arch_asan_late_init() {}
