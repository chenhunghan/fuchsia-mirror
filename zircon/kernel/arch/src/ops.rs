// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::mp::MpUnplugEvent;
use crate::kernel::types::VAddr;

/// VM feature bit indicating that execute-only memory (XOM) mappings are supported.
pub const ZX_VM_FEATURE_CAN_MAP_XOM: u32 = 1 << 0;

unsafe extern "C" {
    fn arch_zero_page(va: *mut core::ffi::c_void);
    fn arch_clean_cache_range(start: usize, len: usize);
    fn cpp_arch_vm_features() -> u32;
    fn cpp_arch_cpu_features() -> u32;
    fn cpp_arch_cpu_sve_features() -> u32;
    fn cpp_arch_dcache_line_size() -> u32;
    fn cpp_arch_icache_line_size() -> u32;
}

// These are implemented in C++ on x86; arm64 and riscv64 call the Rust implementations directly.
#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn arch_address_tagging_features() -> u32;
}

/// Arch optimized version of a page zero routine against a page aligned buffer.
/// Usually implemented in or called from assembly.
///
/// # Safety
///
/// Caller knows it is safe to zero the given virtual address.
pub unsafe fn zero_page(va: VAddr) {
    // SAFETY: it is safe to zero the given address.
    unsafe {
        arch_zero_page(core::ptr::with_exposed_provenance_mut(va.0));
    }
}

/// Returns the architectural CPU feature flags (`ZX_ARM64_FEATURE_*` / `ZX_X86_64_FEATURE_*` /
/// `ZX_RISCV64_FEATURE_*`).
#[inline(always)]
pub fn cpu_features() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_cpu_features()` in arch_ffi.cc,
    // which has no preconditions.
    unsafe { cpp_arch_cpu_features() }
}

/// Returns the architectural SVE CPU feature flags (`ZX_ARM64_FEATURE_ISA_SVE*`, or 0 on
/// non-ARM64).
#[inline(always)]
pub fn cpu_sve_features() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_cpu_sve_features()` in
    // arch_ffi.cc, which has no preconditions.
    unsafe { cpp_arch_cpu_sve_features() }
}

/// Returns the number of hardware breakpoint registers available on the system.
#[inline(always)]
pub fn get_hw_breakpoint_count() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        super::x86::debugger::get_hw_breakpoint_count()
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arm64_hw_breakpoint_count()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::debugger::arch_get_hw_breakpoint_count()
    }
}

/// Returns the number of hardware watchpoint registers available on the system.
#[inline(always)]
pub fn get_hw_watchpoint_count() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        super::x86::debugger::get_hw_watchpoint_count()
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arm64_hw_watchpoint_count()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::debugger::arch_get_hw_watchpoint_count()
    }
}

/// Returns the architectural address tagging feature flags (`ZX_ARM64_FEATURE_ADDRESS_TAGGING_*`).
#[inline(always)]
pub fn address_tagging_features() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The declaration matches the C++ `uint32_t arch_address_tagging_features()`,
        // which has no preconditions.
        unsafe { arch_address_tagging_features() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arch_address_tagging_features()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::mmu::arch_address_tagging_features()
    }
}

/// Returns the architectural data cache line size in bytes.
#[inline(always)]
pub fn dcache_line_size() -> u32 {
    // SAFETY: `cpp_arch_dcache_line_size` is a pure query of the architectural cache line size.
    unsafe { cpp_arch_dcache_line_size() }
}

