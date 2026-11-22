// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Main entry point to the OS. Initializes modules in order and creates
//! the default thread.

use core::ffi::c_void;
use core::ptr::null_mut;

use debug::dprintf;
use handoff::PhysHandoff;
use init::{
    LK_INIT_LEVEL_ARCH, LK_INIT_LEVEL_ARCH_EARLY, LK_INIT_LEVEL_ARCH_LATE,
    LK_INIT_LEVEL_ARCH_PREVM, LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_HEAP, LK_INIT_LEVEL_INTC,
    LK_INIT_LEVEL_KERNEL, LK_INIT_LEVEL_LAST, LK_INIT_LEVEL_PLATFORM, LK_INIT_LEVEL_PLATFORM_EARLY,
    LK_INIT_LEVEL_PLATFORM_PREVM, LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_TOPOLOGY,
    LK_INIT_LEVEL_USER, LK_INIT_LEVEL_VM, LK_INIT_LEVEL_VM_PREHEAP, LkInitFlags,
};
use kprint::kprint;
use libarch::EarlyTicks;

use crate::arch_rs::{arch_early_init, arch_init, arch_late_init_percpu, arch_prevm_init};
use crate::counters::define_kcounter;
use crate::kernel::init::kernel_init;
use crate::kernel::scheduler::{Scheduler, enable_new_wakeup_accounting};
use crate::kernel::thread::{self, DEFAULT_PRIORITY};
use crate::kernel::types::{BOOT_CPU_ID, SMP_MAX_CPUS, cpu_num_to_mask};
use crate::lockup_detector::{lockup_init, lockup_percpu_init};
use crate::platform_rs::timer::{platform_convert_early_ticks, timer_current_mono_ticks};
use crate::platform_rs::{platform_early_init, platform_init, platform_prevm_init};
use crate::top::handoff::{BootGlobal, end_handoff, post_handoff_bootstrap};
use crate::top::init::{lk_init_level, lk_primary_cpu_init_level};
use crate::userabi::userboot_init;

static CONSTRUCTORS_CALLED: BootGlobal<bool> = BootGlobal::new(false);

/// Invokes the kernel's global C++ constructors from the handoff `DT_INIT_ARRAY`.
///
/// # Safety
///
/// Must be called once on the boot CPU during early boot after `post_handoff_bootstrap`,
/// `thread::init_early`, and `dlog_init_early`, and before secondary CPUs start.
unsafe fn call_constructors() {
    // SAFETY: Caller guarantees single-call early-boot execution while `gPhysHandoff` is valid and
    // no other CPUs are running.
    unsafe {
        PhysHandoff::get().call_init_array();
        CONSTRUCTORS_CALLED.set(true);
    }
}

/// Returns whether the kernel's global constructors have completed.
// SAFETY: Exported symbol matches `extern "C" bool ConstructorsCalled()` in
// `<lib/cxxabi-dynamic-init/cxxabi-dynamic-init.h>` and has no other definition.
#[unsafe(no_mangle)]
pub extern "C" fn ConstructorsCalled() -> bool {
    // SAFETY: `CONSTRUCTORS_CALLED` is only mutated once on the primary CPU during single-threaded
    // early boot before secondary CPUs or threads exist.
    unsafe { CONSTRUCTORS_CALLED.get() }
}

static SECONDARY_IDLE_THREAD_COUNT: BootGlobal<u32> = BootGlobal::new(0);

define_kcounter!(TIMELINE_PHYSBOOT_HANDOFF, "boot.timeline.physboot-handoff", Sum);
define_kcounter!(TIMELINE_THREADING, "boot.timeline.threading", Sum);
define_kcounter!(TIMELINE_INIT, "boot.timeline.init", Sum);

unsafe extern "C" {
    fn dlog_init_early();
    fn topology_init();
}

