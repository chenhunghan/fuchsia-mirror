// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handoff_ffi::{InitializeBootCpuid, load_startup_gdt, load_startup_idt};
use super::idt::_idt_startup;
use super::mp::{ZX_TLS_STACK_GUARD_OFFSET, ZX_TLS_UNSAFE_SP_OFFSET, init_percpu};
use super::registers::X86_MSR_IA32_GS_BASE;
use super::x86::{write_msr, x86_read_gs_offset64};

/// Bootstraps the x86 boot CPU (CPU 0) immediately after physboot handoff.
///
/// # Safety
///
/// The caller must ensure that this function is called once at CPL 0 on the bootstrap processor
/// during early kernel handoff, with `%gs.base` still pointing to physboot's temporary per-CPU
/// ABI region.
#[sanitize(safestack = "off")]
pub unsafe fn post_handoff_bootstrap() {
    // Best to do this early. See docstring for more details.
    // SAFETY: Caller guarantees early bootstrap context at CPL 0 on CPU 0.
    unsafe { load_startup_gdt() };

    // Before setting %gs.base to &bp_percpu, copy over the unsafe stack pointer
    // and stack guard set by physboot.
    //
    // What physboot handed off was a temporary region of memory covering the
    // subset of `x86_percpu` dealing in the thread ABI.
    // SAFETY: Physboot initialized the temporary `%gs` region at `ZX_TLS_STACK_GUARD_OFFSET` and
    // `ZX_TLS_UNSAFE_SP_OFFSET`, and `bp_percpu` is uniquely accessed on CPU 0 during early boot.
    unsafe {
        let stack_guard = x86_read_gs_offset64::<ZX_TLS_STACK_GUARD_OFFSET>() as usize;
        let kernel_unsafe_sp = x86_read_gs_offset64::<ZX_TLS_UNSAFE_SP_OFFSET>() as usize;
        let bp_percpu = super::mp::percpu_for(0).cast::<u8>();
        *bp_percpu.add(ZX_TLS_STACK_GUARD_OFFSET).cast::<usize>() = stack_guard;
        *bp_percpu.add(ZX_TLS_UNSAFE_SP_OFFSET).cast::<usize>() = kernel_unsafe_sp;
        write_msr(X86_MSR_IA32_GS_BASE, bp_percpu.expose_provenance() as u64);
    }

    // Set up the idt.
    // SAFETY: Single-threaded early bootstrap on CPU 0 before `_idt_startup` is loaded into IDTR.
    unsafe {
        (&mut *_idt_startup.get()).setup();
        load_startup_idt();
    }

    // Initialize CPUID value cache - and do so before functions (like
    // x86_init_percpu) begin to access CPUID data.
    // SAFETY: Single-threaded early bootstrap on CPU 0 before any CPUID cache readers run.
    unsafe { InitializeBootCpuid() };

    // Assign this core CPU# 0 and initialize its per cpu state.
    // SAFETY: `%gs.base`, startup GDT/IDT, and boot CPUID cache are initialized for CPU 0.
    unsafe { init_percpu(0) };
}
