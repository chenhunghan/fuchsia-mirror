// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2014 Travis Geiselbrecht
// Copyright (c) 2015 Intel Corporation
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};

use debug::dprintf;

use super::descriptor::set_tss_sp;
use super::mp::{
    PERCPU_CURRENT_THREAD_OFFSET, PERCPU_DIRECT_OFFSET, PERCPU_IN_RESTRICTED_MODE,
    PERCPU_LAST_USER_ASPACE_OFFSET,
};
use super::registers::{
    self, X86_DR6_MASK, X86_DR7_MASK, X86_MSR_IA32_FS_BASE, X86_MSR_IA32_GS_BASE,
    X86_MSR_IA32_KERNEL_GS_BASE,
};
use super::thread_ffi::x86_64_context_switch;
use super::x86::{
    get_gs, read_fsbase, read_msr, set_ds, set_es, set_fs, set_gs, write_fsbase, write_msr,
    x86_ras_fill, x86_read_gs_offset64, x86_write_gs_offset32, x86_write_gs_offset64,
};
use super::{GeneralRegsSource, KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, feature};
#[cfg(sanitize = "safestack")]
use crate::kernel::thread::get_unsafe_stack_top;
use crate::kernel::thread::{
    ARCH_OFFSET, Thread, active_aspace, get_stack_top, in_restricted, is_running,
    is_user_state_saved_locked,
};
use x86_registers_bindings::arch_thread;

/// Bit 63 of `page_fault_resume` indicates whether a data fault should first handle the fault, or
/// immediately return the resume location. The 63rd bit is selected as this bit is invariant over
/// all kernel addresses.
pub(super) const X86_PFR_RUN_FAULT_HANDLER_BIT: u64 = 63;

// Check that the fault handler bit would always be 1 for a kernel address.
zr::static_assert!(
    ((KERNEL_ASPACE_BASE as u64 >> X86_PFR_RUN_FAULT_HANDLER_BIT) & 1) == 1
        && ((KERNEL_ASPACE_SIZE - 1) & KERNEL_ASPACE_BASE) == 0
);

/// Register state layout used by `x86_64_context_switch` in `asm.S`.
#[repr(C)]
struct X86ContextSwitchFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
    rip: u64,
}

zr::static_assert!(size_of::<X86ContextSwitchFrame>() == 56);
zr::static_assert!(align_of::<X86ContextSwitchFrame>() == 8);
zr::static_assert!(offset_of!(X86ContextSwitchFrame, rbp) == 32);
zr::static_assert!(offset_of!(X86ContextSwitchFrame, rip) == 48);

/// Architecture-specific thread state for x86-64 (`struct arch_thread`).
pub(super) type ArchThread = arch_thread;

zr::static_assert!(size_of::<ArchThread>() == 1152);
zr::static_assert!(align_of::<ArchThread>() == 64);
zr::static_assert!(offset_of!(ArchThread, extended_register_buffer).is_multiple_of(64));
// Verify that bindgen's `__has_feature(safe_stack)` matches Rust's `cfg(sanitize = "safestack")`
// (whether `unsafe_sp` is present at offset 8 before `fs_base`).
#[cfg(sanitize = "safestack")]
zr::static_assert!(offset_of!(ArchThread, fs_base) == 16);
#[cfg(not(sanitize = "safestack"))]
zr::static_assert!(offset_of!(ArchThread, fs_base) == 8);

/// Returns a raw pointer to the x86-64 architectural state (`arch_thread`) embedded in `thread`.
///
/// Performs in-bounds byte-offset arithmetic (`ARCH_OFFSET`) without dereferencing `thread`.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` allocation.
#[inline(always)]
pub(super) unsafe fn thread_arch(thread: *mut Thread) -> *mut ArchThread {
    // SAFETY: Caller guarantees `thread` points to a live `Thread` allocation, so offsetting by
    // `ARCH_OFFSET` (`offset_of!(Thread, arch)`) stays in bounds of that allocation.
    unsafe { thread.byte_add(ARCH_OFFSET).cast::<ArchThread>() }
}

/// Returns a const raw pointer to the x86-64 architectural state (`arch_thread`) embedded in
/// `thread`.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` allocation.
#[inline(always)]
pub(super) unsafe fn thread_arch_const(thread: *const Thread) -> *const ArchThread {
    // SAFETY: Caller guarantees `thread` points to a live `Thread` allocation; `thread_arch` only
    // offsets the pointer without mutating through it.
    unsafe { thread_arch(thread.cast_mut()) }.cast_const()
}

