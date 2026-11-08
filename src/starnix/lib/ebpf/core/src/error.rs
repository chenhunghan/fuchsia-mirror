// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::api::EbpfMapType;
use crate::visitor::{ProgramCounter, Register};

pub const E2BIG: u32 = 7;
pub const EACCES: u32 = 13;
pub const EINVAL: u32 = 22;

/// An error reported by the eBPF verifier, bundling the error kind with the
/// instruction PC where it occurred (or `None` for program-wide errors).
#[derive(Debug, Copy)]
pub struct VerifierError {
    pub kind: VerifierErrorKind,
    pub pc: Option<ProgramCounter>,
}

// Explicit `Clone`, `PartialEq`, and `Eq` avoid derived `Option::eq` trait-clause indirection in
// LLBC / Aeneas translation.
impl Clone for VerifierError {
    fn clone(&self) -> Self {
        *self
    }
}

impl PartialEq for VerifierError {
    fn eq(&self, other: &Self) -> bool {
        if self.kind != other.kind {
            return false;
        }
        match (self.pc, other.pc) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for VerifierError {}

impl VerifierError {
    /// The Linux errno corresponding to this error.
    pub fn errno(&self) -> u32 {
        self.kind.errno()
    }
}

impl core::fmt::Display for VerifierError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "at PC {pc}: {}", self.kind),
            None => self.kind.fmt(f),
        }
    }
}

impl core::error::Error for VerifierError {}

/// Specific error kinds reported by the eBPF verifier.
///
/// Each variant maps to an errno that matches the one returned by the Linux
/// verifier for the equivalent failure, see [`VerifierErrorKind::errno`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum VerifierErrorKind {
    // Program-level limits.
    ProgramTooLarge,
    ComplexityLimitExceeded,

    // Malformed instructions.
    InvalidOpcode(u8),
    InvalidByteSwapWidth(i32),
    InvalidShift(i32),
    DivByZero,
    InvalidAtomicOp(u8),
    UnsupportedCallType(u8),
    IncompleteLddw,
    MalformedLddw,
    UnsupportedLddwSource(u8),
    InvalidRegister(Register),

    // Control flow.
    JumpOutOfBounds,
    FellOffProgramEnd,
    UnknownHelper(u32),

    // Map references in lddw.
    InvalidMapIndex(u32),
    LddwMapValueNotArray(u32, EbpfMapType),
    EmptyArrayMap,
    InvalidMapValueOffset(i32, u32),

    // Helper arguments.
    NonConstantBufferSize,
    ExpectedProgramArgument(u8),
    HelperArgumentTypeMismatch(usize),
    HelperArgumentNotMap(u8),
    InvalidMapTypeForHelper(EbpfMapType),

    // Resources.
    UseAfterRelease,
    DoubleRelease,
    ResourceLeak,

    // Memory access.
    WriteToReadOnlyMap,
    WriteToReadOnlyMemory,
    OutOfBoundsMemoryAccess,
    PointerOffsetOverflow,
    UninitializedPointerArithmetic,
    StoreToNonPointer,
    LoadFromNonPointer,
    InvalidStoredValue,

    // Struct access.
    InvalidStructFieldAccess,
    WriteToReadOnlyField,
    InconsistentStructFieldAccess,
    InconsistentMemoryAccess,

    // Stack access.
    VariableStackOffset,
    StackAccessOutOfBounds,
    MisalignedStackAccess,
    MisalignedAtomicAccess,
    AtomicAccessNotAllowed,
    UninitializedStackRead,
    NonScalarStackRead,
    PartialNonScalarStackStore,
    PartialNonScalarStackLoad,

    // Registers.
    WriteToReadOnlyRegister(Register),
    InvalidComparison,
    InvalidReturnValue,
    InvalidByteSwapTarget,

    // Packet access.
    RegisterNotPacket(Register),
    UninitializedPacketOffset,
}

