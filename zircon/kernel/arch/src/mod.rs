// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(target_arch = "aarch64")]
#[path = "../arm64/src/mod.rs"]
pub mod arch_arm64;

#[cfg(target_arch = "aarch64")]
#[allow(unused_imports)]
pub use arch_arm64::{self as arm64, *};

#[cfg(target_arch = "riscv64")]
#[path = "../riscv64/src/mod.rs"]
pub mod arch_riscv64;

#[cfg(target_arch = "riscv64")]
#[allow(unused_imports)]
pub use arch_riscv64::{self as riscv64, *};

#[cfg(target_arch = "x86_64")]
#[path = "../x86/src/mod.rs"]
pub mod arch_x86;

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)]
pub use arch_x86::{self as x86, *};

pub use arch_types_bindings::{GeneralRegsSource, UserEntryState};

#[macro_use]
pub mod api;

pub mod debugger;
pub use debugger::{get_instruction_pointer, set_return_instruction_pointer};

pub mod exception;
pub use exception::{
    ArchExceptionContext, arch_dump_exception_context, arch_fill_in_exception_context,
    arch_install_exception_context, arch_remove_exception_context, dispatch_user_policy_exception,
};

pub mod ops;
pub use ops::{enter_idle_state, flush_state_and_halt, restore_user_state, save_user_state};

#[cfg(target_arch = "aarch64")]
assert_arch_signatures!(arm64);

#[cfg(target_arch = "riscv64")]
assert_arch_signatures!(riscv64);
#[cfg(target_arch = "x86_64")]
assert_arch_signatures!(x86);

use crate::kernel::percpu::PerCpu;
use crate::kernel::types::cpu_num_t;
use zx_status::Status;

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_arch_blocking_disallowed() -> bool;
    fn cpp_arch_ints_disabled() -> bool;
    fn cpp_arch_disable_ints();
    fn cpp_arch_enable_ints();
    fn cpp_arch_interrupt_save() -> InterruptSavedState;
    fn cpp_arch_interrupt_restore(state: InterruptSavedState);
    fn cpp_arch_max_num_cpus() -> u32;
    fn cpp_arch_set_blocking_disallowed(value: bool);
    fn cpp_arch_set_restricted_flag(restricted: bool);
    // `PerCpu` contains ZST fields which trigger `improper_ctypes`, even though `&'static PerCpu`
    // is a thin pointer with identical ABI to `void*` and `PerCpu`'s layout is statically asserted
    // against C++ `struct percpu`.
    #[allow(improper_ctypes)]
    fn cpp_arch_get_curr_percpu() -> &'static PerCpu;
    fn cpp_with_frame_pointers() -> bool;
    #[cfg(target_arch = "x86_64")]
    fn arch_setup_percpu(cpu_num: cpu_num_t, percpu: *mut PerCpu);
}

/// Sets the architecture-specific restricted mode flag on the current CPU.
#[inline(always)]
pub fn set_restricted_flag(restricted: bool) {
    // SAFETY: Foreign function call into architecture-specific restricted mode flag setting.
    unsafe { cpp_arch_set_restricted_flag(restricted) }
}

/// Returns the architecture-specific restricted mode flag on the current CPU
/// (`arch_get_restricted_flag`).
///
/// Call with interrupts or preemption disabled so the calling thread cannot migrate across CPUs
/// and observe a stale flag.
///
/// # Safety
///
/// The calling CPU's per-CPU pointer register (`x20` on arm64, `s11` on riscv64, `%gs.base` on
/// x86_64) must already be initialized to a valid per-CPU structure (i.e., after early per-CPU
/// initialization).
#[inline(always)]
pub unsafe fn get_restricted_flag() -> bool {
    // SAFETY: Caller guarantees per-CPU state is initialized on the calling CPU.
    unsafe { arch_get_restricted_flag() }
}

/// Returns `true` if the kernel was compiled with frame pointers enabled (`WITH_FRAME_POINTERS`).
#[inline(always)]
pub fn with_frame_pointers() -> bool {
    // SAFETY: `cpp_with_frame_pointers` is a pure compile-time configuration query with no
    // preconditions.
    unsafe { cpp_with_frame_pointers() }
}

