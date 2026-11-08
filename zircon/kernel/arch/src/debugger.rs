// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Cross-architecture debugger register state accessors (`<arch/debugger.h>`).

use crate::kernel::thread::Thread;
use zx_status::Status;

#[cfg(target_arch = "x86_64")]
mod ffi {
    use crate::kernel::thread::Thread;
    use zr::OpaqueFacade;
    use zx_types::{zx_status_t, zx_thread_state_general_regs_t, zx_thread_state_single_step_t};

    /// Opaque facade for `zx_thread_state_fp_regs_t` on x86_64.
    #[repr(C)]
    pub(super) struct ZxThreadStateFpRegs(OpaqueFacade);

    /// Opaque facade for `zx_thread_state_vector_regs_t` on x86_64.
    #[repr(C)]
    pub(super) struct ZxThreadStateVectorRegs(OpaqueFacade);

    /// Opaque facade for `zx_thread_state_debug_regs_t` on x86_64.
    #[repr(C)]
    pub(super) struct ZxThreadStateDebugRegs(OpaqueFacade);

    #[allow(improper_ctypes)]
    unsafe extern "C" {
        /// Reads the saved user-mode general registers for `thread` into `out`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `out` must be aligned and writable for
        /// `zx_thread_state_general_regs_t`.
        pub(super) fn arch_get_general_regs(
            thread: *mut Thread,
            out: *mut zx_thread_state_general_regs_t,
        ) -> zx_status_t;

        /// Writes `in_` into the saved user-mode general registers for `thread`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `in_` must point to an initialized
        /// `zx_thread_state_general_regs_t`.
        pub(super) fn arch_set_general_regs(
            thread: *mut Thread,
            in_: *const zx_thread_state_general_regs_t,
        ) -> zx_status_t;

        /// Reads the saved floating-point registers for `thread` into `out`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `out` must be aligned and writable for
        /// `zx_thread_state_fp_regs_t`.
        pub(super) fn arch_get_fp_regs(
            thread: *mut Thread,
            out: *mut ZxThreadStateFpRegs,
        ) -> zx_status_t;

        /// Writes `in_` into the saved floating-point registers for `thread`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `in_` must point to an initialized
        /// `zx_thread_state_fp_regs_t`.
        pub(super) fn arch_set_fp_regs(
            thread: *mut Thread,
            in_: *const ZxThreadStateFpRegs,
        ) -> zx_status_t;

        /// Reads the saved vector registers for `thread` into `out`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `out` must be aligned and writable for
        /// `zx_thread_state_vector_regs_t`.
        pub(super) fn arch_get_vector_regs(
            thread: *mut Thread,
            out: *mut ZxThreadStateVectorRegs,
        ) -> zx_status_t;

        /// Writes `in_` into the saved vector registers for `thread`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `in_` must point to an initialized
        /// `zx_thread_state_vector_regs_t`.
        pub(super) fn arch_set_vector_regs(
            thread: *mut Thread,
            in_: *const ZxThreadStateVectorRegs,
        ) -> zx_status_t;

        /// Reads the debug registers for `thread` into `out`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `out` must be aligned and writable for
        /// `zx_thread_state_debug_regs_t`.
        pub(super) fn arch_get_debug_regs(
            thread: *mut Thread,
            out: *mut ZxThreadStateDebugRegs,
        ) -> zx_status_t;

        /// Writes `in_` into the debug registers for `thread`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `in_` must point to an initialized
        /// `zx_thread_state_debug_regs_t`.
        pub(super) fn arch_set_debug_regs(
            thread: *mut Thread,
            in_: *const ZxThreadStateDebugRegs,
        ) -> zx_status_t;

        /// Reads the single-step state for `thread` into `out`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `out` must be aligned and writable for
        /// `zx_thread_state_single_step_t`.
        pub(super) fn arch_get_single_step(
            thread: *mut Thread,
            out: *mut zx_thread_state_single_step_t,
        ) -> zx_status_t;

        /// Writes `in_` into the single-step state for `thread`.
        ///
        /// # Safety
        ///
        /// `thread` must point to a live `Thread` that is suspended or in an exception with no
        /// active chain lock transaction, and `in_` must point to an initialized
        /// `zx_thread_state_single_step_t`.
        pub(super) fn arch_set_single_step(
            thread: *mut Thread,
            in_: *const zx_thread_state_single_step_t,
        ) -> zx_status_t;
    }
}

