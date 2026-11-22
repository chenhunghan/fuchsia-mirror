// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    AtomicOp, TARGET_PC_EXIT, resolve_jump_target, resolve_lddw, resolve_patched_target_loc,
};
use crate::{
    BPF_STACK_SIZE, DataWidth, EbpfInstruction, HelperLinkage, JitError, VerifierErrorKind,
};
use ebpf_core::visitor::{BpfVisitor, Register as BpfReg, Source};

pub const IS_SUPPORTED: bool = true;

// ABI for JIT-compiled eBPF programs (`crate::jit::JitFn`):
// `run_context` is passed in `X0` and preserved in `X27` across the execution
// of the program. `arg1..arg5` are passed in `X1..X5` (corresponding directly
// to eBPF `R1..R5`). The return value (`R0`, mapped to `X6`) is moved to `X0`
// in the epilogue.

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Register {
    R0 = 0,
    R1 = 1,
    R2 = 2,
    R3 = 3,
    R4 = 4,
    R5 = 5,
    R6 = 6,
    R19 = 19,
    R20 = 20,
    R21 = 21,
    R22 = 22,
    R23 = 23,
    R24 = 24,
    R25 = 25,
    R26 = 26,
    R27 = 27,
    R28 = 28,
    R29 = 29,
    R30 = 30,
    R31 = 31,
}

const SP: Register = Register::R31;
const RZ: Register = Register::R31;

/// Callee-saved registers saved in the prologue and restored in the epilogue.
/// Must be an even number of registers so they can be paired with `STP`/`LDP`.
const CALLEE_SAVED_REGISTERS: [Register; 10] = [
    Register::R19,
    Register::R20,
    Register::R21,
    Register::R22,
    Register::R23,
    Register::R24,
    Register::R25,
    Register::R26,
    Register::R27,
    Register::R28,
];

/// Scratch register used for materializing immediates and holding atomic `old_val`.
const TEMP_REGISTER: Register = Register::R24;
/// Scratch register used for `MOD` division quotient and holding atomic `new_val`.
const TEMP_DIV_REGISTER: Register = Register::R25;
/// Scratch register used for large memory offsets and holding atomic target addresses.
const OFFSET_REGISTER: Register = Register::R26;
/// Callee-saved register holding the `run_context` pointer (`X0` on entry) across the program.
const RUN_CONTEXT_REGISTER: Register = Register::R27;
/// Scratch register used for LL/SC (`STLXR`) status flag.
const SCRATCH_REGISTER: Register = Register::R28;

/// Offset from `SP` of the 16-byte scratch slot used for `packet_load_fn`'s `out_value: *mut u64`.
/// Layout starting at `SP`:
/// - `[SP + 0 .. 16]`: `R29, R30`
/// - `[SP + 16 .. 96]`: `R19..R28` (10 registers = 80 bytes)
/// - `[SP + 96 .. 112]`: 16-byte scratch space (`[SP + 96]` = `u64` slot)
/// - `[SP + 112 .. 624]`: 512-byte (`BPF_STACK_SIZE`) eBPF stack (`R10` = `SP + 624`)
const PACKET_LOAD_OUT_OFFSET: u32 = 2 * 8 + (CALLEE_SAVED_REGISTERS.len() as u32) * 8;
const REGISTER_SPACE: u32 = PACKET_LOAD_OUT_OFFSET + 16;

/// Map eBPF register `0..=10` to AArch64 host register:
/// - eBPF `R0` (return value) -> `R6`
/// - eBPF `R1..R5` (arguments / caller-saved scratch) -> `R1..R5`
/// - eBPF `R6..R10` (callee-saved + frame pointer) -> `R19..R23`
fn map_register(r: BpfReg) -> Result<Register, VerifierErrorKind> {
    match r {
        0 => Ok(Register::R6),
        1 => Ok(Register::R1),
        2 => Ok(Register::R2),
        3 => Ok(Register::R3),
        4 => Ok(Register::R4),
        5 => Ok(Register::R5),
        6 => Ok(Register::R19),
        7 => Ok(Register::R20),
        8 => Ok(Register::R21),
        9 => Ok(Register::R22),
        10 => Ok(Register::R23),
        _ => Err(VerifierErrorKind::InvalidRegister(r)),
    }
}

#[derive(Copy, Clone, Debug)]
struct PatchableTarget {
    target_pc: u32,
    offset: usize,
}

struct JitState<'a> {
    code: &'a [EbpfInstruction],
    helpers: &'a [HelperLinkage],
    packet_load_fn: Option<u64>,
    pc: usize,
    buf: Vec<u32>,
    pc_locs: Vec<Option<u32>>,
    jumps: Vec<PatchableTarget>,
    exit_loc: u32,
}

impl<'a> JitState<'a> {
    fn new(
        code: &'a [EbpfInstruction],
        helpers: &'a [HelperLinkage],
        packet_load_fn: Option<u64>,
    ) -> Self {
        let num_insts = code.len();
        Self {
            code,
            helpers,
            packet_load_fn,
            pc: 0,
            buf: Vec::with_capacity(num_insts * 2 + 32),
            pc_locs: vec![None; num_insts],
            jumps: Vec::new(),
            exit_loc: 0,
        }
    }

