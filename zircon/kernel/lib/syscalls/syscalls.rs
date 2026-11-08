// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Main syscall dispatch routine. For every syscall in the system stamp out a separate
//! `wrapper_<name of syscall>` routine using the [`do_syscall`] inline function instantiated
//! by Zither-generated wrapper functions.
//!
//! The end result is a `wrapper_<syscall>` that does per syscall argument validation and
//! argument marshalling to an inner routine called `sys_<syscall>`.

use super::syscall_numbers::SYSCALL_NAME_REFS;
use crate::kernel::{stats, thread};
use crate::ktrace_rs::{InternedString, declare_interned_string};
use crate::object::ProcessDispatcher;
use crate::userabi::valid_syscall_pc as ValidSyscallPC;
use ::debug::{ltracef, ltracef_level};
use zr::{static_assert, static_assert_size_and_align};
use zx_status::Status;
use zx_types::ZX_EXCP_POLICY_CODE_BAD_SYSCALL;

const LOCAL_TRACE: u32 = 0;

declare_interned_string!(OUT_OF_RANGE_REF, "[out of range]");

/// Result returned from the syscall dispatcher to the assembly entry point.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyscallResult {
    /// The assembler relies on the fact that the ABI will return this in
    /// `x0`,`x1` (arm) or `rax`,`rdx` (x86) so we use plain types here to ensure this.
    pub status: u64,
    /// Non-zero if thread was signaled.
    pub is_signaled: u64,
}

static_assert_size_and_align!(SyscallResult, 16, 8);
static_assert!(core::mem::offset_of!(SyscallResult, status) == 0);
static_assert!(core::mem::offset_of!(SyscallResult, is_signaled) == 8);

/// Conversion of a syscall's return value into the raw 64-bit value handed back to user mode.
trait SyscallReturn {
    /// Returns the value as the 64-bit integer the assembly entry point returns to user mode.
    fn into_raw_return(self) -> u64;
}

impl SyscallReturn for Status {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        // A `zx_status_t` is sign-extended into the 64-bit return register.
        i64::from(self.into_raw()) as u64
    }
}

impl SyscallReturn for Result<(), Status> {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        i64::from(Status::result_into_raw(self)) as u64
    }
}

impl SyscallReturn for i64 {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        self as u64
    }
}

impl SyscallReturn for u64 {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        self
    }
}

impl SyscallReturn for u32 {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        u64::from(self)
    }
}

impl SyscallReturn for i32 {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        i64::from(self) as u64
    }
}

impl SyscallReturn for usize {
    #[inline(always)]
    fn into_raw_return(self) -> u64 {
        self as u64
    }
}

/// Returns the interned name of syscall `syscall_num` for `kernel:syscall` trace events.
#[inline]
fn syscall_name_ref(syscall_num: u64) -> &'static InternedString {
    if let Some(&name_ref) = SYSCALL_NAME_REFS.get(syscall_num as usize) {
        return name_ref;
    }
    core::hint::cold_path();
    OUT_OF_RANGE_REF
}

/// Handles a syscall whose number is out of range or whose PC did not originate from the vDSO.
///
/// Signals a `ZX_EXCP_POLICY_CODE_BAD_SYSCALL` policy exception on the current thread and returns
/// `Status::BAD_SYSCALL` as the syscall status.
#[cold]
#[inline(never)]
fn sys_invalid_syscall(num: u64, pc: u64, vdso_code_address: usize) -> Status {
    ltracef!("invalid syscall {} from PC {:#x} vDSO code {:#x}\n", num, pc, vdso_code_address);
    thread::signal_policy_exception(ZX_EXCP_POLICY_CODE_BAD_SYSCALL, num as u32);
    Status::BAD_SYSCALL
}

/// Records the start of a `kernel:syscall` trace duration for `syscall_num`.
#[cold]
#[inline(never)]
fn ktrace_syscall_begin(syscall_num: u64) {
    crate::ktrace_rs::duration_begin!("kernel:syscall", (syscall_name_ref(syscall_num)));
}

/// Records the end of a `kernel:syscall` trace duration for `syscall_num`.
#[cold]
#[inline(never)]
fn ktrace_syscall_end(syscall_num: u64) {
    crate::ktrace_rs::duration_end!("kernel:syscall", (syscall_name_ref(syscall_num)));
}

/// Shared syscall preamble; returns the address of the vDSO code in the current process.
///
/// Try to do as much as possible in the shared preamble code to maximize code reuse
/// between syscalls.
///
/// N.B. Interrupts must be disabled on entry and they will be enabled on exit.
/// The reason is the calls to `curr_cpu_num` in the ktrace and per-CPU stats calls: we
/// don't want the CPU changing during the call.
///
/// # Safety
///
/// Must be called on the syscall path of the current user thread with interrupts disabled.
#[inline(never)]
unsafe fn do_syscall_pre(syscall_num: u64) -> usize {
    if crate::ktrace_rs::category_enabled!("kernel:syscall") {
        ktrace_syscall_begin(syscall_num);
    }

    stats::inc_syscalls();

    // Re-enable interrupts to maintain kernel preemptiveness.
    // This must be done after the above `ktrace_syscall_begin` call, and after the
    // above `inc_syscalls` call as it also calls `curr_cpu_num`.
    crate::arch_rs::enable_ints();

    ProcessDispatcher::with_current(|up| up.vdso_code_address())
}

