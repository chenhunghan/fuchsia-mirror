// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use alloc::vec::Vec;

use crate::api::{
    BPF_A, BPF_ABS, BPF_ADD, BPF_ALU, BPF_ALU64, BPF_AND, BPF_B, BPF_DIV, BPF_EXIT, BPF_H, BPF_IMM,
    BPF_IND, BPF_JA, BPF_JEQ, BPF_JGE, BPF_JGT, BPF_JLE, BPF_JLT, BPF_JMP, BPF_JMP32, BPF_JNE,
    BPF_JSET, BPF_K, BPF_LD, BPF_LDX, BPF_LEN, BPF_LSH, BPF_MAXINSNS, BPF_MEM, BPF_MISC, BPF_MOD,
    BPF_MOV, BPF_MSH, BPF_MUL, BPF_NEG, BPF_OR, BPF_RET, BPF_RSH, BPF_ST, BPF_STX, BPF_SUB,
    BPF_TAX, BPF_TXA, BPF_W, BPF_X, BPF_XOR, EbpfInstruction, SockFilter,
};
use crate::error::CbpfError;

const CBPF_WORD_SIZE: u32 = 4;

// cBPF supports 16 words for scratch memory.
const CBPF_SCRATCH_SIZE: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CbpfLenInstruction {
    Static { len: i32 },
    ContextField { offset: i16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CbpfConfig {
    pub len: CbpfLenInstruction,
    pub allow_msh: bool,
}

pub const fn sock_bpf_class(filter: &SockFilter) -> u8 {
    (filter.code & 0x07) as u8
}

pub const fn sock_bpf_size(filter: &SockFilter) -> u8 {
    (filter.code & 0x18) as u8
}

pub const fn sock_bpf_addressing_mode(filter: &SockFilter) -> u8 {
    (filter.code & 0xe0) as u8
}

pub const fn sock_bpf_op(filter: &SockFilter) -> u8 {
    (filter.code & 0xf0) as u8
}

/// The source for the operation (either a register or an immediate).
fn sock_bpf_src(filter: &SockFilter) -> u8 {
    (filter.code & 0x08) as u8
}

/// Similar to `sock_bpf_src`, but also allows `BPF_A` - used for `RET`.
fn sock_bpf_rval(filter: &SockFilter) -> u8 {
    (filter.code & 0x18) as u8
}

/// Returns offset for the scratch memory with the specified address.
fn cbpf_scratch_offset(addr: u32) -> Result<i16, CbpfError> {
    if addr < CBPF_SCRATCH_SIZE {
        Ok((-(CBPF_SCRATCH_SIZE as i16) + (addr as i16)) * (CBPF_WORD_SIZE as i16))
    } else {
        Err(CbpfError::InvalidScratchOffset(addr))
    }
}

fn prep_patch(
    to_be_patched: &mut Vec<Vec<usize>>,
    bpf_len: usize,
    i: usize,
    cbpf_offset: u32,
    ebpf_source: usize,
) -> Result<(), CbpfError> {
    let offset_usize = cbpf_offset as usize;
    let Some(i_plus_1) = i.checked_add(1) else {
        return Err(CbpfError::InvalidJumpOffset(cbpf_offset));
    };
    let Some(cbpf_target) = i_plus_1.checked_add(offset_usize) else {
        return Err(CbpfError::InvalidJumpOffset(cbpf_offset));
    };
    if cbpf_target >= bpf_len {
        return Err(CbpfError::InvalidJumpOffset(cbpf_offset));
    }
    to_be_patched[cbpf_target].push(ebpf_source);
    Ok(())
}

// There are only two BPF registers, A and X. There are 10 eBPF general
// registers, numbered 0-9. We map between the two as follows:

// R0: Mapped to A.
const REG_A: u8 = 0;

// R1: Incoming argument pointing at the packet context (e.g. `sk_buff`). Moved to R6.
const REG_ARG1: u8 = 1;

// R6: Pointer to the program context. Initially passed as the first argument. Implicitly
//     used by eBPF when executing the legacy packet access instructions (`BPF_LD | BPF_ABS`
//     and `BPF_LD | BPF_IND`).
const REG_CONTEXT: u8 = 6;

// R7: Temp register used in the `BPF_MSH` implementation.
const REG_TMP: u8 = 7;

// R9: Mapped to X.
const REG_X: u8 = 9;

// R10: Const stack pointer. cBPF scratch memory (16 words) is stored on top of the stack.
const REG_STACK: u8 = 10;

fn apply_patches_for_pc(
    to_be_patched: &[Vec<usize>],
    i: usize,
    ebpf_code: &mut Vec<EbpfInstruction>,
) {
    let patch_count = to_be_patched[i].len();
    let mut p = 0;
    while p < patch_count {
        let index = to_be_patched[i][p];
        // `bpf_code.len() <= BPF_MAXINSNS` (4096) and each cBPF instruction expands to at
        // most 6 eBPF instructions (`BPF_MSH`), so the forward jump delta fits in `i16`.
        let delta = ebpf_code.len() - index - 1;
        let offset = delta as i16;
        let mut patched = ebpf_code[index];
        patched.set_offset(offset);
        ebpf_code[index] = patched;
        p += 1;
    }
}

fn convert_single_cbpf_inst(
    bpf_instruction: &SockFilter,
    bpf_len: usize,
    i: usize,
    config: &CbpfConfig,
    to_be_patched: &mut Vec<Vec<usize>>,
    ebpf_code: &mut Vec<EbpfInstruction>,
) -> Result<(), CbpfError> {
    // Classic BPF instruction codes are 8-bit values stored in a 16-bit field; non-zero upper
    // bits are invalid.
    if bpf_instruction.code > (u8::MAX as u16) {
        return Err(CbpfError::InvalidInstruction(bpf_instruction.code));
    }
    let code = bpf_instruction.code as u8;

    match sock_bpf_class(bpf_instruction) {
        BPF_ALU => match sock_bpf_op(bpf_instruction) {
            op @ (BPF_ADD | BPF_SUB | BPF_MUL | BPF_DIV | BPF_MOD | BPF_AND | BPF_OR | BPF_XOR
            | BPF_LSH | BPF_RSH) => {
                if sock_bpf_src(bpf_instruction) == BPF_K {
                    // Division and remainder by 0 are rejected.
                    if (op == BPF_DIV || op == BPF_MOD) && bpf_instruction.k == 0 {
                        return Err(CbpfError::InvalidInstruction(bpf_instruction.code));
                    }
                    // LSH and RSH by more than 31 are rejected.
                    if (op == BPF_LSH || op == BPF_RSH) && bpf_instruction.k >= 32 {
                        return Err(CbpfError::InvalidInstruction(bpf_instruction.code));
                    }
                    ebpf_code.push(EbpfInstruction::new(
                        code,
                        REG_A,
                        0,
                        0,
                        bpf_instruction.k as i32,
                    ));
                } else {
                    // Division and remainder by 0 must stop the execution and return 0.
                    if op == BPF_DIV || op == BPF_MOD {
                        // If X != 0, skip 2 instructions.
                        ebpf_code.push(EbpfInstruction::new(
                            BPF_JMP32 | BPF_JNE | BPF_K,
                            REG_X,
                            0,
                            2,
                            0,
                        ));
                        // Return 0.
                        ebpf_code.push(EbpfInstruction::new(
                            BPF_ALU | BPF_MOV | BPF_IMM,
                            REG_A,
                            0,
                            0,
                            0,
                        ));
                        ebpf_code.push(EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0));
                    }

                    ebpf_code.push(EbpfInstruction::new(code, REG_A, REG_X, 0, 0));
                }
            }
            BPF_NEG if sock_bpf_src(bpf_instruction) == BPF_K => {
                ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_NEG, REG_A, REG_A, 0, 0));
            }
            _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
        },
        class @ (BPF_LD | BPF_LDX) => {
            let dst_reg = if class == BPF_LDX { REG_X } else { REG_A };

            let mode = sock_bpf_addressing_mode(bpf_instruction);
            let size = sock_bpf_size(bpf_instruction);

            // Half-word (`BPF_H`) and byte (`BPF_B`) loads are allowed only for `BPF_ABS` and
            // `BPF_IND`. Also `BPF_ABS` and `BPF_IND` are not allowed with `BPF_LDX`.
            // `BPF_LEN`, `BPF_IMM` and `BPF_MEM` loads should be word-sized (i.e. `BPF_W`).
            // `BPF_MSH` is allowed only with `BPF_B` and `BPF_LDX`.
            match (size, mode, class) {
                (BPF_H | BPF_B | BPF_W, BPF_ABS | BPF_IND, BPF_LD) => {}
                (BPF_W, BPF_LEN | BPF_IMM | BPF_MEM, BPF_LD | BPF_LDX) => {}
                (BPF_B, BPF_MSH, BPF_LDX) if config.allow_msh => {}
                _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
            }

            let k = bpf_instruction.k;

            match mode {
                BPF_ABS => {
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_LD | BPF_ABS | size,
                        REG_A,
                        0,
                        0,
                        k as i32,
                    ));
                }
                BPF_IND => {
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_LD | BPF_IND | size,
                        REG_A,
                        REG_X,
                        0,
                        k as i32,
                    ));
                }
                BPF_IMM => {
                    let imm = k as i32;
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_MOV | BPF_K,
                        dst_reg,
                        0,
                        0,
                        imm,
                    ));
                }
                BPF_MEM => {
                    // cBPF's scratch memory is stored in the stack referenced by R10.
                    let offset = match cbpf_scratch_offset(k) {
                        Ok(off) => off,
                        Err(err) => return Err(err),
                    };
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_LDX | BPF_MEM,
                        dst_reg,
                        REG_STACK,
                        offset,
                        0,
                    ));
                }
                BPF_LEN => {
                    ebpf_code.push(match config.len {
                        CbpfLenInstruction::Static { len } => {
                            EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, dst_reg, 0, 0, len)
                        }
                        CbpfLenInstruction::ContextField { offset } => EbpfInstruction::new(
                            BPF_LDX | BPF_MEM | BPF_W,
                            dst_reg,
                            REG_CONTEXT,
                            offset,
                            0,
                        ),
                    });
                }
                BPF_MSH => {
                    // `BPF_MSH` loads `4 * (P[k:1] & 0xf)`, which translates to 6 instructions.
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_MOV | BPF_X,
                        REG_TMP,
                        REG_A,
                        0,
                        0,
                    ));
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_LD | BPF_ABS | BPF_B,
                        REG_A,
                        0,
                        0,
                        k as i32,
                    ));
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_AND | BPF_K,
                        REG_A,
                        0,
                        0,
                        0x0f,
                    ));
                    ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_MUL | BPF_K, REG_A, 0, 0, 4));
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_MOV | BPF_X,
                        REG_X,
                        REG_A,
                        0,
                        0,
                    ));
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_MOV | BPF_X,
                        REG_A,
                        REG_TMP,
                        0,
                        0,
                    ));
                }
                _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
            }
        }
        BPF_JMP => match sock_bpf_op(bpf_instruction) {
            BPF_JA if sock_bpf_src(bpf_instruction) == BPF_K => {
                ebpf_code.push(EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, -1, 0));
                if let Err(err) =
                    prep_patch(to_be_patched, bpf_len, i, bpf_instruction.k, ebpf_code.len() - 1)
                {
                    return Err(err);
                }
            }
            op @ (BPF_JGT | BPF_JGE | BPF_JEQ | BPF_JSET) => {
                // In cBPF, JMPs have a jump-if-true and jump-if-false branch. eBPF only
                // has jump-if-true. In most cases only one of the two branches actually
                // jumps (the other one is set to 0). In these cases the instruction can
                // be translated to 1 eBPF instruction. Otherwise two instructions are
                // produced in the output.
                let src = sock_bpf_src(bpf_instruction);
                let SockFilter { k, jt, jf, .. } = *bpf_instruction;
                let (src_reg, imm) = if src == BPF_K { (0, k as i32) } else { (REG_X, 0) };

                // When jumping only for the false case we can negate the comparison
                // operator to achieve the same effect with a single jump-if-true eBPF
                // instruction. That doesn't work for `BPF_JSET`. It is handled below
                // using 2 instructions.
                if jt == 0 && op != BPF_JSET {
                    let neg_op = match op {
                        BPF_JGT => BPF_JLE,
                        BPF_JGE => BPF_JLT,
                        _ => BPF_JNE,
                    };

                    ebpf_code.push(EbpfInstruction::new(
                        BPF_JMP32 | neg_op | src,
                        REG_A,
                        src_reg,
                        -1,
                        imm,
                    ));
                    if let Err(err) =
                        prep_patch(to_be_patched, bpf_len, i, jf as u32, ebpf_code.len() - 1)
                    {
                        return Err(err);
                    }
                } else {
                    // Jump if true.
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_JMP32 | op | src,
                        REG_A,
                        src_reg,
                        -1,
                        imm,
                    ));
                    if let Err(err) =
                        prep_patch(to_be_patched, bpf_len, i, jt as u32, ebpf_code.len() - 1)
                    {
                        return Err(err);
                    }

                    // Jump if false. Jumps with 0 offset are no-op and can be omitted.
                    if jf > 0 {
                        ebpf_code.push(EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, -1, 0));
                        if let Err(err) =
                            prep_patch(to_be_patched, bpf_len, i, jf as u32, ebpf_code.len() - 1)
                        {
                            return Err(err);
                        }
                    }
                }
            }
            _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
        },
        BPF_MISC => match sock_bpf_op(bpf_instruction) {
            BPF_TAX if sock_bpf_src(bpf_instruction) == BPF_K => {
                ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_X, REG_X, REG_A, 0, 0));
            }
            BPF_TXA if sock_bpf_src(bpf_instruction) == BPF_K => {
                ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_X, REG_A, REG_X, 0, 0));
            }
            _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
        },
        class @ (BPF_ST | BPF_STX) => {
            if sock_bpf_addressing_mode(bpf_instruction) != 0 || sock_bpf_size(bpf_instruction) != 0
            {
                return Err(CbpfError::InvalidInstruction(bpf_instruction.code));
            }

            // cBPF's scratch memory is stored in the stack referenced by R10.
            let src_reg = if class == BPF_STX { REG_X } else { REG_A };
            let offset = match cbpf_scratch_offset(bpf_instruction.k) {
                Ok(off) => off,
                Err(err) => return Err(err),
            };
            ebpf_code.push(EbpfInstruction::new(
                BPF_STX | BPF_MEM | BPF_W,
                REG_STACK,
                src_reg,
                offset,
                0,
            ));
        }
        BPF_RET => {
            if sock_bpf_addressing_mode(bpf_instruction) != 0 {
                return Err(CbpfError::InvalidInstruction(bpf_instruction.code));
            }
            match sock_bpf_rval(bpf_instruction) {
                BPF_K => {
                    // We're returning a particular value instead of the contents of the
                    // return register, so load that value into the return register.
                    let imm = bpf_instruction.k as i32;
                    ebpf_code.push(EbpfInstruction::new(
                        BPF_ALU | BPF_MOV | BPF_IMM,
                        REG_A,
                        0,
                        0,
                        imm,
                    ));
                }
                BPF_A => {}
                _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
            }

            ebpf_code.push(EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0));
        }
        _ => return Err(CbpfError::InvalidInstruction(bpf_instruction.code)),
    }

    Ok(())
}