/// Initializes the architectural state of `t` (`arch_thread_initialize`).
///
/// # Safety
///
/// The caller must ensure that:
/// - [`registers::extended_register_init`] has completed on CPU 0.
/// - `t` points to a live (not freed), valid `Thread` that is not currently running, and the caller
///   holds exclusive access to `t` (or `t`'s lock).
/// - `t` has an allocated kernel stack whose top (`get_stack_top(t)`) is 16-byte aligned with at
///   least `size_of::<u64>() + size_of::<X86ContextSwitchFrame>()` (`64`) writable bytes below
///   `stack_top` (and a 16-byte-aligned `unsafe_sp` when safe-stack is enabled).
#[inline(always)]
pub unsafe fn thread_initialize(t: *mut Thread, entry_point: usize) {
    // Create a default stack frame on the stack.
    // SAFETY: Caller guarantees `t` points to a valid `Thread` with an allocated kernel stack and
    // exclusive access.
    let mut stack_top = unsafe { get_stack_top(t) };

    // Make sure the top of the stack is 16-byte aligned for ABI compliance.
    debug_assert!(stack_top.is_multiple_of(16));

    // Make sure we start the frame 8-byte unaligned (relative to the 16-byte alignment)
    // because of the way the context switch will pop the return address off the stack. After
    // the first context switch, this leaves the stack unaligned relative to how a called
    // function expects it.
    stack_top = stack_top.wrapping_sub(8);
    let frame = core::ptr::with_exposed_provenance_mut::<X86ContextSwitchFrame>(stack_top);

    // Record a zero return address so that backtraces will stop here.
    // Otherwise if heap debugging is on, and say there is 99..99 here,
    // then the debugger could try to continue the backtrace from there.
    // SAFETY: Caller guarantees `stack_top` has at least `8 + size_of::<X86ContextSwitchFrame>()`
    // writable, 8-byte-aligned bytes below the initial 16-byte-aligned stack top.
    let frame = unsafe {
        frame.cast::<u64>().write(0);

        // Move down a frame size and zero it out.
        let frame = frame.sub(1);
        frame.write(X86ContextSwitchFrame {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbp: 0,
            rbx: 0,
            rip: entry_point as u64,
        });
        frame
    };

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0 and holds exclusive
    // access to `t`; `(*arch).extended_register_buffer` is a 64-byte-aligned
    // `X86_MAX_EXTENDED_REGISTER_SIZE` buffer.
    unsafe {
        // Initialize the saved extended register state.
        let arch = thread_arch(t);
        debug_assert!((&raw const (*arch).extended_register_buffer).addr().is_multiple_of(64));
        registers::extended_register_init_state(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
        );

        // Set the stack pointer.
        (*arch).sp = frame.expose_provenance();
        #[cfg(sanitize = "safestack")]
        {
            let unsafe_top = get_unsafe_stack_top(t);
            debug_assert!(unsafe_top.is_multiple_of(16));
            (*arch).unsafe_sp = unsafe_top;
        }

        // Initialize the fs, gs and kernel bases to 0.
        (*arch).fs_base = 0;
        (*arch).gs_base = 0;

        // Initialize the debug registers to a valid initial state.
        (*arch).track_debug_state = false;
        (*arch).debug_state.dr = [0; 4]; // Set dr0-dr3.
        (*arch).debug_state.dr6 = X86_DR6_MASK;
        (*arch).debug_state.dr7 = X86_DR7_MASK;
    }
}

/// Sets `%gs:[PERCPU_CURRENT_THREAD_OFFSET]` to `t` (`arch_set_current_thread`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to the calling CPU's `x86_percpu`,
/// interrupts are disabled on the calling CPU, and `t` points to a live (not freed), valid `Thread`
/// dedicated to the calling CPU (not concurrently running on another CPU) that remains live and
/// valid for as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
#[inline(always)]
unsafe fn arch_set_current_thread(t: *mut Thread) {
    // Set GS:current_thread pointer to this one to establish the new context.
    // SAFETY: Caller guarantees `%gs.base` is initialized to the calling CPU's `x86_percpu`,
    // interrupts are disabled, and `t` is a valid `Thread` dedicated to this CPU that remains live
    // while installed as the current thread.
    unsafe { x86_write_gs_offset64::<PERCPU_CURRENT_THREAD_OFFSET>(t.expose_provenance() as u64) };
}