    #[inline]
    fn offset(&self) -> usize {
        self.buf.len()
    }
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum AddSubOpcode {
    Add = 0,
    Sub = 2,
    Subs = 3,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum LogicalOpcode {
    And = 0,
    Orr = 1,
    Eor = 2,
    Ands = 3,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum MoveWideOpcode {
    Movn = 0,
    Movz = 2,
    Movk = 3,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum BitfieldOpcode {
    Sbfm = 0,
    Ubfm = 2,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum UnconditionalBranchOpcode {
    Blr = 0xd63f0000,
    Ret = 0xd65f0000,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum UnconditionalBranchImmediateOpcode {
    B = 0x14000000,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum Condition {
    Eq = 0x0,
    Ne = 0x1,
    Cs = 0x2,
    Cc = 0x3,
    Hi = 0x8,
    Ls = 0x9,
    Ge = 0xa,
    Lt = 0xb,
    Gt = 0xc,
    Le = 0xd,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum DataProc1SourceOpcode {
    Rev16 = 1,
    Rev32 = 2,
    Rev64 = 3,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum DataProc2SourceOpcode {
    Udiv = 2,
    Lslv = 8,
    Lsrv = 9,
    Asrv = 10,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum DataProc3SourceOpcode {
    Madd = 0,
    Msub = 1,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum LoadStoreImmediateOpcode {
    Strb = 0x38000000,
    Strh = 0x78000000,
    Strw = 0xb8000000,
    Strx = 0xf8000000,
    Ldrb = 0x38400000,
    Ldrh = 0x78400000,
    Ldrw = 0xb8400000,
    Ldrx = 0xf8400000,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum LoadStoreRegisterOpcode {
    Strb = 0x38206800,
    Strh = 0x78206800,
    Strw = 0xb8206800,
    Strx = 0xf8206800,
    Ldrb = 0x38606800,
    Ldrh = 0x78606800,
    Ldrw = 0xb8606800,
    Ldrx = 0xf8606800,
}

#[repr(u32)]
#[derive(Copy, Clone)]
enum LoadStorePairOpcode {
    Stpx = 0xa9000000,
    Ldpx = 0xa9400000,
}

fn emit_instruction(state: &mut JitState<'_>, inst: u32) {
    state.buf.push(inst);
}

fn emit_addsub_immediate(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: AddSubOpcode,
    rd: Register,
    rn: Register,
    imm12: u32,
) {
    const IMM_OP_BASE: u32 = 0x11000000;
    assert!(imm12 < 0x1000);
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | IMM_OP_BASE | ((op as u32) << 29) | (imm12 << 10) | ((rn as u32) << 5) | (rd as u32),
    );
}

fn emit_addsub_register(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: AddSubOpcode,
    rd: Register,
    rn: Register,
    rm: Register,
) {
    const REG_OP_BASE: u32 = 0x0b000000;
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | REG_OP_BASE
            | ((op as u32) << 29)
            | ((rm as u32) << 16)
            | ((rn as u32) << 5)
            | (rd as u32),
    );
}

fn emit_logical_register(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: LogicalOpcode,
    rd: Register,
    rn: Register,
    rm: Register,
) {
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | (1u32 << 27)
            | (1u32 << 25)
            | ((op as u32) << 29)
            | ((rm as u32) << 16)
            | ((rn as u32) << 5)
            | (rd as u32),
    );
}

fn emit_movewide_immediate_helper(
    state: &mut JitState<'_>,
    is_64_op: bool,
    rd: Register,
    hw: u32,
    imm16: u16,
    op: MoveWideOpcode,
) {
    const MW_OP_BASE: u32 = 0x12800000;
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | MW_OP_BASE | ((op as u32) << 29) | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32),
    );
}

fn emit_movewide_immediate(state: &mut JitState<'_>, is_64_op: bool, rd: Register, imm: u64) {
    let mut count0000 = 0;
    let mut countffff = 0;
    let num_halfwords = if is_64_op { 4 } else { 2 };
    for i in 0..num_halfwords {
        let imm16 = ((imm >> (i * 16)) & 0xffff) as u16;
        if imm16 == 0xffff {
            countffff += 1;
        } else if imm16 == 0x0000 {
            count0000 += 1;
        }
    }

    let mut invert = count0000 < countffff;
    let mut op = if invert { MoveWideOpcode::Movn } else { MoveWideOpcode::Movz };
    let skip_pattern = if invert { 0xffffu16 } else { 0x0000u16 };
    let mut emitted_any = false;
    for i in 0..num_halfwords {
        let imm16 = ((imm >> (i * 16)) & 0xffff) as u16;
        if imm16 != skip_pattern {
            emit_movewide_immediate_helper(
                state,
                is_64_op,
                rd,
                i,
                if invert { !imm16 } else { imm16 },
                op,
            );
            op = MoveWideOpcode::Movk;
            invert = false;
            emitted_any = true;
        }
    }

    if !emitted_any {
        emit_movewide_immediate_helper(state, is_64_op, rd, 0, 0, op);
    }
}

fn emit_bitfield(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: BitfieldOpcode,
    rd: Register,
    rn: Register,
    immr: u32,
    imms: u32,
) {
    const BF_OP_BASE: u32 = 0x13000000;
    let sf_and_n = if is_64_op { (1u32 << 31) | (1u32 << 22) } else { 0 };
    emit_instruction(
        state,
        sf_and_n
            | BF_OP_BASE
            | ((op as u32) << 29)
            | (immr << 16)
            | (imms << 10)
            | ((rn as u32) << 5)
            | (rd as u32),
    );
}

/// Emit `UXTH Wd, Wn` (`UBFM Wd, Wn, #0, #15`) to zero-extend a 16-bit value to 64 bits.
fn emit_uxth(state: &mut JitState<'_>, rd: Register, rn: Register) {
    emit_bitfield(state, false, BitfieldOpcode::Ubfm, rd, rn, 0, 15);
}

fn emit_unconditionalbranch_register(
    state: &mut JitState<'_>,
    op: UnconditionalBranchOpcode,
    rn: Register,
) {
    emit_instruction(state, (op as u32) | ((rn as u32) << 5));
}

fn note_jump(state: &mut JitState<'_>, target_pc: u32) {
    state.jumps.push(PatchableTarget { target_pc, offset: state.offset() });
}

fn emit_unconditionalbranch_immediate(
    state: &mut JitState<'_>,
    op: UnconditionalBranchImmediateOpcode,
    target_pc: u32,
) {
    note_jump(state, target_pc);
    emit_instruction(state, op as u32);
}

fn emit_conditionalbranch_immediate(state: &mut JitState<'_>, cond: Condition, target_pc: u32) {
    const CB_OP_BASE: u32 = 0x54000000;
    note_jump(state, target_pc);
    emit_instruction(state, CB_OP_BASE | (cond as u32));
}

fn emit_dataproc_1source(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: DataProc1SourceOpcode,
    rd: Register,
    rn: Register,
) {
    const DP1_OP_BASE: u32 = 0x5ac00000;
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | DP1_OP_BASE | ((op as u32) << 10) | ((rn as u32) << 5) | (rd as u32),
    );
}

fn emit_dataproc_2source(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: DataProc2SourceOpcode,
    rd: Register,
    rn: Register,
    rm: Register,
) {
    const DP2_OP_BASE: u32 = 0x1ac00000;
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | DP2_OP_BASE
            | ((rm as u32) << 16)
            | ((op as u32) << 10)
            | ((rn as u32) << 5)
            | (rd as u32),
    );
}

fn emit_dataproc_3source(
    state: &mut JitState<'_>,
    is_64_op: bool,
    op: DataProc3SourceOpcode,
    rd: Register,
    rn: Register,
    rm: Register,
    ra: Register,
) {
    const DP3_OP_BASE: u32 = 0x1b000000;
    let sf = if is_64_op { 1u32 << 31 } else { 0 };
    emit_instruction(
        state,
        sf | DP3_OP_BASE
            | ((rm as u32) << 16)
            | ((op as u32) << 15)
            | ((ra as u32) << 10)
            | ((rn as u32) << 5)
            | (rd as u32),
    );
}

fn emit_loadstore_immediate(
    state: &mut JitState<'_>,
    op: LoadStoreImmediateOpcode,
    rt: Register,
    rn: Register,
    imm9: i16,
) {
    assert!((-256..256).contains(&imm9));
    let imm = (imm9 as u32) & 0x1ff;
    emit_instruction(state, (op as u32) | (imm << 12) | ((rn as u32) << 5) | (rt as u32));
}

fn emit_loadstore_register(
    state: &mut JitState<'_>,
    op: LoadStoreRegisterOpcode,
    rt: Register,
    rn: Register,
    rm: Register,
) {
    emit_instruction(state, (op as u32) | ((rm as u32) << 16) | ((rn as u32) << 5) | (rt as u32));
}

fn emit_loadstorepair_immediate(
    state: &mut JitState<'_>,
    op: LoadStorePairOpcode,
    rt: Register,
    rt2: Register,
    rn: Register,
    imm7: i32,
) {
    let imm_div_8 = imm7 / 8;
    assert!(imm7 % 8 == 0 && (-64..64).contains(&imm_div_8));
    let imm = (imm_div_8 as u32) & 0x7f;
    emit_instruction(
        state,
        (op as u32) | (imm << 15) | ((rt2 as u32) << 10) | ((rn as u32) << 5) | (rt as u32),
    );
}

/// Emit Load-Acquire Exclusive Register (`LDAXR Wt, [Xn]` or `LDAXR Xt, [Xn]`).
fn emit_ldaxr(state: &mut JitState<'_>, is_64_op: bool, rt: Register, rn: Register) {
    let sz = if is_64_op { 0x40000000 } else { 0 };
    emit_instruction(state, 0x885ffc00 | sz | ((rn as u32) << 5) | (rt as u32));
}

/// Emit Store-Release Exclusive Register (`STLXR Ws, Wt, [Xn]` or `STLXR Ws, Xt, [Xn]`).
fn emit_stlxr(state: &mut JitState<'_>, is_64_op: bool, rs: Register, rt: Register, rn: Register) {
    let sz = if is_64_op { 0x40000000 } else { 0 };
    emit_instruction(
        state,
        0x8800fc00 | sz | ((rs as u32) << 16) | ((rn as u32) << 5) | (rt as u32),
    );
}

/// Emit Clear Exclusive (`CLREX`).
fn emit_clrex(state: &mut JitState<'_>) {
    emit_instruction(state, 0xd5033f5f);
}

/// Emit Compare and Branch on Non-Zero (`CBNZ Wt, label`).
fn emit_cbnz_w(state: &mut JitState<'_>, rt: Register, target_inst_offset: usize) {
    let rel_insts = (target_inst_offset as isize) - (state.offset() as isize);
    assert!((-(1 << 18)..(1 << 18)).contains(&rel_insts));
    let imm19 = (rel_insts as u32) & 0x7ffff;
    emit_instruction(state, 0x35000000 | (imm19 << 5) | (rt as u32));
}

fn update_branch_immediate(instr: &mut u32, offset: usize, imm_insts: i64) -> Result<(), JitError> {
    if (*instr & 0xfe000000) == 0x54000000 {
        // Conditional branch: 19-bit signed immediate at bits [23:5].
        if !(-(1i64 << 18)..(1i64 << 18)).contains(&imm_insts) {
            return Err(JitError::BranchDisplacementOutOfRange { offset, displacement: imm_insts });
        }
        *instr |= ((imm_insts as u32) & 0x7ffff) << 5;
    } else if (*instr & 0x7c000000) == 0x14000000 {
        // Unconditional branch: 26-bit signed immediate at bits [25:0].
        if !(-(1i64 << 25)..(1i64 << 25)).contains(&imm_insts) {
            return Err(JitError::BranchDisplacementOutOfRange { offset, displacement: imm_insts });
        }
        *instr |= (imm_insts as u32) & 0x03ffffff;
    } else {
        return Err(JitError::UnexpectedBranchInstruction { instruction: *instr });
    }
    Ok(())
}

fn div_and_mod(
    state: &mut JitState<'_>,
    is_64_op: bool,
    mod_op: bool,
    rd: Register,
    rn: Register,
    rm: Register,
) {
    let div_dest = if mod_op { TEMP_DIV_REGISTER } else { rd };

    // Do not need to separately handle divide by zero: AArch64 UDIV returns 0 on division by zero,
    // which causes MSUB to preserve `rn` (zero-extended to 64 bits if `is_64_op == false`).
    emit_dataproc_2source(state, is_64_op, DataProc2SourceOpcode::Udiv, div_dest, rn, rm);
    if mod_op {
        emit_dataproc_3source(state, is_64_op, DataProc3SourceOpcode::Msub, rd, rm, div_dest, rn);
    }
}

fn fits_in_addsub_immediate(imm: u64) -> bool {
    imm < 0x1000
}

fn load_store_register(
    state: &mut JitState<'_>,
    op: LoadStoreRegisterOpcode,
    rt: Register,
    rn: Register,
    offset: i16,
) {
    emit_movewide_immediate(state, true, OFFSET_REGISTER, offset as i64 as u64);
    emit_loadstore_register(state, op, rt, rn, OFFSET_REGISTER);
}

/// Emit an effective address calculation `OFFSET_REGISTER = base + offset`.
fn emit_atomic_address(state: &mut JitState<'_>, base: Register, offset: i16) {
    let abs_offset = u32::from(offset.unsigned_abs());
    if offset == 0 {
        emit_logical_register(state, true, LogicalOpcode::Orr, OFFSET_REGISTER, RZ, base);
    } else if offset > 0 && abs_offset < 0x1000 {
        emit_addsub_immediate(state, true, AddSubOpcode::Add, OFFSET_REGISTER, base, abs_offset);
    } else if offset < 0 && abs_offset < 0x1000 {
        emit_addsub_immediate(state, true, AddSubOpcode::Sub, OFFSET_REGISTER, base, abs_offset);
    } else {
        emit_movewide_immediate(state, true, OFFSET_REGISTER, offset as i64 as u64);
        emit_addsub_register(
            state,
            true,
            AddSubOpcode::Add,
            OFFSET_REGISTER,
            base,
            OFFSET_REGISTER,
        );
    }
}

impl JitState<'_> {
    fn resolve_src(&mut self, is_64_op: bool, src: Source) -> Result<Register, VerifierErrorKind> {
        match src {
            Source::Reg(src_reg) => map_register(src_reg),
            Source::Value(imm) => {
                let imm_val = if is_64_op { imm } else { imm as u32 as u64 };
                emit_movewide_immediate(self, is_64_op, TEMP_REGISTER, imm_val);
                Ok(TEMP_REGISTER)
            }
        }
    }

    fn emit_add_sub(
        &mut self,
        is_64_op: bool,
        op: AddSubOpcode,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_addsub_register(self, is_64_op, op, dst, dst, src);
            }
            Source::Value(imm) => {
                let imm_val = if is_64_op { imm } else { imm as u32 as u64 };
                if fits_in_addsub_immediate(imm_val) {
                    emit_addsub_immediate(self, is_64_op, op, dst, dst, imm_val as u32);
                } else {
                    emit_movewide_immediate(self, is_64_op, TEMP_REGISTER, imm_val);
                    emit_addsub_register(self, is_64_op, op, dst, dst, TEMP_REGISTER);
                }
            }
        }
        Ok(())
    }

    fn emit_logical(
        &mut self,
        is_64_op: bool,
        op: LogicalOpcode,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = self.resolve_src(is_64_op, src)?;
        emit_logical_register(self, is_64_op, op, dst, dst, src);
        Ok(())
    }

    fn emit_lsh(
        &mut self,
        is_64_op: bool,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_dataproc_2source(self, is_64_op, DataProc2SourceOpcode::Lslv, dst, dst, src);
            }
            Source::Value(imm) => {
                let max_shift = if is_64_op { 63 } else { 31 };
                let shift = (imm as u32) & max_shift;
                let immr = shift.wrapping_neg() & max_shift;
                let imms = max_shift - shift;
                emit_bitfield(self, is_64_op, BitfieldOpcode::Ubfm, dst, dst, immr, imms);
            }
        }
        Ok(())
    }

