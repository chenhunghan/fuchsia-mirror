// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The `<platform.h>` entry points for the generic arm64 platform: early init and the persistent
//! RAM carve-up, CPU and system suspend through PSCI, CPU topology and secondary CPU bring-up, the
//! resource ranges, and the panic and halt paths.

use crate::arch_rs::arm64::{
    MPIDR_EL1_RAW, arch_curr_cpu_num, arch_max_num_cpus, arch_mp_cpu_unplug,
    arch_mp_prep_cpu_unplug, arch_mp_send_ipi, arch_register_mpid, arch_set_num_cpus,
    arm64_create_secondary_stack, arm64_free_secondary_stack, arm64_mmu_early_init,
    arm64_secondary_start_paddr, clean_dcache_range, cpu_num_to_mpidr, wfi,
};
use crate::arch_rs::{disable_ints, ints_disabled};
use crate::arm_generic_timer::{platform_resume_timer_curr_cpu, platform_suspend_timer_curr_cpu};
use crate::backtrace::Backtrace;
#[cfg(console_enabled)]
use crate::console::panic_shell_start;
use crate::control_region::ControlRegion;
use crate::crashlog::MIN_CRASHLOG_SIZE;
use crate::jtrace::{
    IS_PERSISTENT as JTRACE_IS_PERSISTENT,
    TARGET_PERSISTENT_BUFFER_SIZE as JTRACE_TARGET_PERSISTENT_BUFFER_SIZE, TraceBufferType,
    jtrace_dump, jtrace_set_location,
};
use crate::kernel::cpu_distance_map::CpuDistanceMap;
use crate::kernel::mp::{MpIpi, MpIpiTarget, SMP_MAX_CPUS, get_online_mask};
use crate::kernel::relaxed_atomic::RelaxedAtomicBool;
use crate::kernel::thread::{self, ThreadPtr};
use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t, cpu_num_t, cpu_num_to_mask};
use crate::lockup_detector::{lockup_percpu_init, lockup_percpu_shutdown};
use crate::object::ResourceDispatcher;
use crate::pdev_interrupt::{
    interrupt_get_base_vector, interrupt_get_max_vector, resume_interrupts_curr_cpu,
    suspend_interrupts_curr_cpu,
};
use crate::pdev_power::{
    PowerCpuState, PowerRebootFlags, power_cpu_off, power_cpu_on, power_get_cpu_state,
    power_reboot, power_shutdown, power_system_resume,
};
use crate::persistent_debuglog_rs::{
    PERSISTENT_RAM_ALLOCATION_GRANULARITY, TARGET_PERSISTENT_DEBUGLOG_SIZE,
};
use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::debug::platform_serial_drain_tx;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use crate::platform_rs::power::{PlatformHaltAction, ZirconCrashReason};
use crate::platform_rs::timer::{
    DurationMono, current_boot_time, current_mono_time, timer_current_boot_ticks,
};
use crate::psci::{
    CpuPoweredDown, PsciCpuSuspendMaxScope, PsciSuspendMode, psci_cpu_suspend,
    psci_get_suspend_mode, psci_is_cpu_suspend_supported, psci_is_set_suspend_mode_supported,
    psci_is_system_suspend_supported, psci_might_powerdown, psci_select_cpu_suspend_power_state,
    psci_set_suspend_mode, psci_system_suspend,
};
use crate::vm::pmm;
use crate::{debuglog_rs as debuglog, persistent_debuglog_rs as persistent_debuglog, topology};
use boot_options::BootOptions;
use core::ffi::c_void;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{dprintf, ltracef};
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
use zx_status::Status;
use zx_types::{
    ARM_SMC_SERVICE_CALL_NUM_MAX, ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SMC,
    ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_COUNT,
};

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    /// The start of the kernel load image, from kernel.ld. Only its address is used.
    static __executable_start: [u8; 0];
    /// The end of the kernel load image, from kernel.ld. Only its address is used.
    static _end: [u8; 0];

    // TODO(https://fxbug.dev/543468018): Expose these from the `handoff` crate, whose header
    // declares them.
    fn cpp_phys_handoff_cpu_topology(count: &mut usize) -> *const zbi::TopologyNode;
    fn cpp_phys_handoff_memory(count: &mut usize) -> *const memalloc::Range;
    fn cpp_phys_handoff_nvram(size: &mut usize) -> *mut u8;
}

/// The console is compiled out, so the panic shell is a no-op like the
/// `<lib/console.h>` stub.
#[cfg(not(console_enabled))]
fn panic_shell_start() {}

static PANIC_STARTED: AtomicBool = AtomicBool::new(false);
static HALTED: AtomicBool = AtomicBool::new(false);

/// Whether this platform implementation supports CPU suspend. Written on the boot CPU before the
/// secondary CPUs start, and never afterwards.
static CPU_SUSPEND_SUPPORTED: RelaxedAtomicBool = RelaxedAtomicBool::new(false);

/// Whether this platform implementation supports system suspend. Written on the boot CPU before
/// the secondary CPUs start, and never afterwards.
static SYSTEM_SUSPEND_SUPPORTED: RelaxedAtomicBool = RelaxedAtomicBool::new(false);

/// Used for `platform_suspend_cpu` coordination.
static CONTROL_REGION: LazyInit<ControlRegion, NoCheck> = LazyInit::uninit();

/// The platform's crashlog implementation, backed by the persistent RAM range
/// that `allocate_persistent_ram` carves out for it.
static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();

/// Whether EFI services are expected on this platform.
pub fn platform_is_efi_expected() -> bool {
    false
}

/// Returns [`CONTROL_REGION`].
///
/// # Safety
///
/// [`platform_early_init`] must have run.
unsafe fn control_region() -> &'static ControlRegion {
    // SAFETY: `platform_early_init` initializes `CONTROL_REGION`, and the caller guarantees that it
    // has run.
    unsafe { CONTROL_REGION.get_unchecked() }
}