/// Establishes `t` as the current thread on the calling CPU (`arch_thread_construct_first`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to the calling CPU's `x86_percpu`,
/// interrupts are disabled on the calling CPU, and `t` points to a live (not freed), valid `Thread`
/// dedicated to the calling CPU (not concurrently running on another CPU) that remains live and
/// valid for as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
#[inline(always)]
pub unsafe fn thread_construct_first(t: *mut Thread) {
    // SAFETY: Caller guarantees `%gs.base` is initialized, interrupts are disabled, and `t` is a
    // valid `Thread` dedicated to this CPU that remains live while installed as current thread.
    unsafe { arch_set_current_thread(t) };
}

/// Dumps architecture-specific thread state for `t` (`arch_dump_thread`).
///
/// # Safety
///
/// The caller must ensure that `t` points to a live (not freed), valid `Thread`, and that the
/// caller either holds `t`'s lock (shared or exclusive) or ensures no other CPU can concurrently
/// modify `t`'s scheduler state or saved `arch.sp` (for example, `t` is the current thread on the
/// calling CPU with interrupts disabled, or all other CPUs have been halted during a panic or crash
/// dump).
#[inline(always)]
pub unsafe fn dump_thread(t: *const Thread) {
    // SAFETY: Caller guarantees `t` is a valid `Thread` and either holds `t`'s lock or ensures no
    // other CPU can concurrently modify `t`'s scheduler state.
    if unsafe { !is_running(t) } {
        dprintf!(INFO, "\tarch: ");
        // SAFETY: Caller guarantees `t` is a valid `Thread` and either holds `t`'s lock or ensures
        // no other CPU can concurrently modify `t`'s saved `arch.sp`.
        dprintf!(INFO, "sp {:#x}\n", unsafe { (*thread_arch_const(t)).sp });
    }
}

/// Returns the frame pointer of a blocked thread `t`, or `0` if frame pointers are disabled
/// (`arch_thread_get_blocked_fp`).
///
/// # Safety
///
/// The caller must ensure that `t` points to a live (not freed), valid blocked `Thread` whose saved
/// `arch.sp` points to a valid [`X86ContextSwitchFrame`], and that the caller holds `t`'s lock (or
/// otherwise ensures `t` remains blocked for the duration of the call).
#[inline(always)]
pub unsafe fn thread_get_blocked_fp(t: *const Thread) -> usize {
    if !crate::arch_rs::with_frame_pointers() {
        return 0;
    }

    // SAFETY: Caller guarantees `t` is a valid blocked thread whose `sp` points to its live
    // `X86ContextSwitchFrame` for the duration of this call.
    unsafe {
        let sp = (*thread_arch_const(t)).sp;
        let frame = core::ptr::with_exposed_provenance::<X86ContextSwitchFrame>(sp);
        (*frame).rbp as usize
    }
}

/// Applies Spectre V2 Return Address Stack (RAS) fill and Indirect Branch Prediction Barrier
/// (IBPB) mitigations during a context switch.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `%gs.base` is initialized, interrupts are disabled, and
/// `oldthread` and `newthread` point to live (not freed), valid `Thread`s undergoing a context
/// switch on the current CPU.
#[inline(always)]
unsafe fn context_switch_spec_mitigations(oldthread: *mut Thread, newthread: *mut Thread) {
    // It should always be safe to access a thread's aspace member during a
    // context switch.  The threads must still be alive (even if the old thread is
    // context switching for the very last time, and is just about to disappear),
    // and neither one is really "running" (they cannot be in the process of
    // switching address spaces).
    // Note: `last_user_aspace` is only ever compared by address value against `newthread_aspace`
    // and is never dereferenced as a pointer.
    // SAFETY: `oldthread` and `newthread` are valid threads undergoing a context switch on this
    // CPU.
    let (oldthread_aspace, newthread_aspace) =
        unsafe { (active_aspace(oldthread).addr(), active_aspace(newthread).addr()) };

    // Spectre V2: Overwrite the Return Address Stack to ensure it's not poisoned
    // Only overwrite/fill if the prior thread was a user thread or if we're on CPUs vulnerable
    // to RSB underflow attacks.
    if feature::x86_cpu_should_ras_fill_on_ctxt_switch()
        && (oldthread_aspace != 0 || feature::x86_cpu_vulnerable_to_rsb_underflow())
    {
        x86_ras_fill();
    }

    // Flush Indirect Branch Predictor State, if:
    // 1) We are switching from a user address space to another user address space OR
    // 2) We are switching from the kernel address space to a user address space and the
    //    new user address space is not the same as the last user address space that ran
    //    on this core.
    // TODO(https://fxbug.dev/42115502): Handle aspace* reuse.
    // SAFETY: Caller guarantees CPL 0, `%gs.base` is initialized, and interrupts are disabled.
    // `x86_cpu_should_ibpb_on_ctxt_switch()` guarantees `x86_cpu_has_ibpb()` is true.
    unsafe {
        if feature::x86_cpu_should_ibpb_on_ctxt_switch()
            && (((oldthread_aspace != 0 && newthread_aspace != 0)
                && (oldthread_aspace != newthread_aspace))
                || ((oldthread_aspace == 0 && newthread_aspace != 0)
                    && (x86_read_gs_offset64::<PERCPU_LAST_USER_ASPACE_OFFSET>() as usize
                        != newthread_aspace)))
        {
            feature::x86_cpu_ibpb();
        }
        if oldthread_aspace != 0 && newthread_aspace == 0 {
            x86_write_gs_offset64::<PERCPU_LAST_USER_ASPACE_OFFSET>(oldthread_aspace as u64);
        }
    }
}

