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

// System V AMD64 ABI for JIT-compiled eBPF programs:
// - `run_context` is passed in `RDI` and preserved in callee-saved `R12`.
// - `arg1..arg5` are passed in `RSI, RDX, RCX, R8, R9` (`RCX` is moved to `R10`
//   in the prologue so `RCX` is free for shifts and division).
// - Return value (`R0`, mapped to `RAX`) is returned in `RAX`.

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Register {
    Rax = 0,
    Rcx = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

impl Register {
    #[inline]
    const fn is_ext(self) -> bool {
        (self as u8 & 8) != 0
    }

    #[inline]
    const fn low3(self) -> u8 {
        self as u8 & 7
    }
}

/// Callee-saved registers pushed in the prologue and popped in reverse order in the epilogue.
const CALLEE_SAVED_REGISTERS: [Register; 6] =
    [Register::Rbp, Register::Rbx, Register::R12, Register::R13, Register::R14, Register::R15];

/// Mapping from eBPF registers `R0..R10` to x86-64 registers:
/// - `R0 -> RAX` (return value)
/// - `R1..R5 -> RSI, RDX, R10, R8, R9` (arguments 1..5; `R3` moved from `RCX` to `R10`)
/// - `R6..R9 -> RBX, R13, R14, R15` (callee-saved)
/// - `R10 -> RBP` (callee-saved eBPF frame pointer)
///
/// `R12` is reserved for `run_context` (callee-saved).
/// `RCX`, `RDI`, and `R11` are free caller-saved scratch registers.
const REGISTER_MAP: [Register; 11] = [
    Register::Rax, // R0
    Register::Rsi, // R1
    Register::Rdx, // R2
    Register::R10, // R3
    Register::R8,  // R4
    Register::R9,  // R5
    Register::Rbx, // R6
    Register::R13, // R7
    Register::R14, // R8
    Register::R15, // R9
    Register::Rbp, // R10
];

#[inline]
fn map_register(r: BpfReg) -> Result<Register, VerifierErrorKind> {
    REGISTER_MAP.get(r as usize).copied().ok_or(VerifierErrorKind::InvalidRegister(r))
}

/// Extra stack space allocated above `RBP` (`[RBP .. RBP + 8]`):
/// aligns `RSP` to a 16-byte boundary after 6 pushes (on entry `RSP % 16 == 8`) while providing
/// an 8-byte aligned scratch slot at `[RBP + 0]` for `packet_load_fn`.
const STACK_SCRATCH_AND_ALIGN_SIZE: i32 = 8;
const PACKET_LOAD_OUT_OFFSET: i32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperandSize {
    S8,
    S16,
    S32,
    S64,
}

impl From<DataWidth> for OperandSize {
    fn from(width: DataWidth) -> Self {
        match width {
            DataWidth::U8 => Self::S8,
            DataWidth::U16 => Self::S16,
            DataWidth::U32 => Self::S32,
            DataWidth::U64 => Self::S64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DivModOp {
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug)]
struct Jump {
    offset_loc: usize,
    target_pc: u32,
}

struct JitState<'a> {
    code: &'a [EbpfInstruction],
    helpers: &'a [HelperLinkage],
    packet_load_fn: Option<u64>,
    pc: usize,
    buf: Vec<u8>,
    pc_locs: Vec<Option<u32>>,
    exit_loc: u32,
    jumps: Vec<Jump>,
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
            buf: Vec::with_capacity((num_insts + 16) * 8),
            pc_locs: vec![None; num_insts],
            exit_loc: 0,
            jumps: Vec::new(),
        }
    }

    #[inline]
    fn offset(&self) -> usize {
        self.buf.len()
    }
}

#[inline]
fn emit_bytes(state: &mut JitState<'_>, data: &[u8]) {
    state.buf.extend_from_slice(data);
}

#[inline]
fn emit1(state: &mut JitState<'_>, x: u8) {
    state.buf.push(x);
}

#[inline]
fn emit2(state: &mut JitState<'_>, x: u16) {
    emit_bytes(state, &x.to_le_bytes());
}

#[inline]
fn emit4(state: &mut JitState<'_>, x: u32) {
    emit_bytes(state, &x.to_le_bytes());
}

#[inline]
fn emit8(state: &mut JitState<'_>, x: u64) {
    emit_bytes(state, &x.to_le_bytes());
}

#[inline]
fn emit_jump_target_address(state: &mut JitState<'_>, target_pc: u32) {
    state.jumps.push(Jump { offset_loc: state.offset(), target_pc });
    emit4(state, 0);
}

#[inline]
fn emit_modrm(state: &mut JitState<'_>, mode: u8, reg_or_subop: u8, m: Register) {
    debug_assert_eq!(mode & !0xc0, 0);
    debug_assert!(reg_or_subop < 8);
    emit1(state, (mode & 0xc0) | (reg_or_subop << 3) | m.low3());
}

#[inline]
fn emit_modrm_reg2reg(state: &mut JitState<'_>, r: Register, m: Register) {
    emit_modrm(state, 0xc0, r.low3(), m);
}

#[inline]
fn emit_modrm_subop_and_displacement(state: &mut JitState<'_>, subop: u8, m: Register, d: i32) {
    debug_assert_ne!(m.low3(), Register::Rsp.low3(), "RSP/R12 base requires SIB byte");
    if d == 0 && m.low3() != Register::Rbp.low3() {
        emit_modrm(state, 0x00, subop, m);
    } else if (-128..=127).contains(&d) {
        emit_modrm(state, 0x40, subop, m);
        emit1(state, d as i8 as u8);
    } else {
        emit_modrm(state, 0x80, subop, m);
        emit4(state, d as u32);
    }
}

#[inline]
fn emit_modrm_and_displacement(state: &mut JitState<'_>, r: Register, m: Register, d: i32) {
    emit_modrm_subop_and_displacement(state, r.low3(), m, d);
}

#[inline]
fn emit_rex(state: &mut JitState<'_>, w: u8, r: u8, x: u8, b: u8) {
    debug_assert!(w <= 1 && r <= 1 && x <= 1 && b <= 1);
    emit1(state, 0x40 | (w << 3) | (r << 2) | (x << 1) | b);
}