fn halt_other_cpus() {
    if !HALTED.swap(true, Ordering::SeqCst) {
        // stop the other cpus
        kprint!("stopping other cpus\n");
        arch_mp_send_ipi(MpIpiTarget::AllButLocal, 0, MpIpi::Halt);

        // spin for a while
        // TODO: find a better way to spin at this low level
        for _ in 0..100_000_000 {
            libarch::r#yield();
        }
    }
}

/// Difference on SMT systems is that the AFF0 (cpu_id) level is implicit and not stored in the
/// info.
fn to_smt_mpid(info: &zbi::TopologyArm64Info, cpu_id: u8) -> u64 {
    (u64::from(info.cluster_3_id) << 32)
        | (u64::from(info.cluster_2_id) << 16)
        | (u64::from(info.cluster_1_id) << 8)
        | u64::from(cpu_id)
}

fn to_mpid(info: &zbi::TopologyArm64Info) -> u64 {
    (u64::from(info.cluster_3_id) << 32)
        | (u64::from(info.cluster_2_id) << 16)
        | (u64::from(info.cluster_1_id) << 8)
        | u64::from(info.cpu_id)
}

/// The arm64 information of the processor `node`.
///
/// Panics, as `topology_cpu_init` does, if `node` is not an arm64 processor.
fn arm64_info(node: &topology::Node) -> &zbi::TopologyArm64Info {
    match node.entity() {
        zbi::TopologyEntity::Processor(zbi::TopologyProcessor {
            architecture_info: zbi::TopologyArchitectureInfo::Arm64(info),
            ..
        }) => info,
        _ => panic!("Invalid processor node."),
    }
}

// TODO(https://fxbug.dev/42180675): Refactor platform_panic_start.
/// Informs the system that a panic message is about to be printed and that `platform_halt` will
/// be called shortly. Stops the other CPUs first if `halt_others` is set.
pub fn platform_panic_start(halt_others: bool) {
    disable_ints();
    debuglog::dlog_panic_start();

    if halt_others {
        halt_other_cpus();
    }

    if !PANIC_STARTED.swap(true, Ordering::SeqCst) {
        debuglog::dlog_bluescreen_init();
        // Attempt to dump the current debug trace buffer, if we have one.
        jtrace_dump(TraceBufferType::Current);
    }
}

/// Stops the calling CPU.
///
/// # Safety
///
/// [`platform_early_init`] must have run.
pub unsafe fn platform_halt_cpu() -> ! {
    // TODO(https://fxbug.dev/450973098): Considering modeling offline CPUs as already in the
    // control region.
    // SAFETY: The caller guarantees that `platform_early_init` has run.
    unsafe { control_region() }.unregister();

    let result = power_cpu_off();
    // should have never returned
    panic!("power_cpu_off returned {}", Status::result_into_raw(result) as u32);
}

/// Returns true iff this platform supports [`platform_suspend_cpu`].
pub fn platform_supports_suspend_cpu() -> bool {
    CPU_SUSPEND_SUPPORTED.load()
}

/// Returns true iff this platform supports [`platform_suspend_system`].
pub fn platform_supports_suspend_system() -> bool {
    SYSTEM_SUSPEND_SUPPORTED.load()
}

// TODO(https://fxbug.dev/417558115): Expand to include a deadline parameter
// that's used to wake the CPU based on the boot time clock.
/// Suspends the calling CPU. If `allow_domain` is set, the power domain encapsulating the calling
/// CPU may also be suspended and powered down.
///
/// # Safety
///
/// Interrupts and preemption must be disabled, the caller must be pinned to the calling CPU, the
/// calling thread must be a kernel-only thread, and some interrupt that will wake the CPU and
/// resume execution must be set up, as `platform_suspend_cpu` in `<platform.h>` requires.
pub unsafe fn platform_suspend_cpu(allow_domain: bool) -> Result<(), Status> {
    ltracef!(
        "platform_suspend_cpu cpu-{} current_boot_time={} allow_domain={}\n",
        arch_curr_cpu_num(),
        current_boot_time().0,
        u32::from(allow_domain)
    );

    // TODO(https://fxbug.dev/458140022): Once we have figured out the cause of "negative time" we
    // can remove this assert.
    let boot_ticks_before = timer_current_boot_ticks().0;
    debug_assert!(boot_ticks_before >= 0, "boot_ticks_before={}", boot_ticks_before);

    debug_assert!(!thread::preempt_is_enabled());
    debug_assert!(ints_disabled());
    // Make sure this thread is a kernel-only thread.
    // Otherwise, we might need to save and restore some vector/floating-point
    // state if we are going to power down.
    debug_assert!({
        // SAFETY: Threading is up whenever a CPU suspends.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` is the running thread, which stays valid while it runs this.
        unsafe { current.user_thread_raw() }.is_null()
    });

    if !CPU_SUSPEND_SUPPORTED.load() {
        return Err(Status::NOT_SUPPORTED);
    }

    // SAFETY: Only `platform_init` sets `CPU_SUSPEND_SUPPORTED`, checked above, and its contract
    // requires `platform_early_init` to have run.
    let control_region = unsafe { control_region() };

    let cpu_might_power_down = psci_might_powerdown();

    // Power-domain idle states use OS-initiated coordination (`CONTROL_REGION` only passes
    // `last_to_enter = true` on the last CPU), so only request them when OSI mode was enabled; see
    // `platform_init`.
    let max_scope = if psci_get_suspend_mode() == PsciSuspendMode::OsInitiated && allow_domain {
        PsciCpuSuspendMaxScope::CpuAndMore
    } else {
        PsciCpuSuspendMaxScope::CpuOnly
    };

    if cpu_might_power_down {
        // SAFETY: The caller has interrupts and preemption disabled (preventing migration), and
        // `lockup_init` ran on the boot CPU before `platform_init` set `CPU_SUSPEND_SUPPORTED`.
        unsafe { lockup_percpu_shutdown() };
        let _ = platform_suspend_timer_curr_cpu();
        // SAFETY: CPU suspend is enabled only by `platform_init`, which runs after
        // `driver_handoff_post_vm`, where the interrupt controller driver registers its ops.
        let _ = unsafe { suspend_interrupts_curr_cpu() };
    }

    let result;

    {
        let last_to_enter = !control_region.try_enter();
        if last_to_enter {
            control_region.close_enter();
        }

        let power_state = psci_select_cpu_suspend_power_state(max_scope, last_to_enter);

        ltracef!(
            "platform_suspend_cpu for cpu-{}, current_boot_time={}, suspending...\n",
            arch_curr_cpu_num(),
            current_boot_time().0
        );
        // The following call may not return for an arbitrartily long time.
        // SAFETY: The caller meets the interrupt, preemption, pinning and kernel-only thread
        // requirements, which are `psci_cpu_suspend`'s too.
        result = unsafe { psci_cpu_suspend(power_state) };
        ltracef!(
            "psci_cpu_suspend for cpu-{}, status {}\n",
            arch_curr_cpu_num(),
            Status::result_into_raw(result.map(|_| ()))
        );

        debug_assert!(ints_disabled());

        let first_to_leave = !control_region.try_leave();
        if first_to_leave {
            // TODO(https://fxbug.dev/458140022): Once we have figured out the cause of "negative
            // time" we can remove this assert.
            let boot_ticks_after = timer_current_boot_ticks().0;
            debug_assert!(boot_ticks_after >= 0, "boot_ticks_after={}", boot_ticks_after);
            debug_assert!(
                boot_ticks_before <= boot_ticks_after,
                "boot_ticks_before={} boot_ticks_after={}",
                boot_ticks_before,
                boot_ticks_after
            );

            control_region.open_leave();
        }
    }

    if cpu_might_power_down {
        // SAFETY: As for `suspend_interrupts_curr_cpu` above.
        let status = unsafe { resume_interrupts_curr_cpu() };
        debug_assert!(
            status.is_ok(),
            "resume_interrupts_curr_cpu: {}",
            Status::result_into_raw(status)
        );
        let status = platform_resume_timer_curr_cpu();
        debug_assert!(
            status.is_ok(),
            "platform_resume_timer_curr_cpu: {}",
            Status::result_into_raw(status)
        );
        // SAFETY: The caller is pinned to this CPU, which runs threads, so its `percpu` is
        // installed. `lockup_init` ran on the boot CPU before `platform_init` set
        // `CPU_SUSPEND_SUPPORTED`, which is checked above.
        unsafe { lockup_percpu_init() };
    } else {
        // If the requested power state isn't a "power down" power state, then make
        // sure we did not in fact power down.
        debug_assert!(result.is_err() || result == Ok(CpuPoweredDown::No));
    }

    ltracef!(
        "platform_suspend_cpu for cpu-{} current_boot_time={}, done\n",
        arch_curr_cpu_num(),
        current_boot_time().0
    );

    result.map(|_| ())
}

