// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 spinlocks.
//!
//! A lock holds the number of the CPU holding it plus one, so 0 means unheld.
//!
//! The `arch_spin_*` functions below are the architecture half of `SpinLockBase` in
//! `crate::kernel::spinlock`, which holds the lock word and calls them with interrupts disabled.

// TODO(https://fxbug.dev/581987928): Remove `#![coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#![coverage(off)]

use super::mp::{read_percpu_field, write_percpu_field};
use crate::kernel::spin_tracing::{EncodedLockId, FinishType, Tracer};
use crate::kernel::types::cpu_num_t;
use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

/// Counts a spinlock the calling CPU has just acquired in its `num_spinlocks`.
///
/// # Safety
///
/// The calling CPU has just acquired a spinlock, with interrupts disabled.
#[inline(always)]
unsafe fn on_lock_acquired() {
    let count = read_percpu_field!(num_spinlocks).wrapping_add(1);
    // SAFETY: `num_spinlocks` counts the spinlocks the calling CPU holds, and it has just acquired
    // one. Interrupts are disabled, so no interrupt handler on this CPU changes the count between
    // the read and this write, and the thread cannot move to another CPU. x20 points at this CPU's
    // `arm64_percpu`, and Rust holds no reference to its fields (see `write_percpu_field!`).
    unsafe { write_percpu_field!(num_spinlocks, count) };
}

/// # Safety
///
/// Interrupts are disabled.
#[inline(always)]
unsafe fn arch_spin_lock_core(lock: &AtomicU32, val: cpu_num_t) {
    // SAFETY: `lock` is a live, 4-byte aligned `AtomicU32`, so the exclusive load and store
    // through `lock.as_ptr()` access valid memory that atomic operations may modify concurrently.
    unsafe {
        asm!(
            "sevl",
            "1: wfe",
            "2: ldaxr   {temp:w}, [{lock}]",
            "cbnz    {temp:w}, 1b",
            "stxr    {temp:w}, {val:w}, [{lock}]",
            "cbnz    {temp:w}, 2b",
            temp = out(reg) _,
            lock = in(reg) lock.as_ptr(),
            val = in(reg) val,
            options(nostack),
        );
    }

    // SAFETY: The loop above has acquired `lock`, and the caller has disabled interrupts.
    unsafe { on_lock_acquired() };
}

/// Acquires `lock` without lock trace instrumentation.
///
/// # Safety
///
/// Interrupts are disabled on the calling CPU, and `cpu` equals `curr_cpu_num()`.
#[inline(always)]
pub unsafe fn arch_spin_lock_non_instrumented(lock: &AtomicU32, cpu: cpu_num_t) {
    let val = cpu.wrapping_add(1);
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_lock_core(lock, val) };
}

/// Acquires `lock`, tracing the spin with `encoded_lock_id` if the first attempt fails.
///
/// # Safety
///
/// Interrupts are disabled on the calling CPU, and `cpu` equals `curr_cpu_num()`.
#[inline(always)]
pub unsafe fn arch_spin_lock_trace_instrumented(
    lock: &AtomicU32,
    cpu: cpu_num_t,
    encoded_lock_id: EncodedLockId,
) {
    let val = cpu.wrapping_add(1);

    if lock.compare_exchange(0, val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        // SAFETY: The compare-exchange has acquired `lock`, and the caller has disabled
        // interrupts.
        unsafe { on_lock_acquired() };
        return;
    }

    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_lock_trace_instrumented_contended(lock, val, encoded_lock_id) };
}

/// The spin and trace of `arch_spin_lock_trace_instrumented`, apart so the uncontended path
/// needs no stack frame for the tracer.
///
/// # Safety
///
/// Interrupts are disabled.
#[cold]
#[inline(never)]
unsafe fn arch_spin_lock_trace_instrumented_contended(
    lock: &AtomicU32,
    val: cpu_num_t,
    encoded_lock_id: EncodedLockId,
) {
    let spin_tracer = Tracer::<true>::new();
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_lock_core(lock, val) };
    spin_tracer.finish(FinishType::LockAcquired, encoded_lock_id);
}

/// Tries to acquire `lock` without waiting for its holder; a failed store-exclusive is retried, so
/// a failure means another CPU held the lock. Returns false if the lock was acquired and true
/// otherwise.
///
/// Note: trylock operations are not permitted to fail spuriously, even on
/// architectures with weak memory ordering.  If a trylock operation fails, it
/// must be because the lock was actually observed to be held by another thread
/// during the attempt.
///
/// # Safety
///
/// Interrupts are disabled on the calling CPU, and `cpu` equals `curr_cpu_num()`.
#[inline(always)]
pub unsafe fn arch_spin_trylock(lock: &AtomicU32, cpu: cpu_num_t) -> bool {
    let val = cpu.wrapping_add(1);
    let out: cpu_num_t;
    // SAFETY: See `arch_spin_lock_core`.
    unsafe {
        asm!(
            "1:",
            "ldaxr   {out:w}, [{lock}]",
            "cbnz    {out:w}, 2f",
            "stxr    {out:w}, {val:w}, [{lock}]",
            // Even though this is a try lock, if the store on acquire fails we try
            // again. This is to prevent spurious failures from misleading the caller
            // into thinking the lock is held by another thread. See
            // https://fxbug.dev/42164720 for details.
            "cbnz    {out:w}, 1b",
            "2:",
            "clrex",
            out = out(reg) out,
            lock = in(reg) lock.as_ptr(),
            val = in(reg) val,
            options(nostack),
        );
    }

    if out == 0 {
        // SAFETY: The loop above has acquired `lock`, and the caller has disabled interrupts.
        unsafe { on_lock_acquired() };
    }
    out != 0
}

/// Releases `lock`, which the calling CPU holds.
///
/// Takes a raw pointer `*const AtomicU32` rather than `&AtomicU32` because releasing the lock
/// allows another CPU to immediately acquire it and free the containing object before this
/// function returns.
///
/// # Safety
///
/// `lock` points to a live, properly aligned `AtomicU32` that the calling CPU holds, and
/// interrupts are disabled.
#[inline(always)]
pub unsafe fn arch_spin_unlock(lock: *const AtomicU32) {
    let count = read_percpu_field!(num_spinlocks).wrapping_sub(1);
    // SAFETY: `num_spinlocks` counts the spinlocks the calling CPU holds, and it is releasing
    // `lock`, which it holds. Interrupts are disabled, so no interrupt handler on this CPU changes
    // the count between the read and this write, and the thread cannot move to another CPU. x20
    // points at this CPU's `arm64_percpu`, and Rust holds no reference to its fields (see
    // `write_percpu_field!`).
    unsafe { write_percpu_field!(num_spinlocks, count) };
    // SAFETY: The caller guarantees `lock` is live, 4-byte aligned, and held by this CPU until
    // this store. Using an inline `stlr` avoids forming `&AtomicU32` across the release store and
    // avoids `ub_checks` branches on the raw pointer in debug-assertion builds.
    unsafe {
        asm!(
            "stlr wzr, [{lock}]",
            lock = in(reg) lock,
            options(nostack, preserves_flags),
        );
    }
}