/// Shared syscall postamble that disables interrupts, ends the ktrace duration, and checks for
/// pending thread signals.
///
/// # Safety
///
/// Must be called on the syscall path of the current user thread with interrupts enabled; it
/// returns with interrupts disabled.
#[inline(never)]
unsafe fn do_syscall_post(ret: u64, syscall_num: u64) -> SyscallResult {
    ltracef_level!(2, "t {:p} ret {:#x}\n", thread::current_get(), ret);

    // Disable interrupts on the way out before checking thread signals.
    //
    // To avoid a situation where we fail to process a thread signal (the "lost wakeup" problem),
    // it's critical that once we've checked for thread signals, we do not enable interrupts until
    // we have either processed all pending signals or we have returned to user mode.
    //
    // Also, disable interrupts before the `ktrace_syscall_end` call below.
    crate::arch_rs::disable_ints();

    if crate::ktrace_rs::category_enabled!("kernel:syscall") {
        ktrace_syscall_end(syscall_num);
    }

    // The assembler caller will re-disable interrupts at the appropriate time.
    SyscallResult { status: ret, is_signaled: u64::from(thread::current_is_signaled()) }
}

/// Stamped out syscall veneer routine for every syscall. Try to maximize shared code by forcing
/// most of the setup and teardown code into non-inlined preamble and postamble code.
///
/// N.B. Interrupts must be disabled on entry and they will be disabled on exit.
/// The reason is the two calls to `curr_cpu_num` in the ktrace calls: we
/// don't want the CPU changing during the call.
///
/// `valid_pc` tests whether the offset of `pc` from the start of the vDSO code is one at which
/// the vDSO issues syscall `syscall_num`; `make_call` marshals the arguments and calls the
/// syscall's implementation.
///
/// # Safety
///
/// Must be called on the syscall path of the current user thread with interrupts disabled; it
/// returns with interrupts disabled.
#[inline(always)]
unsafe fn do_syscall(
    syscall_num: u64,
    pc: u64,
    valid_pc: impl FnOnce(usize) -> bool,
    make_call: impl FnOnce() -> u64,
) -> SyscallResult {
    // Call the shared preamble code.
    // SAFETY: The caller guarantees this is the current user thread's syscall path with
    // interrupts disabled, which is what `do_syscall_pre` requires.
    let vdso_code_address = unsafe { do_syscall_pre(syscall_num) };

    ltracef_level!(
        2,
        "t {:p} syscall {} ip/pc {:#x}\n",
        thread::current_get(),
        syscall_name_ref(syscall_num).as_c_str().to_str().unwrap_or("?"),
        pc
    );

    // Validate the user space program counter originated from the vDSO at the proper location,
    // otherwise call through to the invalid syscall handler.
    let ret = if !valid_pc((pc as usize).wrapping_sub(vdso_code_address)) {
        core::hint::cold_path();
        sys_invalid_syscall(syscall_num, pc, vdso_code_address).into_raw_return()
    } else {
        // Per syscall inlined routine to marshall args appropriately.
        make_call()
    };

    // Call through to the shared postamble code.
    // SAFETY: `do_syscall_pre` enabled interrupts and nothing since has disabled them, which is
    // what `do_syscall_post` requires.
    unsafe { do_syscall_post(ret, syscall_num) }
}

/// Called when an out of bounds syscall number is passed from user space.
///
/// # Safety
///
/// Must only be called by the architecture syscall dispatcher on the syscall path of the current
/// user thread, with interrupts disabled and `pc` the user program counter that issued the
/// syscall.
#[unsafe(no_mangle)]
unsafe extern "C" fn unknown_syscall(syscall_num: u64, pc: u64) -> SyscallResult {
    // SAFETY: The caller upholds the `do_syscall` contract: this is the syscall path of the
    // current user thread and interrupts are disabled.
    unsafe { do_syscall(syscall_num, pc, |_| false, || unreachable!()) }
}

/// Autogenerated per-syscall wrapper functions.
mod kernel_wrappers {
    include!(rustenv_path::envpath!("KERNEL_WRAPPERS_PATH"));
}

/// Tests for the syscall name table used by `kernel:syscall` trace events.
#[cfg(ktest)]
#[unittest::suite(name = "syscalls_rust")]
mod tests {
    use super::super::syscall_numbers::{SYSCALL_NAME_REFS, ZX_SYS_COUNT};
    use super::{OUT_OF_RANGE_REF, syscall_name_ref};
    use unittest::{expect_eq, expect_true};

    /// Tests that every valid syscall number maps to its own entry of the generated table.
    #[test]
    fn test_syscall_name_ref_in_range() {
        expect_eq!(SYSCALL_NAME_REFS.len(), ZX_SYS_COUNT);
        expect_true!(ZX_SYS_COUNT > 0);
        for (num, name) in SYSCALL_NAME_REFS.iter().enumerate() {
            expect_true!(core::ptr::eq(syscall_name_ref(num as u64), *name));
            expect_true!(!name.as_c_str().is_empty());
        }
        // The table interns the bare syscall name, without the `zx_` prefix, so that trace
        // labels read e.g. `channel_write`.
        expect_true!(SYSCALL_NAME_REFS.iter().any(|name| name.as_c_str() == c"channel_write"));
    }

    /// Tests that out-of-range syscall numbers map to the `[out of range]` label.
    #[test]
    fn test_syscall_name_ref_out_of_range() {
        expect_true!(core::ptr::eq(syscall_name_ref(ZX_SYS_COUNT as u64), OUT_OF_RANGE_REF));
        expect_true!(core::ptr::eq(syscall_name_ref(u64::MAX), OUT_OF_RANGE_REF));
        expect_true!(syscall_name_ref(u64::MAX).as_c_str() == c"[out of range]");
    }
}
