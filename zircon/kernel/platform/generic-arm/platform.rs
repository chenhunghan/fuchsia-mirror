// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::ResourceDispatcher;
use crate::pdev_interrupt::{interrupt_get_base_vector, interrupt_get_max_vector};
use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use core::ffi::c_void;
use core::pin::Pin;
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
use zx_types::{
    ARM_SMC_SERVICE_CALL_NUM_MAX, ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SMC,
    ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_COUNT,
};

static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();

// TODO(https://fxbug.dev/507423574): Remove once `allocate_persistent_ram` is ported to Rust.
/// Initializes the platform's [`MappedCrashlog`] with `[base, base + size)` and binds it as the
/// [`PlatformCrashlog`] implementation.
///
/// # Safety
///
/// If `size > 0`, `base` must be 8-byte aligned and point to a valid, mutable persistent RAM
/// region of `size` bytes that remains mapped and exclusively owned by [`MappedCrashlog`] for the
/// lifetime of the kernel. Must be called at most once during single-threaded early boot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mapped_crashlog_bind(base: *mut c_void, size: usize) {
    // SAFETY: The caller upholds the pointer validity, alignment, exclusive ownership,
    // single-threaded early-boot initialization, and `'static` lifetime requirements.
    unsafe {
        let range = zr::slice_from_raw_parts_mut(base.cast::<u8>(), size);
        let _ = Pin::static_ref(&MAPPED_CRASHLOG).init_pin(MappedCrashlog::new(range));
        let _ = PlatformCrashlog::bind(MAPPED_CRASHLOG.get_unchecked());
    }
}

/// Initialize Resource system after the heap is initialized.
fn arm_resource_dispatcher_init_hook(_level: init::LkInitLevel) {
    // 64 bit address space for MMIO on ARM64
    if let Err(status) = ResourceDispatcher::initialize_allocator(ZX_RSRC_KIND_MMIO, 0, usize::MAX)
    {
        kprint!("Resources: Failed to initialize MMIO allocator: {}\n", status.into_raw());
    }
    // Set up IRQs based on values from the GIC
    let (base_vector, max_vector) = (*interrupt_get_base_vector(), *interrupt_get_max_vector());
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_IRQ,
        u64::from(base_vector),
        max_vector as usize,
    ) {
        kprint!("Resources: Failed to initialize IRQ allocator: {}\n", status.into_raw());
    }
    // Set up SMC valid service call range
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SMC,
        0,
        (ARM_SMC_SERVICE_CALL_NUM_MAX as usize) + 1,
    ) {
        kprint!("Resources: Failed to initialize SMC allocator: {}\n", status.into_raw());
    }
    // Set up range of valid system resources.
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SYSTEM,
        0,
        ZX_RSRC_SYSTEM_COUNT as usize,
    ) {
        kprint!("Resources: Failed to initialize system allocator: {}\n", status.into_raw());
    }
}

init::lk_init_hook!(arm_resource_init, arm_resource_dispatcher_init_hook, init::LK_INIT_LEVEL_INTC);