/// Suspends the entire system from the boot CPU after all secondary CPUs have been powered off.
///
/// # Safety
///
/// Interrupts and preemption must be disabled, the caller must be running on the boot CPU, the
/// calling thread must be a kernel-only thread, and some interrupt that will wake the boot CPU and
/// resume execution must be set up, as `platform_suspend_system` in `<platform.h>` requires.
pub unsafe fn platform_suspend_system() -> Result<(), Status> {
    ltracef!(
        "platform_suspend_system cpu-{} current_boot_time={}\n",
        arch_curr_cpu_num(),
        current_boot_time().0
    );

    debug_assert!(!thread::preempt_is_enabled());
    debug_assert!(ints_disabled());
    debug_assert!(arch_curr_cpu_num() == BOOT_CPU_ID);
    // Make sure this thread is a kernel-only thread.
    // Otherwise, we might need to save and restore some vector/floating-point
    // state if we are going to power down.
    debug_assert!({
        // SAFETY: Threading is up whenever the system suspends.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` is the running thread, which stays valid while it runs this.
        unsafe { current.user_thread_raw() }.is_null()
    });

    if !SYSTEM_SUSPEND_SUPPORTED.load() {
        return Err(Status::NOT_SUPPORTED);
    }

    // A secondary CPU may have been brought online concurrently with the caller's suspend
    // operation, so treat this as an error rather than asserting.
    let online = get_online_mask();
    if online != cpu_num_to_mask(BOOT_CPU_ID) {
        dprintf!(
            INFO,
            "platform_suspend_system: secondary CPUs are online (online mask {:#x})\n",
            online
        );
        return Err(Status::BAD_STATE);
    }

    // It is critical that every other CPU has finished powering off (i.e. completed CPU_OFF) before
    // calling SYSTEM_SUSPEND.  Verify that the PSCI implementation agrees before proceeding.
    for cpu_id in 0..arch_max_num_cpus() {
        if cpu_id == BOOT_CPU_ID {
            continue;
        }
        let state = platform_get_cpu_state(cpu_id);
        if state.is_err() || state != Ok(PowerCpuState::Off) {
            dprintf!(
                INFO,
                "platform_suspend_system: cpu-{} is not off (status {}, state {})\n",
                cpu_id,
                Status::result_into_raw(state.map(|_| ())),
                state.map_or(-1, |state| state as i32)
            );
            return Err(Status::BAD_STATE);
        }
    }

    // SAFETY: The caller is pinned to the boot CPU by having interrupts and preemption disabled,
    // and `lockup_init` ran on the boot CPU before `platform_init` set `SYSTEM_SUSPEND_SUPPORTED`.
    unsafe { lockup_percpu_shutdown() };
    let _ = platform_suspend_timer_curr_cpu();
    // SAFETY: System suspend is enabled only by `platform_init`, which runs after
    // `driver_handoff_post_vm`, where the interrupt controller driver registers its ops.
    let _ = unsafe { suspend_interrupts_curr_cpu() };

    ltracef!(
        "platform_suspend_system for cpu-{}, current_boot_time={}, suspending...\n",
        arch_curr_cpu_num(),
        current_boot_time().0
    );
    // The following call may not return for an arbitrarily long time.
    // SAFETY: The caller meets the interrupt, preemption, boot CPU and kernel-only thread
    // requirements, and the loop above confirmed that every other CPU is off.
    let suspend_status = unsafe { psci_system_suspend() };
    ltracef!(
        "psci_system_suspend for cpu-{}, status {}\n",
        arch_curr_cpu_num(),
        Status::result_into_raw(suspend_status)
    );

    debug_assert!(ints_disabled());

    // Firmware may have altered platform power state (e.g. CPU OPP registers) while the system was
    // suspended. Restore the kernel's intended state before anything else runs on the resumed
    // system. Do this regardless of suspend_status: if the suspend was aborted, re-applying the
    // current state is harmless.
    //
    // TODO(https://fxbug.dev/576640818): Determine whether cluster power-down via
    // platform_suspend_cpu() can also reset this state, in which case the restore must also happen
    // when the first CPU in a domain resumes.
    if let Err(status) = power_system_resume() {
        dprintf!(
            CRITICAL,
            "platform_suspend_system: power_system_resume failed: {}\n",
            status.into_raw()
        );
    }

    // SAFETY: As for `suspend_interrupts_curr_cpu` above.
    let status = unsafe { resume_interrupts_curr_cpu() };
    debug_assert!(
        status.is_ok(),
        "resume_interrupts_curr_cpu: {}",
        Status::result_into_raw(status)
    );
    let status = platform_resume_timer_curr_cpu();
    debug_assert!(
        status.is_ok(),
        "platform_resume_timer_curr_cpu: {}",
        Status::result_into_raw(status)
    );
    // SAFETY: The caller is pinned to the boot CPU by having interrupts and preemption disabled,
    // and the boot CPU's `percpu` is installed. `lockup_init` ran on the boot CPU before
    // `platform_init` set `SYSTEM_SUSPEND_SUPPORTED`, which is checked above.
    unsafe { lockup_percpu_init() };

    ltracef!(
        "platform_suspend_system for cpu-{} current_boot_time={}, done\n",
        arch_curr_cpu_num(),
        current_boot_time().0
    );

    suspend_status
}

