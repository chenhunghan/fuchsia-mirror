// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;
use pin_init::InPlaceWrite;
use zx_status::Status;

use super::guest::VpidAllocator;

/// FFI entry point for `VpidAllocator::VpidAllocator`.
///
/// # Safety
///
/// `allocator` must be a non-null, properly aligned pointer to uninitialized storage valid for
/// writes of `size_of::<VpidAllocator>()` bytes, which must not be moved or reused until it is
/// passed to `rust_vpid_allocator_destroy`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vpid_allocator_init(allocator: *mut MaybeUninit<VpidAllocator>) {
    // SAFETY: The caller guarantees `allocator` is valid, aligned, writable storage for a
    // `VpidAllocator`, to which it has exclusive access.
    let slot = unsafe { &mut *allocator };
    // `write_pin_init` leaves the slot pinned in place, as the caller guarantees it is not moved or
    // reused until `rust_vpid_allocator_destroy`. If initialization fails, the `assert!` below
    // panics, so the slot is never used.
    let result = slot.write_pin_init(VpidAllocator::init());
    // `init` only fails if the largest VPID does not fit in a `u16`, which is statically asserted
    // against. This is not a debug assertion, as the storage would otherwise be left uninitialized
    // and later dropped by `rust_vpid_allocator_destroy`.
    assert!(result.is_ok(), "failed to initialize VpidAllocator: {:?}", result.err());
}

/// FFI entry point for `VpidAllocator::~VpidAllocator`.
///
/// # Safety
///
/// `allocator` must be a non-null, properly aligned pointer to a `VpidAllocator` initialized by
/// `rust_vpid_allocator_init`, to which the caller has exclusive access, and which is not used
/// again.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vpid_allocator_destroy(allocator: *mut VpidAllocator) {
    // SAFETY: The caller guarantees `allocator` points to an initialized `VpidAllocator` that it
    // has exclusive access to and does not use again.
    unsafe { core::ptr::drop_in_place(allocator) };
}

/// FFI entry point for `VpidAllocator::TryAlloc`.
#[unsafe(no_mangle)]
extern "C" fn rust_vpid_allocator_try_alloc(
    allocator: &VpidAllocator,
    out: &mut u16,
) -> Result<(), Status> {
    *out = allocator.try_alloc()?;
    Ok(())
}

/// FFI entry point for `VpidAllocator::Free`.
#[unsafe(no_mangle)]
extern "C" fn rust_vpid_allocator_free(allocator: &VpidAllocator, vpid: u16) -> Result<(), Status> {
    allocator.free(vpid)
}