#[inline]
fn emit_basic_rex(state: &mut JitState<'_>, w: u8, src: Register, dst: Register) {
    if w != 0 || src.is_ext() || dst.is_ext() {
        emit_rex(state, w, u8::from(src.is_ext()), 0, u8::from(dst.is_ext()));
    }
}

#[inline]
fn emit_rex_rm(state: &mut JitState<'_>, w: u8, dst: Register) {
    if w != 0 || dst.is_ext() {
        emit_rex(state, w, 0, 0, u8::from(dst.is_ext()));
    }
}

#[inline]
fn emit_push(state: &mut JitState<'_>, r: Register) {
    emit_rex_rm(state, 0, r);
    emit1(state, 0x50 | r.low3());
}

#[inline]
fn emit_pop(state: &mut JitState<'_>, r: Register) {
    emit_rex_rm(state, 0, r);
    emit1(state, 0x58 | r.low3());
}

#[inline]
fn emit_alu32(state: &mut JitState<'_>, op: u8, src: Register, dst: Register) {
    emit_basic_rex(state, 0, src, dst);
    emit1(state, op);
    emit_modrm_reg2reg(state, src, dst);
}

#[inline]
fn emit_alu32_subop(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register) {
    emit_rex_rm(state, 0, dst);
    emit1(state, op);
    emit_modrm(state, 0xc0, subop, dst);
}

#[inline]
fn emit_alu32_imm32(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register, imm: i32) {
    emit_alu32_subop(state, op, subop, dst);
    emit4(state, imm as u32);
}

#[inline]
fn emit_alu32_imm8(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register, imm: i8) {
    emit_alu32_subop(state, op, subop, dst);
    emit1(state, imm as u8);
}

#[inline]
fn emit_alu64(state: &mut JitState<'_>, op: u8, src: Register, dst: Register) {
    emit_basic_rex(state, 1, src, dst);
    emit1(state, op);
    emit_modrm_reg2reg(state, src, dst);
}

#[inline]
fn emit_alu64_subop(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register) {
    emit_rex_rm(state, 1, dst);
    emit1(state, op);
    emit_modrm(state, 0xc0, subop, dst);
}

#[inline]
fn emit_alu64_imm32(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register, imm: i32) {
    emit_alu64_subop(state, op, subop, dst);
    emit4(state, imm as u32);
}

#[inline]
fn emit_alu64_imm8(state: &mut JitState<'_>, op: u8, subop: u8, dst: Register, imm: i8) {
    emit_alu64_subop(state, op, subop, dst);
    emit1(state, imm as u8);
}

/// 64-bit register-to-register `mov dst, src`.
#[inline]
fn emit_mov(state: &mut JitState<'_>, src: Register, dst: Register) {
    if src != dst {
        emit_alu64(state, 0x89, src, dst);
    }
}

/// 32-bit register-to-register `mov dst32, src32` (zero-extends low 32 bits of `src` into 64-bit
/// `dst`).
#[inline]
fn emit_mov32(state: &mut JitState<'_>, src: Register, dst: Register) {
    emit_alu32(state, 0x89, src, dst);
}

/// Zero-extend low 16 bits of `src` into `dst` (`movzx dst32, src16`, which also clears bits
/// 63..32 of `dst`).
#[inline]
fn emit_movzx16(state: &mut JitState<'_>, src: Register, dst: Register) {
    emit_basic_rex(state, 0, dst, src);
    emit1(state, 0x0f);
    emit1(state, 0xb7);
    emit_modrm_reg2reg(state, dst, src);
}

#[inline]
fn emit_cmp_imm32(state: &mut JitState<'_>, dst: Register, imm: i32) {
    emit_alu64_imm32(state, 0x81, 7, dst, imm);
}

#[inline]
fn emit_cmp32_imm32(state: &mut JitState<'_>, dst: Register, imm: i32) {
    emit_alu32_imm32(state, 0x81, 7, dst, imm);
}

#[inline]
fn emit_cmp(state: &mut JitState<'_>, src: Register, dst: Register) {
    emit_alu64(state, 0x39, src, dst);
}

#[inline]
fn emit_cmp32(state: &mut JitState<'_>, src: Register, dst: Register) {
    emit_alu32(state, 0x39, src, dst);
}

#[inline]
fn emit_jcc(state: &mut JitState<'_>, code: u8, target_pc: u32) {
    emit1(state, 0x0f);
    emit1(state, code);
    emit_jump_target_address(state, target_pc);
}

#[inline]
fn emit_jmp(state: &mut JitState<'_>, target_pc: u32) {
    emit1(state, 0xe9);
    emit_jump_target_address(state, target_pc);
}

/// Load `[src + offset]` into `dst`.
fn emit_load(
    state: &mut JitState<'_>,
    size: OperandSize,
    src: Register,
    dst: Register,
    offset: i32,
) {
    emit_basic_rex(state, u8::from(size == OperandSize::S64), dst, src);
    match size {
        OperandSize::S8 => {
            emit1(state, 0x0f);
            emit1(state, 0xb6);
        }
        OperandSize::S16 => {
            emit1(state, 0x0f);
            emit1(state, 0xb7);
        }
        OperandSize::S32 | OperandSize::S64 => {
            emit1(state, 0x8b);
        }
    }
    emit_modrm_and_displacement(state, dst, src, offset);
}

/// Load 64-bit immediate into `dst` (using sign-extended `mov r/m64, imm32` when in `i32` range,
/// or `movabs r64, imm64` otherwise).
fn emit_load_imm(state: &mut JitState<'_>, dst: Register, imm: i64) {
    if let Ok(imm32) = i32::try_from(imm) {
        emit_alu64_imm32(state, 0xc7, 0, dst, imm32);
    } else {
        emit_rex_rm(state, 1, dst);
        emit1(state, 0xb8 | dst.low3());
        emit8(state, imm as u64);
    }
}