/// Saves `thread`'s user `%fs.base` and `%gs.base` into its `ArchThread`.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is
/// reported, `%gs.base` is initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in
/// `IA32_KERNEL_GS_BASE`), interrupts are disabled, and `thread` points to the current live (not
/// freed) `Thread` with exclusive access to its `fs_base` and `gs_base` fields.
#[inline(always)]
unsafe fn segment_selector_save_state(thread: *mut Thread) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees `thread` is a valid `Thread`.
    let arch = unsafe { thread_arch(thread) };
    // Save the user fs_base and gs_base.  The new rdfsbase instruction is much faster than
    // reading the MSR, so use the former when available.
    if feature::x86_feature_fsgsbase() {
        // SAFETY: Caller guarantees CPL 0, `CR4.FSGSBASE == 1`, `%gs.base` is initialized,
        // interrupts are disabled around the `swapgs` pair, and `arch` is uniquely accessed.
        unsafe {
            (*arch).fs_base = read_fsbase() as usize;
            // Remember, the user and kernel gs_base values have been swapped -- the user value is
            // currently in KERNEL_GS_BASE.
            let gs_base: u64;
            core::arch::asm!(
                "swapgs",
                "rdgsbase {}",
                "swapgs",
                out(reg) gs_base,
                options(nostack, preserves_flags),
            );
            (*arch).gs_base = gs_base as usize;
        }
    } else {
        core::hint::cold_path();
        // SAFETY: Caller guarantees CPL 0 and `arch` is uniquely accessed.
        unsafe {
            (*arch).fs_base = read_msr(X86_MSR_IA32_FS_BASE) as usize;
            (*arch).gs_base = read_msr(X86_MSR_IA32_KERNEL_GS_BASE) as usize;
        }
    }
}

/// Resets data segment selectors and restores `thread`'s user `%fs.base` and `%gs.base`.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is
/// reported, `%gs.base` is initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in
/// `IA32_KERNEL_GS_BASE`), interrupts are disabled, and `thread` points to a live (not freed),
/// valid `Thread` with shared access to its `fs_base` and `gs_base` fields containing canonical
/// addresses.
#[inline(always)]
unsafe fn segment_selector_restore_state(thread: *const Thread) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees CPL 0, `%gs.base` is initialized, and interrupts are disabled; `0`
    // is the valid null segment selector in 64-bit mode, and `%gs.base` (`X86_MSR_IA32_GS_BASE`)
    // is immediately restored from `%gs:[PERCPU_DIRECT_OFFSET]` (a canonical kernel address) when
    // `set_gs(0)` is executed.
    unsafe {
        set_ds(0);
        set_es(0);
        set_fs(0);
        if get_gs() != 0 {
            core::hint::cold_path();
            // Assigning to %gs may clobber gs_base, so we must restore gs_base afterwards.
            let gs_base = x86_read_gs_offset64::<PERCPU_DIRECT_OFFSET>();
            set_gs(0);
            write_msr(X86_MSR_IA32_GS_BASE, gs_base);
        }
    }

    // SAFETY: Caller guarantees `thread` is valid for reads of `fs_base` and `gs_base`.
    let (fs_base, gs_base) = unsafe {
        let arch = thread_arch_const(thread);
        ((*arch).fs_base as u64, (*arch).gs_base as u64)
    };

    // Restore fs_base and save+restore user gs_base.  Note that the user and kernel gs_base
    // values have been swapped -- the user value is currently in KERNEL_GS_BASE.
    if feature::x86_feature_fsgsbase() {
        // SAFETY: Caller guarantees CPL 0, `CR4.FSGSBASE == 1`, `%gs.base` is initialized,
        // interrupts are disabled around the `swapgs` pair, and `fs_base` and `gs_base` are
        // canonical addresses.
        unsafe {
            write_fsbase(fs_base);
            // There is no variant of the {rd,wr}gsbase instructions for accessing KERNEL_GS_BASE,
            // so we wrap those in two swapgs instructions to get the same effect.  This is a
            // little convoluted, but still faster than using the KERNEL_GS_BASE MSRs.
            core::arch::asm!(
                "swapgs",
                "wrgsbase {}",
                "swapgs",
                in(reg) gs_base,
                options(nostack, preserves_flags),
            );
        }
    } else {
        core::hint::cold_path();
        // SAFETY: Caller guarantees CPL 0 and `fs_base` and `gs_base` are canonical addresses.
        unsafe {
            write_msr(X86_MSR_IA32_FS_BASE, fs_base);
            write_msr(X86_MSR_IA32_KERNEL_GS_BASE, gs_base);
        }
    }
}

