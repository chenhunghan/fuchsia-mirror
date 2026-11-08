// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::converter::{CbpfConfig, CbpfLenInstruction};

use crate::EbpfError;
use crate::api::{BPF_MAXINSNS, EbpfInstruction, SockFilter};
use crate::program::{
    BpfProgramContext, EbpfProgram, ProgramArgument, StaticHelperSet, link_program,
};
use crate::verifier::{
    CallingContext, NullVerifierLogger, Type, VerifiedEbpfProgram, verify_program,
};
use ebpf_core::converter::{
    cbpf_to_ebpf_core, sock_bpf_addressing_mode, sock_bpf_class, sock_bpf_op, sock_bpf_size,
};
use linux_uapi::sock_filter;
use std::collections::HashMap;

const fn sock_filter_to_core(filter: &sock_filter) -> SockFilter {
    SockFilter { code: filter.code, jt: filter.jt, jf: filter.jf, k: filter.k }
}

// These are accessors for bits in a BPF/eBPF instruction.
// Instructions are encoded in one byte. The first 3 LSB represent
// the operation, and the other bits represent various modifiers.
// Brief comments are given to indicate what the functions broadly
// represent, but for the gory detail, consult a detailed guide to
// BPF, like the one at https://docs.kernel.org/bpf/instruction-set.html

/// The `bpf_class` is the instruction type (e.g., load/store/jump/ALU).
pub const fn bpf_class(filter: &sock_filter) -> u8 {
    sock_bpf_class(&sock_filter_to_core(filter))
}

/// The `bpf_size` is the 4th and 5th bit of load and store
/// instructions. It indicates the bit width of the load / store
/// target (8, 16, 32, 64 bits).
pub const fn bpf_size(filter: &sock_filter) -> u8 {
    sock_bpf_size(&sock_filter_to_core(filter))
}

/// The addressing mode is the most significant three bits of load and
/// store instructions. They indicate whether the instruction accesses a
/// constant, accesses from memory, or accesses from memory atomically.
pub const fn bpf_addressing_mode(filter: &sock_filter) -> u8 {
    sock_bpf_addressing_mode(&sock_filter_to_core(filter))
}

/// Modifiers for jumps and ALU operations. For example, a jump can
/// be jeq, jlt, etc. An ALU operation can be plus, minus, divide,
/// etc.
pub const fn bpf_op(filter: &sock_filter) -> u8 {
    sock_bpf_op(&sock_filter_to_core(filter))
}

/// Transforms a program in classic BPF (cBPF, as stored in `struct sock_filter`)
/// to extended BPF (stored as `EbpfInstruction`).
/// The `bpf_code` parameter is kept as an array for easy transfer
/// via FFI. This currently only allows the subset of BPF permitted
/// by `seccomp(2)`.
fn cbpf_to_ebpf(
    bpf_code: &[sock_filter],
    config: &CbpfConfig,
) -> Result<Vec<EbpfInstruction>, EbpfError> {
    if bpf_code.len() > BPF_MAXINSNS as usize {
        return Err(EbpfError::CbpfProgramTooLarge(bpf_code.len()));
    }
    let core_code: Vec<SockFilter> = bpf_code.iter().map(sock_filter_to_core).collect();
    cbpf_to_ebpf_core(&core_code, config).map_err(EbpfError::from)
}

/// Instantiates an `EbpfProgram` given a cBPF original that will work with a packet of the
/// specified type.
pub fn convert_and_verify_cbpf(
    bpf_code: &[sock_filter],
    packet_type: Type,
    config: &CbpfConfig,
) -> Result<VerifiedEbpfProgram, EbpfError> {
    let context = CallingContext {
        maps: vec![],
        helpers: HashMap::new(),
        args: vec![packet_type.clone()],
        packet_type: Some(packet_type),
        allow_ptr_leaks: false,
    };
    let ebpf_code = cbpf_to_ebpf(bpf_code, config)?;
    Ok(verify_program(ebpf_code, context, &mut NullVerifierLogger)?)
}