/// Store register `src` to `[dst + offset]`.
fn emit_store(
    state: &mut JitState<'_>,
    size: OperandSize,
    src: Register,
    dst: Register,
    offset: i32,
) {
    if size == OperandSize::S16 {
        emit1(state, 0x66);
    }
    let rexw = u8::from(size == OperandSize::S64);
    if rexw != 0 || src.is_ext() || dst.is_ext() || (size == OperandSize::S8 && (src as u8) >= 4) {
        emit_rex(state, rexw, u8::from(src.is_ext()), 0, u8::from(dst.is_ext()));
    }
    emit1(state, if size == OperandSize::S8 { 0x88 } else { 0x89 });
    emit_modrm_and_displacement(state, src, dst, offset);
}

/// Store immediate `imm` to `[dst + offset]`.
fn emit_store_imm32(
    state: &mut JitState<'_>,
    size: OperandSize,
    dst: Register,
    offset: i32,
    imm: i32,
) {
    if size == OperandSize::S16 {
        emit1(state, 0x66);
    }
    emit_rex_rm(state, u8::from(size == OperandSize::S64), dst);
    emit1(state, if size == OperandSize::S8 { 0xc6 } else { 0xc7 });
    emit_modrm_subop_and_displacement(state, 0, dst, offset);
    match size {
        OperandSize::S32 | OperandSize::S64 => emit4(state, imm as u32),
        OperandSize::S16 => emit2(state, imm as u16),
        OperandSize::S8 => emit1(state, imm as u8),
    }
}

/// Emit `DIV` or `MOD` (32-bit or 64-bit, immediate or register).
///
/// Uses caller-saved scratch registers `RCX` (divisor), `R11` (saved `RAX`), and `RDI`
/// (saved `RDX`), avoiding any stack pushes/pops.
fn emit_divmod(
    state: &mut JitState<'_>,
    is64: bool,
    op: DivModOp,
    dst: BpfReg,
    src: Source,
) -> Result<(), VerifierErrorKind> {
    let dst = map_register(dst)?;

    // 1. Load the divisor into RCX first, before RAX or RDX is touched.
    //    `BpfVisitor::visit` (`div_imm`) already rejects `imm == 0` with `DivByZero`.
    match src {
        Source::Reg(src_reg) => {
            let src = map_register(src_reg)?;
            emit_mov(state, src, Register::Rcx);
        }
        Source::Value(imm) => {
            if is64 {
                emit_load_imm(state, Register::Rcx, imm as i64);
            } else {
                emit_alu32_imm32(state, 0xc7, 0, Register::Rcx, imm as i32);
            }
        }
    }

    // 2. Save RAX (into R11) and RDX (into RDI) if `dst` is not that register.
    if dst != Register::Rax {
        emit_mov(state, Register::Rax, Register::R11);
    }
    if dst != Register::Rdx {
        emit_mov(state, Register::Rdx, Register::Rdi);
    }

    // 3. Move `dst` (dividend) into RAX (zero-extended if 32-bit).
    if is64 {
        emit_mov(state, dst, Register::Rax);
    } else {
        emit_mov32(state, dst, Register::Rax);
    }

    match op {
        DivModOp::Div => {
            // Clear RDX before unsigned division.
            emit_alu32(state, 0x31, Register::Rdx, Register::Rdx);
            if matches!(src, Source::Reg(_)) {
                // Test divisor (RCX for 64-bit, ECX for 32-bit) for zero.
                if is64 {
                    emit_alu64(state, 0x85, Register::Rcx, Register::Rcx);
                } else {
                    emit_alu32(state, 0x85, Register::Rcx, Register::Rcx);
                }
                // jz .div_zero
                let jz_loc = state.offset();
                emit1(state, 0x74);
                emit1(state, 0);

                if is64 {
                    emit_alu64_subop(state, 0xf7, 6, Register::Rcx);
                } else {
                    emit_alu32_subop(state, 0xf7, 6, Register::Rcx);
                }
                // jmp .div_done
                let jmp_loc = state.offset();
                emit1(state, 0xeb);
                emit1(state, 0);

                // .div_zero: quotient is 0 on division by zero.
                state.buf[jz_loc + 1] = (state.offset() - (jz_loc + 2)) as u8;
                emit_alu32(state, 0x31, Register::Rax, Register::Rax);

                // .div_done:
                state.buf[jmp_loc + 1] = (state.offset() - (jmp_loc + 2)) as u8;
            } else if is64 {
                emit_alu64_subop(state, 0xf7, 6, Register::Rcx);
            } else {
                emit_alu32_subop(state, 0xf7, 6, Register::Rcx);
            }
            emit_mov(state, Register::Rax, dst);
        }
        DivModOp::Mod => {
            // MOD: on modulo by zero, eBPF preserves dividend (zero-extended for 32-bit, which is
            // already in RAX!).
            emit_alu32(state, 0x31, Register::Rdx, Register::Rdx);
            let jz_loc = matches!(src, Source::Reg(_)).then(|| {
                if is64 {
                    emit_alu64(state, 0x85, Register::Rcx, Register::Rcx);
                } else {
                    emit_alu32(state, 0x85, Register::Rcx, Register::Rcx);
                }
                // jz .mod_done (leave RAX = dividend)
                let loc = state.offset();
                emit1(state, 0x74);
                emit1(state, 0);
                loc
            });

            if is64 {
                emit_alu64_subop(state, 0xf7, 6, Register::Rcx);
                emit_mov(state, Register::Rdx, Register::Rax);
            } else {
                emit_alu32_subop(state, 0xf7, 6, Register::Rcx);
                emit_mov32(state, Register::Rdx, Register::Rax);
            }

            if let Some(jz_loc) = jz_loc {
                // .mod_done:
                state.buf[jz_loc + 1] = (state.offset() - (jz_loc + 2)) as u8;
            }
            emit_mov(state, Register::Rax, dst);
        }
    }

    // 4. Restore RDX and RAX if they were not `dst`.
    if dst != Register::Rdx {
        emit_mov(state, Register::Rdi, Register::Rdx);
    }
    if dst != Register::Rax {
        emit_mov(state, Register::R11, Register::Rax);
    }
    Ok(())
}