/// The arch_blocking_disallowed() flag is used to check that in-kernel interrupt
/// handlers do not do any blocking operations.  This is a per-CPU flag.
/// Various blocking operations, such as mutex.Acquire(), contain assertions
/// that arch_blocking_disallowed() is false.
///
/// arch_blocking_disallowed() should only be true when interrupts are
/// disabled.
#[inline(always)]
pub fn blocking_disallowed() -> bool {
    unsafe { cpp_arch_blocking_disallowed() }
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    unsafe { cpp_arch_ints_disabled() }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    unsafe { cpp_arch_disable_ints() }
}

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    unsafe { cpp_arch_enable_ints() }
}

/// The saved interrupt state, representing architecture-specific interrupt flags.
#[cfg(target_arch = "x86_64")]
#[must_use = "saved interrupt state must be passed to a matching restore"]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptSavedState(usize);

/// The saved interrupt state, representing architecture-specific interrupt flags.
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
#[must_use = "saved interrupt state must be passed to a matching restore"]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptSavedState(bool);

// Must match `kExpectedInterruptSavedStateSize` and `kExpectedInterruptSavedStateAlign` in
// `zircon/kernel/arch/arch_ffi.cc`, which check `interrupt_saved_state_t`.
#[cfg(target_arch = "x86_64")]
zr::static_assert_size_and_align!(InterruptSavedState, 8, 8);
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
zr::static_assert_size_and_align!(InterruptSavedState, 1, 1);

/// Save the current interrupt state (specifically, whether interrupts are enabled)
/// and disable interrupts on the current CPU.
#[inline(always)]
pub fn arch_interrupt_save() -> InterruptSavedState {
    unsafe { cpp_arch_interrupt_save() }
}

/// Restore the interrupt state on the current CPU to a previously saved state.
#[inline(always)]
pub fn arch_interrupt_restore(state: InterruptSavedState) {
    unsafe { cpp_arch_interrupt_restore(state) }
}

/// A guard that disables interrupts on the current CPU when created,
/// and restores the previous interrupt state when dropped.
pub struct InterruptDisableGuard {
    state: InterruptSavedState,
}

impl InterruptDisableGuard {
    #[inline(always)]
    pub fn new() -> Self {
        Self { state: arch_interrupt_save() }
    }
}

impl Drop for InterruptDisableGuard {
    #[inline(always)]
    fn drop(&mut self) {
        arch_interrupt_restore(self.state);
    }
}

/// Returns the CPU number of the calling CPU.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub fn curr_cpu_num() -> u32 {
    arch_curr_cpu_num()
}

/// Returns the maximum number of CPUs in the system.
#[inline(always)]
pub fn max_num_cpus() -> u32 {
    unsafe { cpp_arch_max_num_cpus() }
}

/// Return a reference to the high-level percpu struct for the calling CPU.
#[inline(always)]
pub fn get_curr_percpu() -> &'static PerCpu {
    // SAFETY: `cpp_arch_get_curr_percpu` returns a valid, non-null, 64-byte-aligned `'static`
    // `PerCpu` pointer for the calling CPU (initialized during boot before Rust callers run), and
    // all mutable fields of `PerCpu` use interior mutability (`UnsafeCell` / atomics).
    unsafe { cpp_arch_get_curr_percpu() }
}

