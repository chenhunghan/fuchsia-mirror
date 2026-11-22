// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

use zx_status::Status;
use zx_types::zx_status_t;

use crate::kernel::thread::Thread;

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn inp(port: u16) -> u8 {
    let value: u8;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn inpw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in ax, dx",
            in("dx") port,
            out("ax") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Reading from the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn inpd(port: u16) -> u32 {
    let value: u32;
    // SAFETY: Input from CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn outp(port: u16, value: u8) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn outpw(port: u16, value: u16) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that:
/// 1. The I/O port address `port` is valid and mapped on the system.
/// 2. Writing `value` to the port does not cause side effects that violate memory safety or system
///    stability.
pub unsafe fn outpd(port: u16, value: u32) {
    // SAFETY: Output to CPU I/O port as requested by the caller.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads a 64-bit Model Specific Register (MSR).
///
/// # Safety
///
/// The caller must ensure that `msr_id` is a valid MSR supported by the current CPU. Reading an
/// unsupported MSR will cause a General Protection Fault (#GP).
#[inline(always)]
pub unsafe fn read_msr(msr_id: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: rdmsr reads the MSR specified in ecx into edx:eax. Caller guarantees `msr_id` is
    // valid.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr_id,
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Reads the low 32 bits of a Model Specific Register (MSR).
///
/// # Safety
///
/// The caller must ensure that `msr_id` is a valid MSR supported by the current CPU. Reading an
/// unsupported MSR will cause a General Protection Fault (#GP).
#[inline(always)]
pub unsafe fn read_msr32(msr_id: u32) -> u32 {
    let low: u32;
    // SAFETY: rdmsr reads the MSR specified in ecx into edx:eax. Caller guarantees `msr_id` is
    // valid.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr_id,
            out("eax") low,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    low
}

/// Writes a 64-bit value to a Model Specific Register (MSR).
///
/// # Safety
///
/// The caller must ensure that `msr_id` is a valid MSR supported by the current CPU and that `val`
/// contains valid configuration bits for that MSR. Writing invalid values or writing to an
/// unsupported MSR will cause a General Protection Fault (#GP) or undefined behavior.
#[inline(always)]
pub unsafe fn write_msr(msr_id: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    // SAFETY: wrmsr writes edx:eax to the MSR specified in ecx. Caller guarantees `msr_id` and
    // `val` are valid.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr_id,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags),
        );
    }
}

mod ffi {
    use super::*;

    // Implemented in assembly (`ops.S`).
    unsafe extern "C" {
        /// Reads MSR `msr_id` into `val` with `#GP` fault recovery (`read_msr_safe`).
        ///
        /// # Safety
        ///
        /// `val` must point to a live (not freed) object properly aligned for `u64` and valid for
        /// writes for the duration of the call, and reading `msr_id` must not trigger unwanted
        /// hardware side effects.
        pub(super) fn read_msr_safe(msr_id: u32, val: *mut u64) -> zx_status_t;
        /// Writes `val` into MSR `msr_id` with `#GP` fault recovery (`write_msr_safe`).
        ///
        /// # Safety
        ///
        /// Writing to `msr_id` must not violate system invariants or trigger unwanted hardware side
        /// effects.
        pub(super) fn write_msr_safe(msr_id: u32, val: u64) -> zx_status_t;

        /// Arms the CPU's `MWAIT` address monitor at `addr`.
        ///
        /// # Safety
        ///
        /// `CPL == 0` must hold, `MONITOR` must be enabled on the current logical processor, and
        /// `addr` must point to canonical, readable Write-Back (`WB`) memory.
        pub(super) fn x86_monitor(addr: *const core::ffi::c_void);

        /// Enables maskable interrupts and enters `MWAIT` with `hints` (`sti; mwait`).
        ///
        /// # Safety
        ///
        /// `CPL == 0` and `RFLAGS.IF == 0` must hold on entry with `MONITOR`/`MWAIT` enabled,
        /// enabling interrupts on return must be sound, and deep C-state side effects must be
        /// handled by the caller.
        pub(super) fn x86_enable_ints_and_mwait(hints: u32);

        /// Overwrites the hardware Return Address Stack (RAS/RSB) with 32 speculation-trap
        /// entries.
        ///
        /// # Safety
        ///
        /// At least 264 bytes of kernel stack space must be available below `%rsp`.
        pub(super) fn x86_ras_fill();

        /// Overwrites the CPU's microarchitectural buffers (`verw`).
        // SAFETY: `mds_buff_overwrite` (`arch/x86/asm.S`) only copies `%ds` to an 8-byte temporary
        // slot allocated on the stack (`subq $8, %rsp` / `addq $8, %rsp`), executes the
        // non-privileged `verw (%rsp)` instruction (which is valid in all processor modes and only
        // modifies caller-saved `RFLAGS.ZF` while flushing microarchitectural buffers when
        // `MD_CLEAR` is supported), and returns via `RET_AND_SPECULATION_POSTFENCE` while
        // preserving `%rsp`, callee-saved registers, and all other memory.
        pub(super) safe fn mds_buff_overwrite();
    }
}

/// Arms the CPU's `MWAIT` address monitor at `addr`.
///
/// # Safety
///
/// - The current privilege level must be `CPL == 0` (`#UD` on Intel / `#GP(0)` on AMD otherwise).
/// - `MONITOR` must be supported and enabled on the current logical processor
///   (`CPUID.01H:ECX[3]`, with Intel `IA32_MISC_ENABLE[18]` set and AMD `HWCR[MonMwaitDis]`
///   clear).
/// - `addr` must point to canonical, readable memory of Write-Back (`WB`) caching type (`UC`,
///   `WC`, or MMIO addresses do not fault on `MONITOR` yet fail to arm the hardware monitor).
pub unsafe fn x86_monitor(addr: *const core::ffi::c_void) {
    // SAFETY: Caller guarantees `CPL == 0`, `MONITOR` is enabled on this logical processor, and
    // `addr` points to canonical, readable Write-Back memory.
    unsafe { ffi::x86_monitor(addr) };
}

/// Enables maskable interrupts and enters `MWAIT` with `hints` (`sti; mwait`), returning with
/// `RFLAGS.IF = 1`.
///
/// # Safety
///
/// - The caller must be at `CPL == 0` with `MONITOR`/`MWAIT` enabled on the current logical
///   processor.
/// - Maskable interrupts must be disabled on entry (`RFLAGS.IF == 0`), as `STI` only creates the
///   1-instruction interrupt shadow covering `mwait` when transitioning `IF` from `0` to `1`.
/// - Because this function returns with `RFLAGS.IF = 1`, the caller must be in a context where
///   enabling and servicing interrupts is sound (no spinlocks or `InterruptDisableGuard` held
///   across the call).
/// - On processors susceptible to cross-thread Return Address Stack (RAS) consumption during
///   `MWAIT` (such as SMT AMD Family 17h `AMD-SB-1045`, even in `C1`), the caller must fill the
///   RAS (`x86_ras_fill`) before entering `MWAIT`.
/// - When `hints` requests a C-state deeper than `C1`, the caller must account for architectural
///   side effects of deep C-states (potential TLB flush, non-ARAT Local APIC timer stop, RSB/RAS
///   emptying on Skylake, and DMA stores not waking `MWAIT`).
pub unsafe fn x86_enable_ints_and_mwait(hints: u32) {
    // SAFETY: Caller guarantees `CPL == 0`, `MONITOR`/`MWAIT` is enabled, `RFLAGS.IF == 0` on
    // entry, enabling interrupts on return is sound, and C-state side effects are handled.
    unsafe { ffi::x86_enable_ints_and_mwait(hints) };
}

/// Overwrites the hardware Return Address Stack (RAS/RSB) with 32 speculation-trap entries.
#[inline]
pub fn x86_ras_fill() {
    // SAFETY: `x86_ras_fill` (`ops.S`) uses 264 bytes of temporary kernel stack space and restores
    // `%rcx` and `%rsp` before returning.
    unsafe { ffi::x86_ras_fill() };
}

/// Overwrites the CPU's microarchitectural buffers, to mitigate MDS and TAA (`mds_buff_overwrite`).
#[inline]
pub fn mds_buff_overwrite() {
    ffi::mds_buff_overwrite();
}

/// Returns the user `fs_base` of `thread` (`thread->arch().fs_base`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live `Thread` whose `arch.fs_base` is
/// initialized and is not concurrently mutated or exclusively borrowed (`&mut ArchThread`) during
/// this call.
#[inline]
pub unsafe fn x86_thread_fs_base(thread: *const Thread) -> usize {
    // SAFETY: The caller guarantees that `thread` points to a live `Thread` whose `arch.fs_base`
    // is initialized and not concurrently mutated or exclusively borrowed.
    unsafe { (*super::thread::thread_arch_const(thread)).fs_base }
}

/// Returns the buffer that holds the saved extended register state of `thread`
/// (`thread->arch().extended_register_buffer`).
///
/// # Safety
///
/// `thread` must point to a live `Thread`.
#[inline]
pub unsafe fn x86_thread_extended_register_buffer(thread: *mut Thread) -> *mut u8 {
    // SAFETY: The caller guarantees that `thread` points to a live `Thread`, so projecting to its
    // embedded `arch_thread` stays in bounds. No reference is created.
    unsafe {
        (&raw mut (*super::thread::thread_arch(thread)).extended_register_buffer).cast::<u8>()
    }
}

/// Safely reads a 64-bit Model Specific Register (MSR) with fault recovery.
///
/// Returns `Ok(value)` if the read succeeded, or `Err(Status)` if a `#GP(0)` fault occurred.
///
/// # Safety
///
/// - While this catches `#GP(0)` faults, the caller must ensure that reading `msr_id` does not
///   trigger unwanted hardware side effects.
/// - Because `#GP(0)` recovery uses per-CPU `%gs:PERCPU_GPF_RETURN_OFFSET` and the kernel IDT's
///   `x86_exception_handler` -> `x86_gpf_handler`, `%gs.base` and the kernel IDT must be
///   initialized, `!blocking_disallowed()` must hold if `msr_id` can fault (otherwise
///   `x86_exception_handler` panics on interrupt-handler recursion), and thread-context callers
///   must have preemption or interrupts disabled so the thread cannot migrate or be preempted
///   before recovery completes (see `read_msr_safe_on_cpu`).
pub unsafe fn read_msr_safe(msr_id: u32) -> Result<u64, Status> {
    let mut val: u64 = 0;
    // SAFETY: Calls assembly implementation with valid pointer to local variable.
    let status = unsafe { ffi::read_msr_safe(msr_id, &mut val) };
    Status::ok(status).map(|()| val)
}

/// Safely writes a 64-bit value to a Model Specific Register (MSR) with fault recovery.
///
/// Returns `Ok(())` if the write succeeded, or `Err(Status)` if a `#GP(0)` fault occurred.
///
/// # Safety
///
/// - While this catches `#GP(0)` faults, the caller must ensure that writing `val` to `msr_id`
///   does not violate system invariants or trigger unwanted hardware side effects.
/// - Because `#GP(0)` recovery uses per-CPU `%gs:PERCPU_GPF_RETURN_OFFSET` and the kernel IDT's
///   `x86_exception_handler` -> `x86_gpf_handler`, `%gs.base` and the kernel IDT must be
///   initialized, `!blocking_disallowed()` must hold if `msr_id` can fault (otherwise
///   `x86_exception_handler` panics on interrupt-handler recursion), and thread-context callers
///   must have preemption or interrupts disabled so the thread cannot migrate or be preempted
///   before recovery completes (see `write_msr_safe_on_cpu`).
pub unsafe fn write_msr_safe(msr_id: u32, val: u64) -> Result<(), Status> {
    // SAFETY: Calls assembly implementation which handles #GP faults safely.
    let status = unsafe { ffi::write_msr_safe(msr_id, val) };
    Status::ok(status)
}

/// Halts the calling CPU until the next interrupt.
#[inline(always)]
pub fn x86_hlt() {
    // SAFETY: Executing `hlt` in ring 0 halts the processor until the next interrupt.
    unsafe {
        core::arch::asm!("hlt", options(nostack, preserves_flags));
    }
}

/// Reads the `CR0` control register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline]
pub unsafe fn x86_get_cr0() -> u64 {
    let val: u64;
    // SAFETY: Reading CR0 at CPL 0 preserves stack and flags.
    unsafe {
        core::arch::asm!(
            "mov {}, cr0",
            out(reg) val,
            options(nostack, preserves_flags),
        );
    }
    val
}

/// Writes `val` to the `CR0` control register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `val` is a valid `CR0`
/// configuration that preserves paging and protected mode invariants required by the kernel.
#[inline]
pub unsafe fn x86_set_cr0(val: u64) {
    // SAFETY: Caller guarantees `val` is a valid CR0 configuration at CPL 0.
    unsafe {
        core::arch::asm!(
            "mov cr0, {}",
            in(reg) val,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads the `CR2` page-fault linear address register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline]
pub unsafe fn x86_get_cr2() -> u64 {
    let val: u64;
    // SAFETY: Reading CR2 at CPL 0 preserves stack and flags.
    unsafe {
        core::arch::asm!(
            "mov {}, cr2",
            out(reg) val,
            options(nostack, preserves_flags),
        );
    }
    val
}

/// Reads the `CR3` page-directory base register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline]
pub unsafe fn x86_get_cr3() -> u64 {
    let val: u64;
    // SAFETY: Reading CR3 at CPL 0 preserves stack and flags.
    unsafe {
        core::arch::asm!(
            "mov {}, cr3",
            out(reg) val,
            options(nostack, preserves_flags),
        );
    }
    val
}

/// Writes `val` to the `CR3` page-directory base register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `val` is a valid physical
/// PML4 address / PCID configuration that keeps the current kernel code and stack mapped.
#[inline]
pub unsafe fn x86_set_cr3(val: u64) {
    // SAFETY: Caller guarantees `val` is a valid CR3 value at CPL 0.
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) val,
            options(preserves_flags),
        );
    }
}

/// Reads the `CR4` control register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline]
pub unsafe fn x86_get_cr4() -> u64 {
    let val: u64;
    // SAFETY: Reading CR4 at CPL 0 preserves stack and flags.
    unsafe {
        core::arch::asm!(
            "mov {}, cr4",
            out(reg) val,
            options(nostack, preserves_flags),
        );
    }
    val
}

/// Writes `val` to the `CR4` control register.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `val` is a valid `CR4`
/// configuration supported by the current CPU.
#[inline]
pub unsafe fn x86_set_cr4(val: u64) {
    // SAFETY: Caller guarantees `val` is a valid CR4 configuration at CPL 0.
    unsafe {
        core::arch::asm!(
            "mov cr4, {}",
            in(reg) val,
            options(nostack, preserves_flags),
        );
    }
}

/// Loads the Interrupt Descriptor Table Register (`IDTR`) from the 10-byte descriptor at `base`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `base` points to a valid
/// 10-byte `Idtr` descriptor whose IDT remains mapped and valid while loaded.
#[inline]
pub unsafe fn x86_lidt(base: *const c_void) {
    // SAFETY: Caller guarantees `base` points to a valid IDTR descriptor.
    unsafe {
        core::arch::asm!(
            "lidt [{}]",
            in(reg) base,
            options(nostack, preserves_flags),
        );
    }
}

/// Loads the Global Descriptor Table Register (`GDTR`) from the 10-byte descriptor at `base`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `base` points to a valid
/// 10-byte `Gdtr` descriptor whose GDT remains mapped and valid while loaded.
#[inline]
pub unsafe fn x86_lgdt(base: *const c_void) {
    // SAFETY: Caller guarantees `base` points to a valid GDTR descriptor.
    unsafe {
        core::arch::asm!(
            "lgdt [{}]",
            in(reg) base,
            options(nostack, preserves_flags),
        );
    }
}

/// Writes back and invalidates all processor caches (`wbinvd`).
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline(always)]
pub unsafe fn wbinvd() {
    // SAFETY: `wbinvd` writes back modified cache lines and invalidates internal caches at CPL 0.
    unsafe {
        core::arch::asm!("wbinvd", options(nostack, preserves_flags),);
    }
}