/// Called from arch code.
///
/// # Safety
///
/// `handoff` must be a valid non-null pointer to the `PhysHandoff` passed by physboot, and this
/// function must be called once on the boot CPU at kernel entry with interrupts disabled.
// SAFETY: Exported symbol matches `extern "C" void lk_main(PhysHandoff*) __NO_RETURN` in
// `<lk/main.h>` and has no other definition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_main(handoff: *mut PhysHandoff) -> ! {
    let handoff_time = EarlyTicks::get();

    #[cfg(any(lk_debuglevel = "2", lk_debuglevel_gt2))]
    {
        // Check this even before PostHandoffBootstrap, but after the time sample.
        if boot_options::BootOptions::get().debug_boot_spin {
            core::hint::cold_path();
            // Wait for gDebugBootSpinReady to be changed magically from outside (e.g. by
            // the kernel developer using the debugger interface to the emulator).
            while !super::handoff::DEBUG_BOOT_SPIN_READY.is_ready() {}
        }
    }

    // SAFETY: `handoff` is valid and we are in single-threaded early boot on the boot CPU.
    unsafe {
        post_handoff_bootstrap(handoff);
    }

    // After post_handoff_bootstrap(), gPhysHandoff should now be set.
    // SAFETY: `post_handoff_bootstrap` just initialized `gPhysHandoff` on the boot CPU with no
    // concurrent writers.
    debug_assert!(!unsafe { PhysHandoff::as_ptr() }.is_null());

    // Care is taken to do this top-level in lk_main() and after
    // post_handoff_bootstrap(). This needs to be top-level as the updating of the
    // stack guard needs to happen in a function that does not return (e.g.,
    // lk_main()) since on return the invalidated guard would be compared
    // against the new one. Further, post_handoff_bootstrap() will initialize enough of
    // the kernel to give choose_stack_guard() access to supported means of
    // hardware randomness.
    //
    // TODO(https://fxbug.dev/398677049): This should be done in physboot when it
    // has access to a source of randomness (as it will for KASLR support) at the
    // time it sets up the compiler ABI.
    {
        let stack_guard = crate::arch_rs::get_current_compiler_thread_pointer()
            .wrapping_byte_offset(crate::arch_rs::ZX_TLS_STACK_GUARD_OFFSET)
            .cast::<usize>();
        // SAFETY: `stack_guard` points to the current CPU/thread TLS stack guard slot with
        // interrupts disabled, and `lk_main` never returns so the updated guard will not be
        // checked against an old frame.
        unsafe {
            *stack_guard = super::debug::choose_stack_guard();
        }
    }

    // Initialize debug tracing (if enabled) as early as possible. This allows
    // debug tracing to be used before the debug log comes up, and before global
    // constructors are executed.  Note that if debug tracing is configured to be
    // persistent, then trace records will be dropped until we get to the point
    // that the ZBI is processed and our NVRAM location is discovered.
    crate::jtrace::jtrace_init();

    // SAFETY: `lk_main` executes once on CPU 0 with interrupts disabled during single-threaded
    // early boot before global constructors, threading, or secondary CPUs are active, with early
    // arch thread-pointer / `%gs.base` state initialized by early boot code.
    unsafe {
        // get us into some sort of thread context so Thread::Current works.
        thread::init_early();

        lk_main_post_thread_init(handoff_time)
    }
}

