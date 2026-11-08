// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT. Generated from FIDL library zx by zither, a Fuchsia platform tool.

/// Syscall number of `zx_channel_create`.
pub const ZX_SYS_CHANNEL_CREATE: u64 = 0;
/// Syscall number of `zx_channel_read`.
pub const ZX_SYS_CHANNEL_READ: u64 = 1;
/// Syscall number of `zx_channel_write`.
pub const ZX_SYS_CHANNEL_WRITE: u64 = 2;
/// Syscall number of `zx_clock_get_monotonic_via_kernel`.
pub const ZX_SYS_CLOCK_GET_MONOTONIC_VIA_KERNEL: u64 = 3;
/// Syscall number of `zx_handle_close_many`.
pub const ZX_SYS_HANDLE_CLOSE_MANY: u64 = 4;
/// Syscall number of `zx_ktrace_control`.
pub const ZX_SYS_KTRACE_CONTROL: u64 = 5;
/// Syscall number of `zx_nanosleep`.
pub const ZX_SYS_NANOSLEEP: u64 = 6;
/// Syscall number of `zx_process_exit`.
pub const ZX_SYS_PROCESS_EXIT: u64 = 7;
/// Syscall number of `zx_syscall_next`.
pub const ZX_SYS_SYSCALL_NEXT: u64 = 8;
/// Syscall number of `zx_syscall_test0`.
pub const ZX_SYS_SYSCALL_TEST0: u64 = 9;
/// Syscall number of `zx_syscall_test1`.
pub const ZX_SYS_SYSCALL_TEST1: u64 = 10;
/// Syscall number of `zx_syscall_test2`.
pub const ZX_SYS_SYSCALL_TEST2: u64 = 11;

/// Number of kernel syscall entry points; valid syscall numbers are `0..ZX_SYS_COUNT`.
pub const ZX_SYS_COUNT: usize = 12;

/// Interned syscall names, indexed by syscall number, for `kernel:syscall` trace events.
pub static SYSCALL_NAME_REFS: [&crate::ktrace_rs::InternedString; ZX_SYS_COUNT] = [
    crate::ktrace_rs::resolve_string!("channel_create"),
    crate::ktrace_rs::resolve_string!("channel_read"),
    crate::ktrace_rs::resolve_string!("channel_write"),
    crate::ktrace_rs::resolve_string!("clock_get_monotonic_via_kernel"),
    crate::ktrace_rs::resolve_string!("handle_close_many"),
    crate::ktrace_rs::resolve_string!("ktrace_control"),
    crate::ktrace_rs::resolve_string!("nanosleep"),
    crate::ktrace_rs::resolve_string!("process_exit"),
    crate::ktrace_rs::resolve_string!("syscall_next"),
    crate::ktrace_rs::resolve_string!("syscall_test0"),
    crate::ktrace_rs::resolve_string!("syscall_test1"),
    crate::ktrace_rs::resolve_string!("syscall_test2"),
];
