// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT. Generated from FIDL library zx by zither, a Fuchsia platform tool.

// Per-syscall kernel entry points. The architecture syscall dispatcher calls `wrapper_<syscall>`
// with the raw 64-bit register arguments of the syscall followed by the user program counter;
// each wrapper sanitizes the arguments, calls the `sys_<syscall>` implementation and copies any
// output handles back to user memory.

#[allow(unused_imports)]
use super::{SyscallResult, SyscallReturn, ValidSyscallPC, do_syscall};
#[allow(unused_imports)]
use crate::kernel::thread;
#[allow(unused_imports)]
use crate::object::HandleValue;
#[allow(unused_imports)]
use crate::syscalls_rs::safe_syscall_argument::SafeSyscallArgument;
#[allow(unused_imports)]
use crate::syscalls_rs::syscall_numbers::*;
#[allow(unused_imports)]
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
#[allow(unused_imports)]
use zx_status::Status;
#[allow(unused_imports)]
use zx_types::*;

// The `sys_<syscall>` implementations, defined with `#[syscalls_macro::syscall]` in Rust or as
// `extern "C"` functions in C++.
#[allow(improper_ctypes)]
unsafe extern "C" {
    fn sys_channel_create(
        options: u32,
        out0: &mut HandleValue,
        out1: &mut HandleValue,
    ) -> Result<(), Status>;
    fn sys_channel_read(
        handle: HandleValue,
        options: u32,
        bytes: UserOutPtr<u8>,
        handles: UserOutPtr<zx_handle_t>,
        num_bytes: u32,
        num_handles: u32,
        actual_bytes: UserOutPtr<u32>,
        actual_handles: UserOutPtr<u32>,
    ) -> Result<(), Status>;
    fn sys_channel_write(
        handle: HandleValue,
        options: u32,
        bytes: UserInPtr<u8>,
        num_bytes: u32,
        handles: UserInPtr<zx_handle_t>,
        num_handles: u32,
    ) -> Result<(), Status>;
    fn sys_clock_get_monotonic_via_kernel() -> zx_time_t;
    fn sys_handle_close_many(
        handles: UserInPtr<zx_handle_t>,
        num_handles: usize,
    ) -> Result<(), Status>;
    fn sys_ktrace_control(
        handle: HandleValue,
        action: u32,
        options: u32,
        ptr: UserInOutPtr<u8>,
    ) -> Result<(), Status>;
    fn sys_nanosleep(deadline: zx_time_t) -> Result<(), Status>;
    fn sys_process_exit(retcode: i64) -> ();
    fn sys_syscall_next() -> ();
    fn sys_syscall_test0() -> ();
    fn sys_syscall_test1() -> ();
    fn sys_syscall_test2() -> ();
}

/// Kernel entry point of `zx_channel_create`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_channel_create(
    arg_options: u64,
    arg_out0: u64,
    arg_out1: u64,
    pc: u64,
) -> SyscallResult {
    let make_call = move || {
        let mut out_handle_out0 = HandleValue::new(ZX_HANDLE_INVALID);
        let mut out_handle_out1 = HandleValue::new(ZX_HANDLE_INVALID);
        // SAFETY: `sys_channel_create` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value
        // and output handles are written to live locals.
        let mut result = unsafe {
            sys_channel_create(
                SafeSyscallArgument::sanitize(arg_options),
                &mut out_handle_out0,
                &mut out_handle_out1,
            )
        };
        // Complete copy-out of output handles.
        if result.is_err() {
            core::hint::cold_path();
            return result.into_raw_return();
        }
        result = UserOutPtr::<zx_handle_t>::sanitize(arg_out0)
            .copy_to_user(&out_handle_out0.raw_value());
        if result.is_err() {
            core::hint::cold_path();
            thread::signal_policy_exception(ZX_EXCP_POLICY_CODE_HANDLE_LEAK, 0);
        }
        result = UserOutPtr::<zx_handle_t>::sanitize(arg_out1)
            .copy_to_user(&out_handle_out1.raw_value());
        if result.is_err() {
            core::hint::cold_path();
            thread::signal_policy_exception(ZX_EXCP_POLICY_CODE_HANDLE_LEAK, 0);
        }
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_CHANNEL_CREATE, pc, ValidSyscallPC::channel_create, make_call) }
}

/// Kernel entry point of `zx_channel_read`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_channel_read(
    arg_handle: u64,
    arg_options: u64,
    arg_bytes: u64,
    arg_handles: u64,
    arg_num_bytes: u64,
    arg_num_handles: u64,
    arg_actual_bytes: u64,
    arg_actual_handles: u64,
    pc: u64,
) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_channel_read` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        let result = unsafe {
            sys_channel_read(
                SafeSyscallArgument::sanitize(arg_handle),
                SafeSyscallArgument::sanitize(arg_options),
                SafeSyscallArgument::sanitize(arg_bytes),
                SafeSyscallArgument::sanitize(arg_handles),
                SafeSyscallArgument::sanitize(arg_num_bytes),
                SafeSyscallArgument::sanitize(arg_num_handles),
                SafeSyscallArgument::sanitize(arg_actual_bytes),
                SafeSyscallArgument::sanitize(arg_actual_handles),
            )
        };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_CHANNEL_READ, pc, ValidSyscallPC::channel_read, make_call) }
}

/// Kernel entry point of `zx_channel_write`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_channel_write(
    arg_handle: u64,
    arg_options: u64,
    arg_bytes: u64,
    arg_num_bytes: u64,
    arg_handles: u64,
    arg_num_handles: u64,
    pc: u64,
) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_channel_write` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        let result = unsafe {
            sys_channel_write(
                SafeSyscallArgument::sanitize(arg_handle),
                SafeSyscallArgument::sanitize(arg_options),
                SafeSyscallArgument::sanitize(arg_bytes),
                SafeSyscallArgument::sanitize(arg_num_bytes),
                SafeSyscallArgument::sanitize(arg_handles),
                SafeSyscallArgument::sanitize(arg_num_handles),
            )
        };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_CHANNEL_WRITE, pc, ValidSyscallPC::channel_write, make_call) }
}