/// Saves `oldthread`'s user `%fs.base`/`%gs.base`, resets data segment selectors, and restores
/// `newthread`'s user `%fs.base`/`%gs.base`.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is
/// reported, `%gs.base` is initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in
/// `IA32_KERNEL_GS_BASE`), interrupts are disabled, and `oldthread` and `newthread` point to live
/// (not freed), valid `Thread`s (with exclusive access to `oldthread`'s `fs_base`/`gs_base` and
/// shared access to `newthread`'s canonical `fs_base`/`gs_base`).
#[inline(always)]
unsafe fn segment_selector_context_switch(oldthread: *mut Thread, newthread: *const Thread) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees `oldthread` and `newthread` point to valid `Thread`s.
    let (old_arch, new_arch) = unsafe { (thread_arch(oldthread), thread_arch_const(newthread)) };

    let has_fsgsbase = feature::x86_feature_fsgsbase();

    // Save the user fs_base register value.  The new rdfsbase instruction is much faster than
    // reading the MSR, so use the former in preference.
    if has_fsgsbase {
        // SAFETY: Caller guarantees `CR4.FSGSBASE == 1` and exclusive access to `old_arch`.
        unsafe { (*old_arch).fs_base = read_fsbase() as usize };
    } else {
        core::hint::cold_path();
        // SAFETY: Caller guarantees CPL 0 and exclusive access to `old_arch`.
        unsafe { (*old_arch).fs_base = read_msr(X86_MSR_IA32_FS_BASE) as usize };
    }

    // The segment selector registers can't be preserved across context switches in all cases,
    // because some values get clobbered when returning from interrupts.  If an interrupt
    // occurs when a userland process has set %fs = 1 (for example), the IRET instruction used
    // for returning from the interrupt will reset %fs to 0.
    //
    // To prevent the segment selector register values from leaking between processes, we reset
    // these registers across context switches.
    // SAFETY: Caller guarantees CPL 0, `%gs.base` is initialized, and interrupts are disabled; `0`
    // is the valid null segment selector in 64-bit mode, and `%gs.base` (`X86_MSR_IA32_GS_BASE`)
    // is immediately restored from `%gs:[PERCPU_DIRECT_OFFSET]` (a canonical kernel address) when
    // `set_gs(0)` is executed.
    unsafe {
        set_ds(0);
        set_es(0);
        set_fs(0);
        if get_gs() != 0 {
            core::hint::cold_path();
            // Assigning to %gs may clobber gs_base, so we must restore gs_base afterwards.
            let gs_base = x86_read_gs_offset64::<PERCPU_DIRECT_OFFSET>();
            set_gs(0);
            write_msr(X86_MSR_IA32_GS_BASE, gs_base);
        }
    }

    // Restore fs_base and save+restore user gs_base.  Note that the user and kernel gs_base
    // values have been swapped -- the user value is currently in KERNEL_GS_BASE.
    // SAFETY: Caller guarantees shared access to `new_arch`.
    let (new_gsbase, new_fsbase) =
        unsafe { ((*new_arch).gs_base as u64, (*new_arch).fs_base as u64) };
    if has_fsgsbase {
        // There is no variant of the {rd,wr}gsbase instructions for accessing KERNEL_GS_BASE,
        // so we wrap those in two swapgs instructions to get the same effect.  This is a
        // little convoluted, but still faster than using the KERNEL_GS_BASE MSRs.
        let old_gsbase: u64;
        // SAFETY: Caller guarantees CPL 0, `CR4.FSGSBASE == 1`, `%gs.base` is initialized,
        // interrupts are disabled around the `swapgs` pair, `new_fsbase` and `new_gsbase` are
        // canonical addresses, and `old_arch` is uniquely accessed.
        unsafe {
            core::arch::asm!(
                "swapgs",
                "rdgsbase {old_gsbase}",
                "wrgsbase {new_gsbase}",
                "swapgs",
                "wrfsbase {new_fsbase}",
                old_gsbase = out(reg) old_gsbase,
                new_gsbase = in(reg) new_gsbase,
                new_fsbase = in(reg) new_fsbase,
                options(nostack, preserves_flags),
            );
            (*old_arch).gs_base = old_gsbase as usize;
        }
    } else {
        core::hint::cold_path();
        // SAFETY: Caller guarantees CPL 0, `new_fsbase` and `new_gsbase` are canonical addresses,
        // and `old_arch` is uniquely accessed.
        unsafe {
            (*old_arch).gs_base = read_msr(X86_MSR_IA32_KERNEL_GS_BASE) as usize;
            write_msr(X86_MSR_IA32_FS_BASE, new_fsbase);
            write_msr(X86_MSR_IA32_KERNEL_GS_BASE, new_gsbase);
        }
    }
}