impl core::fmt::Display for VerifierErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ProgramTooLarge => write!(f, "ebpf program too long"),
            Self::ComplexityLimitExceeded => write!(f, "verification complexity limit exceeded"),
            Self::InvalidOpcode(op) => write!(f, "invalid op code {op:x}"),
            Self::InvalidByteSwapWidth(w) => write!(f, "invalid byte swap width: {w}"),
            Self::InvalidShift(s) => write!(f, "invalid shift value: {s}"),
            Self::DivByZero => write!(f, "division by zero"),
            Self::InvalidAtomicOp(op) => write!(f, "invalid atomic operation {op:x}"),
            Self::UnsupportedCallType(t) => write!(f, "unsupported call type {t}"),
            Self::IncompleteLddw => write!(f, "lddw is missing its second instruction"),
            Self::MalformedLddw => write!(f, "second instruction of lddw has non-zero fields"),
            Self::UnsupportedLddwSource(src) => write!(f, "unsupported lddw source {src}"),
            Self::InvalidRegister(reg) => write!(f, "R{reg} is invalid"),
            Self::JumpOutOfBounds => write!(f, "jump outside of program"),
            Self::FellOffProgramEnd => write!(f, "execution falls off the end of the program"),
            Self::UnknownHelper(id) => write!(f, "unknown helper {id}"),
            Self::InvalidMapIndex(idx) => write!(f, "lddw with invalid map index: {idx}"),
            Self::LddwMapValueNotArray(idx, map_type) => {
                write!(
                    f,
                    "lddw map value requires an array map, map at index {idx} has type {map_type}"
                )
            }
            Self::EmptyArrayMap => write!(f, "array has no entry"),
            Self::InvalidMapValueOffset(off, size) => {
                write!(f, "map value offset {off} out of bounds for value_size {size}")
            }
            Self::NonConstantBufferSize => write!(f, "buffer size is not a constant"),
            Self::ExpectedProgramArgument(arg) => {
                write!(f, "helper expects program argument {arg}")
            }
            Self::HelperArgumentTypeMismatch(idx) => {
                write!(f, "incorrect type for helper argument {idx}")
            }
            Self::HelperArgumentNotMap(idx) => write!(f, "helper argument {idx} is not a map"),
            Self::InvalidMapTypeForHelper(t) => write!(f, "map type {t} not allowed in helper"),
            Self::UseAfterRelease => write!(f, "access to released resource"),
            Self::DoubleRelease => write!(f, "resource already released"),
            Self::ResourceLeak => write!(f, "some resources have not been released at exit time"),
            Self::WriteToReadOnlyMap => write!(f, "write into map forbidden"),
            Self::WriteToReadOnlyMemory => write!(f, "write into read-only memory forbidden"),
            Self::OutOfBoundsMemoryAccess => write!(f, "out of bounds memory access"),
            Self::PointerOffsetOverflow => write!(f, "pointer offset overflow"),
            Self::UninitializedPointerArithmetic => {
                write!(f, "pointer arithmetic with uninitialized scalar")
            }
            Self::StoreToNonPointer => write!(f, "store through a non-pointer value"),
            Self::LoadFromNonPointer => write!(f, "load through a non-pointer value"),
            Self::InvalidStoredValue => {
                write!(f, "cannot store a pointer or an uninitialized value to memory")
            }
            Self::InvalidStructFieldAccess => write!(f, "no struct field at this offset"),
            Self::WriteToReadOnlyField => write!(f, "store to a read-only field"),
            Self::InconsistentStructFieldAccess => write!(f, "inconsistent struct field access"),
            Self::InconsistentMemoryAccess => {
                write!(f, "instruction mixes struct and non-struct memory access")
            }
            Self::VariableStackOffset => write!(f, "variable stack offset"),
            Self::StackAccessOutOfBounds => write!(f, "stack access out of bounds"),
            Self::MisalignedStackAccess => write!(f, "misaligned stack access"),
            Self::MisalignedAtomicAccess => write!(f, "misaligned atomic access"),
            Self::AtomicAccessNotAllowed => {
                write!(f, "atomic access is allowed only to the stack and map values")
            }
            Self::UninitializedStackRead => write!(f, "reading unwritten value from the stack"),
            Self::NonScalarStackRead => write!(f, "reading non scalar value from the stack"),
            Self::PartialNonScalarStackStore => {
                write!(f, "cannot store part of a non scalar value on the stack")
            }
            Self::PartialNonScalarStackLoad => {
                write!(f, "cannot load part of a non scalar value from the stack")
            }
            Self::WriteToReadOnlyRegister(reg) => write!(f, "R{reg} is read-only"),
            Self::InvalidComparison => write!(f, "non permitted comparison"),
            Self::InvalidReturnValue => write!(f, "register 0 is incorrect at exit time"),
            Self::InvalidByteSwapTarget => {
                write!(f, "byte swap requires an initialized scalar register")
            }
            Self::RegisterNotPacket(reg) => write!(f, "R{reg} is not a packet"),
            Self::UninitializedPacketOffset => write!(f, "access to unwritten offset"),
        }
    }
}

