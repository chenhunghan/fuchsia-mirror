// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::error::*;

use static_assertions::const_assert_eq;

const_assert_eq!(E2BIG, linux_uapi::E2BIG);
const_assert_eq!(EACCES, linux_uapi::EACCES);
const_assert_eq!(EINVAL, linux_uapi::EINVAL);

/// Specific error kinds reported when JIT-compiling relocated eBPF instructions into target
/// machine code.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum JitError {
    Verifier(VerifierErrorKind),
    UnsupportedArchitecture,
    InvalidJumpTarget { target_pc: u32, len: usize },
    BranchDisplacementOutOfRange { offset: usize, displacement: i64 },
    UnexpectedBranchInstruction { instruction: u32 },
    CodeBufferOverflow,
}

impl From<VerifierErrorKind> for JitError {
    fn from(err: VerifierErrorKind) -> Self {
        Self::Verifier(err)
    }
}

impl core::fmt::Display for JitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Verifier(err) => write!(f, "{err}"),
            Self::UnsupportedArchitecture => {
                write!(f, "JIT compilation is not supported on this architecture")
            }
            Self::InvalidJumpTarget { target_pc, len } => {
                write!(f, "Invalid jump target pc {target_pc} (len {len})")
            }
            Self::BranchDisplacementOutOfRange { offset, displacement } => {
                write!(f, "Branch displacement {displacement} at offset {offset} out of range")
            }
            Self::UnexpectedBranchInstruction { instruction } => {
                write!(f, "Unexpected branch instruction for patching: {instruction:#x}")
            }
            Self::CodeBufferOverflow => write!(f, "JIT code buffer size overflow"),
        }
    }
}

impl core::error::Error for JitError {}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum EbpfError {
    #[error("Verification error loading program: {0}")]
    ProgramVerifyError(#[from] VerifierError),

    #[error("Failed to link program: {0}")]
    ProgramLinkError(String),

    #[error("cBPF program too large: {0} instructions")]
    CbpfProgramTooLarge(usize),

    #[error("Invalid cBPF instruction code: 0x{0:x}")]
    InvalidCbpfInstruction(u16),

    #[error("Invalid cBPF scratch memory offset: 0x{0:x}")]
    InvalidCbpfScratchOffset(u32),

    #[error("Invalid cBPF jump offset: 0x{0:x}")]
    InvalidCbpfJumpOffset(u32),
}

impl From<CbpfError> for EbpfError {
    fn from(err: CbpfError) -> Self {
        match err {
            CbpfError::ProgramTooLarge(len) => Self::CbpfProgramTooLarge(len),
            CbpfError::InvalidInstruction(code) => Self::InvalidCbpfInstruction(code),
            CbpfError::InvalidScratchOffset(offset) => Self::InvalidCbpfScratchOffset(offset),
            CbpfError::InvalidJumpOffset(offset) => Self::InvalidCbpfJumpOffset(offset),
        }
    }
}

impl From<LinkError> for EbpfError {
    fn from(err: LinkError) -> Self {
        Self::ProgramLinkError(err.to_string())
    }
}

impl From<JitError> for EbpfError {
    fn from(err: JitError) -> Self {
        Self::ProgramLinkError(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verifier_error_errno() {
        // Program-level limits map to E2BIG.
        assert_eq!(VerifierErrorKind::ProgramTooLarge.errno(), E2BIG);
        assert_eq!(VerifierErrorKind::ComplexityLimitExceeded.errno(), E2BIG);
        assert_eq!(
            VerifierError { kind: VerifierErrorKind::ProgramTooLarge, pc: None }.errno(),
            E2BIG
        );

        // Structural errors map to EINVAL.
        assert_eq!(VerifierErrorKind::InvalidOpcode(0xff).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidByteSwapWidth(8).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidShift(32).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::DivByZero.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidAtomicOp(0xff).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnsupportedCallType(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::IncompleteLddw.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::MalformedLddw.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnsupportedLddwSource(99).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidRegister(11).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::JumpOutOfBounds.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::FellOffProgramEnd.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::UnknownHelper(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidMapIndex(0).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::LddwMapValueNotArray(0, 1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::EmptyArrayMap.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidMapValueOffset(-4, 8).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InvalidMapTypeForHelper(1).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::ResourceLeak.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::InconsistentMemoryAccess.errno(), EINVAL);
        assert_eq!(VerifierErrorKind::RegisterNotPacket(6).errno(), EINVAL);
        assert_eq!(VerifierErrorKind::PointerOffsetOverflow.errno(), EINVAL);

        // Type, bounds, and permission violations map to EACCES.
        assert_eq!(VerifierErrorKind::NonConstantBufferSize.errno(), EACCES);
        assert_eq!(VerifierErrorKind::ExpectedProgramArgument(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::HelperArgumentTypeMismatch(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::HelperArgumentNotMap(0).errno(), EACCES);
        assert_eq!(VerifierErrorKind::UseAfterRelease.errno(), EACCES);
        assert_eq!(VerifierErrorKind::DoubleRelease.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyMap.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyMemory.errno(), EACCES);
        assert_eq!(VerifierErrorKind::OutOfBoundsMemoryAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedPointerArithmetic.errno(), EACCES);
        assert_eq!(VerifierErrorKind::StoreToNonPointer.errno(), EACCES);
        assert_eq!(VerifierErrorKind::LoadFromNonPointer.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidStoredValue.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidStructFieldAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyField.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InconsistentStructFieldAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::VariableStackOffset.errno(), EACCES);
        assert_eq!(VerifierErrorKind::StackAccessOutOfBounds.errno(), EACCES);
        assert_eq!(VerifierErrorKind::MisalignedStackAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::MisalignedAtomicAccess.errno(), EACCES);
        assert_eq!(VerifierErrorKind::AtomicAccessNotAllowed.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedStackRead.errno(), EACCES);
        assert_eq!(VerifierErrorKind::NonScalarStackRead.errno(), EACCES);
        assert_eq!(VerifierErrorKind::PartialNonScalarStackStore.errno(), EACCES);
        assert_eq!(VerifierErrorKind::PartialNonScalarStackLoad.errno(), EACCES);
        assert_eq!(VerifierErrorKind::WriteToReadOnlyRegister(10).errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidComparison.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidReturnValue.errno(), EACCES);
        assert_eq!(VerifierErrorKind::InvalidByteSwapTarget.errno(), EACCES);
        assert_eq!(VerifierErrorKind::UninitializedPacketOffset.errno(), EACCES);
    }
}