/// Start the given cpu, whose MPIDR is `mpid`, in a way the platform finds appropriate. `context`
/// contains an opaque value passed through to the new cpu for the bootstrap code that the cpu
/// starts on. Most likely the context is a pointer to a data structure or the stack for the cpu to
/// set up.
///
/// # Safety
///
/// [`platform_early_init`] must have run, `mpid` must be the MPIDR of `cpu_id`, `context` must be a
/// stack that `arm64_create_secondary_stack` created for `cpu_id`, and CPU `cpu_id` must not be
/// running.
pub unsafe fn platform_start_cpu(cpu_id: cpu_num_t, mpid: u64, context: u64) -> Result<(), Status> {
    // TODO(https://fxbug.dev/450973098): Considering modeling offline CPUs as already in the
    // control region.
    // SAFETY: The caller guarantees that `platform_early_init` has run.
    unsafe { control_region() }.register();

    let kernel_secondary_entry_paddr = arm64_secondary_start_paddr();
    let ret = power_cpu_on(mpid, u64::from(kernel_secondary_entry_paddr), context);
    dprintf!(
        INFO,
        "Trying to start cpu {}, mpid {:#x} returned: {}\n",
        cpu_id,
        mpid,
        Status::result_into_raw(ret)
    );
    if ret.is_err() {
        return Err(Status::INTERNAL);
    }
    Ok(())
}

/// Get the state of a CPU.
pub fn platform_get_cpu_state(cpu_id: cpu_num_t) -> Result<PowerCpuState, Status> {
    debug_assert!((cpu_id as usize) < SMP_MAX_CPUS);
    power_get_cpu_state(cpu_num_to_mpidr(cpu_id))
}

/// Registers every processor in the system topology and starts the secondary ones.
///
/// # Safety
///
/// Must be called once, on the boot CPU from a thread whose `Thread` is initialized, after
/// [`platform_early_init`] and `init_topology` have run and before any secondary CPU has started.
unsafe fn topology_cpu_init() {
    // We need booted secondary CPUs - *before* they enable their caches - to
    // have a view of the relevant memory that's coherent with the boot CPU. It
    // should suffice to ensure that (1) the code the secondary CPUs would touch
    // before enabling data caches and (2) the variables it loads are cleaned to
    // the point of coherency. While we could be surgical about that, it suffices
    // to simply clean the whole kernel load image, which surely includes (1) and
    // (2).
    let start = (&raw const __executable_start).addr();
    let len = (&raw const _end).addr() - start;
    // SAFETY: `[__executable_start, _end)` is the kernel load image, which stays mapped.
    unsafe { clean_dcache_range(start, len) };

    let mut registered_cpus: cpu_mask_t = 0;
    for node in topology::get_system_topology().processors() {
        let zbi::TopologyEntity::Processor(processor) = node.entity() else {
            panic!("Invalid processor node.");
        };
        let zbi::TopologyArchitectureInfo::Arm64(info) = &processor.architecture_info else {
            panic!("Invalid processor node.");
        };

        for i in 0..processor.logical_id_count {
            let mpid =
                if processor.logical_id_count > 1 { to_smt_mpid(info, i) } else { to_mpid(info) };
            let logical_id = cpu_num_t::from(processor.logical_ids[usize::from(i)]);
            // The topology comes from the ZBI, and the safety of everything below rests on each
            // id being in range and appearing once.
            assert!((logical_id as usize) < SMP_MAX_CPUS, "logical id {} too large", logical_id);
            let logical_id_mask = cpu_num_to_mask(logical_id);
            assert!(registered_cpus & logical_id_mask == 0, "logical id {} repeated", logical_id);
            registered_cpus |= logical_id_mask;

            // SAFETY: The asserts above put `logical_id` in range and make this its first
            // appearance. The caller runs this on the boot CPU before any secondary CPU has
            // started, and a secondary CPU starts only below, so CPU `logical_id` is either the
            // calling CPU or has not started yet.
            unsafe { arch_register_mpid(logical_id, mpid) };

            // Skip processor 0, we are only starting secondary processors.
            if logical_id == 0 {
                continue;
            }

            // SAFETY: The asserts and the check above put `logical_id` in `1..SMP_MAX_CPUS` and
            // make this its first appearance, so it has not started and has no stack yet. Only
            // this CPU creates stacks here, and the caller's `Thread` is initialized.
            let stack = unsafe { arm64_create_secondary_stack(logical_id) }
                .expect("arm64_create_secondary_stack failed");

            // start the cpu
            // SAFETY: The caller guarantees that `platform_early_init` has run. `mpid` is the MPIDR
            // registered for `logical_id` above, and `stack` is the stack
            // `arm64_create_secondary_stack` just created for it. It has not started: the asserts
            // above make this its first appearance.
            let status = unsafe { platform_start_cpu(logical_id, mpid, stack as u64) };

            if status.is_err() {
                // TODO(maniscalco): Is continuing really the right thing to do here?

                // start failed, free the stack
                // SAFETY: `logical_id` is in range and appears only once, by the asserts above,
                // and the CPU did not start, so nothing runs on the stack being freed.
                let status = unsafe { arm64_free_secondary_stack(logical_id) };
                debug_assert!(status.is_ok());
                continue;
            }
        }
    }
}