impl core::error::Error for VerifierErrorKind {}

impl VerifierErrorKind {
    /// The Linux errno corresponding to this error.
    pub fn errno(&self) -> u32 {
        match self {
            // Program-level limits.
            Self::ProgramTooLarge | Self::ComplexityLimitExceeded => E2BIG,

            // Structural errors: malformed instructions, invalid control flow, and invalid
            // references to maps or helpers.
            Self::InvalidOpcode(_)
            | Self::InvalidByteSwapWidth(_)
            | Self::InvalidShift(_)
            | Self::DivByZero
            | Self::InvalidAtomicOp(_)
            | Self::UnsupportedCallType(_)
            | Self::IncompleteLddw
            | Self::MalformedLddw
            | Self::UnsupportedLddwSource(_)
            | Self::InvalidRegister(_)
            | Self::JumpOutOfBounds
            | Self::FellOffProgramEnd
            | Self::UnknownHelper(_)
            | Self::InvalidMapIndex(_)
            | Self::LddwMapValueNotArray(_, _)
            | Self::EmptyArrayMap
            | Self::InvalidMapValueOffset(_, _)
            | Self::InvalidMapTypeForHelper(_)
            | Self::ResourceLeak
            | Self::InconsistentMemoryAccess
            | Self::RegisterNotPacket(_)
            | Self::PointerOffsetOverflow => EINVAL,

            // Type, bounds and permission violations.
            Self::NonConstantBufferSize
            | Self::ExpectedProgramArgument(_)
            | Self::HelperArgumentTypeMismatch(_)
            | Self::HelperArgumentNotMap(_)
            | Self::UseAfterRelease
            | Self::DoubleRelease
            | Self::WriteToReadOnlyMap
            | Self::WriteToReadOnlyMemory
            | Self::OutOfBoundsMemoryAccess
            | Self::UninitializedPointerArithmetic
            | Self::StoreToNonPointer
            | Self::LoadFromNonPointer
            | Self::InvalidStoredValue
            | Self::InvalidStructFieldAccess
            | Self::WriteToReadOnlyField
            | Self::InconsistentStructFieldAccess
            | Self::VariableStackOffset
            | Self::StackAccessOutOfBounds
            | Self::MisalignedStackAccess
            | Self::MisalignedAtomicAccess
            | Self::AtomicAccessNotAllowed
            | Self::UninitializedStackRead
            | Self::NonScalarStackRead
            | Self::PartialNonScalarStackStore
            | Self::PartialNonScalarStackLoad
            | Self::WriteToReadOnlyRegister(_)
            | Self::InvalidComparison
            | Self::InvalidReturnValue
            | Self::InvalidByteSwapTarget
            | Self::UninitializedPacketOffset => EACCES,
        }
    }
}

/// Errors reported during classic BPF to eBPF conversion.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CbpfError {
    ProgramTooLarge(usize),
    InvalidInstruction(u16),
    InvalidScratchOffset(u32),
    InvalidJumpOffset(u32),
}

impl core::fmt::Display for CbpfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ProgramTooLarge(len) => write!(f, "cBPF program too large: {len} instructions"),
            Self::InvalidInstruction(code) => {
                write!(f, "Invalid cBPF instruction code: 0x{code:x}")
            }
            Self::InvalidScratchOffset(offset) => {
                write!(f, "Invalid cBPF scratch memory offset: 0x{offset:x}")
            }
            Self::InvalidJumpOffset(offset) => {
                write!(f, "Invalid cBPF jump offset: 0x{offset:x}")
            }
        }
    }
}

impl core::error::Error for CbpfError {}

/// Detailed error kinds for struct field remapping during program linking.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StructMappingError {
    PcOutOfBounds { pc: ProgramCounter },
    TargetOffsetOutOfRange { pc: ProgramCounter, target_offset: usize },
    SourceOffsetOutOfRange { pc: ProgramCounter, source_offset: usize },
    OffsetDifferenceOverflow { pc: ProgramCounter },
    OffsetOverflow { pc: ProgramCounter },
    Unmapped32BitField { pc: ProgramCounter },
}