/// Transforms a program in classic BPF (`SockFilter`) to extended BPF (`EbpfInstruction`).
pub fn cbpf_to_ebpf_core(
    bpf_code: &[SockFilter],
    config: &CbpfConfig,
) -> Result<Vec<EbpfInstruction>, CbpfError> {
    if bpf_code.len() > BPF_MAXINSNS as usize {
        return Err(CbpfError::ProgramTooLarge(bpf_code.len()));
    }
    if bpf_code.len() == 0 {
        return Err(CbpfError::InvalidInstruction(0));
    }

    // Table from jump target PC in cBPF to the list of eBPF jump instructions that target it.
    let mut to_be_patched: Vec<Vec<usize>> = Vec::new();
    let mut init_idx = 0;
    while init_idx < bpf_code.len() {
        to_be_patched.push(Vec::new());
        init_idx += 1;
    }

    let mut ebpf_code: Vec<EbpfInstruction> = Vec::new();

    // Save the arguments to registers that won't get clobbered by `BPF_LD`.
    ebpf_code.push(EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, REG_CONTEXT, REG_ARG1, 0, 0));

    // Reset A to 0. This is necessary in case one of the load operations exits prematurely.
    ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, REG_A, 0, 0, 0));
    // Reset X to 0. Linux does it and some programs might not validate otherwise.
    ebpf_code.push(EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, REG_X, 0, 0, 0));

    let mut loop_err: Option<CbpfError> = None;
    let mut i = 0;
    while i < bpf_code.len() && loop_err.is_none() {
        apply_patches_for_pc(&to_be_patched, i, &mut ebpf_code);
        match convert_single_cbpf_inst(
            &bpf_code[i],
            bpf_code.len(),
            i,
            config,
            &mut to_be_patched,
            &mut ebpf_code,
        ) {
            Ok(()) => {
                i += 1;
            }
            Err(err) => {
                loop_err = Some(err);
            }
        }
    }
    if let Some(err) = loop_err {
        return Err(err);
    }

    // Classic BPF programs must end with a `BPF_RET | BPF_K` or `BPF_RET | BPF_A` instruction.
    let last_code = bpf_code[bpf_code.len() - 1].code;
    if last_code != (BPF_RET | BPF_K) as u16 && last_code != (BPF_RET | BPF_A) as u16 {
        return Err(CbpfError::InvalidInstruction(last_code));
    }

    Ok(ebpf_code)
}
