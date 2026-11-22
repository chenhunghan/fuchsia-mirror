// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::descriptor;

/// Temporary GDT defined in assembly is used during AP/BP setup process.
use super::descriptor::TempGdt;

unsafe extern "C" {
    #[link_name = "_temp_gdt"]
    pub(super) static TEMP_GDT: TempGdt;

    #[link_name = "_temp_gdt_end"]
    pub(super) static TEMP_GDT_END: u8;
}

/// FFI entry point for [`clear_tss_busy`].
///
/// # Safety
///
/// The caller must ensure that `sel` is a valid 64-bit TSS selector index within the active GDT
/// and that the caller has exclusive access to that TSS descriptor entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_clear_tss_busy(sel: u16) {
    // SAFETY: Caller guarantees `sel` is a valid, exclusively accessed 64-bit TSS selector.
    unsafe { descriptor::clear_tss_busy(sel) };
}

/// FFI entry point for [`gdt_get`].
#[unsafe(no_mangle)]
pub extern "C" fn gdt_get() -> usize {
    descriptor::gdt_get()
}