/// The topology of just the boot processor, used when the handed-off topology is unusable.
static FALLBACK_TOPOLOGY: zbi::TopologyNode = zbi::TopologyNode {
    entity: zbi::TopologyEntity::Processor(zbi::TopologyProcessor {
        architecture_info: zbi::TopologyArchitectureInfo::Arm64(zbi::TopologyArm64Info {
            cluster_1_id: 0,
            cluster_2_id: 0,
            cluster_3_id: 0,
            cpu_id: 0,
            gic_id: 0,
        }),
        flags: zbi::TopologyProcessorFlags::empty(),
        logical_ids: [0, 0, 0, 0],
        logical_id_count: 1,
    }),
    parent_index: zbi::TOPOLOGY_NO_PARENT,
};

fn init_topology() {
    let mut handoff_count = 0usize;
    // SAFETY: `handoff_count` is a live local that the callee fills in.
    let handoff = unsafe { cpp_phys_handoff_cpu_topology(&mut handoff_count) };

    // SAFETY: `handoff` and `handoff_count` describe the topology nodes physboot handed off, which
    // stay valid until the handoff ends, after this hook. The callee validates their contents.
    let mut result =
        unsafe { topology::Graph::initialize_system_topology_from_raw(handoff, handoff_count) };
    if let Err(status) = result {
        kprint!("Failed to initialize system topology! error: {}\n", status.into_raw());

        // Try to fallback to a topology of just this processor.
        result =
            topology::Graph::initialize_system_topology(core::slice::from_ref(&FALLBACK_TOPOLOGY));
        assert!(result.is_ok());
    }

    // SAFETY: This hook runs on the boot CPU before any secondary CPU starts.
    unsafe { arch_set_num_cpus(topology::get_system_topology().processor_count() as u32) };

    if debug::dprintf::dprintf_enabled(debug::dprintf::INFO) {
        for node in topology::get_system_topology().processors() {
            let info = arm64_info(node);
            dprintf!(
                INFO,
                "System topology: CPU {}:{}:{}:{}\n",
                info.cluster_3_id,
                info.cluster_2_id,
                info.cluster_1_id,
                info.cpu_id
            );
        }
    }
}

init::lk_init_hook!(init_topology, |_| init_topology(), init::LK_INIT_LEVEL_VM);

