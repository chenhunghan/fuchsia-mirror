// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::api::*;

pub type CbpfInstruction = linux_uapi::sock_filter;

use static_assertions::const_assert_eq;

const_assert_eq!(BPF_MAXINSNS, linux_uapi::BPF_MAXINSNS);
const_assert_eq!(BPF_MAP_TYPE_ARRAY, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);

const_assert_eq!(
    std::mem::size_of::<EbpfInstruction>(),
    std::mem::size_of::<linux_uapi::bpf_insn>()
);
const_assert_eq!(std::mem::size_of::<SockFilter>(), std::mem::size_of::<linux_uapi::sock_filter>());
const_assert_eq!(
    std::mem::align_of::<SockFilter>(),
    std::mem::align_of::<linux_uapi::sock_filter>()
);
const_assert_eq!(
    std::mem::offset_of!(SockFilter, code),
    std::mem::offset_of!(CbpfInstruction, code)
);
const_assert_eq!(std::mem::offset_of!(SockFilter, jt), std::mem::offset_of!(CbpfInstruction, jt));
const_assert_eq!(std::mem::offset_of!(SockFilter, jf), std::mem::offset_of!(CbpfInstruction, jf));
const_assert_eq!(std::mem::offset_of!(SockFilter, k), std::mem::offset_of!(CbpfInstruction, k));

const_assert_eq!(BPF_LD, linux_uapi::BPF_LD as u8);
const_assert_eq!(BPF_LDX, linux_uapi::BPF_LDX as u8);
const_assert_eq!(BPF_ST, linux_uapi::BPF_ST as u8);
const_assert_eq!(BPF_STX, linux_uapi::BPF_STX as u8);
const_assert_eq!(BPF_ALU, linux_uapi::BPF_ALU as u8);
const_assert_eq!(BPF_JMP, linux_uapi::BPF_JMP as u8);
const_assert_eq!(BPF_JMP32, linux_uapi::BPF_JMP32 as u8);
const_assert_eq!(BPF_ALU64, linux_uapi::BPF_ALU64 as u8);

const_assert_eq!(BPF_SRC_REG, linux_uapi::BPF_X as u8);
const_assert_eq!(BPF_SRC_IMM, linux_uapi::BPF_K as u8);

const_assert_eq!(BPF_IMM, linux_uapi::BPF_IMM as u8);
const_assert_eq!(BPF_MEM, linux_uapi::BPF_MEM as u8);
const_assert_eq!(BPF_ATOMIC, linux_uapi::BPF_ATOMIC as u8);
const_assert_eq!(BPF_ABS, linux_uapi::BPF_ABS as u8);
const_assert_eq!(BPF_IND, linux_uapi::BPF_IND as u8);

const_assert_eq!(BPF_TO_BE, linux_uapi::BPF_TO_BE as u8);
const_assert_eq!(BPF_TO_LE, linux_uapi::BPF_TO_LE as u8);

const_assert_eq!(BPF_B, linux_uapi::BPF_B as u8);
const_assert_eq!(BPF_H, linux_uapi::BPF_H as u8);
const_assert_eq!(BPF_W, linux_uapi::BPF_W as u8);
const_assert_eq!(BPF_DW, linux_uapi::BPF_DW as u8);

const_assert_eq!(BPF_ADD, linux_uapi::BPF_ADD as u8);
const_assert_eq!(BPF_SUB, linux_uapi::BPF_SUB as u8);
const_assert_eq!(BPF_MUL, linux_uapi::BPF_MUL as u8);
const_assert_eq!(BPF_DIV, linux_uapi::BPF_DIV as u8);
const_assert_eq!(BPF_OR, linux_uapi::BPF_OR as u8);
const_assert_eq!(BPF_AND, linux_uapi::BPF_AND as u8);
const_assert_eq!(BPF_LSH, linux_uapi::BPF_LSH as u8);
const_assert_eq!(BPF_RSH, linux_uapi::BPF_RSH as u8);
const_assert_eq!(BPF_NEG, linux_uapi::BPF_NEG as u8);
const_assert_eq!(BPF_MOD, linux_uapi::BPF_MOD as u8);
const_assert_eq!(BPF_XOR, linux_uapi::BPF_XOR as u8);
const_assert_eq!(BPF_MOV, linux_uapi::BPF_MOV as u8);
const_assert_eq!(BPF_ARSH, linux_uapi::BPF_ARSH as u8);
const_assert_eq!(BPF_END, linux_uapi::BPF_END as u8);

const_assert_eq!(BPF_JA, linux_uapi::BPF_JA as u8);
const_assert_eq!(BPF_JEQ, linux_uapi::BPF_JEQ as u8);
const_assert_eq!(BPF_JGT, linux_uapi::BPF_JGT as u8);
const_assert_eq!(BPF_JGE, linux_uapi::BPF_JGE as u8);
const_assert_eq!(BPF_JSET, linux_uapi::BPF_JSET as u8);
const_assert_eq!(BPF_JNE, linux_uapi::BPF_JNE as u8);
const_assert_eq!(BPF_JSGT, linux_uapi::BPF_JSGT as u8);
const_assert_eq!(BPF_JSGE, linux_uapi::BPF_JSGE as u8);
const_assert_eq!(BPF_CALL, linux_uapi::BPF_CALL as u8);
const_assert_eq!(BPF_EXIT, linux_uapi::BPF_EXIT as u8);
const_assert_eq!(BPF_JLT, linux_uapi::BPF_JLT as u8);
const_assert_eq!(BPF_JLE, linux_uapi::BPF_JLE as u8);
const_assert_eq!(BPF_JSLT, linux_uapi::BPF_JSLT as u8);
const_assert_eq!(BPF_JSLE, linux_uapi::BPF_JSLE as u8);