/// Switches hardware debug registers (`DR0`-`DR3`, `DR6`, `DR7`) from `old_thread` to `new_thread`.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `DR7.GD == 0`, and `old_thread` and `new_thread` point to
/// valid `Thread`s with shared access to their `track_debug_state` and `debug_state` fields (where
/// `debug_state` was validated by `validate_debug_state` if `track_debug_state` is true).
#[inline(always)]
unsafe fn debug_context_switch(old_thread: *mut Thread, new_thread: *const Thread) {
    // SAFETY: Caller guarantees CPL 0, `DR7.GD == 0`, and `old_thread` and `new_thread` are valid
    // with shared access to their validated `debug_state` when `track_debug_state` is true.
    unsafe {
        let old_arch = thread_arch_const(old_thread);
        let new_arch = thread_arch_const(new_thread);

        // If the new thread has debug state, then install it, replacing the current contents.
        if (*new_arch).track_debug_state {
            core::hint::cold_path();
            // NOTE: There is no enable debug state call, as x86 doesn't have a global
            //       enable/disable switch, but rather enables particular registers through DR7.
            //       These registers are selected by userspace (and filtered by zircon) in the
            //       thread_write_state syscall.
            //
            //       This means that just writing the thread debug state into the CPU is enough to
            //       activate the debug functionality.
            registers::write_hw_debug_regs(&(*new_arch).debug_state);
            return;
        }

        // If the old thread had debug state running and the new one doesn't use it, disable the
        // debug capabilities.
        if (*old_arch).track_debug_state {
            core::hint::cold_path();
            registers::disable_debug_state();
        }
    }
}

/// Restores or disables hardware debug registers (`DR0`-`DR3`, `DR6`, `DR7`) for `thread`.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `DR7.GD == 0`, and `thread` points to a valid `Thread`
/// with shared access to its `track_debug_state` and `debug_state` fields (where `debug_state` was
/// validated by `validate_debug_state` if `track_debug_state` is true).
#[inline(always)]
unsafe fn debug_restore_state(thread: *const Thread) {
    // SAFETY: Caller guarantees CPL 0, `DR7.GD == 0`, and `thread` is valid with shared access to
    // its validated `debug_state` when `track_debug_state` is true.
    unsafe {
        let arch = thread_arch_const(thread);
        // If `thread` has debug state, restore it, which enables it.
        if (*arch).track_debug_state {
            core::hint::cold_path();
            registers::write_hw_debug_regs(&(*arch).debug_state);
        } else {
            // We don't know if the current CPU has debugging enabled or not, but we do know that
            // `thread` shouldn't have it enabled so disable.
            registers::disable_debug_state();
        }
    }
}