/// Emit an atomic memory operation (`BPF_STX | BPF_ATOMIC | BPF_W` or `BPF_DW`).
fn emit_atomic(
    state: &mut JitState<'_>,
    is64: bool,
    op: AtomicOp,
    dst: Register,
    src: Register,
    offset: i16,
) {
    let w = u8::from(is64);
    let disp = i32::from(offset);

    match op {
        AtomicOp::Add { fetch: false } => {
            // lock add [dst + disp], src
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x01);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::Add { fetch: true } => {
            // lock xadd [dst + disp], src (writes old value to `src`, zero-extending if 32-bit)
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x0f);
            emit1(state, 0xc1);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::And { fetch: false } => {
            // lock and [dst + disp], src
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x21);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::Or { fetch: false } => {
            // lock or [dst + disp], src
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x09);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::Xor { fetch: false } => {
            // lock xor [dst + disp], src
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x31);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::Xchg => {
            // lock xchg [dst + disp], src (zero-extends `src` if 32-bit)
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x87);
            emit_modrm_and_displacement(state, src, dst, disp);
        }
        AtomicOp::Cmpxchg => {
            // lock cmpxchg [dst + disp], src (implicitly compares with RAX/EAX and writes old
            // value to RAX/EAX)
            emit1(state, 0xf0);
            emit_basic_rex(state, w, src, dst);
            emit1(state, 0x0f);
            emit1(state, 0xb1);
            emit_modrm_and_displacement(state, src, dst, disp);
            if !is64 {
                // On 32-bit CMPXCHG hit (ZF=1), x86-64 does not write EAX, so zero-extend
                // EAX -> RAX.
                emit_mov32(state, Register::Rax, Register::Rax);
            }
        }
        AtomicOp::And { fetch: true }
        | AtomicOp::Or { fetch: true }
        | AtomicOp::Xor { fetch: true } => {
            let alu_opcode = match op {
                AtomicOp::And { .. } => 0x21,
                AtomicOp::Or { .. } => 0x09,
                AtomicOp::Xor { .. } => 0x31,
                _ => unreachable!(),
            };

            // Save RAX into R11 before modifying RAX; if the base pointer `dst` is RAX, copy it
            // into RDI first so `addr_reg` stays intact across the CAS loop.
            emit_mov(state, Register::Rax, Register::R11);
            let addr_reg = if dst == Register::Rax {
                emit_mov(state, dst, Register::Rdi);
                Register::Rdi
            } else {
                dst
            };

            // Load initial `*[addr_reg + disp]` into RAX/EAX.
            emit_load(
                state,
                if is64 { OperandSize::S64 } else { OperandSize::S32 },
                addr_reg,
                Register::Rax,
                disp,
            );

            let operand_reg = if src == Register::Rax { Register::R11 } else { src };
            let loop_start = state.offset();
            // RCX = RAX (old_val)
            emit_mov(state, Register::Rax, Register::Rcx);
            // RCX = RCX <op> operand_reg
            if is64 {
                emit_alu64(state, alu_opcode, operand_reg, Register::Rcx);
            } else {
                emit_alu32(state, alu_opcode, operand_reg, Register::Rcx);
            }
            // lock cmpxchg [addr_reg + disp], rcx (updates RAX on failure)
            emit1(state, 0xf0);
            emit_basic_rex(state, w, Register::Rcx, addr_reg);
            emit1(state, 0x0f);
            emit1(state, 0xb1);
            emit_modrm_and_displacement(state, Register::Rcx, addr_reg, disp);
            // jne loop_start
            let rel = (loop_start as isize) - ((state.offset() as isize) + 2);
            debug_assert!((-128..=127).contains(&rel));
            emit1(state, 0x75);
            emit1(state, rel as i8 as u8);

            // Store fetched `old_val` (in RAX) into `src` (zero-extended if 32-bit).
            if is64 {
                emit_mov(state, Register::Rax, src);
            } else {
                emit_mov32(state, Register::Rax, src);
            }
            // Restore RAX if `src` was not R0 (RAX).
            if src != Register::Rax {
                emit_mov(state, Register::R11, Register::Rax);
            }
        }
    }
}