/// Completes `lk_main` after `thread::init_early()` has installed the boot CPU's initial `Thread`
/// in the architectural thread-pointer register.
///
/// Kept out of line from `lk_main` because `thread::init_early()` updates `tpidr_el1` / `tp`, while
/// LLVM models `__builtin_thread_pointer()` (`llvm.thread.pointer`) as `speculatable` and
/// `memory(none)` within a single function.
///
/// # Safety
///
/// Must be called once from `lk_main` on the boot CPU with interrupts disabled immediately after
/// `thread::init_early()`.
#[inline(never)]
unsafe fn lk_main_post_thread_init(handoff_time: EarlyTicks) -> ! {
    // SAFETY: Caller guarantees execution once on the boot CPU with interrupts disabled after
    // `thread::init_early()` and before threading or secondary CPUs are active.
    unsafe {
        // Now that Thread::Current works, jtrace is allowed to capture TIDs and
        // disable preemption while recording entries.
        crate::jtrace::jtrace_set_after_thread_init_early();

        // bring the debuglog up early so we can safely printf
        dlog_init_early();

        // deal with any static constructors
        call_constructors();
    }

    // we can safely printf now since we have the debuglog, the current thread set
    // which holds (a per-line buffer), and global ctors finished (some of the
    // printf machinery depends on ctors right now).
    // NOTE: botanist depends on this string being printed to serial. If this changes,
    // that code must be changed as well. See https://fxbug.dev/42138089#c20.
    dprintf!(ALWAYS, "printing enabled\n");

    kprint!("{:s}", crate::top::boot_constants::kernel_version_ident());

    // At this point the physmap (set up in start.S) is available and all static
    // constructors (if needed) have been run.

    // SAFETY: Ordered single-CPU primary initialization prior to `driver_handoff_early` and serial
    // bringup; interrupts are disabled and `PhysHandoff::as_ptr()` is valid.
    unsafe {
        lk_primary_cpu_init_level(LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_ARCH_EARLY - 1);

        // Carry out any early architecture-specific and platform-specific init
        // required to get the boot CPU and platform into a known state.
        arch_early_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_EARLY, LK_INIT_LEVEL_PLATFORM_EARLY - 1);

        platform_early_init();
        crate::dev_init::driver_handoff_early(PhysHandoff::get());
        lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM_EARLY, LK_INIT_LEVEL_ARCH_PREVM - 1);
    }

    // At this point, the kernel command line and serial are set up.

    dprintf!(INFO, "\nwelcome to Zircon\n\n");

    // Now that the platform clock driver is ready, convert the entry sample.
    TIMELINE_PHYSBOOT_HANDOFF.set(platform_convert_early_ticks(handoff_time) as u64);

    // Perform any additional arch and platform-specific set up that needs to be done
    // before virtual memory or the heap are set up.
    // SAFETY: Ordered single-CPU primary initialization on the boot CPU with interrupts disabled
    // while `PhysHandoff::as_ptr()` remains valid.
    unsafe {
        dprintf!(SPEW, "initializing arch pre-vm\n");
        arch_prevm_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_PREVM, LK_INIT_LEVEL_PLATFORM_PREVM - 1);
        dprintf!(SPEW, "initializing platform pre-vm\n");
        platform_prevm_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM_PREVM, LK_INIT_LEVEL_VM_PREHEAP - 1);

        // perform basic virtual memory setup
        dprintf!(SPEW, "initializing vm pre-heap\n");
        crate::vm::vm::vm_init_preheap();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_VM_PREHEAP, LK_INIT_LEVEL_HEAP - 1);

        // bring up the kernel heap
        dprintf!(SPEW, "initializing heap\n");
        crate::vm::heap::heap_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_HEAP, LK_INIT_LEVEL_VM - 1);

        // enable virtual memory
        dprintf!(SPEW, "initializing vm\n");
        // `vm_init` runs once, after `vm_init_preheap` and `heap_init` and before the
        // `LK_INIT_LEVEL_VM` hooks enable break-before-make. Physboot maps each `PhysVmar` with
        // its `PhysMapping` permissions, and no Rust reference into those mappings is live yet.
        crate::vm::vm::vm_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_VM, LK_INIT_LEVEL_INTC - 1);

        // Give handoff drivers an opportunity to use the VM to map memory.
        dprintf!(SPEW, "initializing handoff drivers post-vm\n");
        crate::dev_init::driver_handoff_post_vm(PhysHandoff::get());
        lk_primary_cpu_init_level(LK_INIT_LEVEL_INTC, LK_INIT_LEVEL_TOPOLOGY - 1);

        // Initialize the lockup detector, after the platform timer has been
        // configured, but before the topology subsystem has brought up other CPUs.
        dprintf!(SPEW, "initializing lockup detector on boot cpu\n");
        lockup_init();
        lockup_percpu_init();

        // Initialize the thread sampler. This needs to be done after the
        // platform timer is configured.
        crate::thread_sampler::sampler_percpu_init();

        // initialize the system topology
        dprintf!(SPEW, "initializing system topology\n");
        topology_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_TOPOLOGY, LK_INIT_LEVEL_KERNEL - 1);

        // initialize other parts of the kernel
        dprintf!(SPEW, "initializing kernel\n");
        kernel_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_KERNEL, LK_INIT_LEVEL_THREADING - 1);
    }

    // Mark the current CPU as being active, then create a thread to complete
    // system initialization
    dprintf!(SPEW, "creating bootstrap completion thread\n");
    // SAFETY: `kernel_init` has initialized the boot CPU's scheduler, `lk_main` runs with
    // interrupts disabled, and no scheduler queue lock is held.
    unsafe { Scheduler::set_curr_cpu_active(true) };
    // SAFETY: `bootstrap2` ignores its argument, so a null `arg` is safe to run on a new thread.
    let t = unsafe {
        thread::create_with_priority(
            c"bootstrap2".as_ptr(),
            bootstrap2,
            null_mut(),
            DEFAULT_PRIORITY,
        )
    }
    .expect("failed to create bootstrap2 thread");
    // As this thread will initialize per-CPU state, ensure that it runs on the boot CPU.
    // SAFETY: `t` was just created and is not scheduled until `resume` below, so it cannot have
    // exited, been joined, or been destroyed. It is detached only once, here.
    unsafe {
        t.set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID));
        let _ = t.detach();
        t.resume();
    }

    // become the idle thread and enable interrupts to start the scheduler
    // SAFETY: `lk_main` runs on the boot CPU's initial bootstrap thread with interrupts disabled,
    // the boot CPU already marked active in the scheduler, preemption disabled once by
    // `construct_first_thread`, and no locks or `ChainLockTransaction` held.
    unsafe { thread::current_become_idle() }
}

