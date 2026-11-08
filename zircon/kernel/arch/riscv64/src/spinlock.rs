// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RISC-V 64 spinlock implementation.
//!
//! Simple spinning lock, using LR/SC CAS instructions.  Stores the current cpu
//! number + 1 for debugging purposes, so that 0 means unheld.

// TODO(https://fxbug.dev/581987928): Remove `#![coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#![coverage(off)]

use super::arch::arch_yield;
use super::mp::{percpu_dec_num_spinlocks, percpu_inc_num_spinlocks};
use crate::kernel::spin_tracing::{EncodedLockId, FinishType, Tracer};
use core::sync::atomic::{AtomicU32, Ordering};

/// Acquire a spinlock without lock trace instrumentation.
///
/// # Safety
///
/// Interrupts are disabled.
#[inline]
pub(crate) unsafe fn arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    let new_val = super::mp::arch_curr_cpu_num() + 1;
    if lock.compare_exchange_weak(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_contended(lock, new_val);
    }
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

/// Acquire a spinlock with lock trace instrumentation.
///
/// # Safety
///
/// Interrupts are disabled.
#[inline]
pub(crate) unsafe fn arch_spin_lock_trace_instrumented(
    lock: &AtomicU32,
    encoded_lock_id: EncodedLockId,
) {
    // SAFETY: The caller has disabled interrupts.
    if unsafe { arch_spin_trylock(lock) } {
        return;
    }
    arch_spin_lock_trace_instrumented_contended(lock, encoded_lock_id);
}

/// Acquires `lock`, tracing the spin, after a failed first attempt.
///
/// Kept out of line so that the uncontended path inlined into every lock acquisition stays small.
#[cold]
#[inline(never)]
fn arch_spin_lock_trace_instrumented_contended(lock: &AtomicU32, encoded_lock_id: EncodedLockId) {
    let spin_tracer = Tracer::<true>::new();
    arch_spin_lock_contended(lock, super::mp::arch_curr_cpu_num() + 1);
    percpu_inc_num_spinlocks();
    spin_tracer.finish(FinishType::LockAcquired, encoded_lock_id);
}

/// Try to acquire a spinlock without blocking.
///
/// Returns `true` if the lock was acquired, or `false` if it was already held.
///
/// # Safety
///
/// Interrupts are disabled.
#[inline]
pub(crate) unsafe fn arch_spin_trylock(lock: &AtomicU32) -> bool {
    let new_val = super::mp::arch_curr_cpu_num() + 1;
    if lock.compare_exchange(0, new_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        percpu_inc_num_spinlocks();
        true
    } else {
        false
    }
}

/// Release a previously held spinlock.
///
/// # Safety
///
/// The calling CPU holds `lock`, and interrupts are disabled.
#[inline]
pub(crate) unsafe fn arch_spin_unlock(lock: &AtomicU32) {
    percpu_dec_num_spinlocks();
    lock.store(0, Ordering::Release);
}

// C FFI exports.
//
// These take `&AtomicU32` rather than `*mut u32`: C++ always passes the address of
// an `arch_spin_lock_t`, which is non-null and suitably aligned, and saying so in
// the signature removes both the cast and the null/alignment checks the raw
// pointer dereference generated on every lock acquisition.
//
// Each requires what `SpinLock` in <kernel/spinlock.h> requires of its callers: interrupts are
// disabled on the calling CPU.

/// Acquires `lock` without lock trace instrumentation.
///
/// # Safety
///
/// Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_lock_non_instrumented(lock) };
}

/// Acquires `lock`, tracing the spin with `encoded_lock_id` if the first attempt fails.
///
/// # Safety
///
/// Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_lock_trace_instrumented(
    lock: &AtomicU32,
    encoded_lock_id: EncodedLockId,
) {
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_lock_trace_instrumented(lock, encoded_lock_id) };
}

/// Tries to acquire `lock` without waiting. Returns `true` if the lock was acquired.
///
/// # Safety
///
/// Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_trylock(lock: &AtomicU32) -> bool {
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spin_trylock(lock) }
}

/// Releases `lock`.
///
/// # Safety
///
/// The calling CPU holds `lock`, and interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_spin_unlock(lock: &AtomicU32) {
    // SAFETY: The caller holds `lock` and has disabled interrupts.
    unsafe { arch_spin_unlock(lock) };
}

const _: () = assert!(core::mem::size_of::<core::sync::atomic::AtomicU32>() == 4);