/// Kernel entry point of `zx_clock_get_monotonic_via_kernel`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_clock_get_monotonic_via_kernel(pc: u64) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_clock_get_monotonic_via_kernel` is the kernel implementation of this
        // zero-argument syscall.
        let result = unsafe { sys_clock_get_monotonic_via_kernel() };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe {
        do_syscall(
            ZX_SYS_CLOCK_GET_MONOTONIC_VIA_KERNEL,
            pc,
            ValidSyscallPC::clock_get_monotonic_via_kernel,
            make_call,
        )
    }
}

/// Kernel entry point of `zx_handle_close_many`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_handle_close_many(
    arg_handles: u64,
    arg_num_handles: u64,
    pc: u64,
) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_handle_close_many` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        let result = unsafe {
            sys_handle_close_many(
                SafeSyscallArgument::sanitize(arg_handles),
                SafeSyscallArgument::sanitize(arg_num_handles),
            )
        };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe {
        do_syscall(ZX_SYS_HANDLE_CLOSE_MANY, pc, ValidSyscallPC::handle_close_many, make_call)
    }
}

/// Kernel entry point of `zx_ktrace_control`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_ktrace_control(
    arg_handle: u64,
    arg_action: u64,
    arg_options: u64,
    arg_ptr: u64,
    pc: u64,
) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_ktrace_control` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        let result = unsafe {
            sys_ktrace_control(
                SafeSyscallArgument::sanitize(arg_handle),
                SafeSyscallArgument::sanitize(arg_action),
                SafeSyscallArgument::sanitize(arg_options),
                SafeSyscallArgument::sanitize(arg_ptr),
            )
        };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_KTRACE_CONTROL, pc, ValidSyscallPC::ktrace_control, make_call) }
}

/// Kernel entry point of `zx_nanosleep`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_nanosleep(arg_deadline: u64, pc: u64) -> SyscallResult {
    let make_call = move || {
        // SAFETY: `sys_nanosleep` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        let result = unsafe { sys_nanosleep(SafeSyscallArgument::sanitize(arg_deadline)) };
        result.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_NANOSLEEP, pc, ValidSyscallPC::nanosleep, make_call) }
}

/// Kernel entry point of `zx_process_exit`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_process_exit(arg_retcode: u64, pc: u64) -> SyscallResult {
    let make_call = move || {
        /* noreturn */
        // SAFETY: `sys_process_exit` is the kernel implementation of this
        // syscall: every argument is sanitized from the raw user-provided register value.
        unsafe { sys_process_exit(SafeSyscallArgument::sanitize(arg_retcode)) };
        /* NOTREACHED */
        Status::BAD_STATE.into_raw_return()
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_PROCESS_EXIT, pc, ValidSyscallPC::process_exit, make_call) }
}

/// Kernel entry point of `zx_syscall_next`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_syscall_next(pc: u64) -> SyscallResult {
    let make_call = move || {
        /* void */
        // SAFETY: `sys_syscall_next` is the kernel implementation of this
        // zero-argument syscall.
        unsafe { sys_syscall_next() };
        i64::from(ZX_OK) as u64
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_SYSCALL_NEXT, pc, ValidSyscallPC::syscall_next, make_call) }
}

/// Kernel entry point of `zx_syscall_test0`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_syscall_test0(pc: u64) -> SyscallResult {
    let make_call = move || {
        /* void */
        // SAFETY: `sys_syscall_test0` is the kernel implementation of this
        // zero-argument syscall.
        unsafe { sys_syscall_test0() };
        i64::from(ZX_OK) as u64
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_SYSCALL_TEST0, pc, ValidSyscallPC::syscall_test0, make_call) }
}

/// Kernel entry point of `zx_syscall_test1`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_syscall_test1(pc: u64) -> SyscallResult {
    let make_call = move || {
        /* void */
        // SAFETY: `sys_syscall_test1` is the kernel implementation of this
        // zero-argument syscall.
        unsafe { sys_syscall_test1() };
        i64::from(ZX_OK) as u64
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_SYSCALL_TEST1, pc, ValidSyscallPC::syscall_test1, make_call) }
}

/// Kernel entry point of `zx_syscall_test2`.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wrapper_syscall_test2(pc: u64) -> SyscallResult {
    let make_call = move || {
        /* void */
        // SAFETY: `sys_syscall_test2` is the kernel implementation of this
        // zero-argument syscall.
        unsafe { sys_syscall_test2() };
        i64::from(ZX_OK) as u64
    };
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(ZX_SYS_SYSCALL_TEST2, pc, ValidSyscallPC::syscall_test2, make_call) }
}
