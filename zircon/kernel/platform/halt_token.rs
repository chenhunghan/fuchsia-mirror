// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::Deadline;
use crate::kernel::event::Event;
use core::convert::Infallible;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pin_init};
use zx_status::Status;

static G_INSTANCE: LazyInit<HaltToken> = LazyInit::uninit();

init::lk_init_hook!(
    halt_token,
    |_level| {
        // SAFETY: `LK_INIT_LEVEL_EARLIEST` primary-CPU hooks run once on the boot CPU with
        // interrupts disabled before threading or secondary CPUs start, and `HaltToken::init` does
        // not access `G_INSTANCE`, so `init_pin` is serialized with respect to all other accesses
        // to `G_INSTANCE`.
        let Ok(()) = unsafe { Pin::static_ref(&G_INSTANCE).init_pin(HaltToken::init()) };
    },
    init::LK_INIT_LEVEL_EARLIEST
);

/// This object is used to coordinate concurrent halt/reboot operations.
///
/// The idea is there's a single resource, the "halt token" and only the holder of the token may
/// initiate a halt/reboot (except for panics).
#[pin_data]
pub struct HaltToken {
    // No public construction. Global singleton only.
    halt_token_claimed: AtomicBool,
    #[pin]
    ack_event: Event,
}

impl HaltToken {
    /// In-place initializer for the global singleton.
    fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            halt_token_claimed: AtomicBool::new(false),
            ack_event <- Event::init_unsignaled(),
        })
    }

    /// Accessor for the global singleton halt token.
    ///
    /// # Panics
    ///
    /// Panics if called before initialization at `LK_INIT_LEVEL_EARLIEST`.
    #[inline]
    pub fn get() -> &'static Self {
        G_INSTANCE.get()
    }

    /// The `take` method attempts to acquire the token and signals an irrevocable
    /// intention to halt (or reboot) the system.
    ///
    /// If this method returns `true`, the caller has acquired the token and is now
    /// responsible for halting/reboot.
    ///
    /// If this method returns `false`, the caller failed to acquire the token
    /// (because some other caller got it). In this case the caller must take no
    /// action and allow the holder to halt/reboot.
    #[must_use]
    pub fn take(&self) -> bool {
        !self.halt_token_claimed.swap(true, Ordering::SeqCst)
    }

    /// Wait until `deadline` for user-mode to acknowledge a kernel-signaled Halt.
    /// In practice, this occurs when the kernel memory watchdog encounters a fatal
    /// OOM condition and signals user mode, in order to give it a last chance to
    /// persist logs and cleanly shutdown drivers before the reboot actually takes
    /// place.
    pub fn wait_for_ack(&self, deadline: &Deadline) -> Result<(), Status> {
        self.ack_event.wait(deadline)
    }

    /// Called during processing of the
    /// `ZX_SYSTEM_POWERCTL_ACK_KERNEL_INITIATED_REBOOT` topic in `zx_system_powerctl`.
    /// Indicates that user-mode has finished responding to the kernel's signal of
    /// an impending reboot, and that user-mode is now ready for the reboot to
    /// proceed.
    ///
    /// If the halt token has not yet been claimed, this function will return an
    /// error and leave the `ack_event` in the unsignaled state.
    pub fn ack_pending_halt(&self) -> Result<(), Status> {
        if !self.halt_token_claimed.load(Ordering::SeqCst) {
            return Err(Status::BAD_STATE);
        }
        self.ack_event.signal();
        Ok(())
    }
}
