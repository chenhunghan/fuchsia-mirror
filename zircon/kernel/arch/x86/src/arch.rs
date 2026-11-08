// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2015 Intel Corporation
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(not(target_feature = "cmpxchg16b"))]
compile_error!("missing cmpxchg16b target feature (-mcx16)");

use core::sync::atomic::{AtomicU32, Ordering};
use debug::ltracef;
use init::{LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_THREADING, LkInitFlags, LkInitLevel};
use kprint::kprintln;

use super::arch_ffi::{
    lk_secondary_cpu_entry, x86_cpu_feature_init, x86_cpu_feature_late_init_percpu,
    x86_feature_debug, x86_mmu_early_init, x86_mmu_init, x86_mmu_percpu_init, x86_mmu_prevm_init,
    x86_uspace_entry,
};
use super::handoff::ZX_TLS_STACK_GUARD_OFFSET;
#[cfg(sanitize = "safestack")]
use super::handoff::ZX_TLS_UNSAFE_SP_OFFSET;
use super::mp_ffi::{x86_apic_id_to_cpu_num, x86_init_percpu, x86_percpu_for};
use super::registers::{
    X86_CR0_CD, X86_FLAGS_IF, X86_FLAGS_IOPL_SHIFT, X86_MSR_IA32_FS_BASE, X86_MSR_IA32_GS_BASE,
};
use super::vm::{is_kernel_address, is_valid_user_pc};
use super::x86::{read_msr, write_msr, x86_cli, x86_get_cr0, x86_hlt};
use super::{Iframe, USER_CODE_64_SELECTOR, USER_DATA_SELECTOR, UserEntryState};
use crate::kernel::mp;
use crate::kernel::thread::{self, Thread};

const LOCAL_TRACE: u32 = 0;

/// Early architecture initialization on the boot CPU before the VM and heap are initialized
/// (`arch_early_init`).
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during early boot before the
/// VM and heap are initialized.
#[inline]
pub unsafe fn early_init() {
    // SAFETY: Caller guarantees single-CPU early boot context on CPU 0.
    unsafe { x86_mmu_early_init() };

    // Mark the boot cpu as online here after global constructors have run.
    mp::set_curr_cpu_online();
}

/// Pre-VM architecture initialization on the boot CPU (`arch_prevm_init`).
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during pre-VM boot.
#[inline]
pub unsafe fn prevm_init() {
    // SAFETY: Caller guarantees single-CPU pre-VM boot context on CPU 0.
    unsafe {
        x86_cpu_feature_init();
        // Perform some MMU initialization that is dependent on having done the previous feature
        // init.
        x86_mmu_prevm_init();
    }
}

/// Main architecture initialization on the boot CPU after the VM and heap are available
/// (`arch_init`).
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 while only CPU 0 is online
/// and after early feature, kernel address space, and VM initialization have completed.
#[inline]
pub unsafe fn init() {
    let model = super::feature::x86_get_model();
    kprintln!(
        "Processor Model Info: type {:#x} family {:#x} model {:#x} stepping {:#x}",
        model.processor_type,
        model.family,
        model.model,
        model.stepping
    );
    kprintln!(
        "\tdisplay_family {:#x} display_model {:#x}",
        model.display_family,
        model.display_model
    );

    // SAFETY: Caller guarantees single-CPU `arch_init` context on CPU 0 after early feature and
    // VM initialization.
    unsafe {
        x86_feature_debug();
        x86_mmu_init();
        super::descriptor::gdt_setup();
        super::idt::idt_setup_readonly();
    }
}

/// Late per-CPU architecture initialization on the boot CPU (`arch_late_init_percpu`).
///
/// # Safety
///
/// The caller must ensure that this function is called on CPU 0 during late arch initialization
/// after topology and platform initialization have completed.
#[inline]
pub unsafe fn late_init_percpu() {
    // Call per-CPU init function for the boot CPU.
    // SAFETY: Caller guarantees late arch initialization context on CPU 0.
    unsafe { x86_cpu_feature_late_init_percpu() };
}

