// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/timer/timer.cc
//
// Platform Device (pdev) Timer Driver Interface.
//
// Systemwide timer interface, for platforms or architectures that utilize the
// abstraction. These correspond (currently) to calls defined in
// platform/timer.h.

use core::sync::atomic::Ordering;
use zr::AtomicConstPtr;
use zx_status::Status;
use zx_types::zx_ticks_t;

/// Hooks that a pdev timer driver must implement.
/// These correspond directly to the timer interface in dev/timer.h.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct PdevTimerOps {
    /// Read the current ticks of the hardware timer, at the rate the timer is ticking.
    /// Generally converted to real time in current_mono_time().
    pub current_ticks: Option<extern "C" fn() -> zx_ticks_t>,
    /// Set a timer to fire at the deadline specified that calls timer_tick().
    pub set_oneshot_timer: Option<extern "C" fn(deadline: zx_ticks_t) -> Result<(), Status>>,
    /// Cancel a pending oneshot timer. Okay to call if no pending oneshot.
    pub stop: Option<extern "C" fn() -> Result<(), Status>>,
    /// Stop the timer hardware. Stop should be called before shutdown.
    pub shutdown: Option<extern "C" fn() -> Result<(), Status>>,
}

zr::static_assert!(core::mem::size_of::<PdevTimerOps>() == 32);
zr::static_assert!(core::mem::align_of::<PdevTimerOps>() == 8);

static DEFAULT_OPS: PdevTimerOps =
    PdevTimerOps { current_ticks: None, set_oneshot_timer: None, stop: None, shutdown: None };

static TIMER_OPS: AtomicConstPtr<PdevTimerOps> =
    AtomicConstPtr::new(core::ptr::addr_of!(DEFAULT_OPS));

fn get_ops() -> &'static PdevTimerOps {
    // The acquire load pairs with the release store in `pdev_register_timer` (the equivalent of
    // the `arch::ThreadMemoryBarrier()` publication in C++).
    let ops_ptr = TIMER_OPS.load(Ordering::Acquire);
    // SAFETY: `TIMER_OPS` always points to either `DEFAULT_OPS` or a registered `PdevTimerOps`
    // table with `'static` lifetime guaranteed by the caller of `pdev_register_timer`.
    unsafe { &*ops_ptr }
}

/// Read the current ticks of the hardware timer, at the rate the timer is ticking.
/// Generally converted to real time in current_mono_time().
pub fn timer_current_ticks() -> zx_ticks_t {
    if let Some(current_ticks) = get_ops().current_ticks { current_ticks() } else { 0 }
}

/// Set a timer to fire at the deadline specified that calls timer_tick().
pub fn timer_set_oneshot_timer(deadline: zx_ticks_t) -> Result<(), Status> {
    if let Some(set_oneshot_timer) = get_ops().set_oneshot_timer {
        set_oneshot_timer(deadline)
    } else {
        panic!("pdev timer set_oneshot_timer unimplemented")
    }
}

/// Cancel a pending oneshot timer. Okay to call if no pending oneshot.
pub fn timer_stop() -> Result<(), Status> {
    if let Some(stop) = get_ops().stop { stop() } else { panic!("pdev timer stop unimplemented") }
}

/// Stop the timer hardware. Stop should be called before shutdown.
pub fn timer_shutdown() -> Result<(), Status> {
    if let Some(shutdown) = get_ops().shutdown {
        shutdown()
    } else {
        panic!("pdev timer shutdown unimplemented")
    }
}

/// Registers the platform timer operations table with `'static` lifetime.
pub fn pdev_register_timer(ops: &'static PdevTimerOps) {
    // Publish the ops pointer with release semantics so that the table (and any driver
    // initialization writes preceding registration) is visible to all CPUs that subsequently
    // observe it. This is the equivalent of the C++ `arch::ThreadMemoryBarrier()`.
    TIMER_OPS.store(ops as *const PdevTimerOps, Ordering::Release);
}

// C FFI exports

/// Read the current ticks of the hardware timer (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_current_ticks() -> zx_ticks_t {
    timer_current_ticks()
}

/// Set a oneshot timer to fire at `deadline` (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_set_oneshot_timer(deadline: zx_ticks_t) -> Result<(), Status> {
    timer_set_oneshot_timer(deadline)
}

/// Cancel a pending oneshot timer (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_stop() -> Result<(), Status> {
    timer_stop()
}

/// Stop the timer hardware (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_shutdown() -> Result<(), Status> {
    timer_shutdown()
}

/// Registers the platform timer operations table from C-ABI.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevTimerOps` table that remains valid
/// for the duration of the kernel's execution.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_register_timer(ops: *const PdevTimerOps) {
    let target_ptr = if ops.is_null() { core::ptr::addr_of!(DEFAULT_OPS) } else { ops };
    // SAFETY: Caller guarantees `ops` points to a valid `PdevTimerOps` table that remains valid
    // for the duration of the kernel's execution.
    TIMER_OPS.store(target_ptr, core::sync::atomic::Ordering::Release);
}
