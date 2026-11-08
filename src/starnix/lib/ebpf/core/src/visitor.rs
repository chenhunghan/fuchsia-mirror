// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::api::{
    BPF_ABS, BPF_ADD, BPF_ALU, BPF_ALU64, BPF_AND, BPF_ARSH, BPF_ATOMIC, BPF_B, BPF_CALL,
    BPF_CMPXCHG, BPF_DIV, BPF_DW, BPF_END, BPF_EXIT, BPF_FETCH, BPF_H, BPF_IND, BPF_JA, BPF_JEQ,
    BPF_JGE, BPF_JGT, BPF_JLE, BPF_JLT, BPF_JMP, BPF_JMP32, BPF_JNE, BPF_JSET, BPF_JSGE, BPF_JSGT,
    BPF_JSLE, BPF_JSLT, BPF_K, BPF_LD, BPF_LDDW, BPF_LDX, BPF_LSH, BPF_MEM, BPF_MOD, BPF_MOV,
    BPF_MUL, BPF_NEG, BPF_OR, BPF_RSH, BPF_ST, BPF_STX, BPF_SUB, BPF_TO_BE, BPF_TO_LE, BPF_W,
    BPF_X, BPF_XCHG, BPF_XOR, DataWidth, EbpfInstruction,
};
use crate::error::VerifierErrorKind;

/// The index into the registers. 10 is the stack pointer.
pub type Register = u8;

/// The index into the program.
pub type ProgramCounter = usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Reg(Register),
    Value(u64),
}

fn div_imm(imm: i32) -> Result<Source, VerifierErrorKind> {
    if imm == 0 { Err(VerifierErrorKind::DivByZero) } else { Ok(Source::Value(imm as u64)) }
}

fn shift_imm(imm: i32, max_bits: i32) -> Result<Source, VerifierErrorKind> {
    if imm < 0 || imm >= max_bits {
        Err(VerifierErrorKind::InvalidShift(imm))
    } else {
        Ok(Source::Value(imm as u64))
    }
}

fn width_from_imm(imm: i32) -> Result<DataWidth, VerifierErrorKind> {
    match imm {
        16 => Ok(DataWidth::U16),
        32 => Ok(DataWidth::U32),
        64 => Ok(DataWidth::U64),
        _ => Err(VerifierErrorKind::InvalidByteSwapWidth(imm)),
    }
}