/// Prepares `out` and `%fs.base` for entering user space with `state` (`arch_prepare_uspace`).
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 on the thread about to enter user
/// space, `out` points to valid, writable memory for an [`Iframe`], and `state.tp` is a canonical
/// virtual address.
#[inline]
pub unsafe fn prepare_uspace(state: &UserEntryState, out: *mut Iframe) {
    debug_assert!(super::vm::is_vaddr_canonical(state.tp as usize));
    // The %fs.base (thread pointer) isn't in the iframe. It's directly in the
    // CPU's register for the current thread. It's always accessible via MSR,
    // and accessible directly with wrfsbase only on newer CPUs.
    // SAFETY: `X86_MSR_IA32_FS_BASE` is a valid architectural MSR at CPL 0
    // and `state.tp` is canonical.
    unsafe { write_msr(X86_MSR_IA32_FS_BASE, state.tp) };

    // SAFETY: Caller guarantees `out` points to valid writable storage for an `Iframe`.
    unsafe {
        out.write(Iframe {
            rdi: state.arg1,
            rsi: state.arg2,
            r15: state.abi_reg,

            ip: state.pc,
            cs: USER_CODE_64_SELECTOR,

            // Default user space flags: IOPL 0; interrupts enabled.
            flags: (0 << X86_FLAGS_IOPL_SHIFT) | X86_FLAGS_IF,

            user_sp: state.sp,
            user_ss: USER_DATA_SELECTOR,
            ..Iframe::default()
        });
    }
}

/// Enters user space using the register state in `iframe` (`arch_enter_uspace`).
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled, `%gs.base` and the current CPU's TSS
/// `rsp0` are initialized for the current thread, and `iframe` points to a valid user-space
/// [`Iframe`] with a canonical user `ip`, `cs == USER_CODE_64_SELECTOR`,
/// `user_ss == USER_DATA_SELECTOR`,
/// `flags & X86_FLAGS_IOPL_MASK == 0`, and `flags & X86_FLAGS_IF != 0`.
/// Under `#[cfg(sanitize = "safestack")]`, the caller must also ensure that `%gs.base`'s
/// `current_thread` slot points to a valid running [`Thread`] with an initialized `KernelStack`.
#[inline]
pub unsafe fn enter_uspace(iframe: *const Iframe) -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());

    // SAFETY: Caller guarantees `iframe` points to a valid `Iframe`.
    let (ip, user_sp, cs, user_ss, flags) = unsafe {
        ((*iframe).ip, (*iframe).user_sp, (*iframe).cs, (*iframe).user_ss, (*iframe).flags)
    };

    ltracef!("entry {:#x} user stack {:#x}\n", ip, user_sp);
    // SAFETY: Caller guarantees `%gs.base` is initialized for the current CPU.
    ltracef!("kernel stack {:#x}\n", unsafe { super::descriptor::get_tss_sp() });
    #[cfg(sanitize = "safestack")]
    // SAFETY: `thread::current_get()` returns the current running `Thread`.
    let unsafe_top = unsafe { thread::get_unsafe_stack_top(thread::current_get()) };
    #[cfg(sanitize = "safestack")]
    ltracef!("kernel unsafe stack {:#x}\n", unsafe_top);

    // Check that we are accessing userspace code.
    assert!(is_valid_user_pc(ip as usize));
    assert!(cs == USER_CODE_64_SELECTOR && user_ss == USER_DATA_SELECTOR);
    assert!(
        flags & super::registers::X86_FLAGS_IOPL_MASK == 0
            && flags & super::registers::X86_FLAGS_IF != 0
    );

    // Check that we're still pointed at the kernel gs.
    // SAFETY: `X86_MSR_IA32_GS_BASE` is a valid architectural MSR at CPL 0.
    debug_assert!(is_kernel_address(unsafe { read_msr(X86_MSR_IA32_GS_BASE) } as usize));

    // Check that the kernel stack is set properly.
    // SAFETY: Caller guarantees `%gs.base` is initialized for the current CPU.
    debug_assert!(is_kernel_address(unsafe { super::descriptor::get_tss_sp() }));

    #[cfg(sanitize = "safestack")]
    // SAFETY: Interrupts are disabled, `iframe` is valid, and `unsafe_top` is the top of the
    // current thread's unsafe stack.
    unsafe {
        // Set the kernel unsafe stack back to the top as we enter user space.
        x86_uspace_entry(iframe, unsafe_top)
    }

    #[cfg(not(sanitize = "safestack"))]
    // SAFETY: Interrupts are disabled and `iframe` points to a valid user-space `Iframe`.
    unsafe {
        x86_uspace_entry(iframe)
    }
}

