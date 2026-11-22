// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hardware breakpoints and watchpoints, the per-thread MDSCR_EL1 debug enables, and the exception
//! frame dump.

use kprint::kprint;
use libarch::arm64::{
    BreakpointControlRegister, DEBUG_BANK_SIZE, DebugFeatureRegister0, ID_AA64DFR0_EL1,
    MonitorDebugSystemControlRegister, WatchpointControlRegister, write_dbgbcr, write_dbgbvr,
    write_dbgwcr, write_dbgwvr,
};

use super::exceptions::alt_hex_lead;
use super::vm::is_user_accessible;
use super::{Iframe, isb};

/// Kernel tracking of the current state of the debug registers for a particular thread
/// (`arm64_debug_state_t`).
pub(super) type Arm64DebugState = arm64_thread_bindings::arm64_debug_state_t;

const ARM64_MAX_HW_BREAKPOINTS: usize = arm64_thread_bindings::ARM64_MAX_HW_BREAKPOINTS as usize;
const ARM64_MAX_HW_WATCHPOINTS: usize = arm64_thread_bindings::ARM64_MAX_HW_WATCHPOINTS as usize;

// The bank writers accept any index below `DEBUG_BANK_SIZE`.
zr::static_assert!(ARM64_MAX_HW_BREAKPOINTS == DEBUG_BANK_SIZE);
zr::static_assert!(ARM64_MAX_HW_WATCHPOINTS == DEBUG_BANK_SIZE);

/// `DBGBCR<n>_EL1` of an active breakpoint: enabled, PMC = 0b10 and BAS = 0b1111.
const ARM64_DBGBCR_ACTIVE_MASK: u32 = (BreakpointControlRegister::E_MASK
    | (0b10 << BreakpointControlRegister::PMC_SHIFT)
    | BreakpointControlRegister::BAS_MASK) as u32;

/// The actual addresses bits that we will allow the user to write for a hw breakpoint.
const ARM64_DBGBVR_USER_MASK: u64 = 0xffff_ffff_fffc;

/// The actual fields userspace can write to.
const ARM64_DBGWCR_ACTIVE_MASK: u32 = (WatchpointControlRegister::E_MASK
    | WatchpointControlRegister::LSC_MASK
    | WatchpointControlRegister::BAS_MASK) as u32;

/// The DBGWVR format is as follows (x = writeable):
///
/// ```text
/// |63  ...  49|48           ...             2|10|
/// |0000...0000|xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx|00|
/// ```
const ARM64_DBGWVR_USER_MASK: u64 = 0xffff_ffff_fffc;

// These masks and field positions match the architecture, and the DBGBCR and DBGWCR layouts in
// <zircon/hw/debug/arm64.h>.
zr::static_assert!(ARM64_DBGBCR_ACTIVE_MASK == 0x1e5);
zr::static_assert!(ARM64_DBGWCR_ACTIVE_MASK == 0x1ff9);
zr::static_assert!(BreakpointControlRegister::E_MASK == 1);
zr::static_assert!(WatchpointControlRegister::E_MASK == 1);
zr::static_assert!(
    WatchpointControlRegister::PAC_MASK == 0b11 << 1 && WatchpointControlRegister::PAC_SHIFT == 1
);
zr::static_assert!(
    WatchpointControlRegister::SSC_MASK == 0b11 << 14 && WatchpointControlRegister::SSC_SHIFT == 14
);
zr::static_assert!(
    DebugFeatureRegister0::BRPS_MASK == 0xf000 && DebugFeatureRegister0::BRPS_SHIFT == 12
);
zr::static_assert!(
    DebugFeatureRegister0::WRPS_MASK == 0xf0_0000 && DebugFeatureRegister0::WRPS_SHIFT == 20
);
zr::static_assert!(MonitorDebugSystemControlRegister::MDE_MASK == 1 << 15);
zr::static_assert!(MonitorDebugSystemControlRegister::KDE_MASK == 1 << 13);

/// Enables or disables the HW debug functionalities in `mdscr_el1`, a thread's saved MDSCR_EL1.
pub(super) fn arm64_set_debug_state_for_thread(mdscr_el1: &mut u64, active: bool) {
    let mut mdscr = MonitorDebugSystemControlRegister::from(*mdscr_el1);
    if active {
        mdscr.set_mde(true); // MDE enables hardware exceptions.
        mdscr.set_kde(true); // KDE enables local debugging in EL0.
    } else {
        mdscr.set_mde(false); // MDE enables hardware exceptions.
        mdscr.set_kde(false); // KDE enables local debugging in EL0.
    }
    *mdscr_el1 = mdscr.bits();
}