/// Setup the high-level percpu struct pointer for `cpu_num`.
///
/// # Safety
///
/// - `cpu_num` must identify the calling CPU and be less than `SMP_MAX_CPUS` with interrupts
///   disabled.
/// - `percpu` must point to a valid, initialized `'static` [`PerCpu`] instance dedicated to
///   `cpu_num`.
#[inline]
pub unsafe fn setup_percpu(cpu_num: cpu_num_t, percpu: *mut PerCpu) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees `cpu_num` identifies the calling CPU (< `SMP_MAX_CPUS`) with
        // interrupts disabled and `percpu` points to its initialized `'static` `PerCpu` instance.
        unsafe { arch_arm64::mp::arch_setup_percpu(cpu_num, percpu) };
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `cpu_num` identifies the calling CPU (< `SMP_MAX_CPUS`) with
        // interrupts disabled and `percpu` points to its initialized `'static` `PerCpu` instance.
        unsafe { arch_riscv64::mp::arch_setup_percpu(cpu_num, percpu) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller guarantees `cpu_num` identifies the calling CPU (< `SMP_MAX_CPUS`) with
        // interrupts disabled and `percpu` points to its initialized `'static` `PerCpu` instance.
        unsafe { arch_setup_percpu(cpu_num, percpu) };
    }
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`.
///
/// # Panics
///
/// Panics if any `lockdep`-tracked locks are held.
///
/// # Safety
///
/// Caller must ensure that the current CPU per-CPU and thread context are initialized, blocking is
/// allowed, no spinlocks are held, and `dst` is valid for writes of `len` bytes in kernel memory
/// without concurrent conflicting accesses.
#[inline(always)]
pub unsafe fn arch_copy_from_user(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    len: usize,
) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees valid pointers and safety invariants.
        unsafe { arm64::arch_copy_from_user(dst, src, len) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the `arch_copy_from_user` safety preconditions.
        unsafe { riscv64::arch_copy_from_user(dst, src, len) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the `arch_copy_from_user` safety preconditions.
        unsafe { x86::arch_copy_from_user(dst, src, len) }
    }
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`.
///
/// # Panics
///
/// Panics if any `lockdep`-tracked locks are held.
///
/// # Safety
///
/// Caller must ensure that the current CPU per-CPU and thread context are initialized, blocking is
/// allowed, no spinlocks are held, and `src` is valid for reads of `len` initialized bytes in
/// kernel memory without concurrent conflicting mutations.
#[inline(always)]
pub unsafe fn arch_copy_to_user(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    len: usize,
) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees valid pointers and safety invariants.
        unsafe { arm64::arch_copy_to_user(dst, src, len) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the `arch_copy_to_user` safety preconditions.
        unsafe { riscv64::arch_copy_to_user(dst, src, len) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the `arch_copy_to_user` safety preconditions.
        unsafe { x86::arch_copy_to_user(dst, src, len) }
    }
}

/// Page fault information captured during a user copy operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultInfo {
    pub pf_va: usize,
    pub pf_flags: u32,
}

/// Error type returned by user copy routines that capture page faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserCopyCaptureFaultsError {
    pub status: Status,
    pub fault_info: Option<FaultInfo>,
}

impl From<Status> for UserCopyCaptureFaultsError {
    #[inline(always)]
    fn from(status: Status) -> Self {
        Self { status, fault_info: None }
    }
}