/// # Safety
///
/// The caller must ensure that `%gs.base` and `x86_init_percpu(cpu_num)` have been initialized on
/// `cpu_num`, `aps_still_booting` points to the active bootstrap bitmask until this CPU's bit is
/// cleared, and `thread` points to the secondary CPU's live (not freed) bootstrap `Thread`.
#[inline(never)]
unsafe fn finish_secondary_entry(
    aps_still_booting: *const AtomicU32,
    thread: *mut Thread,
    cpu_num: u32,
) -> ! {
    // Mark this cpu as online so MP code can try to deliver IPIs.
    // Mark here so any code waiting for the cpu to be started will see the cpu
    // online after the atomic below.
    mp::set_curr_cpu_online();

    let cpu_mask = mp::cpu_num_to_mask(cpu_num);

    // Signal that this CPU is initialized. It is important that after this
    // operation, we do not touch any resources associated with bootstrap
    // besides our Thread and stack, since this is the checkpoint the
    // bootstrap process uses to identify completion.
    // SAFETY: `aps_still_booting` points to a valid `AtomicU32` on the BSP's stack until this
    // `fetch_and` clears `cpu_num`'s bit. The temporary shared reference does not outlive the
    // `fetch_and` expression.
    let old_val = unsafe { (*aps_still_booting).fetch_and(!cpu_mask, Ordering::SeqCst) };
    'fail: {
        if old_val == 0 {
            // If the value is already zero, then booting this CPU timed out.
            core::hint::cold_path();
            break 'fail;
        }

        // Defer configuring memory settings until after the atomic_and above.
        // This ensures that we were in no-fill cache mode for the duration of early
        // AP init.
        // SAFETY: Called at CPL 0 on the secondary CPU during bootstrap.
        debug_assert!((unsafe { x86_get_cr0() } & X86_CR0_CD) != 0);
        // SAFETY: Configures per-CPU MMU settings on the newly online secondary CPU.
        unsafe { x86_mmu_percpu_init() };

        // Load the appropriate PAT/MTRRs. This must happen after init_percpu, so
        // that this CPU is considered online.
        // SAFETY: `cpu_num` has completed `x86_init_percpu` and is marked online.
        unsafe { super::mmu_mem_types::pat_sync(cpu_mask) };

        // Run early secondary cpu init routines up to the threading level.
        // SAFETY: Runs secondary CPU init hooks on `cpu_num` prior to entering threading.
        unsafe {
            crate::top::init::lk_init_level(
                LkInitFlags::SecondaryCpus,
                LK_INIT_LEVEL_EARLIEST,
                LkInitLevel(LK_INIT_LEVEL_THREADING.0 - 1),
            );
            thread::secondary_cpu_init_early(thread);
            lk_secondary_cpu_entry();
        }
    }

    // lk_secondary_cpu_entry only returns on an error, halt the core in this
    // case.
    crate::arch_rs::disable_ints();
    loop {
        x86_hlt();
    }
}

/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 on a newly started Application
/// Processor with `aps_still_booting` pointing to a valid `AtomicU32` bitmask and `thread`
/// pointing to this AP's live (not freed) bootstrap `Thread`.
///
/// 1. Nothing may access `%gs` before the `write_msr(X86_MSR_IA32_GS_BASE, ...)` call below.
///    INIT resets GS base to 0, so any percpu access dereferences linear address 0 + offset
///    (e.g. panicking on `cpu_num <= 0`).
/// 2. The `ZX_TLS_UNSAFE_SP_OFFSET` write must occur before the `write_msr` call so that once the
///    GS base is set, the AP's `kernel_unsafe_sp` is immediately available to out-of-line
///    functions with unsafe stack allocations.
#[sanitize(safestack = "off")]
pub unsafe fn secondary_entry(aps_still_booting: *const AtomicU32, thread: *mut Thread) -> ! {
    // Would prefer this to be in init_percpu, but there is a dependency on a
    // page mapping existing, and the BP calls that before the VM subsystem is
    // initialized.
    super::lapic::apic_local_init();

    let local_apic_id = super::lapic::apic_local_id() as u32;
    // SAFETY: `x86_apic_id_to_cpu_num` reads the APIC ID table initialized before AP bringup.
    let cpu_num = unsafe { x86_apic_id_to_cpu_num(local_apic_id) };
    if cpu_num <= 0 {
        // If we could not find our CPU number, do not proceed further.
        // SAFETY: Disables interrupts and halts the unrecognized AP at CPL 0.
        // Note: we cannot panic here, because panic accesses percpu variables and `%gs.base` is 0.
        unsafe {
            x86_cli();
            loop {
                x86_hlt();
            }
        }
    }

    let cpu_num = cpu_num as u32;

    // Set %gs.base to our percpu struct.
    // SAFETY: `cpu_num > 0` is a valid allocated AP CPU number, and `BP_PERCPU` was initialized on
    // CPU 0 before AP bringup (`ZX_TLS_STACK_GUARD_OFFSET` lies within `size_of::<X86Percpu>()`).
    unsafe {
        let percpu = x86_percpu_for(cpu_num).cast::<u8>();

        // Copy the stack-guard value from the boot CPU's percpu.
        let bp_percpu = super::mp_ffi::BP_PERCPU.as_mut_ptr().cast_const();
        let stack_guard = *bp_percpu.add(ZX_TLS_STACK_GUARD_OFFSET).cast::<usize>();
        *percpu.add(ZX_TLS_STACK_GUARD_OFFSET).cast::<usize>() = stack_guard;

        #[cfg(sanitize = "safestack")]
        let unsafe_top = thread::get_unsafe_stack_top(thread);
        #[cfg(sanitize = "safestack")]
        {
            // Set up the initial unsafe stack pointer.
            *percpu.add(ZX_TLS_UNSAFE_SP_OFFSET).cast::<usize>() = unsafe_top;
        }

        write_msr(X86_MSR_IA32_GS_BASE, percpu.expose_provenance() as u64);

        #[cfg(sanitize = "safestack")]
        {
            debug_assert!(unsafe_top.is_multiple_of(16));
        }
    }

    // SAFETY: `%gs.base`, `stack_guard`, and `kernel_unsafe_sp` are initialized for `cpu_num`.
    unsafe {
        x86_init_percpu(cpu_num);

        // Now do the rest of the work, in a function that is free to
        // use %gs in its code.
        finish_secondary_entry(aps_still_booting, thread, cpu_num)
    }
}