impl core::fmt::Display for StructMappingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PcOutOfBounds { pc } => {
                write!(f, "Struct field access PC {pc} out of bounds")
            }
            Self::TargetOffsetOutOfRange { pc, target_offset } => {
                write!(f, "Target struct field offset {target_offset} out of range at PC {pc}")
            }
            Self::SourceOffsetOutOfRange { pc, source_offset } => {
                write!(f, "Source struct field offset {source_offset} out of range at PC {pc}")
            }
            Self::OffsetDifferenceOverflow { pc } => {
                write!(f, "Struct field offset difference overflow at PC {pc}")
            }
            Self::OffsetOverflow { pc } => {
                write!(f, "Struct field offset overflow at PC {pc}")
            }
            Self::Unmapped32BitField { pc } => {
                write!(f, "32-bit field isn't mapped at pc {pc}")
            }
        }
    }
}

impl core::error::Error for StructMappingError {}

/// Detailed error kinds for map linkage during program linking.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MapLinkageError {
    MapCountMismatch { expected: usize, actual: usize },
    NegativeMapIndex { imm: i32, pc: ProgramCounter },
    InvalidMapIndex { map_index: usize },
    MapValueRequiresNonEmptyArray { map_index: usize },
    InvalidMapValueOffset { offset: i32, map_index: usize },
    MissingMapDataAddr { map_index: usize },
    MapValueOffsetOverflow,
}

impl core::fmt::Display for MapLinkageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MapCountMismatch { expected, actual } => {
                write!(f, "Map count mismatch: expected {expected}, got {actual}")
            }
            Self::NegativeMapIndex { imm, pc } => {
                write!(f, "Negative map_index={imm} at pc {pc}")
            }
            Self::InvalidMapIndex { map_index } => {
                write!(f, "Invalid map_index: {map_index}")
            }
            Self::MapValueRequiresNonEmptyArray { map_index } => {
                write!(
                    f,
                    "BPF_PSEUDO_MAP_IDX_VALUE requires non-empty array map at index {map_index}"
                )
            }
            Self::InvalidMapValueOffset { offset, map_index } => {
                write!(f, "Invalid map value offset {offset} for map at index {map_index}")
            }
            Self::MissingMapDataAddr { map_index } => {
                write!(f, "Unable to get value at 0 for map at index {map_index}")
            }
            Self::MapValueOffsetOverflow => {
                write!(f, "Unable to use offset for map access")
            }
        }
    }
}

impl core::error::Error for MapLinkageError {}

/// Detailed error kinds for malformed instructions encountered during program linking.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ProgramLinkError {
    UnsupportedCallSrcReg { src_reg: u8, pc: ProgramCounter },
    IncompleteLddw { pc: ProgramCounter },
    MalformedLddw { pc: ProgramCounter },
    UnsupportedLddwSrcReg { src_reg: u8 },
}

impl core::fmt::Display for ProgramLinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedCallSrcReg { src_reg, pc } => {
                write!(f, "Unsupported src_reg={src_reg} in call instruction at pc {pc}")
            }
            Self::IncompleteLddw { pc } => {
                write!(f, "Incomplete lddw instruction at pc {pc}")
            }
            Self::MalformedLddw { pc } => {
                write!(f, "Malformed lddw instruction at pc {pc}")
            }
            Self::UnsupportedLddwSrcReg { src_reg } => {
                write!(f, "Unsupported value for src_reg in lddw: {src_reg}")
            }
        }
    }
}

impl core::error::Error for ProgramLinkError {}

/// Specific error kinds reported when linking a verified eBPF program against
/// target-process linkage metadata.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LinkError {
    InvalidStructMapping(StructMappingError),
    InvalidMapLinkage(MapLinkageError),
    MissingHelper(u32),
    MissingPacketLoadFn,
    InvalidProgram(ProgramLinkError),
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidStructMapping(err) => write!(f, "Invalid struct field mapping: {err}"),
            Self::InvalidMapLinkage(err) => write!(f, "Invalid map linkage: {err}"),
            Self::MissingHelper(id) => write!(f, "Missing implementation for helper with id={id}"),
            Self::MissingPacketLoadFn => {
                write!(f, "Missing packet_load_fn_addr for packet load instruction")
            }
            Self::InvalidProgram(err) => write!(f, "Invalid program instruction: {err}"),
        }
    }
}

impl core::error::Error for LinkError {}
