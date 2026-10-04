// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::Deadline;
use crate::kernel::event::Event;
use core::sync::atomic::{AtomicBool, Ordering};
use halt_token_bindings as bindings;
use zx_status::Status;

// One of the members of the HaltToken has a zero sized type, which is not FFI compliant, however we
// are separately validating the equivalence of the final layout of the C++ and Rust objects so
// this is okay.
#[allow(improper_ctypes)]
unsafe extern "C" {
    // C++ name mangled form of `HaltToken HaltToken::g_instance`
    #[link_name = "_ZN9HaltToken10g_instanceE"]
    static G_INSTANCE: HaltToken;
}

/// This object is used to coordinate concurrent halt/reboot operations.
///
/// The idea is there's a single resource, the "halt token" and only the holder of the token may
/// initiate a halt/reboot (except for panics).
#[repr(C)]
pub struct HaltToken {
    // No public construction. Global singleton only.
    halt_token_claimed: AtomicBool,
    ack_event: Event,
}

// Compile-time layout assertions against the C++ HaltToken type via bindgen.
zr::static_assert!(
    core::mem::size_of::<HaltToken>() == core::mem::size_of::<bindings::HaltToken>()
);
zr::static_assert!(
    core::mem::align_of::<HaltToken>() == core::mem::align_of::<bindings::HaltToken>()
);
zr::static_assert!(
    core::mem::offset_of!(HaltToken, halt_token_claimed)
        == core::mem::offset_of!(bindings::HaltToken, halt_token_claimed_)
);
zr::static_assert!(
    core::mem::offset_of!(HaltToken, ack_event)
        == core::mem::offset_of!(bindings::HaltToken, ack_event_)
);

impl HaltToken {
    /// Accessor for the global singleton halt token.
    #[inline]
    pub fn get() -> &'static Self {
        // SAFETY: `G_INSTANCE` is the global `HaltToken::g_instance` singleton, and `HaltToken`
        // uses interior synchronization (`AtomicBool` and `Event`) so shared access is safe.
        unsafe { &G_INSTANCE }
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