    fn emit_right_shift(
        &mut self,
        is_64_op: bool,
        arithmetic: bool,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                let op = if arithmetic {
                    DataProc2SourceOpcode::Asrv
                } else {
                    DataProc2SourceOpcode::Lsrv
                };
                emit_dataproc_2source(self, is_64_op, op, dst, dst, src);
            }
            Source::Value(imm) => {
                let max_shift = if is_64_op { 63 } else { 31 };
                let shift = (imm as u32) & max_shift;
                let op = if arithmetic { BitfieldOpcode::Sbfm } else { BitfieldOpcode::Ubfm };
                emit_bitfield(self, is_64_op, op, dst, dst, shift, max_shift);
            }
        }
        Ok(())
    }

    fn emit_mul(
        &mut self,
        is_64_op: bool,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = self.resolve_src(is_64_op, src)?;
        emit_dataproc_3source(self, is_64_op, DataProc3SourceOpcode::Madd, dst, dst, src, RZ);
        Ok(())
    }

    fn emit_div_mod(
        &mut self,
        is_64_op: bool,
        mod_op: bool,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = self.resolve_src(is_64_op, src)?;
        div_and_mod(self, is_64_op, mod_op, dst, dst, src);
        Ok(())
    }

    fn emit_mov(
        &mut self,
        is_64_op: bool,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_logical_register(self, is_64_op, LogicalOpcode::Orr, dst, RZ, src);
            }
            Source::Value(imm) => {
                let imm_val = if is_64_op { imm } else { imm as u32 as u64 };
                emit_movewide_immediate(self, is_64_op, dst, imm_val);
            }
        }
        Ok(())
    }

    fn emit_cond_jump(
        &mut self,
        is_64_op: bool,
        cond: Condition,
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_addsub_register(self, is_64_op, AddSubOpcode::Subs, RZ, dst, src);
            }
            Source::Value(imm) => {
                let imm_val = if is_64_op { imm } else { imm as u32 as u64 };
                if fits_in_addsub_immediate(imm_val) {
                    emit_addsub_immediate(
                        self,
                        is_64_op,
                        AddSubOpcode::Subs,
                        RZ,
                        dst,
                        imm_val as u32,
                    );
                } else {
                    emit_movewide_immediate(self, is_64_op, TEMP_REGISTER, imm_val);
                    emit_addsub_register(
                        self,
                        is_64_op,
                        AddSubOpcode::Subs,
                        RZ,
                        dst,
                        TEMP_REGISTER,
                    );
                }
            }
        }
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_conditionalbranch_immediate(self, cond, target_pc);
        Ok(())
    }

    fn emit_jset(
        &mut self,
        is_64_op: bool,
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = self.resolve_src(is_64_op, src)?;
        emit_logical_register(self, is_64_op, LogicalOpcode::Ands, RZ, dst, src);
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_conditionalbranch_immediate(self, Condition::Ne, target_pc);
        Ok(())
    }

    /// Emit an atomic operation (`BPF_STX | BPF_ATOMIC | BPF_W` or `BPF_DW`) using a
    /// Load-Exclusive / Store-Exclusive (`LDAXR`/`STLXR`) loop from the mandatory base A64
    /// instruction set (supported on all ARMv8.0-A and later processors without requiring
    /// `FEAT_LSE`).
    fn emit_atomic(
        &mut self,
        is_64_op: bool,
        op: AtomicOp,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = map_register(src)?;
        emit_atomic_address(self, dst, offset);

        if op == AtomicOp::Cmpxchg {
            let r0 = Register::R6;
            let loop_start = self.offset();
            // LDAXR R24, [R26]
            emit_ldaxr(self, is_64_op, TEMP_REGISTER, OFFSET_REGISTER);
            // CMP R24, R0 (R6)
            emit_addsub_register(self, is_64_op, AddSubOpcode::Subs, RZ, TEMP_REGISTER, r0);
            // B.NE +12 (skip STLXR and CBNZ to CLREX)
            emit_instruction(self, 0x54000000 | (3 << 5) | (Condition::Ne as u32));
            // STLXR W28, src, [R26]
            emit_stlxr(self, is_64_op, SCRATCH_REGISTER, src, OFFSET_REGISTER);
            // CBNZ W28, loop_start
            emit_cbnz_w(self, SCRATCH_REGISTER, loop_start);
            // CLREX
            emit_clrex(self);
            // R0 = old_val (zero-extended if 32-bit)
            emit_logical_register(self, is_64_op, LogicalOpcode::Orr, r0, RZ, TEMP_REGISTER);
            return Ok(());
        }

        let loop_start = self.offset();
        // LDAXR R24, [R26]
        emit_ldaxr(self, is_64_op, TEMP_REGISTER, OFFSET_REGISTER);

        // Compute new value into R25 (or use `src` directly for XCHG).
        let (store_reg, write_old_to_src) = match op {
            AtomicOp::Add { fetch } => {
                emit_addsub_register(
                    self,
                    is_64_op,
                    AddSubOpcode::Add,
                    TEMP_DIV_REGISTER,
                    TEMP_REGISTER,
                    src,
                );
                (TEMP_DIV_REGISTER, fetch)
            }
            AtomicOp::And { fetch } => {
                emit_logical_register(
                    self,
                    is_64_op,
                    LogicalOpcode::And,
                    TEMP_DIV_REGISTER,
                    TEMP_REGISTER,
                    src,
                );
                (TEMP_DIV_REGISTER, fetch)
            }
            AtomicOp::Or { fetch } => {
                emit_logical_register(
                    self,
                    is_64_op,
                    LogicalOpcode::Orr,
                    TEMP_DIV_REGISTER,
                    TEMP_REGISTER,
                    src,
                );
                (TEMP_DIV_REGISTER, fetch)
            }
            AtomicOp::Xor { fetch } => {
                emit_logical_register(
                    self,
                    is_64_op,
                    LogicalOpcode::Eor,
                    TEMP_DIV_REGISTER,
                    TEMP_REGISTER,
                    src,
                );
                (TEMP_DIV_REGISTER, fetch)
            }
            AtomicOp::Xchg => (src, true),
            AtomicOp::Cmpxchg => unreachable!(),
        };

        // STLXR W28, store_reg, [R26]
        emit_stlxr(self, is_64_op, SCRATCH_REGISTER, store_reg, OFFSET_REGISTER);
        // CBNZ W28, loop_start
        emit_cbnz_w(self, SCRATCH_REGISTER, loop_start);

        if write_old_to_src {
            emit_logical_register(self, is_64_op, LogicalOpcode::Orr, src, RZ, TEMP_REGISTER);
        }

        Ok(())
    }
}

