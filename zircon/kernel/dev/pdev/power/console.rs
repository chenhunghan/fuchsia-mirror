// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Power and Runtime Processor Power Management (RPPM) Console Commands.
//
// Exposes interactive kernel console commands (`power` and `rppm`) for inspecting
// and controlling system power lifecycle states, CPU performance limits, and
// energy model operating performance points.

#[cfg(not(target_arch = "x86_64"))]
use super::{
    PowerRebootFlags, power_cpu_off, power_cpu_on, power_get_cpu_state, power_opp_get,
    power_opp_get_domain_count, power_opp_set, power_reboot, power_shutdown,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler::Scheduler;
use crate::kernel::types::{
    BOOT_CPU_ID, INVALID_CPU, SMP_MAX_CPUS, cpu_mask_t, cpu_num_t, remove_cpu_from_mask,
};
use core::ffi::{CStr, c_int};
use debug::dprintf;
use fbl::Array;
use kprint::kprintln;
use zx_status::Status;
use zx_types::{ZX_CPU_PERF_LIMIT_TYPE_RATE, zx_cpu_perf_limit_t};

zr::static_assert!(SMP_MAX_CPUS <= cpu_mask_t::BITS as usize);

/// Dumps all registered power domains and per-CPU power state to the kernel console.
///
/// # Safety
///
/// The caller must ensure that per-CPU state is initialized.
unsafe fn rppm_dump() {
    // SAFETY: `BOOT_CPU_ID` (`0`) is less than `PerCpu::processor_count()` (which is always at
    // least `1`), and the caller guarantees that per-CPU state is initialized.
    let power_domain_set =
        unsafe { PerCpu::get(BOOT_CPU_ID) }.scheduler.get_power_domain_set_for_testing();
    power_domain_set.dump();

    kprintln!("");

    PerCpu::for_each(|cpu, percpu| {
        let snapshot = percpu.scheduler.power_state_snapshot();

        kprintln!("CPU {:u}:", cpu);
        kprintln!("  Power domain {}:", snapshot.domain_id.map_or(-1, i64::from));
        kprintln!(
            "  Current active power level {}.",
            snapshot.active_power_level.map_or(-1, i32::from),
        );
        kprintln!(
            "  Current idle state index {}.",
            snapshot.idle_state_index.map_or(-1, i32::from),
        );
        kprintln!("  Active power coefficient {:u} nw", snapshot.active_power_coefficient_nw);
        kprintln!("  Idle power {:u} nw", snapshot.idle_power_nw);
    });
}

/// Updates the active power level reported to `cpu`'s scheduler.
///
/// # Safety
///
/// The caller must ensure that per-CPU state is initialized and that no [`Scheduler`]
/// `queue_lock_` is held on the current CPU.
unsafe fn rppm_update_active_power_level(cpu: cpu_num_t, power_level: u8) -> Result<(), Status> {
    if (cpu as usize) >= PerCpu::processor_count() {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `cpu < PerCpu::processor_count()` is checked above, and the caller guarantees that
    // per-CPU state is initialized and no `Scheduler` `queue_lock_` is held on the current CPU.
    Status::ok(unsafe { PerCpu::get(cpu).scheduler.update_active_power_level(power_level) })
}

/// Posts a test power level transition request through `cpu`'s scheduler.
///
/// # Safety
///
/// The caller must ensure that per-CPU state is initialized and that no [`Scheduler`]
/// `queue_lock_` is held on the current CPU.
unsafe fn rppm_request_power_level_for_testing(cpu: cpu_num_t, power_level: u8) -> bool {
    if (cpu as usize) >= PerCpu::processor_count() {
        return false;
    }
    // SAFETY: `cpu < PerCpu::processor_count()` is checked above, and the caller guarantees that
    // per-CPU state is initialized and no `Scheduler` `queue_lock_` is held on the current CPU.
    unsafe { PerCpu::get(cpu).scheduler.request_power_level_for_testing(power_level) }
}

/// Returns the current active power level of `cpu`'s scheduler, if set.
///
/// # Safety
///
/// The caller must ensure that per-CPU state is initialized.
unsafe fn rppm_get_active_power_level(cpu: cpu_num_t) -> Option<u8> {
    if (cpu as usize) >= PerCpu::processor_count() {
        return None;
    }
    // SAFETY: `cpu < PerCpu::processor_count()` is checked above, and the caller guarantees that
    // per-CPU state is initialized.
    unsafe { PerCpu::get(cpu) }.scheduler.get_active_power_level()
}

/// Updates the processing rate limits for all CPUs selected in `cpu_mask_val`.
///
/// # Safety
///
/// The caller must ensure that per-CPU state is initialized and that no [`Scheduler`]
/// `queue_lock_` is held on the current CPU.
unsafe fn rppm_update_processing_limits(
    cpu_mask_val: u64,
    min_rate: u64,
    max_rate: u64,
) -> Result<(), Status> {
    let processor_count = PerCpu::processor_count();
    let max_cpu_mask = if processor_count >= 64 { u64::MAX } else { (1u64 << processor_count) - 1 };
    if cpu_mask_val > max_cpu_mask {
        return Err(Status::INVALID_ARGS);
    }
    let mut cpu_mask = cpu_mask_val as cpu_mask_t;
    let cpu_count = cpu_mask.count_ones() as usize;

    let mut limits =
        Array::<zx_cpu_perf_limit_t>::try_new(cpu_count).map_err(|_| Status::NO_MEMORY)?;

    for i in 0..cpu_count {
        let cpu = remove_cpu_from_mask(&mut cpu_mask);
        debug_assert!(cpu != INVALID_CPU);
        kprintln!("Setting CPU{:u} rate limits: min={:u} max={:u}", cpu, min_rate, max_rate);
        limits[i] = zx_cpu_perf_limit_t {
            logical_cpu_number: cpu,
            limit_type: ZX_CPU_PERF_LIMIT_TYPE_RATE,
            min: min_rate,
            max: max_rate,
        };
    }
    debug_assert!(cpu_mask == 0);

    // SAFETY: `PerCpu::processor_count() <= SMP_MAX_CPUS <= cpu_mask_t::BITS`, so
    // `cpu_mask_val <= max_cpu_mask` guarantees `cpu_count <= processor_count` and every
    // `logical_cpu_number` extracted from `cpu_mask` is `< processor_count`, and the caller
    // guarantees that per-CPU state is initialized and no `Scheduler` `queue_lock_` is held on
    // the current CPU.
    unsafe {
        Scheduler::update_processing_limits(&limits);
    }
    Ok(())
}

/// Helper function to safely read a null-terminated C string argument from `CmdArgs`.
///
/// # Safety
///
/// If `arg.arg_str` is non-null, it must point to a valid null-terminated C string.
unsafe fn arg_cstr(arg: &CmdArgs) -> Option<&CStr> {
    if arg.arg_str.is_null() {
        None
    } else {
        // SAFETY: Caller ensures `arg.arg_str` points to a valid null-terminated C string.
        Some(unsafe { CStr::from_ptr(arg.arg_str) })
    }
}

fn rppm_usage(cmd_name: &str) -> c_int {
    dprintf!(ALWAYS, "{cmd_name} dump\n");
    dprintf!(ALWAYS, "{cmd_name} set-level <cpu id> <0-based active level index>\n");
    dprintf!(ALWAYS, "{cmd_name} req-level <cpu id> <0-based active level index>\n");
    dprintf!(ALWAYS, "{cmd_name} get-level <cpu id>\n");
    dprintf!(ALWAYS, "{cmd_name} set-limit <cpu mask> <min rate> <max rate>\n");
    -1
}

/// Console command handler for `rppm` (Runtime Processor Power Management).
///
/// # Safety
///
/// The caller must ensure:
/// - If `argc > 0`, `argv` points to a valid array of `CmdArgs` structures of length `argc`, and
///   every non-null `arg_str` pointer in `argv[0..argc]` points to a valid null-terminated C
///   string.
/// - Per-CPU state is initialized, and no [`Scheduler`] `queue_lock_` is held on the current CPU.
pub unsafe extern "C" fn rppm_console_cmd(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 2 || argv.is_null() {
        dprintf!(ALWAYS, "not enough arguments\n");
        let cmd_name = if argc > 0 && !argv.is_null() {
            // SAFETY: Caller guarantees `argv` points to `argc` valid `CmdArgs`.
            let slice = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
            // SAFETY: Caller guarantees non-null `arg_str` points to a valid C string.
            unsafe { arg_cstr(&slice[0]) }.and_then(|c| c.to_str().ok()).unwrap_or("rppm")
        } else {
            "rppm"
        };
        return rppm_usage(cmd_name);
    }

    // SAFETY: Verified `argv` is non-null and `argc >= 2`.
    let slice = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    // SAFETY: Caller guarantees non-null `arg_str` points to a valid C string.
    let cmd_name = unsafe { arg_cstr(&slice[0]) }.and_then(|c| c.to_str().ok()).unwrap_or("rppm");
    // SAFETY: Caller guarantees non-null `arg_str` points to a valid C string.
    let subcmd = match unsafe { arg_cstr(&slice[1]) } {
        Some(cstr) => cstr,
        None => {
            dprintf!(ALWAYS, "not enough arguments\n");
            return rppm_usage(cmd_name);
        }
    };

    if subcmd == c"dump" {
        // SAFETY: Caller guarantees per-CPU state is initialized.
        unsafe {
            rppm_dump();
        }
        return 0;
    }

    let is_set_level = subcmd == c"set-level";
    let is_req_level = subcmd == c"req-level";
    let is_get_level = subcmd == c"get-level";
    let is_set_limit = subcmd == c"set-limit";

    if !(is_set_level || is_req_level || is_get_level || is_set_limit) {
        dprintf!(ALWAYS, "Unrecognized command\n");
        return rppm_usage(cmd_name);
    }

    if ((is_set_level || is_req_level) && argc < 4)
        || (is_get_level && argc < 3)
        || (is_set_limit && argc < 5)
    {
        dprintf!(ALWAYS, "not enough arguments\n");
        return rppm_usage(cmd_name);
    }

    let processor_count = PerCpu::processor_count();

    if is_set_limit {
        let cpu_mask_arg = slice[2].arg_uint as u64;
        let max_cpu_mask =
            if processor_count >= 64 { u64::MAX } else { (1u64 << processor_count) - 1 };
        if cpu_mask_arg > max_cpu_mask {
            dprintf!(
                ALWAYS,
                "Invalid cpu mask {:#x}. Valid values are in the range [0, {:#x}].\n",
                cpu_mask_arg,
                max_cpu_mask
            );
            return rppm_usage(cmd_name);
        }
        let min_rate = slice[3].arg_uint as u64;
        let max_rate = slice[4].arg_uint as u64;
        // SAFETY: Caller guarantees per-CPU state is initialized and no `Scheduler` `queue_lock_`
        // is held on the current CPU.
        let status = unsafe { rppm_update_processing_limits(cpu_mask_arg, min_rate, max_rate) };
        return Status::result_into_raw(status);
    }

    let cpu_id = slice[2].arg_uint as usize;
    if cpu_id >= processor_count {
        dprintf!(
            ALWAYS,
            "Invalid cpu id {}. Valid values are in the range [0, {}].\n",
            cpu_id,
            processor_count.saturating_sub(1)
        );
        return rppm_usage(cmd_name);
    }

    if is_set_level || is_req_level {
        let level_arg = slice[3].arg_uint;
        if level_arg > (u8::MAX as core::ffi::c_ulong) {
            dprintf!(ALWAYS, "Invalid power level\n");
            return rppm_usage(cmd_name);
        }
        let power_level = level_arg as u8;

        if is_set_level {
            // SAFETY: Caller guarantees per-CPU state is initialized and no `Scheduler`
            // `queue_lock_` is held on the current CPU.
            let status =
                unsafe { rppm_update_active_power_level(cpu_id as cpu_num_t, power_level) };
            if let Err(status) = status {
                dprintf!(ALWAYS, "Failed to set power level: {status}\n");
            } else {
                dprintf!(ALWAYS, "Set CPU {cpu_id} to power level {power_level}\n");
            }
        } else if is_req_level {
            // SAFETY: Caller guarantees per-CPU state is initialized and no `Scheduler`
            // `queue_lock_` is held on the current CPU.
            let posted =
                unsafe { rppm_request_power_level_for_testing(cpu_id as cpu_num_t, power_level) };
            if posted {
                dprintf!(ALWAYS, "CPU {cpu_id} request for power level {power_level} posted.\n");
            } else {
                dprintf!(ALWAYS, "CPU {cpu_id} request for power level {power_level} ignored.\n");
                // SAFETY: Caller guarantees per-CPU state is initialized.
                if let Some(current_level) =
                    unsafe { rppm_get_active_power_level(cpu_id as cpu_num_t) }
                {
                    dprintf!(ALWAYS, "CPU {cpu_id} at power level {current_level}\n");
                } else {
                    dprintf!(ALWAYS, "CPU {cpu_id} power level not set\n");
                }
            }
        }
    } else if is_get_level {
        // SAFETY: Caller guarantees per-CPU state is initialized.
        if let Some(power_level) = unsafe { rppm_get_active_power_level(cpu_id as cpu_num_t) } {
            dprintf!(ALWAYS, "CPU {cpu_id} at power level {power_level}\n");
        } else {
            dprintf!(ALWAYS, "CPU {cpu_id} power level not set\n");
        }
    }

    0
}

#[cfg(not(target_arch = "x86_64"))]
fn power_usage(cmd_name: &str) -> c_int {
    dprintf!(ALWAYS, "Usage: {cmd_name} <subcommand>\n");
    dprintf!(ALWAYS, "  reboot [normal|bootloader|recovery|panic] : reboot the system\n");
    dprintf!(ALWAYS, "  shutdown                                 : shut down the system\n");
    dprintf!(ALWAYS, "  cpu-off                                  : turn off calling cpu\n");
    dprintf!(ALWAYS, "  cpu-on <hw_cpu_id>                       : turn on cpu\n");
    dprintf!(ALWAYS, "  cpu-state <hw_cpu_id>                    : query cpu power state\n");
    dprintf!(ALWAYS, "  opp-domains                              : get OPP domain count\n");
    dprintf!(ALWAYS, "  opp-get <domain_id>                      : get OPP level for domain\n");
    dprintf!(ALWAYS, "  opp-set <domain_id> <opp>                : set OPP level for domain\n");
    -1
}

/// Console command handler for low-level PDEV power operations (`power`).
///
/// # Safety
///
/// If `argc > 0`, `argv` must point to a valid array of `CmdArgs` structures of length `argc`.
#[cfg(not(target_arch = "x86_64"))]
pub unsafe extern "C" fn power_console_cmd(
    argc: c_int,
    argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    if argc < 2 || argv.is_null() {
        let cmd_name = if argc > 0 && !argv.is_null() {
            // SAFETY: Caller guarantees `argv` points to `argc` valid `CmdArgs`.
            let slice = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
            // SAFETY: Valid C string argument.
            unsafe { arg_cstr(&slice[0]) }.and_then(|c| c.to_str().ok()).unwrap_or("power")
        } else {
            "power"
        };
        return power_usage(cmd_name);
    }

    // SAFETY: Verified `argv` is non-null and `argc >= 2`.
    let slice = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    // SAFETY: Valid C string argument.
    let cmd_name = unsafe { arg_cstr(&slice[0]) }.and_then(|c| c.to_str().ok()).unwrap_or("power");
    // SAFETY: Valid C string argument.
    let subcmd = match unsafe { arg_cstr(&slice[1]) } {
        Some(cstr) => cstr,
        None => return power_usage(cmd_name),
    };

    if subcmd == c"reboot" {
        let flags = if argc >= 3 {
            // SAFETY: Valid C string argument for reboot target.
            match unsafe { arg_cstr(&slice[2]) } {
                Some(c) if c == c"bootloader" => PowerRebootFlags::Bootloader,
                Some(c) if c == c"recovery" => PowerRebootFlags::Recovery,
                Some(c) if c == c"panic" => PowerRebootFlags::Panic,
                _ => PowerRebootFlags::Normal,
            }
        } else {
            PowerRebootFlags::Normal
        };
        power_reboot(flags);
        return 0;
    }

    if subcmd == c"shutdown" {
        power_shutdown();
        return 0;
    }

    if subcmd == c"cpu-off" {
        let status = power_cpu_off();
        dprintf!(ALWAYS, "cpu_off returned: {status:?}\n");
        return Status::result_into_raw(status);
    }

    if subcmd == c"cpu-on" {
        if argc < 3 {
            dprintf!(ALWAYS, "Missing hw_cpu_id argument\n");
            return power_usage(cmd_name);
        }
        let hw_cpu_id = slice[2].arg_uint as u64;
        let status = power_cpu_on(hw_cpu_id, 0, 0);
        dprintf!(ALWAYS, "cpu_on({hw_cpu_id}) returned: {status:?}\n");
        return Status::result_into_raw(status);
    }

    if subcmd == c"cpu-state" {
        if argc < 3 {
            dprintf!(ALWAYS, "Missing hw_cpu_id argument\n");
            return power_usage(cmd_name);
        }
        let hw_cpu_id = slice[2].arg_uint as u64;
        let res = power_get_cpu_state(hw_cpu_id);
        match res {
            Ok(state) => dprintf!(ALWAYS, "CPU {hw_cpu_id} state: {state:?}\n"),
            Err(err) => dprintf!(ALWAYS, "Failed to get CPU {hw_cpu_id} state: {err}\n"),
        }
        return Status::result_into_raw(res.map(|_| ()));
    }

    if subcmd == c"opp-domains" {
        let res = power_opp_get_domain_count();
        match res {
            Ok(count) => dprintf!(ALWAYS, "OPP domain count: {count}\n"),
            Err(err) => dprintf!(ALWAYS, "Failed to get OPP domain count: {err}\n"),
        }
        return Status::result_into_raw(res.map(|_| ()));
    }

    if subcmd == c"opp-get" {
        if argc < 3 {
            dprintf!(ALWAYS, "Missing domain_id argument\n");
            return power_usage(cmd_name);
        }
        let domain_id = slice[2].arg_uint as u32;
        let res = power_opp_get(domain_id);
        match res {
            Ok(opp) => dprintf!(ALWAYS, "Domain {domain_id} active OPP: {opp}\n"),
            Err(err) => dprintf!(ALWAYS, "Failed to get domain {domain_id} OPP: {err}\n"),
        }
        return Status::result_into_raw(res.map(|_| ()));
    }

    if subcmd == c"opp-set" {
        if argc < 4 {
            dprintf!(ALWAYS, "Missing domain_id or opp arguments\n");
            return power_usage(cmd_name);
        }
        let domain_id = slice[2].arg_uint as u32;
        let opp = slice[3].arg_uint as u64;
        let status = power_opp_set(domain_id, opp);
        if status.is_ok() {
            dprintf!(ALWAYS, "Set domain {domain_id} OPP to {opp}\n");
        } else if let Err(err) = status {
            dprintf!(ALWAYS, "Failed to set domain {domain_id} OPP to {opp}: {err}\n");
        }
        return Status::result_into_raw(status);
    }

    dprintf!(ALWAYS, "Unrecognized command\n");
    power_usage(cmd_name)
}

static_command!(
    RPPM_CMD,
    c"rppm".as_ptr(),
    c"runtime processor power management commands".as_ptr(),
    rppm_console_cmd,
    CMD_AVAIL_NORMAL
);

#[cfg(not(target_arch = "x86_64"))]
static_command!(
    POWER_CMD,
    c"power".as_ptr(),
    c"low-level platform power commands".as_ptr(),
    power_console_cmd,
    CMD_AVAIL_NORMAL
);
