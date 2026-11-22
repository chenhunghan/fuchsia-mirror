// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 kernel debug console commands (`smmuv2 help`, `smmuv2 list`, `smmuv2 set`,
//! `smmuv2 show`, `smmuv2 dumpregs`, `smmuv2 lock`, and `smmuv2 isids`).
//!
//! Console command output is printed with `dprintf!(ALWAYS, ...)` so that it is never compiled out
//! by a reduced `kernel_debug_print_level`.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};
use debug::dprintf;
use fbl::RefPtr;
use zx_status::Status;

pub(in crate::arm_smmu) use crate::arm_smmu::common::mmio::dump_reg;
use crate::arm_smmu::common::{ArmSmmuMode, MappedPin, PAGE_SHIFT, ZX_MAX_NAME_LEN};
use crate::arm_smmu::v2::registers::gr0::{
    Cr0, Cr2, Idr0, Idr1, Idr2, Idr7, S2cr, S2crBypass, S2crFault, S2crTranslation, S2crType, Smr,
};
use crate::arm_smmu::v2::registers::gr1::{
    Cba2r, Cba2rVa64, Cbar, CbarS1TS2Bypass, CbarS1TS2Fault, CbarS1TS2Translate, CbarS2Translation,
    CbarType, Cbfrsynra,
};
use crate::arm_smmu::v2::registers::s1cbr::{
    Sctlr, Tcr, Tcr2, Tcr32Bit, Tcr64Bit, TcrExt32Bit, Ttbr0_32Bit, Ttbr0_64Bit, Ttbr0Ext32Bit,
    Ttbr1_32Bit, Ttbr1_64Bit, Ttbr1Ext32Bit,
};
use crate::arm_smmu::v2::smmu::{INSTANCES, SmmuV2, SmmuV2InstanceLock, SmmuV2LockGuard, TagBuf};
use crate::arm_smmu::v2::smmu_bti::SmmuV2Bti;
use crate::arm_smmu::v2::smmu_pmt::SmmuV2Pmt;
use crate::arm_smmu::v2::utils::{
    AddrMode, Bitmask128, BtiMode, SmrValue, addr_mode_to_string, arm_cbar_type_to_string,
    arm_s2cr_type_to_string, arm_smmu_mode_to_string, bti_mode_to_string,
};

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_SMMUV2,
    c"smmuv2".as_ptr(),
    c"SMMUv2 commands".as_ptr(),
    cmd_smmuv2,
    crate::console::CMD_AVAIL_NORMAL
);

/// Kernel console command entry point for `smmuv2`.
///
/// # Safety
///
/// If `argc > 0` and `!argv.is_null()`, `argv` must point to at least `argc` initialized
/// [`crate::console::CmdArgs`] elements within a single allocation that remain valid for reads and
/// unmutated for the duration of this call, and each element's `arg_str` must be either null or a
/// valid pointer to a NUL-terminated C string that likewise remains valid for reads and unmutated
/// for the duration of this call.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_smmuv2(
    argc: core::ffi::c_int,
    argv: *const crate::console::CmdArgs,
    flags: u32,
) -> core::ffi::c_int {
    let Ok(mut parsed_args) =
        kalloc::Box::try_new([ConsoleArg { str_val: "", u: 0 }; MAX_CONSOLE_ARGS])
    else {
        return Status::NO_MEMORY.into_raw();
    };
    let count =
        if argc <= 0 || argv.is_null() { 0usize } else { (argc as usize).min(parsed_args.len()) };
    if count > 0 {
        // SAFETY: Caller guarantees `argv` points to at least `argc` initialized `CmdArgs`
        // elements valid for reads for the duration of this call.
        let raw_args = unsafe { zr::slice_from_raw_parts(argv, count) };
        for (dst, src) in parsed_args[..count].iter_mut().zip(raw_args.iter()) {
            // `as_str()` yields an empty string for an argument which is not valid UTF-8. Such
            // an argument is rejected like any other unrecognized one, but the "Invalid command"
            // / "Invalid target" message shows `""` instead of the bytes which were typed.
            *dst = ConsoleArg { str_val: src.as_str(), u: src.arg_uint };
        }
    }
    SmmuV2::console_cmd(&parsed_args[..count], flags)
}

/// The most arguments [`SmmuV2::console_cmd`] takes, the command's own name included.
pub const MAX_CONSOLE_ARGS: usize = 16;

#[cfg(console_enabled)]
zr::static_assert!(MAX_CONSOLE_ARGS == crate::console::MAX_NUM_ARGS);

/// Parsed console argument passed to [`SmmuV2::console_cmd`].
#[derive(Clone, Copy, Debug)]
pub struct ConsoleArg<'a> {
    /// String token.
    pub str_val: &'a str,
    /// Unsigned integer value.
    pub u: u64,
}

static DEFAULT_SMMU_NDX: AtomicU32 = AtomicU32::new(0);