/// Divides `range` up between the crashlog, persistent debug logging and persistent debug tracing.
///
/// # Safety
///
/// Must be called at most once, during single-CPU early platform initialization before
/// `LK_INIT_LEVEL_HEAP`, and before anything reads `MAPPED_CRASHLOG` or calls
/// `jtrace_dump(TraceBufferType::Recovered)`.
unsafe fn allocate_persistent_ram(range: &'static mut [u8]) {
    // Figure out how to divide up our persistent RAM.  Right now there are
    // three potential users:
    //
    // 1) The crashlog.
    // 2) Persistent debug logging.
    // 3) Persistent debug tracing.
    //
    // Persistent debug logging and tracing have target amounts of RAM they would
    // _like_ to have, and crash-logging has a minimum amount it is guaranteed to
    // get.  Additionally, all allocated are made in a chunks of the minimum
    // persistent RAM allocation granularity.
    //
    // Make sure that the crashlog gets as much of its minimum allocation as is
    // possible.  Then attempt to satisfy the target for persistent debug logging,
    // followed by persistent debug tracing.  Finally, give anything leftovers to
    // the crashlog.
    let (crashlog_size, pdlog_size, jtrace_size) = {
        // start by figuring out how many chunks of RAM we have available to
        // us total.
        let mut persistent_chunks_available = range.len() / PERSISTENT_RAM_ALLOCATION_GRANULARITY;

        // If we have not already configured a non-trivial crashlog implementation
        // for the platform, make sure that crashlog gets its minimum allocation, or
        // all of the RAM if it cannot meet even its minimum allocation.
        let mut crashlog_chunks = if !PlatformCrashlog::has_non_trivial_impl() {
            core::cmp::min(
                persistent_chunks_available,
                MIN_CRASHLOG_SIZE / PERSISTENT_RAM_ALLOCATION_GRANULARITY,
            )
        } else {
            0
        };
        persistent_chunks_available -= crashlog_chunks;

        // Next in line is persistent debug logging. When set, the boot option
        // overrides the compile-time target size. Cap the override at half of the
        // remaining space so an oversized command-line value cannot starve
        // persistent tracing and the crashlog.
        let pdlog_target_override = BootOptions::get().persistent_dlog_target_size as usize;
        let pdlog_target_chunks = if pdlog_target_override != 0 {
            core::cmp::min(
                persistent_chunks_available / 2,
                pdlog_target_override / PERSISTENT_RAM_ALLOCATION_GRANULARITY,
            )
        } else {
            TARGET_PERSISTENT_DEBUGLOG_SIZE / PERSISTENT_RAM_ALLOCATION_GRANULARITY
        };
        let pdlog_chunks = core::cmp::min(persistent_chunks_available, pdlog_target_chunks);
        persistent_chunks_available -= pdlog_chunks;

        // Next up is persistent debug tracing.
        let jtrace_chunks = core::cmp::min(
            persistent_chunks_available,
            JTRACE_TARGET_PERSISTENT_BUFFER_SIZE / PERSISTENT_RAM_ALLOCATION_GRANULARITY,
        );
        persistent_chunks_available -= jtrace_chunks;

        // Finally, anything left over can go to the crashlog.
        crashlog_chunks += persistent_chunks_available;

        (
            crashlog_chunks * PERSISTENT_RAM_ALLOCATION_GRANULARITY,
            pdlog_chunks * PERSISTENT_RAM_ALLOCATION_GRANULARITY,
            jtrace_chunks * PERSISTENT_RAM_ALLOCATION_GRANULARITY,
        )
    };

    // Configure up the crashlog RAM
    let (crashlog, leftover) = range.split_at_mut(crashlog_size);
    if crashlog_size > 0 {
        dprintf!(INFO, "Crashlog configured with {} bytes\n", crashlog_size);
        // SAFETY: The caller runs this at most once, during single-CPU early platform
        // initialization, before anything reads `MAPPED_CRASHLOG`.
        let _ =
            unsafe { Pin::static_ref(&MAPPED_CRASHLOG).init_pin(MappedCrashlog::new(crashlog)) };
        // SAFETY: `MAPPED_CRASHLOG` was initialized just above.
        let _ = PlatformCrashlog::bind(unsafe { MAPPED_CRASHLOG.get_unchecked() });
    }

    // Configure the persistent debuglog RAM (if we have any)
    let (pdlog, leftover) = leftover.split_at_mut(pdlog_size);
    if pdlog_size > 0 {
        dprintf!(
            INFO,
            "Persistent debug logging enabled and configured with {} bytes\n",
            pdlog_size
        );
        // SAFETY: The caller runs this at most once, during single-CPU early platform
        // initialization before `LK_INIT_LEVEL_HEAP`.
        unsafe { persistent_debuglog::persistent_dlog_set_location(pdlog) };
    }

    // Do _not_ attempt to set the location of the debug trace buffer if this is
    // not a persistent debug trace buffer.  The location of a non-persistent
    // trace buffer would have been already set during (very) early init.
    if JTRACE_IS_PERSISTENT {
        let (jtrace, _) = leftover.split_at_mut(jtrace_size);
        let jtrace_len = jtrace.len();
        // SAFETY: The caller runs this at most once, during single-CPU early platform
        // initialization, so nothing calls `jtrace_set_location` or
        // `jtrace_dump(TraceBufferType::Recovered)` concurrently. `jtrace` is part of the
        // persistent RAM range, which stays mapped for the life of the kernel, and nothing here
        // touches it again.
        unsafe { jtrace_set_location(jtrace.as_mut_ptr().cast::<c_void>(), jtrace_len) };
    }
}

/// Super early platform initialization, before almost everything.
///
/// # Safety
///
/// Must be called once, on the boot CPU during single-threaded early boot, after
/// `arm64_feature_init` and before any CPU reaches the start, halt or suspend paths in this file.
pub unsafe fn platform_early_init() {
    // SAFETY: The caller runs this once, on the boot CPU, before anything uses `CONTROL_REGION`.
    let Ok(()) = unsafe { Pin::static_ref(&CONTROL_REGION).init_pin(ControlRegion::init()) };

    let mut nvram_size = 0usize;
    // SAFETY: `nvram_size` is a live local that the callee fills in.
    let nvram = unsafe { cpp_phys_handoff_nvram(&mut nvram_size) };
    if nvram_size != 0 {
        // SAFETY: physboot hands off `nvram_size` bytes of persistent RAM at `nvram`, mapped for
        // the life of the kernel. The caller runs this once, and this is the only place that takes
        // a reference to them.
        let range = unsafe { zr::slice_from_raw_parts_mut(nvram, nvram_size) };
        // SAFETY: The caller runs this once, during single-threaded early boot.
        unsafe { allocate_persistent_ram(range) };
    }

    // Initialize the PmmChecker now that the cmdline has been parsed.
    // SAFETY: The caller runs this during single-threaded early boot.
    unsafe { pmm::pmm_checker_init_from_cmdline() };

    let mut range_count = 0usize;
    // SAFETY: `range_count` is a live local that the callee fills in.
    let ranges = unsafe { cpp_phys_handoff_memory(&mut range_count) };
    // SAFETY: `ranges` and `range_count` describe the memory map physboot handed off, which stays
    // valid until the handoff ends, after this call.
    let ranges = unsafe { zr::slice_from_raw_parts(ranges, range_count) };
    // SAFETY: The caller runs this once, during single-threaded early boot, so this is the only
    // call to `pmm::init`, and `ranges` is the system's physical memory map.
    assert!(unsafe { pmm::init(ranges) }.is_ok());

    // give the mmu code a chance to do some bookkeeping
    // SAFETY: The caller runs this once, on the boot CPU during single-threaded early boot, after
    // `arm64_feature_init` and before any secondary CPU starts.
    unsafe { arm64_mmu_early_init() };
}

/// Performs any set up required before virtual memory is enabled, or the heap is set up.
pub fn platform_prevm_init() {}

