// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `hypervisor::InterruptTracker`.
// TODO(https://fxbug.dev/572853263): Remove once all C++ callers are ported to Rust.

use core::mem::MaybeUninit;
use pin_init::PinInit;
use zx_status::Status;

use super::interrupt_tracker::InterruptTracker;
use crate::platform_rs::timer::InstantMono;

type FfiInterruptTracker = InterruptTracker<256>;

/// Initializes an `InterruptTracker<256>` in place at `tracker`.
///
/// # Safety
///
/// The caller must ensure that `tracker` remains pinned in memory (never moved, overwritten, or
/// deallocated without running `rust_interrupt_tracker_destroy`) until
/// `rust_interrupt_tracker_destroy` completes.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_tracker_init(
    tracker: &mut MaybeUninit<FfiInterruptTracker>,
) {
    // SAFETY: `tracker.as_mut_ptr()` is non-null, 8-byte aligned, and valid for exclusive writes of
    // `size_of::<FfiInterruptTracker>()` bytes during this call, and by `# Safety` precondition the
    // pointee remains pinned until dropped in place by `rust_interrupt_tracker_destroy`. We call
    // `PinInit::__pinned_init` directly rather than `InPlaceWrite::write_pin_init` because
    // `InPlaceWrite` is only implemented for `&'static mut MaybeUninit<T>` and `Box`.
    let Ok(()) = unsafe { FfiInterruptTracker::init().__pinned_init(tracker.as_mut_ptr()) };
}

/// Drops `tracker` in place.
///
/// # Safety
///
/// The caller must ensure that `tracker` is a valid, non-null, 8-byte aligned pointer to an
/// initialized, live `InterruptTracker<256>` with exclusive ownership (no concurrent accesses or
/// live references on any thread), and that `tracker` is not accessed after this call.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_tracker_destroy(tracker: *mut FfiInterruptTracker) {
    // SAFETY: By `# Safety` precondition, `tracker` is non-null, 8-byte aligned
    // (`align_of::<FfiInterruptTracker>() == 8`), points to an initialized, live
    // `InterruptTracker<256>` with exclusive ownership, and will not be accessed again after
    // `drop_in_place`.
    unsafe {
        core::ptr::drop_in_place(tracker);
    }
}

/// Returns whether there are pending interrupts in `tracker`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_pending(tracker: &FfiInterruptTracker) -> bool {
    tracker.pending()
}

/// Clears all vectors in `[min, max)` in `tracker`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_clear(tracker: &FfiInterruptTracker, min: u32, max: u32) {
    tracker.clear(min, max);
}

/// Pops `vector` from `tracker` if pending.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_try_pop(
    tracker: &FfiInterruptTracker,
    vector: u32,
) -> bool {
    tracker.try_pop(vector)
}

/// Pops the highest-priority pending interrupt from `tracker`, writing it to `*vector_out`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_pop(
    tracker: &FfiInterruptTracker,
    vector_out: &mut MaybeUninit<u32>,
) -> bool {
    if let Some(vector) = tracker.pop() {
        vector_out.write(vector);
        true
    } else {
        false
    }
}

/// Tracks `vector` in `tracker`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_track(
    tracker: &FfiInterruptTracker,
    vector: u32,
) -> Result<(), Status> {
    tracker.track(vector)
}

/// Tracks `vector` in `tracker` and signals any waiters.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_interrupt(
    tracker: &FfiInterruptTracker,
    vector: u32,
) -> Result<(), Status> {
    tracker.interrupt(vector)
}

/// Cancels a wait on `tracker`.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub extern "C" fn rust_interrupt_tracker_cancel(tracker: &FfiInterruptTracker) {
    tracker.cancel();
}

/// Waits for an interrupt on `tracker` until `deadline`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `tracker` remains live and pinned for the entire duration of this call (including while the
///   calling thread is blocked).
/// - The calling thread is a running thread with no spinlocks held and no `ChainLockTransaction`
///   active.
/// - Blocking is permitted (`!arch_blocking_disallowed()`, preemption enabled) on entry to this
///   call.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_tracker_wait(
    tracker: &FfiInterruptTracker,
    deadline: InstantMono,
) -> Result<(), Status> {
    tracker.wait(deadline, None)
}