/// Returns the architectural instruction cache line size in bytes.
#[inline(always)]
pub fn icache_line_size() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_icache_line_size()` in
    // arch_ffi.cc, which has no preconditions.
    unsafe { cpp_arch_icache_line_size() }
}

/// Cleans the data cache for `[start, start + len)`.
///
/// # Safety
///
/// `[start, start + len)` must be a valid mapped kernel address range for the duration of the call.
#[inline(always)]
pub unsafe fn clean_cache_range(start: *const u8, len: usize) {
    // SAFETY: Caller guarantees `[start, start + len)` is a valid mapped kernel address range.
    // Exposing provenance allows the C/assembly implementation to cast the integer address back to
    // a pointer and observe prior stores.
    unsafe {
        arch_clean_cache_range(start.expose_provenance(), len);
    }
}

/// Returns the architecture-specific VM feature flags (`ZX_VM_FEATURE_*`).
#[inline(always)]
pub fn arch_vm_features() -> u32 {
    // SAFETY: `cpp_arch_vm_features` is a pure query of architectural VM features.
    unsafe { cpp_arch_vm_features() }
}

/// Returns the number of spinlocks currently held on the calling CPU.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub fn num_spinlocks_held() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        super::x86::mp::num_spinlocks_held()
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::mp::num_spinlocks_held()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::mp::num_spinlocks_held()
    }
}

/// Enters the architecture-specific processor idle state.
///
/// When called from `IdlePowerThread::run`, the rules are:
/// 1. Preemption *must* be disabled.
/// 2. Interrupts *must* be disabled.
/// 3. Preemption will *not* be re-enabled at any point during [`enter_idle_state`].
/// 4. Interrupts *may* be re-enabled as needed by the architecture-specific implementation, but
///    this is not guaranteed to happen. If they are re-enabled, they will be disabled again before
///    the function returns.
/// 5. If an implementation needs to re-enable interrupts during its sequence, it will *always* do
///    so just before falling into the instruction which waits for the interrupt intended to kick
///    the system out of idle. IOW - if the final entry into the idle-but-waiting-for-irq state
///    requires that interrupts be re-enabled, the operation can be considered to be "atomic" from
///    the caller's perspective. The wakeup interrupt will not be missed because of a race.
///
/// For callers, the primary takeaway from these rules is that users *must* disable IRQs, but *must
/// not* rely on interrupts being disabled for the entire time to preserve any of their invariants.
///
/// # Safety
///
/// Must be called on the calling CPU's idle/power thread with both interrupts and preemption
/// disabled on the calling CPU.
#[inline(always)]
pub unsafe fn enter_idle_state() {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the idle-thread, interrupt-disabled, and preempt-disabled
        // invariants.
        unsafe { super::x86::arch_enter_idle_state() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds the idle-thread, interrupt-disabled, and preempt-disabled
        // invariants.
        unsafe { super::arm64::arch_enter_idle_state() }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the idle-thread, interrupt-disabled, and preempt-disabled
        // invariants.
        unsafe { super::riscv64::arch_enter_idle_state() }
    }
}

/// Flushes architectural state on the calling CPU and halts it, signaling `flush_done`.
///
/// # Safety
///
/// Must be called on the unplugging CPU's idle/power thread with interrupts disabled, an
/// initialized current `Thread`, no spinlocks or active `ChainLockTransaction` held, and a valid
/// pointer `flush_done` to an initialized [`MpUnplugEvent`]. Once `flush_done` is signaled inside
/// this diverging call, the waiting CPU may immediately wake and tear down or reuse the storage
/// backing `flush_done`, so `flush_done` must not be accessed after signaling.
#[inline(always)]
pub unsafe fn flush_state_and_halt(flush_done: *mut MpUnplugEvent) -> ! {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds the unplugging-CPU, interrupt-disabled, lock-free, and valid
        // `flush_done` event invariants.
        unsafe { super::x86::mp::flush_state_and_halt(flush_done) }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds the unplugging-CPU, interrupt-disabled, lock-free, and valid
        // `flush_done` event invariants.
        unsafe { super::arm64::arch_flush_state_and_halt(flush_done) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds the unplugging-CPU, interrupt-disabled, lock-free, and valid
        // `flush_done` event invariants.
        unsafe { super::riscv64::arch_flush_state_and_halt(flush_done) }
    }
}

/// Initializes the architectural state of `thread` so that its first context switch enters
/// `entry_point` at the top of its stack (`arch_thread_initialize`).
///
/// # Safety
///
/// The caller must ensure that:
/// - Early CPU 0 architectural initialization (including `x86::registers::extended_register_init`
///   on x86-64) has completed.
/// - `thread` points to a live (not freed), valid [`Thread`](crate::kernel::thread::Thread) that is
///   not currently running, and to which the caller has exclusive access (holding the thread's
///   lock, or owning the unpublished thread while constructing it).
/// - `thread` has an initialized `KernelStack` whose top (`get_stack_top(thread)`) is 16-byte
///   aligned with sufficient writable space below `stack_top` for the initial context-switch frame
///   (at least 64 bytes on x86-64, plus a 16-byte-aligned `unsafe_sp` when safe-stack is enabled
///   and an initialized shadow-call stack where applicable), and `entry_point` is a valid thread
///   trampoline that never returns.
#[inline(always)]
pub unsafe fn thread_initialize(thread: *mut crate::kernel::thread::Thread, entry_point: usize) {
    // SAFETY: Caller guarantees early CPU 0 architectural initialization has completed, `thread` is
    // a live, non-running `Thread` with exclusive access and an initialized, aligned kernel stack,
    // and `entry_point` is a valid non-returning trampoline.
    unsafe { super::arch_thread_initialize(thread.cast(), entry_point) }
}

/// Moves the calling CPU from its early-boot fake thread state to `thread`, the first real
/// [`Thread`](crate::kernel::thread::Thread) on the calling CPU (`arch_thread_construct_first`).
///
/// # Safety
///
/// The caller must ensure that:
/// - Interrupts are disabled on the calling CPU (`ints_disabled()`).
/// - On x86-64, `%gs.base` is initialized to the calling CPU's `x86_percpu`; on ARM64 and RISC-V,
///   the thread pointer still points to the early-boot fake `arch_thread` (or `thread` itself on
///   RISC-V secondary CPUs).
/// - `thread` points to a live, newly constructed [`Thread`](crate::kernel::thread::Thread)
///   dedicated to the calling CPU (not concurrently running on another CPU) that remains live for
///   as long as it stays installed as the current thread.
#[inline(always)]
pub unsafe fn thread_construct_first(thread: *mut crate::kernel::thread::Thread) {
    // SAFETY: Caller guarantees interrupts are disabled, early per-CPU/thread-pointer state is
    // valid for this CPU, and `thread` is a live, newly constructed `Thread` dedicated to this CPU.
    unsafe { super::arch_thread_construct_first(thread.cast()) }
}

/// Dumps architecture-specific state for `thread` (`arch_dump_thread`).
///
/// # Safety
///
/// `thread` must point to a valid, live, initialized [`Thread`](crate::kernel::thread::Thread).
/// The caller must either hold `thread`'s lock or be in a kernel panic, where the reads are best
/// effort and may observe concurrent updates.
#[inline(always)]
pub unsafe fn dump_thread(thread: *const crate::kernel::thread::Thread) {
    // SAFETY: Caller guarantees `thread` points to a valid, live, initialized `Thread` and either
    // holds `thread`'s lock or is in a kernel panic.
    unsafe { super::arch_dump_thread(thread) }
}

/// Returns the saved frame pointer of a blocked `thread` (`arch_thread_get_blocked_fp`).
///
/// # Safety
///
/// `thread` must point to an initialized [`Thread`](crate::kernel::thread::Thread) in a blocked,
/// sleeping, or suspended state with its saved architecture register state valid under `thread`'s
/// lock (`thread.lock()`).
#[inline(always)]
pub unsafe fn thread_get_blocked_fp(thread: *const crate::kernel::thread::Thread) -> usize {
    // SAFETY: Caller guarantees `thread` is an initialized blocked/sleeping/suspended `Thread` with
    // saved register state valid under `thread`'s lock for `arch_thread_get_blocked_fp`.
    unsafe { super::arch_thread_get_blocked_fp(thread) }
}

/// Saves the current user thread's non-general-purpose architectural user register state (FPU,
/// vector, debug registers, etc.) into `thread`'s `arch_thread` (`arch_save_user_state`).
///
/// # Safety
///
/// - Interrupts must be disabled (`ints_disabled()`).
/// - `thread` must point to the valid, currently executing user
///   [`Thread`](crate::kernel::thread::Thread) (`thread == current_get()`,
///   `is_user_thread(thread)`) with initialized architectural user-state storage, and the caller
///   must hold `thread`'s lock (`thread.lock()`).
#[inline(always)]
pub unsafe fn save_user_state(thread: *mut crate::kernel::thread::Thread) {
    // SAFETY: Caller guarantees interrupts are disabled and `thread` is the valid, currently
    // executing user `Thread` with initialized architectural user-state storage and its lock held.
    unsafe { super::arch_save_user_state(thread.cast()) }
}

/// Restores the current user thread's non-general-purpose architectural user register state (FPU,
/// vector, debug registers, etc.) from `thread`'s `arch_thread` (`arch_restore_user_state`).
///
/// # Safety
///
/// - Interrupts must be disabled (`ints_disabled()`).
/// - `thread` must point to the valid, currently executing user
///   [`Thread`](crate::kernel::thread::Thread) (`thread == current_get()`,
///   `is_user_thread(thread)`) with initialized architectural user-state storage whose user
///   register state was previously saved by [`save_user_state`], and the caller must hold
///   `thread`'s lock (`thread.lock()`).
#[inline(always)]
pub unsafe fn restore_user_state(thread: *mut crate::kernel::thread::Thread) {
    // SAFETY: Caller guarantees interrupts are disabled and `thread` is the valid, currently
    // executing user `Thread` with previously saved user register state and its lock held.
    unsafe { super::arch_restore_user_state(thread.cast()) }
}

/// Records `gregs` (with representation `source`) as `thread`'s suspended general-purpose registers
/// (`arch_set_suspended_general_regs`).
///
/// # Safety
///
/// - `thread` must point to a valid [`Thread`](crate::kernel::thread::Thread) with exclusive access
///   to its `arch.suspended_general_regs` field (with `thread.lock()` held).
/// - `source` must be a valid non-`None` [`super::GeneralRegsSource`] supported by the target
///   architecture (`Iframe` or `Syscall`), and `gregs` must point to a valid general-register frame
///   matching `source` that remains valid until [`reset_suspended_general_regs`] is called or the
///   thread exits.
#[inline(always)]
pub unsafe fn set_suspended_general_regs(
    thread: *mut crate::kernel::thread::Thread,
    source: super::GeneralRegsSource,
    gregs: *mut core::ffi::c_void,
) {
    // SAFETY: Caller guarantees `thread` is valid with its lock held, and `gregs` is a valid frame
    // matching `source` that outlives the suspended-general-regs registration.
    unsafe { super::arch_set_suspended_general_regs(thread.cast(), source, gregs) }
}

/// Clears `thread`'s suspended general-purpose register pointer previously installed by
/// [`set_suspended_general_regs`] (`arch_reset_suspended_general_regs`).
///
/// # Safety
///
/// `thread` must point to a valid [`Thread`](crate::kernel::thread::Thread) with exclusive access
/// to its `arch.suspended_general_regs` field (with `thread.lock()` held).
#[inline(always)]
pub unsafe fn reset_suspended_general_regs(thread: *mut crate::kernel::thread::Thread) {
    // SAFETY: Caller guarantees `thread` is valid with its lock held.
    unsafe { super::arch_reset_suspended_general_regs(thread.cast()) }
}
