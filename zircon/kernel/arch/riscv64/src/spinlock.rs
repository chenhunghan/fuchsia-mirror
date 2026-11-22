// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RISC-V 64 spinlock implementation.
//!
//! Simple spinning lock, using LR/SC CAS instructions.  Stores the current cpu
//! number + 1 for debugging purposes, so that 0 means unheld.
//!
//! The `arch_spin_*` functions below are the architecture half of `SpinLockBase`
//! in `crate::kernel::spinlock`, which holds the lock word and calls them with
//! interrupts disabled.

// TODO(https://fxbug.dev/581987928): Remove `#![coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#![coverage(off)]

use super::arch::arch_yield;
use super::mp::{percpu_dec_num_spinlocks, percpu_inc_num_spinlocks};
use crate::kernel::spin_tracing::{EncodedLockId, FinishType, Tracer};
use crate::kernel::types::cpu_num_t;
use core::sync::atomic::{AtomicU32, Ordering};

/// Counts a spinlock the calling CPU has just acquired in its `num_spinlocks`.
#[inline(always)]
fn on_lock_acquired() {
    percpu_inc_num_spinlocks();
}

/// Spins until `lock` is acquired with `new_val`, after a failed first attempt.
///
/// Kept out of line so that the uncontended path inlined into every lock acquisition stays small.
/// As on x86, waits for the lock to appear free with plain loads before retrying the CAS, to limit
/// coherence traffic while contended.
#[cold]
#[inline(never)]
fn arch_spin_lock_contended(lock: &AtomicU32, new_val: u32) {
    loop {
        loop {
            arch_yield();
            if lock.load(Ordering::Relaxed) == 0 {
                break;
            }
        }
        if lock.compare_exchange_weak(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }
    }
}

/// Acquires `lock` without lock trace instrumentation.
///
/// # Safety
///
/// Interrupts are disabled on the calling CPU, and `cpu` equals `curr_cpu_num()`.
#[inline(always)]
pub unsafe fn arch_spin_lock_non_instrumented(lock: &AtomicU32, cpu: cpu_num_t) {
    let new_val = cpu.wrapping_add(1);
    if lock.compare_exchange_weak(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_contended(lock, new_val);
    }
    on_lock_acquired();
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
    let new_val = cpu.wrapping_add(1);

    if lock.compare_exchange(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        on_lock_acquired();
        return;
    }

    arch_spin_lock_trace_instrumented_contended(lock, new_val, encoded_lock_id);
}

/// The spin and trace of `arch_spin_lock_trace_instrumented`, apart so the uncontended path
/// needs no stack frame for the tracer.
#[cold]
#[inline(never)]
fn arch_spin_lock_trace_instrumented_contended(
    lock: &AtomicU32,
    new_val: u32,
    encoded_lock_id: EncodedLockId,
) {
    let spin_tracer = Tracer::<true>::new();
    arch_spin_lock_contended(lock, new_val);
    on_lock_acquired();
    spin_tracer.finish(FinishType::LockAcquired, encoded_lock_id);
}

/// Tries to acquire `lock` without waiting for its holder. Returns false if the lock was
/// acquired and true otherwise.
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
    let new_val = cpu.wrapping_add(1);
    let failed = lock.compare_exchange(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_err();
    if !failed {
        on_lock_acquired();
    }
    failed
}

/// Releases `lock`, which the calling CPU holds.
///
/// Takes a raw pointer `*const AtomicU32` rather than `&AtomicU32` because releasing the lock
/// allows another CPU to immediately acquire it and free the containing object before this
/// function returns.
///
/// # Safety
///
/// `lock` points to a live, 4-byte-aligned `AtomicU32` that the calling CPU holds, and interrupts
/// are disabled on the calling CPU.
#[inline(always)]
pub unsafe fn arch_spin_unlock(lock: *const AtomicU32) {
    percpu_dec_num_spinlocks();
    // SAFETY: The caller guarantees `lock` is live, 4-byte aligned, and held by this CPU until
    // this store. Using an inline RVWMO release store (`fence rw, w; sw`) avoids forming
    // `&AtomicU32` across the release store and avoids `ub_checks` branches on the raw pointer in
    // debug-assertion builds.
    unsafe {
        core::arch::asm!(
            "fence rw, w",
            "sw zero, 0({lock})",
            lock = in(reg) lock,
            options(nostack, preserves_flags),
        );
    }
}

zr::static_assert!(core::mem::size_of::<AtomicU32>() == 4);
zr::static_assert!(core::mem::align_of::<AtomicU32>() == 4);
