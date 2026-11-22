// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/power/iris/power.cc
//
// Iris SoC CPU Power and Performance Driver.
//
// Implements platform power lifecycle operations (reboot, shutdown, CPU on/off)
// and hardware Operating Performance Point (OPP) frequency scaling across Iris CPU domains.

use crate::kernel::types::PAddr;
use crate::pdev_power::{
    ActivePowerLevelConfig, ControlInterface, IdlePowerModel, IdlePowerStateConfig, IdleStateFlags,
    PdevPowerOps, PowerCpuState, PowerDomainConfig, PowerRebootFlags, pdev_register_power,
    power_management_boot_boost_enabled, power_management_register_domains,
    power_management_rppm_enabled, power_management_set_rate_limits,
};
use crate::platform_rs::timer::DurationMono;
use crate::psci::{
    MAX_CPU_SUSPEND_STATES, PsciCpuSuspendState, PsciSuspendMode, psci_cpu_off, psci_cpu_on,
    psci_get_cpu_state, psci_get_cpu_suspend_states, psci_get_suspend_mode, psci_system_off,
    psci_system_reset_cold, psci_system_reset2_raw,
};
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use debug::dprintf;
use kalloc::Box;
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
#[cfg(ktest)]
use unittest as _;
use zbi::ArmPsciCpuSuspendStateFlags;
use zx_status::Status;

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Iris.
const VENDOR_SPECIFIC_WARM_RESET_TYPE: u32 = 0x8000_0000;

const POWER_DOMAIN_COUNT: usize = 4;

/// CPUs in each power domain: Little (CPUs 0-1), Medium 1 (CPUs 2-4), Medium 2 (CPUs 5-6), and
/// Big (CPU 7).
const DOMAIN_CPU_MASKS: [u64; POWER_DOMAIN_COUNT] = [0x03, 0x1c, 0x60, 0x80];
zr::static_assert!(
    (DOMAIN_CPU_MASKS[0] | DOMAIN_CPU_MASKS[1] | DOMAIN_CPU_MASKS[2] | DOMAIN_CPU_MASKS[3]) == 0xff
);
zr::static_assert!(
    DOMAIN_CPU_MASKS[0].count_ones()
        + DOMAIN_CPU_MASKS[1].count_ones()
        + DOMAIN_CPU_MASKS[2].count_ones()
        + DOMAIN_CPU_MASKS[3].count_ones()
        == 8
);

/// Maximum number of PSCI `CPU_SUSPEND` idle states registered for each power domain. Bounds per-
/// domain heap allocation and linear governor scans across CPU, cluster, and system topologies.
const MAX_PSCI_IDLE_STATES: usize = 8;
/// Total per-domain idle states (WFI + PSCI states) must fit in `EnergyModel`'s `u8` state indices.
const MAX_DOMAIN_IDLE_STATES: usize = 1 + MAX_PSCI_IDLE_STATES;
zr::static_assert!(MAX_DOMAIN_IDLE_STATES <= u8::MAX as usize + 1);

const DOMAIN0_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x00);
const DOMAIN1_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x08);
const DOMAIN2_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x10);
const DOMAIN3_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x18);
const OPP_REG_PHYS_BASE: PAddr = PAddr(0x200c_0790);
const OPP_BANK_SIZE: usize = 0x20;

static OPP_REG_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());

const NO_REQUESTED_OPP: u64 = u64::MAX;

// The last OPP the kernel requested for each domain, or `NO_REQUESTED_OPP` if none has been
// requested yet. This is a record of the kernel's intent, not a cache of the hardware state: the
// PERF_STATE registers can be changed behind the kernel's back (e.g. CAP firmware re-initializes
// them when leaving S3), so every OPP set or get goes to the hardware and this value is only used
// to re-apply the kernel's intent after such events. See b/576640818.
static REQUESTED_OPPS: [AtomicU64; POWER_DOMAIN_COUNT] = [
    AtomicU64::new(NO_REQUESTED_OPP),
    AtomicU64::new(NO_REQUESTED_OPP),
    AtomicU64::new(NO_REQUESTED_OPP),
    AtomicU64::new(NO_REQUESTED_OPP),
];

#[cfg(ktest)]
fn reset_requested_opps_for_test() {
    for opp in &REQUESTED_OPPS {
        opp.store(NO_REQUESTED_OPP, Ordering::SeqCst);
    }
}

crate::ksync_kernel::declare_singleton_spinlock!(Domain0Lock);
crate::ksync_kernel::declare_singleton_spinlock!(Domain1Lock);
crate::ksync_kernel::declare_singleton_spinlock!(Domain2Lock);
crate::ksync_kernel::declare_singleton_spinlock!(Domain3Lock);

/// Acquires the spinlock corresponding to `domain_index` and executes `f`.
fn with_domain_lock<R>(
    domain_index: usize,
    f: impl FnOnce() -> Result<R, Status>,
) -> Result<R, Status> {
    match domain_index {
        0 => {
            ksync::lock!(Domain0Lock::lock());
            f()
        }
        1 => {
            ksync::lock!(Domain1Lock::lock());
            f()
        }
        2 => {
            ksync::lock!(Domain2Lock::lock());
            f()
        }
        3 => {
            ksync::lock!(Domain3Lock::lock());
            f()
        }
        _ => Err(Status::INVALID_ARGS),
    }
}

#[derive(Copy, Clone, Debug)]
struct DomainInfo {
    opp_count: u32,
    mmio_offset: u32,
    boot_opp: u64,
    floor_opp: u64,
    reg_offset: Offset<u32, RwSafe>,
}

const DOMAIN_INFOS: [DomainInfo; 4] = [
    // Domain 0 (Little)
    DomainInfo {
        opp_count: 22,
        mmio_offset: 2,
        boot_opp: 8,
        floor_opp: 15,
        reg_offset: DOMAIN0_REG_OFFSET,
    },
    // Domain 1 (Medium 1):
    DomainInfo {
        opp_count: 24,
        mmio_offset: 0,
        boot_opp: 11,
        floor_opp: 20,
        reg_offset: DOMAIN1_REG_OFFSET,
    },
    // Domain 2 (Medium 2):
    DomainInfo {
        opp_count: 24,
        mmio_offset: 0,
        boot_opp: 11,
        floor_opp: 20,
        reg_offset: DOMAIN2_REG_OFFSET,
    },
    // Domain 3 (Big):
    DomainInfo {
        opp_count: 23,
        mmio_offset: 1,
        boot_opp: 10,
        floor_opp: 21,
        reg_offset: DOMAIN3_REG_OFFSET,
    },
];

/// Reboots the system via PSCI cold reset or vendor-specific warm reset for panic.
extern "C" fn iris_reboot(flags: PowerRebootFlags) -> Result<(), Status> {
    match flags {
        PowerRebootFlags::Normal | PowerRebootFlags::Bootloader | PowerRebootFlags::Recovery => {
            dprintf!(INFO, "Iris reboot: performing cold reset\n");
            psci_system_reset_cold()
        }
        PowerRebootFlags::Panic => {
            dprintf!(INFO, "Iris panic reboot: performing warm reset\n");
            // On Iris VENDOR_SPECIFIC_WARM_RESET_TYPE ignores the cookie parameter.
            psci_system_reset2_raw(VENDOR_SPECIFIC_WARM_RESET_TYPE, 0)
        }
    }
}

/// Shuts down the system via PSCI system off call.
extern "C" fn iris_shutdown() -> Result<(), Status> {
    psci_system_off()
}

/// Powers off the calling CPU core via PSCI cpu off call.
extern "C" fn iris_cpu_off() -> Result<(), Status> {
    psci_cpu_off()
}

/// Powers on the CPU core with the specified hardware ID via PSCI.
extern "C" fn iris_cpu_on(hw_cpu_id: u64, entry: u64, context: u64) -> Result<(), Status> {
    // SAFETY: `PdevPowerOps::cpu_on` callers (via `power_cpu_on`) supply the kernel secondary
    // entry physical address and bootstrap context (or operator-authorized console arguments).
    unsafe { psci_cpu_on(hw_cpu_id, entry, context) }
}

/// Retrieves the current power state of the CPU core with the specified hardware ID.
extern "C" fn iris_get_cpu_state(
    hw_cpu_id: u64,
    out_state: *mut PowerCpuState,
) -> Result<(), Status> {
    if out_state.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let state = psci_get_cpu_state(hw_cpu_id)?;
    // SAFETY: `out_state` was checked non-null above, and `PdevPowerOps::get_cpu_state` callers
    // supply a pointer valid for writes of `PowerCpuState`.
    unsafe { out_state.write(state) };
    Ok(())
}

/// Helper function to construct a `regio::MmioBank` for the OPP register block.
fn get_opp_bank() -> Option<MmioBank<u32, RwSafe>> {
    let base = OPP_REG_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: `OPP_REG_BASE` is mapped into kernel address space during early boot
    // and remains mapped for the kernel's lifetime.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    Some(MmioBank::new(ptr, OPP_BANK_SIZE))
}

/// Sets the active Operating Performance Point (OPP) for the specified power domain.
extern "C" fn iris_opp_set(domain_id: u32, opp: u64) -> Result<(), Status> {
    let Ok(domain_index) = usize::try_from(domain_id) else {
        return Err(Status::INVALID_ARGS);
    };
    let Some(info) = DOMAIN_INFOS.get(domain_index) else {
        return Err(Status::INVALID_ARGS);
    };
    if opp >= info.opp_count as u64 {
        return Err(Status::INVALID_ARGS);
    }

    // Clamp the requested OPP to the hardware floor OPP as driver-level defense in depth.
    let opp = opp.min(info.floor_opp);

    // Always write the hardware, even if the requested OPP matches the last request. The
    // PERF_STATE register is not exclusively owned by the kernel (see `REQUESTED_OPPS`), so
    // skipping writes based on remembered state can silently leave the domain at the wrong OPP.
    // Redundant writes are cheap: the GIA only interrupts the CAP firmware when PERF_STATE
    // differs from the current DVFS state.
    with_domain_lock(domain_index, || {
        let Some(bank) = get_opp_bank() else {
            return Err(Status::BAD_STATE);
        };

        let mmio_opp = (opp as u32) + info.mmio_offset;
        // SAFETY: `info.reg_offset` is within `OPP_BANK_SIZE` (0x20) and aligned to 4 bytes.
        let reg = unsafe { bank.at(info.reg_offset) };
        reg.write(mmio_opp);
        REQUESTED_OPPS[domain_index].store(opp, Ordering::Release);
        Ok(())
    })
}