/// Loads the Task Register (`TR`) with the TSS segment selector `sel` (`ltr`).
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `sel` selects a valid,
/// non-busy 64-bit TSS descriptor in the current GDT whose backing `Tss64` remains valid while
/// loaded.
#[inline(always)]
pub unsafe fn x86_ltr(sel: u16) {
    // SAFETY: Caller guarantees `sel` is a valid non-busy TSS selector in the current GDT.
    unsafe {
        core::arch::asm!(
            "ltr ax",
            in("ax") sel,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads a 64-bit value from `%gs:[OFFSET]`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to a valid `x86_percpu` structure and
/// `OFFSET` is a valid 8-byte aligned field offset within `x86_percpu`.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub unsafe fn x86_read_gs_offset64<const OFFSET: usize>() -> u64 {
    let ret: u64;
    // SAFETY: Caller guarantees `%gs:[OFFSET]` is a valid 64-bit per-CPU field.
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[{offset}]",
            out(reg) ret,
            offset = const OFFSET,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// Writes a 64-bit value to `%gs:[OFFSET]`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to a valid `x86_percpu` structure and
/// `OFFSET` is a valid 64-bit field offset within `x86_percpu` that is safe to overwrite with
/// `val`.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub unsafe fn x86_write_gs_offset64<const OFFSET: usize>(val: u64) {
    // SAFETY: Caller guarantees `%gs:[OFFSET]` is a valid 64-bit per-CPU field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{offset}], {}",
            in(reg) val,
            offset = const OFFSET,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads a 32-bit value from `%gs:[OFFSET]`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to a valid `x86_percpu` structure and
/// `OFFSET` is a valid 4-byte aligned field offset within `x86_percpu`.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub unsafe fn x86_read_gs_offset32<const OFFSET: usize>() -> u32 {
    let ret: u32;
    // SAFETY: Caller guarantees `%gs:[OFFSET]` is a valid 32-bit per-CPU field.
    unsafe {
        core::arch::asm!(
            "mov {:e}, gs:[{offset}]",
            out(reg) ret,
            offset = const OFFSET,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// Writes a 32-bit value to `%gs:[OFFSET]`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to a valid `x86_percpu` structure and
/// `OFFSET` is a valid 4-byte aligned field offset within `x86_percpu` that is safe to overwrite
/// with `val`.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub unsafe fn x86_write_gs_offset32<const OFFSET: usize>(val: u32) {
    // SAFETY: Caller guarantees `%gs:[OFFSET]` is a valid 32-bit per-CPU field.
    unsafe {
        core::arch::asm!(
            "mov gs:[{offset}], {:e}",
            in(reg) val,
            offset = const OFFSET,
            options(nostack, preserves_flags),
        );
    }
}

/// Adds `DELTA` to the 32-bit value at `%gs:[OFFSET]`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to a valid `x86_percpu` structure,
/// interrupts are disabled on the calling CPU, and `OFFSET` is a valid 4-byte aligned field offset
/// within `x86_percpu` that is safe to modify.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub unsafe fn x86_add_gs_offset32<const OFFSET: usize, const DELTA: i32>() {
    // SAFETY: Caller guarantees `%gs:[OFFSET]` is a valid 32-bit per-CPU field with exclusive
    // access on the current CPU.
    unsafe {
        core::arch::asm!(
            "add dword ptr gs:[{offset}], {delta}",
            offset = const OFFSET,
            delta = const DELTA,
            options(nostack),
        );
    }
}

/// Disables maskable interrupts on the current CPU (`cli`).
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[inline(always)]
pub unsafe fn x86_cli() {
    // SAFETY: `cli` clears `RFLAGS.IF` at CPL 0. Omitting `nomem` makes the `asm!` block itself a
    // compiler memory barrier without an out-of-line `compiler_fence` call.
    unsafe {
        core::arch::asm!("cli", options(nostack, preserves_flags));
    }
}

/// Writes `val` to the `%ds` data segment selector register.
///
/// # Safety
///
/// The caller must ensure that `val` is a valid data segment selector (or the null selector `0` in
/// 64-bit mode).
#[inline(always)]
pub unsafe fn set_ds(val: u16) {
    // SAFETY: Caller guarantees `val` is a valid `%ds` selector.
    unsafe {
        core::arch::asm!("mov ds, {:x}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Writes `val` to the `%es` extra segment selector register.
///
/// # Safety
///
/// The caller must ensure that `val` is a valid data segment selector (or the null selector `0` in
/// 64-bit mode).
#[inline(always)]
pub unsafe fn set_es(val: u16) {
    // SAFETY: Caller guarantees `val` is a valid `%es` selector.
    unsafe {
        core::arch::asm!("mov es, {:x}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Writes `val` to the `%fs` segment selector register.
///
/// # Safety
///
/// The caller must ensure that `val` is a valid data segment selector (or the null selector `0` in
/// 64-bit mode). Note that loading `%fs` may clobber `%fs.base`.
#[inline(always)]
pub unsafe fn set_fs(val: u16) {
    // SAFETY: Caller guarantees `val` is a valid `%fs` selector.
    unsafe {
        core::arch::asm!("mov fs, {:x}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Reads the `%gs` segment selector register.
#[inline(always)]
pub fn get_gs() -> u16 {
    let val: u16;
    // SAFETY: Reading the `%gs` selector has no stack or flag side effects.
    unsafe {
        core::arch::asm!("mov {:x}, gs", out(reg) val, options(nostack, preserves_flags));
    }
    val
}

/// Writes `val` to the `%gs` segment selector register.
///
/// # Safety
///
/// The caller must ensure that `val` is a valid segment selector (or the null selector `0` in
/// 64-bit mode), interrupts are disabled on the current CPU, and kernel `%gs.base` (`IA32_GS_BASE`)
/// is restored before re-enabling interrupts or performing any subsequent `%gs`-relative per-CPU
/// access, because loading `%gs` clobbers `%gs.base`.
#[inline(always)]
pub unsafe fn set_gs(val: u16) {
    // SAFETY: Caller guarantees `val` is a valid `%gs` selector, interrupts are disabled, and
    // `%gs.base` is restored afterwards.
    unsafe {
        core::arch::asm!("mov gs, {:x}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Reads the `%fs.base` register using the `rdfsbase` instruction (`_readfsbase_u64`).
///
/// # Safety
///
/// The caller must ensure that `CR4.FSGSBASE` is enabled on the current CPU.
#[inline(always)]
pub unsafe fn read_fsbase() -> u64 {
    let val: u64;
    // SAFETY: Caller guarantees `CR4.FSGSBASE` is enabled; `rdfsbase` has no stack or flag side
    // effects.
    unsafe {
        core::arch::asm!("rdfsbase {}", out(reg) val, options(nostack, preserves_flags));
    }
    val
}

/// Writes `val` to the `%fs.base` register using the `wrfsbase` instruction (`_writefsbase_u64`).
///
/// # Safety
///
/// The caller must ensure that `CR4.FSGSBASE` is enabled on the current CPU and `val` is a
/// canonical virtual address.
#[inline(always)]
pub unsafe fn write_fsbase(val: u64) {
    // SAFETY: Caller guarantees `CR4.FSGSBASE` is enabled and `val` is a canonical address.
    unsafe {
        core::arch::asm!("wrfsbase {}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Saves and returns the current `RFLAGS` register (`x86_save_flags`).
///
/// We use `pushfq; pop` to initialize the full output register. Note that the
/// instruction `pushf` does not initialize the upper 32 bits, but does
/// increment `rsp` by 8, resulting in uninitialized bits. To ensure we
/// cleanly initialize the full output, we use `pushfq` here.
#[inline(always)]
pub fn x86_save_flags() -> u64 {
    let state: u64;
    // SAFETY: `pushfq; pop` saves `RFLAGS` without altering flags; red-zone is disabled in kernel
    // builds.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            out(reg) state,
            options(preserves_flags),
        );
    }
    state
}

/// Restores the `RFLAGS` register from `flags` (`x86_restore_flags`).
///
/// # Safety
///
/// The caller must ensure that `flags` is a valid `RFLAGS` value for the current privilege level
/// and interrupt context. Specifically:
/// - `(flags & X86_FLAGS_DF) == 0` (direction flag must be clear).
/// - `(flags & X86_FLAGS_AC) == 0` outside user-copy windows (alignment check).
/// - `TF == 0` (trap flag).
/// - `NT == 0` (nested task).
/// - `CPL == 0` if modifying privileged flags.
/// - No spinlocks or `InterruptDisableGuard`s are held if setting `IF == 1`.
#[inline(always)]
pub unsafe fn x86_restore_flags(flags: u64) {
    debug_assert_eq!(flags & super::registers::X86_FLAGS_DF, 0);
    // SAFETY: Caller guarantees `flags` is valid for the current context.
    unsafe {
        core::arch::asm!(
            "push {}",
            "popfq",
            in(reg) flags,
        );
    }
}