impl BpfVisitor<()> for JitState<'_> {
    fn add(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_add_sub(false, AddSubOpcode::Add, dst, src)
    }

    fn add64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_add_sub(true, AddSubOpcode::Add, dst, src)
    }

    fn and(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_logical(false, LogicalOpcode::And, dst, src)
    }

    fn and64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_logical(true, LogicalOpcode::And, dst, src)
    }

    fn arsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_right_shift(false, true, dst, src)
    }

    fn arsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_right_shift(true, true, dst, src)
    }

    fn div(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_div_mod(false, false, dst, src)
    }

    fn div64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_div_mod(true, false, dst, src)
    }

    fn lsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_lsh(false, dst, src)
    }

    fn lsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_lsh(true, dst, src)
    }

    fn r#mod(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_div_mod(false, true, dst, src)
    }

    fn mod64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_div_mod(true, true, dst, src)
    }

    fn mov(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_mov(false, dst, src)
    }

    fn mov64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_mov(true, dst, src)
    }

    fn mul(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_mul(false, dst, src)
    }

    fn mul64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_mul(true, dst, src)
    }

    fn or(&mut self, _context: &mut (), dst: BpfReg, src: Source) -> Result<(), VerifierErrorKind> {
        self.emit_logical(false, LogicalOpcode::Orr, dst, src)
    }

    fn or64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_logical(true, LogicalOpcode::Orr, dst, src)
    }

    fn rsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_right_shift(false, false, dst, src)
    }

    fn rsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_right_shift(true, false, dst, src)
    }

    fn sub(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_add_sub(false, AddSubOpcode::Sub, dst, src)
    }

    fn sub64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_add_sub(true, AddSubOpcode::Sub, dst, src)
    }

    fn xor(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_logical(false, LogicalOpcode::Eor, dst, src)
    }

    fn xor64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_logical(true, LogicalOpcode::Eor, dst, src)
    }

    fn neg(&mut self, _context: &mut (), dst: BpfReg) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        emit_addsub_register(self, false, AddSubOpcode::Sub, dst, RZ, dst);
        Ok(())
    }

    fn neg64(&mut self, _context: &mut (), dst: BpfReg) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        emit_addsub_register(self, true, AddSubOpcode::Sub, dst, RZ, dst);
        Ok(())
    }

    fn be(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match width {
            DataWidth::U16 => {
                emit_dataproc_1source(self, false, DataProc1SourceOpcode::Rev16, dst, dst);
                emit_uxth(self, dst, dst);
            }
            DataWidth::U32 => {
                emit_dataproc_1source(self, false, DataProc1SourceOpcode::Rev32, dst, dst);
            }
            DataWidth::U64 => {
                emit_dataproc_1source(self, true, DataProc1SourceOpcode::Rev64, dst, dst);
            }
            DataWidth::U8 => unreachable!(),
        }
        Ok(())
    }

    fn le(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match width {
            DataWidth::U16 => emit_uxth(self, dst, dst),
            DataWidth::U32 => emit_logical_register(self, false, LogicalOpcode::Orr, dst, RZ, dst),
            DataWidth::U64 => {}
            DataWidth::U8 => unreachable!(),
        }
        Ok(())
    }

    fn call_external(&mut self, _context: &mut (), index: u32) -> Result<(), VerifierErrorKind> {
        let helper = self
            .helpers
            .iter()
            .find(|h| h.id == index && h.fn_addr != 0)
            .ok_or(VerifierErrorKind::UnknownHelper(index))?;
        let fn_addr = helper.fn_addr;

        // Pass `run_context` in X0; X1..X5 already contain eBPF R1..R5.
        emit_logical_register(
            self,
            true,
            LogicalOpcode::Orr,
            Register::R0,
            RZ,
            RUN_CONTEXT_REGISTER,
        );
        emit_movewide_immediate(self, true, TEMP_REGISTER, fn_addr);
        emit_unconditionalbranch_register(self, UnconditionalBranchOpcode::Blr, TEMP_REGISTER);
        // Store return value from X0 into eBPF R0 (R6).
        emit_logical_register(self, true, LogicalOpcode::Orr, Register::R6, RZ, Register::R0);
        Ok(())
    }

    fn exit(&mut self, _context: &mut ()) -> Result<(), VerifierErrorKind> {
        emit_unconditionalbranch_immediate(
            self,
            UnconditionalBranchImmediateOpcode::B,
            TARGET_PC_EXIT,
        );
        Ok(())
    }

    fn jump(&mut self, _context: &mut (), offset: i16) -> Result<(), VerifierErrorKind> {
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_unconditionalbranch_immediate(self, UnconditionalBranchImmediateOpcode::B, target_pc);
        Ok(())
    }

    fn jeq(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Eq, dst, src, offset)
    }

    fn jeq64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Eq, dst, src, offset)
    }

    fn jne(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Ne, dst, src, offset)
    }

    fn jne64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Ne, dst, src, offset)
    }

    fn jge(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Cs, dst, src, offset)
    }

    fn jge64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Cs, dst, src, offset)
    }

    fn jgt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Hi, dst, src, offset)
    }

    fn jgt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Hi, dst, src, offset)
    }

    fn jle(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Ls, dst, src, offset)
    }

    fn jle64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Ls, dst, src, offset)
    }

    fn jlt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Cc, dst, src, offset)
    }

    fn jlt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Cc, dst, src, offset)
    }

    fn jsge(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Ge, dst, src, offset)
    }

    fn jsge64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Ge, dst, src, offset)
    }

    fn jsgt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Gt, dst, src, offset)
    }

    fn jsgt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Gt, dst, src, offset)
    }

    fn jsle(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Le, dst, src, offset)
    }

    fn jsle64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Le, dst, src, offset)
    }

    fn jslt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, Condition::Lt, dst, src, offset)
    }

    fn jslt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, Condition::Lt, dst, src, offset)
    }

    fn jset(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_jset(false, dst, src, offset)
    }

    fn jset64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_jset(true, dst, src, offset)
    }

    fn atomic_add(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::Add { fetch }, dst, offset, src)
    }

    fn atomic_add64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::Add { fetch }, dst, offset, src)
    }

    fn atomic_and(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::And { fetch }, dst, offset, src)
    }

    fn atomic_and64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::And { fetch }, dst, offset, src)
    }

    fn atomic_or(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::Or { fetch }, dst, offset, src)
    }

    fn atomic_or64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::Or { fetch }, dst, offset, src)
    }

    fn atomic_xor(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::Xor { fetch }, dst, offset, src)
    }

    fn atomic_xor64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::Xor { fetch }, dst, offset, src)
    }

    fn atomic_xchg(
        &mut self,
        _context: &mut (),
        _fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::Xchg, dst, offset, src)
    }

    fn atomic_xchg64(
        &mut self,
        _context: &mut (),
        _fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::Xchg, dst, offset, src)
    }

    fn atomic_cmpxchg(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(false, AtomicOp::Cmpxchg, dst, offset, src)
    }

    fn atomic_cmpxchg64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_atomic(true, AtomicOp::Cmpxchg, dst, offset, src)
    }

    fn load(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = map_register(src)?;
        if (-256..256).contains(&offset) {
            let op = match width {
                DataWidth::U8 => LoadStoreImmediateOpcode::Ldrb,
                DataWidth::U16 => LoadStoreImmediateOpcode::Ldrh,
                DataWidth::U32 => LoadStoreImmediateOpcode::Ldrw,
                DataWidth::U64 => LoadStoreImmediateOpcode::Ldrx,
            };
            emit_loadstore_immediate(self, op, dst, src, offset);
        } else {
            let op = match width {
                DataWidth::U8 => LoadStoreRegisterOpcode::Ldrb,
                DataWidth::U16 => LoadStoreRegisterOpcode::Ldrh,
                DataWidth::U32 => LoadStoreRegisterOpcode::Ldrw,
                DataWidth::U64 => LoadStoreRegisterOpcode::Ldrx,
            };
            load_store_register(self, op, dst, src, offset);
        }
        Ok(())
    }

    fn load64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: u8,
        lower: u32,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let imm64 = resolve_lddw(self.code, self.pc, src, lower)?;
        emit_movewide_immediate(self, true, dst, imm64);
        self.pc += 1;
        Ok(())
    }

    fn load_from_packet(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: BpfReg,
        offset: i32,
        register_offset: Option<BpfReg>,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src_reg = map_register(src)?;
        let fn_ptr = self
            .packet_load_fn
            .filter(|&addr| addr != 0)
            .ok_or(VerifierErrorKind::RegisterNotPacket(src))?;
        let width_bytes = width.bytes() as u32;

        // If `src_reg` happens to be `X2`, stash it in `OFFSET_REGISTER` (`X26`) before writing
        // `reg_offset` into `X2`.
        let skb_reg = if src_reg == Register::R2 {
            emit_logical_register(self, true, LogicalOpcode::Orr, OFFSET_REGISTER, RZ, src_reg);
            OFFSET_REGISTER
        } else {
            src_reg
        };

        // Set X2 (`reg_offset`) FIRST in case `register_offset` is in R1..R5 (`X1..X5`).
        if let Some(off_reg) = register_offset {
            let off_reg = map_register(off_reg)?;
            emit_logical_register(self, false, LogicalOpcode::Orr, Register::R2, RZ, off_reg);
        } else {
            emit_movewide_immediate(self, false, Register::R2, 0);
        }

        // X0 = run_context (X27)
        emit_logical_register(
            self,
            true,
            LogicalOpcode::Orr,
            Register::R0,
            RZ,
            RUN_CONTEXT_REGISTER,
        );
        // X1 = skb pointer (`src_reg`, normally eBPF R6 -> X19)
        emit_logical_register(self, true, LogicalOpcode::Orr, Register::R1, RZ, skb_reg);
        // X3 = imm_offset
        emit_movewide_immediate(self, false, Register::R3, offset as u32 as u64);
        // X4 = width_bytes
        emit_movewide_immediate(self, false, Register::R4, width_bytes as u64);
        // X5 = &out_value at [SP, #PACKET_LOAD_OUT_OFFSET]
        emit_addsub_immediate(
            self,
            true,
            AddSubOpcode::Add,
            Register::R5,
            SP,
            PACKET_LOAD_OUT_OFFSET,
        );

        // Call packet_load_fn
        emit_movewide_immediate(self, true, TEMP_REGISTER, fn_ptr);
        emit_unconditionalbranch_register(self, UnconditionalBranchOpcode::Blr, TEMP_REGISTER);

        // Load `*out_value` into `dst` (eBPF R0 -> R6). When `packet_load_fn` returns 0,
        // `*out_value` is 0, so eBPF R0 is already 0 if we branch to exit.
        emit_loadstore_immediate(
            self,
            LoadStoreImmediateOpcode::Ldrx,
            dst,
            SP,
            PACKET_LOAD_OUT_OFFSET as i16,
        );

        // Compare return status `X0` with 0; if 0, exit the program.
        emit_addsub_immediate(self, true, AddSubOpcode::Subs, RZ, Register::R0, 0);
        emit_conditionalbranch_immediate(self, Condition::Eq, TARGET_PC_EXIT);

        Ok(())
    }

    fn store(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: Source,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = self.resolve_src(true, src)?;
        if (-256..256).contains(&offset) {
            let op = match width {
                DataWidth::U8 => LoadStoreImmediateOpcode::Strb,
                DataWidth::U16 => LoadStoreImmediateOpcode::Strh,
                DataWidth::U32 => LoadStoreImmediateOpcode::Strw,
                DataWidth::U64 => LoadStoreImmediateOpcode::Strx,
            };
            emit_loadstore_immediate(self, op, src, dst, offset);
        } else {
            let op = match width {
                DataWidth::U8 => LoadStoreRegisterOpcode::Strb,
                DataWidth::U16 => LoadStoreRegisterOpcode::Strh,
                DataWidth::U32 => LoadStoreRegisterOpcode::Strw,
                DataWidth::U64 => LoadStoreRegisterOpcode::Strx,
            };
            load_store_register(self, op, src, dst, offset);
        }
        Ok(())
    }
}