/// Retrieves the active Operating Performance Point (OPP) for the specified power domain.
extern "C" fn iris_opp_get(domain_id: u32, out_opp: *mut u64) -> Result<(), Status> {
    if out_opp.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let Ok(domain_index) = usize::try_from(domain_id) else {
        return Err(Status::INVALID_ARGS);
    };
    let Some(info) = DOMAIN_INFOS.get(domain_index) else {
        return Err(Status::INVALID_ARGS);
    };

    // Always read the hardware rather than remembered state; see `iris_opp_set`.
    with_domain_lock(domain_index, || {
        let Some(bank) = get_opp_bank() else {
            return Err(Status::BAD_STATE);
        };

        // SAFETY: `info.reg_offset` is within `OPP_BANK_SIZE` (0x20) and aligned to 4 bytes.
        let reg = unsafe { bank.at(info.reg_offset) };
        let raw_val = reg.read();
        let opp = raw_val.saturating_sub(info.mmio_offset) as u64;
        // SAFETY: `out_opp` was checked non-null above.
        unsafe {
            *out_opp = opp;
        }
        Ok(())
    })
}

/// Retrieves the number of supported OPP control domains.
extern "C" fn iris_opp_get_domain_count(out_count: *mut usize) -> Result<(), Status> {
    if out_count.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `out_count` was checked non-null above.
    unsafe {
        *out_count = POWER_DOMAIN_COUNT;
    }
    Ok(())
}

/// Re-applies the kernel's last requested OPP to every domain after returning from system
/// suspend.
///
/// Leaving S3 cold-boots SSWRP_CPU, and the CAP M0 firmware's `cpu_dvfs_init()` rewrites the
/// PERF_STATE registers with its own initial OPPs. The kernel scheduler's power level bookkeeping
/// still reflects the OPPs it requested before suspend and will not issue a new request until
/// demand crosses a level boundary, so without this the domains would run at the firmware OPPs
/// for an unbounded time. Rewriting the requested OPPs brings the hardware back in line with the
/// scheduler's bookkeeping.
///
/// Domains that have never had an OPP requested are left alone.
extern "C" fn iris_system_resume() -> Result<(), Status> {
    let mut result = Ok(());
    for (domain_index, requested) in REQUESTED_OPPS.iter().enumerate() {
        let opp = requested.load(Ordering::Acquire);
        if opp == NO_REQUESTED_OPP {
            continue;
        }
        if let Err(status) = iris_opp_set(domain_index as u32, opp) {
            dprintf!(
                CRITICAL,
                "POWER: Failed to restore iris domain {} OPP {} on resume: {}\n",
                domain_index,
                opp,
                status.into_raw()
            );
            // Keep going so the remaining domains are still restored; report the first error.
            if result.is_ok() {
                result = Err(status);
            }
        }
    }
    result
}

static IRIS_POWER_OPS: PdevPowerOps = PdevPowerOps {
    reboot: Some(iris_reboot),
    shutdown: Some(iris_shutdown),
    cpu_off: Some(iris_cpu_off),
    cpu_on: Some(iris_cpu_on),
    get_cpu_state: Some(iris_get_cpu_state),
    opp_set: Some(iris_opp_set),
    opp_get: Some(iris_opp_get),
    opp_get_domain_count: Some(iris_opp_get_domain_count),
    system_resume: Some(iris_system_resume),
};

/// Early initialization hook for Iris power management and OPP register bank.
#[unsafe(no_mangle)]
pub extern "C" fn iris_power_init_early() {
    dprintf!(INFO, "POWER: registering iris power hooks\n");
    // Retrieves the mapped virtual address for the OPP peripheral block.
    let vaddr = crate::arch_rs::arm64::periph_paddr_to_vaddr(OPP_REG_PHYS_BASE)
        .map_or(core::ptr::null_mut(), |v| core::ptr::with_exposed_provenance_mut::<u32>(v.0));
    OPP_REG_BASE.store(vaddr, Ordering::Release);

    for (domain_id, info) in DOMAIN_INFOS.iter().enumerate() {
        let _ = iris_opp_set(domain_id as u32, info.boot_opp);
    }

    pdev_register_power(&IRIS_POWER_OPS);
}

fn zbi_domains_complete(domains: &[zbi::CpuEnergyModelDomain]) -> bool {
    domains.len() >= POWER_DOMAIN_COUNT
        && domains[..POWER_DOMAIN_COUNT].iter().all(|d| d.opp_count > 0)
}

/// Formatted ASCII diagnostic name (`"PSCI_CPU_0x..."` or `"PSCI_DOMAIN_0x..."`, at most 22 bytes)
/// borrowed by [`IdlePowerStateConfig::diagnostic_name`] until [`EnergyModel`] copies it inline.
#[derive(Clone, Copy)]
struct IdleNameBuf([u8; 24]);

impl IdleNameBuf {
    const EMPTY: Self = Self([0; 24]);

    fn set_psci_name(&mut self, targets_domain: bool, power_state: u32) -> &str {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let prefix: &[u8] = if targets_domain { b"PSCI_DOMAIN_0x" } else { b"PSCI_CPU_0x" };
        self.0[..prefix.len()].copy_from_slice(prefix);
        let mut cursor = prefix.len();
        let nibbles = (32 - power_state.leading_zeros()).div_ceil(4).max(1) as usize;
        for shift_idx in (0..nibbles).rev() {
            let nibble = ((power_state >> (shift_idx * 4)) & 0xf) as usize;
            self.0[cursor] = HEX[nibble];
            cursor += 1;
        }
        core::str::from_utf8(&self.0[..cursor]).unwrap_or("PSCI")
    }
}

