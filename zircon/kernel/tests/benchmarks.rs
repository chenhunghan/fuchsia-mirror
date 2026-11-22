// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel microbenchmarks invoked by the `bench` console command.

use core::alloc::Layout;
use core::cell::Cell;
use core::ffi::{c_int, c_void};
use core::mem::{align_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{Ordering, compiler_fence};

use affine::Round;
use debug::tracef;
use kprint::kprintln;
use libc::string::{memcpy, memset};

use super::{InactiveCpuGuard, opaque_const_ptr, opaque_ptr};
use crate::arch_rs::InterruptSavedState;
use crate::arch_rs::ops::zero_page;
use crate::console::CmdArgs;
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
use crate::kernel::brwlock::{BrwLock, BrwLockEnablePi, BrwLockNoPi, BrwLockPi};
use crate::kernel::mutex::Mutex;
use crate::kernel::spinlock::{MonitoredSpinLock, SpinLock};
use crate::kernel::types::VAddr;
use crate::pdev_watchdog::{hw_watchdog_is_enabled, hw_watchdog_present, hw_watchdog_set_enabled};
use crate::platform_rs::timer::{timer_current_mono_ticks, timer_get_ticks_to_time_ratio, zx_sec};

/// Must be smaller than max allowed heap allocation.
const BUFSIZE: usize = 512 * 1024;
/// Enough iterations to have to copy/set 1GB of memory.
const ITER: usize = (1024 * 1024 * 1024) / BUFSIZE;

const KB: usize = 1024;
const MB: usize = 1024 * KB;

#[repr(C, align(4096))]
struct AlignedPage([u8; page::SIZE]);

zr::static_assert!(size_of::<AlignedPage>() == page::SIZE);
zr::static_assert!(align_of::<AlignedPage>() == page::SIZE);

/// Reads the architectural cycle counter with compiler fences on both sides so LLVM cannot reorder
/// measured memory or allocator operations across benchmark timing boundaries (`libarch::cycles()`
/// uses `options(nomem)` / `IntrNoMem`).
#[inline(always)]
fn cycles_fenced() -> u64 {
    compiler_fence(Ordering::SeqCst);
    let c = libarch::cycles();
    compiler_fence(Ordering::SeqCst);
    c
}

#[inline(never)]
fn bench_cycles_per_second() {
    {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and the body below does not re-enable interrupts before `_inactive_cpu_guard`
        // drops.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };
        let before_ticks = timer_current_mono_ticks();
        let before_cycles = cycles_fenced();
        for _ in 0..100_000_000usize {
            // SAFETY: Empty inline assembly has no side effects and prevents loop removal.
            unsafe { core::arch::asm!("") };
        }
        let after_ticks = timer_current_mono_ticks();
        let after_cycles = cycles_fenced();
        let delta_time = timer_get_ticks_to_time_ratio()
            .scale::<{ Round::DOWN }>(after_ticks.0.wrapping_sub(before_ticks.0));
        let delta_cycles = after_cycles.wrapping_sub(before_cycles);
        kprintln!(
            "{:u} cycles per second ({:u} cycles in {:d} ns)",
            delta_cycles.wrapping_mul(zx_sec(1) as u64).checked_div(delta_time as u64).unwrap_or(0),
            delta_cycles,
            delta_time,
        );
    }
}

#[inline(never)]
fn bench_set_overhead() {
    let Ok(buf) = kalloc::Box::<[u32]>::try_new_uninit_slice(BUFSIZE / size_of::<u32>()) else {
        tracef!("error: malloc failed\n");
        return;
    };

    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and the loop below does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            // SAFETY: Empty inline assembly has no side effects and prevents loop removal.
            unsafe { core::arch::asm!("") };
        }
        cycles_fenced().wrapping_sub(start)
    };

    kprintln!("took {:u} cycles overhead to loop {:u} times", count, ITER);

    drop(buf);
}