impl JitState<'_> {
    fn emit_mul(&mut self, is64: bool, dst: BpfReg, src: Source) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let w = u8::from(is64);
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_basic_rex(self, w, dst, src);
                emit1(self, 0x0f);
                emit1(self, 0xaf);
                emit_modrm_reg2reg(self, dst, src);
            }
            Source::Value(imm) => {
                emit_basic_rex(self, w, dst, dst);
                emit1(self, 0x69);
                emit_modrm_reg2reg(self, dst, dst);
                emit4(self, imm as u32);
            }
        }
        Ok(())
    }

    fn emit_alu_binop(
        &mut self,
        is64: bool,
        reg_op: u8,
        imm_subop: u8,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                if is64 {
                    emit_alu64(self, reg_op, src, dst);
                } else {
                    emit_alu32(self, reg_op, src, dst);
                }
            }
            Source::Value(imm) => {
                if is64 {
                    emit_alu64_imm32(self, 0x81, imm_subop, dst, imm as i32);
                } else {
                    emit_alu32_imm32(self, 0x81, imm_subop, dst, imm as i32);
                }
            }
        }
        Ok(())
    }

    fn emit_shift(
        &mut self,
        is64: bool,
        subop: u8,
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_mov(self, src, Register::Rcx);
                if is64 {
                    emit_alu64_subop(self, 0xd3, subop, dst);
                } else {
                    emit_alu32_subop(self, 0xd3, subop, dst);
                }
            }
            Source::Value(imm) => {
                let mask = if is64 { 63 } else { 31 };
                let shift = ((imm as u32) & mask) as i8;
                if is64 {
                    emit_alu64_imm8(self, 0xc1, subop, dst, shift);
                } else {
                    emit_alu32_imm8(self, 0xc1, subop, dst, shift);
                }
            }
        }
        Ok(())
    }

    fn emit_cond_jump(
        &mut self,
        is64: bool,
        jcc_code: u8,
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                if is64 {
                    emit_cmp(self, src, dst);
                } else {
                    emit_cmp32(self, src, dst);
                }
            }
            Source::Value(imm) => {
                if is64 {
                    emit_cmp_imm32(self, dst, imm as i32);
                } else {
                    emit_cmp32_imm32(self, dst, imm as i32);
                }
            }
        }
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_jcc(self, jcc_code, target_pc);
        Ok(())
    }

    fn emit_jset(
        &mut self,
        is64: bool,
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                if is64 {
                    emit_alu64(self, 0x85, src, dst);
                } else {
                    emit_alu32(self, 0x85, src, dst);
                }
            }
            Source::Value(imm) => {
                if is64 {
                    emit_alu64_imm32(self, 0xf7, 0, dst, imm as i32);
                } else {
                    emit_alu32_imm32(self, 0xf7, 0, dst, imm as i32);
                }
            }
        }
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_jcc(self, 0x85, target_pc);
        Ok(())
    }

    fn emit_bpf_atomic(
        &mut self,
        is64: bool,
        op: AtomicOp,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        let src = map_register(src)?;
        emit_atomic(self, is64, op, dst, src, offset);
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
        self.emit_alu_binop(false, 0x01, 0, dst, src)
    }

    fn add64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(true, 0x01, 0, dst, src)
    }

    fn and(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(false, 0x21, 4, dst, src)
    }

    fn and64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(true, 0x21, 4, dst, src)
    }

    fn arsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(false, 7, dst, src)
    }

    fn arsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(true, 7, dst, src)
    }

    fn div(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        emit_divmod(self, false, DivModOp::Div, dst, src)
    }

    fn div64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        emit_divmod(self, true, DivModOp::Div, dst, src)
    }

    fn lsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(false, 4, dst, src)
    }

    fn lsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(true, 4, dst, src)
    }

    fn r#mod(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        emit_divmod(self, false, DivModOp::Mod, dst, src)
    }

    fn mod64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        emit_divmod(self, true, DivModOp::Mod, dst, src)
    }

    fn mov(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_mov32(self, src, dst);
            }
            Source::Value(imm) => {
                emit_alu32_imm32(self, 0xc7, 0, dst, imm as i32);
            }
        }
        Ok(())
    }

    fn mov64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_mov(self, src, dst);
            }
            Source::Value(imm) => {
                emit_load_imm(self, dst, imm as i64);
            }
        }
        Ok(())
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
        self.emit_alu_binop(false, 0x09, 1, dst, src)
    }

    fn or64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(true, 0x09, 1, dst, src)
    }

    fn rsh(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(false, 5, dst, src)
    }

    fn rsh64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_shift(true, 5, dst, src)
    }

    fn sub(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(false, 0x29, 5, dst, src)
    }

    fn sub64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(true, 0x29, 5, dst, src)
    }

    fn xor(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(false, 0x31, 6, dst, src)
    }

    fn xor64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_alu_binop(true, 0x31, 6, dst, src)
    }

    fn neg(&mut self, _context: &mut (), dst: BpfReg) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        emit_alu32_subop(self, 0xf7, 3, dst);
        Ok(())
    }

    fn neg64(&mut self, _context: &mut (), dst: BpfReg) -> Result<(), VerifierErrorKind> {
        let dst = map_register(dst)?;
        emit_alu64_subop(self, 0xf7, 3, dst);
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
                emit1(self, 0x66);
                emit_alu32_imm8(self, 0xc1, 0, dst, 8);
                emit_movzx16(self, dst, dst);
            }
            DataWidth::U32 => {
                emit_rex_rm(self, 0, dst);
                emit1(self, 0x0f);
                emit1(self, 0xc8 | dst.low3());
            }
            DataWidth::U64 => {
                emit_rex_rm(self, 1, dst);
                emit1(self, 0x0f);
                emit1(self, 0xc8 | dst.low3());
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
            DataWidth::U16 => emit_movzx16(self, dst, dst),
            DataWidth::U32 => emit_mov32(self, dst, dst),
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

        // Pass `run_context` in RDI and eBPF R3 (R10) in RCX; RSI, RDX, R8, R9 already
        // hold R1, R2, R4, R5.
        emit_mov(self, Register::R12, Register::Rdi);
        emit_mov(self, Register::R10, Register::Rcx);
        emit_load_imm(self, Register::Rax, fn_addr as i64);
        emit1(self, 0xff);
        emit1(self, 0xd0);
        // Return value is already in RAX (eBPF R0).
        Ok(())
    }

    fn exit(&mut self, _context: &mut ()) -> Result<(), VerifierErrorKind> {
        emit_jmp(self, TARGET_PC_EXIT);
        Ok(())
    }

    fn jump(&mut self, _context: &mut (), offset: i16) -> Result<(), VerifierErrorKind> {
        let target_pc = resolve_jump_target(self.pc, offset, self.code.len())?;
        emit_jmp(self, target_pc);
        Ok(())
    }

    fn jeq(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x84, dst, src, offset)
    }

    fn jeq64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x84, dst, src, offset)
    }

    fn jne(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x85, dst, src, offset)
    }

    fn jne64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x85, dst, src, offset)
    }

    fn jge(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x83, dst, src, offset)
    }

    fn jge64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x83, dst, src, offset)
    }

    fn jgt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x87, dst, src, offset)
    }

    fn jgt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x87, dst, src, offset)
    }

    fn jle(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x86, dst, src, offset)
    }

    fn jle64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x86, dst, src, offset)
    }

    fn jlt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x82, dst, src, offset)
    }

    fn jlt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x82, dst, src, offset)
    }

    fn jsge(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x8d, dst, src, offset)
    }

    fn jsge64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x8d, dst, src, offset)
    }

    fn jsgt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x8f, dst, src, offset)
    }

    fn jsgt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x8f, dst, src, offset)
    }

    fn jsle(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x8e, dst, src, offset)
    }

    fn jsle64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x8e, dst, src, offset)
    }

    fn jslt(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(false, 0x8c, dst, src, offset)
    }

    fn jslt64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_cond_jump(true, 0x8c, dst, src, offset)
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
        self.emit_bpf_atomic(false, AtomicOp::Add { fetch }, dst, offset, src)
    }

    fn atomic_add64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::Add { fetch }, dst, offset, src)
    }

    fn atomic_and(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(false, AtomicOp::And { fetch }, dst, offset, src)
    }

    fn atomic_and64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::And { fetch }, dst, offset, src)
    }

    fn atomic_or(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(false, AtomicOp::Or { fetch }, dst, offset, src)
    }

    fn atomic_or64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::Or { fetch }, dst, offset, src)
    }

    fn atomic_xor(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(false, AtomicOp::Xor { fetch }, dst, offset, src)
    }

    fn atomic_xor64(
        &mut self,
        _context: &mut (),
        fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::Xor { fetch }, dst, offset, src)
    }

    fn atomic_xchg(
        &mut self,
        _context: &mut (),
        _fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(false, AtomicOp::Xchg, dst, offset, src)
    }

    fn atomic_xchg64(
        &mut self,
        _context: &mut (),
        _fetch: bool,
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::Xchg, dst, offset, src)
    }

    fn atomic_cmpxchg(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(false, AtomicOp::Cmpxchg, dst, offset, src)
    }

    fn atomic_cmpxchg64(
        &mut self,
        _context: &mut (),
        dst: BpfReg,
        offset: i16,
        src: BpfReg,
    ) -> Result<(), VerifierErrorKind> {
        self.emit_bpf_atomic(true, AtomicOp::Cmpxchg, dst, offset, src)
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
        emit_load(self, OperandSize::from(width), src, dst, i32::from(offset));
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
        emit_load_imm(self, dst, imm64 as i64);
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

        // If `src_reg` happens to be `RDX`, stash it in `R11` before writing `reg_offset` into
        // `RDX`.
        let skb_reg = if src_reg == Register::Rdx {
            emit_mov(self, src_reg, Register::R11);
            Register::R11
        } else {
            src_reg
        };

        // Set RDX (`reg_offset`, 3rd argument) FIRST in case `register_offset` is in R1..R5.
        if let Some(off_reg) = register_offset {
            let off_reg = map_register(off_reg)?;
            emit_mov32(self, off_reg, Register::Rdx);
        } else {
            emit_alu32(self, 0x31, Register::Rdx, Register::Rdx);
        }

        // RDI (1st arg) = run_context (R12)
        emit_mov(self, Register::R12, Register::Rdi);
        // RSI (2nd arg) = skb pointer (`src_reg`, normally eBPF R6 -> RBX)
        emit_mov(self, skb_reg, Register::Rsi);
        // RCX (4th arg) = imm_offset
        emit_alu32_imm32(self, 0xc7, 0, Register::Rcx, offset);
        // R8 (5th arg) = width_bytes
        emit_alu32_imm32(self, 0xc7, 0, Register::R8, width_bytes as i32);
        // R9 (6th arg) = &out_value at [RBP + PACKET_LOAD_OUT_OFFSET] (`PACKET_LOAD_OUT_OFFSET = 0`)
        emit_mov(self, Register::Rbp, Register::R9);

        // Call packet_load_fn
        emit_load_imm(self, Register::Rax, fn_ptr as i64);
        emit1(self, 0xff);
        emit1(self, 0xd0);

        // Test return status (RAX == 0 means out-of-bounds); `mov dst, [rbp + 0]` does not affect
        // ZF, and on OOB `*out_value` is 0 (`RAX` is also already 0).
        emit_alu64(self, 0x85, Register::Rax, Register::Rax);
        emit_load(self, OperandSize::S64, Register::Rbp, dst, PACKET_LOAD_OUT_OFFSET);
        emit_jcc(self, 0x84, TARGET_PC_EXIT);

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
        let size = OperandSize::from(width);
        match src {
            Source::Reg(src_reg) => {
                let src = map_register(src_reg)?;
                emit_store(self, size, src, dst, i32::from(offset));
            }
            Source::Value(imm) => {
                emit_store_imm32(self, size, dst, i32::from(offset), imm as i32);
            }
        }
        Ok(())
    }
}