/// Active OPP levels and idle states for one power domain.
struct DomainEnergyModel<'a> {
    active_levels: Box<[ActivePowerLevelConfig<'static>]>,
    idle_states: Box<[IdlePowerStateConfig<'a>]>,
}

/// Builds the active OPP levels and idle states for the power domain at `domain_index`, whose
/// CPUs are `cpu_mask`, from its ZBI OPPs and the PSCI idle states that every CPU in `cpu_mask`
/// may enter (excluding `TARGETS_POWER_DOMAIN` states when `allow_domain_states` is false).
fn build_domain_energy_model<'a>(
    domain_index: usize,
    domain: &zbi::CpuEnergyModelDomain,
    cpu_mask: u64,
    psci_states: &[PsciCpuSuspendState],
    allow_domain_states: bool,
    name_bufs: &'a mut [IdleNameBuf; MAX_PSCI_IDLE_STATES],
) -> Option<DomainEnergyModel<'a>> {
    let floor_opp = DOMAIN_INFOS.get(domain_index).map(|info| info.floor_opp).unwrap_or(u64::MAX);

    let num_opps = (domain.opp_count as usize)
        .min(domain.opps.len())
        .min(zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize);

    // Sort a 32-byte index array in ascending order of `(frequency_khz, capacity)` via insertion
    // sort so power levels are non-decreasing in processing rate regardless of ZBI input order,
    // without allocating a 640-byte `[CpuEnergyModelOpp; 32]` copy or `quicksort` scratch frame on
    // the kernel stack.
    let mut sorted_indices = [0u8; zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize];
    for i in 0..num_opps {
        let mut j = i;
        let key = {
            let opp = &domain.opps[i];
            (opp.frequency_khz, opp.capacity)
        };
        while j > 0 {
            let prev_opp = &domain.opps[sorted_indices[j - 1] as usize];
            if (prev_opp.frequency_khz, prev_opp.capacity) <= key {
                break;
            }
            sorted_indices[j] = sorted_indices[j - 1];
            j -= 1;
        }
        sorted_indices[j] = i as u8;
    }
    let sorted_indices_slice = &sorted_indices[..num_opps];

    // First pass: count how many active OPP levels satisfy the floor OPP threshold.
    // Iris hardware OPP index 0 corresponds to the fastest OPP, and index num_opps - 1
    // corresponds to the slowest OPP. Since sorted_indices_slice is ascending (idx 0 = slowest),
    // the hardware control argument is mapped as (num_opps - 1 - idx).
    let mut valid_opp_count = 0usize;
    for idx in 0..sorted_indices_slice.len() {
        let control_arg = (num_opps - 1 - idx) as u64;
        if control_arg <= floor_opp {
            valid_opp_count += 1;
        }
    }

    // Allocate filtered active OPP levels directly on the kernel heap rather than maintaining a
    // transient stack array.
    let mut uninit_active =
        Box::<[ActivePowerLevelConfig<'static>]>::try_new_uninit_slice(valid_opp_count).ok()?;

    if let Some(&last_opp_idx) = sorted_indices_slice.last() {
        let max_freq = u64::from(domain.opps[last_opp_idx as usize].frequency_khz);
        let mut write_idx = 0usize;
        for (idx, &opp_idx) in sorted_indices_slice.iter().enumerate() {
            let control_arg = (num_opps - 1 - idx) as u64;

            // Filter out power levels below the hardware floor OPP (i.e. control_arg > floor_opp).
            // Operating below this floor causes UFS storage controller and interconnect stalls.
            if control_arg > floor_opp {
                continue;
            }

            let opp = &domain.opps[opp_idx as usize];
            let rate = if opp.capacity > 0 {
                opp.capacity as u64
            } else if max_freq > 0 {
                u64::from(opp.frequency_khz)
                    .saturating_mul(domain.max_rate)
                    .div_ceil(max_freq)
                    .max(1)
            } else {
                1
            };
            let power_nw = if opp.power_uw > 0 {
                u64::from(opp.power_uw) * 1_000
            } else {
                rate.saturating_mul(200_000).saturating_add(10_000_000)
            };
            let static_power_nw = u64::from(opp.static_power_uw) * 1_000;

            uninit_active[write_idx].write(ActivePowerLevelConfig {
                options: 0,
                processing_rate: rate,
                active_power_nw: power_nw,
                static_power_nw,
                control_interface: ControlInterface::CpuDriver,
                control_argument: control_arg,
                diagnostic_name: "OPP",
            });
            write_idx += 1;
        }
        debug_assert_eq!(write_idx, valid_opp_count);
    }

    // Select the PSCI idle states with explicit CPU topology masks that every CPU in the domain
    // may enter, skipping unmapped boot-shim fallback masks (`cpu_mask_is_fallback`) and omitting
    // domain-targeted states when PSCI OS-initiated suspend mode is disabled
    // (`!allow_domain_states`). If multiple entries on the domain share a `power_state`, merge
    // their flags, domain `cpu_mask`s, and worst-case latencies and residency, then keep up to
    // `MAX_PSCI_IDLE_STATES` states (power-down states first, shallowest first within each class)
    // ordered for `EnergyModel`.
    let mut psci_idle_states = [PsciCpuSuspendState::EMPTY; MAX_CPU_SUSPEND_STATES];
    let mut num_psci_idle_states = 0usize;
    for state in psci_states {
        if state.cpu_mask_is_fallback || cpu_mask == 0 || (state.cpu_mask & cpu_mask) != cpu_mask {
            continue;
        }
        if let Some(retained) = psci_idle_states[..num_psci_idle_states]
            .iter_mut()
            .find(|retained| retained.power_state == state.power_state)
        {
            // Only union `cpu_mask` across domain-targeted entries (where `cpu_mask` means "CPUs
            // that must all be idle"); a per-CPU entry's `cpu_mask` only indicates which CPUs may
            // enter the state and must not widen a domain state's required-idle scope.
            let retained_targets_domain =
                retained.flags.contains(ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN);
            let state_targets_domain =
                state.flags.contains(ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN);
            match (retained_targets_domain, state_targets_domain) {
                (true, true) | (false, false) => retained.cpu_mask |= state.cpu_mask,
                (false, true) => retained.cpu_mask = state.cpu_mask,
                (true, false) => {}
            }
            retained.flags |= state.flags;
            retained.is_powerdown |= state.is_powerdown;
            retained.entry_latency_us = retained.entry_latency_us.max(state.entry_latency_us);
            retained.exit_latency_us = retained.exit_latency_us.max(state.exit_latency_us);
            retained.min_residency_us = retained.min_residency_us.max(state.min_residency_us);
        } else if num_psci_idle_states < psci_idle_states.len() {
            psci_idle_states[num_psci_idle_states] = *state;
            num_psci_idle_states += 1;
        }
    }
    // After merging duplicate `power_state`s, omit domain-targeted entries when OS-initiated
    // suspend mode is disabled and drop uncharacterized zero-residency entries (which otherwise
    // claim 0 ns break-even or sort `!is_powerdown` `ArmPsci` ahead of `ARM_WFI` in `EnergyModel`).
    let mut kept = 0usize;
    for i in 0..num_psci_idle_states {
        let state = psci_idle_states[i];
        let targets_domain =
            state.flags.contains(ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN);
        if !allow_domain_states && targets_domain {
            continue;
        }
        if state.min_residency_us == 0 {
            dprintf!(
                CRITICAL,
                "POWER: WARNING: skipping PSCI idle state 0x{:x} with zero min_residency_us for \
                 iris domain {}\n",
                state.power_state,
                domain_index
            );
            continue;
        }
        psci_idle_states[kept] = state;
        kept += 1;
    }
    num_psci_idle_states = kept;
    // Prioritize power-down states (`!state.is_powerdown` false < true) when applying
    // `MAX_PSCI_IDLE_STATES` so uncharacterized standby/retention states never evict deeper
    // power-collapse states, then re-sort the surviving `<= 8` entries into `EnergyModel`'s
    // `ClockGated`-first order. For `<= 16` elements `sort_unstable_by_key` runs an in-place
    // insertion sort with no scratch allocation.
    psci_idle_states[..num_psci_idle_states].sort_unstable_by_key(|state| {
        (
            !state.is_powerdown,
            state.min_residency_us,
            state.exit_latency_us,
            state.entry_latency_us,
            state.power_state,
        )
    });
    if num_psci_idle_states > MAX_PSCI_IDLE_STATES {
        dprintf!(
            CRITICAL,
            "POWER: WARNING: ignoring PSCI idle states beyond {} for iris domain {}\n",
            MAX_PSCI_IDLE_STATES,
            domain_index
        );
        num_psci_idle_states = MAX_PSCI_IDLE_STATES;
    }
    psci_idle_states[..num_psci_idle_states].sort_unstable_by_key(|state| {
        (
            state.is_powerdown,
            state.min_residency_us,
            state.exit_latency_us,
            state.entry_latency_us,
            state.power_state,
        )
    });
    let psci_idle_states = &psci_idle_states[..num_psci_idle_states];

    // Idle states consist of index 0 = clock-gated WFI followed by PSCI CPU_SUSPEND states ordered
    // from shallowest to deepest. Per-CPU PSCI states set `scope_cpu_mask` to `0` (scoped solely to
    // the calling CPU), whereas domain-targeted cluster and system states (retained only when PSCI
    // OS-initiated suspend mode is enabled) carry their full coordinated `state.cpu_mask` across
    // all covered DVFS domains.
    let idle_state_count = 1 + psci_idle_states.len();
    let mut uninit_idle =
        Box::<[IdlePowerStateConfig<'a>]>::try_new_uninit_slice(idle_state_count).ok()?;
    uninit_idle[0].write(IdlePowerStateConfig::ARM_WFI);
    let name_bufs = &mut name_bufs[..psci_idle_states.len()];
    for ((rank, state), name_buf) in psci_idle_states.iter().enumerate().zip(name_bufs.iter_mut()) {
        let targets_domain =
            state.flags.contains(ArmPsciCpuSuspendStateFlags::TARGETS_POWER_DOMAIN);
        let mut flags = IdleStateFlags::empty();
        if state.flags.contains(ArmPsciCpuSuspendStateFlags::LOCAL_TIMER_STOPS) {
            flags |= IdleStateFlags::LOCAL_TIMER_STOPS;
        }
        // TODO(https://fxbug.dev/588409143): Plumb characterized per-state retention and residual
        // power from ZBI into `EnergyModel` and order idle states by `min_residency_ns` directly.
        // Until then, standby/retention states (`!is_powerdown`) charge the active OPP's
        // `static_power_nw` (same as `ARM_WFI`) and power-down states (`is_powerdown`) use 0 nW.
        let power_model = if state.is_powerdown {
            IdlePowerModel::PowerGated { residual_power_nw: 0 }
        } else {
            IdlePowerModel::ClockGated
        };
        let diagnostic_name = name_buf.set_psci_name(targets_domain, state.power_state);
        uninit_idle[1 + rank].write(IdlePowerStateConfig {
            power_model,
            entry_latency_ns: DurationMono::from_micros(i64::from(state.entry_latency_us))
                .into_nanos(),
            exit_latency_ns: DurationMono::from_micros(i64::from(state.exit_latency_us))
                .into_nanos(),
            min_residency_ns: DurationMono::from_micros(i64::from(state.min_residency_us))
                .into_nanos(),
            scope_cpu_mask: if targets_domain { state.cpu_mask } else { 0 },
            flags,
            control_interface: ControlInterface::ArmPsci,
            control_argument: u64::from(state.power_state),
            diagnostic_name,
        });
    }

    // SAFETY: Every element of `uninit_active` (`valid_opp_count` entries) and `uninit_idle`
    // (`uninit_idle[0]` plus `0..psci_idle_states.len()`, which equals `idle_state_count`) was
    // explicitly initialized above.
    let (active_levels, idle_states) =
        unsafe { (uninit_active.assume_init(), uninit_idle.assume_init()) };
    Some(DomainEnergyModel { active_levels, idle_states })
}

/// Initializes Iris power domains and energy models for the kernel scheduler.
///
/// Both the `kernel.power.rppm` boot option and a valid, populated `domains` array
/// in the ZBI must be present to enable OPP control on Iris.
///
/// # Safety
///
/// If `domains` is non-null and `domain_count` > 0, caller must ensure `domains` points
/// to a valid array of `domain_count` initialized `zbi::CpuEnergyModelDomain` structs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_power_init(
    domains: *const zbi::CpuEnergyModelDomain,
    domain_count: usize,
) {
    if !power_management_rppm_enabled() {
        dprintf!(INFO, "POWER: Iris energy model registration disabled by boot option\n");
        return;
    }

    if domains.is_null() || domain_count == 0 {
        dprintf!(INFO, "POWER: Iris energy model not supplied in ZBI, skipping registration\n");
        return;
    }

    // SAFETY: Pointer and count are guaranteed valid by caller if non-null and non-zero.
    let domain_slice = unsafe { core::slice::from_raw_parts(domains, domain_count) };
    // EnergyModel rejects domains without active levels, and one invalid domain fails
    // registration for every domain, so validate all of them up front.
    if !zbi_domains_complete(domain_slice) {
        dprintf!(
            INFO,
            "POWER: Iris energy model in ZBI is empty or incomplete, skipping registration\n"
        );
        return;
    }

    dprintf!(INFO, "POWER: initializing iris power domains from ZBI energy model payload\n");

    let mut psci_states = [PsciCpuSuspendState::EMPTY; MAX_CPU_SUSPEND_STATES];
    let psci_state_count = psci_get_cpu_suspend_states(&mut psci_states);
    if psci_state_count > psci_states.len() {
        dprintf!(
            CRITICAL,
            "POWER: WARNING: PSCI retained {} idle states, truncating to {}\n",
            psci_state_count,
            psci_states.len()
        );
    }
    let psci_states = &psci_states[..psci_state_count.min(psci_states.len())];
    if psci_states.iter().any(|state| state.cpu_mask_is_fallback) {
        dprintf!(
            CRITICAL,
            "POWER: WARNING: skipping PSCI idle states with unmapped fallback cpu_mask on iris\n"
        );
    }
    let allow_domain_states = psci_get_suspend_mode() == PsciSuspendMode::OsInitiated;

    // Heap-allocate each domain's `ActivePowerLevelConfig`, `IdlePowerStateConfig`, and diagnostic
    // name buffers via `kalloc::Box` so all four domains' expanded descriptor tables are not held
    // simultaneously on the boot stack across `power_management_register_domains`.
    let Ok(mut all_idle_names) =
        Box::try_new([[IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES]; POWER_DOMAIN_COUNT])
    else {
        dprintf!(CRITICAL, "POWER: Failed to allocate memory for iris idle state names\n");
        return;
    };
    let mut models: [Option<DomainEnergyModel<'_>>; POWER_DOMAIN_COUNT] =
        [const { None }; POWER_DOMAIN_COUNT];
    let mut domain_configs = [PowerDomainConfig {
        domain_id: 0,
        cpu_mask: 0,
        active_levels: &[],
        idle_states: &[],
        active_transitions: &[],
    }; POWER_DOMAIN_COUNT];
    for ((domain_index, (model_slot, config)), name_bufs) in
        models.iter_mut().zip(domain_configs.iter_mut()).enumerate().zip(all_idle_names.iter_mut())
    {
        let Some(allocated_model) = build_domain_energy_model(
            domain_index,
            &domain_slice[domain_index],
            DOMAIN_CPU_MASKS[domain_index],
            psci_states,
            allow_domain_states,
            name_bufs,
        ) else {
            dprintf!(
                CRITICAL,
                "POWER: Failed to allocate memory for iris domain {} power levels\n",
                domain_index
            );
            return;
        };
        let model = model_slot.insert(allocated_model);
        *config = PowerDomainConfig {
            domain_id: domain_index as u32,
            cpu_mask: DOMAIN_CPU_MASKS[domain_index],
            active_levels: &model.active_levels,
            idle_states: &model.idle_states,
            active_transitions: &[],
        };
    }

    if let Err(status) = power_management_register_domains(&domain_configs) {
        dprintf!(CRITICAL, "POWER: Failed to register iris power domains: {}\n", status.into_raw());
        return;
    }

    dprintf!(INFO, "POWER: Registered iris power domains\n");

    // When boot boosting is enabled, lock processing rates to boot OPPs during startup to ensure
    // responsive boot performance, prevent the system from dropping to lower OPPs during I/O gaps,
    // and prevent excessive thermal dissipation before userspace thermal services initialize.
    //
    // Note: power_management_set_rate_limits operates in userspace rate units (scale of 1000)
    // where rate = round(capacity * 1000 / max_system_capacity=1024):
    // - Domain 0 (Little, CPUs 0-1): Boot OPP 8 (1.882 GHz, cap 200) -> rate 196
    // - Domain 1 (Medium 1, CPUs 2-4): Boot OPP 11 (1.785 GHz, cap 556) -> rate 543
    // - Domain 2 (Medium 2, CPUs 5-6): Boot OPP 11 (1.785 GHz, cap 555) -> rate 542
    // - Domain 3 (Big, CPU 7): Boot OPP 10 (2.208 GHz, cap 811) -> rate 792
    if power_management_boot_boost_enabled() {
        if let Err(status) = power_management_set_rate_limits(DOMAIN_CPU_MASKS[0], 196, 196) {
            dprintf!(
                CRITICAL,
                "POWER: Failed to set iris domain 0 boot performance limits: {}\n",
                status.into_raw()
            );
        }
        if let Err(status) = power_management_set_rate_limits(DOMAIN_CPU_MASKS[1], 543, 543) {
            dprintf!(
                CRITICAL,
                "POWER: Failed to set iris domain 1 boot performance limits: {}\n",
                status.into_raw()
            );
        }
        if let Err(status) = power_management_set_rate_limits(DOMAIN_CPU_MASKS[2], 542, 542) {
            dprintf!(
                CRITICAL,
                "POWER: Failed to set iris domain 2 boot performance limits: {}\n",
                status.into_raw()
            );
        }
        if let Err(status) = power_management_set_rate_limits(DOMAIN_CPU_MASKS[3], 792, 792) {
            dprintf!(
                CRITICAL,
                "POWER: Failed to set iris domain 3 boot performance limits: {}\n",
                status.into_raw()
            );
        }
    }
}

/// In-kernel unit tests for the Iris power driver.
#[cfg(ktest)]
#[unittest::suite(name = "iris_power")]
mod tests {
    use super::{
        ArmPsciCpuSuspendStateFlags, Box, ControlInterface, DOMAIN_CPU_MASKS, IdleNameBuf,
        IdlePowerModel, IdleStateFlags, MAX_PSCI_IDLE_STATES, OPP_REG_BASE, Ordering,
        PowerDomainConfig, PsciCpuSuspendState, build_domain_energy_model,
    };
    use crate::power_management::EnergyModel;
    use unittest::{
        assert_eq, assert_err, assert_false, assert_ok, assert_true, unwrap_ok, unwrap_some,
    };
    use zx_status::Status;

    /// Tests that passing a null output pointer to get_cpu_state returns INVALID_ARGS.
    #[test]
    fn test_iris_get_cpu_state_null_arg() {
        assert_err!(super::iris_get_cpu_state(0, core::ptr::null_mut()), Status::INVALID_ARGS);
    }

    /// Tests that with_domain_lock acquires and releases domain spinlocks properly.
    #[test]
    fn test_iris_domain_spinlock() {
        for domain in 0..4 {
            assert_ok!(super::with_domain_lock(domain, || Ok(())));
        }
        assert_err!(super::with_domain_lock(4, || Ok(())), Status::INVALID_ARGS);
    }

    /// Tests opp_get_domain_count.
    #[test]
    fn test_iris_opp_get_domain_count() {
        assert_err!(super::iris_opp_get_domain_count(core::ptr::null_mut()), Status::INVALID_ARGS);

        let mut count = 0usize;
        assert_ok!(super::iris_opp_get_domain_count(&mut count));
        assert_eq!(count, 4);
    }

    /// Tests opp_get and opp_set with mock backing memory, verifying the hardware is the source of truth.
    #[test]
    fn test_iris_opp_get_set() {
        super::reset_requested_opps_for_test();
        let mut mock_reg_bank = [0u32; 8];
        let old_base = OPP_REG_BASE.swap(mock_reg_bank.as_mut_ptr(), Ordering::SeqCst);

        // Test Domain 0 (mmio_offset = 2)
        assert_ok!(super::iris_opp_set(0, 5));
        assert_eq!(mock_reg_bank[0], 7); // 5 + 2
        assert_eq!(super::REQUESTED_OPPS[0].load(Ordering::SeqCst), 5);

        let mut opp = 0u64;
        assert_ok!(super::iris_opp_get(0, &mut opp));
        assert_eq!(opp, 5);

        // Setting the same OPP again must still write the hardware, so that an out-of-band
        // change to the register (e.g. firmware re-initialization on resume) is corrected.
        mock_reg_bank[0] = 0xbeef;
        assert_ok!(super::iris_opp_set(0, 5));
        assert_eq!(mock_reg_bank[0], 7); // Rewritten despite matching the last request.

        // Getting the OPP must reflect an out-of-band change to the register.
        mock_reg_bank[0] = 9; // OPP 7 + mmio_offset 2
        assert_ok!(super::iris_opp_get(0, &mut opp));
        assert_eq!(opp, 7);
        // The last request is unaffected by reads.
        assert_eq!(super::REQUESTED_OPPS[0].load(Ordering::SeqCst), 5);

        // Setting a different OPP writes to MMIO and records the request.
        assert_ok!(super::iris_opp_set(0, 6));
        assert_eq!(mock_reg_bank[0], 8); // 6 + 2
        assert_eq!(super::REQUESTED_OPPS[0].load(Ordering::SeqCst), 6);

        // Test Domain 3 (mmio_offset = 1, reg offset 0x18 -> index 6)
        assert_ok!(super::iris_opp_set(3, 10));
        assert_eq!(mock_reg_bank[6], 11); // 10 + 1

        assert_ok!(super::iris_opp_get(3, &mut opp));
        assert_eq!(opp, 10);

        // Test Out of bounds domain
        assert_err!(super::iris_opp_set(4, 0), Status::INVALID_ARGS);
        assert_err!(super::iris_opp_get(4, &mut opp), Status::INVALID_ARGS);

        // Test Out of bounds opp
        assert_err!(super::iris_opp_set(0, 22), Status::INVALID_ARGS);

        // Restore original base pointer and reset the requested OPPs.
        OPP_REG_BASE.store(old_base, Ordering::SeqCst);
        super::reset_requested_opps_for_test();
    }

    /// Tests that system_resume rewrites the last requested OPP for every domain that has one.
    #[test]
    fn test_iris_system_resume_restores_requested_opps() {
        super::reset_requested_opps_for_test();
        let mut mock_reg_bank = [0u32; 8];
        let old_base = OPP_REG_BASE.swap(mock_reg_bank.as_mut_ptr(), Ordering::SeqCst);

        // With nothing requested yet, resume is a no-op.
        assert_ok!(super::iris_system_resume());
        for reg in mock_reg_bank {
            assert_eq!(reg, 0);
        }

        // Request OPPs on domains 0 and 3 only.
        assert_ok!(super::iris_opp_set(0, 5)); // reg index 0 = 5 + 2
        assert_ok!(super::iris_opp_set(3, 10)); // reg index 6 = 10 + 1
        assert_eq!(mock_reg_bank[0], 7);
        assert_eq!(mock_reg_bank[6], 11);

        // Simulate firmware re-initializing the registers while suspended.
        mock_reg_bank[0] = 0xa + 2;
        mock_reg_bank[2] = 0xb; // domain 1, never requested
        mock_reg_bank[4] = 0xb; // domain 2, never requested
        mock_reg_bank[6] = 0xa + 1;

        assert_ok!(super::iris_system_resume());

        // Requested domains are restored; untouched domains keep the firmware value.
        assert_eq!(mock_reg_bank[0], 7);
        assert_eq!(mock_reg_bank[2], 0xb);
        assert_eq!(mock_reg_bank[4], 0xb);
        assert_eq!(mock_reg_bank[6], 11);

        // Restore original base pointer and reset the requested OPPs.
        OPP_REG_BASE.store(old_base, Ordering::SeqCst);
        super::reset_requested_opps_for_test();
    }

    const EMPTY_DOMAINS: [zbi::CpuEnergyModelDomain; 4] = [zbi::CpuEnergyModelDomain {
        cpu_mask: 0,
        max_rate: 0,
        domain_id: 0,
        opp_count: 0,
        opps: [zbi::CpuEnergyModelOpp {
            frequency_khz: 0,
            capacity: 0,
            power_uw: 0,
            voltage_mv: 0,
            static_power_uw: 0,
        }; zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize],
    }; 4];

    /// Tests that passing a null or empty config to `iris_power_init` returns gracefully.
    #[test]
    fn test_iris_power_init_null_or_empty_config() {
        // SAFETY: Testing null pointer handling.
        unsafe {
            super::iris_power_init(core::ptr::null(), 0);
        }
        // SAFETY: Pointer and length are valid for call duration.
        unsafe {
            super::iris_power_init(EMPTY_DOMAINS.as_ptr(), EMPTY_DOMAINS.len());
        }
    }

    /// Tests `zbi_domains_complete` across domain counts and per-domain `opp_count` values.
    #[test]
    fn test_zbi_domains_complete() {
        assert_false!(super::zbi_domains_complete(&EMPTY_DOMAINS[..3]));
        assert_false!(super::zbi_domains_complete(&EMPTY_DOMAINS));

        // Allocate the 2,656-byte 4-domain fixture on the kernel heap to keep it off the 8 KB
        // kernel thread stack.
        let mut uninit =
            Box::<[zbi::CpuEnergyModelDomain]>::try_new_uninit_slice(EMPTY_DOMAINS.len()).unwrap();
        for slot in uninit.iter_mut() {
            slot.write(EMPTY_DOMAINS[0]);
        }
        // SAFETY: Every element in `uninit` was initialized above.
        let mut domains = unsafe { uninit.assume_init() };
        domains[0].opp_count = 1;
        domains[1].opp_count = 1;
        domains[2].opp_count = 1;
        assert_false!(super::zbi_domains_complete(&domains));

        domains[3].opp_count = 1;
        assert_true!(super::zbi_domains_complete(&domains));
    }

    /// Tests that zero-capacity OPPs clamp frequency-derived processing rates to at least 1.
    #[test]
    fn test_build_domain_energy_model_zero_capacity_clamps_rate() {
        let mut domain = zbi::CpuEnergyModelDomain {
            cpu_mask: 0x03,
            max_rate: 0,
            domain_id: 0,
            opp_count: 2,
            opps: [zbi::CpuEnergyModelOpp {
                frequency_khz: 0,
                capacity: 0,
                power_uw: 0,
                voltage_mv: 0,
                static_power_uw: 0,
            }; zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize],
        };
        domain.opps[0].frequency_khz = 500_000;
        domain.opps[1].frequency_khz = 1_000_000;

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let levels =
            unwrap_some!(build_domain_energy_model(0, &domain, 0x03, &[], true, &mut names))
                .active_levels;
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].processing_rate, 1);
        assert_eq!(levels[1].processing_rate, 1);

        // With a non-zero `max_rate`, zero-capacity OPPs scale proportionally by frequency via
        // `div_ceil`, while `frequency_khz == 0` still clamps to 1.
        domain.max_rate = 100;
        domain.opp_count = 3;
        domain.opps[0].frequency_khz = 0;
        domain.opps[1].frequency_khz = 500_000;
        domain.opps[2].frequency_khz = 1_000_000;
        let scaled =
            unwrap_some!(build_domain_energy_model(0, &domain, 0x03, &[], true, &mut names))
                .active_levels;
        assert_eq!(scaled.len(), 3);
        assert_eq!(scaled[0].processing_rate, 1);
        assert_eq!(scaled[1].processing_rate, 50);
        assert_eq!(scaled[2].processing_rate, 100);
    }

    /// Tests OPP sorting and `static_power_uw` conversion in `build_domain_energy_model`.
    #[test]
    fn test_build_domain_energy_model_opp_sorting() {
        // Ascending OPPs (500MHz, 1000MHz, 2000MHz) with non-monotonic static leakage (1800 uW ->
        // 1500 uW -> 3200 uW).
        let mut domain = zbi::CpuEnergyModelDomain {
            cpu_mask: 0x03,
            max_rate: 150,
            domain_id: 0,
            opp_count: 3,
            opps: [zbi::CpuEnergyModelOpp {
                frequency_khz: 0,
                capacity: 0,
                power_uw: 0,
                voltage_mv: 0,
                static_power_uw: 0,
            }; zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize],
        };
        domain.opps[0] = zbi::CpuEnergyModelOpp {
            frequency_khz: 500_000,
            capacity: 50,
            power_uw: 100,
            voltage_mv: 700,
            static_power_uw: 1_800,
        };
        domain.opps[1] = zbi::CpuEnergyModelOpp {
            frequency_khz: 1_000_000,
            capacity: 100,
            power_uw: 200,
            voltage_mv: 800,
            static_power_uw: 1_500,
        };
        domain.opps[2] = zbi::CpuEnergyModelOpp {
            frequency_khz: 2_000_000,
            capacity: 150,
            power_uw: 300,
            voltage_mv: 900,
            static_power_uw: 3_200,
        };

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let asc_levels =
            unwrap_some!(build_domain_energy_model(0, &domain, 0x03, &[], true, &mut names))
                .active_levels;

        // Reverse in place to test descending OPPs (2000MHz, 1000MHz, 500MHz) without allocating a
        // second 664-byte `CpuEnergyModelDomain` on the kernel stack.
        domain.opps[..3].reverse();
        let desc_levels =
            unwrap_some!(build_domain_energy_model(0, &domain, 0x03, &[], true, &mut names))
                .active_levels;

        assert_eq!(asc_levels.len(), 3);
        assert_eq!(desc_levels.len(), 3);
        for (asc, desc) in asc_levels.iter().zip(desc_levels.iter()) {
            assert_eq!(asc.options, desc.options);
            assert_eq!(asc.processing_rate, desc.processing_rate);
            assert_eq!(asc.active_power_nw, desc.active_power_nw);
            assert_eq!(asc.static_power_nw, desc.static_power_nw);
            assert_eq!(asc.control_interface, desc.control_interface);
            assert_eq!(asc.control_argument, desc.control_argument);
            assert_true!(asc.diagnostic_name == desc.diagnostic_name);
        }
        assert_true!(*asc_levels == *desc_levels);

        // Verify 0-based active level ordering: Level 0 = 50 (ctrl arg 2), Level 1 = 100 (ctrl arg
        // 1), Level 2 = 150 (ctrl arg 0).
        assert_eq!(asc_levels[0].processing_rate, 50);
        assert_eq!(asc_levels[0].active_power_nw, 100_000);
        assert_eq!(asc_levels[0].static_power_nw, 1_800_000);
        assert_eq!(asc_levels[0].control_interface, super::ControlInterface::CpuDriver);
        assert_eq!(asc_levels[0].control_argument, 2);

        assert_eq!(asc_levels[1].processing_rate, 100);
        assert_eq!(asc_levels[1].active_power_nw, 200_000);
        assert_eq!(asc_levels[1].static_power_nw, 1_500_000);
        assert_eq!(asc_levels[1].control_argument, 1);

        assert_eq!(asc_levels[2].processing_rate, 150);
        assert_eq!(asc_levels[2].active_power_nw, 300_000);
        assert_eq!(asc_levels[2].static_power_nw, 3_200_000);
        assert_eq!(asc_levels[2].control_argument, 0);
    }

    /// Tests that `build_domain_energy_model` filters out power levels below `floor_opp`.
    #[test]
    fn test_build_domain_energy_model_opp_filtering_below_floor() {
        // Create a domain with 24 OPPs (frequencies 100MHz to 2400MHz, ctrl args 0..23).
        let mut domain = zbi::CpuEnergyModelDomain {
            cpu_mask: 0x60,
            max_rate: 2400,
            domain_id: 2,
            opp_count: 24,
            opps: [zbi::CpuEnergyModelOpp {
                frequency_khz: 0,
                capacity: 0,
                power_uw: 0,
                voltage_mv: 0,
                static_power_uw: 0,
            }; zbi::KERNEL_DRIVER_CPU_ENERGY_MODEL_MAX_OPPS as usize],
        };
        for (i, opp) in domain.opps.iter_mut().enumerate().take(24) {
            *opp = zbi::CpuEnergyModelOpp {
                frequency_khz: ((i + 1) * 100_000) as u32,
                capacity: ((i + 1) * 100) as u32,
                power_uw: ((i + 1) * 50) as u32,
                voltage_mv: 800,
                static_power_uw: ((i + 1) * 10) as u32,
            };
        }

        // Domain 2 has floor_opp = 20. Control args > 20 (i.e. 21..23) must be filtered out.
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let levels =
            unwrap_some!(build_domain_energy_model(2, &domain, 0x60, &[], true, &mut names))
                .active_levels;

        // 21 active OPP levels (ctrl args 0..20).
        assert_eq!(levels.len(), 21);

        // Slowest active level (index 0) should correspond to floor_opp (ctrl arg 20).
        assert_eq!(levels[0].control_argument, 20);
        assert_eq!(levels[0].processing_rate, 400);
        assert_eq!(levels[0].static_power_nw, 40_000);

        // Fastest active level (index 20) should correspond to max OPP (ctrl arg 0).
        assert_eq!(levels[20].control_argument, 0);
        assert_eq!(levels[20].processing_rate, 2400);
        assert_eq!(levels[20].static_power_nw, 240_000);
    }

    // Builds an Extended-format PSCI state (`StateType` is bit 30).
    const fn make_psci_state(
        power_state: u32,
        flags: u32,
        entry_latency_us: u32,
        exit_latency_us: u32,
        min_residency_us: u32,
        cpu_mask: u64,
    ) -> PsciCpuSuspendState {
        PsciCpuSuspendState {
            power_state,
            flags: ArmPsciCpuSuspendStateFlags::from_bits_retain(flags),
            is_powerdown: (power_state & (1 << 30)) != 0,
            cpu_mask_is_fallback: false,
            entry_latency_us,
            exit_latency_us,
            min_residency_us,
            cpu_mask,
        }
    }

    const fn make_fallback_psci_state(
        power_state: u32,
        flags: u32,
        entry_latency_us: u32,
        exit_latency_us: u32,
        min_residency_us: u32,
        cpu_mask: u64,
    ) -> PsciCpuSuspendState {
        let mut state = make_psci_state(
            power_state,
            flags,
            entry_latency_us,
            exit_latency_us,
            min_residency_us,
            cpu_mask,
        );
        state.cpu_mask_is_fallback = true;
        state
    }

    // Two-OPP test domain and synthetic PSCI idle states stored in `.rodata` to keep the 856-byte
    // fixtures off the 8 KB kernel thread stack.
    const SAMPLE_DOMAIN: zbi::CpuEnergyModelDomain = {
        let mut domain = EMPTY_DOMAINS[0];
        domain.cpu_mask = 0x60;
        domain.max_rate = 500;
        domain.domain_id = 2;
        domain.opp_count = 2;
        domain.opps[0] = zbi::CpuEnergyModelOpp {
            frequency_khz: 1_000_000,
            capacity: 250,
            power_uw: 200,
            voltage_mv: 800,
            static_power_uw: 1_200,
        };
        domain.opps[1] = zbi::CpuEnergyModelOpp {
            frequency_khz: 2_000_000,
            capacity: 500,
            power_uw: 400,
            voltage_mv: 900,
            static_power_uw: 2_800,
        };
        domain
    };

    // Deliberately unsorted, with states that only some or none of the domain's CPUs may enter.
    const SAMPLE_PSCI_STATES: [PsciCpuSuspendState; 6] = [
        // System-wide C4.
        make_psci_state(0x4002_0100, 0x3, 400, 1_000, 5_000, 0xff),
        // Other cluster's C2 shares `power_state` but must not merge its slower timings.
        make_psci_state(0x4000_0002, 0x1, 900, 900, 9_000, 0x1c),
        // Cluster C3 spanning domains 2 and 3.
        make_psci_state(0x4001_0010, 0x3, 200, 500, 2_000, 0xe0),
        // This domain's per-CPU C2.
        make_psci_state(0x4000_0002, 0x1, 50, 100, 500, 0x60),
        // Per-CPU retention (`!is_powerdown`, timer kept on).
        make_psci_state(0x0000_0001, 0x0, 20, 40, 120, 0x60),
        // Only CPU 6: excluded because CPU 5 cannot enter it.
        make_psci_state(0x4000_0005, 0x1, 1, 1, 1, 0x40),
    ];

    /// Tests PSCI diagnostic name formatting at zero, multi-digit hex, and max-width bounds.
    #[test]
    fn test_idle_name_buf_formats_zero_and_max_power_state() {
        let mut name = IdleNameBuf::EMPTY;
        assert_true!(name.set_psci_name(true, 0xffff_ffff) == "PSCI_DOMAIN_0xffffffff");
        assert_true!(name.set_psci_name(true, 0x89ab_cdef) == "PSCI_DOMAIN_0x89abcdef");
        // Reusing the same buffer for `power_state == 0` truncates cleanly to 12 bytes.
        assert_true!(name.set_psci_name(false, 0) == "PSCI_CPU_0x0");
    }

    /// Tests that the domain's PSCI idle states are populated with latencies, scope, and flags.
    #[test]
    fn test_build_domain_energy_model_psci_idle_states() {
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &SAMPLE_PSCI_STATES,
            true,
            &mut names,
        ));

        assert_eq!(model.active_levels.len(), 2);
        assert_eq!(model.active_levels[0].control_interface, ControlInterface::CpuDriver);
        assert_eq!(model.active_levels[0].processing_rate, 250);
        assert_eq!(model.active_levels[0].control_argument, 1);
        assert_eq!(model.active_levels[0].active_power_nw, 200_000);
        assert_eq!(model.active_levels[0].static_power_nw, 1_200_000);
        assert_eq!(model.active_levels[1].control_interface, ControlInterface::CpuDriver);
        assert_eq!(model.active_levels[1].processing_rate, 500);
        assert_eq!(model.active_levels[1].control_argument, 0);
        assert_eq!(model.active_levels[1].active_power_nw, 400_000);
        assert_eq!(model.active_levels[1].static_power_nw, 2_800_000);

        // WFI + retention + C2 + C3 + C4 = 5 idle states, ordered shallowest to deepest.
        assert_eq!(model.idle_states.len(), 5);
        let wfi = &model.idle_states[0];
        assert_true!(wfi.power_model == IdlePowerModel::ClockGated);
        assert_eq!(wfi.entry_latency_ns, 0);
        assert_eq!(wfi.exit_latency_ns, 0);
        assert_eq!(wfi.min_residency_ns, 0);
        assert_eq!(wfi.scope_cpu_mask, 0);
        assert_eq!(wfi.flags.bits(), 0);
        assert_eq!(wfi.control_interface, ControlInterface::ArmWfi);
        assert_eq!(wfi.control_argument, 0);
        assert_true!(wfi.diagnostic_name == "WFI");

        // Index 1: per-CPU retention (clock-gated static leakage, timer kept on).
        let retention = &model.idle_states[1];
        assert_true!(retention.power_model == IdlePowerModel::ClockGated);
        assert_eq!(retention.entry_latency_ns, 20_000);
        assert_eq!(retention.exit_latency_ns, 40_000);
        assert_eq!(retention.min_residency_ns, 120_000);
        assert_eq!(retention.scope_cpu_mask, 0);
        assert_eq!(retention.flags.bits(), 0);
        assert_eq!(retention.control_interface, ControlInterface::ArmPsci);
        assert_eq!(retention.control_argument, 0x0000_0001);
        assert_true!(retention.diagnostic_name == "PSCI_CPU_0x1");

        // Index 2: per-CPU C2 (local timer stops).
        let c2 = &model.idle_states[2];
        assert_true!(c2.power_model == IdlePowerModel::PowerGated { residual_power_nw: 0 });
        assert_eq!(c2.entry_latency_ns, 50_000);
        assert_eq!(c2.exit_latency_ns, 100_000);
        assert_eq!(c2.min_residency_ns, 500_000);
        assert_eq!(c2.scope_cpu_mask, 0);
        assert_eq!(c2.flags.bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());
        assert_eq!(c2.control_interface, ControlInterface::ArmPsci);
        assert_eq!(c2.control_argument, 0x4000_0002);
        assert_true!(c2.diagnostic_name == "PSCI_CPU_0x40000002");

        // Index 3: cluster C3 spanning domains 2 and 3.
        let c3 = &model.idle_states[3];
        assert_true!(c3.power_model == IdlePowerModel::PowerGated { residual_power_nw: 0 });
        assert_eq!(c3.entry_latency_ns, 200_000);
        assert_eq!(c3.exit_latency_ns, 500_000);
        assert_eq!(c3.min_residency_ns, 2_000_000);
        assert_eq!(c3.scope_cpu_mask, 0xe0);
        assert_eq!(c3.flags.bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());
        assert_eq!(c3.control_interface, ControlInterface::ArmPsci);
        assert_eq!(c3.control_argument, 0x4001_0010);
        assert_true!(c3.diagnostic_name == "PSCI_DOMAIN_0x40010010");

        // Index 4: system-wide C4.
        let c4 = &model.idle_states[4];
        assert_true!(c4.power_model == IdlePowerModel::PowerGated { residual_power_nw: 0 });
        assert_eq!(c4.entry_latency_ns, 400_000);
        assert_eq!(c4.exit_latency_ns, 1_000_000);
        assert_eq!(c4.min_residency_ns, 5_000_000);
        assert_eq!(c4.scope_cpu_mask, 0xff);
        assert_eq!(c4.flags.bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());
        assert_eq!(c4.control_interface, ControlInterface::ArmPsci);
        assert_eq!(c4.control_argument, 0x4002_0100);
        assert_true!(c4.diagnostic_name == "PSCI_DOMAIN_0x40020100");
    }

    /// Tests that `EnergyModel::create_from_config` accepts Iris PSCI idle states.
    #[test]
    fn test_psci_idle_model_is_accepted_by_energy_model() {
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let allocated = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &SAMPLE_PSCI_STATES,
            true,
            &mut names,
        ));
        let config = PowerDomainConfig {
            domain_id: 2,
            cpu_mask: 0x60,
            active_levels: &allocated.active_levels,
            idle_states: &allocated.idle_states,
            active_transitions: &[],
        };
        let energy_model =
            unwrap_ok!(EnergyModel::create_from_config(&config, 0), "valid iris energy model");

        assert_eq!(energy_model.active_levels().len(), 2);
        assert_eq!(energy_model.idle_states().len(), 5);

        // Indices 0..=1 are ClockGated (WFI and retention); indices 2..=4 are PowerGated PSCI
        // power-down states shallowest-first.
        assert_eq!(energy_model.idle_states()[0].control(), ControlInterface::ArmWfi);
        assert_true!(energy_model.idle_states()[0].power_model() == IdlePowerModel::ClockGated);
        assert_eq!(energy_model.idle_states()[1].control_argument(), 0x0000_0001);
        assert_true!(energy_model.idle_states()[1].power_model() == IdlePowerModel::ClockGated);
        assert_false!(energy_model.idle_states()[1].local_timer_stops());
        assert_eq!(energy_model.idle_states()[1].scope_cpu_mask(), 0);
        assert_eq!(energy_model.idle_states()[2].control_argument(), 0x4000_0002);
        assert_true!(energy_model.idle_states()[2].local_timer_stops());
        assert_eq!(energy_model.idle_states()[2].scope_cpu_mask(), 0);
        assert_eq!(energy_model.idle_states()[3].state_index(), 3);
        assert_true!(
            energy_model.idle_states()[3].power_model()
                == IdlePowerModel::PowerGated { residual_power_nw: 0 }
        );
        assert_eq!(energy_model.idle_states()[3].control_argument(), 0x4001_0010);
        assert_eq!(energy_model.idle_states()[3].scope_cpu_mask(), 0xe0);
        assert_eq!(energy_model.idle_states()[3].exit_latency_ns(), 500_000);
        assert_eq!(energy_model.idle_states()[4].control_argument(), 0x4002_0100);
        assert_eq!(energy_model.idle_states()[4].scope_cpu_mask(), 0xff);

        // Clock-gated WFI and standby/retention states scale with the active OPP's
        // static_power_nw, whereas power-gated PSCI states consume 0 nW independent of active OPP.
        for clock_gated_idx in 0..=1 {
            assert_eq!(energy_model.idle_power_nw(clock_gated_idx, Some(0)), 1_200_000);
            assert_eq!(energy_model.idle_power_nw(clock_gated_idx, Some(1)), 2_800_000);
        }
        for state_idx in 2..5 {
            assert_eq!(energy_model.idle_power_nw(state_idx, Some(0)), 0);
            assert_eq!(energy_model.idle_power_nw(state_idx, Some(1)), 0);
        }

        // Verify `EnergyModel` copied diagnostic names inline out of `allocated` and `names`, and
        // check `u32::MAX` microsecond-to-nanosecond widening without 32-bit overflow.
        drop(allocated);
        names.fill(IdleNameBuf::EMPTY);
        assert_true!(energy_model.idle_states()[0].name() == b"WFI".as_slice());
        assert_true!(energy_model.idle_states()[1].name() == b"PSCI_CPU_0x1".as_slice());
        assert_true!(energy_model.idle_states()[2].name() == b"PSCI_CPU_0x40000002".as_slice());
        assert_true!(energy_model.idle_states()[3].name() == b"PSCI_DOMAIN_0x40010010".as_slice());
        assert_true!(energy_model.idle_states()[4].name() == b"PSCI_DOMAIN_0x40020100".as_slice());

        const MAX_US_STATE: [PsciCpuSuspendState; 1] =
            [make_psci_state(0x4000_00ff, 0x1, u32::MAX, u32::MAX, u32::MAX, 0x60)];
        let max_us_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &MAX_US_STATE,
            true,
            &mut names,
        ));
        assert_eq!(max_us_model.idle_states[1].entry_latency_ns, 4_294_967_295_000);
        assert_eq!(max_us_model.idle_states[1].exit_latency_ns, 4_294_967_295_000);
        assert_eq!(max_us_model.idle_states[1].min_residency_ns, 4_294_967_295_000);
    }

    /// Tests that cluster and system PSCI states route across `DOMAIN_CPU_MASKS` domains.
    #[test]
    fn test_build_domain_energy_model_psci_cluster_state_spans_domains() {
        // Domain 0 (`0x03`) matches only system C4 (`0xff`); domain 1 (`0x1c`) matches its per-CPU
        // C2 (`0x1c`) and system C4 (`0xff`).
        let mut d0_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let d0_model = unwrap_some!(build_domain_energy_model(
            0,
            &SAMPLE_DOMAIN,
            DOMAIN_CPU_MASKS[0],
            &SAMPLE_PSCI_STATES,
            true,
            &mut d0_names,
        ));
        assert_eq!(d0_model.idle_states.len(), 2);
        assert_eq!(d0_model.idle_states[1].control_argument, 0x4002_0100);

        let mut d1_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let d1_model = unwrap_some!(build_domain_energy_model(
            1,
            &SAMPLE_DOMAIN,
            DOMAIN_CPU_MASKS[1],
            &SAMPLE_PSCI_STATES,
            true,
            &mut d1_names,
        ));
        assert_eq!(d1_model.idle_states.len(), 3);
        assert_eq!(d1_model.idle_states[1].control_argument, 0x4002_0100);
        assert_eq!(d1_model.idle_states[2].control_argument, 0x4000_0002);

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let big_model = unwrap_some!(build_domain_energy_model(
            3,
            &SAMPLE_DOMAIN,
            DOMAIN_CPU_MASKS[3],
            &SAMPLE_PSCI_STATES,
            true,
            &mut names,
        ));
        assert_eq!(big_model.active_levels.len(), 2);
        // WFI + cluster C3 (0xe0) + system C4 (0xff).
        assert_eq!(big_model.idle_states.len(), 3);
        assert_eq!(big_model.idle_states[1].control_argument, 0x4001_0010);
        assert_eq!(big_model.idle_states[1].scope_cpu_mask, 0xe0);
        assert_eq!(big_model.idle_states[1].exit_latency_ns, 500_000);
        assert_eq!(big_model.idle_states[2].control_argument, 0x4002_0100);
        assert_eq!(big_model.idle_states[2].scope_cpu_mask, 0xff);
    }

    /// Tests that domain-targeted PSCI states are omitted when OS-initiated mode is disabled.
    #[test]
    fn test_build_domain_energy_model_psci_omits_domain_states_without_osi() {
        // Domain 2 keeps WFI + retention + per-CPU C2 while omitting cluster C3 and system C4.
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &SAMPLE_PSCI_STATES,
            false,
            &mut names,
        ));
        assert_eq!(model.idle_states.len(), 3);
        assert_eq!(model.idle_states[0].control_interface, ControlInterface::ArmWfi);
        assert_eq!(model.idle_states[1].control_argument, 0x0000_0001);
        assert_eq!(model.idle_states[1].scope_cpu_mask, 0);
        assert_true!(model.idle_states[1].diagnostic_name == "PSCI_CPU_0x1");
        assert_eq!(model.idle_states[2].control_argument, 0x4000_0002);
        assert_eq!(model.idle_states[2].scope_cpu_mask, 0);
        assert_true!(model.idle_states[2].diagnostic_name == "PSCI_CPU_0x40000002");

        // Domain 3 (`0x80`) only matches domain-targeted states in `SAMPLE_PSCI_STATES`, so
        // disabling domain states leaves clock-gated WFI only.
        let mut big_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let big_model = unwrap_some!(build_domain_energy_model(
            3,
            &SAMPLE_DOMAIN,
            0x80,
            &SAMPLE_PSCI_STATES,
            false,
            &mut big_names,
        ));
        assert_eq!(big_model.idle_states.len(), 1);
        assert_eq!(big_model.idle_states[0].control_interface, ControlInterface::ArmWfi);
    }

    /// Tests mixed-flag duplicate PSCI states and the 8-state cap without OS-initiated mode.
    #[test]
    fn test_build_domain_energy_model_psci_mixed_duplicates_and_cap_without_osi() {
        // A duplicate `power_state` on this domain that carries `TARGETS_POWER_DOMAIN` on only one
        // entry is still excluded after merging, while an unrelated cluster's domain state
        // (`0x1c`) sharing a `power_state` ID does not suppress this domain's per-CPU state
        // (`0x60`), and omitted shallow domain states do not consume `MAX_PSCI_IDLE_STATES` slots.
        const MIXED_STATES: [PsciCpuSuspendState; 12] = [
            make_psci_state(0x4001_0010, 0x1, 5, 10, 50, 0x60),
            make_psci_state(0x4001_0010, 0x2, 5, 10, 50, 0xe0),
            make_psci_state(0x4002_0100, 0x3, 6, 12, 60, 0xff),
            make_psci_state(0x4000_0001, 0x3, 1, 1, 1, 0x1c),
            make_psci_state(0x4000_0001, 0x1, 10, 100, 300, 0x60),
            make_psci_state(0x4000_0002, 0x1, 20, 200, 600, 0x60),
            make_psci_state(0x4000_0003, 0x1, 30, 300, 900, 0x60),
            make_psci_state(0x4000_0004, 0x1, 40, 400, 1_200, 0x60),
            make_psci_state(0x4000_0005, 0x1, 50, 500, 1_500, 0x60),
            make_psci_state(0x4000_0006, 0x1, 60, 600, 1_800, 0x60),
            make_psci_state(0x4000_0007, 0x1, 70, 700, 2_100, 0x60),
            make_psci_state(0x4000_0008, 0x1, 80, 800, 2_400, 0x60),
        ];
        const EXPECTED_NAMES: [&str; MAX_PSCI_IDLE_STATES] = [
            "PSCI_CPU_0x40000001",
            "PSCI_CPU_0x40000002",
            "PSCI_CPU_0x40000003",
            "PSCI_CPU_0x40000004",
            "PSCI_CPU_0x40000005",
            "PSCI_CPU_0x40000006",
            "PSCI_CPU_0x40000007",
            "PSCI_CPU_0x40000008",
        ];
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let mixed_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &MIXED_STATES,
            false,
            &mut names,
        ));
        assert_eq!(mixed_model.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);
        for (idx, id_low) in (1u64..=8).enumerate() {
            assert_eq!(mixed_model.idle_states[1 + idx].control_argument, 0x4000_0000 | id_low);
            assert_eq!(mixed_model.idle_states[1 + idx].scope_cpu_mask, 0);
            assert_true!(mixed_model.idle_states[1 + idx].diagnostic_name == EXPECTED_NAMES[idx]);
        }
    }

    /// Tests that a domain-targeted PSCI state without `LOCAL_TIMER_STOPS` keeps the local timer.
    #[test]
    fn test_build_domain_energy_model_psci_domain_state_without_timer_stop() {
        let psci_states = [make_psci_state(0x4001_0011, 0x2, 100, 200, 1_000, 0xe0)];
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];

        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &psci_states,
            true,
            &mut names,
        ));

        assert_eq!(model.idle_states.len(), 2);
        assert_eq!(model.idle_states[1].flags.bits(), 0);
        assert_eq!(model.idle_states[1].scope_cpu_mask, 0xe0);
        assert_true!(model.idle_states[1].diagnostic_name == "PSCI_DOMAIN_0x40010011");
    }

    /// Tests that duplicate `power_state` entries merge flags, domain masks, and timings.
    #[test]
    fn test_build_domain_energy_model_psci_deduplicates_power_state() {
        // When a per-CPU entry (`0x70`, `flags = 0x1`) merges with a domain-targeted entry
        // (`0xe0`, `flags = 0x2`), `scope_cpu_mask` retains only the domain-targeted mask (`0xe0`)
        // in either input order so CPU 4 (`0x10`) outside the PSCI domain is not required-idle.
        let per_cpu_then_domain = [
            make_psci_state(0x4001_0010, 0x1, 300, 400, 2_500, 0x70),
            make_psci_state(0x4001_0010, 0x2, 200, 600, 2_000, 0xe0),
        ];
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &per_cpu_then_domain,
            true,
            &mut names,
        ));
        assert_eq!(model.idle_states.len(), 2);
        let merged = &model.idle_states[1];
        assert_true!(merged.power_model == IdlePowerModel::PowerGated { residual_power_nw: 0 });
        assert_eq!(merged.control_argument, 0x4001_0010);
        assert_eq!(merged.entry_latency_ns, 300_000);
        assert_eq!(merged.exit_latency_ns, 600_000);
        assert_eq!(merged.min_residency_ns, 2_500_000);
        assert_eq!(merged.scope_cpu_mask, 0xe0);
        assert_eq!(merged.flags.bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());
        assert_true!(merged.diagnostic_name == "PSCI_DOMAIN_0x40010010");

        let domain_then_per_cpu = [per_cpu_then_domain[1], per_cpu_then_domain[0]];
        let mut rev_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let rev_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &domain_then_per_cpu,
            true,
            &mut rev_names,
        ));
        assert_true!(
            rev_model.idle_states[1].power_model
                == IdlePowerModel::PowerGated { residual_power_nw: 0 }
        );
        assert_eq!(rev_model.idle_states[1].scope_cpu_mask, 0xe0);
        assert_eq!(rev_model.idle_states[1].flags.bits(), IdleStateFlags::LOCAL_TIMER_STOPS.bits());

        // Two domain-targeted entries sharing a `power_state` still union their `cpu_mask`s.
        let two_domain_entries = [
            make_psci_state(0x4001_0010, 0x3, 300, 400, 2_500, 0x70),
            make_psci_state(0x4001_0010, 0x2, 200, 600, 2_000, 0xe0),
        ];
        let mut dom_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let dom_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &two_domain_entries,
            true,
            &mut dom_names,
        ));
        assert_eq!(dom_model.idle_states[1].scope_cpu_mask, 0xf0);
    }

    /// Tests that duplicate `power_state` entries are collapsed before applying the 8-state cap.
    #[test]
    fn test_build_domain_energy_model_psci_dedups_before_capping() {
        // Nine unique per-CPU `power_state`s (`0x4000_0001..=0x4000_0009`) plus a duplicate of
        // `0x4000_0002` at index 9 (`>= MAX_PSCI_IDLE_STATES`) carrying worse timings and a
        // superset `cpu_mask`.
        const PSCI_STATES: [PsciCpuSuspendState; 10] = [
            make_psci_state(0x4000_0001, 0x1, 10, 100, 300, 0xff),
            make_psci_state(0x4000_0002, 0x1, 20, 200, 600, 0x60),
            make_psci_state(0x4000_0003, 0x1, 30, 300, 900, 0xff),
            make_psci_state(0x4000_0004, 0x1, 40, 400, 1_200, 0xff),
            make_psci_state(0x4000_0005, 0x1, 50, 500, 1_500, 0xff),
            make_psci_state(0x4000_0006, 0x1, 60, 600, 1_800, 0xff),
            make_psci_state(0x4000_0007, 0x1, 70, 700, 2_100, 0xff),
            make_psci_state(0x4000_0008, 0x1, 80, 800, 2_400, 0xff),
            make_psci_state(0x4000_0009, 0x1, 90, 900, 2_700, 0xff),
            make_psci_state(0x4000_0002, 0x1, 75, 250, 650, 0xff),
        ];

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &PSCI_STATES,
            true,
            &mut names,
        ));
        // 10 entries collapse to 9 unique `power_state`s, then cap to WFI + 8 (`1..=8`).
        assert_eq!(model.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);

        // `0x4000_0002` merged the trailing duplicate's slower timings before capping, while
        // keeping `scope_cpu_mask == 0` because it remains per-CPU (`flags == 0x1`).
        let merged = &model.idle_states[2];
        assert_eq!(merged.control_argument, 0x4000_0002);
        assert_eq!(merged.entry_latency_ns, 75_000);
        assert_eq!(merged.exit_latency_ns, 250_000);
        assert_eq!(merged.min_residency_ns, 650_000);
        assert_eq!(merged.scope_cpu_mask, 0);

        assert_eq!(model.idle_states[1].control_argument, 0x4000_0001);
        assert_eq!(model.idle_states[1].scope_cpu_mask, 0);
        assert_eq!(model.idle_states[8].control_argument, 0x4000_0008);
        assert_eq!(model.idle_states[8].scope_cpu_mask, 0);
    }

    /// Tests that unmapped boot-shim fallback masks and zero-residency entries are skipped on Iris.
    #[test]
    fn test_build_domain_energy_model_psci_skips_fallback_and_zero_residency() {
        const FALLBACK_ZERO_AND_EXPLICIT: [PsciCpuSuspendState; 5] = [
            make_fallback_psci_state(0x4000_0002, 0x1, 50, 100, 500, 0xff),
            make_fallback_psci_state(0x4001_0010, 0x3, 200, 500, 2_000, 0xff),
            // Zero-residency retention and power-down states must be dropped so `ARM_WFI` stays at
            // `idle_states()[0]` and power-collapse never advertises 0 ns break-even residency.
            make_psci_state(0x0000_0001, 0x0, 0, 0, 0, 0x60),
            make_psci_state(0x4000_0003, 0x1, 50, 100, 0, 0x60),
            // Explicit system-wide `0xff` state (`cpu_mask_is_fallback == false`) is still kept.
            make_psci_state(0x4002_0100, 0x3, 400, 1_000, 5_000, 0xff),
        ];

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &FALLBACK_ZERO_AND_EXPLICIT,
            true,
            &mut names,
        ));
        assert_eq!(model.idle_states.len(), 2);
        assert_eq!(model.idle_states[0].control_interface, ControlInterface::ArmWfi);
        assert_eq!(model.idle_states[1].control_argument, 0x4002_0100);
        assert_eq!(model.idle_states[1].scope_cpu_mask, 0xff);

        // If only fallback or zero-residency entries exist, the domain degrades cleanly to WFI.
        let mut skipped_only_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let skipped_only = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &FALLBACK_ZERO_AND_EXPLICIT[..4],
            true,
            &mut skipped_only_names,
        ));
        assert_eq!(skipped_only.idle_states.len(), 1);
        assert_eq!(skipped_only.idle_states[0].control_interface, ControlInterface::ArmWfi);

        // A zero-residency duplicate merges into its characterized twin before the zero-residency
        // filter runs, so the kept state retains the duplicate's worst-case latencies.
        const ZERO_RESIDENCY_DUPLICATE: [PsciCpuSuspendState; 2] = [
            make_psci_state(0x4000_0002, 0x1, 50, 100, 500, 0x60),
            make_psci_state(0x4000_0002, 0x1, 80, 300, 0, 0x60),
        ];
        let mut dup_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let dup_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &ZERO_RESIDENCY_DUPLICATE,
            true,
            &mut dup_names,
        ));
        assert_eq!(dup_model.idle_states.len(), 2);
        assert_eq!(dup_model.idle_states[1].entry_latency_ns, 80_000);
        assert_eq!(dup_model.idle_states[1].exit_latency_ns, 300_000);
        assert_eq!(dup_model.idle_states[1].min_residency_ns, 500_000);

        // Zero-residency entries are filtered before the 8-state cap and never evict a
        // characterized state.
        drop(model);
        const ZERO_RESIDENCY_PLUS_FULL_CAP: [PsciCpuSuspendState; 9] = [
            make_psci_state(0x0000_0001, 0x0, 0, 0, 0, 0x60),
            make_psci_state(0x4000_0001, 0x1, 10, 100, 300, 0x60),
            make_psci_state(0x4000_0002, 0x1, 20, 200, 600, 0x60),
            make_psci_state(0x4000_0003, 0x1, 30, 300, 900, 0x60),
            make_psci_state(0x4000_0004, 0x1, 40, 400, 1_200, 0x60),
            make_psci_state(0x4000_0005, 0x1, 50, 500, 1_500, 0x60),
            make_psci_state(0x4000_0006, 0x1, 60, 600, 1_800, 0x60),
            make_psci_state(0x4000_0007, 0x1, 70, 700, 2_100, 0x60),
            make_psci_state(0x4000_0008, 0x1, 80, 800, 2_400, 0x60),
        ];
        let cap_model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &ZERO_RESIDENCY_PLUS_FULL_CAP,
            true,
            &mut names,
        ));
        assert_eq!(cap_model.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);
        assert_eq!(cap_model.idle_states[1].control_argument, 0x4000_0001);
        assert_eq!(cap_model.idle_states[8].control_argument, 0x4000_0008);
    }

    /// Tests that a domain without PSCI idle states keeps only clock-gated WFI.
    #[test]
    fn test_build_domain_energy_model_without_psci_states_keeps_wfi_only() {
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let wfi_only_model =
            unwrap_some!(build_domain_energy_model(2, &SAMPLE_DOMAIN, 0x60, &[], true, &mut names));
        assert_eq!(wfi_only_model.active_levels.len(), 2);
        assert_eq!(wfi_only_model.idle_states.len(), 1);
        assert_eq!(wfi_only_model.idle_states[0].control_interface, ControlInterface::ArmWfi);
    }

    /// Tests that a zero `cpu_mask` does not match any PSCI idle states.
    #[test]
    fn test_build_domain_energy_model_psci_zero_cpu_mask_skips_idle_states() {
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0,
            &SAMPLE_PSCI_STATES,
            true,
            &mut names,
        ));
        assert_eq!(model.active_levels.len(), 2);
        assert_eq!(model.idle_states.len(), 1);
        assert_eq!(model.idle_states[0].control_interface, ControlInterface::ArmWfi);
    }

    /// Tests that power-down states take priority under the 8-state cap before `EnergyModel` sort.
    #[test]
    fn test_build_domain_energy_model_psci_caps_at_max_idle_states() {
        // Eight power-down states (`0x4000_005b` and `0x4000_005d..=0x4000_0063`) plus two
        // retention states (`0x0000_005a` and `0x0000_005c`) exercise cap-survival priority and
        // every tiebreaker in `(min_residency_us, exit_latency_us, entry_latency_us, power_state)`.
        const PSCI_STATES: [PsciCpuSuspendState; 10] = [
            // Deepest power-down state: survives ahead of shallower retention states when 8
            // power-down states compete for 8 slots.
            make_psci_state(0x4000_005b, 0x1, 90, 900, 4_000, 0x60),
            // Deeper retention state (`!is_powerdown`): dropped ahead of `0x0000_005c`.
            make_psci_state(0x0000_005a, 0x0, 85, 850, 3_600, 0x60),
            // Shallower retention state (`!is_powerdown`): dropped when all 8 power-down states are
            // present, but survives and re-sorts to `idle_states[1]` when `<= 7` power-down states
            // are present.
            make_psci_state(0x0000_005c, 0x0, 80, 800, 100, 0x60),
            // Ties residency=2_000, higher exit latency.
            make_psci_state(0x4000_005d, 0x1, 70, 700, 2_000, 0x60),
            // Ties residency=2_000 and exit=500, higher entry latency.
            make_psci_state(0x4000_005e, 0x1, 95, 500, 2_000, 0x60),
            make_psci_state(0x4000_005f, 0x1, 20, 500, 2_000, 0x60),
            // Ties all timings with `0x4000_0060`, higher `power_state`.
            make_psci_state(0x4000_0061, 0x1, 40, 300, 1_000, 0x60),
            make_psci_state(0x4000_0060, 0x1, 40, 300, 1_000, 0x60),
            make_psci_state(0x4000_0062, 0x1, 30, 200, 800, 0x60),
            make_psci_state(0x4000_0063, 0x1, 10, 100, 400, 0x60),
        ];

        // Case 1: 8 power-down + 2 retention -> all 8 power-down states (including deepest
        // `0x4000_005b`) survive, while both retention states are dropped.
        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let all_powerdown = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &PSCI_STATES,
            true,
            &mut names,
        ));
        assert_eq!(all_powerdown.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);
        assert_eq!(all_powerdown.idle_states[1].control_argument, 0x4000_0063);
        assert_eq!(all_powerdown.idle_states[2].control_argument, 0x4000_0062);
        assert_eq!(all_powerdown.idle_states[3].control_argument, 0x4000_0060);
        assert_eq!(all_powerdown.idle_states[4].control_argument, 0x4000_0061);
        assert_eq!(all_powerdown.idle_states[5].control_argument, 0x4000_005f);
        assert_eq!(all_powerdown.idle_states[6].control_argument, 0x4000_005e);
        assert_eq!(all_powerdown.idle_states[7].control_argument, 0x4000_005d);
        assert_eq!(all_powerdown.idle_states[8].control_argument, 0x4000_005b);

        // Case 2: 7 power-down + 2 retention (`&PSCI_STATES[1..]`) -> shallower retention
        // `0x0000_005c` takes the 8th slot ahead of `0x0000_005a`, then re-sorts to index 1
        // (`ClockGated`) ahead of the 7 `PowerGated` power-down states to match `EnergyModel`.
        let mut mixed_names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &PSCI_STATES[1..],
            true,
            &mut mixed_names,
        ));
        assert_eq!(model.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);
        assert_eq!(model.idle_states[1].control_argument, 0x0000_005c);
        assert_true!(model.idle_states[1].power_model == IdlePowerModel::ClockGated);
        assert_eq!(model.idle_states[2].control_argument, 0x4000_0063);
        assert_eq!(model.idle_states[3].control_argument, 0x4000_0062);
        assert_eq!(model.idle_states[4].control_argument, 0x4000_0060);
        assert_eq!(model.idle_states[5].control_argument, 0x4000_0061);
        assert_eq!(model.idle_states[6].control_argument, 0x4000_005f);
        assert_eq!(model.idle_states[7].control_argument, 0x4000_005e);
        assert_eq!(model.idle_states[8].control_argument, 0x4000_005d);

        // Verify `EnergyModel::create_from_config` ranks the re-sorted survivors identically.
        let config = PowerDomainConfig {
            domain_id: 2,
            cpu_mask: 0x60,
            active_levels: &model.active_levels,
            idle_states: &model.idle_states,
            active_transitions: &[],
        };
        let energy_model = unwrap_ok!(EnergyModel::create_from_config(&config, 0));
        assert_eq!(energy_model.idle_states()[0].control(), ControlInterface::ArmWfi);
        for idx in 1..=MAX_PSCI_IDLE_STATES {
            assert_eq!(
                energy_model.idle_states()[idx].control_argument(),
                model.idle_states[idx].control_argument
            );
        }
    }

    /// Tests that cap eviction breaks `min_residency_us` ties by exit, entry, and `power_state`.
    #[test]
    fn test_build_domain_energy_model_psci_cap_eviction_tiebreakers() {
        const CAP_TIE_STATES: [PsciCpuSuspendState; 11] = [
            make_psci_state(0x4000_0001, 0x1, 10, 100, 100, 0x60),
            make_psci_state(0x4000_0002, 0x1, 10, 100, 200, 0x60),
            make_psci_state(0x4000_0003, 0x1, 10, 100, 300, 0x60),
            make_psci_state(0x4000_0004, 0x1, 10, 100, 400, 0x60),
            make_psci_state(0x4000_0005, 0x1, 10, 100, 500, 0x60),
            make_psci_state(0x4000_0006, 0x1, 10, 100, 600, 0x60),
            make_psci_state(0x4000_0007, 0x1, 10, 100, 700, 0x60),
            // Worse exit_latency_us (700 > 500), despite lower entry_latency_us and power_state.
            make_psci_state(0x4000_0070, 0x1, 10, 700, 2_000, 0x60),
            // Tied exit_latency_us (500), worse entry_latency_us (95 > 40), lower power_state.
            make_psci_state(0x4000_0071, 0x1, 95, 500, 2_000, 0x60),
            // Tied residency/exit/entry, higher power_state (0x73 > 0x72).
            make_psci_state(0x4000_0073, 0x1, 40, 500, 2_000, 0x60),
            // Winner for the 8th PSCI slot.
            make_psci_state(0x4000_0072, 0x1, 40, 500, 2_000, 0x60),
        ];

        let mut names = [IdleNameBuf::EMPTY; MAX_PSCI_IDLE_STATES];
        let model = unwrap_some!(build_domain_energy_model(
            2,
            &SAMPLE_DOMAIN,
            0x60,
            &CAP_TIE_STATES,
            true,
            &mut names,
        ));
        assert_eq!(model.idle_states.len(), 1 + MAX_PSCI_IDLE_STATES);
        assert_eq!(model.idle_states[8].control_argument, 0x4000_0072);
    }
}
