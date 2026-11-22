// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Interactive kernel test and benchmark console commands.

mod benchmarks;
mod brwlock_tests;
mod cache_tests;
mod clock_tests;
mod fibo;
mod mem_tests;
mod mmu_tests;
mod mp_hotplug_tests;
mod mutex_spin_time_tests;
mod owned_wait_queue_topology;
mod pi_tests;
mod sleep_tests;
mod spinner;
mod string_tests;
mod timer_tests;
mod uart_tests;

use core::marker::PhantomData;

use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
use crate::console::{CMD_AVAIL_NORMAL, static_command};
use crate::kernel::scheduler::{Scheduler, peek_is_active};
use crate::kernel_tests::benchmarks::benchmarks;
use crate::kernel_tests::clock_tests::clock_tests;
use crate::kernel_tests::fibo::fibo;
use crate::kernel_tests::sleep_tests::sleep_tests;
use crate::kernel_tests::spinner::spinner;
use crate::kernel_tests::timer_tests::{timer_diag, timer_stress};
use crate::kernel_tests::uart_tests::uart_tests;

/// Passes `ptr` through an empty register-only inline assembly constraint so LLVM cannot
/// dead-store-eliminate repeated `memset`/`memcpy` calls or merge adjacent writes across calls,
/// without spilling `ptr` to the stack or SafeStack.
///
/// # Safety-usable invariant
///
/// Returns `ptr` unchanged in both address and provenance, without reading or modifying any
/// memory.
#[inline(always)]
fn opaque_ptr<T>(mut ptr: *mut T) -> *mut T {
    // SAFETY: The template is an empty assembly comment emitting zero instructions, so it touches
    // no stack or red zone (`nostack`), modifies no CPU flags (`preserves_flags`), performs no
    // memory writes (`readonly`), and passes `ptr` through `inout(reg)` unchanged in value and
    // provenance. Using `readonly` without `nomem` prevents LLVM from dead-store-eliminating or
    // merging preceding `memset`/`memcpy` writes across this call.
    unsafe {
        core::arch::asm!(
            "/* {0} */",
            inout(reg) ptr,
            options(readonly, nostack, preserves_flags),
        );
    }
    ptr
}

/// Const-pointer counterpart to [`opaque_ptr`].
///
/// # Safety-usable invariant
///
/// Returns `ptr` unchanged in both address and provenance, without reading or modifying any
/// memory.
#[inline(always)]
fn opaque_const_ptr<T>(ptr: *const T) -> *const T {
    opaque_ptr(ptr.cast_mut()).cast_const()
}

/// Disables interrupts and marks the calling CPU as inactive for the lifetime of the object.
///
/// The purpose of this guard is to "isolate" the calling CPU from the rest of the system to ensure
/// benchmarks are not impacted by interrupts and ensure the lockup detector does not think the
/// calling CPU has become unresponsive (the lockup detector only monitors active CPUs).
struct InactiveCpuGuard {
    _interrupt_guard: InterruptDisableGuard,
    was_active: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

impl InactiveCpuGuard {
    /// # Safety
    ///
    /// - The calling CPU's [`Scheduler`] must be initialized.
    /// - The calling CPU's scheduler `queue_lock_` must not be held when `new` is called or when
    ///   the returned guard is dropped.
    /// - Interrupts must not be re-enabled on the calling CPU while the returned guard is alive,
    ///   so the calling thread remains pinned to the same CPU until `Drop` restores its active
    ///   state.
    unsafe fn new() -> Self {
        let interrupt_guard = InterruptDisableGuard::new();
        let was_active = peek_is_active(curr_cpu_num());
        if was_active {
            // SAFETY: Interrupts were just disabled by `interrupt_guard`, and the caller
            // guarantees the current CPU's `Scheduler` is initialized and its `queue_lock_` is
            // not held.
            unsafe { Scheduler::set_curr_cpu_active(false) };
        }
        Self { _interrupt_guard: interrupt_guard, was_active, _not_send_sync: PhantomData }
    }
}

impl Drop for InactiveCpuGuard {
    fn drop(&mut self) {
        if self.was_active {
            // SAFETY: `Drop::drop` runs before `_interrupt_guard` is dropped and
            // `InactiveCpuGuard` is `!Send`, so we are still on the same initialized CPU with
            // interrupts disabled until after the active bit is restored, and `new`'s caller
            // guaranteed `queue_lock_` is not held at drop time.
            unsafe { Scheduler::set_curr_cpu_active(self.was_active) };
        }
    }
}

static_command!(
    CLOCK_TESTS_CMD,
    c"clock_tests".as_ptr(),
    c"test clocks".as_ptr(),
    clock_tests,
    CMD_AVAIL_NORMAL
);
static_command!(
    SLEEP_TESTS_CMD,
    c"sleep_tests".as_ptr(),
    c"tests sleep".as_ptr(),
    sleep_tests,
    CMD_AVAIL_NORMAL
);
static_command!(
    BENCH_CMD,
    c"bench".as_ptr(),
    c"miscellaneous benchmarks".as_ptr(),
    benchmarks,
    CMD_AVAIL_NORMAL
);
static_command!(FIBO_CMD, c"fibo".as_ptr(), c"threaded fibonacci".as_ptr(), fibo, CMD_AVAIL_NORMAL);
static_command!(
    SPINNER_CMD,
    c"spinner".as_ptr(),
    c"create a spinning thread".as_ptr(),
    spinner,
    CMD_AVAIL_NORMAL
);
static_command!(
    TIMER_DIAG_CMD,
    c"timer_diag".as_ptr(),
    c"prints timer diagnostics".as_ptr(),
    timer_diag,
    CMD_AVAIL_NORMAL
);
static_command!(
    TIMER_STRESS_CMD,
    c"timer_stress".as_ptr(),
    c"runs a timer stress test".as_ptr(),
    timer_stress,
    CMD_AVAIL_NORMAL
);
static_command!(
    UART_TESTS_CMD,
    c"uart_tests".as_ptr(),
    c"tests uart Tx".as_ptr(),
    uart_tests,
    CMD_AVAIL_NORMAL
);
