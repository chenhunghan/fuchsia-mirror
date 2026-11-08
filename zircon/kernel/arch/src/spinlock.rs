// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Architecture-independent interface to the architecture spinlock primitives, mirroring the
//! `arch_spin_*` functions declared in `<arch/spinlock.h>`.
//!
//! A spinlock is the `value` of a C++ `arch_spin_lock_t`, an `AtomicU32` holding the number of the
//! CPU holding it plus one, so 0 means unheld. The lock and unlock operations forward to the
//! per-architecture Rust implementations (which the C++ `arch_spin_*` functions also call through
//! FFI), giving them the same signatures and semantics on every architecture.
//!
//! Like all kernel code, these rely on the calling CPU's per-CPU pointer (`%gs.base` on x86, x20
//! on arm64 and s11 on riscv64) having been set up, which is not repeated in each `# Safety`
//! section.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::kernel::spin_tracing::EncodedLockId;
use crate::kernel::types::cpu_num_t;

#[cfg(target_arch = "aarch64")]
use super::arch_arm64::spinlock as arch_spinlock;
#[cfg(target_arch = "riscv64")]
use super::arch_riscv64::spinlock as arch_spinlock;
#[cfg(target_arch = "x86_64")]
use super::arch_x86::spinlock as arch_spinlock;

/// Acquires `lock` without lock trace instrumentation.
///
/// # Safety
///
/// Interrupts must be disabled on the calling CPU, and the calling CPU must not already hold
/// `lock`.
#[inline(always)]
pub unsafe fn arch_spin_lock_non_instrumented(lock: &AtomicU32) {
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spinlock::arch_spin_lock_non_instrumented(lock) };
}

/// Acquires `lock`, tracing the spin with `encoded_lock_id` if the lock is contended.
///
/// # Safety
///
/// Interrupts must be disabled on the calling CPU, and the calling CPU must not already hold
/// `lock`.
#[inline(always)]
pub unsafe fn arch_spin_lock_trace_instrumented(lock: &AtomicU32, encoded_lock_id: EncodedLockId) {
    // SAFETY: The caller has disabled interrupts.
    unsafe { arch_spinlock::arch_spin_lock_trace_instrumented(lock, encoded_lock_id) };
}

/// Tries to acquire `lock` without waiting for its holder.
///
/// Returns `false` if the lock was acquired, and `true` if it was not, matching the
/// `TA_TRY_ACQ(false, lock)` contract of the C++ `arch_spin_trylock()`.
///
/// This is not permitted to fail spuriously, even on architectures with weak memory ordering: if
/// it fails, the lock was actually observed to be held during the attempt.
///
/// # Safety
///
/// Interrupts must be disabled on the calling CPU.
#[inline(always)]
pub unsafe fn arch_spin_trylock(lock: &AtomicU32) -> bool {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        // SAFETY: The caller has disabled interrupts.
        unsafe { arch_spinlock::arch_spin_trylock(lock) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // The riscv64 implementation returns `true` if the lock was acquired.
        //
        // SAFETY: The caller has disabled interrupts.
        !unsafe { arch_spinlock::arch_spin_trylock(lock) }
    }
}

/// Releases `lock`.
///
/// Takes a raw pointer rather than a reference, matching the x86 implementation, which accesses
/// the lock word only through the pointer. A reference would also be sound, as every byte of an
/// `AtomicU32` is interior-mutable, but the caller must still keep `lock` live until this returns.
///
/// # Safety
///
/// `lock` must be properly aligned, point to an `AtomicU32` currently held by the calling CPU, and
/// remain live until `arch_spin_unlock` returns. Interrupts must be disabled on the calling CPU.
#[inline(always)]
pub unsafe fn arch_spin_unlock(lock: *const AtomicU32) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The caller guarantees that `lock` is properly aligned, remains live until this
        // returns, and is held by the calling CPU, and that interrupts are disabled.
        unsafe { arch_spinlock::arch_spin_unlock(lock) };
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: The caller guarantees that `lock` is properly aligned, remains live until this
        // returns, and is held by the calling CPU, and that interrupts are disabled. Every byte of
        // an `AtomicU32` is interior-mutable, so the reference makes no assertion that the lock is
        // unchanged once this call has released it.
        unsafe { arch_spinlock::arch_spin_unlock(&*lock) };
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: As for aarch64.
        unsafe { arch_spinlock::arch_spin_unlock(&*lock) };
    }
}

/// Returns which CPU currently holds `lock`, or `INVALID_CPU` if it is not held.
///
/// Note that this does not synchronize other memory operations (i.e. it has relaxed atomic
/// semantics).
#[inline(always)]
pub fn arch_spin_lock_holder_cpu(lock: &AtomicU32) -> cpu_num_t {
    lock.load(Ordering::Relaxed).wrapping_sub(1)
}

/// Returns `true` if the calling CPU holds `lock`.
///
/// In practice, this value is only useful if migration to another CPU is prevented, by disabling
/// preemption or interrupts, while the value is being used.
///
/// Note that this does not synchronize other memory operations (i.e. it has relaxed atomic
/// semantics).
#[inline(always)]
pub fn arch_spin_lock_held(lock: &AtomicU32) -> bool {
    arch_spin_lock_holder_cpu(lock) == super::curr_cpu_num()
}