fn usage(ret: i32) -> i32 {
    dprintf!(ALWAYS, "Usage: smmuv2 <cmd>\n");
    dprintf!(ALWAYS, "Valid commands are:\n");
    dprintf!(ALWAYS, "  help     : show this message\n");
    dprintf!(
        ALWAYS,
        "  list     : list all discovered IOMMU instances and their currently managed BTIs.\n"
    );
    dprintf!(ALWAYS, "  set <id> : set the default SMMU to use with other commands.\n");
    dprintf!(ALWAYS, "  show <tgt> [-v] [-pmts]\n");
    dprintf!(ALWAYS, "    Print the current high level status of the selected SMMU.\n");
    dprintf!(ALWAYS, "    Use <tgt> to select a specific BTI to dump details about, or\n");
    dprintf!(ALWAYS, "    \"all\" to show the status for the entire smmu\n");
    dprintf!(ALWAYS, "  dumpregs\n");
    dprintf!(ALWAYS, "    Print raw register information for the selected SMMU\n");
    dprintf!(ALWAYS, "  lock <tgt>\n");
    dprintf!(ALWAYS, "    Attempt to set the operating mode of the target BTI(s) to FAULT.\n");
    dprintf!(ALWAYS, "  isids <tgt>\n");
    dprintf!(ALWAYS, "    Attempt to invalidate all Stream IDs of the target BTI(s).\n");
    dprintf!(ALWAYS, "\n");
    dprintf!(ALWAYS, "Params:\n");
    dprintf!(ALWAYS, "  <tgt>   : One of (all | bti <N> | sid <Stream ID>)\n");
    dprintf!(
        ALWAYS,
        "  <--id>  : The ID of the SMMU to operate on, instead of the current default.\n"
    );
    dprintf!(
        ALWAYS,
        "  [-v]    : Optionally enable verbose output, for sub-commands who support it.\n"
    );
    dprintf!(ALWAYS, "  [-pmts] : Show detailed PMT status when using the show command\n");
    ret
}