#[inline(never)]
fn bench_memset() {
    let Ok(mut buf) = kalloc::Box::<[AlignedPage]>::try_new_uninit_slice(BUFSIZE / page::SIZE)
    else {
        tracef!("error: memalign failed\n");
        return;
    };

    let base = buf.as_mut_ptr().cast::<c_void>();
    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `memset` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            // SAFETY: `base` points to a uniquely owned, `page::SIZE`-aligned allocation of
            // `BUFSIZE` bytes valid for writes for the duration of the loop. `opaque_ptr` prevents
            // LLVM from collapsing repeated `memset` calls across iterations without spilling to
            // the stack.
            unsafe {
                memset(opaque_ptr(base), 0, BUFSIZE);
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = ((BUFSIZE * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to memset a buffer of size {:u} {:u} times ",
            "({:u} bytes), {:u}.{:03u} bytes/cycle",
        ),
        count,
        BUFSIZE,
        ITER,
        BUFSIZE * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

#[inline(never)]
fn bench_memset_per_page() {
    let Ok(mut buf) = kalloc::Box::<[AlignedPage]>::try_new_uninit_slice(BUFSIZE / page::SIZE)
    else {
        tracef!("error: memalign failed\n");
        return;
    };

    let base = buf.as_mut_ptr().cast::<c_void>();
    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `memset` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            for j in (0..BUFSIZE).step_by(page::SIZE) {
                // SAFETY: `j + page::SIZE <= BUFSIZE`, so `base.wrapping_byte_add(j)` is in-bounds
                // of the uniquely owned `BUFSIZE`-byte allocation and valid for writes of
                // `page::SIZE` bytes. `opaque_ptr` prevents LLVM from merging adjacent per-page
                // `memset` calls into a single `BUFSIZE` `memset` without spilling to the stack.
                unsafe {
                    memset(opaque_ptr(base.wrapping_byte_add(j)), 0, page::SIZE);
                }
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = ((BUFSIZE * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to per-page memset a buffer of size {:u} {:u} times ",
            "({:u} bytes), {:u}.{:03u} bytes/cycle",
        ),
        count,
        BUFSIZE,
        ITER,
        BUFSIZE * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

#[inline(never)]
fn bench_zero_page() {
    let Ok(mut buf) = kalloc::Box::<[AlignedPage]>::try_new_uninit_slice(BUFSIZE / page::SIZE)
    else {
        tracef!("error: memalign failed\n");
        return;
    };

    let base_addr = buf.as_mut_ptr().expose_provenance();
    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `zero_page` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            for j in (0..BUFSIZE).step_by(page::SIZE) {
                // SAFETY: `base_addr` comes from `expose_provenance()` on the `page::SIZE`-aligned
                // `BUFSIZE`-byte heap allocation `buf` (so `zero_page`'s
                // `with_exposed_provenance_mut` reconstructs valid write provenance), and
                // `[base_addr + j, base_addr + j + page::SIZE)` is page-aligned and in-bounds.
                unsafe { zero_page(VAddr(base_addr.wrapping_add(j))) };
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = ((BUFSIZE * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to arch_zero_page a buffer of size {:u} {:u} times ",
            "({:u} bytes), {:u}.{:03u} bytes/cycle",
        ),
        count,
        BUFSIZE,
        ITER,
        BUFSIZE * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

trait ZeroWord: Copy {
    const ZERO: Self;
}

impl ZeroWord for u8 {
    const ZERO: Self = 0;
}
impl ZeroWord for u16 {
    const ZERO: Self = 0;
}
impl ZeroWord for u32 {
    const ZERO: Self = 0;
}
impl ZeroWord for u64 {
    const ZERO: Self = 0;
}

#[inline(never)]
fn bench_cset<T: ZeroWord>() {
    let Ok(mut buf) = kalloc::Box::<[T]>::try_new_uninit_slice(BUFSIZE / size_of::<T>()) else {
        tracef!("error: malloc failed\n");
        return;
    };

    let base = buf.as_mut_ptr().cast::<T>();
    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `write_volatile` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            for j in 0..(BUFSIZE / size_of::<T>()) {
                // SAFETY: `base` is aligned for `T` and valid for `BUFSIZE / size_of::<T>()`
                // elements, and `j` is in-bounds. `write_volatile` preserves the exact `T`-sized
                // store width without LLVM collapsing the loop into `memset` or dead-store
                // eliminating iterations.
                unsafe { base.wrapping_add(j).write_volatile(T::ZERO) };
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = ((BUFSIZE * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to clear a buffer using wordsize {:u} of size {:u} {:u} times ",
            "({:u} bytes), {:u}.{:03u} bytes/cycle",
        ),
        count,
        size_of::<T>(),
        BUFSIZE,
        ITER,
        BUFSIZE * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

#[inline(never)]
fn bench_cset_wide() {
    let Ok(mut buf) = kalloc::Box::<[u64]>::try_new_uninit_slice(BUFSIZE / size_of::<u64>()) else {
        tracef!("error: malloc failed\n");
        return;
    };

    let base = buf.as_mut_ptr().cast::<u64>();
    let count = {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `write_volatile` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            for j in 0..(BUFSIZE / size_of::<u64>() / 8) {
                // SAFETY: `j < BUFSIZE / size_of::<u64>() / 8`, so `j * 8 + 7 < buf.len()`, and
                // `base` is aligned for `u64` and uniquely owned. `write_volatile` preserves the
                // eight 64-bit stores per iteration without collapsing into `memset`.
                unsafe {
                    base.wrapping_add(j * 8).write_volatile(0);
                    base.wrapping_add(j * 8 + 1).write_volatile(0);
                    base.wrapping_add(j * 8 + 2).write_volatile(0);
                    base.wrapping_add(j * 8 + 3).write_volatile(0);
                    base.wrapping_add(j * 8 + 4).write_volatile(0);
                    base.wrapping_add(j * 8 + 5).write_volatile(0);
                    base.wrapping_add(j * 8 + 6).write_volatile(0);
                    base.wrapping_add(j * 8 + 7).write_volatile(0);
                }
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = ((BUFSIZE * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to clear a buffer 64 bytes at a time of size {:u} {:u} times ",
            "({:u} bytes), {:u}.{:03u} bytes/cycle",
        ),
        count,
        BUFSIZE,
        ITER,
        BUFSIZE * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

#[inline(never)]
fn bench_memcpy() {
    let Ok(mut buf) = kalloc::Box::<[u8]>::try_new_zeroed_slice(BUFSIZE) else {
        tracef!("error: calloc failed\n");
        return;
    };

    let count = {
        let (dst, src) = buf.split_at_mut(BUFSIZE / 2);
        let dst_ptr = dst.as_mut_ptr().cast::<c_void>();
        let src_ptr = src.as_ptr().cast::<c_void>();

        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `memcpy` does not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        let start = cycles_fenced();
        for _ in 0..ITER {
            // SAFETY: `split_at_mut(BUFSIZE / 2)` yields two disjoint initialized halves of length
            // `BUFSIZE / 2` with independent sibling provenances, so `src_ptr` is valid for reads,
            // `dst_ptr` is valid for writes, and the two `BUFSIZE / 2`-byte regions do not
            // overlap. `opaque_ptr` and `opaque_const_ptr` prevent LLVM from folding repeated
            // `memcpy` calls across iterations without spilling to the stack.
            unsafe {
                memcpy(opaque_ptr(dst_ptr), opaque_const_ptr(src_ptr), BUFSIZE / 2);
            }
        }
        cycles_fenced().wrapping_sub(start)
    };

    let bytes_cycle = (((BUFSIZE / 2) * ITER) as u64 * 1000).checked_div(count).unwrap_or(0);
    kprintln!(
        concat!(
            "took {:u} cycles to memcpy a buffer of size {:u} {:u} times ",
            "({:u} source bytes), {:u}.{:03u} source bytes/cycle",
        ),
        count,
        BUFSIZE / 2,
        ITER,
        (BUFSIZE / 2) * ITER,
        bytes_cycle / 1000,
        bytes_cycle % 1000,
    );

    drop(buf);
}

trait BenchmarkSpinLock: Sized {
    fn new() -> Self;

    /// # Safety
    ///
    /// Interrupts must already be disabled on the calling CPU, the calling CPU must not already
    /// hold `self`, and this call must be paired with [`Self::release`] on the same CPU before
    /// interrupts are re-enabled or the thread blocks or migrates.
    unsafe fn acquire(&self, tag: ksync::SourceTag);

    /// # Safety
    ///
    /// Interrupts must be disabled, `self` must currently be held by the calling CPU without
    /// outstanding references protected by this acquisition, and `self` must not guard the
    /// destruction of its own backing allocation.
    unsafe fn release(&self);

    /// # Safety
    ///
    /// The calling CPU must not already hold `self`, and the returned [`InterruptSavedState`] must
    /// be passed to [`Self::release_irq_restore`] on the same CPU before the thread blocks or
    /// migrates.
    unsafe fn acquire_irq_save(&self, tag: ksync::SourceTag) -> InterruptSavedState;

    /// # Safety
    ///
    /// `self` must currently be held by the calling CPU without outstanding references protected by
    /// this acquisition, `state` must be the [`InterruptSavedState`] returned by the corresponding
    /// [`Self::acquire_irq_save`] call on this CPU, and `self` must not guard the destruction of
    /// its own backing allocation.
    unsafe fn release_irq_restore(&self, state: InterruptSavedState);
}

impl BenchmarkSpinLock for SpinLock {
    #[inline]
    fn new() -> Self {
        Self::new()
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::acquire`].
    #[inline]
    unsafe fn acquire(&self, _tag: ksync::SourceTag) {
        // SAFETY: By `BenchmarkSpinLock::acquire`'s contract, interrupts are disabled on the
        // calling CPU, `self` is not already held by this CPU, and this acquisition will be paired
        // with `release` on the same CPU before interrupts are re-enabled or the thread blocks or
        // migrates.
        unsafe { Self::acquire(self) };
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::release`].
    #[inline]
    unsafe fn release(&self) {
        // SAFETY: By `BenchmarkSpinLock::release`'s contract, interrupts are disabled, `self` is
        // held by the calling CPU with no outstanding guarded references, and `self` does not guard
        // the destruction of its own backing allocation (which `&self` also keeps live for the
        // duration of this call).
        unsafe { Self::release(self) };
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::acquire_irq_save`].
    #[inline]
    unsafe fn acquire_irq_save(&self, _tag: ksync::SourceTag) -> InterruptSavedState {
        // SAFETY: By `BenchmarkSpinLock::acquire_irq_save`'s contract, `self` is not already held
        // by the calling CPU, and the returned `InterruptSavedState` will be passed to
        // `release_irq_restore` on the same CPU before the thread blocks or migrates.
        unsafe { Self::acquire_irq_save(self) }
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::release_irq_restore`].
    #[inline]
    unsafe fn release_irq_restore(&self, state: InterruptSavedState) {
        // SAFETY: By `BenchmarkSpinLock::release_irq_restore`'s contract, `self` is held by the
        // calling CPU with no outstanding guarded references, `state` is the matching
        // `InterruptSavedState` from `acquire_irq_save` on this CPU, and `self` does not guard the
        // destruction of its own backing allocation (which `&self` also keeps live for the duration
        // of this call).
        unsafe { Self::release_irq_restore(self, state) };
    }
}

impl BenchmarkSpinLock for MonitoredSpinLock {
    #[inline]
    fn new() -> Self {
        Self::new()
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::acquire`].
    #[inline]
    unsafe fn acquire(&self, tag: ksync::SourceTag) {
        // SAFETY: By `BenchmarkSpinLock::acquire`'s contract, interrupts are disabled on the
        // calling CPU, `self` is not already held by this CPU, and this acquisition will be paired
        // with `release` on the same CPU before interrupts are re-enabled or the thread blocks or
        // migrates.
        unsafe { Self::acquire(self, tag) };
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::release`].
    #[inline]
    unsafe fn release(&self) {
        // SAFETY: By `BenchmarkSpinLock::release`'s contract, interrupts are disabled, `self` is
        // held by the calling CPU with no outstanding guarded references, and `self` does not guard
        // the destruction of its own backing allocation (which `&self` also keeps live for the
        // duration of this call).
        unsafe { Self::release(self) };
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::acquire_irq_save`].
    #[inline]
    unsafe fn acquire_irq_save(&self, tag: ksync::SourceTag) -> InterruptSavedState {
        // SAFETY: By `BenchmarkSpinLock::acquire_irq_save`'s contract, `self` is not already held
        // by the calling CPU, and the returned `InterruptSavedState` will be passed to
        // `release_irq_restore` on the same CPU before the thread blocks or migrates.
        unsafe { Self::acquire_irq_save(self, tag) }
    }

    /// # Safety
    ///
    /// See [`BenchmarkSpinLock::release_irq_restore`].
    #[inline]
    unsafe fn release_irq_restore(&self, state: InterruptSavedState) {
        // SAFETY: By `BenchmarkSpinLock::release_irq_restore`'s contract, `self` is held by the
        // calling CPU with no outstanding guarded references, `state` is the matching
        // `InterruptSavedState` from `acquire_irq_save` on this CPU, and `self` does not guard the
        // destruction of its own backing allocation (which `&self` also keeps live for the duration
        // of this call).
        unsafe { Self::release_irq_restore(self, state) };
    }
}

#[inline(never)]
fn bench_spinlock<L: BenchmarkSpinLock>(spin_lock_name: &str) {
    let lock = L::new();
    let mut c: u64;

    const COUNT: usize = 128 * 1024 * 1024;
    // test 1: acquire/release a spinlock with interrupts already disabled
    {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `acquire` / `release` do not re-enable interrupts.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        c = cycles_fenced();
        for _ in 0..COUNT {
            // SAFETY: Interrupts are disabled by `_inactive_cpu_guard`, `lock` is a stack local
            // that guards no references or allocation destruction, is unheld at the start of each
            // iteration, and `release` unlocks it immediately on the same CPU.
            unsafe {
                lock.acquire(ksync::source_tag!());
                lock.release();
            }
        }
        c = cycles_fenced().wrapping_sub(c);
    }

    kprintln!(
        "{:u} cycles to acquire/release {:s} {:u} times ({:u} cycles per)",
        c,
        spin_lock_name,
        COUNT,
        c / COUNT as u64,
    );

    // test 2: acquire/release a spinlock with irq save and irqs already disabled
    {
        // SAFETY: Benchmarks run in normal thread context after scheduler init with `queue_lock_`
        // unheld, and `acquire_irq_save` / `release_irq_restore` preserve the disabled interrupt
        // state captured by `_inactive_cpu_guard`.
        let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };

        c = cycles_fenced();
        for _ in 0..COUNT {
            // SAFETY: `lock` is a stack local that guards no references or allocation destruction,
            // is unheld at the start of each iteration, and `state` is immediately restored on the
            // same CPU by `release_irq_restore` (keeping interrupts disabled).
            unsafe {
                let state = lock.acquire_irq_save(ksync::source_tag!());
                lock.release_irq_restore(state);
            }
        }
        c = cycles_fenced().wrapping_sub(c);
    }

    kprintln!(
        concat!(
            "{:u} cycles to acquire/release {:s} w/irqsave (already disabled) {:u} times ",
            "({:u} cycles per)",
        ),
        c,
        spin_lock_name,
        COUNT,
        c / COUNT as u64,
    );

    // test 2: acquire/release a spinlock with irq save and irqs enabled
    c = cycles_fenced();
    for _ in 0..COUNT {
        // SAFETY: `lock` is a stack local that guards no references or allocation destruction, is
        // unheld at the start of each iteration, and `state` is immediately passed to
        // `release_irq_restore` on the same CPU without blocking or migrating.
        unsafe {
            let state = lock.acquire_irq_save(ksync::source_tag!());
            lock.release_irq_restore(state);
        }
    }
    c = cycles_fenced().wrapping_sub(c);

    kprintln!(
        "{:u} cycles to acquire/release {:s} w/irqsave {:u} times ({:u} cycles per)",
        c,
        spin_lock_name,
        COUNT,
        c / COUNT as u64,
    );
}

#[inline(never)]
fn bench_mutex() {
    let m = Mutex::new();

    const COUNT: u32 = 128 * 1024 * 1024;
    let mut c = cycles_fenced();
    for _ in 0..COUNT {
        // SAFETY: `benchmarks` runs in blockable normal thread context (`AutoPreemptDisabler` only
        // defers preemption, not blocking) with no spinlocks held and no `ChainLockTransaction`
        // active. `m` is unheld before `acquire` and held by the current thread for `release`.
        unsafe {
            m.acquire(Mutex::SPIN_MAX_DURATION);
            Mutex::release(NonNull::from(&m));
        }
    }
    c = cycles_fenced().wrapping_sub(c);

    kprintln!(
        "{:u} cycles to acquire/release uncontended mutex {:u} times ({:u} cycles per)",
        c,
        COUNT,
        c / u64::from(COUNT),
    );
}

/// A `BrwLock` variant benchmarked by [`bench_rwlock`].
trait BenchmarkRwLock {
    /// Whether the lock respects priority inheritance.
    type EnablePi: BrwLockEnablePi;
}

impl<P: BrwLockEnablePi> BenchmarkRwLock for BrwLock<P> {
    type EnablePi = P;
}

/// Benchmarks uncontended read and write acquisition and release of a `LockType`.
///
/// # Safety
///
/// Must be called from blockable kernel thread context with interrupts enabled, no spinlocks held,
/// and no `ChainLockTransaction` active.
#[inline(never)]
unsafe fn bench_rwlock<LockType: BenchmarkRwLock>() {
    pin_init::stack_pin_init!(let rw = BrwLock::<LockType::EnablePi>::init());
    const COUNT: u32 = 128 * 1024 * 1024;
    let is_pi = i32::from(LockType::EnablePi::ENABLED);
    let mut c = cycles_fenced();
    for _ in 0..COUNT {
        // SAFETY: Forwarded from the caller (`AutoPreemptDisabler` in `benchmarks` only defers
        // preemption, and readers of a `BrwLockPi` do not block), and `rw` is unheld before
        // `read_acquire` and held for read by the current thread for `read_release`.
        unsafe {
            rw.read_acquire();
            rw.read_release();
        }
    }
    c = cycles_fenced().wrapping_sub(c);

    kprintln!(
        "{:u} cycles to acquire/release uncontended brwlock(PI: {:d}) for read {:u} times ({:u} \
         cycles per)",
        c,
        is_pi,
        COUNT,
        c / u64::from(COUNT),
    );

    c = cycles_fenced();
    for _ in 0..COUNT {
        // SAFETY: As above, with `rw` unheld before `write_acquire` and held for write by the
        // current thread for `write_release`.
        unsafe {
            rw.write_acquire();
            rw.write_release();
        }
    }
    c = cycles_fenced().wrapping_sub(c);

    kprintln!(
        "{:u} cycles to acquire/release uncontended brwlock(PI: {:d}) for write {:u} times ({:u} \
         cycles per)",
        c,
        is_pi,
        COUNT,
        c / u64::from(COUNT),
    );
}

#[inline(never)]
fn bench_heap() {
    const HEAP_TO_USE: usize = 256 * MB;
    const ALLOC_SIZES: [usize; 3] = [256, KB, 2 * KB];

    for alloc_size in ALLOC_SIZES {
        let num_allocs = HEAP_TO_USE / alloc_size;
        assert!(alloc_size >= size_of::<Option<NonNull<u8>>>());
        let layout = Layout::from_size_align(alloc_size, align_of::<Option<NonNull<u8>>>())
            .expect("valid benchmark allocation layout");

        let before_alloc = cycles_fenced();
        let after_alloc: u64;

        {
            let alloc_chain: Cell<Option<NonNull<u8>>> = Cell::new(None);
            let _cleanup = zr::defer(|| {
                while let Some(curr) = alloc_chain.get() {
                    // SAFETY: Every node stored in `alloc_chain` was allocated below with `layout`
                    // and initialized with the previous `Option<NonNull<u8>>` in its first word.
                    let next_alloc = unsafe { curr.cast::<Option<NonNull<u8>>>().read() };
                    // SAFETY: `curr` is a live allocation of `layout` from `kalloc::alloc` and is
                    // not accessed again after `next_alloc` was read above.
                    unsafe { kalloc::dealloc(curr.as_ptr(), layout) };
                    alloc_chain.set(next_alloc);
                }
            });

            for _ in 0..num_allocs {
                // SAFETY: `layout.size() >= size_of::<Option<NonNull<u8>>>() > 0`.
                let Some(next_alloc) = (unsafe { kalloc::alloc(layout) }) else {
                    kprintln!("Allocation failed during {:s}", "bench_heap");
                    return;
                };
                // SAFETY: `next_alloc` is a fresh, uniquely owned allocation of `layout`, whose
                // size and alignment are at least those of `Option<NonNull<u8>>`.
                unsafe { next_alloc.cast::<Option<NonNull<u8>>>().write(alloc_chain.get()) };
                alloc_chain.set(Some(next_alloc));
            }
            after_alloc = cycles_fenced();
            // End the block to trigger cleanup and free.
        }
        let after_free = cycles_fenced();

        kprintln!(
            concat!(
                "Heap test using {:u} allocations of {:u} bytes took {:u} ",
                "cycles to allocate and {:u} cycles to free",
            ),
            num_allocs,
            alloc_size,
            after_alloc.wrapping_sub(before_alloc),
            after_free.wrapping_sub(after_alloc),
        );
    }
}

/// Console command entry point (`bench`) running kernel microbenchmarks.
///
/// # Safety
///
/// Must be called from normal, blockable kernel thread context after early boot with interrupts
/// enabled, no spinlocks held, and no `ChainLockTransaction` active (as guaranteed by
/// `CMD_AVAIL_NORMAL` console dispatch). `_argv` is not dereferenced.
pub(super) unsafe extern "C" fn benchmarks(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    // Disable the hardware watchdog (if present and enabled) because some of these benchmarks will
    // disable interrupts for extended periods of time.
    let mut need_to_reenable = false;
    if hw_watchdog_present() && hw_watchdog_is_enabled() {
        let _ = hw_watchdog_set_enabled(false);
        need_to_reenable = true;
    }
    let _reenable_hw_watchdog = zr::defer(move || {
        if need_to_reenable {
            let _ = hw_watchdog_set_enabled(true);
        }
    });

    // Ensure that benchmarks aren't impacted by preemption.
    let _preempt_disabler = AutoPreemptDisabler::new();

    bench_cycles_per_second();
    bench_set_overhead();
    bench_heap();
    bench_memcpy();
    bench_memset();

    bench_memset_per_page();
    bench_zero_page();

    bench_cset::<u8>();
    bench_cset::<u16>();
    bench_cset::<u32>();
    bench_cset::<u64>();
    bench_cset_wide();
    bench_spinlock::<SpinLock>("SpinLock");
    bench_spinlock::<MonitoredSpinLock>("MonitoredSpinLock");
    bench_mutex();
    // SAFETY: `benchmarks` runs in blockable kernel thread context with interrupts enabled, no
    // spinlocks held, and no `ChainLockTransaction` active.
    unsafe {
        bench_rwlock::<BrwLockPi>();
        bench_rwlock::<BrwLockNoPi>();
    }

    0
}