/// Compile a relocated slice of eBPF instructions into 32-bit AArch64 (A64) instructions.
pub fn compile_instructions(
    insts: &[EbpfInstruction],
    helpers: &[HelperLinkage],
    packet_load_fn: Option<u64>,
) -> Result<Vec<u32>, JitError> {
    let mut state = JitState::new(insts, helpers, packet_load_fn);

    // Stack frame size:
    // - 16 bytes for R29, R30
    // - 80 bytes for CALLEE_SAVED_REGISTERS (R19..R28)
    // - 16 bytes scratch slot at [SP + PACKET_LOAD_OUT_OFFSET]
    // - 512 bytes (BPF_STACK_SIZE) for eBPF stack
    let stack_size = (BPF_STACK_SIZE as u32 + REGISTER_SPACE + 15) & !15u32;

    // Prologue:
    emit_addsub_immediate(&mut state, true, AddSubOpcode::Sub, SP, SP, stack_size);
    emit_loadstorepair_immediate(
        &mut state,
        LoadStorePairOpcode::Stpx,
        Register::R29,
        Register::R30,
        SP,
        0,
    );
    emit_addsub_immediate(&mut state, true, AddSubOpcode::Add, Register::R29, SP, 0);

    for i in (0..CALLEE_SAVED_REGISTERS.len()).step_by(2) {
        emit_loadstorepair_immediate(
            &mut state,
            LoadStorePairOpcode::Stpx,
            CALLEE_SAVED_REGISTERS[i],
            CALLEE_SAVED_REGISTERS[i + 1],
            SP,
            ((i as i32) + 2) * 8,
        );
    }

    // Preserve `run_context` (`X0` on entry) in `RUN_CONTEXT_REGISTER` (`X27`).
    emit_logical_register(
        &mut state,
        true,
        LogicalOpcode::Orr,
        RUN_CONTEXT_REGISTER,
        RZ,
        Register::R0,
    );

    // Initialize eBPF R10 (`X23`) to point to the top of the 512-byte eBPF stack.
    emit_addsub_immediate(&mut state, true, AddSubOpcode::Add, Register::R23, SP, stack_size);

    while state.pc < insts.len() {
        let pc_loc = u32::try_from(state.offset()).map_err(|_| JitError::CodeBufferOverflow)?;
        state.pc_locs[state.pc] = Some(pc_loc);
        state.visit(&mut (), insts[state.pc])?;
        state.pc += 1;
    }

    // Epilogue:
    state.exit_loc = u32::try_from(state.offset()).map_err(|_| JitError::CodeBufferOverflow)?;

    // Move register 0 into X0 (return value).
    emit_logical_register(&mut state, true, LogicalOpcode::Orr, Register::R0, RZ, Register::R6);

    // Restore stack pointer and callee-saved registers.
    emit_addsub_immediate(&mut state, true, AddSubOpcode::Add, SP, Register::R29, 0);
    for idx in (0..CALLEE_SAVED_REGISTERS.len()).step_by(2) {
        emit_loadstorepair_immediate(
            &mut state,
            LoadStorePairOpcode::Ldpx,
            CALLEE_SAVED_REGISTERS[idx],
            CALLEE_SAVED_REGISTERS[idx + 1],
            SP,
            ((idx as i32) + 2) * 8,
        );
    }
    emit_loadstorepair_immediate(
        &mut state,
        LoadStorePairOpcode::Ldpx,
        Register::R29,
        Register::R30,
        SP,
        0,
    );
    emit_addsub_immediate(&mut state, true, AddSubOpcode::Add, SP, SP, stack_size);
    emit_unconditionalbranch_register(&mut state, UnconditionalBranchOpcode::Ret, Register::R30);

    // Patch jump targets:
    for jump in &state.jumps {
        let target_loc =
            resolve_patched_target_loc(jump.target_pc, state.exit_loc, &state.pc_locs)?;
        let jump_offset = i64::try_from(jump.offset).map_err(|_| JitError::CodeBufferOverflow)?;
        let rel_insts = i64::from(target_loc) - jump_offset;
        update_branch_immediate(&mut state.buf[jump.offset], jump.offset, rel_insts)?;
    }

    Ok(state.buf)
}