/// Stores the context in which the user copy is being invoked.
#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CopyContext {
    /// The caller's context allows blocking. When this option is used, the fault handler that
    /// runs during the copy may resolve access faults (on architectures that have explicit
    /// access faults), which may acquire mutexes and therefore may block the calling thread.
    ///
    /// It is an error to use this option while holding a spinlock.
    #[default]
    BlockingAllowed = 0,

    /// The caller's context does not allow blocking. When this option is used, the fault
    /// handler that runs during the copy will not acquire any mutexes. As a side effect, this
    /// means that any and all faults will be captured and returned to the calling thread.
    ///
    /// It is safe to use this option when holding a spinlock.
    BlockingNotAllowed = 1,
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`, capturing any page
/// faults.
///
/// # Safety
///
/// Caller must ensure that the current CPU per-CPU and thread context are initialized, `dst`
/// is valid for writes of `len` bytes in kernel memory without concurrent conflicting
/// accesses, and `context` is not [`CopyContext::BlockingAllowed`] while holding a spinlock.
#[inline(always)]
pub unsafe fn arch_copy_from_user_capture_faults(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees valid pointers and safety invariants.
        unsafe { arm64::arch_copy_from_user_capture_faults(dst, src, len, context) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the `arch_copy_from_user_capture_faults` safety preconditions.
        unsafe { riscv64::arch_copy_from_user_capture_faults(dst, src, len, context) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the `arch_copy_from_user_capture_faults` safety preconditions.
        unsafe { x86::arch_copy_from_user_capture_faults(dst, src, len, context) }
    }
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`, capturing any page
/// faults.
///
/// # Safety
///
/// Caller must ensure that the current CPU per-CPU and thread context are initialized, `src`
/// is valid for reads of `len` initialized bytes in kernel memory without concurrent
/// conflicting mutations, and `context` is not [`CopyContext::BlockingAllowed`] while holding a
/// spinlock.
#[inline(always)]
pub unsafe fn arch_copy_to_user_capture_faults(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    len: usize,
    context: CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees valid pointers and safety invariants.
        unsafe { arm64::arch_copy_to_user_capture_faults(dst, src, len, context) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the `arch_copy_to_user_capture_faults` safety preconditions.
        unsafe { riscv64::arch_copy_to_user_capture_faults(dst, src, len, context) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the `arch_copy_to_user_capture_faults` safety preconditions.
        unsafe { x86::arch_copy_to_user_capture_faults(dst, src, len, context) }
    }
}

/// Sets whether the current thread is allowed to block.
#[inline(always)]
pub fn set_blocking_disallowed(value: bool) {
    unsafe { cpp_arch_set_blocking_disallowed(value) }
}

pub use zx_types::ZX_TLS_STACK_GUARD_OFFSET;

/// Returns the current compiler thread pointer (`tpidr_el1` on arm64, `tp` on riscv64, per-CPU
/// `x86_percpu` pointer via `%gs:0` on x86_64).
///
/// Reads the register via inline assembly rather than `__builtin_thread_pointer()` because
/// `llvm.thread.pointer` is `memory(none)` and would be CSE'd across `thread_init_early()` when
/// `lk_main` updates the early-boot stack guard before installing the initial `Thread`.
///
/// On `x86_64`, because this returns the current CPU's per-CPU pointer rather than a per-thread
/// pointer, dereferencing the returned pointer requires interrupts or preemption/CPU migration to
/// be disabled.
#[inline(always)]
pub fn get_current_compiler_thread_pointer() -> *mut u8 {
    let tp: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: Reads `tpidr_el1`, the kernel compiler thread-pointer register on arm64.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) tp, options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: Reads `tp`, the kernel compiler thread-pointer register on riscv64.
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp, options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: In kernel mode on x86_64, `%gs:0` (`x86_percpu::direct`) is a valid 64-bit self
    // pointer once `post_handoff_bootstrap` / secondary per-CPU setup has initialized `%gs.base`.
    unsafe {
        tp = x86::x86::x86_read_gs_offset64::<0>() as usize;
    }
    core::ptr::with_exposed_provenance_mut(tp)
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod sys {
    unsafe extern "C" {
        #[cfg(target_arch = "x86_64")]
        pub fn arch_mp_send_ipi(
            target: crate::kernel::mp::MpIpiTarget,
            mask: crate::kernel::types::cpu_mask_t,
            ipi: crate::kernel::mp::MpIpi,
        );
        #[cfg(target_arch = "x86_64")]
        pub fn arch_mp_reschedule(mask: crate::kernel::types::cpu_mask_t);
        #[cfg(target_arch = "x86_64")]
        pub fn arch_mp_cpu_hotplug(
            cpu_id: crate::kernel::types::cpu_num_t,
        ) -> zx_types::zx_status_t;
        #[cfg(target_arch = "aarch64")]
        pub fn platform_mp_prep_cpu_unplug(
            cpu_id: crate::kernel::types::cpu_num_t,
        ) -> zx_types::zx_status_t;
        #[cfg(target_arch = "aarch64")]
        pub fn platform_mp_cpu_unplug(
            cpu_id: crate::kernel::types::cpu_num_t,
        ) -> zx_types::zx_status_t;
    }
}

/// Performs minimal architecture-specific boot CPU setup right after taking over from physboot.
///
/// # Safety
///
/// Must be called once on the boot CPU during `post_handoff_bootstrap` with interrupts disabled,
/// before per-CPU state or the trampoline mapping is torn down/used, and with a valid
/// `arch_handoff` pointer from physboot.
#[inline(always)]
pub unsafe fn post_handoff_bootstrap(arch_handoff: *const ::handoff::ArchPhysHandoff) {
    #[cfg(target_arch = "aarch64")]
    {
        let _ = arch_handoff;
        // SAFETY: Forwarded from `post_handoff_bootstrap` on the boot CPU.
        unsafe { arm64::arch::post_handoff_bootstrap() };
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Forwarded from `post_handoff_bootstrap` on the boot CPU with a valid
        // `arch_handoff`.
        unsafe { riscv64::arch::post_handoff_bootstrap(arch_handoff.cast()) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        let _ = arch_handoff;
        // SAFETY: Forwarded from `post_handoff_bootstrap` on CPU 0 at CPL 0 before `%gs.base` is
        // switched from physboot's temporary per-CPU ABI region.
        unsafe { x86::handoff::post_handoff_bootstrap() };
    }
}

/// Sends an inter-processor interrupt (IPI) to the specified target CPUs.
///
/// Note: Calling with [`crate::kernel::mp::MpIpiTarget::AllButLocal`] requires interrupts to be
/// disabled on the calling CPU.
#[inline(always)]
pub fn mp_send_ipi(
    target: crate::kernel::mp::MpIpiTarget,
    mask: crate::kernel::types::cpu_mask_t,
    ipi: crate::kernel::mp::MpIpi,
) {
    #[cfg(target_arch = "aarch64")]
    {
        arm64::mp::arch_mp_send_ipi(target, mask, ipi);
    }
    #[cfg(target_arch = "riscv64")]
    {
        riscv64::mp::arch_mp_send_ipi(target, mask, ipi);
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Foreign function call with value arguments.
        unsafe { sys::arch_mp_send_ipi(target, mask, ipi) };
    }
}

pub use mp_send_ipi as arch_mp_send_ipi;

/// Reschedules tasks on the CPUs specified by `mask`.
#[inline(always)]
pub fn mp_reschedule(mask: crate::kernel::types::cpu_mask_t) {
    #[cfg(target_arch = "aarch64")]
    {
        arm64::mp::arch_mp_reschedule(mask);
    }
    #[cfg(target_arch = "riscv64")]
    {
        riscv64::mp::arch_mp_reschedule(mask);
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `arch_mp_reschedule` masks `mask` to `< x86_num_cpus` before indexing
        // `bp_percpu` / `ap_percpus`.
        unsafe { sys::arch_mp_reschedule(mask) };
    }
}

/// Asks the architecture layer to bring a CPU up and enter it into the scheduler.
///
/// # Safety
///
/// The caller must serialize this with every other hotplug and unplug of `cpu_id`, and `cpu_id`
/// must be fully stopped (not still booting and not partway through halting).
#[inline(always)]
pub unsafe fn mp_cpu_hotplug(cpu_id: crate::kernel::types::cpu_num_t) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds the serialization and stopped-CPU preconditions.
        unsafe { arm64::mp::arch_mp_cpu_hotplug(cpu_id) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the serialization and stopped-CPU preconditions.
        unsafe { riscv64::mp::arch_mp_cpu_hotplug(cpu_id) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the serialization and stopped-CPU preconditions.
        Status::ok(unsafe { sys::arch_mp_cpu_hotplug(cpu_id) })
    }
}

/// Prepares the platform for CPU unplug.
#[inline(always)]
pub fn platform_mp_prep_cpu_unplug(cpu_id: crate::kernel::types::cpu_num_t) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Foreign function call with value argument.
        Status::ok(unsafe { sys::platform_mp_prep_cpu_unplug(cpu_id) })
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::platform_mp_prep_cpu_unplug(cpu_id)
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::platform::platform_mp_prep_cpu_unplug(cpu_id)
    }
}

/// Shuts down the specified CPU at the platform level after it is no longer scheduled on.
///
/// # Safety
///
/// `cpu_id` must not be the calling CPU and must already be offline/halted and no longer scheduled
/// on.
#[inline(always)]
pub unsafe fn platform_mp_cpu_unplug(
    cpu_id: crate::kernel::types::cpu_num_t,
) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees `cpu_id` is not the calling CPU and is already offline/halted.
        Status::ok(unsafe { sys::platform_mp_cpu_unplug(cpu_id) })
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::platform::platform_mp_cpu_unplug(cpu_id)
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller guarantees `cpu_id` is not the calling CPU and is already offline/halted
        // and no longer scheduled on.
        unsafe { crate::platform_pc::platform::platform_mp_cpu_unplug(cpu_id) }
    }
}