const_assert_eq!(BPF_FETCH, linux_uapi::BPF_FETCH as u8);
const_assert_eq!(BPF_XCHG, linux_uapi::BPF_XCHG as u8);
const_assert_eq!(BPF_CMPXCHG, linux_uapi::BPF_CMPXCHG as u8);

const_assert_eq!(BPF_LEN, linux_uapi::BPF_LEN as u8);
const_assert_eq!(BPF_MISC, linux_uapi::BPF_MISC as u8);
const_assert_eq!(BPF_RET, linux_uapi::BPF_RET as u8);
const_assert_eq!(BPF_MSH, linux_uapi::BPF_MSH as u8);
const_assert_eq!(BPF_A, linux_uapi::BPF_A as u8);
const_assert_eq!(BPF_K, linux_uapi::BPF_K as u8);
const_assert_eq!(BPF_X, linux_uapi::BPF_X as u8);
const_assert_eq!(BPF_TXA, linux_uapi::BPF_TXA as u8);
const_assert_eq!(BPF_TAX, linux_uapi::BPF_TAX as u8);

const_assert_eq!(BPF_PSEUDO_MAP_FD, linux_uapi::BPF_PSEUDO_MAP_FD as u8);
const_assert_eq!(BPF_PSEUDO_MAP_IDX, linux_uapi::BPF_PSEUDO_MAP_IDX as u8);
const_assert_eq!(BPF_PSEUDO_MAP_VALUE, linux_uapi::BPF_PSEUDO_MAP_VALUE as u8);
const_assert_eq!(BPF_PSEUDO_MAP_IDX_VALUE, linux_uapi::BPF_PSEUDO_MAP_IDX_VALUE as u8);
const_assert_eq!(BPF_PSEUDO_BTF_ID, linux_uapi::BPF_PSEUDO_BTF_ID as u8);
const_assert_eq!(BPF_PSEUDO_FUNC, linux_uapi::BPF_PSEUDO_FUNC as u8);
const_assert_eq!(BPF_PSEUDO_CALL, linux_uapi::BPF_PSEUDO_CALL as u8);
const_assert_eq!(BPF_PSEUDO_KFUNC_CALL, linux_uapi::BPF_PSEUDO_KFUNC_CALL as u8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ebpf_instruction_bitfields_and_mutators() {
        let mut inst = EbpfInstruction::new(0x95, 15, 14, -1, -1);
        assert_eq!(inst.code(), 0x95);
        assert_eq!(inst.dst_reg(), 15);
        assert_eq!(inst.src_reg(), 14);
        assert_eq!(inst.offset(), -1);
        assert_eq!(inst.imm(), -1);
        assert_eq!(u64::from(inst), inst.get());
        assert_eq!(EbpfInstruction::from(inst.get()), inst);

        // Verify negative offset/imm and adjacent nibbles never clobber neighboring fields.
        inst.set_offset(i16::MIN);
        assert_eq!(inst.code(), 0x95);
        assert_eq!(inst.dst_reg(), 15);
        assert_eq!(inst.src_reg(), 14);
        assert_eq!(inst.offset(), i16::MIN);
        assert_eq!(inst.imm(), -1);

        inst.set_imm(i32::MIN);
        assert_eq!(inst.offset(), i16::MIN);
        assert_eq!(inst.imm(), i32::MIN);

        inst.set_code(0xb7);
        inst.set_dst_reg(0);
        inst.set_src_reg(15);
        assert_eq!(inst.code(), 0xb7);
        assert_eq!(inst.dst_reg(), 0);
        assert_eq!(inst.src_reg(), 15);
        assert_eq!(inst.offset(), i16::MIN);
        assert_eq!(inst.imm(), i32::MIN);
    }

    #[test]
    #[should_panic]
    fn test_ebpf_instruction_dst_out_of_range() {
        let _ = EbpfInstruction::new(0, 16, 0, 0, 0);
    }

    #[test]
    #[should_panic]
    fn test_ebpf_instruction_src_out_of_range() {
        let _ = EbpfInstruction::new(0, 0, 16, 0, 0);
    }

    #[test]
    fn test_apply_endianness() {
        let raw = 0x0123_4567_89ab_cdef_u64;

        // `BPF_TO_BE`: byte-swaps the low `width` bits and zeroes upper bits.
        assert_eq!(apply_endianness(true, DataWidth::U8, raw), 0xef);
        assert_eq!(apply_endianness(true, DataWidth::U16, raw), 0xefcd);
        assert_eq!(apply_endianness(true, DataWidth::U32, raw), 0xefcd_ab89);
        assert_eq!(apply_endianness(true, DataWidth::U64, raw), 0xefcd_ab89_6745_2301);

        // `BPF_TO_LE`: preserves little-endian byte order and zeroes upper bits.
        assert_eq!(apply_endianness(false, DataWidth::U8, raw), 0xef);
        assert_eq!(apply_endianness(false, DataWidth::U16, raw), 0xcdef);
        assert_eq!(apply_endianness(false, DataWidth::U32, raw), 0x89ab_cdef);
        assert_eq!(apply_endianness(false, DataWidth::U64, raw), raw);
    }
}