fn arm64_validate_hw_breakpoints(state: &mut Arm64DebugState) -> Option<u32> {
    let mut breakpoint_count = 0;

    // Validate that the addresses are valid. HW breakpoints that are beyond the
    // arch resource count will be ignored. This is because zircon will never
    // update or use those values in any way, so they will always be zero,
    // independent of what the user provided within those.
    let hw_bp_count = usize::from(arm64_hw_breakpoint_count());
    for hw_bp in state.hw_bps.iter_mut().take(hw_bp_count) {
        let dbgbcr = hw_bp.dbgbcr;
        let dbgbvr = hw_bp.dbgbvr;

        // Verify that the breakpoint refers to userspace.
        if dbgbvr != 0 && !is_user_accessible(dbgbvr as usize) {
            return None;
        }
        hw_bp.dbgbvr &= ARM64_DBGBVR_USER_MASK;

        // If the address is valid and the breakpoint is activated, we mask in
        // the other necessary value and count it for bookkeepping.
        if BreakpointControlRegister::from(u64::from(dbgbcr)).e() {
            hw_bp.dbgbcr = ARM64_DBGBCR_ACTIVE_MASK;
            breakpoint_count += 1;
        } else {
            hw_bp.dbgbcr = 0;
        }
    }

    Some(breakpoint_count)
}

fn arm64_validate_hw_watchpoints(state: &mut Arm64DebugState) -> Option<u32> {
    let mut watchpoint_count = 0;
    // Validate that the addresses are valid. HW watchpoint that are beyond the
    // arch resource count will be ignored. This is because zircon will never
    // update or use those values in any way, so they will always be zero,
    // independent of what the user provided within those.
    let hw_wp_count = usize::from(arm64_hw_watchpoint_count());
    for hw_wp in state.hw_wps.iter_mut().take(hw_wp_count) {
        let dbgwcr = &mut hw_wp.dbgwcr;
        let dbgwvr = &mut hw_wp.dbgwvr;

        // Verify that the breakpoint refers to userspace.
        if *dbgwvr != 0 && !is_user_accessible(*dbgwvr as usize) {
            return None;
        }
        *dbgwvr &= ARM64_DBGWVR_USER_MASK;

        // If the address is valid and the watchpoint is active, we mask in
        // the other necessary bits.
        if WatchpointControlRegister::from(u64::from(*dbgwcr)).e() {
            *dbgwcr &= ARM64_DBGWCR_ACTIVE_MASK;

            // See zircon/hw/debug/arm64.h for details on the fields set for PAC, HMC and SSC.
            // The mask leaves HMC and SSC at 0.
            let mut wcr = WatchpointControlRegister::from(u64::from(*dbgwcr));
            wcr.set_pac(0b10);
            *dbgwcr = wcr.bits() as u32;

            watchpoint_count += 1;
        }
    }

    Some(watchpoint_count)
}

/// Checks whether `state` is valid to install on a running thread.
///
/// Validates that every DBGBVR and DBGWVR value is either 0 or a valid userspace address, and
/// masks out reserved values in the control registers. The masking is for the caller's
/// convenience, since there is no good mechanism to tell the user what went wrong with the call.
///
/// Values beyond the CPU's register count are ignored. Zircon never uses them, so they are
/// always zero, whatever the user provided in them.
///
/// On success, returns the number of activated breakpoints and watchpoints in `state`.
pub(super) fn arm64_validate_debug_state(state: &mut Arm64DebugState) -> Option<(u32, u32)> {
    let active_breakpoints = arm64_validate_hw_breakpoints(state)?;
    let active_watchpoints = arm64_validate_hw_watchpoints(state)?;
    Some((active_breakpoints, active_watchpoints))
}

/// Returns the amount of HW breakpoints present in this CPU.
pub fn arm64_hw_breakpoint_count() -> u8 {
    let count = ID_AA64DFR0_EL1.read().brps() + 1;
    debug_assert!(usize::from(count) <= ARM64_MAX_HW_BREAKPOINTS);
    count
}

/// Returns the amount of HW watchpoints present in this CPU.
pub fn arm64_hw_watchpoint_count() -> u8 {
    let count = ID_AA64DFR0_EL1.read().wrps() + 1;
    debug_assert!(usize::from(count) <= ARM64_MAX_HW_WATCHPOINTS);
    count
}

// Writing Debug State ---------------------------------------------------------------------------

/// Writes breakpoint `index`'s control and value registers.
///
/// The bank writers panic on an `index` of `ARM64_MAX_HW_BREAKPOINTS` or more, which no caller
/// passes: the count it stays below is a 4-bit field plus one.
fn arm64_write_hw_breakpoint_by_index(index: usize, dbgbcr: u32, dbgbvr: u64) {
    write_dbgbcr(index, BreakpointControlRegister::from(u64::from(dbgbcr)));
    write_dbgbvr(index, dbgbvr);
    isb();
}

/// Writes watchpoint `index`'s control and value registers.
///
/// The bank writers panic on an `index` of `ARM64_MAX_HW_WATCHPOINTS` or more, which no caller
/// passes: the count it stays below is a 4-bit field plus one.
fn arm64_write_hw_watchpoint_by_index(index: usize, dbgwcr: u32, dbgwvr: u64) {
    write_dbgwcr(index, WatchpointControlRegister::from(u64::from(dbgwcr)));
    write_dbgwvr(index, dbgwvr);
    isb();
}