pub trait BpfVisitor<C> {
    fn add(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn add64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn and(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn and64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn arsh(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn arsh64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn div(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn div64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn lsh(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn lsh64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn r#mod(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn mod64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn mov(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn mov64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn mul(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn mul64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn or(&mut self, context: &mut C, dst: Register, src: Source) -> Result<(), VerifierErrorKind>;
    fn or64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn rsh(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn rsh64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn sub(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn sub64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;
    fn xor(&mut self, context: &mut C, dst: Register, src: Source)
    -> Result<(), VerifierErrorKind>;
    fn xor64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind>;

    fn neg(&mut self, context: &mut C, dst: Register) -> Result<(), VerifierErrorKind>;
    fn neg64(&mut self, context: &mut C, dst: Register) -> Result<(), VerifierErrorKind>;

    fn be(
        &mut self,
        context: &mut C,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind>;
    fn le(
        &mut self,
        context: &mut C,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind>;

    fn call_external(&mut self, context: &mut C, index: u32) -> Result<(), VerifierErrorKind>;

    fn exit(&mut self, context: &mut C) -> Result<(), VerifierErrorKind>;

    fn jump(&mut self, context: &mut C, offset: i16) -> Result<(), VerifierErrorKind>;

    fn jeq(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jeq64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jne(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jne64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jge(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jge64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jgt(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jgt64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jle(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jle64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jlt(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jlt64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsge(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsge64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsgt(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsgt64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsle(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jsle64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jslt(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jslt64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jset(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;
    fn jset64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_add(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_add64(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_and(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_and64(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_or(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_or64(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_xor(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_xor64(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_xchg(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_xchg64(
        &mut self,
        context: &mut C,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_cmpxchg(
        &mut self,
        context: &mut C,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn atomic_cmpxchg64(
        &mut self,
        context: &mut C,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind>;

    fn load(
        &mut self,
        context: &mut C,
        dst: Register,
        offset: i16,
        src: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind>;

    fn load64(
        &mut self,
        context: &mut C,
        dst: Register,
        src: u8,
        lower: u32,
    ) -> Result<(), VerifierErrorKind>;

    fn load_from_packet(
        &mut self,
        context: &mut C,
        dst: Register,
        src: Register,
        offset: i32,
        register_offset: Option<Register>,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind>;

    fn store(
        &mut self,
        context: &mut C,
        dst: Register,
        offset: i16,
        src: Source,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind>;

    #[inline(always)]
    fn visit(&mut self, context: &mut C, inst: EbpfInstruction) -> Result<(), VerifierErrorKind> {
        let dst = inst.dst_reg();
        let src_imm = Source::Value(inst.imm() as u64);
        let src_reg = Source::Reg(inst.src_reg());

        const BPF_ALU_ADD_K: u8 = BPF_ALU | BPF_ADD | BPF_K;
        const BPF_ALU_ADD_X: u8 = BPF_ALU | BPF_ADD | BPF_X;
        const BPF_ALU_SUB_K: u8 = BPF_ALU | BPF_SUB | BPF_K;
        const BPF_ALU_SUB_X: u8 = BPF_ALU | BPF_SUB | BPF_X;
        const BPF_ALU_MUL_K: u8 = BPF_ALU | BPF_MUL | BPF_K;
        const BPF_ALU_MUL_X: u8 = BPF_ALU | BPF_MUL | BPF_X;
        const BPF_ALU_DIV_K: u8 = BPF_ALU | BPF_DIV | BPF_K;
        const BPF_ALU_DIV_X: u8 = BPF_ALU | BPF_DIV | BPF_X;
        const BPF_ALU_OR_K: u8 = BPF_ALU | BPF_OR | BPF_K;
        const BPF_ALU_OR_X: u8 = BPF_ALU | BPF_OR | BPF_X;
        const BPF_ALU_AND_K: u8 = BPF_ALU | BPF_AND | BPF_K;
        const BPF_ALU_AND_X: u8 = BPF_ALU | BPF_AND | BPF_X;
        const BPF_ALU_LSH_K: u8 = BPF_ALU | BPF_LSH | BPF_K;
        const BPF_ALU_LSH_X: u8 = BPF_ALU | BPF_LSH | BPF_X;
        const BPF_ALU_RSH_K: u8 = BPF_ALU | BPF_RSH | BPF_K;
        const BPF_ALU_RSH_X: u8 = BPF_ALU | BPF_RSH | BPF_X;
        const BPF_ALU_MOD_K: u8 = BPF_ALU | BPF_MOD | BPF_K;
        const BPF_ALU_MOD_X: u8 = BPF_ALU | BPF_MOD | BPF_X;
        const BPF_ALU_XOR_K: u8 = BPF_ALU | BPF_XOR | BPF_K;
        const BPF_ALU_XOR_X: u8 = BPF_ALU | BPF_XOR | BPF_X;
        const BPF_ALU_MOV_K: u8 = BPF_ALU | BPF_MOV | BPF_K;
        const BPF_ALU_MOV_X: u8 = BPF_ALU | BPF_MOV | BPF_X;
        const BPF_ALU_ARSH_K: u8 = BPF_ALU | BPF_ARSH | BPF_K;
        const BPF_ALU_ARSH_X: u8 = BPF_ALU | BPF_ARSH | BPF_X;
        const BPF_ALU_NEG: u8 = BPF_ALU | BPF_NEG;

        const BPF_ALU64_ADD_K: u8 = BPF_ALU64 | BPF_ADD | BPF_K;
        const BPF_ALU64_ADD_X: u8 = BPF_ALU64 | BPF_ADD | BPF_X;
        const BPF_ALU64_SUB_K: u8 = BPF_ALU64 | BPF_SUB | BPF_K;
        const BPF_ALU64_SUB_X: u8 = BPF_ALU64 | BPF_SUB | BPF_X;
        const BPF_ALU64_MUL_K: u8 = BPF_ALU64 | BPF_MUL | BPF_K;
        const BPF_ALU64_MUL_X: u8 = BPF_ALU64 | BPF_MUL | BPF_X;
        const BPF_ALU64_DIV_K: u8 = BPF_ALU64 | BPF_DIV | BPF_K;
        const BPF_ALU64_DIV_X: u8 = BPF_ALU64 | BPF_DIV | BPF_X;
        const BPF_ALU64_OR_K: u8 = BPF_ALU64 | BPF_OR | BPF_K;
        const BPF_ALU64_OR_X: u8 = BPF_ALU64 | BPF_OR | BPF_X;
        const BPF_ALU64_AND_K: u8 = BPF_ALU64 | BPF_AND | BPF_K;
        const BPF_ALU64_AND_X: u8 = BPF_ALU64 | BPF_AND | BPF_X;
        const BPF_ALU64_LSH_K: u8 = BPF_ALU64 | BPF_LSH | BPF_K;
        const BPF_ALU64_LSH_X: u8 = BPF_ALU64 | BPF_LSH | BPF_X;
        const BPF_ALU64_RSH_K: u8 = BPF_ALU64 | BPF_RSH | BPF_K;
        const BPF_ALU64_RSH_X: u8 = BPF_ALU64 | BPF_RSH | BPF_X;
        const BPF_ALU64_MOD_K: u8 = BPF_ALU64 | BPF_MOD | BPF_K;
        const BPF_ALU64_MOD_X: u8 = BPF_ALU64 | BPF_MOD | BPF_X;
        const BPF_ALU64_XOR_K: u8 = BPF_ALU64 | BPF_XOR | BPF_K;
        const BPF_ALU64_XOR_X: u8 = BPF_ALU64 | BPF_XOR | BPF_X;
        const BPF_ALU64_MOV_K: u8 = BPF_ALU64 | BPF_MOV | BPF_K;
        const BPF_ALU64_MOV_X: u8 = BPF_ALU64 | BPF_MOV | BPF_X;
        const BPF_ALU64_ARSH_K: u8 = BPF_ALU64 | BPF_ARSH | BPF_K;
        const BPF_ALU64_ARSH_X: u8 = BPF_ALU64 | BPF_ARSH | BPF_X;
        const BPF_ALU64_NEG: u8 = BPF_ALU64 | BPF_NEG;

        const BPF_ALU_END_TO_BE: u8 = BPF_ALU | BPF_END | BPF_TO_BE;
        const BPF_ALU_END_TO_LE: u8 = BPF_ALU | BPF_END | BPF_TO_LE;
        const BPF_JMP32_JEQ_K: u8 = BPF_JMP32 | BPF_JEQ | BPF_K;
        const BPF_JMP32_JEQ_X: u8 = BPF_JMP32 | BPF_JEQ | BPF_X;
        const BPF_JMP32_JGT_K: u8 = BPF_JMP32 | BPF_JGT | BPF_K;
        const BPF_JMP32_JGT_X: u8 = BPF_JMP32 | BPF_JGT | BPF_X;
        const BPF_JMP32_JGE_K: u8 = BPF_JMP32 | BPF_JGE | BPF_K;
        const BPF_JMP32_JGE_X: u8 = BPF_JMP32 | BPF_JGE | BPF_X;
        const BPF_JMP32_JSET_K: u8 = BPF_JMP32 | BPF_JSET | BPF_K;
        const BPF_JMP32_JSET_X: u8 = BPF_JMP32 | BPF_JSET | BPF_X;
        const BPF_JMP32_JNE_K: u8 = BPF_JMP32 | BPF_JNE | BPF_K;
        const BPF_JMP32_JNE_X: u8 = BPF_JMP32 | BPF_JNE | BPF_X;
        const BPF_JMP32_JSGT_K: u8 = BPF_JMP32 | BPF_JSGT | BPF_K;
        const BPF_JMP32_JSGT_X: u8 = BPF_JMP32 | BPF_JSGT | BPF_X;
        const BPF_JMP32_JSGE_K: u8 = BPF_JMP32 | BPF_JSGE | BPF_K;
        const BPF_JMP32_JSGE_X: u8 = BPF_JMP32 | BPF_JSGE | BPF_X;
        const BPF_JMP32_JLT_K: u8 = BPF_JMP32 | BPF_JLT | BPF_K;
        const BPF_JMP32_JLT_X: u8 = BPF_JMP32 | BPF_JLT | BPF_X;
        const BPF_JMP32_JLE_K: u8 = BPF_JMP32 | BPF_JLE | BPF_K;
        const BPF_JMP32_JLE_X: u8 = BPF_JMP32 | BPF_JLE | BPF_X;
        const BPF_JMP32_JSLT_K: u8 = BPF_JMP32 | BPF_JSLT | BPF_K;
        const BPF_JMP32_JSLT_X: u8 = BPF_JMP32 | BPF_JSLT | BPF_X;
        const BPF_JMP32_JSLE_K: u8 = BPF_JMP32 | BPF_JSLE | BPF_K;
        const BPF_JMP32_JSLE_X: u8 = BPF_JMP32 | BPF_JSLE | BPF_X;
        const BPF_JMP_JEQ_K: u8 = BPF_JMP | BPF_JEQ | BPF_K;
        const BPF_JMP_JEQ_X: u8 = BPF_JMP | BPF_JEQ | BPF_X;
        const BPF_JMP_JGT_K: u8 = BPF_JMP | BPF_JGT | BPF_K;
        const BPF_JMP_JGT_X: u8 = BPF_JMP | BPF_JGT | BPF_X;
        const BPF_JMP_JGE_K: u8 = BPF_JMP | BPF_JGE | BPF_K;
        const BPF_JMP_JGE_X: u8 = BPF_JMP | BPF_JGE | BPF_X;
        const BPF_JMP_JSET_K: u8 = BPF_JMP | BPF_JSET | BPF_K;
        const BPF_JMP_JSET_X: u8 = BPF_JMP | BPF_JSET | BPF_X;
        const BPF_JMP_JNE_K: u8 = BPF_JMP | BPF_JNE | BPF_K;
        const BPF_JMP_JNE_X: u8 = BPF_JMP | BPF_JNE | BPF_X;
        const BPF_JMP_JSGT_K: u8 = BPF_JMP | BPF_JSGT | BPF_K;
        const BPF_JMP_JSGT_X: u8 = BPF_JMP | BPF_JSGT | BPF_X;
        const BPF_JMP_JSGE_K: u8 = BPF_JMP | BPF_JSGE | BPF_K;
        const BPF_JMP_JSGE_X: u8 = BPF_JMP | BPF_JSGE | BPF_X;
        const BPF_JMP_JLT_K: u8 = BPF_JMP | BPF_JLT | BPF_K;
        const BPF_JMP_JLT_X: u8 = BPF_JMP | BPF_JLT | BPF_X;
        const BPF_JMP_JLE_K: u8 = BPF_JMP | BPF_JLE | BPF_K;
        const BPF_JMP_JLE_X: u8 = BPF_JMP | BPF_JLE | BPF_X;
        const BPF_JMP_JSLT_K: u8 = BPF_JMP | BPF_JSLT | BPF_K;
        const BPF_JMP_JSLT_X: u8 = BPF_JMP | BPF_JSLT | BPF_X;
        const BPF_JMP_JSLE_K: u8 = BPF_JMP | BPF_JSLE | BPF_K;
        const BPF_JMP_JSLE_X: u8 = BPF_JMP | BPF_JSLE | BPF_X;
        const BPF_JMP_JA: u8 = BPF_JMP | BPF_JA;
        const BPF_JMP_CALL: u8 = BPF_JMP | BPF_CALL;
        const BPF_JMP_EXIT: u8 = BPF_JMP | BPF_EXIT;

        const BPF_LD_B_ABS: u8 = BPF_LD | BPF_B | BPF_ABS;
        const BPF_LD_B_IND: u8 = BPF_LD | BPF_B | BPF_IND;
        const BPF_LD_H_ABS: u8 = BPF_LD | BPF_H | BPF_ABS;
        const BPF_LD_H_IND: u8 = BPF_LD | BPF_H | BPF_IND;
        const BPF_LD_W_ABS: u8 = BPF_LD | BPF_W | BPF_ABS;
        const BPF_LD_W_IND: u8 = BPF_LD | BPF_W | BPF_IND;
        const BPF_LD_DW_ABS: u8 = BPF_LD | BPF_DW | BPF_ABS;
        const BPF_LD_DW_IND: u8 = BPF_LD | BPF_DW | BPF_IND;
        const BPF_LDX_B_MEM: u8 = BPF_LDX | BPF_B | BPF_MEM;
        const BPF_LDX_H_MEM: u8 = BPF_LDX | BPF_H | BPF_MEM;
        const BPF_LDX_W_MEM: u8 = BPF_LDX | BPF_W | BPF_MEM;
        const BPF_LDX_DW_MEM: u8 = BPF_LDX | BPF_DW | BPF_MEM;
        const BPF_STX_B_MEM: u8 = BPF_STX | BPF_B | BPF_MEM;
        const BPF_STX_H_MEM: u8 = BPF_STX | BPF_H | BPF_MEM;
        const BPF_STX_W_MEM: u8 = BPF_STX | BPF_W | BPF_MEM;
        const BPF_STX_DW_MEM: u8 = BPF_STX | BPF_DW | BPF_MEM;
        const BPF_ST_B_MEM: u8 = BPF_ST | BPF_B | BPF_MEM;
        const BPF_ST_H_MEM: u8 = BPF_ST | BPF_H | BPF_MEM;
        const BPF_ST_W_MEM: u8 = BPF_ST | BPF_W | BPF_MEM;
        const BPF_ST_DW_MEM: u8 = BPF_ST | BPF_DW | BPF_MEM;
        const BPF_STX_ATOMIC_W: u8 = BPF_STX | BPF_ATOMIC | BPF_W;
        const BPF_STX_ATOMIC_DW: u8 = BPF_STX | BPF_ATOMIC | BPF_DW;

        const BPF_ADD_FETCH: u8 = BPF_ADD | BPF_FETCH;
        const BPF_AND_FETCH: u8 = BPF_AND | BPF_FETCH;
        const BPF_OR_FETCH: u8 = BPF_OR | BPF_FETCH;
        const BPF_XOR_FETCH: u8 = BPF_XOR | BPF_FETCH;

        match inst.code() {
            // 32-bit ALU instructions.
            BPF_ALU_ADD_K => self.add(context, dst, src_imm),
            BPF_ALU_ADD_X => self.add(context, dst, src_reg),
            BPF_ALU_SUB_K => self.sub(context, dst, src_imm),
            BPF_ALU_SUB_X => self.sub(context, dst, src_reg),
            BPF_ALU_MUL_K => self.mul(context, dst, src_imm),
            BPF_ALU_MUL_X => self.mul(context, dst, src_reg),
            BPF_ALU_DIV_K => match div_imm(inst.imm()) {
                Ok(src) => self.div(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU_DIV_X => self.div(context, dst, src_reg),
            BPF_ALU_OR_K => self.or(context, dst, src_imm),
            BPF_ALU_OR_X => self.or(context, dst, src_reg),
            BPF_ALU_AND_K => self.and(context, dst, src_imm),
            BPF_ALU_AND_X => self.and(context, dst, src_reg),
            BPF_ALU_LSH_K => match shift_imm(inst.imm(), 32) {
                Ok(src) => self.lsh(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU_LSH_X => self.lsh(context, dst, src_reg),
            BPF_ALU_RSH_K => match shift_imm(inst.imm(), 32) {
                Ok(src) => self.rsh(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU_RSH_X => self.rsh(context, dst, src_reg),
            BPF_ALU_MOD_K => match div_imm(inst.imm()) {
                Ok(src) => self.r#mod(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU_MOD_X => self.r#mod(context, dst, src_reg),
            BPF_ALU_XOR_K => self.xor(context, dst, src_imm),
            BPF_ALU_XOR_X => self.xor(context, dst, src_reg),
            BPF_ALU_MOV_K => self.mov(context, dst, src_imm),
            BPF_ALU_MOV_X => self.mov(context, dst, src_reg),
            BPF_ALU_ARSH_K => match shift_imm(inst.imm(), 32) {
                Ok(src) => self.arsh(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU_ARSH_X => self.arsh(context, dst, src_reg),
            BPF_ALU_NEG => self.neg(context, dst),

            // 64-bit ALU instructions.
            BPF_ALU64_ADD_K => self.add64(context, dst, src_imm),
            BPF_ALU64_ADD_X => self.add64(context, dst, src_reg),
            BPF_ALU64_SUB_K => self.sub64(context, dst, src_imm),
            BPF_ALU64_SUB_X => self.sub64(context, dst, src_reg),
            BPF_ALU64_MUL_K => self.mul64(context, dst, src_imm),
            BPF_ALU64_MUL_X => self.mul64(context, dst, src_reg),
            BPF_ALU64_DIV_K => match div_imm(inst.imm()) {
                Ok(src) => self.div64(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU64_DIV_X => self.div64(context, dst, src_reg),
            BPF_ALU64_OR_K => self.or64(context, dst, src_imm),
            BPF_ALU64_OR_X => self.or64(context, dst, src_reg),
            BPF_ALU64_AND_K => self.and64(context, dst, src_imm),
            BPF_ALU64_AND_X => self.and64(context, dst, src_reg),
            BPF_ALU64_LSH_K => match shift_imm(inst.imm(), 64) {
                Ok(src) => self.lsh64(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU64_LSH_X => self.lsh64(context, dst, src_reg),
            BPF_ALU64_RSH_K => match shift_imm(inst.imm(), 64) {
                Ok(src) => self.rsh64(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU64_RSH_X => self.rsh64(context, dst, src_reg),
            BPF_ALU64_MOD_K => match div_imm(inst.imm()) {
                Ok(src) => self.mod64(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU64_MOD_X => self.mod64(context, dst, src_reg),
            BPF_ALU64_XOR_K => self.xor64(context, dst, src_imm),
            BPF_ALU64_XOR_X => self.xor64(context, dst, src_reg),
            BPF_ALU64_MOV_K => self.mov64(context, dst, src_imm),
            BPF_ALU64_MOV_X => self.mov64(context, dst, src_reg),
            BPF_ALU64_ARSH_K => match shift_imm(inst.imm(), 64) {
                Ok(src) => self.arsh64(context, dst, src),
                Err(err) => Err(err),
            },
            BPF_ALU64_ARSH_X => self.arsh64(context, dst, src_reg),
            BPF_ALU64_NEG => self.neg64(context, dst),

            // Byte swap instruction.
            BPF_ALU_END_TO_BE => match width_from_imm(inst.imm()) {
                Ok(width) => self.be(context, dst, width),
                Err(err) => Err(err),
            },
            BPF_ALU_END_TO_LE => match width_from_imm(inst.imm()) {
                Ok(width) => self.le(context, dst, width),
                Err(err) => Err(err),
            },

            // 32-bit conditional jump.
            BPF_JMP32_JEQ_K => self.jeq(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JEQ_X => self.jeq(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JGT_K => self.jgt(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JGT_X => self.jgt(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JGE_K => self.jge(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JGE_X => self.jge(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JSET_K => self.jset(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JSET_X => self.jset(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JNE_K => self.jne(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JNE_X => self.jne(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JSGT_K => self.jsgt(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JSGT_X => self.jsgt(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JSGE_K => self.jsge(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JSGE_X => self.jsge(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JLT_K => self.jlt(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JLT_X => self.jlt(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JLE_K => self.jle(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JLE_X => self.jle(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JSLT_K => self.jslt(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JSLT_X => self.jslt(context, dst, src_reg, inst.offset()),
            BPF_JMP32_JSLE_K => self.jsle(context, dst, src_imm, inst.offset()),
            BPF_JMP32_JSLE_X => self.jsle(context, dst, src_reg, inst.offset()),

            // 64-bit conditional jump.
            BPF_JMP_JEQ_K => self.jeq64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JEQ_X => self.jeq64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JGT_K => self.jgt64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JGT_X => self.jgt64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JGE_K => self.jge64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JGE_X => self.jge64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JSET_K => self.jset64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JSET_X => self.jset64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JNE_K => self.jne64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JNE_X => self.jne64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JSGT_K => self.jsgt64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JSGT_X => self.jsgt64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JSGE_K => self.jsge64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JSGE_X => self.jsge64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JLT_K => self.jlt64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JLT_X => self.jlt64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JLE_K => self.jle64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JLE_X => self.jle64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JSLT_K => self.jslt64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JSLT_X => self.jslt64(context, dst, src_reg, inst.offset()),
            BPF_JMP_JSLE_K => self.jsle64(context, dst, src_imm, inst.offset()),
            BPF_JMP_JSLE_X => self.jsle64(context, dst, src_reg, inst.offset()),

            // Unconditional jump.
            BPF_JMP_JA => self.jump(context, inst.offset()),

            // Function call.
            BPF_JMP_CALL => match inst.src_reg() {
                0 => self.call_external(context, inst.imm() as u32),
                _ => Err(VerifierErrorKind::UnsupportedCallType(inst.src_reg())),
            },

            // Return.
            BPF_JMP_EXIT => self.exit(context),

            // 64-bit load.
            BPF_LDDW => self.load64(context, dst, inst.src_reg(), inst.imm() as u32),

            // Legacy packet access instructions.
            // All read the packet from R6, storing the result in R0.
            BPF_LD_B_ABS => self.load_from_packet(context, 0, 6, inst.imm(), None, DataWidth::U8),
            BPF_LD_B_IND => self.load_from_packet(
                context,
                0,
                6,
                inst.imm(),
                Some(inst.src_reg()),
                DataWidth::U8,
            ),
            BPF_LD_H_ABS => self.load_from_packet(context, 0, 6, inst.imm(), None, DataWidth::U16),
            BPF_LD_H_IND => self.load_from_packet(
                context,
                0,
                6,
                inst.imm(),
                Some(inst.src_reg()),
                DataWidth::U16,
            ),
            BPF_LD_W_ABS => self.load_from_packet(context, 0, 6, inst.imm(), None, DataWidth::U32),
            BPF_LD_W_IND => self.load_from_packet(
                context,
                0,
                6,
                inst.imm(),
                Some(inst.src_reg()),
                DataWidth::U32,
            ),
            BPF_LD_DW_ABS => self.load_from_packet(context, 0, 6, inst.imm(), None, DataWidth::U64),
            BPF_LD_DW_IND => self.load_from_packet(
                context,
                0,
                6,
                inst.imm(),
                Some(inst.src_reg()),
                DataWidth::U64,
            ),

            // Memory Load.
            BPF_LDX_B_MEM => self.load(context, dst, inst.offset(), inst.src_reg(), DataWidth::U8),
            BPF_LDX_H_MEM => self.load(context, dst, inst.offset(), inst.src_reg(), DataWidth::U16),
            BPF_LDX_W_MEM => self.load(context, dst, inst.offset(), inst.src_reg(), DataWidth::U32),
            BPF_LDX_DW_MEM => {
                self.load(context, dst, inst.offset(), inst.src_reg(), DataWidth::U64)
            }

            // Memory Store register.
            BPF_STX_B_MEM => self.store(context, dst, inst.offset(), src_reg, DataWidth::U8),
            BPF_STX_H_MEM => self.store(context, dst, inst.offset(), src_reg, DataWidth::U16),
            BPF_STX_W_MEM => self.store(context, dst, inst.offset(), src_reg, DataWidth::U32),
            BPF_STX_DW_MEM => self.store(context, dst, inst.offset(), src_reg, DataWidth::U64),

            // Memory Store constant.
            BPF_ST_B_MEM => self.store(context, dst, inst.offset(), src_imm, DataWidth::U8),
            BPF_ST_H_MEM => self.store(context, dst, inst.offset(), src_imm, DataWidth::U16),
            BPF_ST_W_MEM => self.store(context, dst, inst.offset(), src_imm, DataWidth::U32),
            BPF_ST_DW_MEM => self.store(context, dst, inst.offset(), src_imm, DataWidth::U64),

            // 32-bit atomic ops.
            BPF_STX_ATOMIC_W => {
                let operation = inst.imm() as u8;
                match operation {
                    BPF_ADD => self.atomic_add(context, false, dst, inst.offset(), inst.src_reg()),
                    BPF_ADD_FETCH => {
                        self.atomic_add(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_AND => self.atomic_and(context, false, dst, inst.offset(), inst.src_reg()),
                    BPF_AND_FETCH => {
                        self.atomic_and(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_OR => self.atomic_or(context, false, dst, inst.offset(), inst.src_reg()),
                    BPF_OR_FETCH => {
                        self.atomic_or(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_XOR => self.atomic_xor(context, false, dst, inst.offset(), inst.src_reg()),
                    BPF_XOR_FETCH => {
                        self.atomic_xor(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_XCHG => self.atomic_xchg(context, true, dst, inst.offset(), inst.src_reg()),
                    BPF_CMPXCHG => self.atomic_cmpxchg(context, dst, inst.offset(), inst.src_reg()),
                    _ => Err(VerifierErrorKind::InvalidAtomicOp(operation)),
                }
            }

            // 64-bit atomic ops.
            BPF_STX_ATOMIC_DW => {
                let operation = inst.imm() as u8;
                match operation {
                    BPF_ADD => {
                        self.atomic_add64(context, false, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_ADD_FETCH => {
                        self.atomic_add64(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_AND => {
                        self.atomic_and64(context, false, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_AND_FETCH => {
                        self.atomic_and64(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_OR => self.atomic_or64(context, false, dst, inst.offset(), inst.src_reg()),
                    BPF_OR_FETCH => {
                        self.atomic_or64(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_XOR => {
                        self.atomic_xor64(context, false, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_XOR_FETCH => {
                        self.atomic_xor64(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_XCHG => {
                        self.atomic_xchg64(context, true, dst, inst.offset(), inst.src_reg())
                    }
                    BPF_CMPXCHG => {
                        self.atomic_cmpxchg64(context, dst, inst.offset(), inst.src_reg())
                    }
                    _ => Err(VerifierErrorKind::InvalidAtomicOp(operation)),
                }
            }

            _ => Err(VerifierErrorKind::InvalidOpcode(inst.code())),
        }
    }
}
