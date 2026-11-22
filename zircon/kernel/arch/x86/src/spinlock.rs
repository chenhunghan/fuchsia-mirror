// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86_64 spinlock implementation.
//!
//! Stores the current CPU number + 1 so that 0 represents an unheld lock.
//!
//! The `arch_spin_*` functions below are the architecture half of `SpinLockBase` in
//! `crate::kernel::spinlock`, which holds the lock word and calls them with interrupts disabled.

// TODO(https://fxbug.dev/581987928): Remove `#![coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#![coverage(off)]

use core::sync::atomic::{AtomicU32, Ordering};

use super::mp::PERCPU_NUM_SPINLOCKS_OFFSET;
use super::x86::x86_add_gs_offset32;
use crate::kernel::spin_tracing::{EncodedLockId, FinishType, Tracer};
use crate::kernel::types::cpu_num_t;

#[inline(always)]
fn arch_spin_lock_core(lock: &AtomicU32, val: u32) {
    if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_core_contended(lock, val);
    }
}

/// Contended spin loop of [`arch_spin_lock_core`], outlined so the uncontended fast path falls
/// straight through without executing `pause` (`libarch::r#yield()`) or loop branches.
#[cold]
#[inline(never)]
fn arch_spin_lock_core_contended(lock: &AtomicU32, val: u32) {
    loop {
        loop {
            libarch::r#yield();
            if lock.load(Ordering::Relaxed) == 0 {
                break;
            }
        }
        if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
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
    let val = cpu.wrapping_add(1);
    arch_spin_lock_core(lock, val);
    // SAFETY: `%gs.base` points at the calling CPU's `x86_percpu` from before any C++ or Rust code
    // runs on that CPU (see `super::mp::arch_curr_cpu_num`), and the caller has disabled
    // interrupts.
    unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
}

/// Acquires `lock` with lock trace instrumentation.
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

    // If this lock acquisition is trace instrumented, try to obtain the lock once
    // before we decide that we need to spin and produce spin trace events.
    if lock.compare_exchange_weak(0, val, Ordering::Acquire, Ordering::Relaxed).is_err() {
        arch_spin_lock_trace_instrumented_contended(lock, val, encoded_lock_id);
    }

    // SAFETY: `%gs.base` points at the calling CPU's `x86_percpu` from before any C++ or Rust code
    // runs on that CPU (see `super::mp::arch_curr_cpu_num`), and the caller has disabled
    // interrupts.
    unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
}

/// Contended spin and trace path of [`arch_spin_lock_trace_instrumented`], outlined so the
/// uncontended fast path needs no stack frame or callee-saved register spills for [`Tracer`].
#[cold]
#[inline(never)]
fn arch_spin_lock_trace_instrumented_contended(
    lock: &AtomicU32,
    val: u32,
    encoded_lock_id: EncodedLockId,
) {
    let spin_tracer = Tracer::<true>::new();
    arch_spin_lock_core_contended(lock, val);
    spin_tracer.finish(FinishType::LockAcquired, encoded_lock_id);
}

/// Attempts to acquire `lock` without spinning.
///
/// Returns `false` if the lock was acquired, or `true` if it was already held.
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

    let failed = lock.compare_exchange(0, val, Ordering::Acquire, Ordering::Relaxed).is_err();
    if !failed {
        // SAFETY: `%gs.base` points at the calling CPU's `x86_percpu` from before any C++ or Rust
        // code runs on that CPU (see `super::mp::arch_curr_cpu_num`), and the caller has disabled
        // interrupts.
        unsafe { x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, 1>() };
    }
    failed
}

/// Releases a previously held `lock`.
///
/// Takes a raw pointer `*const AtomicU32` rather than `&AtomicU32` because releasing the lock
/// allows another CPU to immediately acquire it and free the containing object before this
/// function returns.
///
/// # Safety
///
/// `lock` points to a live, properly aligned `AtomicU32` that the calling CPU holds, and
/// interrupts are disabled on the calling CPU.
#[inline(always)]
pub unsafe fn arch_spin_unlock(lock: *const AtomicU32) {
    // SAFETY: `%gs.base` points at the calling CPU's `x86_percpu` from before any C++ or Rust code
    // runs on that CPU (see `super::mp::arch_curr_cpu_num`), and the caller guarantees interrupts
    // are disabled and `lock` is live, 4-byte aligned, and held by this CPU until the release
    // store below. Under x86_64 TSO an aligned 32-bit `mov` has release ordering (with `asm!`
    // acting as a compiler barrier), avoiding `&AtomicU32` across the release store and avoiding
    // `ub_checks` branches on the raw pointer in debug-assertion builds.
    unsafe {
        x86_add_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET, -1>();
        core::arch::asm!(
            "mov dword ptr [{lock}], 0",
            lock = in(reg) lock,
            options(nostack, preserves_flags),
        );
    }
}

zr::static_assert!(core::mem::size_of::<AtomicU32>() == 4);
zr::static_assert!(core::mem::align_of::<AtomicU32>() == 4);