#[cfg(console_enabled)]
pub mod console {
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_status::Status;
    use zx_types::ZX_OK;

    use crate::arch_rs::InterruptDisableGuard;
    use crate::arch_rs::x86::Iframe;
    use crate::arch_rs::x86::arch_ffi::console::{x86_print_frame, x86_request_context};
    use crate::arch_rs::x86::arch_ffi::x86_feature_debug;
    use crate::arch_rs::x86::msr::{read_msr_safe_on_cpu, write_msr_safe_on_cpu};
    use crate::backtrace::Backtrace;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::mp;
    use crate::kernel::percpu::PerCpu;
    use crate::kernel::types::cpu_num_t;
    use crate::platform_rs::timer::DurationMono;

    /// Prints an x86 interrupt frame to the kernel console.
    fn print_frame(frame: &Iframe) {
        // SAFETY: `frame` is a valid `Iframe` reference.
        unsafe { x86_print_frame(frame) };
    }

    /// Prints x86 CPU feature and vulnerability information to the kernel console.
    fn feature_debug() {
        // SAFETY: Reads CPUID/feature state initialized during early boot with no caller
        // preconditions.
        unsafe { x86_feature_debug() };
    }

    /// Context for a CPU, representing its state at a given point in time.
    #[repr(C)]
    pub struct CpuContext {
        frame: Iframe,
        backtrace: Backtrace,
    }
    zr::static_assert!(core::mem::offset_of!(CpuContext, frame) == 0);
    zr::static_assert!(core::mem::offset_of!(CpuContext, backtrace) == 176);

    zr::static_assert!(core::mem::size_of::<CpuContext>() == 448);
    zr::static_assert!(core::mem::align_of::<CpuContext>() == 8);

    fn get_context(argv: &[CmdArgs], _flags: u32) -> c_int {
        let print_usage = || {
            kprintln!("usage:");
            kprintln!("{:s} context <cpu_id> <timeout_ms>", argv[0].as_str());
        };

        if argv.len() < 4 {
            kprintln!("not enough arguments");
            print_usage();
            return 1;
        }

        let target = argv[2].arg_uint as cpu_num_t;
        if (target as usize) >= PerCpu::processor_count() {
            kprintln!("invalid cpu_id: {target}");
            return 1;
        }

        let timeout_ms = argv[3].arg_uint;
        kprintln!("requesting context of CPU-{target} with timeout {timeout_ms}ms");

        let mut context = core::mem::MaybeUninit::<CpuContext>::zeroed();
        let status = {
            let _irqd = InterruptDisableGuard::new();
            // SAFETY: Interrupts are disabled by `_irqd`, `target < PerCpu::processor_count()`, and
            // `context.as_mut_ptr()` points to a zero-initialized `CpuContext` valid for writes.
            unsafe {
                x86_request_context(
                    target,
                    DurationMono::from_millis(timeout_ms as i64).0,
                    context.as_mut_ptr(),
                )
            }
        };
        if status != ZX_OK {
            kprintln!("error: {status}");
            return 1;
        }
        // SAFETY: `context` was zero-initialized and populated by `x86_request_context`.
        let context = unsafe { context.assume_init_ref() };
        context.backtrace.print();
        print_frame(&context.frame);

        0
    }