/// Reads the saved user-mode general registers for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_general_regs_t`.
#[inline(always)]
pub unsafe fn arch_get_general_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_general_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_general_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_get_general_regs(thread, out.cast()) })
    }
}

/// Writes `in_` into the saved user-mode general registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_thread_state_general_regs_t`.
#[inline(always)]
pub unsafe fn arch_set_general_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_general_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_general_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_set_general_regs(thread, in_.cast()) })
    }
}

/// Reads the saved floating-point registers for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_fp_regs_t`.
#[inline(always)]
pub unsafe fn arch_get_fp_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::debugger::arch_get_fp_regs(thread, out.cast())
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_fp_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_get_fp_regs(thread, out.cast()) })
    }
}

/// Writes `in_` into the saved floating-point registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_thread_state_fp_regs_t`.
#[inline(always)]
pub unsafe fn arch_set_fp_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::debugger::arch_set_fp_regs(thread, in_.cast())
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_fp_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_set_fp_regs(thread, in_.cast()) })
    }
}

/// Reads the saved vector registers for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_vector_regs_t`.
#[inline(always)]
pub unsafe fn arch_get_vector_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_vector_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_vector_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_get_vector_regs(thread, out.cast()) })
    }
}

/// Writes `in_` into the saved vector registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_thread_state_vector_regs_t`.
#[inline(always)]
pub unsafe fn arch_set_vector_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_vector_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_vector_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_set_vector_regs(thread, in_.cast()) })
    }
}

/// Reads the debug registers for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_debug_regs_t`.
#[inline(always)]
pub unsafe fn arch_get_debug_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_debug_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_debug_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_get_debug_regs(thread, out.cast()) })
    }
}

/// Writes `in_` into the debug registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_thread_state_debug_regs_t`.
#[inline(always)]
pub unsafe fn arch_set_debug_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_debug_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_debug_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_set_debug_regs(thread, in_.cast()) })
    }
}

/// Reads the single-step state for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_single_step_t`.
#[inline(always)]
pub unsafe fn arch_get_single_step(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_single_step(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(
            unsafe { super::riscv64::debugger::arch_get_single_step(thread.cast(), out.cast()) }
                .into_raw(),
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_get_single_step(thread, out.cast()) })
    }
}

/// Writes `in_` into the single-step state for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_thread_state_single_step_t`.
#[inline(always)]
pub unsafe fn arch_set_single_step(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_single_step(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(
            unsafe { super::riscv64::debugger::arch_set_single_step(thread.cast(), in_.cast()) }
                .into_raw(),
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(unsafe { ffi::arch_set_single_step(thread, in_.cast()) })
    }
}

/// Returns whether ARM64 SVE register state is supported on this system.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn arch_arm64_sve_supported() -> bool {
    super::arm64::debugger::arch_arm64_sve_supported()
}

/// Reads the ARM64 SVE predicate registers and FFR for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_arm64_thread_state_sve_regs_t`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn arch_get_arm64_sve(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
    unsafe { super::arm64::debugger::arch_get_arm64_sve(thread, out.cast()) }
}

/// Writes `in_` into the ARM64 SVE predicate registers and FFR for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_arm64_thread_state_sve_regs_t`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn arch_set_arm64_sve(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
    unsafe { super::arm64::debugger::arch_set_arm64_sve(thread, in_.cast()) }
}