/// Compile a relocated slice of eBPF instructions into AArch64 machine code.
pub fn compile(
    insts: &[EbpfInstruction],
    helpers: &[HelperLinkage],
    packet_load_fn: Option<u64>,
) -> Result<Vec<u8>, JitError> {
    let instructions = compile_instructions(insts, helpers, packet_load_fn)?;
    Ok(instructions.into_iter().flat_map(u32::to_le_bytes).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::test::parse_asm;

    #[test]
    fn test_ret_encoding_at_end_of_compiled_code() {
        let insts = parse_asm(
            r#"
            mov %r0, 42
            exit
            "#,
        );
        let arm_insts = compile_instructions(&insts, &[], None).expect("compile_instructions");
        // Last instruction must be RET (0xd65f03c0).
        assert_eq!(*arm_insts.last().unwrap(), 0xd65f03c0);

        let code = compile(&insts, &[], None).expect("compile");
        assert_eq!(code.len(), arm_insts.len() * 4);
        let last_inst = u32::from_le_bytes(code[code.len() - 4..].try_into().unwrap());
        assert_eq!(last_inst, 0xd65f03c0);
    }

    #[test]
    fn test_immediate_shifts_do_not_emit_dead_mov() {
        let shift_insts = parse_asm(
            r#"
            lsh %r0, 4
            rsh %r0, 4
            arsh %r0, 4
            lsh32 %r0, 4
            rsh32 %r0, 4
            arsh32 %r0, 4
            "#,
        );
        for inst in shift_insts {
            let mut state = JitState::new(&[], &[], None);
            state.visit(&mut (), inst).expect("visit immediate shift");
            assert_eq!(state.buf.len(), 1, "Immediate shift {inst:?} should emit 1 instruction");
        }
    }

    #[test]
    fn test_update_branch_immediate_out_of_range_returns_err() {
        // Conditional branch (`B.EQ`, 19-bit signed displacement in instructions):
        let mut cond_inst = 0x54000000u32;
        assert!(update_branch_immediate(&mut cond_inst, 0, (1i64 << 18) - 1).is_ok());
        assert_eq!(
            update_branch_immediate(&mut cond_inst, 0, 1i64 << 18),
            Err(JitError::BranchDisplacementOutOfRange { offset: 0, displacement: 1i64 << 18 })
        );
        assert_eq!(
            update_branch_immediate(&mut cond_inst, 0, -((1i64 << 18) + 1)),
            Err(JitError::BranchDisplacementOutOfRange {
                offset: 0,
                displacement: -((1i64 << 18) + 1),
            })
        );

        // Unconditional branch (`B`, 26-bit signed displacement in instructions):
        let mut uncond_inst = 0x14000000u32;
        assert!(update_branch_immediate(&mut uncond_inst, 0, (1i64 << 25) - 1).is_ok());
        assert_eq!(
            update_branch_immediate(&mut uncond_inst, 0, 1i64 << 25),
            Err(JitError::BranchDisplacementOutOfRange { offset: 0, displacement: 1i64 << 25 })
        );

        // Non-branch instruction:
        let mut non_branch = 0xd65f03c0u32;
        assert_eq!(
            update_branch_immediate(&mut non_branch, 0, 0),
            Err(JitError::UnexpectedBranchInstruction { instruction: 0xd65f03c0 })
        );
    }

    #[test]
    fn test_emit_atomic_address_handles_i16_min_and_small_offsets() {
        // Small negative/positive offsets (`|offset| < 0x1000`) use 1 `ADD`/`SUB` immediate
        // instruction; `offset == -4096` and `i16::MIN` (`-32768`) materialize into
        // `OFFSET_REGISTER` (`MOVN`) + `ADD` without overflowing `-offset`.
        for (offset, expected_insts) in
            [(0i16, 1usize), (8, 1), (-8, 1), (-4095, 1), (-4096, 2), (i16::MIN, 2)]
        {
            let mut state = JitState::new(&[], &[], None);
            emit_atomic_address(&mut state, Register::R23, offset);
            assert_eq!(state.buf.len(), expected_insts, "offset={offset}");
        }
    }

    #[test]
    fn test_movewide_32bit_negative_uses_single_movn() {
        let mov32 = parse_asm("mov32 %r0, -1\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), mov32).expect("visit mov32");
        assert_eq!(state.buf.len(), 1);
        // MOVN W6, #0 (`sf = 0`, `opc = 0` -> `0x12800006`).
        assert_eq!(state.buf[0], 0x12800006);

        // 32-bit `add32`, `and32`, and `jeq32` with negative immediates whose upper 16 bits are
        // `0xffff` should materialize `TEMP_REGISTER` via a single `MOVN W24` rather than
        // `MOVZ` + `MOVK` (`MOVN` + ALU = 2 insts; `MOVN` + `SUBS` + `B.EQ` = 3 insts).
        for (asm, expected_insts) in [
            ("add32 %r0, -5000\n", 2usize),
            ("and32 %r0, -2\n", 2),
            ("jeq32 %r0, -1, +0\nexit\n", 3),
        ] {
            let insts = parse_asm(asm);
            let mut state = JitState::new(&insts, &[], None);
            state.visit(&mut (), insts[0]).expect("visit 32-bit negative imm op");
            assert_eq!(state.buf.len(), expected_insts, "asm={asm}");
            // First emitted instruction must be `MOVN W24, ...` (`sf = 0`, `opc = 0` -> top byte `0x12`).
            assert_eq!(state.buf[0] & 0xff80001f, 0x12800018, "asm={asm}");
        }
    }
}