    /// Kernel console command handler for `cpu` subcommands.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `argv` points to a valid array of `CmdArgs` of length `argc`,
    /// with `argc >= 1`.
    unsafe extern "C" fn cmd_cpu(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            core::hint::cold_path();
            return Status::INVALID_ARGS.into_raw();
        }
        // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs`
        // elements, and `argc > 0` and `!argv.is_null()` are verified above.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let usage = || -> c_int {
            core::hint::cold_path();
            kprintln!("usage:");
            kprintln!("{:s} features", argv[0].as_str());
            kprintln!("{:s} rdmsr <cpu_id> <msr_id>", argv[0].as_str());
            kprintln!("{:s} wrmsr <cpu_id> <msr_id> <value>", argv[0].as_str());
            kprintln!("{:s} lbr <subcommand>", argv[0].as_str());
            kprintln!("{:s} context <cpu_id> <timeout_ms>", argv[0].as_str());
            Status::INVALID_ARGS.into_raw()
        };
        'usage: {
            if argv.len() < 2 {
                kprintln!("not enough arguments");
                break 'usage;
            }

            if argv[1].as_str() == "features" {
                feature_debug();
            } else if argv[1].as_str() == "rdmsr" {
                if argv.len() != 4 {
                    break 'usage;
                }
                let Ok(cpu) = cpu_num_t::try_from(argv[2].arg_uint) else {
                    kprintln!("invalid cpu_id: {}", argv[2].arg_uint);
                    return Status::INVALID_ARGS.into_raw();
                };
                if !mp::is_cpu_online(cpu) {
                    kprintln!("CPU {cpu} is not online");
                    return Status::BAD_STATE.into_raw();
                }
                let Ok(msr_id) = u32::try_from(argv[3].arg_uint) else {
                    kprintln!("invalid msr_id: {:x}h", argv[3].arg_uint);
                    return Status::INVALID_ARGS.into_raw();
                };

                // SAFETY: Operator-invoked kernel debug console command; `read_msr_safe_on_cpu`
                // catches `#GP(0)` faults on unimplemented or inaccessible MSRs.
                match unsafe { read_msr_safe_on_cpu(cpu, msr_id) } {
                    Ok(val) => {
                        kprintln!("CPU {cpu} RDMSR {msr_id:x}h val {val:x}h");
                    }
                    Err(err) => {
                        kprintln!("CPU {cpu} RDMSR {msr_id:x}h failed: {}", err.into_raw());
                        return err.into_raw();
                    }
                }
            } else if argv[1].as_str() == "wrmsr" {
                if argv.len() != 5 {
                    break 'usage;
                }
                let Ok(cpu) = cpu_num_t::try_from(argv[2].arg_uint) else {
                    kprintln!("invalid cpu_id: {}", argv[2].arg_uint);
                    return Status::INVALID_ARGS.into_raw();
                };
                if !mp::is_cpu_online(cpu) {
                    kprintln!("CPU {cpu} is not online");
                    return Status::BAD_STATE.into_raw();
                }
                let Ok(msr_id) = u32::try_from(argv[3].arg_uint) else {
                    kprintln!("invalid msr_id: {:x}h", argv[3].arg_uint);
                    return Status::INVALID_ARGS.into_raw();
                };
                let val = argv[4].arg_uint;

                kprintln!("CPU {cpu} WRMSR {msr_id:x}h val {val:x}h");
                // SAFETY: Operator-invoked kernel debug console command; `write_msr_safe_on_cpu`
                // catches `#GP(0)` faults on unimplemented MSRs or invalid `val` bits.
                if let Err(err) = unsafe { write_msr_safe_on_cpu(cpu, msr_id, val) } {
                    kprintln!("CPU {cpu} WRMSR {msr_id:x}h failed: {}", err.into_raw());
                    return err.into_raw();
                }
            } else if argv[1].as_str() == "context" {
                return get_context(argv, flags);
            } else {
                kprintln!("unknown command");
                break 'usage;
            }

            return ZX_OK;
        }

        usage()
    }

    static_command!(
        CMD_CPU,
        c"cpu".as_ptr(),
        c"cpu test commands".as_ptr(),
        cmd_cpu,
        CMD_AVAIL_NORMAL
    );
}
