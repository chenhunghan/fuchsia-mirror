// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(target_arch = "aarch64")]
use super::SuspendWakeupTimer;
#[cfg(target_arch = "aarch64")]
use crate::kernel::types::cpu_mask_t;
#[cfg(target_arch = "aarch64")]
use crate::platform_rs::timer::DurationBoot;
#[cfg(target_arch = "aarch64")]
use core::ffi::c_void;
#[cfg(target_arch = "aarch64")]
use core::pin::Pin;
#[cfg(target_arch = "aarch64")]
use core::ptr::NonNull;
#[cfg(target_arch = "aarch64")]
use zx_status::Status;

/// Opaque facade representing a C++ `Armv7MmioTimer`.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub(crate) struct Armv7MmioTimer {
    _facade: zr::OpaqueFacade,
}

#[cfg(target_arch = "aarch64")]
zr::static_assert!(core::mem::size_of::<Armv7MmioTimer>() == 0);

/// Opaque facade representing a C++ `Armv7MmioTimer::Timer`.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub(crate) struct Armv7MmioTimerSubtimer {
    _facade: zr::OpaqueFacade,
}

#[cfg(target_arch = "aarch64")]
zr::static_assert!(core::mem::size_of::<Armv7MmioTimerSubtimer>() == 0);

// SAFETY: `Armv7MmioTimer::Timer` is internally synchronized by its spinlock (`irq_lock_`).
#[cfg(target_arch = "aarch64")]
unsafe impl Sync for Armv7MmioTimerSubtimer {}

#[cfg(target_arch = "aarch64")]
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqHandlerResult {
    RemainRegistered = 0,
    Unregister = 1,
}

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    fn cpp_armv7_mmio_timer_get(ndx: usize) -> Option<&'static Armv7MmioTimer>;
    fn cpp_armv7_mmio_timer_vct_timer(
        timer_block: &'static Armv7MmioTimer,
    ) -> &'static Armv7MmioTimerSubtimer;
    fn cpp_armv7_mmio_timer_pct_timer(
        timer_block: &'static Armv7MmioTimer,
    ) -> &'static Armv7MmioTimerSubtimer;
    fn cpp_armv7_mmio_timer_timer_supported(timer: &Armv7MmioTimerSubtimer) -> bool;
    fn cpp_armv7_mmio_timer_timer_enabled(timer: &Armv7MmioTimerSubtimer) -> bool;
    fn cpp_armv7_mmio_timer_timer_cancel(timer: &Armv7MmioTimerSubtimer) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_set_handler(
        timer: &Armv7MmioTimerSubtimer,
        context: NonNull<c_void>,
        mask: cpu_mask_t,
    ) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_reset_handler(
        timer: &Armv7MmioTimerSubtimer,
    ) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_set_relative_timer(
        timer: &Armv7MmioTimerSubtimer,
        relative_timeout: DurationBoot,
    ) -> Result<(), Status>;
}

#[cfg(target_arch = "aarch64")]
impl Armv7MmioTimer {
    pub(crate) const MAX_TIMERS: usize = 8;

    /// Returns a reference to the `ndx`-th `Armv7MmioTimer` block, if present.
    ///
    /// # Safety
    ///
    /// Must only be called after platform timer initialization has completed (no earlier than
    /// `LK_INIT_LEVEL_PLATFORM`).
    pub(crate) unsafe fn get(ndx: usize) -> Option<&'static Self> {
        // SAFETY: The caller guarantees platform timer initialization has completed, and
        // `cpp_armv7_mmio_timer_get` bounds-checks `ndx` and returns either a valid pointer to a
        // static `Armv7MmioTimer` or null.
        unsafe { cpp_armv7_mmio_timer_get(ndx) }
    }

    pub(crate) fn vct_timer(&'static self) -> &'static Armv7MmioTimerSubtimer {
        // SAFETY: `self` is a valid static `Armv7MmioTimer` and `vct_timer()` returns a valid
        // C++ reference to its embedded VCT subtimer.
        unsafe { cpp_armv7_mmio_timer_vct_timer(self) }
    }

    pub(crate) fn pct_timer(&'static self) -> &'static Armv7MmioTimerSubtimer {
        // SAFETY: `self` is a valid static `Armv7MmioTimer` and `pct_timer()` returns a valid
        // C++ reference to its embedded PCT subtimer.
        unsafe { cpp_armv7_mmio_timer_pct_timer(self) }
    }
}

#[cfg(target_arch = "aarch64")]
impl Armv7MmioTimerSubtimer {
    pub(crate) fn supported(&self) -> bool {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_supported(self) }
    }

    pub(crate) fn enabled(&self) -> bool {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_enabled(self) }
    }

    pub(crate) fn cancel_timer(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` (internally synchronized by
        // its spinlock).
        unsafe { cpp_armv7_mmio_timer_timer_cancel(self) }
    }

    /// Registers `outer` as the interrupt handler context on `self`.
    ///
    /// C++ treats the context as an opaque pointer and passes it back verbatim to
    /// [`rust_armv7_mmio_suspend_wakeup_timer_irq_handler`].
    ///
    /// # Safety
    ///
    /// `outer` must remain valid and pinned until [`Self::reset_handler`] completes.
    pub(crate) unsafe fn set_handler(
        &self,
        outer: Pin<&SuspendWakeupTimer>,
        mask: cpu_mask_t,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` and the caller guarantees
        // `outer` remains valid while the handler is registered.
        unsafe {
            cpp_armv7_mmio_timer_timer_set_handler(
                self,
                NonNull::from_ref(outer.get_ref()).cast::<c_void>(),
                mask,
            )
        }
    }

    pub(crate) fn reset_handler(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` (internally synchronized by
        // its spinlock).
        unsafe { cpp_armv7_mmio_timer_timer_reset_handler(self) }
    }

    pub(crate) fn set_relative_timer(&self, relative_timeout: DurationBoot) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_set_relative_timer(self, relative_timeout) }
    }
}

/// Interrupt handler callback invoked by `Armv7MmioTimer::Timer`.
#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_suspend_wakeup_timer_irq_handler(
    timer: &SuspendWakeupTimer,
) -> IrqHandlerResult {
    super::Armv7MmioSuspendWakeupTimer::irq_handler(timer)
}