/// Later init, after the kernel has come up.
///
/// # Safety
///
/// Must be called once, on the boot CPU from a thread whose `Thread` is initialized (`bootstrap2`
/// in `top/main.rs`), after [`platform_early_init`] and `init_topology` have run and before any
/// secondary CPU has started.
pub unsafe fn platform_init() {
    if psci_is_cpu_suspend_supported() {
        // If this PSCI implementation supports OSI mode, use it.
        if psci_is_set_suspend_mode_supported() {
            // Secondary CPUs used by the bootloader may still be finishing CPU_OFF in EL3 when the
            // boot CPU reaches platform_init, causing PSCI_SET_SUSPEND_MODE to return PSCI_DENIED
            // (`Status::ACCESS_DENIED`). Retry briefly before falling back to platform-coordinated
            // mode.
            const SET_SUSPEND_MODE_RETRY_TIMEOUT: DurationMono = DurationMono::from_millis(100);
            const SET_SUSPEND_MODE_RETRY_INTERVAL: DurationMono = DurationMono::from_micros(200);
            let start = current_mono_time();
            let deadline = start + SET_SUSPEND_MODE_RETRY_TIMEOUT;
            let mut status = psci_set_suspend_mode(PsciSuspendMode::OsInitiated);
            let mut now = current_mono_time();
            while status == Err(Status::ACCESS_DENIED) && now < deadline {
                let _ = thread::sleep((now + SET_SUSPEND_MODE_RETRY_INTERVAL).min(deadline));
                status = psci_set_suspend_mode(PsciSuspendMode::OsInitiated);
                now = current_mono_time();
            }
            if status.is_ok() {
                dprintf!(
                    INFO,
                    "PSCI: using OS initiated suspend mode (after {} us)\n",
                    (now - start).into_micros()
                );
            } else if status == Err(Status::NOT_SUPPORTED) {
                dprintf!(INFO, "PSCI: OS initiated suspend mode not supported\n");
            } else if status == Err(Status::ACCESS_DENIED) {
                // Leave PSCI in platform-coordinated mode so `platform_suspend_cpu` clamps requests
                // to CPU-level idle states, since firmware in platform-coordinated mode may reject
                // OS-initiated power-domain state IDs. Log unconditionally so any power regression
                // can be traced back here.
                kprint!(
                    "PSCI: OS initiated suspend mode denied after {} us, using platform \
                     coordinated mode (power domain idle states unavailable)\n",
                    (now - start).into_micros()
                );
            } else {
                panic!(
                    "psci_set_suspend_mode failed with unexpected value {}",
                    Status::result_into_raw(status)
                );
            }
        }

        CPU_SUSPEND_SUPPORTED.store(true);

        // Normally, `power_cpu_on` is responsible for registering CPUs with the `ControlRegion`.
        // Register this one manually since it's the boot CPU and was not started via
        // `power_cpu_on`.
        debug_assert!(arch_curr_cpu_num() == BOOT_CPU_ID);
        // SAFETY: The caller runs this after `platform_early_init`.
        unsafe { control_region() }.register();
    }
    let cpu_suspend_supported = CPU_SUSPEND_SUPPORTED.load();
    dprintf!(
        INFO,
        "platform_suspend_cpu support {}\n",
        if cpu_suspend_supported { "enabled" } else { "disabled" }
    );

    // Like platform_suspend_cpu, platform_suspend_system relies on the interrupt controller being
    // able to suspend and resume the current CPU's interrupt state (see
    // suspend_interrupts_curr_cpu), which is not supported on all platforms (e.g. GICv2).
    // Platforms which enable CPU suspend are assumed to support it, so only enable system suspend
    // when CPU suspend is also enabled.
    let system_suspend_supported = cpu_suspend_supported && psci_is_system_suspend_supported();
    SYSTEM_SUSPEND_SUPPORTED.store(system_suspend_supported);
    dprintf!(
        INFO,
        "platform_suspend_system support {}\n",
        if system_suspend_supported { "enabled" } else { "disabled" }
    );

    // SAFETY: The caller runs this once, on the boot CPU from a thread whose `Thread` is
    // initialized, after `platform_early_init` and `init_topology` have run and before any
    // secondary CPU has started.
    unsafe { topology_cpu_init() };
}

/// Prepares for CPU unplug. The platform may want to shift around external interrupts at this
/// time.
#[unsafe(no_mangle)]
pub extern "C" fn platform_mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    arch_mp_prep_cpu_unplug(cpu_id)
}

/// Shuts down the specified CPU. Called after it is no longer being scheduled on.
#[unsafe(no_mangle)]
pub extern "C" fn platform_mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    arch_mp_cpu_unplug(cpu_id)
}

/// The platform specific actions to be taken in a halt situation: reboots or shuts down as
/// `suggested_action` asks, drops into the panic shell after a panic unless it reboots, and
/// otherwise spins forever.
pub fn platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    if suggested_action == PlatformHaltAction::Reboot {
        platform_serial_drain_tx();
        power_reboot(if reason == ZirconCrashReason::NoCrash {
            PowerRebootFlags::Normal
        } else {
            PowerRebootFlags::Panic
        });
        kprint!("reboot failed\n");
    } else if suggested_action == PlatformHaltAction::RebootBootloader {
        platform_serial_drain_tx();
        power_reboot(PowerRebootFlags::Bootloader);
        kprint!("reboot-bootloader failed\n");
    } else if suggested_action == PlatformHaltAction::RebootRecovery {
        platform_serial_drain_tx();
        power_reboot(PowerRebootFlags::Recovery);
        kprint!("reboot-recovery failed\n");
    } else if suggested_action == PlatformHaltAction::Shutdown {
        platform_serial_drain_tx();
        power_shutdown();
    }

    if reason == ZirconCrashReason::Panic {
        let mut bt = Backtrace::new();
        thread::current_get_backtrace(&mut bt);
        bt.print();
        if !halt_on_panic {
            platform_serial_drain_tx();
            power_reboot(PowerRebootFlags::Panic);
            kprint!("reboot failed\n");
        }
        dprintf!(ALWAYS, "CRASH: starting debug shell... (reason = {})\n", reason as u32);
        disable_ints();
        panic_shell_start();
    }

    dprintf!(ALWAYS, "HALT: spinning forever... (reason = {})\n", reason as u32);

    // catch all fallthrough cases
    disable_ints();

    loop {
        wfi();
    }
}