/// Converts, verifies and links a cBPF program for execution in the specified context.
pub fn convert_and_link_cbpf<C: BpfProgramContext + StaticHelperSet>(
    bpf_code: &[sock_filter],
) -> Result<EbpfProgram<C>, EbpfError> {
    let verified = convert_and_verify_cbpf(
        bpf_code,
        <C as BpfProgramContext>::Packet::get_type().clone(),
        C::CBPF_CONFIG,
    )?;
    link_program(&verified, vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        BPF_A, BPF_ABS, BPF_ADD, BPF_ALU, BPF_ALU64, BPF_AND, BPF_B, BPF_DIV, BPF_EXIT, BPF_IMM,
        BPF_JA, BPF_JEQ, BPF_JMP, BPF_JMP32, BPF_JNE, BPF_JSET, BPF_K, BPF_LD, BPF_LDX, BPF_LEN,
        BPF_LSH, BPF_MEM, BPF_MISC, BPF_MOV, BPF_MSH, BPF_MUL, BPF_NEG, BPF_OR, BPF_RET, BPF_RSH,
        BPF_ST, BPF_STX, BPF_SUB, BPF_TAX, BPF_TXA, BPF_W, BPF_X, BPF_XOR,
    };
    use crate::{NoMap, PointerOrigin, empty_static_helper_set, new_memory_id};
    use linux_uapi::{
        AUDIT_ARCH_AARCH64, AUDIT_ARCH_X86_64, SECCOMP_RET_ALLOW, SECCOMP_RET_TRAP, seccomp_data,
        sock_filter,
    };
    use std::mem::offset_of;
    use std::sync::LazyLock;
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    pub const TEST_CBPF_CONFIG: CbpfConfig = CbpfConfig {
        len: CbpfLenInstruction::Static { len: size_of::<seccomp_data>() as i32 },
        allow_msh: true,
    };

    #[test]
    fn test_cbpf_to_ebpf() {
        // Jump to the next instruction.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_JMP | BPF_JA) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Ok(vec![
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 9, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ]),
        );

        // Jump after last instruction.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_JMP | BPF_JA) as u16, jt: 0, jf: 0, k: 1 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Err(EbpfError::InvalidCbpfJumpOffset(1)),
        );

        // Jump out of bounds.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![sock_filter { code: (BPF_JMP | BPF_JA) as u16, jt: 0, jf: 0, k: 0xffffffff }],
                &TEST_CBPF_CONFIG
            ),
            Err(EbpfError::InvalidCbpfJumpOffset(0xffffffff)),
        );

        // BPF_JNE is allowed only in eBPF.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_JMP | BPF_JNE) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Err(EbpfError::InvalidCbpfInstruction((BPF_JMP | BPF_JNE) as u16)),
        );

        // BPF_JEQ is supported in BPF.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_JMP | BPF_JEQ) as u16, jt: 1, jf: 0, k: 0 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Ok(vec![
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 9, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP32 | BPF_JEQ, 0, 0, 1, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ]),
        );

        // Make sure the jump is translated correctly when the jump target produces 2 instructions.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_JMP | BPF_JA) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_RET | BPF_K) as u16, jt: 0, jf: 0, k: 1 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Ok(vec![
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 9, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_IMM, 0, 0, 0, 1),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ]),
        );

        // BPF_MEM access.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_LD | BPF_MEM) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_LDX | BPF_MEM) as u16, jt: 0, jf: 0, k: 15 },
                    sock_filter { code: BPF_ST as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: BPF_STX as u16, jt: 0, jf: 0, k: 15 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Ok(vec![
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_ALU | BPF_MOV | BPF_K, 9, 0, 0, 0),
                EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 0, 10, -64, 0),
                EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 9, 10, -4, 0),
                EbpfInstruction::new(BPF_STX | BPF_MEM | BPF_W, 10, 0, -64, 0),
                EbpfInstruction::new(BPF_STX | BPF_MEM | BPF_W, 10, 9, -4, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ]),
        );

        // BPF_MEM access out of bounds.
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_LD | BPF_MEM) as u16, jt: 0, jf: 0, k: 17 },
                    sock_filter { code: (BPF_RET | BPF_A) as u16, jt: 0, jf: 0, k: 0 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Err(EbpfError::InvalidCbpfScratchOffset(17)),
        );

        // Empty program and programs not ending in BPF_RET | BPF_K or BPF_RET | BPF_A are rejected.
        assert_eq!(cbpf_to_ebpf(&[], &TEST_CBPF_CONFIG), Err(EbpfError::InvalidCbpfInstruction(0)));
        assert_eq!(
            cbpf_to_ebpf(
                &vec![
                    sock_filter { code: (BPF_RET | BPF_K) as u16, jt: 0, jf: 0, k: 0 },
                    sock_filter { code: (BPF_ALU | BPF_ADD | BPF_K) as u16, jt: 0, jf: 0, k: 1 },
                ],
                &TEST_CBPF_CONFIG
            ),
            Err(EbpfError::InvalidCbpfInstruction((BPF_ALU | BPF_ADD | BPF_K) as u16)),
        );

        // Instructions with non-zero upper 8 bits or invalid source/op bits are rejected.
        for bad_code in [
            0x0100 | (BPF_RET | BPF_K) as u16,
            (BPF_ALU | BPF_NEG | BPF_X) as u16,
            (BPF_JMP | BPF_JA | BPF_X) as u16,
            (BPF_MISC | BPF_TAX | BPF_X) as u16,
            (BPF_MISC | BPF_TXA | BPF_X) as u16,
            (BPF_ST | BPF_X) as u16,
            (BPF_STX | BPF_X) as u16,
            (BPF_RET | BPF_X) as u16,
            (BPF_RET | BPF_K | BPF_ABS) as u16,
        ] {
            assert_eq!(
                cbpf_to_ebpf(
                    &vec![
                        sock_filter { code: bad_code, jt: 0, jf: 0, k: 0 },
                        sock_filter { code: (BPF_RET | BPF_K) as u16, jt: 0, jf: 0, k: 0 },
                    ],
                    &TEST_CBPF_CONFIG
                ),
                Err(EbpfError::InvalidCbpfInstruction(bad_code)),
            );
        }
    }

    const BPF_ALU_ADD_K: u16 = (BPF_ALU | BPF_ADD | BPF_K) as u16;
    const BPF_ALU_SUB_K: u16 = (BPF_ALU | BPF_SUB | BPF_K) as u16;
    const BPF_ALU_MUL_K: u16 = (BPF_ALU | BPF_MUL | BPF_K) as u16;
    const BPF_ALU_DIV_K: u16 = (BPF_ALU | BPF_DIV | BPF_K) as u16;
    const BPF_ALU_AND_K: u16 = (BPF_ALU | BPF_AND | BPF_K) as u16;
    const BPF_ALU_OR_K: u16 = (BPF_ALU | BPF_OR | BPF_K) as u16;
    const BPF_ALU_XOR_K: u16 = (BPF_ALU | BPF_XOR | BPF_K) as u16;
    const BPF_ALU_LSH_K: u16 = (BPF_ALU | BPF_LSH | BPF_K) as u16;
    const BPF_ALU_RSH_K: u16 = (BPF_ALU | BPF_RSH | BPF_K) as u16;

    const BPF_ALU_OR_X: u16 = (BPF_ALU | BPF_OR | BPF_X) as u16;

    const BPF_LD_W_ABS: u16 = (BPF_LD | BPF_ABS | BPF_W) as u16;
    const BPF_LD_W_MEM: u16 = (BPF_LD | BPF_MEM | BPF_W) as u16;
    const BPF_JEQ_K: u16 = (BPF_JMP | BPF_JEQ | BPF_K) as u16;
    const BPF_JSET_K: u16 = (BPF_JMP | BPF_JSET | BPF_K) as u16;
    const BPF_RET_K: u16 = (BPF_RET | BPF_K) as u16;
    const BPF_RET_A: u16 = (BPF_RET | BPF_A) as u16;
    const BPF_ST_REG: u16 = BPF_ST as u16;
    const BPF_MISC_TAX: u16 = (BPF_MISC | BPF_TAX) as u16;

    struct TestProgramContext {}

    impl BpfProgramContext for TestProgramContext {
        type RunContext<'a> = ();
        type Packet<'a> = &'a seccomp_data;
        type Map = NoMap;
        const CBPF_CONFIG: &'static CbpfConfig = &TEST_CBPF_CONFIG;
    }

    empty_static_helper_set!(TestProgramContext);

    static SECCOMP_DATA_TYPE: LazyLock<Type> = LazyLock::new(|| Type::PtrToMemory {
        id: new_memory_id(),
        offset: 0.into(),
        buffer_size: 0,
        origin: PointerOrigin::KernelReadOnly,
    });

    impl ProgramArgument for &'_ seccomp_data {
        fn get_type() -> &'static Type {
            &*SECCOMP_DATA_TYPE
        }
    }

    fn with_prg_assert_result(
        prg: &EbpfProgram<TestProgramContext>,
        mut data: seccomp_data,
        result: u32,
        msg: &str,
    ) {
        let return_value = prg.run(&mut (), &mut data);
        assert_eq!(return_value, result as u64, "{}: filter return value is {}", msg, return_value);
    }

    #[test]
    fn test_filter_with_dw_load() {
        let test_prg = [
            // Check data.arch
            sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 4 },
            sock_filter { code: BPF_JEQ_K, jt: 1, jf: 0, k: AUDIT_ARCH_X86_64 },
            // Return 1 if arch is wrong
            sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: 1 },
            // Load data.nr (the syscall number)
            sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 0 },
            // Always allow 41
            sock_filter { code: BPF_JEQ_K, jt: 0, jf: 1, k: 41 },
            sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW },
            // Don't allow 115
            sock_filter { code: BPF_JEQ_K, jt: 0, jf: 1, k: 115 },
            sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_TRAP },
            // For other syscalls, check the args
            // A common hack to deal with 64-bit numbers in BPF: deal
            // with 32 bits at a time.
            // First, Load arg0's most significant 32 bits in M[0]
            sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 16 },
            sock_filter { code: BPF_ST_REG, jt: 0, jf: 0, k: 0 },
            // Load arg0's least significant 32 bits into M[1]
            sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 20 },
            sock_filter { code: BPF_ST_REG, jt: 0, jf: 0, k: 1 },
            // JSET is A & k.  Check the first 32 bits.  If the test
            // is successful, jump, otherwise, check the next 32 bits.
            sock_filter { code: BPF_LD_W_MEM, jt: 0, jf: 0, k: 0 },
            sock_filter { code: BPF_JSET_K, jt: 2, jf: 0, k: 4294967295 },
            sock_filter { code: BPF_LD_W_MEM, jt: 0, jf: 0, k: 1 },
            sock_filter { code: BPF_JSET_K, jt: 0, jf: 1, k: 4294967292 },
            sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_TRAP },
            sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW },
        ];

        let prg =
            convert_and_link_cbpf::<TestProgramContext>(&test_prg).expect("Error parsing program");

        with_prg_assert_result(
            &prg,
            seccomp_data { arch: AUDIT_ARCH_AARCH64, ..Default::default() },
            1,
            "Did not reject incorrect arch",
        );

        with_prg_assert_result(
            &prg,
            seccomp_data { arch: AUDIT_ARCH_X86_64, nr: 41, ..Default::default() },
            SECCOMP_RET_ALLOW,
            "Did not pass simple RET_ALLOW",
        );

        with_prg_assert_result(
            &prg,
            seccomp_data {
                arch: AUDIT_ARCH_X86_64,
                nr: 100,
                args: [0xFF00000000, 0, 0, 0, 0, 0],
                ..Default::default()
            },
            SECCOMP_RET_TRAP,
            "Did not treat load of first 32 bits correctly",
        );

        with_prg_assert_result(
            &prg,
            seccomp_data {
                arch: AUDIT_ARCH_X86_64,
                nr: 100,
                args: [0x4, 0, 0, 0, 0, 0],
                ..Default::default()
            },
            SECCOMP_RET_TRAP,
            "Did not correctly reject load of second 32 bits",
        );

        with_prg_assert_result(
            &prg,
            seccomp_data {
                arch: AUDIT_ARCH_X86_64,
                nr: 100,
                args: [0x0, 0, 0, 0, 0, 0],
                ..Default::default()
            },
            SECCOMP_RET_ALLOW,
            "Did not correctly accept load of second 32 bits",
        );
    }

    #[test]
    fn test_alu_insns() {
        {
            let test_prg = [
                // Load data.nr (the syscall number)
                sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 0 }, // = 1, 11
                // Do some math.
                sock_filter { code: BPF_ALU_ADD_K, jt: 0, jf: 0, k: 3 }, // = 4, 14
                sock_filter { code: BPF_ALU_SUB_K, jt: 0, jf: 0, k: 2 }, // = 2, 12
                sock_filter { code: BPF_MISC_TAX, jt: 0, jf: 0, k: 0 },  // 2, 12 -> X
                sock_filter { code: BPF_ALU_MUL_K, jt: 0, jf: 0, k: 8 }, // = 16, 96
                sock_filter { code: BPF_ALU_DIV_K, jt: 0, jf: 0, k: 2 }, // = 8, 48
                sock_filter { code: BPF_ALU_AND_K, jt: 0, jf: 0, k: 15 }, // = 8, 0
                sock_filter { code: BPF_ALU_OR_K, jt: 0, jf: 0, k: 16 }, // = 24, 16
                sock_filter { code: BPF_ALU_XOR_K, jt: 0, jf: 0, k: 7 }, // = 31, 23
                sock_filter { code: BPF_ALU_LSH_K, jt: 0, jf: 0, k: 2 }, // = 124, 92
                sock_filter { code: BPF_ALU_OR_X, jt: 0, jf: 0, k: 1 },  // = 127, 92
                sock_filter { code: BPF_ALU_RSH_K, jt: 0, jf: 0, k: 1 }, // = 63, 46
                sock_filter { code: BPF_RET_A, jt: 0, jf: 0, k: 0 },
            ];

            let prg = convert_and_link_cbpf::<TestProgramContext>(&test_prg)
                .expect("Error parsing program");

            with_prg_assert_result(
                &prg,
                seccomp_data { nr: 1, ..Default::default() },
                63,
                "BPF math does not work",
            );

            with_prg_assert_result(
                &prg,
                seccomp_data { nr: 11, ..Default::default() },
                46,
                "BPF math does not work",
            );
        }

        {
            // Negative numbers simple check
            let test_prg = [
                // Load data.nr (the syscall number)
                sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 0 }, // = -1
                sock_filter { code: BPF_ALU_SUB_K, jt: 0, jf: 0, k: 2 }, // = -3
                sock_filter { code: BPF_RET_A, jt: 0, jf: 0, k: 0 },
            ];

            let prg = convert_and_link_cbpf::<TestProgramContext>(&test_prg)
                .expect("Error parsing program");

            with_prg_assert_result(
                &prg,
                seccomp_data { nr: -1, ..Default::default() },
                u32::MAX - 2,
                "BPF math does not work",
            );
        }
    }

    // Test BPF_MSH cBPF instruction.
    #[test]
    fn test_ld_msh() {
        let test_prg = [
            // X <- 4 * (P[0] & 0xf)
            sock_filter { code: (BPF_LDX | BPF_MSH | BPF_B) as u16, jt: 0, jf: 0, k: 0 },
            // A <- X
            sock_filter { code: (BPF_MISC | BPF_TXA) as u16, jt: 0, jf: 0, k: 0 },
            // ret A
            sock_filter { code: BPF_RET_A, jt: 0, jf: 0, k: 0 },
        ];

        let prg =
            convert_and_link_cbpf::<TestProgramContext>(&test_prg).expect("Error parsing program");

        for i in [0x00, 0x01, 0x07, 0x15, 0xff].iter() {
            with_prg_assert_result(
                &prg,
                seccomp_data { nr: *i, ..Default::default() },
                4 * (*i & 0xf) as u32,
                "BPF math does not work",
            )
        }
    }

    #[test]
    fn test_static_packet_len() {
        let test_prg = [
            // A <- packet_len
            sock_filter { code: (BPF_LD | BPF_LEN | BPF_W) as u16, jt: 0, jf: 0, k: 0 },
            // ret A
            sock_filter { code: BPF_RET_A, jt: 0, jf: 0, k: 0 },
        ];

        let prg =
            convert_and_link_cbpf::<TestProgramContext>(&test_prg).expect("Error parsing program");

        let data = seccomp_data::default();
        assert_eq!(prg.run(&mut (), &data), size_of::<seccomp_data>() as u64);
    }

    // A packet used by `test_variable_packet_len()` below to verify the case when the packet
    // length is stored as a struct field.
    #[repr(C)]
    #[derive(Debug, Default, IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct VariableLengthPacket {
        foo: u32,
        len: i32,
        bar: u64,
    }

    static VARIABLE_LENGTH_PACKET_TYPE: LazyLock<Type> = LazyLock::new(|| Type::PtrToMemory {
        id: new_memory_id(),
        offset: 0.into(),
        buffer_size: size_of::<VariableLengthPacket>() as u64,
        origin: PointerOrigin::KernelReadOnly,
    });

    impl ProgramArgument for &'_ VariableLengthPacket {
        fn get_type() -> &'static Type {
            &*VARIABLE_LENGTH_PACKET_TYPE
        }
    }

    pub const VARIABLE_LENGTH_CBPF_CONFIG: CbpfConfig = CbpfConfig {
        len: CbpfLenInstruction::ContextField {
            offset: offset_of!(VariableLengthPacket, len) as i16,
        },
        allow_msh: true,
    };

    struct VariableLengthPacketContext {}

    impl BpfProgramContext for VariableLengthPacketContext {
        type RunContext<'a> = ();
        type Packet<'a> = &'a VariableLengthPacket;
        type Map = NoMap;
        const CBPF_CONFIG: &'static CbpfConfig = &VARIABLE_LENGTH_CBPF_CONFIG;
    }

    empty_static_helper_set!(VariableLengthPacketContext);

    #[test]
    fn test_variable_packet_len() {
        let test_prg = [
            // A <- packet_len
            sock_filter { code: (BPF_LD | BPF_LEN | BPF_W) as u16, jt: 0, jf: 0, k: 0 },
            // ret A
            sock_filter { code: BPF_RET_A, jt: 0, jf: 0, k: 0 },
        ];

        let prg = convert_and_link_cbpf::<VariableLengthPacketContext>(&test_prg)
            .expect("Error parsing program");
        let data = VariableLengthPacket { len: 42, ..VariableLengthPacket::default() };
        assert_eq!(prg.run(&mut (), &data), data.len as u64);
    }

    #[test]
    fn test_cbpf_max_insns_limit() {
        let max_insns = BPF_MAXINSNS as usize;
        let too_large = vec![sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: 0 }; max_insns + 1];
        assert_eq!(
            cbpf_to_ebpf(&too_large, &VARIABLE_LENGTH_CBPF_CONFIG),
            Err(EbpfError::CbpfProgramTooLarge(max_insns + 1))
        );

        // At `BPF_MAXINSNS` (4096), jumping over 4094 6-instruction `BPF_MSH` expansions produces
        // a 24564-instruction delta, which fits within `i16::MAX` (32767).
        let msh_count = max_insns - 2;
        let mut max_prg = Vec::with_capacity(max_insns);
        max_prg.push(sock_filter {
            code: (BPF_JMP | BPF_JA) as u16,
            jt: 0,
            jf: 0,
            k: msh_count as u32,
        });
        max_prg.extend(std::iter::repeat_n(
            sock_filter { code: (BPF_LDX | BPF_MSH | BPF_B) as u16, jt: 0, jf: 0, k: 0 },
            msh_count,
        ));
        max_prg.push(sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: 1 });
        let ebpf = cbpf_to_ebpf(&max_prg, &VARIABLE_LENGTH_CBPF_CONFIG)
            .expect("BPF_MAXINSNS program should convert");
        assert_eq!(ebpf[3].offset(), (msh_count * 6) as i16);
    }
}