/// Performs an architectural context switch from `oldthread` to `newthread`
/// (`arch_context_switch`).
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL is 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`, [`registers::extended_register_init`]
///   has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is enabled, and `CR4.FSGSBASE == 1`
///   when `X86_FEATURE_FSGSBASE` is reported.
/// - `%gs.base` is initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in
///   `IA32_KERNEL_GS_BASE`) and interrupts are disabled on the current CPU.
/// - `oldthread` and `newthread` are distinct (`oldthread != newthread`) pointers to live (not
///   freed), valid `Thread`s: `oldthread` is the current thread on this CPU (`THREAD_RUNNING` or
///   switching out), `newthread` is not currently running on any CPU, and `newthread` remains live
///   and valid for as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
/// - Both `oldthread`'s and `newthread`'s locks are held by the caller (with no conflicting
///   `ChainLockProof` or live borrow of `oldthread`'s lock-protected state).
/// - `oldthread` (when `!is_user_state_saved_locked(oldthread)`) has a 64-byte-aligned writable
///   `extended_register_buffer`, `newthread` has a 64-byte-aligned `extended_register_buffer`
///   containing a valid extended register state image (created by
///   [`registers::extended_register_init_state`],
///   [`registers::extended_register_init_state_from_bv`], or
///   [`registers::extended_register_save_state`]), both have canonical `fs_base` and `gs_base`
///   addresses and validated `debug_state` (when `track_debug_state` is true), and `newthread`'s
///   `arch.sp` points to a valid [`X86ContextSwitchFrame`] on its kernel stack (with a valid
///   `arch.unsafe_sp` when safe-stack is enabled).
#[inline(never)]
pub unsafe fn context_switch(oldthread: *mut Thread, newthread: *mut Thread) {
    // Out of line to keep `Scheduler::reschedule_common` (which reaches it via
    // `Scheduler::context_switch`) compact.
    // set the tss SP0 value to point at the top of our stack
    // SAFETY: Caller guarantees `%gs.base` is initialized, interrupts are disabled, and
    // `newthread` is a valid, locked `Thread`.
    unsafe { set_tss_sp(get_stack_top(newthread)) };

    // SAFETY: Caller guarantees `oldthread` is valid and holds `oldthread`'s lock with no
    // conflicting `ChainLockProof` or live borrow of `oldthread`'s lock-protected state.
    if unsafe { !is_user_state_saved_locked(oldthread) } {
        // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
        // `extended_register_init` has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is
        // enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is
        // initialized, interrupts are disabled, and `oldthread` and `newthread` are distinct,
        // valid, locked `Thread`s with 64-byte-aligned `extended_register_buffer`s (`oldthread`'s
        // writable and `newthread`'s containing a valid extended register state image), canonical
        // `fs_base`/`gs_base`, and validated `debug_state`.
        unsafe {
            registers::extended_register_context_switch(oldthread, newthread);
            debug_context_switch(oldthread, newthread);
            segment_selector_context_switch(oldthread, newthread);
        }
    } else {
        core::hint::cold_path();
        // Nothing left to save for `oldthread`, so just restore `newthread`.  Technically, we
        // could skip restoring here since we know a higher layer will restore before leaving
        // the kernel.  We restore anyway so we don't leave `oldthread`'s state lingering in
        // the hardware registers.
        // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
        // `extended_register_init` has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is
        // enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is
        // initialized, interrupts are disabled, and `newthread` is a valid, locked `Thread` with a
        // 64-byte-aligned `extended_register_buffer` containing a valid extended register state
        // image, canonical `fs_base`/`gs_base`, and validated `debug_state`.
        unsafe {
            let new_arch = thread_arch_const(newthread);
            registers::extended_register_restore_state(
                (&raw const (*new_arch).extended_register_buffer).cast::<u8>(),
            );
            debug_restore_state(newthread);
            segment_selector_restore_state(newthread);
        }
    }

    // SAFETY: Caller guarantees CPL 0, `%gs.base` is initialized to the calling CPU's `x86_percpu`,
    // interrupts are disabled, and `oldthread` and `newthread` are distinct, valid, locked threads
    // undergoing a context switch where `newthread` has a valid `X86ContextSwitchFrame` on its
    // inactive kernel stack and remains live while installed as current thread.
    unsafe {
        context_switch_spec_mitigations(oldthread, newthread);

        // set the GS:in_restricted_mode pointer to the state of the new thread
        x86_write_gs_offset32::<PERCPU_IN_RESTRICTED_MODE>(u32::from(in_restricted(newthread)));

        // On entry to the assembly function, we're still the old thread as far as
        // the compiler and kernel are concerned.  When it returns, we're back again
        // to being that same old thread.
        let old_arch = thread_arch(oldthread);
        let new_arch = thread_arch_const(newthread);
        #[cfg(sanitize = "safestack")]
        x86_64_context_switch(
            &raw mut (*old_arch).sp,
            (*new_arch).sp,
            newthread,
            &raw mut (*old_arch).unsafe_sp,
            (*new_arch).unsafe_sp,
        );
        #[cfg(not(sanitize = "safestack"))]
        x86_64_context_switch(&raw mut (*old_arch).sp, (*new_arch).sp, newthread);
    }
}

/// Saves `thread`'s user extended register and segment selector state (`arch_save_user_state`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.EM == 0`, `CR0.TS == 0`,
/// [`registers::extended_register_init`] has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is
/// enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized
/// to the calling CPU's `x86_percpu` (with user `%gs.base` in `IA32_KERNEL_GS_BASE`), interrupts
/// are disabled, and `thread` points to the current live (not freed) `Thread` with exclusive access
/// to its saved user state.
#[inline(always)]
pub unsafe fn save_user_state(thread: *mut Thread) {
    // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `extended_register_init` has
    // completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is enabled, `CR4.FSGSBASE == 1` when
    // `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized, interrupts are disabled, and
    // `thread` is the current thread with exclusive access to its 64-byte-aligned
    // `extended_register_buffer` and `fs_base`/`gs_base`.
    unsafe {
        let arch = thread_arch(thread);
        registers::extended_register_save_state(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
        );
        // Not saving debug state because the arch_thread_t's debug state is authoritative.
        segment_selector_save_state(thread);
    }
}