extern "C" fn bootstrap2(_arg: *mut c_void) -> i32 {
    debug_assert!(crate::arch_rs::curr_cpu_num() == BOOT_CPU_ID);

    TIMELINE_THREADING.set(timer_current_mono_ticks().0 as u64);

    dprintf!(SPEW, "top of bootstrap2()\n");

    // TODO(https://fxbug.dev/322207536): Stop resetting start and finish times
    // when unblocking once we solve races higher in the stack.
    dprintf!(
        INFO,
        "Boot option: New thread wakeup accounting {}\n",
        if enable_new_wakeup_accounting() { "enabled" } else { "disabled" }
    );

    // Initialize the rest of the architecture and platform.
    // SAFETY: `bootstrap2` runs once on `BOOT_CPU_ID` in sequence after `lk_main`, while
    // `PhysHandoff::as_ptr()` is still valid.
    unsafe {
        lk_primary_cpu_init_level(LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_ARCH - 1);

        dprintf!(SPEW, "initializing arch\n");
        arch_init();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH, LK_INIT_LEVEL_PLATFORM - 1);

        dprintf!(SPEW, "initializing platform\n");
        platform_init();
        crate::dev_init::driver_handoff_late(PhysHandoff::get());
        lk_primary_cpu_init_level(LK_INIT_LEVEL_PLATFORM, LK_INIT_LEVEL_ARCH_LATE - 1);
    }

    // At this point, other cores in the system have been started (though may
    // not yet be online).  Signal that the boot CPU is ready.
    crate::kernel::mp::signal_curr_cpu_ready();

    // Perform per-CPU set up on the boot CPU.
    dprintf!(SPEW, "initializing late arch\n");
    // SAFETY: `bootstrap2` is pinned to `BOOT_CPU_ID` and runs late arch init once on the boot CPU.
    unsafe {
        arch_late_init_percpu();
        lk_primary_cpu_init_level(LK_INIT_LEVEL_ARCH_LATE, LK_INIT_LEVEL_USER - 1);
    }

    // End hand-off before shell initialization, as we want kernel state to be
    // 'finalized' before we run any kernel scripts (e.g., for unit-testing).
    // SAFETY: All hand-off consumers up through `LK_INIT_LEVEL_USER - 1` have completed, and no
    // references into temporary hand-off memory remain live.
    let handoff_end = unsafe { end_handoff() };

    // Give the kernel shell an opportunity to run. If it exits this function, continue booting.
    #[cfg(console_enabled)]
    crate::console::kernel_shell_init();

    dprintf!(SPEW, "starting user space\n");
    userboot_init(handoff_end);

    dprintf!(SPEW, "moving to last init level\n");
    // SAFETY: Runs the final primary-CPU init hooks once after `userboot_init`.
    unsafe {
        lk_primary_cpu_init_level(LK_INIT_LEVEL_USER, LK_INIT_LEVEL_LAST);
    }

    TIMELINE_INIT.set(timer_current_mono_ticks().0 as u64);
    0
}

