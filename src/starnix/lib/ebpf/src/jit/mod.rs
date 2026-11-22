// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use crate::VerifierErrorKind;
use crate::{EbpfInstruction, HelperLinkage, JitError};

#[cfg(target_arch = "aarch64")]
pub mod arm64;
#[cfg(target_arch = "aarch64")]
use arm64 as arch;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod stub {
    use crate::{EbpfInstruction, HelperLinkage, JitError};

    pub const IS_SUPPORTED: bool = false;

    pub fn compile(
        _code: &[EbpfInstruction],
        _helpers: &[HelperLinkage],
        _packet_load_fn_addr: Option<u64>,
    ) -> Result<Vec<u8>, JitError> {
        Err(JitError::UnsupportedArchitecture)
    }
}
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
use stub as arch;

#[cfg(target_os = "fuchsia")]
mod fuchsia;

#[cfg(target_os = "fuchsia")]
pub use fuchsia::*;

/// Whether JIT compilation is supported on the current target architecture.
pub const IS_SUPPORTED: bool = arch::IS_SUPPORTED;

/// Entry point signature for JIT-compiled eBPF programs.
pub type JitFn =
    unsafe extern "C" fn(run_context: *mut (), r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const TARGET_PC_EXIT: u32 = u32::MAX;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomicOp {
    Add { fetch: bool },
    And { fetch: bool },
    Or { fetch: bool },
    Xor { fetch: bool },
    Xchg,
    Cmpxchg,
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn resolve_jump_target(pc: usize, offset: i16, len: usize) -> Result<u32, VerifierErrorKind> {
    let target = pc as isize + offset as isize + 1;
    if target < 0 || target >= len as isize {
        return Err(VerifierErrorKind::JumpOutOfBounds);
    }
    Ok(target as u32)
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn resolve_lddw(
    code: &[EbpfInstruction],
    pc: usize,
    src: u8,
    lower: u32,
) -> Result<u64, VerifierErrorKind> {
    if src != 0 {
        return Err(VerifierErrorKind::UnsupportedLddwSource(src));
    }
    let inst2 = *code.get(pc + 1).ok_or(VerifierErrorKind::IncompleteLddw)?;
    if inst2.code() != 0 || inst2.dst_reg() != 0 || inst2.src_reg() != 0 || inst2.offset() != 0 {
        return Err(VerifierErrorKind::MalformedLddw);
    }
    Ok(((inst2.imm() as u32 as u64) << 32) | (lower as u64))
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn resolve_patched_target_loc(
    target_pc: u32,
    exit_loc: u32,
    pc_locs: &[Option<u32>],
) -> Result<u32, JitError> {
    if target_pc == TARGET_PC_EXIT {
        return Ok(exit_loc);
    }
    match pc_locs.get(target_pc as usize).copied().flatten() {
        Some(loc) => Ok(loc),
        None => Err(JitError::InvalidJumpTarget { target_pc, len: pc_locs.len() }),
    }
}

/// Compiles relocated eBPF instructions into target architecture machine code.
pub fn compile(
    code: &[EbpfInstruction],
    helpers: &[HelperLinkage],
    packet_load_fn_addr: Option<u64>,
) -> Result<Vec<u8>, JitError> {
    arch::compile(code, helpers, packet_load_fn_addr)
}

#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64")))]
mod tests {
    use super::*;
    use crate::api::{BPF_CALL, BPF_EXIT, BPF_JMP, BPF_LDDW, BPF_PSEUDO_CALL};
    use crate::conformance::test::parse_asm;

    #[test]
    fn test_compile_simple_exit() {
        let insts = parse_asm(
            r#"
            mov %r0, 42
            exit
            "#,
        );
        let code = compile(&insts, &[], None).expect("compile");
        assert!(!code.is_empty());
    }

    #[test]
    fn test_compile_missing_helper_returns_err() {
        let insts = parse_asm(
            r#"
            call 99
            exit
            "#,
        );
        assert_eq!(
            compile(&insts, &[], None),
            Err(JitError::Verifier(VerifierErrorKind::UnknownHelper(99)))
        );
    }

    #[test]
    fn test_compile_missing_packet_load_fn_returns_err() {
        let insts = parse_asm(
            r#"
            ldpw
            exit
            "#,
        );
        assert_eq!(
            compile(&insts, &[], None),
            Err(JitError::Verifier(VerifierErrorKind::RegisterNotPacket(6)))
        );
    }

    #[test]
    fn test_compile_all_atomics_and_endianness() {
        let insts = parse_asm(
            r#"
            lock add32 [%r10-8], %r1
            lock fetch add32 [%r10-8], %r1
            lock and32 [%r10-8], %r1
            lock fetch and32 [%r10-8], %r1
            lock or32 [%r10-8], %r1
            lock fetch or32 [%r10-8], %r1
            lock xor32 [%r10-8], %r1
            lock fetch xor32 [%r10-8], %r1
            lock xchg32 [%r10-8], %r1
            lock cmpxchg32 [%r10-8], %r1
            lock add [%r10-8], %r1
            lock fetch add [%r10-8], %r1
            lock and [%r10-8], %r1
            lock fetch and [%r10-8], %r1
            lock or [%r10-8], %r1
            lock fetch or [%r10-8], %r1
            lock xor [%r10-8], %r1
            lock fetch xor [%r10-8], %r1
            lock xchg [%r10-8], %r1
            lock cmpxchg [%r10-8], %r1
            le16 %r0
            be16 %r0
            le32 %r0
            be32 %r0
            le64 %r0
            be64 %r0
            exit
            "#,
        );
        let code = compile(&insts, &[], None).expect("compile atomics and endianness");
        assert!(!code.is_empty());
    }

    #[test]
    fn test_compile_extreme_i16_offsets_and_atomics() {
        // Exercises `i16::MIN` (`-32768`) and `i16::MAX` (`32767`) memory and atomic offsets to
        // ensure negating `i16::MIN` never overflows in debug builds.
        let insts = parse_asm(
            r#"
            lock add [%r1-32768], %r2
            lock fetch add32 [%r1+32767], %r2
            ldxdw %r0, [%r1-32768]
            stxdw [%r1+32767], %r0
            exit
            "#,
        );
        let code = compile(&insts, &[], None).expect("compile extreme i16 offsets");
        assert!(!code.is_empty());
    }

    #[test]
    fn test_compile_rejects_malformed_call_lddw_and_mid_lddw_jump() {
        // Pseudo-call (`src_reg != 0`) must be rejected.
        let pseudo_call = [
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, BPF_PSEUDO_CALL, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            compile(&pseudo_call, &[], None),
            Err(JitError::Verifier(VerifierErrorKind::UnsupportedCallType(BPF_PSEUDO_CALL)))
        );

        // Unrelocated `BPF_LDDW` (`src_reg != 0`) must be rejected.
        let unrelocated_lddw = parse_asm(
            r#"
            lddw %r1, map[0]
            mov %r0, 0
            exit
            "#,
        );
        assert!(matches!(
            compile(&unrelocated_lddw, &[], None),
            Err(JitError::Verifier(VerifierErrorKind::UnsupportedLddwSource(_)))
        ));

        // Malformed second slot of `BPF_LDDW` (`code != 0`) must be rejected.
        let malformed_lddw = [
            EbpfInstruction::new(BPF_LDDW, 1, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            compile(&malformed_lddw, &[], None),
            Err(JitError::Verifier(VerifierErrorKind::MalformedLddw))
        );

        // Jumping into the second slot of a `BPF_LDDW` (`pc = 2`, whose `pc_locs` entry is `None`)
        // must be rejected during jump target resolution.
        let jump_into_lddw = parse_asm(
            r#"
            ja +1
            lddw %r0, 1
            exit
            "#,
        );
        assert_eq!(
            compile(&jump_into_lddw, &[], None),
            Err(JitError::InvalidJumpTarget { target_pc: 2, len: 4 })
        );
    }
}