/// Restores `thread`'s user segment selector, debug, and extended register state
/// (`arch_restore_user_state`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
/// [`registers::extended_register_init`] has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is
/// enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized
/// to the calling CPU's `x86_percpu` (with user `%gs.base` in `IA32_KERNEL_GS_BASE`), interrupts
/// are disabled, and `thread` points to the current live (not freed) `Thread` with shared access to
/// its valid saved user state (`fs_base` and `gs_base` are canonical addresses, `debug_state` was
/// validated by `validate_debug_state` if `track_debug_state` is true, and
/// `extended_register_buffer` contains a valid extended register state image created by
/// [`registers::extended_register_init_state`],
/// [`registers::extended_register_init_state_from_bv`], or
/// [`registers::extended_register_save_state`]).
#[inline(always)]
pub unsafe fn restore_user_state(thread: *const Thread) {
    // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
    // `extended_register_init` has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is enabled,
    // `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized,
    // interrupts are disabled, and `thread` is the current thread with valid saved user state
    // (including a 64-byte-aligned `extended_register_buffer` containing a valid extended register
    // state image).
    unsafe {
        segment_selector_restore_state(thread);
        debug_restore_state(thread);
        let arch = thread_arch_const(thread);
        registers::extended_register_restore_state(
            (&raw const (*arch).extended_register_buffer).cast::<u8>(),
        );
    }
}

/// Records `gregs` with `source` as the suspended general registers pointer for `thread`
/// (`arch_set_suspended_general_regs`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` with exclusive
/// access to its `suspended_general_regs` and `general_regs_source` fields (e.g., holding
/// `thread`'s lock), and `gregs` is a non-null pointer to a live `Iframe` or `SyscallRegs`
/// matching `source` (`GeneralRegsSource::Iframe` or `GeneralRegsSource::Syscall`) that remains
/// valid and live until cleared by [`reset_suspended_general_regs`].
#[inline(always)]
pub unsafe fn set_suspended_general_regs(
    thread: *mut Thread,
    source: GeneralRegsSource,
    gregs: *mut c_void,
) {
    // SAFETY: Caller guarantees `thread` is valid and its `suspended_general_regs` and
    // `general_regs_source` fields are uniquely accessed.
    unsafe {
        let arch = thread_arch(thread);
        debug_assert!((*arch).suspended_general_regs.gregs.is_null());
        debug_assert!(!gregs.is_null());
        // `GeneralRegsSource` only has three variants (`None = 0`, `Iframe = 1`, `Syscall = 2`),
        // so a static message avoids an address-taken `core::fmt::Arguments` SafeStack frame.
        debug_assert!(
            source == GeneralRegsSource::Iframe || source == GeneralRegsSource::Syscall,
            "invalid source 0"
        );
        (*arch).general_regs_source = source;
        (*arch).suspended_general_regs.gregs = gregs;
    }
}

/// Clears `thread`'s suspended general registers pointer (`arch_reset_suspended_general_regs`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` with exclusive
/// access to its `suspended_general_regs` and `general_regs_source` fields (e.g., holding
/// `thread`'s lock).
#[inline(always)]
pub unsafe fn reset_suspended_general_regs(thread: *mut Thread) {
    // SAFETY: Caller guarantees `thread` is valid and its `suspended_general_regs` and
    // `general_regs_source` fields are uniquely accessed.
    unsafe {
        let arch = thread_arch(thread);
        (*arch).general_regs_source = GeneralRegsSource::None;
        (*arch).suspended_general_regs.gregs = core::ptr::null_mut();
    }
}