/// Entry point for secondary CPUs.
///
/// # Safety
///
/// Must be called at most once per start of a secondary CPU, on its initial bootstrap stack with
/// interrupts disabled, after `lk_init_secondary_cpus` has completed on the boot CPU.
// SAFETY: Exported symbol matches `extern "C" void lk_secondary_cpu_entry()` in `<lk/main.h>` and
// has no other definition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_secondary_cpu_entry() {
    let cpu = crate::arch_rs::curr_cpu_num();
    debug_assert!(cpu != 0);

    // SAFETY: `SECONDARY_IDLE_THREAD_COUNT` was initialized on the boot CPU in
    // `lk_init_secondary_cpus` before any secondary CPU was started, and is read-only hereafter.
    let idle_thread_count = unsafe { SECONDARY_IDLE_THREAD_COUNT.get() };
    if cpu > idle_thread_count {
        core::hint::cold_path();
        dprintf!(
            CRITICAL,
            "Invalid secondary cpu num {}, SMP_MAX_CPUS {}, secondary_idle_thread_count {}\n",
            cpu,
            SMP_MAX_CPUS,
            idle_thread_count
        );
        return;
    }

    // SAFETY: Caller guarantees secondary CPU entry on its initial bootstrap thread after
    // `secondary_cpu_init_early` (so `blocking_disallowed()` is true and interrupts are disabled),
    // with `cpu <= idle_thread_count` (verified above, so its idle/power thread was created by
    // `create_idle_thread`) and no locks or `ChainLockTransaction` held.
    unsafe {
        // late CPU initialization for secondary CPUs
        arch_late_init_percpu();

        // secondary cpu initialize from threading level up. 0 to threading was handled in arch
        lk_init_level(LkInitFlags::SecondaryCpus, LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_LAST);

        lockup_percpu_init();
        crate::thread_sampler::sampler_percpu_init();

        dprintf!(SPEW, "entering scheduler on cpu {}\n", cpu);
        thread::secondary_cpu_entry()
    }
}

/// Initializes idle threads for secondary CPUs.
///
/// # Safety
///
/// Must be called at most once on the boot CPU after `kernel_init` (including
/// `PerCpu::initialize_secondaries_begin`, so `PerCpu::processor_count() > secondary_cpu_count`
/// after clamping to `SMP_MAX_CPUS - 1`) and before any secondary CPUs are started, from a
/// context where blocking is permitted with no active `ChainLockTransaction`.
pub unsafe fn lk_init_secondary_cpus(mut secondary_cpu_count: u32) {
    if secondary_cpu_count as usize >= SMP_MAX_CPUS {
        core::hint::cold_path();
        dprintf!(
            CRITICAL,
            "Invalid secondary_cpu_count {}, SMP_MAX_CPUS {}\n",
            secondary_cpu_count,
            SMP_MAX_CPUS
        );
        secondary_cpu_count = (SMP_MAX_CPUS - 1) as u32;
    }

    let mut count = 0;
    for i in 0..secondary_cpu_count {
        // SAFETY: `i + 1` is in `1..SMP_MAX_CPUS` and `< PerCpu::processor_count()`, and caller
        // guarantees single-call execution on the boot CPU before secondary CPUs start with
        // blocking permitted and no active `ChainLockTransaction`.
        let t = unsafe { thread::create_idle_thread(i + 1) };
        if t.is_none() {
            core::hint::cold_path();
            dprintf!(CRITICAL, "could not allocate idle thread {}\n", i + 1);
            break;
        }
        count += 1;
    }
    // SAFETY: Caller guarantees `lk_init_secondary_cpus` runs on the boot CPU before any secondary
    // CPU starts and reads `SECONDARY_IDLE_THREAD_COUNT`.
    unsafe {
        SECONDARY_IDLE_THREAD_COUNT.set(count);
    }
}