/// Initialize Resource system after the heap is initialized.
fn arm_resource_dispatcher_init_hook(_level: init::LkInitLevel) {
    // 64 bit address space for MMIO on ARM64
    if let Err(status) = ResourceDispatcher::initialize_allocator(ZX_RSRC_KIND_MMIO, 0, usize::MAX)
    {
        kprint!("Resources: Failed to initialize MMIO allocator: {}\n", status.into_raw());
    }
    // Set up IRQs based on values from the GIC
    let (base_vector, max_vector) = (*interrupt_get_base_vector(), *interrupt_get_max_vector());
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_IRQ,
        u64::from(base_vector),
        max_vector as usize,
    ) {
        kprint!("Resources: Failed to initialize IRQ allocator: {}\n", status.into_raw());
    }
    // Set up SMC valid service call range
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SMC,
        0,
        (ARM_SMC_SERVICE_CALL_NUM_MAX as usize) + 1,
    ) {
        kprint!("Resources: Failed to initialize SMC allocator: {}\n", status.into_raw());
    }
    // Set up range of valid system resources.
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SYSTEM,
        0,
        ZX_RSRC_SYSTEM_COUNT as usize,
    ) {
        kprint!("Resources: Failed to initialize system allocator: {}\n", status.into_raw());
    }
}

init::lk_init_hook!(arm_resource_init, arm_resource_dispatcher_init_hook, init::LK_INIT_LEVEL_INTC);

/// Sets up the CPU distance map from the system topology.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after `init_topology` has run, before any secondary CPU
/// has started and before anything else uses the CPU distance map.
// The `1 *` terms in the distance callback keep each depth's weight visible.
#[allow(clippy::identity_op)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn topology_init() {
    // Check MPIDR_EL1.MT to determine how to interpret AFF0 (i.e. cpu_id). For
    // now, assume that MT is set consistently across all PEs in the system. When
    // MT is set, use the next affinity level for the first cache depth element.
    // This approach should be adjusted if we find examples of systems that do not
    // set MT uniformly, and may require delaying cache-aware load balancing until
    // all PEs are initialized.
    let cpu_id_is_thread_id = (MPIDR_EL1_RAW.read() & (1 << 24)) != 0;
    kprint!("topology_init: MPIDR_EL1.MT={}\n", u8::from(cpu_id_is_thread_id));

    // TODO(eieio): Determine automatically or provide a way to specify in the ZBI.
    /// The current value matches the depth of the first significant cache above.
    const DISTANCE_THRESHOLD: u32 = 2;

    // This platform initializes the topology earlier than this standard hook.
    // Setup the CPU distance map with the already initialized topology.
    let processor_count = topology::get_system_topology().processor_count();
    let distance = move |from_id: cpu_num_t, to_id: cpu_num_t| -> u32 {
        let topology = topology::get_system_topology();

        let Ok(from_node) = topology.processor_by_logical_id(from_id) else {
            kprint!("Failed to get processor node for CPU {}\n", from_id);
            return u32::MAX;
        };

        let Ok(to_node) = topology.processor_by_logical_id(to_id) else {
            kprint!("Failed to get processor node for CPU {}\n", to_id);
            return u32::MAX;
        };

        let from_info = arm64_info(from_node);
        let to_info = arm64_info(to_node);

        // Distance according to the MPIDR affinity levels: the maximum cache depth not shared
        // when multithreaded or single threaded, respectively.
        let affinity_distance = if cpu_id_is_thread_id {
            (1 * u32::from(from_info.cluster_1_id != to_info.cluster_1_id))
                .max(2 * u32::from(from_info.cluster_2_id != to_info.cluster_2_id))
                .max(3 * u32::from(from_info.cluster_3_id != to_info.cluster_3_id))
        } else {
            (1 * u32::from(from_info.cpu_id != to_info.cpu_id))
                .max(2 * u32::from(from_info.cluster_1_id != to_info.cluster_1_id))
                .max(3 * u32::from(from_info.cluster_2_id != to_info.cluster_2_id))
                .max(4 * u32::from(from_info.cluster_3_id != to_info.cluster_3_id))
        };

        // The affinity levels do not necessarily reflect the clusters the firmware describes. On
        // DynamIQ parts every core of a DSU reports the same AFF2 with MT set and a distinct AFF1,
        // which puts an entire heterogeneous SoC at affinity distance 1 and therefore into a
        // single logical cluster, even though the devicetree cpu-map (and the ZBI topology built
        // from it) groups the cores by cluster. Processors whose nearest cluster nodes in the
        // topology differ are at least the first significant distance apart, so that the
        // described clusters are never merged by auto-clustering.
        /// The nearest cluster ancestor of `node`, if it has one.
        fn cluster_of(node: &topology::Node) -> Option<&topology::Node> {
            let mut next = node.parent();
            while let Some(ancestor) = next {
                if let zbi::TopologyEntity::Cluster(_) = ancestor.entity() {
                    return Some(ancestor);
                }
                next = ancestor.parent();
            }
            None
        }
        let from_cluster = cluster_of(from_node);
        let to_cluster = cluster_of(to_node);
        let cluster_distance = match (from_cluster, to_cluster) {
            (Some(from_cluster), Some(to_cluster)) if !core::ptr::eq(from_cluster, to_cluster) => {
                DISTANCE_THRESHOLD
            }
            _ => 0,
        };

        affinity_distance.max(cluster_distance)
    };
    // SAFETY: The caller runs this once, on the boot CPU, before any secondary CPU has started and
    // before anything else uses the map.
    unsafe { CpuDistanceMap::initialize(processor_count, distance) };

    // SAFETY: The map was initialized above, and no secondary CPU has started.
    unsafe { CpuDistanceMap::set_distance_threshold(DISTANCE_THRESHOLD) };

    // SAFETY: As for `CpuDistanceMap::set_distance_threshold` above.
    unsafe { CpuDistanceMap::dump() };
}
