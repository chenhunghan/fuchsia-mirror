// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `hypervisor::Page`.
// TODO(https://fxbug.dev/572853263): Remove once all C++ callers are ported to Rust.

use core::ffi::c_void;
use core::mem::MaybeUninit;
use zx_status::Status;

use super::page::Page;
use crate::kernel::types::PAddr;

/// Initializes a `Page` in place at `page`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_hypervisor_page_init(page: &mut MaybeUninit<Page>) {
    page.write(Page::new());
}

/// Drops `page` in place, freeing its underlying PMM page if allocated.
///
/// # Safety
///
/// The caller must ensure that `page` is a valid, non-null, 8-byte aligned pointer to an
/// initialized, live `Page` with exclusive ownership (no concurrent accesses or live references on
/// any thread), and that `page` is not accessed after this call.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_hypervisor_page_free(page: *mut Page) {
    // SAFETY: By `# Safety` precondition, `page` is non-null, 8-byte aligned (`align_of::<Page>()
    // == 8`), points to an initialized, live `Page` with exclusive ownership, and will not be
    // accessed again after `drop_in_place`.
    unsafe {
        core::ptr::drop_in_place(page);
    }
}

/// Allocates a wired PMM page for `page` and fills it with `fill`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_hypervisor_page_alloc(page: &mut Page, fill: u8) -> Result<(), Status> {
    page.alloc(fill)
}

/// Returns the physmap virtual address of `page`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_hypervisor_page_virtual_address(page: &Page) -> *mut c_void {
    page.virtual_address::<c_void>()
}

/// Returns the physical address of `page`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_hypervisor_page_physical_address(page: &Page) -> PAddr {
    page.physical_address()
}

/// Returns `true` if `page` is allocated.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_hypervisor_page_is_allocated(page: &Page) -> bool {
    page.is_allocated()
}