/// Writes `debug_state` into the CPU registers.
///
/// This is used in the context switch, so no validation is done, just writing. In any other
/// context (e.g. setting debug values from a syscall), the state must pass
/// `arm64_validate_debug_state` first.
///
/// # Safety
///
/// `debug_state` must have passed `arm64_validate_debug_state`, as a thread's saved debug state did
/// when it was set: unvalidated values can arm breakpoints and watchpoints on kernel addresses.
pub(super) unsafe fn arm64_write_hw_debug_regs(debug_state: &Arm64DebugState) {
    // Write the HW Breakpoints.
    let bps_count = usize::from(arm64_hw_breakpoint_count());
    for (i, hw_bp) in debug_state.hw_bps.iter().enumerate().take(bps_count) {
        let dbgbcr = hw_bp.dbgbcr;
        let dbgbvr = hw_bp.dbgbvr;
        arm64_write_hw_breakpoint_by_index(i, dbgbcr, dbgbvr);
    }

    // Write the HW Watchpoints.
    let wps_count = usize::from(arm64_hw_watchpoint_count());
    for (i, hw_wp) in debug_state.hw_wps.iter().enumerate().take(wps_count) {
        let dbgwcr = hw_wp.dbgwcr;
        let dbgwvr = hw_wp.dbgwvr;
        arm64_write_hw_watchpoint_by_index(i, dbgwcr, dbgwvr);
    }
}

/// Prints `frame`'s registers to the console.
pub(super) fn print_frame(frame: &Iframe) {
    kprint!("iframe {:p}:\n", frame);
    kprint!(
        "x0  {:cs}{:x} x1  {:cs}{:x} x2  {:cs}{:x} x3  {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[0]),
        frame.r[0],
        alt_hex_lead::<18>(frame.r[1]),
        frame.r[1],
        alt_hex_lead::<18>(frame.r[2]),
        frame.r[2],
        alt_hex_lead::<18>(frame.r[3]),
        frame.r[3]
    );
    kprint!(
        "x4  {:cs}{:x} x5  {:cs}{:x} x6  {:cs}{:x} x7  {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[4]),
        frame.r[4],
        alt_hex_lead::<18>(frame.r[5]),
        frame.r[5],
        alt_hex_lead::<18>(frame.r[6]),
        frame.r[6],
        alt_hex_lead::<18>(frame.r[7]),
        frame.r[7]
    );
    kprint!(
        "x8  {:cs}{:x} x9  {:cs}{:x} x10 {:cs}{:x} x11 {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[8]),
        frame.r[8],
        alt_hex_lead::<18>(frame.r[9]),
        frame.r[9],
        alt_hex_lead::<18>(frame.r[10]),
        frame.r[10],
        alt_hex_lead::<18>(frame.r[11]),
        frame.r[11]
    );
    kprint!(
        "x12 {:cs}{:x} x13 {:cs}{:x} x14 {:cs}{:x} x15 {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[12]),
        frame.r[12],
        alt_hex_lead::<18>(frame.r[13]),
        frame.r[13],
        alt_hex_lead::<18>(frame.r[14]),
        frame.r[14],
        alt_hex_lead::<18>(frame.r[15]),
        frame.r[15]
    );
    kprint!(
        "x16 {:cs}{:x} x17 {:cs}{:x} x18 {:cs}{:x} x19 {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[16]),
        frame.r[16],
        alt_hex_lead::<18>(frame.r[17]),
        frame.r[17],
        alt_hex_lead::<18>(frame.r[18]),
        frame.r[18],
        alt_hex_lead::<18>(frame.r[19]),
        frame.r[19]
    );
    kprint!(
        "x20 {:cs}{:x} x21 {:cs}{:x} x22 {:cs}{:x} x23 {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[20]),
        frame.r[20],
        alt_hex_lead::<18>(frame.r[21]),
        frame.r[21],
        alt_hex_lead::<18>(frame.r[22]),
        frame.r[22],
        alt_hex_lead::<18>(frame.r[23]),
        frame.r[23]
    );
    kprint!(
        "x24 {:cs}{:x} x25 {:cs}{:x} x26 {:cs}{:x} x27 {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[24]),
        frame.r[24],
        alt_hex_lead::<18>(frame.r[25]),
        frame.r[25],
        alt_hex_lead::<18>(frame.r[26]),
        frame.r[26],
        alt_hex_lead::<18>(frame.r[27]),
        frame.r[27]
    );
    kprint!(
        "x28 {:cs}{:x} x29 {:cs}{:x} lr  {:cs}{:x} usp {:cs}{:x}\n",
        alt_hex_lead::<18>(frame.r[28]),
        frame.r[28],
        alt_hex_lead::<18>(frame.r[29]),
        frame.r[29],
        alt_hex_lead::<18>(frame.lr),
        frame.lr,
        alt_hex_lead::<18>(frame.usp),
        frame.usp
    );
    kprint!("elr  {:cs}{:x}\n", alt_hex_lead::<18>(frame.elr), frame.elr);
    kprint!("spsr {:cs}{:x}\n", alt_hex_lead::<18>(frame.spsr), frame.spsr);
}