fn find_optional_flag(args: &[ConsoleArg<'_>], option: &str) -> bool {
    args.iter().any(|arg| arg.str_val == option)
}

/// Returns the value of a console argument which must be a number that fits in 32 bits.
///
/// The console parses every token as a number, giving zero for one which is not. A command
/// which takes a number must therefore check the token itself: otherwise `lock bti foo`, or an
/// option written where a number was expected, would act on item zero.
fn required_u32(arg: &ConsoleArg<'_>, what: &str) -> Result<u32, Status> {
    if !arg.str_val.starts_with(|c: char| c.is_ascii_digit()) {
        dprintf!(ALWAYS, "Expected a number for {}, found \"{}\"\n", what, arg.str_val);
        return Err(Status::INVALID_ARGS);
    }
    if arg.u > (u32::MAX as u64) {
        dprintf!(ALWAYS, "Value out of range for {} ({})!\n", what, arg.u);
        return Err(Status::OUT_OF_RANGE);
    }
    Ok(arg.u as u32)
}

fn find_optional_u32(args: &[ConsoleArg<'_>], option: &str) -> Result<Option<u32>, Status> {
    let mut ret: Option<u32> = None;
    let mut i = 0usize;
    while i < args.len() {
        if args[i].str_val == option {
            if ret.is_some() {
                dprintf!(ALWAYS, "Multiple \"{}\" options specified!\n", option);
                return Err(Status::ALREADY_EXISTS);
            }
            i += 1;
            if i >= args.len() {
                dprintf!(ALWAYS, "No value specified for option \"{}\"!\n", option);
                return Err(Status::INVALID_ARGS);
            }
            ret = Some(required_u32(&args[i], option)?);
        }
        i += 1;
    }
    Ok(ret)
}

impl SmmuV2 {
    /// Dumps raw register information for this SMMU (`smmuv2 dumpregs`).
    pub(crate) fn cmd_dump_regs(&self) {
        ksync::lock!(let guard = self.lock_lock());

        let gr0_base = self.gr0_base();
        let gr1_base = self.gr1_base();
        let st = self.init_state();

        dprintf!(ALWAYS, "Dumping configuration for {}\n", self.name());
        dprintf!(ALWAYS, "Version    : {}.{}\n", st.idr7.major(), st.idr7.minor());
        dprintf!(ALWAYS, "User-Bound : {}\n", if *guard.user_mode_bound() { "Yes" } else { "No" });

        let idr0 = Idr0::read_from(&gr0_base);
        dump_reg("IDR0", idr0.reg_value() as u64, 4, Idr0::OFFSET, Idr0::FIELDS, &gr0_base, 0);
        dump_reg(
            "IDR1",
            Idr1::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr1::OFFSET,
            Idr1::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "IDR2",
            Idr2::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr2::OFFSET,
            Idr2::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "IDR7",
            Idr7::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr7::OFFSET,
            Idr7::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "CR0",
            Cr0::read_from(&gr0_base).reg_value() as u64,
            4,
            Cr0::OFFSET,
            Cr0::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "CR2",
            Cr2::read_from(&gr0_base).reg_value() as u64,
            4,
            Cr2::OFFSET,
            Cr2::FIELDS,
            &gr0_base,
            0,
        );

        let mut tag = TagBuf::new();
        macro_rules! fmt_tag {
            ($($arg:tt)*) => {{
                tag.clear();
                let _ = core::write!(&mut tag, $($arg)*);
                tag.as_str()
            }};
        }

        // If this unit supports stream matching, dump the state of the available
        // stream matching register groups. If a stream matching group is in
        // translation mode, and has a valid context bank ID configured, make a note
        // of it so we can come back later and dump the status of the in-use context
        // banks.
        let mut in_use_cbs = Bitmask128::new();
        if idr0.sms() != 0 {
            for i in 0..idr0.numsmrg() {
                let smr = Smr::read_at(&gr0_base, i);
                if smr.valid() != 0 {
                    dump_reg(
                        fmt_tag!("SMR{}", i),
                        smr.reg_value() as u64,
                        4,
                        Smr::offset(i),
                        Smr::FIELDS,
                        &gr0_base,
                        0,
                    );
                    // The format of S2CR registers depends on the type value currently programmed
                    // into the register.
                    let s2cr = S2cr::read_at(&gr0_base, i);
                    let s2cr_tag =
                        fmt_tag!("S2CR{} ({})", i, arm_s2cr_type_to_string(s2cr.s2cr_type()));
                    match s2cr.s2cr_type() {
                        S2crType::Translation => {
                            let s2cr_trans = S2crTranslation::from_value(s2cr.reg_value());
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crTranslation::FIELDS,
                                &gr0_base,
                                0,
                            );
                            if s2cr_trans.cbndx() >= self.num_cbs() {
                                dprintf!(
                                    ALWAYS,
                                    "Invalid Context Bank ID {}\n",
                                    s2cr_trans.cbndx()
                                );
                            } else {
                                in_use_cbs.set_bit(s2cr_trans.cbndx());
                            }
                        }
                        S2crType::Bypass => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crBypass::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                        S2crType::Fault => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crFault::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                        S2crType::Invalid => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2cr::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                    }
                }
            }
        }

        // Now dump details about any in-use context banks.
        for i in 0..self.num_cbs() {
            if !in_use_cbs.test_bit(i) {
                continue;
            }

            dprintf!(ALWAYS, "Context Bank {}:\n", i);
            let cbar = Cbar::read_at(&gr1_base, i);
            let cbar_fields = match cbar.cbar_type() {
                CbarType::S2Translation => CbarS2Translation::FIELDS,
                CbarType::S1TS2Bypass => CbarS1TS2Bypass::FIELDS,
                CbarType::S1TS2Fault => CbarS1TS2Fault::FIELDS,
                CbarType::S1TS2Translate => CbarS1TS2Translate::FIELDS,
            };
            dump_reg(
                fmt_tag!("CBAR{} ({})", i, arm_cbar_type_to_string(cbar.cbar_type())),
                cbar.reg_value() as u64,
                4,
                Cbar::offset(i),
                cbar_fields,
                &gr1_base,
                0,
            );

            let cba2r = Cba2r::read_at(&gr1_base, i);
            dump_reg(
                fmt_tag!("CBA2R{}", i),
                cba2r.reg_value() as u64,
                4,
                Cba2r::offset(i),
                Cba2r::FIELDS,
                &gr1_base,
                0,
            );

            let cbfrsynra = Cbfrsynra::read_at(&gr1_base, i);
            dump_reg(
                fmt_tag!("CBFRSYNRA{}", i),
                cbfrsynra.reg_value() as u64,
                4,
                Cbfrsynra::offset(i),
                Cbfrsynra::FIELDS,
                &gr1_base,
                0,
            );

            let cb_base = self.get_cb_base(i);
            if cbar.cbar_type() != CbarType::S2Translation {
                dump_reg(
                    fmt_tag!("SCTLR({})", i),
                    Sctlr::read_from(&cb_base).reg_value() as u64,
                    4,
                    Sctlr::OFFSET,
                    Sctlr::FIELDS,
                    &cb_base,
                    0,
                );
                let tcr = Tcr::read_from(&cb_base);
                let addr_mode = if cba2r.va64() == Cba2rVa64::Va64 {
                    AddrMode::Mode64Bit
                } else if tcr.eae() != 0 {
                    AddrMode::Ext32Bit
                } else {
                    AddrMode::Mode32Bit
                };
                match addr_mode {
                    AddrMode::Mode32Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 0, EAE == 0)", i),
                            tcr.reg_value() as u64,
                            4,
                            Tcr32Bit::OFFSET,
                            Tcr32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr0_32Bit::OFFSET,
                            Ttbr0_32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr1_32Bit::OFFSET,
                            Ttbr1_32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Ext32Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 0, EAE == 1)", i),
                            tcr.reg_value() as u64,
                            4,
                            TcrExt32Bit::OFFSET,
                            TcrExt32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0Ext32Bit::OFFSET,
                            Ttbr0Ext32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1Ext32Bit::OFFSET,
                            Ttbr1Ext32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Mode64Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 1)", i),
                            tcr.reg_value() as u64,
                            4,
                            Tcr64Bit::OFFSET,
                            Tcr64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0_64Bit::OFFSET,
                            Ttbr0_64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1_64Bit::OFFSET,
                            Ttbr1_64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Invalid => {
                        dump_reg(
                            fmt_tag!("TCR({}) (Bad AddrMode {})", i, addr_mode as u32),
                            tcr.reg_value() as u64,
                            4,
                            Tcr::OFFSET,
                            Tcr::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                }
            }
        }
    }

    fn cmd_show_all_locked(&self, guard: &SmmuV2LockGuard<'_>, show_pmts: bool, verbose: bool) {
        let st = self.init_state();

        dprintf!(ALWAYS, "Dumping configuration for {}\n", self.name());
        dprintf!(ALWAYS, "Version    : {}.{}\n", st.idr7.major(), st.idr7.minor());
        dprintf!(ALWAYS, "User-Bound : {}\n", if *guard.user_mode_bound() { "Yes" } else { "No" });
        dprintf!(ALWAYS, "OpMode     : {}\n", arm_smmu_mode_to_string(st.op_mode));
        dprintf!(ALWAYS, "BTI Count  : {}\n", guard.bti_list().len());

        for (i, bti) in guard.bti_list().iter().enumerate() {
            let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
            bti.get_name(&mut bti_name_buf);
            let bti_name = SmmuV2Bti::name_display(&bti_name_buf);
            dprintf!(ALWAYS, "\nBTI #{} (\"{}\")\n", i, bti_name);
            bti.cmd_show(1, show_pmts, verbose);
        }
    }

    fn find_cmd_target_locked(
        &self,
        guard: &SmmuV2LockGuard<'_>,
        args: &[ConsoleArg<'_>],
        out_ndx: Option<&mut u32>,
    ) -> Result<Option<RefPtr<SmmuV2Bti>>, Status> {
        let mut local_ndx = 0u32;
        let ndx_ref = match out_ndx {
            Some(r) => {
                *r = 0;
                r
            }
            None => &mut local_ndx,
        };

        let Some(first) = args.first() else {
            dprintf!(ALWAYS, "No target specified.  One of (all|bti <N>|sid <Id>) is required.\n");
            return Err(Status::INVALID_ARGS);
        };

        let tgt_type = first.str_val;
        if tgt_type == "all" {
            return Ok(None);
        }

        let is_bti = tgt_type == "bti";
        if !is_bti && tgt_type != "sid" {
            dprintf!(ALWAYS, "Invalid target \"{}\"\n", tgt_type);
            return Err(Status::INVALID_ARGS);
        }

        if args.len() < 2 {
            dprintf!(ALWAYS, "Insufficient arguments for \"{}\" target\n", tgt_type);
            return Err(Status::INVALID_ARGS);
        }

        let tgt_val = required_u32(&args[1], tgt_type)?;
        if !is_bti && (tgt_val & !self.valid_stream_id_mask()) != 0 {
            dprintf!(ALWAYS, "Invalid SID {:#x}\n", tgt_val);
            return Err(Status::INVALID_ARGS);
        }

        for bti in guard.bti_list().iter() {
            if is_bti {
                if *ndx_ref == tgt_val {
                    return Ok(Some(RefPtr::from_ref(bti)));
                }
            } else if bti.smr_intersects(SmrValue::from_raw(tgt_val, self.valid_stream_id_mask())) {
                return Ok(Some(RefPtr::from_ref(bti)));
            }
            *ndx_ref += 1;
        }

        dprintf!(ALWAYS, "Failed to find {} target {:#x} in {}\n", tgt_type, tgt_val, self.name());
        Err(Status::NOT_FOUND)
    }

    fn cmd_show(&self, args: &[ConsoleArg<'_>], cmd_ndx: usize) -> i32 {
        ksync::lock!(let guard = self.lock_lock());

        let arg_ndx = cmd_ndx + 1;
        debug_assert!(arg_ndx <= args.len());

        let tgt = match self.find_cmd_target_locked(&guard, &args[arg_ndx..], None) {
            Ok(t) => t,
            Err(status) => return status.into_raw(),
        };

        let verbose = find_optional_flag(args, "-v");
        let show_pmts = find_optional_flag(args, "-pmts");
        if let Some(tgt) = tgt {
            tgt.cmd_show(0, show_pmts, verbose);
        } else {
            self.cmd_show_all_locked(&guard, show_pmts, verbose);
        }

        0
    }

    fn cmd_bti_op<F>(&self, args: &[ConsoleArg<'_>], cmd_ndx: usize, mut op: F) -> i32
    where
        F: FnMut(&SmmuV2, &SmmuV2Bti, u32),
    {
        ksync::lock!(let guard = self.lock_lock());

        debug_assert!(cmd_ndx < args.len());
        let tgt_ndx = cmd_ndx + 1;

        if tgt_ndx >= args.len() {
            dprintf!(ALWAYS, "Insufficient arguments for \"{}\" command\n", args[cmd_ndx].str_val);
            return usage(Status::INVALID_ARGS.into_raw());
        }

        let mut found_ndx = 0u32;
        let bti_tgt =
            match self.find_cmd_target_locked(&guard, &args[tgt_ndx..], Some(&mut found_ndx)) {
                Ok(t) => t,
                Err(status) => return status.into_raw(),
            };

        if let Some(bti) = bti_tgt {
            op(self, &bti, found_ndx);
        } else {
            for (ndx, bti) in guard.bti_list().iter().enumerate() {
                op(self, bti, ndx as u32);
            }
        }

        0
    }

    /// Dispatches the `smmu` kernel console command.
    pub fn console_cmd(args: &[ConsoleArg<'_>], _flags: u32) -> i32 {
        // We need at least two arguments to have a valid command.
        if args.len() < 2 {
            dprintf!(ALWAYS, "Bad argument count ({}).\n\n", args.len());
            return usage(Status::INVALID_ARGS.into_raw());
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Cmd {
            Invalid,
            Help,
            List,
            SetDefault,
            Show,
            DumpRegs,
            Lock,
            InvalidateSids,
        }

        const CMD_MAP: &[(&str, Cmd)] = &[
            ("help", Cmd::Help),
            ("list", Cmd::List),
            ("set", Cmd::SetDefault),
            ("show", Cmd::Show),
            ("dumpregs", Cmd::DumpRegs),
            ("lock", Cmd::Lock),
            ("isids", Cmd::InvalidateSids),
        ];

        // Find command in the argument list, and remember its index so that the
        // command handler can parse its positional arguments. The command is the
        // first entry in the argument list which does not start with a "-".
        let mut cmd_ndx = 0usize;
        let mut cmd = Cmd::Invalid;
        for (i, arg) in args.iter().enumerate().skip(1) {
            if !arg.str_val.starts_with('-') {
                cmd_ndx = i;
                for &(name, val) in CMD_MAP {
                    if arg.str_val == name {
                        cmd = val;
                    }
                }
                if cmd == Cmd::Invalid {
                    dprintf!(ALWAYS, "Invalid command \"{}\"\n", arg.str_val);
                    return usage(Status::INVALID_ARGS.into_raw());
                }
                break;
            }
        }

        if cmd_ndx == 0 {
            dprintf!(ALWAYS, "Missing command!\n");
            return usage(Status::INVALID_ARGS.into_raw());
        }

        // If the command is anything but "help" or "list", then we are going to need
        // to find an SMMU to operate on.
        let mut chosen_smmu: Option<RefPtr<SmmuV2>> = None;
        if cmd != Cmd::Help && cmd != Cmd::List {
            let maybe_id = match find_optional_u32(args, "--id") {
                Ok(id) => id,
                Err(status) => return usage(status.into_raw()),
            };

            // If we are attempting to set a new default, then use a value explicitly
            // passed via the `--id` option, if provided, or expect a positional
            // argument otherwise.
            let tgt_ndx = if cmd == Cmd::SetDefault {
                if let Some(id) = maybe_id {
                    id
                } else {
                    let id_ndx = cmd_ndx + 1;
                    if id_ndx >= args.len() {
                        dprintf!(ALWAYS, "No default SMMU ID specified.\n");
                        return usage(Status::INVALID_ARGS.into_raw());
                    }
                    match required_u32(&args[id_ndx], "the default SMMU ID") {
                        Ok(id) => id,
                        Err(status) => return usage(status.into_raw()),
                    }
                }
            } else {
                // `Relaxed` suffices: `DEFAULT_SMMU_NDX` is an advisory index hint for interactive
                // console commands and does not synchronize any other state (`SmmuV2InstanceLock`
                // guards `INSTANCES` lookup below).
                maybe_id.unwrap_or_else(|| DEFAULT_SMMU_NDX.load(Ordering::Relaxed))
            };

            // Lock and try to find the target SMMU. Error out if we cannot.
            {
                ksync::lock!(let mut instance_guard = SmmuV2InstanceLock::lock());
                let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
                for (ndx, smmu) in list.iter().enumerate() {
                    if (ndx as u32) == tgt_ndx {
                        chosen_smmu = Some(RefPtr::from_ref(smmu));
                    }
                }
            }

            let Some(ref smmu) = chosen_smmu else {
                dprintf!(ALWAYS, "Failed to find SMMU #{}\n", tgt_ndx);
                return usage(Status::INVALID_ARGS.into_raw());
            };

            // We found our SMMU. If we are setting the default, update the global
            // default value and get out. Otherwise, continue processing with our
            // chosen SMMU.
            if cmd == Cmd::SetDefault {
                dprintf!(ALWAYS, "Default SMMU is now \"{}\".\n", smmu.name());
                // `Relaxed` suffices: `DEFAULT_SMMU_NDX` is a standalone console index hint and
                // does not publish or synchronize any other memory.
                DEFAULT_SMMU_NDX.store(tgt_ndx, Ordering::Relaxed);
                return 0;
            }
        }

        // Dispatch the command.
        match cmd {
            Cmd::Help => usage(0),
            Cmd::List => {
                ksync::lock!(let mut instance_guard = SmmuV2InstanceLock::lock());
                let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
                dprintf!(ALWAYS, "Discovered {} SMMU instances\n", list.len());
                for (id, smmu) in list.iter().enumerate() {
                    ksync::lock!(let guard = smmu.lock_lock());
                    dprintf!(
                        ALWAYS,
                        "[{}] - {} (mode {}, user-bound : {})\n",
                        id,
                        smmu.name(),
                        arm_smmu_mode_to_string(smmu.op_mode()),
                        if *guard.user_mode_bound() { "Yes" } else { "No" }
                    );

                    for (bti_ndx, bti) in guard.bti_list().iter().enumerate() {
                        let mut sid_buf = [0u8; 128];
                        let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
                        bti.get_name(&mut bti_name_buf);
                        let bti_name = SmmuV2Bti::name_display(&bti_name_buf);

                        ksync::lock!(let bti_pmt_guard = bti.lock_pmt_lock());
                        ksync::lock!(let bti_guard = bti.lock_lock());
                        let sid_str = SmmuV2Bti::render_sid_list_locked(&bti_guard, &mut sid_buf);
                        dprintf!(
                            ALWAYS,
                            "    BTI[{}] - Mode {}{} StreamIDs [{}] Name \"{}\"\n",
                            bti_ndx,
                            bti_mode_to_string(*bti_guard.mode()),
                            if *bti_pmt_guard.orphaned() { " (orphaned)" } else { "" },
                            sid_str,
                            bti_name
                        );
                    }
                }
                0
            }
            Cmd::SetDefault => {
                // Set default should have been handled earlier.
                usage(Status::INVALID_ARGS.into_raw())
            }
            Cmd::Show => {
                // "show" takes an optional BTI ID or target selector.
                let smmu = chosen_smmu.expect("chosen_smmu must be set for Show");
                smmu.cmd_show(args, cmd_ndx)
            }
            Cmd::DumpRegs => {
                // "dumpregs" takes no extra arguments.
                let smmu = chosen_smmu.expect("chosen_smmu must be set for DumpRegs");
                smmu.cmd_dump_regs();
                0
            }
            Cmd::Lock => {
                let smmu = chosen_smmu.expect("chosen_smmu must be set for Lock");
                smmu.cmd_bti_op(args, cmd_ndx, |smmu, bti, ndx| {
                    bti.cmd_lock(smmu, ndx);
                })
            }
            Cmd::InvalidateSids => {
                let smmu = chosen_smmu.expect("chosen_smmu must be set for InvalidateSids");
                smmu.cmd_bti_op(args, cmd_ndx, |smmu, bti, ndx| {
                    let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
                    bti.get_name(&mut bti_name_buf);
                    let bti_name = SmmuV2Bti::name_display(&bti_name_buf);
                    match bti.invalidate_sids() {
                        Ok(()) => {
                            dprintf!(
                                ALWAYS,
                                "{}: Stream IDs for BTI index #{} (\"{}\") have been \
                                 invalidated.\n",
                                smmu.name(),
                                ndx,
                                bti_name
                            );
                        }
                        Err(status) => {
                            dprintf!(
                                ALWAYS,
                                "{}: Failed to invalidate Stream IDs for BTI index #{} (\"{}\") \
                                 (err {}).\n",
                                smmu.name(),
                                ndx,
                                bti_name,
                                status.into_raw()
                            );
                        }
                    }
                })
            }
            Cmd::Invalid => {
                // It should be impossible to have anything but a valid command at this
                // point.
                unreachable!()
            }
        }
    }
}

impl SmmuV2Bti {
    /// Prints diagnostic details of this BTI for `smmuv2 show`.
    fn cmd_show(&self, ilvl: usize, show_pmts: bool, verbose: bool) {
        ksync::lock!(let pmt_guard = self.lock_pmt_lock());
        ksync::lock!(let guard = self.lock_lock());

        let indent0 = ilvl * 2;
        let indent1 = (ilvl + 1) * 2;
        let indent2 = (ilvl + 2) * 2;

        dprintf!(
            ALWAYS,
            "{:indent0$}Mode              : {}{}\n",
            "",
            bti_mode_to_string(*guard.mode()),
            if *pmt_guard.orphaned() { " (orphaned)" } else { "" },
            indent0 = indent0
        );
        let bti_name = Self::name_display(guard.name());
        dprintf!(ALWAYS, "{:indent0$}Name              : {}\n", "", bti_name, indent0 = indent0);

        let mut sid_buf = [0u8; 128];
        let sid_str = Self::render_sid_list_locked(&guard, &mut sid_buf);
        dprintf!(ALWAYS, "{:indent0$}StreamIDs         : {}\n", "", sid_str, indent0 = indent0);
        dprintf!(
            ALWAYS,
            "{:indent0$}Active PMTs       : {}\n",
            "",
            pmt_guard.active_pmt_list().len(),
            indent0 = indent0
        );
        dprintf!(
            ALWAYS,
            "{:indent0$}Quarantined PMTs  : {}\n",
            "",
            *pmt_guard.quarantined_pmt_count(),
            indent0 = indent0
        );
        dprintf!(
            ALWAYS,
            "{:indent0$}Quarantined Pages : {}\n",
            "",
            *pmt_guard.quarantined_page_count(),
            indent0 = indent0
        );
        if let Some(ref aspace) = *pmt_guard.aspace() {
            dprintf!(
                ALWAYS,
                "{:indent0$}TLB Pages         : {}\n",
                "",
                aspace.page_cache().in_flight_pages(),
                indent0 = indent0
            );
            dprintf!(
                ALWAYS,
                "{:indent0$}Cached TLB Pages  : {}\n",
                "",
                aspace.page_cache().cache_entries(),
                indent0 = indent0
            );
        }

        if show_pmts && *guard.mode() == BtiMode::Translation {
            let mut page_total = 0u64;
            let token = pmt_guard.token();

            for (ndx, pmt) in pmt_guard.active_pmt_list().iter().enumerate() {
                // SAFETY: `pmt` is on this BTI's active list, so `self` owns it, and `pmt_guard`
                // holds `self.pmt_lock`.
                if let Some(loc) = unsafe { pmt.guard_pmt_lock(token) }.pin().mapping() {
                    let base = loc.base();
                    let size = loc.size();
                    let page_size = size >> PAGE_SHIFT;
                    dprintf!(
                        ALWAYS,
                        "{:indent1$}PMT[{:3}] : [{:#x}, {:#x}] ({} Page{})\n",
                        "",
                        ndx,
                        base,
                        base + size - 1,
                        page_size,
                        if page_size == 1 { "" } else { "s" },
                        indent1 = indent1
                    );
                    page_total += page_size;
                } else {
                    // If we have an active PMT in a BTI operating in translation mode, we
                    // may not have a mapping (`pin().mapping()`) any more because someone used the
                    // debug console to force-drop our mapping with the `lock` command.
                    dprintf!(ALWAYS, "{:indent1$}PMT[{:3}] : ???\n", "", ndx, indent1 = indent1);
                }
            }
            dprintf!(
                ALWAYS,
                "{:indent1$}Counted {} mapped page{}.\n",
                "",
                page_total,
                if page_total == 1 { "" } else { "s" },
                indent1 = indent1
            );
        }

        let smrg_count = guard.smrg_list().len();
        let mut smrg_num = 0u32;
        for smrg in guard.smrg_list().iter() {
            smrg_num += 1;
            dprintf!(
                ALWAYS,
                "\n{:indent0$}SMRG {}/{} (index {})\n",
                "",
                smrg_num,
                smrg_count,
                smrg.smrg_ndx(),
                indent0 = indent0
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}Mode  : {}\n",
                "",
                arm_s2cr_type_to_string(smrg.mode()),
                indent1 = indent1
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}Mask  : {:#06x}\n",
                "",
                smrg.stream_ids().mask(),
                indent1 = indent1
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}ID    : {:#06x}\n",
                "",
                smrg.stream_ids().id(),
                indent1 = indent1
            );
            dprintf!(ALWAYS, "{:indent1$}CBNdx : {:#06x}\n", "", smrg.cb_ndx(), indent1 = indent1);

            if verbose {
                let gr0_base = smrg.gr0_base();
                let ndx = smrg.smrg_ndx();
                let smr_reg = Smr::read_at(&gr0_base, ndx);
                dump_reg(
                    "SMR",
                    smr_reg.reg_value() as u64,
                    4,
                    Smr::offset(ndx),
                    Smr::FIELDS,
                    &gr0_base,
                    ilvl + 2,
                );
                let s2cr_reg = S2cr::read_at(&gr0_base, ndx);
                let mut s2cr_tag = TagBuf::new();
                let _ =
                    core::write!(&mut s2cr_tag, "S2CR ({})", arm_s2cr_type_to_string(smrg.mode()));
                let fields = match smrg.mode() {
                    S2crType::Translation => S2crTranslation::FIELDS,
                    S2crType::Bypass => S2crBypass::FIELDS,
                    S2crType::Fault => S2crFault::FIELDS,
                    S2crType::Invalid => S2cr::FIELDS,
                };
                dump_reg(
                    s2cr_tag.as_str(),
                    s2cr_reg.reg_value() as u64,
                    4,
                    S2cr::offset(ndx),
                    fields,
                    &gr0_base,
                    ilvl + 2,
                );
            }
        }

        if let Some(ref cb) = *guard.context_bank() {
            let gr1_base = cb.gr1_base();
            let cb_base = cb.cb_base();
            let cb_ndx = cb.cb_ndx();
            let cbar = Cbar::read_at(&gr1_base, cb_ndx);
            let sctlr = Sctlr::read_from(&cb_base);
            let irq_num =
                guard.smmu().as_ref().and_then(|s| s.get_context_irq(cb_ndx)).map_or(0, |d| d.num);

            dprintf!(
                ALWAYS,
                "\n{:indent0$}Context Bank (index {})\n",
                "",
                cb_ndx,
                indent0 = indent0
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}Mode      : {}\n",
                "",
                bti_mode_to_string(cb.mode()),
                indent1 = indent1
            );
            dprintf!(ALWAYS, "{:indent1$}IRQ       : {}\n", "", irq_num, indent1 = indent1);
            dprintf!(
                ALWAYS,
                "{:indent1$}AddrMode  : {}\n",
                "",
                addr_mode_to_string(cb.addr_mode()),
                indent1 = indent1
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}CBAR Type : {}\n",
                "",
                arm_cbar_type_to_string(cbar.cbar_type()),
                indent1 = indent1
            );
            dprintf!(
                ALWAYS,
                "{:indent1$}MMU State : {}\n",
                "",
                if sctlr.m() != 0 { "Enabled" } else { "Disabled" },
                indent1 = indent1
            );

            for (i, ttbr) in cb.ttbrs().iter().enumerate() {
                dprintf!(
                    ALWAYS,
                    "{:indent1$}TTBR[{}]   : {}\n",
                    "",
                    i,
                    if ttbr.enabled { "Enabled" } else { "Disabled" },
                    indent1 = indent1
                );
                if ttbr.enabled {
                    dprintf!(
                        ALWAYS,
                        "{:indent2$}Granule Size : {}\n",
                        "",
                        ttbr.granule_size_bits,
                        indent2 = indent2
                    );
                    dprintf!(
                        ALWAYS,
                        "{:indent2$}First Valid  : {:#018x}\n",
                        "",
                        ttbr.first_valid_addr,
                        indent2 = indent2
                    );
                    dprintf!(
                        ALWAYS,
                        "{:indent2$}Last Valid   : {:#018x}\n",
                        "",
                        ttbr.last_valid_addr,
                        indent2 = indent2
                    );
                    dprintf!(
                        ALWAYS,
                        "{:indent2$}Phys Addr    : {:#018x}\n",
                        "",
                        ttbr.ttbr_paddr,
                        indent2 = indent2
                    );
                }

                // TODO(johngro): If we are in translation mode, walk the translation table
                // and print the active mappings.
            }

            if verbose {
                let (cbar_tag, cbar_fields) = match cbar.cbar_type() {
                    CbarType::S2Translation => ("CBAR (S2Translate)", CbarS2Translation::FIELDS),
                    CbarType::S1TS2Bypass => ("CBAR (S1TS2Bypass)", CbarS1TS2Bypass::FIELDS),
                    CbarType::S1TS2Fault => ("CBAR (S1TS2Fault)", CbarS1TS2Fault::FIELDS),
                    CbarType::S1TS2Translate => {
                        ("CBAR (S1TS2Translate)", CbarS1TS2Translate::FIELDS)
                    }
                };
                dump_reg(
                    cbar_tag,
                    cbar.reg_value() as u64,
                    4,
                    Cbar::offset(cb_ndx),
                    cbar_fields,
                    &gr1_base,
                    ilvl + 2,
                );
                let cba2r = Cba2r::read_at(&gr1_base, cb_ndx);
                dump_reg(
                    "CBA2R",
                    cba2r.reg_value() as u64,
                    4,
                    Cba2r::offset(cb_ndx),
                    Cba2r::FIELDS,
                    &gr1_base,
                    ilvl + 2,
                );
                dump_reg(
                    "SCTLR",
                    sctlr.reg_value() as u64,
                    4,
                    Sctlr::OFFSET,
                    Sctlr::FIELDS,
                    &cb_base,
                    ilvl + 2,
                );
                match cb.addr_mode() {
                    AddrMode::Mode32Bit => {
                        dump_reg(
                            "TTBR0 (32)",
                            Ttbr0_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr0_32Bit::OFFSET,
                            Ttbr0_32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (32)",
                            Ttbr1_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr1_32Bit::OFFSET,
                            Ttbr1_32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (32)",
                            Tcr32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr32Bit::OFFSET,
                            Tcr32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Ext32Bit => {
                        dump_reg(
                            "TTBR0 (E32)",
                            Ttbr0Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0Ext32Bit::OFFSET,
                            Ttbr0Ext32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (E32)",
                            Ttbr1Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1Ext32Bit::OFFSET,
                            Ttbr1Ext32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (E32)",
                            TcrExt32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            TcrExt32Bit::OFFSET,
                            TcrExt32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Mode64Bit => {
                        dump_reg(
                            "TTBR0 (64)",
                            Ttbr0_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0_64Bit::OFFSET,
                            Ttbr0_64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (64)",
                            Ttbr1_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1_64Bit::OFFSET,
                            Ttbr1_64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (64)",
                            Tcr64Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr64Bit::OFFSET,
                            Tcr64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Invalid => {
                        dump_reg(
                            "TCR",
                            Tcr::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr::OFFSET,
                            Tcr::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                }
                dump_reg(
                    "TCR2",
                    Tcr2::read_from(&cb_base).reg_value() as u64,
                    4,
                    Tcr2::OFFSET,
                    Tcr2::FIELDS,
                    &cb_base,
                    ilvl + 2,
                );
            }
        }
    }

    /// Locks down this BTI for the debug console `smmuv2 lock` command.
    fn cmd_lock(&self, smmu: &SmmuV2, ndx: u32) {
        let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
        self.get_name(&mut bti_name_buf);
        let bti_name = Self::name_display(&bti_name_buf);

        // We are going to do one of two things, depending on whether we are in
        // Enforced mode, or in Passthru mode. If we are in enforced mode, we are
        // going to simply go through our list of active PMTs, and remove all of
        // their mappings from the page tables. This does not actually "lock" the
        // BTI, it just makes existing pinned memory inaccessible (assuming that
        // there is any).
        //
        // When we are in passthru mode, we just force the BTI (and its associated
        // context bank) into Fault mode, meaning that the MMU will be enabled, but
        // the TTBRs will be disabled, causing every translation request to fail
        // instead of passing through.
        if smmu.op_mode() == ArmSmmuMode::Enforced {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            // Go through this BTI's PMTs and release any mappings back to the BTI's
            // address space. Don't actually release the pinned memory yet; we are
            // only trying to force some faults.
            let mut released = 0usize;
            let mut maybe_pmt_ptr: Option<*const SmmuV2Pmt> =
                pmt_guard.active_pmt_list().front().map(|p| p as *const SmmuV2Pmt);
            while let Some(pmt_ptr) = maybe_pmt_ptr {
                // In general, we should be able to assert that all active PMTs have a
                // mapping during normal operation. However, it is possible that someone
                // has run this command twice and the mappings were already released. To
                // keep our reporting consistent, make sure to only count mappings we
                // actually released.
                let (pin, next_ptr) = {
                    // SAFETY: `pmt_ptr` points to an active PMT in `active_pmt_list`, and
                    // `pmt_lock` is held across the dereference (and `release_mapping_locked`
                    // does not unlink nodes from `active_pmt_list`).
                    let pmt = unsafe { &*pmt_ptr };
                    let next_ptr = {
                        // SAFETY: `active_pmt_list` is not moved, and `pmt` is an element of
                        // `active_pmt_list`.
                        let mut cursor =
                            unsafe { pmt_guard.as_mut().active_pmt_list_mut().cursor_at(pmt) };
                        cursor.move_next();
                        cursor.get().map(|next| next as *const SmmuV2Pmt)
                    };
                    let token = pmt_guard.as_mut().token_mut();
                    // SAFETY: `pmt` is on this BTI's active list, so `self` owns it, and
                    // `pmt_guard` holds `self.pmt_lock`.
                    let pin = unsafe { pmt.guard_pmt_lock_mut(token) }.pin_mut().take();
                    (pin, next_ptr)
                };
                if pin.has_mapping() {
                    released += 1;
                }
                // The mapping is removed and the pin is kept, now without one.
                let unmapped =
                    pin.unmap(|mapping| self.release_mapping_locked(&mut pmt_guard, Some(mapping)));
                // SAFETY: As above: `pmt_lock` is still held, and releasing the mapping did not
                // unlink the PMT.
                let pmt = unsafe { &*pmt_ptr };
                // SAFETY: `pmt` is on this BTI's active list, so `self` owns it, and `pmt_guard`
                // holds `self.pmt_lock`.
                *unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }.pin_mut() =
                    MappedPin::new(unmapped.into_vmo(), None);
                maybe_pmt_ptr = next_ptr;
            }
            dprintf!(
                ALWAYS,
                "{}: BTI index #{} (\"{}\") released {} active mapping{}.\n",
                smmu.name(),
                ndx,
                bti_name,
                released,
                if released == 1 { "" } else { "s" }
            );
        } else {
            ksync::lock!(let mut guard = self.lock_lock());
            // If the SMMU is not operating in Enforced mode, it must be in passthru
            // mode (if it was in disabled mode, there would be no constructed `SmmuV2Bti`
            // objects).
            debug_assert_eq!(smmu.op_mode(), ArmSmmuMode::Passthru);

            // Force the BTI into fault mode. This will lock things down if we
            // are in passthru mode, and prevent new PMTs from being created moving
            // forward.
            let res = self.set_mode_locked(&mut guard, BtiMode::Fault);
            match res {
                Ok(()) => {
                    // We are forcing our BTI into fault mode from the debug console as opposed
                    // to in reaction to an interrupt. Re-enable the context fault interrupt,
                    // so that the console user can see a fault exception during the next access
                    // in response to the lockdown command issued from the console.
                    if let Some(ref mut cb) = *guard.as_mut().context_bank_mut() {
                        cb.enable_fault_interrupt();
                    }
                    dprintf!(
                        ALWAYS,
                        "{}: BTI index #{} (\"{}\") set to mode FAULT.\n",
                        smmu.name(),
                        ndx,
                        bti_name
                    );
                }
                Err(status) => {
                    dprintf!(
                        ALWAYS,
                        "{}: Failed to set BTI index #{} (\"{}\") to mode FAULT (err {}).\n",
                        smmu.name(),
                        ndx,
                        bti_name,
                        status.into_raw()
                    );
                }
            }
        }
    }
}