/// Compile a relocated slice of eBPF instructions into x86-64 machine code.
pub fn compile(
    insts: &[EbpfInstruction],
    helpers: &[HelperLinkage],
    packet_load_fn: Option<u64>,
) -> Result<Vec<u8>, JitError> {
    let mut state = JitState::new(insts, helpers, packet_load_fn);

    // Prologue:
    // 1. Save callee-saved registers (6 * 8 = 48 bytes; RSP % 16 == 8).
    for &reg in &CALLEE_SAVED_REGISTERS {
        emit_push(&mut state, reg);
    }
    // 2. Preserve `run_context` (RDI) in R12 and move `arg3` (RCX) into R10 (eBPF R3).
    emit_mov(&mut state, Register::Rdi, Register::R12);
    emit_mov(&mut state, Register::Rcx, Register::R10);
    // 3. Allocate 8 bytes (aligns RSP to 16 bytes and reserves [RBP + 0] for packet_load_fn).
    emit_alu64_imm32(&mut state, 0x81, 5, Register::Rsp, STACK_SCRATCH_AND_ALIGN_SIZE);
    // 4. Set eBPF frame pointer R10 (RBP) = RSP.
    emit_mov(&mut state, Register::Rsp, Register::Rbp);
    // 5. Allocate 512-byte eBPF stack (keeps RSP 16-byte aligned before any `call`).
    emit_alu64_imm32(&mut state, 0x81, 5, Register::Rsp, BPF_STACK_SIZE as i32);

    while state.pc < insts.len() {
        let pc_loc = u32::try_from(state.offset()).map_err(|_| JitError::CodeBufferOverflow)?;
        state.pc_locs[state.pc] = Some(pc_loc);
        state.visit(&mut (), insts[state.pc])?;
        state.pc += 1;
    }

    // Epilogue:
    state.exit_loc = u32::try_from(state.offset()).map_err(|_| JitError::CodeBufferOverflow)?;
    // R0 is already mapped to RAX.
    // Restore RSP from RBP (eBPF R10), deallocate 8-byte alignment/scratch space, and pop
    // callee-saved registers.
    emit_mov(&mut state, Register::Rbp, Register::Rsp);
    emit_alu64_imm32(&mut state, 0x81, 0, Register::Rsp, STACK_SCRATCH_AND_ALIGN_SIZE);
    for &reg in CALLEE_SAVED_REGISTERS.iter().rev() {
        emit_pop(&mut state, reg);
    }
    emit1(&mut state, 0xc3);

    // Patch relative jump targets:
    for jump in &state.jumps {
        let target_loc =
            resolve_patched_target_loc(jump.target_pc, state.exit_loc, &state.pc_locs)?;
        let offset_end = i64::try_from(jump.offset_loc)
            .ok()
            .and_then(|loc| loc.checked_add(4))
            .ok_or(JitError::CodeBufferOverflow)?;
        let displacement = i64::from(target_loc) - offset_end;
        let rel = i32::try_from(displacement).map_err(|_| {
            JitError::BranchDisplacementOutOfRange { offset: jump.offset_loc, displacement }
        })?;
        state.buf[jump.offset_loc..jump.offset_loc + 4].copy_from_slice(&rel.to_le_bytes());
    }

    Ok(state.buf)
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
        let code = compile(&insts, &[], None).expect("compile");
        assert_eq!(*code.last().unwrap(), 0xc3); // ret
    }

    #[test]
    fn test_immediate_div_mod_omits_zero_divisor_check() {
        // Immediate `div`/`mod` (both 64-bit and 32-bit) have non-zero divisors enforced by
        // `BpfVisitor::div_imm` and should not emit `test rcx, rcx` (`48 85 c9`) or
        // `test ecx, ecx` (`85 c9`).
        for asm in ["div %r0, 3\n", "div32 %r0, 3\n", "mod %r0, 3\n", "mod32 %r0, 3\n"] {
            let inst = parse_asm(asm)[0];
            let mut state = JitState::new(&[], &[], None);
            state.visit(&mut (), inst).expect("visit immediate div/mod");
            assert!(
                !state.buf.windows(2).any(|w| w == [0x85, 0xc9]),
                "Unexpected `test rcx/ecx, rcx/ecx` for {asm}: {:?}",
                state.buf
            );
        }

        // Register `div`/`mod` must emit `test rcx, rcx` (`48 85 c9`) or `test ecx, ecx` (`85 c9`).
        for (asm, expected) in [
            ("div %r0, %r1\n", &[0x48u8, 0x85, 0xc9][..]),
            ("div32 %r0, %r1\n", &[0x85, 0xc9][..]),
            ("mod %r0, %r1\n", &[0x48, 0x85, 0xc9][..]),
            ("mod32 %r0, %r1\n", &[0x85, 0xc9][..]),
        ] {
            let inst = parse_asm(asm)[0];
            let mut state = JitState::new(&[], &[], None);
            state.visit(&mut (), inst).expect("visit register div/mod");
            assert!(
                state.buf.windows(expected.len()).any(|w| w == expected),
                "Expected {expected:?} for {asm}: {:?}",
                state.buf
            );
        }

        // Zero immediate divisor must be rejected with `DivByZero`.
        for asm in ["div %r0, 0\n", "div32 %r0, 0\n", "mod %r0, 0\n", "mod32 %r0, 0\n"] {
            let inst = parse_asm(asm)[0];
            let mut state = JitState::new(&[], &[], None);
            assert_eq!(state.visit(&mut (), inst), Err(VerifierErrorKind::DivByZero));
        }
    }

    #[test]
    fn test_atomic_fetch_bitwise_rax_and_non_rax_src() {
        // 1. When `src_reg = 0` (`RAX`), `emit_atomic` must read the saved operand from `R11`
        // (`operand_reg = R11`, `REX.R = 1`) and must NOT restore `R11 -> RAX` at the end.
        for (asm, alu_op) in [
            ("lock fetch and [%r10-8], %r0\n", 0x21u8),
            ("lock fetch or [%r10-8], %r0\n", 0x09u8),
            ("lock fetch xor [%r10-8], %r0\n", 0x31u8),
        ] {
            let inst = parse_asm(asm)[0];
            let mut state = JitState::new(&[], &[], None);
            state.visit(&mut (), inst).expect("visit 64-bit fetch bitwise with %r0");
            // `alu rcx, r11` is encoded as `[0x4c, alu_op, 0xd9]`.
            assert!(
                state.buf.windows(3).any(|w| w == [0x4c, alu_op, 0xd9]),
                "Expected `op rcx, r11` in {:?}",
                state.buf
            );
            // Trailing `mov rax, r11` (`[0x4c, 0x89, 0xd8]`) must NOT be emitted when `src == RAX`.
            assert!(!state.buf.ends_with(&[0x4c, 0x89, 0xd8]));
        }

        // 2. When `src_reg = 1` (`RSI`), `emit_atomic` uses `operand_reg = RSI` and restores
        // `mov rax, r11` (`[0x4c, 0x89, 0xd8]`) at the end.
        for (asm, alu_op) in [
            ("lock fetch and32 [%r10-8], %r1\n", 0x21u8),
            ("lock fetch or32 [%r10-8], %r1\n", 0x09u8),
            ("lock fetch xor32 [%r10-8], %r1\n", 0x31u8),
        ] {
            let inst = parse_asm(asm)[0];
            let mut state = JitState::new(&[], &[], None);
            state.visit(&mut (), inst).expect("visit 32-bit fetch bitwise with %r1");
            // `alu ecx, esi` is encoded as `[alu_op, 0xf1]`.
            assert!(
                state.buf.windows(2).any(|w| w == [alu_op, 0xf1]),
                "Expected `op ecx, esi` in {:?}",
                state.buf
            );
            // When `dst != RAX`, `dst` must be addressed directly without `mov rdi, rbp`
            // (`[0x48, 0x89, 0xef]`).
            assert!(!state.buf.windows(3).any(|w| w == [0x48, 0x89, 0xef]));
            assert!(state.buf.ends_with(&[0x4c, 0x89, 0xd8]));
        }

        // 3. When `dst_reg = 0` (`RAX`), `emit_atomic` copies `RAX` into `RDI` (`[0x48, 0x89, 0xc7]`)
        // before loading `[rdi - 8]` into `RAX`.
        let inst_dst_r0 = parse_asm("lock fetch and [%r0-8], %r1\n")[0];
        let mut state_dst_r0 = JitState::new(&[], &[], None);
        state_dst_r0.visit(&mut (), inst_dst_r0).expect("visit fetch and [%r0-8], %r1");
        assert!(state_dst_r0.buf.windows(3).any(|w| w == [0x48, 0x89, 0xc7]));
        assert!(state_dst_r0.buf.ends_with(&[0x4c, 0x89, 0xd8]));
    }

    #[test]
    fn test_imul_register_and_immediate_encodings() {
        let mul64_reg = parse_asm("mul %r0, %r1\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), mul64_reg).expect("mul %r0, %r1");
        assert_eq!(state.buf, [0x48, 0x0f, 0xaf, 0xc6]); // imul rax, rsi

        let mul32_reg = parse_asm("mul32 %r1, %r2\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), mul32_reg).expect("mul32 %r1, %r2");
        assert_eq!(state.buf, [0x0f, 0xaf, 0xf2]); // imul esi, edx

        let mul64_imm = parse_asm("mul %r0, -5\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), mul64_imm).expect("mul %r0, -5");
        assert_eq!(state.buf, [0x48, 0x69, 0xc0, 0xfb, 0xff, 0xff, 0xff]); // imul rax, rax, -5

        let mul32_imm = parse_asm("mul32 %r3, 7\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), mul32_imm).expect("mul32 %r3, 7");
        assert_eq!(state.buf, [0x45, 0x69, 0xd2, 0x07, 0x00, 0x00, 0x00]); // imul r10d, r10d, 7
    }

    #[test]
    fn test_u16_endian_uses_movzx() {
        let le16_r0 = parse_asm("le16 %r0\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), le16_r0).expect("le16 %r0");
        assert_eq!(state.buf, [0x0f, 0xb7, 0xc0]); // movzx eax, ax

        let le16_r3 = parse_asm("le16 %r3\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), le16_r3).expect("le16 %r3");
        assert_eq!(state.buf, [0x45, 0x0f, 0xb7, 0xd2]); // movzx r10d, r10w

        let be16_r0 = parse_asm("be16 %r0\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), be16_r0).expect("be16 %r0");
        assert_eq!(state.buf, [0x66, 0xc1, 0xc0, 0x08, 0x0f, 0xb7, 0xc0]); // rol ax, 8; movzx eax, ax
    }

    #[test]
    fn test_byte_store_sil_and_bpl_emit_mandatory_rex() {
        // Storing low byte of `%r1` (`RSI` -> `SIL`) or `%r10` (`RBP` -> `BPL`) requires an
        // explicit `0x40` REX prefix on x86-64 so the CPU does not address `DH` or `CH`.
        let stxb_r1 = parse_asm("stxb [%r10-8], %r1\n")[0];
        let mut state_r1 = JitState::new(&[], &[], None);
        state_r1.visit(&mut (), stxb_r1).expect("stxb %r1");
        assert_eq!(state_r1.buf, [0x40, 0x88, 0x75, 0xf8]); // mov [rbp - 8], sil

        let stxb_r10 = parse_asm("stxb [%r1], %r10\n")[0];
        let mut state_r10 = JitState::new(&[], &[], None);
        state_r10.visit(&mut (), stxb_r10).expect("stxb %r10");
        assert_eq!(state_r10.buf, [0x40, 0x88, 0x2e]); // mov [rsi], bpl

        // By contrast, `%r0` (`RAX` -> `AL`) and `%r6` (`RBX` -> `BL`) do not need a `0x40` REX
        // prefix when `!dst.is_ext()`.
        let stxb_r0 = parse_asm("stxb [%r1], %r0\n")[0];
        let mut state_r0 = JitState::new(&[], &[], None);
        state_r0.visit(&mut (), stxb_r0).expect("stxb %r0");
        assert_eq!(state_r0.buf, [0x88, 0x06]); // mov [rsi], al

        let stxb_r6 = parse_asm("stxb [%r1], %r6\n")[0];
        let mut state_r6 = JitState::new(&[], &[], None);
        state_r6.visit(&mut (), stxb_r6).expect("stxb %r6");
        assert_eq!(state_r6.buf, [0x88, 0x1e]); // mov [rsi], bl
    }

    #[test]
    fn test_zero_offset_rbp_and_r13_emit_disp8_zero() {
        // Base registers `%r7` (`R13`, `r/m & 7 == 5`) and `%r10` (`RBP`, `r/m & 7 == 5`) cannot
        // use `mod = 0b00` without displacement (which encodes `[rip + disp32]`), so `offset = 0`
        // must emit `mod = 0b01` (`0x45`) + `disp8 = 0x00`.
        let ld_r7 = parse_asm("ldxdw %r0, [%r7]\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), ld_r7).expect("ldxdw [%r7]");
        assert_eq!(state.buf, [0x49, 0x8b, 0x45, 0x00]); // mov rax, [r13 + 0]

        let st_r7 = parse_asm("stxdw [%r7], %r0\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), st_r7).expect("stxdw [%r7]");
        assert_eq!(state.buf, [0x49, 0x89, 0x45, 0x00]); // mov [r13 + 0], rax

        let ld_r10 = parse_asm("ldxdw %r0, [%r10]\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), ld_r10).expect("ldxdw [%r10]");
        assert_eq!(state.buf, [0x48, 0x8b, 0x45, 0x00]); // mov rax, [rbp + 0]

        let st_r10 = parse_asm("stxdw [%r10], %r0\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), st_r10).expect("stxdw [%r10]");
        assert_eq!(state.buf, [0x48, 0x89, 0x45, 0x00]); // mov [rbp + 0], rax

        // By contrast, `%r1` (`RSI`) with `offset = 0` uses `mod = 0b00` with no displacement byte.
        let ld_r1 = parse_asm("ldxdw %r0, [%r1]\n")[0];
        let mut state = JitState::new(&[], &[], None);
        state.visit(&mut (), ld_r1).expect("ldxdw [%r1]");
        assert_eq!(state.buf, [0x48, 0x8b, 0x06]); // mov rax, [rsi]
    }
}
