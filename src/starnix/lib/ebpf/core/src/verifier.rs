// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(unused_variables)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::api::{
    ARGUMENT_COUNT, BPF_ATOMIC, BPF_CALL, BPF_CLS_MASK, BPF_CMPXCHG, BPF_DW, BPF_EXIT, BPF_JA,
    BPF_JMP, BPF_JMP32, BPF_LDDW, BPF_MAP_TYPE_ARRAY, BPF_MAX_INSTS, BPF_PSEUDO_MAP_IDX,
    BPF_PSEUDO_MAP_IDX_VALUE, BPF_STACK_SIZE, BPF_STX, BPF_SUB_OP_MASK, BPF_W, DataWidth,
    EbpfInstruction, EbpfMapType, GENERAL_REGISTER_COUNT, REGISTER_COUNT, apply_endianness,
};
use crate::error::{VerifierError, VerifierErrorKind};
use crate::maps::{MapFlags, MapSchema};
use crate::scalar_value::{ScalarValueData, U32Range, U32ScalarValueData, U64Range};
use crate::visitor::{BpfVisitor, ProgramCounter, Register, Source};

const U32_MAX: u64 = u32::MAX as u64;

/// Maximum allowed offset for `PtrToArray` (packet) pointers.
///
/// Soundness only requires bounding offsets enough to prevent 64-bit pointer
/// wraparound at runtime before comparing against `data_end`, so this limit
/// could potentially be much larger; `0xffff` (64 KiB - 1) is chosen to match
/// Linux's `MAX_PACKET_OFF`.
pub const MAX_PACKET_OFFSET: u64 = 0xffff;

const STACK_ELEMENT_SIZE: usize = 8;

#[inline]
const fn min_u64(a: u64, b: u64) -> u64 {
    if a <= b { a } else { b }
}

#[inline]
const fn max_u64(a: u64, b: u64) -> u64 {
    if a >= b { a } else { b }
}

#[inline]
const fn cmp_u8(a: u8, b: u8) -> Ordering {
    if a < b {
        Ordering::Less
    } else if a > b {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

#[inline]
const fn cmp_u64(a: u64, b: u64) -> Ordering {
    if a < b {
        Ordering::Less
    } else if a > b {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

#[inline]
fn popcount_u64(mut x: u64) -> usize {
    let mut count = 0usize;
    while x != 0 {
        x &= x - 1;
        count += 1;
    }
    count
}

#[inline]
fn checked_add_signed_usize(base: usize, delta: isize) -> Option<usize> {
    if delta >= 0 {
        base.checked_add(delta as usize)
    } else {
        let abs_delta = (0isize.wrapping_sub(delta)) as usize;
        base.checked_sub(abs_delta)
    }
}

fn associate_orderings(o1: Ordering, o2: Ordering) -> Option<Ordering> {
    match (o1, o2) {
        (Ordering::Equal, o) | (o, Ordering::Equal) => Some(o),
        (Ordering::Less, Ordering::Less) => Some(Ordering::Less),
        (Ordering::Greater, Ordering::Greater) => Some(Ordering::Greater),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluLogOp {
    Add32,
    Add64,
    And32,
    And64,
    Arsh32,
    Arsh64,
    Div32,
    Div64,
    Lsh32,
    Lsh64,
    Mod32,
    Mod64,
    Mov32,
    Mov64,
    Mul32,
    Mul64,
    Or32,
    Or64,
    Rsh32,
    Rsh64,
    Sub32,
    Sub64,
    Xor32,
    Xor64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JmpLogOp {
    Jeq32,
    Jeq64,
    Jne32,
    Jne64,
    Jge32,
    Jge64,
    Jgt32,
    Jgt64,
    Jle32,
    Jle64,
    Jlt32,
    Jlt64,
    Jsge32,
    Jsge64,
    Jsgt32,
    Jsgt64,
    Jsle32,
    Jsle64,
    Jslt32,
    Jslt64,
    Jset32,
    Jset64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicLogOp {
    Add32,
    Add64,
    And32,
    And64,
    Or32,
    Or64,
    Xor32,
    Xor64,
    Xchg32,
    Xchg64,
    Cmpxchg32,
    Cmpxchg64,
}

/// Format-free structured log events emitted by the core verifier.
#[derive(Copy, Debug)]
pub enum VerifierLogEvent {
    Alu { op: AluLogOp, dst: Register, src: Source },
    Neg { is_32: bool, dst: Register },
    Endian { to_be: bool, width: DataWidth, dst: Register },
    Call { index: u32 },
    Exit,
    Jump { offset: i16 },
    CondJump { op: JmpLogOp, dst: Register, src: Source, offset: i16 },
    Atomic { op: AtomicLogOp, fetch: bool, dst: Register, offset: i16, src: Register },
    Load { width: DataWidth, dst: Register, src: Register, offset: i16 },
    LddwImm { dst: Register, value: u64 },
    LddwMapByIndex { dst: Register, map_index: u32 },
    LddwMapValueByIndex { dst: Register, map_index: u32, offset: u32 },
    LoadFromPacket { width: DataWidth, offset: i32, register_offset: Option<Register> },
    StoreReg { width: DataWidth, dst: Register, offset: i16, src: Register },
    StoreImm { width: DataWidth, dst: Register, offset: i16, value: u64 },
}

impl Clone for VerifierLogEvent {
    fn clone(&self) -> Self {
        *self
    }
}

impl PartialEq for VerifierLogEvent {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Alu { op: o1, dst: d1, src: s1 }, Self::Alu { op: o2, dst: d2, src: s2 }) => {
                o1 == o2 && d1 == d2 && s1 == s2
            }
            (Self::Neg { is_32: b1, dst: d1 }, Self::Neg { is_32: b2, dst: d2 }) => {
                b1 == b2 && d1 == d2
            }
            (
                Self::Endian { to_be: b1, width: w1, dst: d1 },
                Self::Endian { to_be: b2, width: w2, dst: d2 },
            ) => b1 == b2 && w1 == w2 && d1 == d2,
            (Self::Call { index: i1 }, Self::Call { index: i2 }) => i1 == i2,
            (Self::Exit, Self::Exit) => true,
            (Self::Jump { offset: o1 }, Self::Jump { offset: o2 }) => o1 == o2,
            (
                Self::CondJump { op: o1, dst: d1, src: s1, offset: off1 },
                Self::CondJump { op: o2, dst: d2, src: s2, offset: off2 },
            ) => o1 == o2 && d1 == d2 && s1 == s2 && off1 == off2,
            (
                Self::Atomic { op: o1, fetch: f1, dst: d1, offset: off1, src: s1 },
                Self::Atomic { op: o2, fetch: f2, dst: d2, offset: off2, src: s2 },
            ) => o1 == o2 && f1 == f2 && d1 == d2 && off1 == off2 && s1 == s2,
            (
                Self::Load { width: w1, dst: d1, src: s1, offset: off1 },
                Self::Load { width: w2, dst: d2, src: s2, offset: off2 },
            ) => w1 == w2 && d1 == d2 && s1 == s2 && off1 == off2,
            (Self::LddwImm { dst: d1, value: v1 }, Self::LddwImm { dst: d2, value: v2 }) => {
                d1 == d2 && v1 == v2
            }
            (
                Self::LddwMapByIndex { dst: d1, map_index: m1 },
                Self::LddwMapByIndex { dst: d2, map_index: m2 },
            ) => d1 == d2 && m1 == m2,
            (
                Self::LddwMapValueByIndex { dst: d1, map_index: m1, offset: off1 },
                Self::LddwMapValueByIndex { dst: d2, map_index: m2, offset: off2 },
            ) => d1 == d2 && m1 == m2 && off1 == off2,
            (
                Self::LoadFromPacket { width: w1, offset: off1, register_offset: r1 },
                Self::LoadFromPacket { width: w2, offset: off2, register_offset: r2 },
            ) => {
                w1 == w2
                    && off1 == off2
                    && match (r1, r2) {
                        (Some(a), Some(b)) => a == b,
                        (None, None) => true,
                        _ => false,
                    }
            }
            (
                Self::StoreReg { width: w1, dst: d1, offset: off1, src: s1 },
                Self::StoreReg { width: w2, dst: d2, offset: off2, src: s2 },
            ) => w1 == w2 && d1 == d2 && off1 == off2 && s1 == s2,
            (
                Self::StoreImm { width: w1, dst: d1, offset: off1, value: v1 },
                Self::StoreImm { width: w2, dst: d2, offset: off2, value: v2 },
            ) => w1 == w2 && d1 == d2 && off1 == off2 && v1 == v2,
            _ => false,
        }
    }
}

impl Eq for VerifierLogEvent {}

/// Structured logger interface for the core eBPF verifier.
pub trait CoreVerifierLogger {
    fn is_enabled(&self) -> bool {
        true
    }

    fn log_instruction(&mut self, pc: ProgramCounter, opcode: u8, event: VerifierLogEvent);

    fn log_error(&mut self, error: &VerifierError);
}

/// A `CoreVerifierLogger` that drops all log events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NullCoreVerifierLogger;

impl CoreVerifierLogger for NullCoreVerifierLogger {
    fn is_enabled(&self) -> bool {
        false
    }

    fn log_instruction(&mut self, _pc: ProgramCounter, _opcode: u8, _event: VerifierLogEvent) {}

    fn log_error(&mut self, _error: &VerifierError) {}
}

/// The namespace of a `MemoryId`, used to prevent collisions between different
/// sources of identifiers.
#[doc(hidden)]
#[derive(Copy, Clone, Debug)]
pub enum Namespace {
    /// For static structure identifiers (e.g., `SkBuff`, `BpfSock`) defined in `program_type.rs`.
    Hardcoded,
    /// For IDs generated by `new_memory_id()` using the global counter.
    Generated,
    /// For IDs generated by `VerificationContext::next_id()` using the local counter.
    Verification,
}

impl Namespace {
    const fn tag(self) -> u8 {
        match self {
            Self::Hardcoded => 0,
            Self::Generated => 1,
            Self::Verification => 2,
        }
    }

    const fn cmp_ns(self, other: Self) -> Ordering {
        cmp_u8(self.tag(), other.tag())
    }
}

impl PartialEq for Namespace {
    fn eq(&self, other: &Self) -> bool {
        self.tag() == other.tag()
    }
}

impl Eq for Namespace {}

impl PartialOrd for Namespace {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp_ns(*other))
    }
}

impl Ord for Namespace {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_ns(*other)
    }
}

/// An identifier for a memory buffer accessible by an ebpf program. The identifiers are built as a
/// chain of unique identifier so that a buffer can contain multiple pointers to the same type and
/// the verifier can distinguish between the different instances.
#[derive(Debug)]
pub struct MemoryId {
    namespace: Namespace,
    id: u64,
    parent: Option<Box<MemoryId>>,
}

impl Clone for MemoryId {
    fn clone(&self) -> Self {
        let parent = match &self.parent {
            None => None,
            Some(p) => Some(Box::new((**p).clone())),
        };
        Self { namespace: self.namespace, id: self.id, parent }
    }
}

impl PartialEq for MemoryId {
    fn eq(&self, other: &Self) -> bool {
        if self.namespace != other.namespace || self.id != other.id {
            return false;
        }
        match (&self.parent, &other.parent) {
            (None, None) => true,
            (Some(p1), Some(p2)) => (**p1).eq(&**p2),
            _ => false,
        }
    }
}

impl Eq for MemoryId {}

impl PartialOrd for MemoryId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp_id(other))
    }
}

impl Ord for MemoryId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_id(other)
    }
}

impl From<u64> for MemoryId {
    fn from(id: u64) -> Self {
        Self::from_raw(id)
    }
}

impl MemoryId {
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Build a `MemoryId` from a raw u64.
    ///
    /// This is supposed to be used by the `ebpf_api` crate to build static identifiers (e.g.
    /// for hardcoded program contexts like `SkBuff`), but not elsewhere.
    pub const fn from_raw(id: u64) -> Self {
        Self { namespace: Namespace::Hardcoded, id, parent: None }
    }

    /// Build a `MemoryId` in the `Generated` namespace from an externally supplied counter value.
    ///
    /// External callers should use `ebpf::new_memory_id()` instead.
    #[doc(hidden)]
    pub const fn from_generated(id: u64) -> Self {
        Self { namespace: Namespace::Generated, id, parent: None }
    }

    /// Build a new id such that `other` is prepended to the chain of parent of `self`.
    #[doc(hidden)]
    pub fn prepended(&self, other: MemoryId) -> Self {
        match &self.parent {
            None => {
                MemoryId { namespace: self.namespace, id: self.id, parent: Some(Box::new(other)) }
            }
            Some(parent) => MemoryId {
                namespace: self.namespace,
                id: self.id,
                parent: Some(Box::new((**parent).prepended(other))),
            },
        }
    }

    /// Returns whether the parent of `self` is `parent`
    fn has_parent(&self, parent: &MemoryId) -> bool {
        match &self.parent {
            None => false,
            Some(p) => (**p).eq(parent),
        }
    }

    /// Returns whether `self` and `other` represent the same type. This is true if both have the
    /// same id and if they both have parent, parents must also match.
    #[doc(hidden)]
    pub fn matches(&self, other: &MemoryId) -> bool {
        if self.namespace != other.namespace || self.id != other.id {
            return false;
        }
        match (&self.parent, &other.parent) {
            (Some(p1), Some(p2)) => (**p1).matches(&**p2),
            _ => true,
        }
    }

    fn cmp_id(&self, other: &Self) -> Ordering {
        let ns_ord = self.namespace.cmp_ns(other.namespace);
        if !matches!(ns_ord, Ordering::Equal) {
            return ns_ord;
        }
        let id_ord = cmp_u64(self.id, other.id);
        if !matches!(id_ord, Ordering::Equal) {
            return id_ord;
        }
        match (&self.parent, &other.parent) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(p1), Some(p2)) => (**p1).cmp_id(&**p2),
        }
    }
}

/// The type of a field in a struct pointed by `Type::PtrToStruct`.
#[derive(Debug)]
pub enum FieldType {
    /// Read-only scalar value.
    Scalar { size: usize },
    /// Mutable scalar value.
    MutableScalar { size: usize },
    /// A pointer to the kernel memory. The full buffer is `buffer_size` bytes long.
    PtrToMemory { is_32_bit: bool, id: MemoryId, buffer_size: usize },
    /// A nullable pointer to the kernel memory. The full buffer is `buffer_size` bytes long.
    NullablePtrToMemory { is_32_bit: bool, id: MemoryId, buffer_size: usize },
    /// A pointer to the kernel memory. The full buffer size is determined by an instance of
    /// `PtrToEndArray` with the same `id`.
    PtrToArray { is_32_bit: bool, id: MemoryId },
    /// A pointer to the kernel memory that represents the first non accessible byte of a
    /// `PtrToArray` with the same `id`.
    PtrToEndArray { is_32_bit: bool, id: MemoryId },
}

impl Clone for FieldType {
    fn clone(&self) -> Self {
        match self {
            Self::Scalar { size } => Self::Scalar { size: *size },
            Self::MutableScalar { size } => Self::MutableScalar { size: *size },
            Self::PtrToMemory { is_32_bit, id, buffer_size } => Self::PtrToMemory {
                is_32_bit: *is_32_bit,
                id: id.clone(),
                buffer_size: *buffer_size,
            },
            Self::NullablePtrToMemory { is_32_bit, id, buffer_size } => Self::NullablePtrToMemory {
                is_32_bit: *is_32_bit,
                id: id.clone(),
                buffer_size: *buffer_size,
            },
            Self::PtrToArray { is_32_bit, id } => {
                Self::PtrToArray { is_32_bit: *is_32_bit, id: id.clone() }
            }
            Self::PtrToEndArray { is_32_bit, id } => {
                Self::PtrToEndArray { is_32_bit: *is_32_bit, id: id.clone() }
            }
        }
    }
}

impl PartialEq for FieldType {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Scalar { size: s1 }, Self::Scalar { size: s2 }) => *s1 == *s2,
            (Self::MutableScalar { size: s1 }, Self::MutableScalar { size: s2 }) => *s1 == *s2,
            (
                Self::PtrToMemory { is_32_bit: b1, id: id1, buffer_size: sz1 },
                Self::PtrToMemory { is_32_bit: b2, id: id2, buffer_size: sz2 },
            ) => *b1 == *b2 && *id1 == *id2 && *sz1 == *sz2,
            (
                Self::NullablePtrToMemory { is_32_bit: b1, id: id1, buffer_size: sz1 },
                Self::NullablePtrToMemory { is_32_bit: b2, id: id2, buffer_size: sz2 },
            ) => *b1 == *b2 && *id1 == *id2 && *sz1 == *sz2,
            (
                Self::PtrToArray { is_32_bit: b1, id: id1 },
                Self::PtrToArray { is_32_bit: b2, id: id2 },
            ) => *b1 == *b2 && *id1 == *id2,
            (
                Self::PtrToEndArray { is_32_bit: b1, id: id1 },
                Self::PtrToEndArray { is_32_bit: b2, id: id2 },
            ) => *b1 == *b2 && *id1 == *id2,
            _ => false,
        }
    }
}

impl Eq for FieldType {}

/// Definition of a field in a `Type::PtrToStruct`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDescriptor {
    /// The offset at which the field is located.
    pub offset: usize,

    /// The type of the pointed memory. Currently the verifier supports only `PtrToArray`,
    /// `PtrToStruct`, `PtrToEndArray` and `ScalarValue`.
    pub field_type: FieldType,
}

impl FieldDescriptor {
    fn size(&self) -> usize {
        match self.field_type {
            FieldType::Scalar { size } | FieldType::MutableScalar { size } => size,
            FieldType::PtrToMemory { is_32_bit, .. }
            | FieldType::NullablePtrToMemory { is_32_bit, .. }
            | FieldType::PtrToArray { is_32_bit, .. }
            | FieldType::PtrToEndArray { is_32_bit, .. } => {
                if is_32_bit {
                    4
                } else {
                    8
                }
            }
        }
    }
}

/// The offset and width of a field in a struct.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field {
    pub offset: i16,
    pub width: DataWidth,
}

impl Field {
    pub fn new(offset: i16, width: DataWidth) -> Self {
        Self { offset, width }
    }
}

/// Defines field layout in a struct pointed by `Type::PtrToStruct`.
#[derive(Debug)]
pub struct StructDescriptor {
    /// The list of fields.
    pub fields: Vec<FieldDescriptor>,
}

impl Default for StructDescriptor {
    fn default() -> Self {
        Self { fields: Vec::new() }
    }
}

impl Clone for StructDescriptor {
    fn clone(&self) -> Self {
        let mut fields = Vec::new();
        let mut i = 0;
        while i < self.fields.len() {
            fields.push(self.fields[i].clone());
            i += 1;
        }
        Self { fields }
    }
}

impl PartialEq for StructDescriptor {
    fn eq(&self, other: &Self) -> bool {
        if self.fields.len() != other.fields.len() {
            return false;
        }
        let mut i = 0;
        while i < self.fields.len() {
            if self.fields[i] != other.fields[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Eq for StructDescriptor {}

impl StructDescriptor {
    /// Return true if `self` is a subtype of `super_struct`.
    pub fn is_subtype(&self, super_struct: &StructDescriptor) -> bool {
        // Check that `super_struct.fields` is a subset of `fields`.
        let mut i = 0;
        while i < super_struct.fields.len() {
            let super_field = &super_struct.fields[i];
            let mut found = false;
            let mut j = 0;
            while j < self.fields.len() {
                if self.fields[j] == *super_field {
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Finds the field type for load/store at the specified location. None is returned if the
    /// access is invalid and the program must be rejected.
    pub fn find_field(
        &self,
        base_offset: ScalarValueData,
        field: Field,
    ) -> Option<&FieldDescriptor> {
        // Struct accesses must use a statically known, naturally aligned offset because
        // context fields are remapped by static offset at link time.
        let offset_scalar = base_offset.add_i16(field.offset);
        if !offset_scalar.is_known() {
            return None;
        }
        if offset_scalar.value > (usize::MAX as u64) {
            return None;
        }
        let offset = offset_scalar.value as usize;
        let width_bytes = field.width.bytes();
        if width_bytes == 0 || (offset % width_bytes) != 0 {
            return None;
        }
        let mut found: Option<&FieldDescriptor> = None;
        let mut i = 0;
        while i < self.fields.len() {
            let f = &self.fields[i];
            let Some(f_end) = f.offset.checked_add(f.size()) else {
                i += 1;
                continue;
            };
            if f.offset <= offset && offset < f_end {
                found = Some(f);
                break;
            }
            i += 1;
        }
        let Some(field_desc) = found else {
            return None;
        };
        let is_valid_load = match field_desc.field_type {
            FieldType::Scalar { size } | FieldType::MutableScalar { size } => {
                let Some(access_end) = offset.checked_add(width_bytes) else {
                    return None;
                };
                let Some(field_end) = field_desc.offset.checked_add(size) else {
                    return None;
                };
                access_end <= field_end
            }
            FieldType::PtrToMemory { is_32_bit, .. }
            | FieldType::NullablePtrToMemory { is_32_bit, .. }
            | FieldType::PtrToArray { is_32_bit, .. }
            | FieldType::PtrToEndArray { is_32_bit, .. } => {
                let expected_width = if is_32_bit { DataWidth::U32 } else { DataWidth::U64 };
                offset == field_desc.offset && field.width == expected_width
            }
        };
        if is_valid_load { Some(field_desc) } else { None }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MemoryParameterSize {
    /// The memory buffer have the given size.
    Value(u64),
    /// The memory buffer size is given by the parameter in the given index.
    Reference { index: u8 },
}

impl PartialEq for MemoryParameterSize {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Value(v1), Self::Value(v2)) => *v1 == *v2,
            (Self::Reference { index: i1 }, Self::Reference { index: i2 }) => *i1 == *i2,
            _ => false,
        }
    }
}

impl Eq for MemoryParameterSize {}

impl MemoryParameterSize {
    fn size(&self, context: &ComputationContext) -> Result<u64, VerifierErrorKind> {
        match self {
            Self::Value(size) => Ok(*size),
            Self::Reference { index } => {
                let Some(reg) = index.checked_add(1) else {
                    return Err(VerifierErrorKind::InvalidRegister(*index));
                };
                if reg > ARGUMENT_COUNT {
                    return Err(VerifierErrorKind::InvalidRegister(reg));
                }
                let size_type = match context.reg(reg) {
                    Ok(t) => t,
                    Err(err) => return Err(err),
                };
                match size_type {
                    Type::ScalarValue(data) if data.is_known() => Ok(data.value),
                    _ => Err(VerifierErrorKind::NonConstantBufferSize),
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum MapTypeFilter {
    AllowList(Vec<EbpfMapType>),
    DenyList(Vec<EbpfMapType>),
}

fn clone_vec_u32(v: &[EbpfMapType]) -> Vec<EbpfMapType> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        out.push(v[i]);
        i += 1;
    }
    out
}

impl Clone for MapTypeFilter {
    fn clone(&self) -> Self {
        match self {
            Self::AllowList(v) => Self::AllowList(clone_vec_u32(v)),
            Self::DenyList(v) => Self::DenyList(clone_vec_u32(v)),
        }
    }
}

fn slice_eq_u32(a: &[EbpfMapType], b: &[EbpfMapType]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

impl PartialEq for MapTypeFilter {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AllowList(a), Self::AllowList(b)) => slice_eq_u32(a, b),
            (Self::DenyList(a), Self::DenyList(b)) => slice_eq_u32(a, b),
            _ => false,
        }
    }
}

impl Eq for MapTypeFilter {}

fn slice_contains_u32(types: &[EbpfMapType], map_type: EbpfMapType) -> bool {
    let mut i = 0;
    while i < types.len() {
        if types[i] == map_type {
            return true;
        }
        i += 1;
    }
    false
}

impl MapTypeFilter {
    pub fn is_allowed(&self, map_type: EbpfMapType) -> bool {
        match self {
            Self::AllowList(types) => slice_contains_u32(types, map_type),
            Self::DenyList(types) => !slice_contains_u32(types, map_type),
        }
    }
}

/// Indicates the origin and mutability of a memory buffer referenced by
/// `Type::PtrToMemory`.
#[derive(Clone, Copy, Debug)]
pub enum PointerOrigin {
    /// A pointer to a value in a read-only eBPF map (`BPF_F_RDONLY_PROG`).
    MapReadOnly,
    /// A pointer to a value in a writable eBPF map.
    MapWritable,
    /// A read-only pointer to kernel memory (e.g., packet payload in
    /// `sk_buff`).
    KernelReadOnly,
    /// A writable pointer to kernel memory.
    KernelWritable,
}

impl PointerOrigin {
    const fn tag(self) -> u8 {
        match self {
            Self::MapReadOnly => 0,
            Self::MapWritable => 1,
            Self::KernelReadOnly => 2,
            Self::KernelWritable => 3,
        }
    }

    /// Returns true if the memory referenced by this pointer can be written to.
    pub fn is_writable(self) -> bool {
        match self {
            Self::MapWritable | Self::KernelWritable => true,
            Self::MapReadOnly | Self::KernelReadOnly => false,
        }
    }

    /// Returns true if the pointer points to an eBPF map value.
    pub fn is_map_value(self) -> bool {
        match self {
            Self::MapReadOnly | Self::MapWritable => true,
            Self::KernelReadOnly | Self::KernelWritable => false,
        }
    }
}

impl PartialEq for PointerOrigin {
    fn eq(&self, other: &Self) -> bool {
        self.tag() == other.tag()
    }
}

impl Eq for PointerOrigin {}

// ============================================================================
// Sorted IdSet and BoundsMap
// ============================================================================

/// A sorted set of `MemoryId`s backed by a `Vec` instead of `BTreeSet` so the
/// core verifier stays within the Charon/Aeneas subset for Lean 4 translation.
#[doc(hidden)]
#[derive(Debug)]
pub struct IdSet {
    entries: Vec<MemoryId>,
}

impl Default for IdSet {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for IdSet {
    fn clone(&self) -> Self {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            entries.push(self.entries[i].clone());
            i += 1;
        }
        Self { entries }
    }
}

impl PartialEq for IdSet {
    fn eq(&self, other: &Self) -> bool {
        if self.entries.len() != other.entries.len() {
            return false;
        }
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i] != other.entries[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Eq for IdSet {}

impl IdSet {
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
    }

    pub fn contains(&self, id: &MemoryId) -> bool {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries[i].cmp_id(id) {
                Ordering::Equal => return true,
                Ordering::Greater => return false,
                Ordering::Less => {}
            }
            i += 1;
        }
        false
    }

    pub fn insert(&mut self, id: MemoryId) -> bool {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries[i].cmp_id(&id) {
                Ordering::Equal => return false,
                Ordering::Greater => break,
                Ordering::Less => {}
            }
            i += 1;
        }
        let mut new_entries = Vec::new();
        let mut pos = 0;
        while pos < i {
            new_entries.push(self.entries[pos].clone());
            pos += 1;
        }
        new_entries.push(id);
        while pos < self.entries.len() {
            new_entries.push(self.entries[pos].clone());
            pos += 1;
        }
        self.entries = new_entries;
        true
    }

    pub fn remove(&mut self, id: &MemoryId) -> bool {
        let mut i = 0;
        let mut found = false;
        while i < self.entries.len() {
            match self.entries[i].cmp_id(id) {
                Ordering::Equal => {
                    found = true;
                    break;
                }
                Ordering::Greater => return false,
                Ordering::Less => {}
            }
            i += 1;
        }
        if !found {
            return false;
        }
        let mut new_entries = Vec::new();
        let mut pos = 0;
        while pos < i {
            new_entries.push(self.entries[pos].clone());
            pos += 1;
        }
        pos = i + 1;
        while pos < self.entries.len() {
            new_entries.push(self.entries[pos].clone());
            pos += 1;
        }
        self.entries = new_entries;
        true
    }
}

/// A sorted map from `MemoryId` to `u64` array bounds backed by a `Vec` instead
/// of `BTreeMap` so the core verifier stays within the Charon/Aeneas subset for
/// Lean 4 translation.
#[doc(hidden)]
#[derive(Debug)]
pub struct BoundsMap {
    entries: Vec<(MemoryId, u64)>,
}

impl Default for BoundsMap {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for BoundsMap {
    fn clone(&self) -> Self {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            entries.push((self.entries[i].0.clone(), self.entries[i].1));
            i += 1;
        }
        Self { entries }
    }
}

impl PartialEq for BoundsMap {
    fn eq(&self, other: &Self) -> bool {
        if self.entries.len() != other.entries.len() {
            return false;
        }
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].0 != other.entries[i].0 || self.entries[i].1 != other.entries[i].1 {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Eq for BoundsMap {}

impl BoundsMap {
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
    }

    pub fn get(&self, id: &MemoryId) -> Option<&u64> {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries[i].0.cmp_id(id) {
                Ordering::Equal => return Some(&self.entries[i].1),
                Ordering::Greater => return None,
                Ordering::Less => {}
            }
            i += 1;
        }
        None
    }

    pub fn get_or_zero(&self, id: &MemoryId) -> u64 {
        match self.get(id) {
            Some(v) => *v,
            None => 0,
        }
    }

    pub fn insert(&mut self, id: MemoryId, val: u64) -> Option<u64> {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries[i].0.cmp_id(&id) {
                Ordering::Equal => {
                    let prev = self.entries[i].1;
                    self.entries[i].1 = val;
                    return Some(prev);
                }
                Ordering::Greater => break,
                Ordering::Less => {}
            }
            i += 1;
        }
        let mut new_entries = Vec::new();
        let mut pos = 0;
        while pos < i {
            new_entries.push((self.entries[pos].0.clone(), self.entries[pos].1));
            pos += 1;
        }
        new_entries.push((id, val));
        while pos < self.entries.len() {
            new_entries.push((self.entries[pos].0.clone(), self.entries[pos].1));
            pos += 1;
        }
        self.entries = new_entries;
        None
    }

    pub fn update_max(&mut self, id: MemoryId, new_bound_min: u64) {
        let mut i = 0;
        while i < self.entries.len() {
            match self.entries[i].0.cmp_id(&id) {
                Ordering::Equal => {
                    self.entries[i].1 = max_u64(self.entries[i].1, new_bound_min);
                    return;
                }
                Ordering::Greater => break,
                Ordering::Less => {}
            }
            i += 1;
        }
        let mut new_entries = Vec::new();
        let mut pos = 0;
        while pos < i {
            new_entries.push((self.entries[pos].0.clone(), self.entries[pos].1));
            pos += 1;
        }
        new_entries.push((id, new_bound_min));
        while pos < self.entries.len() {
            new_entries.push((self.entries[pos].0.clone(), self.entries[pos].1));
            pos += 1;
        }
        self.entries = new_entries;
    }
}

/// An abstract value or helper parameter specification tracked by the verifier.
#[derive(Debug)]
pub enum Type {
    /// A number.
    ScalarValue(ScalarValueData),
    /// A pointer to a map object.
    ConstPtrToMap { id: u64, schema: MapSchema },
    /// A pointer into the stack.
    PtrToStack { offset: StackOffset },
    /// A pointer to the kernel memory. The full buffer is `buffer_size` bytes long. The pointer is
    /// situated at `offset` from the start of the buffer.
    PtrToMemory { id: MemoryId, offset: ScalarValueData, buffer_size: u64, origin: PointerOrigin },
    /// A pointer to a struct with the specified `StructDescriptor`.
    PtrToStruct { id: MemoryId, offset: ScalarValueData, descriptor: Box<StructDescriptor> },
    /// A pointer to the kernel memory. The full buffer size is determined by an instance of
    /// `PtrToEndArray` with the same `id`. The pointer is situadted at `offset` from the start of
    /// the buffer.
    PtrToArray { id: MemoryId, offset: ScalarValueData },
    /// A pointer to the kernel memory that represents the first non accessible byte of a
    /// `PtrToArray` with the same `id`.
    PtrToEndArray { id: MemoryId },
    /// A pointer that might be null and must be validated before being referenced.
    NullOr { id: MemoryId, inner: Box<Type> },
    /// An object that must be passed to a method with an associated `ReleaseParameter` before the
    /// end of the program.
    Releasable { id: MemoryId, inner: Box<Type> },
    /// A function parameter that must be a `ScalarValue` when called.
    ScalarValueParameter,
    /// A function parameter that must be a `ConstPtrToMap` when called.
    ConstPtrToMapParameter { filter: MapTypeFilter, must_be_writable: bool },
    /// A function parameter that must be a key of a map.
    MapKeyParameter {
        /// The index in the arguments list that contains a `ConstPtrToMap` for the map this key is
        /// associated with.
        map_ptr_index: u8,
    },
    /// A function parameter that must be a value of a map.
    MapValueParameter {
        /// The index in the arguments list that contains a `ConstPtrToMap` for the map this key is
        /// associated with.
        map_ptr_index: u8,
    },
    /// A function parameter that must be a pointer to memory.
    MemoryParameter {
        /// The index in the arguments list that contains a scalar value containing the size of the
        /// memory.
        size: MemoryParameterSize,
        /// Whether this memory is read by the function.
        input: bool,
        /// Whether this memory is written by the function.
        output: bool,
    },
    /// A function return value that is either null, or the given type.
    NullOrParameter(Box<Type>),
    /// A function parameter that must be a pointer to memory with the given id.
    StructParameter { id: MemoryId },
    /// The parameter should be the same as the specified argument passed to the program.
    ContextParameter { parameter_index: u8 },
    /// A function return value that must be passed to a method with an associated
    /// `ReleaseParameter` before the end of the program.
    ReleasableParameter { id: MemoryId, inner: Box<Type> },
    /// A function parameter that will release the value.
    ReleaseParameter { id: MemoryId },
    /// A function parameter that is not checked.
    AnyParameter,
}

impl Clone for Type {
    fn clone(&self) -> Self {
        match self {
            Self::ScalarValue(d) => Self::ScalarValue(*d),
            Self::ConstPtrToMap { id, schema } => Self::ConstPtrToMap { id: *id, schema: *schema },
            Self::PtrToStack { offset } => Self::PtrToStack { offset: *offset },
            Self::PtrToMemory { id, offset, buffer_size, origin } => Self::PtrToMemory {
                id: id.clone(),
                offset: *offset,
                buffer_size: *buffer_size,
                origin: *origin,
            },
            Self::PtrToStruct { id, offset, descriptor } => Self::PtrToStruct {
                id: id.clone(),
                offset: *offset,
                descriptor: Box::new((**descriptor).clone()),
            },
            Self::PtrToArray { id, offset } => Self::PtrToArray { id: id.clone(), offset: *offset },
            Self::PtrToEndArray { id } => Self::PtrToEndArray { id: id.clone() },
            Self::NullOr { id, inner } => {
                Self::NullOr { id: id.clone(), inner: Box::new((**inner).clone()) }
            }
            Self::Releasable { id, inner } => {
                Self::Releasable { id: id.clone(), inner: Box::new((**inner).clone()) }
            }
            Self::ScalarValueParameter => Self::ScalarValueParameter,
            Self::ConstPtrToMapParameter { filter, must_be_writable } => {
                Self::ConstPtrToMapParameter {
                    filter: filter.clone(),
                    must_be_writable: *must_be_writable,
                }
            }
            Self::MapKeyParameter { map_ptr_index } => {
                Self::MapKeyParameter { map_ptr_index: *map_ptr_index }
            }
            Self::MapValueParameter { map_ptr_index } => {
                Self::MapValueParameter { map_ptr_index: *map_ptr_index }
            }
            Self::MemoryParameter { size, input, output } => {
                Self::MemoryParameter { size: *size, input: *input, output: *output }
            }
            Self::NullOrParameter(inner) => Self::NullOrParameter(Box::new((**inner).clone())),
            Self::StructParameter { id } => Self::StructParameter { id: id.clone() },
            Self::ContextParameter { parameter_index } => {
                Self::ContextParameter { parameter_index: *parameter_index }
            }
            Self::ReleasableParameter { id, inner } => {
                Self::ReleasableParameter { id: id.clone(), inner: Box::new((**inner).clone()) }
            }
            Self::ReleaseParameter { id } => Self::ReleaseParameter { id: id.clone() },
            Self::AnyParameter => Self::AnyParameter,
        }
    }
}

impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ScalarValue(a), Self::ScalarValue(b)) => *a == *b,
            (
                Self::ConstPtrToMap { id: id1, schema: s1 },
                Self::ConstPtrToMap { id: id2, schema: s2 },
            ) => *id1 == *id2 && *s1 == *s2,
            (Self::PtrToStack { offset: o1 }, Self::PtrToStack { offset: o2 }) => *o1 == *o2,
            (
                Self::PtrToMemory { id: id1, offset: o1, buffer_size: b1, origin: org1 },
                Self::PtrToMemory { id: id2, offset: o2, buffer_size: b2, origin: org2 },
            ) => *id1 == *id2 && *o1 == *o2 && *b1 == *b2 && *org1 == *org2,
            (
                Self::PtrToStruct { id: id1, offset: o1, descriptor: d1 },
                Self::PtrToStruct { id: id2, offset: o2, descriptor: d2 },
            ) => *id1 == *id2 && *o1 == *o2 && (**d1).eq(&**d2),
            (
                Self::PtrToArray { id: id1, offset: o1 },
                Self::PtrToArray { id: id2, offset: o2 },
            ) => *id1 == *id2 && *o1 == *o2,
            (Self::PtrToEndArray { id: id1 }, Self::PtrToEndArray { id: id2 }) => *id1 == *id2,
            (Self::NullOr { id: id1, inner: i1 }, Self::NullOr { id: id2, inner: i2 }) => {
                *id1 == *id2 && (**i1).eq(&**i2)
            }
            (Self::Releasable { id: id1, inner: i1 }, Self::Releasable { id: id2, inner: i2 }) => {
                *id1 == *id2 && (**i1).eq(&**i2)
            }
            (Self::ScalarValueParameter, Self::ScalarValueParameter) => true,
            (
                Self::ConstPtrToMapParameter { filter: f1, must_be_writable: w1 },
                Self::ConstPtrToMapParameter { filter: f2, must_be_writable: w2 },
            ) => *f1 == *f2 && *w1 == *w2,
            (
                Self::MapKeyParameter { map_ptr_index: i1 },
                Self::MapKeyParameter { map_ptr_index: i2 },
            ) => *i1 == *i2,
            (
                Self::MapValueParameter { map_ptr_index: i1 },
                Self::MapValueParameter { map_ptr_index: i2 },
            ) => *i1 == *i2,
            (
                Self::MemoryParameter { size: s1, input: in1, output: out1 },
                Self::MemoryParameter { size: s2, input: in2, output: out2 },
            ) => *s1 == *s2 && *in1 == *in2 && *out1 == *out2,
            (Self::NullOrParameter(i1), Self::NullOrParameter(i2)) => (**i1).eq(&**i2),
            (Self::StructParameter { id: id1 }, Self::StructParameter { id: id2 }) => *id1 == *id2,
            (
                Self::ContextParameter { parameter_index: i1 },
                Self::ContextParameter { parameter_index: i2 },
            ) => *i1 == *i2,
            (
                Self::ReleasableParameter { id: id1, inner: i1 },
                Self::ReleasableParameter { id: id2, inner: i2 },
            ) => *id1 == *id2 && (**i1).eq(&**i2),
            (Self::ReleaseParameter { id: id1 }, Self::ReleaseParameter { id: id2 }) => {
                *id1 == *id2
            }
            (Self::AnyParameter, Self::AnyParameter) => true,
            _ => false,
        }
    }
}

impl Eq for Type {}

/// Defines a partial ordering on `Type` instances, capturing the notion of how "broad"
/// a type is in terms of the set of potential values it represents.
///
/// The ordering is defined such that `t1 > t2` if a proof that an eBPF program terminates
/// in a state where a register or memory location has type `t1` is also a proof that
/// the program terminates in a state where that location has type `t2`.
///
/// In other words, a "broader" type represents a larger set of possible values, and
/// proving termination with a broader type implies termination with any narrower type.
///
/// Examples:
/// * `Type::ScalarValue(ScalarValueData({ unknown_mask: 0, .. }))` (a known scalar value) is less than
///   `Type::ScalarValue(ScalarValueData({ unknown_mask: u64::MAX, .. }))` (an unknown scalar value).
impl PartialOrd for Type {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        // UNINITIALIZED is treated as the Top element (Greater than all other types).
        // If a path is proven safe when a register is UNINITIALIZED, it means the path
        // does not read that register. Consequently, it is safe to prune any state
        // that has a more specific type (like a pointer) in that register, because
        // the program will still not read it.
        match (self, other) {
            (Self::ScalarValue(data1), Self::ScalarValue(data2)) => data1.partial_cmp(data2),
            (Self::ScalarValue(data), _) => {
                if *data == ScalarValueData::UNINITIALIZED {
                    Some(Ordering::Greater)
                } else {
                    None
                }
            }
            (_, Self::ScalarValue(data)) => {
                if *data == ScalarValueData::UNINITIALIZED {
                    Some(Ordering::Less)
                } else {
                    None
                }
            }
            _ => {
                if self == other {
                    Some(Ordering::Equal)
                } else {
                    None
                }
            }
        }
    }
}

impl From<ScalarValueData> for Type {
    fn from(value: ScalarValueData) -> Self {
        Self::ScalarValue(value)
    }
}

impl From<u64> for Type {
    fn from(value: u64) -> Self {
        Self::ScalarValue(ScalarValueData::from_known(value))
    }
}

impl Default for Type {
    /// A new instance of `Type` where no bit has been written yet.
    fn default() -> Self {
        Self::UNINITIALIZED
    }
}

impl Type {
    /// An uninitialized value.
    pub const UNINITIALIZED: Self = Self::ScalarValue(ScalarValueData::UNINITIALIZED);

    /// A `Type` where the data is usable by userspace, but the value is not known by the verifier.
    pub const UNKNOWN_SCALAR: Self = Self::ScalarValue(ScalarValueData::UNKNOWN_WRITTEN);

    /// The mask associated with a data of size `width`.
    fn mask(width: DataWidth) -> u64 {
        if width == DataWidth::U64 { u64::MAX } else { 1u64.wrapping_shl(width.bits() as u32) - 1 }
    }

    /// Return the underlying `ScalarValueData` if `self` is a scalar with all bytes initialized.
    fn written_scalar(&self) -> Option<ScalarValueData> {
        match self {
            Self::ScalarValue(data) if data.is_fully_initialized() => Some(*data),
            _ => None,
        }
    }

    /// Return true if `self` is a scalar with all bytes initialized.
    pub fn is_written_scalar(&self) -> bool {
        self.written_scalar().is_some()
    }

    /// Return true if `self` has all bytes initialized.
    pub fn is_initialized(&self) -> bool {
        match self {
            Self::ScalarValue(data) => data.is_fully_initialized(),
            _ => true,
        }
    }

    /// Return true if `self` is a subtype of `super_type`.
    pub fn is_subtype(&self, super_type: &Type) -> bool {
        match (self, super_type) {
            (_, Self::ScalarValue(data)) if data.is_uninitialized() => true,
            (
                Self::PtrToStruct { id: id1, offset: offset1, descriptor: descriptor1 },
                Self::PtrToStruct { id: id2, offset: offset2, descriptor: descriptor2 },
            ) => *id1 == *id2 && *offset1 <= *offset2 && (**descriptor1).is_subtype(&**descriptor2),
            _ => self == super_type,
        }
    }

    /// Return true is `self` is guaranteed to be non-zero
    pub fn is_non_zero(&self) -> bool {
        match self {
            Self::ScalarValue(d) => d.min() > 0,
            Self::PtrToStack { offset } => {
                offset.reg().is_fully_initialized() && offset.reg().max() <= (BPF_STACK_SIZE as u64)
            }
            Self::PtrToMemory { offset, buffer_size, .. } => {
                offset.is_fully_initialized() && offset.max() <= *buffer_size
            }
            Self::PtrToStruct { offset, .. } => offset.is_zero(),
            Self::Releasable { inner, .. } => (**inner).is_non_zero(),
            _ => false,
        }
    }

    fn check_not_released(&self, resources: &IdSet) -> Result<(), VerifierErrorKind> {
        match self {
            Self::Releasable { id, .. } => {
                if !resources.contains(id) {
                    return Err(VerifierErrorKind::UseAfterRelease);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn unwrap_releasable(&self) -> &Type {
        match self {
            Self::Releasable { inner, .. } => &**inner,
            _ => self,
        }
    }

    fn inner(&self, context: &ComputationContext) -> Result<&Type, VerifierErrorKind> {
        match self.check_not_released(&context.resources) {
            Ok(()) => Ok(self.unwrap_releasable()),
            Err(err) => Err(err),
        }
    }

    /// Returns a copy of this pointer with its offset replaced by `offset`, looking through
    /// `Releasable` so that the resource stays tracked. Other types are returned unchanged.
    #[doc(hidden)]
    pub fn with_offset(&self, offset: ScalarValueData) -> Self {
        match self {
            Self::Releasable { id, inner } => {
                Self::Releasable { id: id.clone(), inner: Box::new((**inner).with_offset(offset)) }
            }
            Self::PtrToStack { .. } => Self::PtrToStack { offset: StackOffset(offset) },
            Self::PtrToMemory { id, buffer_size, origin, .. } => Self::PtrToMemory {
                id: id.clone(),
                offset,
                buffer_size: *buffer_size,
                origin: *origin,
            },
            Self::PtrToStruct { id, descriptor, .. } => {
                Self::PtrToStruct { id: id.clone(), offset, descriptor: descriptor.clone() }
            }
            Self::PtrToArray { id, .. } => Self::PtrToArray { id: id.clone(), offset },
            _ => self.clone(),
        }
    }

    fn constrain_equal_offsets(
        context: &mut ComputationContext,
        type1: &Self,
        type2: &Self,
        offset1: ScalarValueData,
        offset2: ScalarValueData,
    ) -> Result<(Self, Self), VerifierErrorKind> {
        // Both pointers share the same base, so they are equal iff their offsets are
        // equal. Constrain the offsets, and keep the rest of each type (including any
        // `Releasable` wrapper) so that neither pointer loses information.
        let (new_offset1, new_offset2) = match Self::constraint(
            context,
            JumpType::Eq,
            JumpWidth::W64,
            Type::ScalarValue(offset1),
            Type::ScalarValue(offset2),
        ) {
            Ok(pair) => pair,
            Err(err) => return Err(err),
        };
        let resolved1 = match new_offset1 {
            Type::ScalarValue(offset) => offset,
            _ => offset1,
        };
        let resolved2 = match new_offset2 {
            Type::ScalarValue(offset) => offset,
            _ => offset2,
        };
        Ok((type1.with_offset(resolved1), type2.with_offset(resolved2)))
    }

    /// Constraints `type1` and `type2` for a conditional jump with the specified `jump_type` and
    /// `jump_width`.
    #[doc(hidden)]
    pub fn constraint(
        context: &mut ComputationContext,
        jump_type: JumpType,
        jump_width: JumpWidth,
        type1: Self,
        type2: Self,
    ) -> Result<(Self, Self), VerifierErrorKind> {
        match type1.check_not_released(&context.resources) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        match type2.check_not_released(&context.resources) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        let result =
            match (jump_width, jump_type, type1.unwrap_releasable(), type2.unwrap_releasable()) {
                (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::ScalarValue(data1),
                    Type::ScalarValue(data2),
                ) => {
                    if data1.is_fully_initialized()
                        && data2.is_fully_initialized()
                        && data1.can_be_equal(*data2)
                    {
                        let umin = max_u64(data1.min(), data2.min());
                        let umax = min_u64(data1.max(), data2.max());
                        let v = Type::ScalarValue(ScalarValueData::new(
                            data1.value | data2.value,
                            data1.unknown_mask & data2.unknown_mask,
                            0,
                            U64Range::new(umin, umax),
                        ));
                        (v.clone(), v)
                    } else {
                        (type1, type2)
                    }
                }
                (
                    JumpWidth::W32,
                    JumpType::Eq,
                    Type::ScalarValue(data1),
                    Type::ScalarValue(data2),
                ) => {
                    if data1.is_fully_initialized()
                        && data2.is_fully_initialized()
                        && U32ScalarValueData::from_u64_scalar(*data1)
                            .can_be_equal(U32ScalarValueData::from_u64_scalar(*data2))
                    {
                        // Refine ranges only when both operands fit in 32 bits; a range
                        // straddling 2^32 can have low bits below its min, so refining would
                        // drop reachable values.
                        let both_fit_u32 = data1.max() <= U32_MAX && data2.max() <= U32_MAX;
                        let (urange1, urange2) = if both_fit_u32 {
                            let umin = max_u64(data1.min(), data2.min());
                            let umax = min_u64(data1.max(), data2.max());
                            let urange = U64Range::new(umin, umax);
                            (urange, urange)
                        } else {
                            (data1.urange, data2.urange)
                        };
                        let value1 = data1.value | (data2.value & U32_MAX);
                        let unknown_mask1 =
                            data1.unknown_mask & (data2.unknown_mask | (U32_MAX << 32));
                        let value2 = data2.value | (data1.value & U32_MAX);
                        let unknown_mask2 =
                            data2.unknown_mask & (data1.unknown_mask | (U32_MAX << 32));
                        if let (Some(s1), Some(s2)) = (
                            ScalarValueData::try_new(value1, unknown_mask1, 0, urange1),
                            ScalarValueData::try_new(value2, unknown_mask2, 0, urange2),
                        ) {
                            (Type::ScalarValue(s1), Type::ScalarValue(s2))
                        } else {
                            (type1, type2)
                        }
                    } else {
                        (type1, type2)
                    }
                }
                (
                    JumpWidth::W64,
                    JumpType::Eq | JumpType::Ge,
                    Type::ScalarValue(data),
                    Type::NullOr { id, inner },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Eq | JumpType::Le,
                    Type::NullOr { id, inner },
                    Type::ScalarValue(data),
                ) if data.is_zero() => {
                    let id_cloned = id.clone();
                    (**inner).unregister_resource(context);
                    context.set_null(&id_cloned, true);
                    let zero = Type::ScalarValue(ScalarValueData::from_known(0));
                    (zero.clone(), zero)
                }
                (JumpWidth::W64, jt, Type::NullOr { id, inner }, Type::ScalarValue(data))
                    if jt.is_strict() && data.is_zero() =>
                {
                    let id_cloned = id.clone();
                    let inner_cloned = (**inner).clone();
                    context.set_null(&id_cloned, false);
                    (inner_cloned, type2)
                }
                (JumpWidth::W64, jt, Type::ScalarValue(data), Type::NullOr { id, inner })
                    if jt.is_strict() && data.is_zero() =>
                {
                    let id_cloned = id.clone();
                    let inner_cloned = (**inner).clone();
                    context.set_null(&id_cloned, false);
                    (type1, inner_cloned)
                }
                (JumpWidth::W64, JumpType::Lt, Type::ScalarValue(lhs), Type::ScalarValue(rhs)) => {
                    if lhs.is_fully_initialized()
                        && rhs.is_fully_initialized()
                        && lhs.min() < rhs.max()
                    {
                        let new_max_lhs = min_u64(lhs.max(), rhs.max() - 1);
                        let new_min_rhs = max_u64(rhs.min(), lhs.min() + 1);
                        let new_range_lhs = U64Range::new(lhs.min(), new_max_lhs);
                        let new_range_rhs = U64Range::new(new_min_rhs, rhs.max());
                        if let (Some(s1), Some(s2)) = (
                            ScalarValueData::try_new(lhs.value, lhs.unknown_mask, 0, new_range_lhs),
                            ScalarValueData::try_new(rhs.value, rhs.unknown_mask, 0, new_range_rhs),
                        ) {
                            (Type::ScalarValue(s1), Type::ScalarValue(s2))
                        } else {
                            (type1, type2)
                        }
                    } else {
                        (type1, type2)
                    }
                }
                (JumpWidth::W64, JumpType::Gt, Type::ScalarValue(lhs), Type::ScalarValue(rhs)) => {
                    if lhs.is_fully_initialized()
                        && rhs.is_fully_initialized()
                        && rhs.min() < lhs.max()
                    {
                        let new_min_lhs = max_u64(lhs.min(), rhs.min() + 1);
                        let new_max_rhs = min_u64(rhs.max(), lhs.max() - 1);
                        let new_range_lhs = U64Range::new(new_min_lhs, lhs.max());
                        let new_range_rhs = U64Range::new(rhs.min(), new_max_rhs);
                        if let (Some(s1), Some(s2)) = (
                            ScalarValueData::try_new(lhs.value, lhs.unknown_mask, 0, new_range_lhs),
                            ScalarValueData::try_new(rhs.value, rhs.unknown_mask, 0, new_range_rhs),
                        ) {
                            (Type::ScalarValue(s1), Type::ScalarValue(s2))
                        } else {
                            (type1, type2)
                        }
                    } else {
                        (type1, type2)
                    }
                }
                (JumpWidth::W64, JumpType::Le, Type::ScalarValue(lhs), Type::ScalarValue(rhs)) => {
                    if lhs.is_fully_initialized()
                        && rhs.is_fully_initialized()
                        && lhs.min() <= rhs.max()
                    {
                        let new_max_lhs = min_u64(lhs.max(), rhs.max());
                        let new_min_rhs = max_u64(rhs.min(), lhs.min());
                        let new_range_lhs = U64Range::new(lhs.min(), new_max_lhs);
                        let new_range_rhs = U64Range::new(new_min_rhs, rhs.max());
                        if let (Some(s1), Some(s2)) = (
                            ScalarValueData::try_new(lhs.value, lhs.unknown_mask, 0, new_range_lhs),
                            ScalarValueData::try_new(rhs.value, rhs.unknown_mask, 0, new_range_rhs),
                        ) {
                            (Type::ScalarValue(s1), Type::ScalarValue(s2))
                        } else {
                            (type1, type2)
                        }
                    } else {
                        (type1, type2)
                    }
                }
                (JumpWidth::W64, JumpType::Ge, Type::ScalarValue(lhs), Type::ScalarValue(rhs)) => {
                    if lhs.is_fully_initialized()
                        && rhs.is_fully_initialized()
                        && rhs.min() <= lhs.max()
                    {
                        let new_min_lhs = max_u64(lhs.min(), rhs.min());
                        let new_max_rhs = min_u64(rhs.max(), lhs.max());
                        let new_range_lhs = U64Range::new(new_min_lhs, lhs.max());
                        let new_range_rhs = U64Range::new(rhs.min(), new_max_rhs);
                        if let (Some(s1), Some(s2)) = (
                            ScalarValueData::try_new(lhs.value, lhs.unknown_mask, 0, new_range_lhs),
                            ScalarValueData::try_new(rhs.value, rhs.unknown_mask, 0, new_range_rhs),
                        ) {
                            (Type::ScalarValue(s1), Type::ScalarValue(s2))
                        } else {
                            (type1, type2)
                        }
                    } else {
                        (type1, type2)
                    }
                }
                (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToArray { id: id1, offset },
                    Type::PtrToEndArray { id: id2 },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToEndArray { id: id1 },
                    Type::PtrToArray { id: id2, offset },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Le,
                    Type::PtrToArray { id: id1, offset },
                    Type::PtrToEndArray { id: id2 },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Ge,
                    Type::PtrToEndArray { id: id1 },
                    Type::PtrToArray { id: id2, offset },
                ) if *id1 == *id2 => {
                    let id_cloned = id1.clone();
                    let off = *offset;
                    context.update_array_bounds(id_cloned, off);
                    (type1, type2)
                }
                (
                    JumpWidth::W64,
                    JumpType::Lt,
                    Type::PtrToArray { id: id1, offset },
                    Type::PtrToEndArray { id: id2 },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Gt,
                    Type::PtrToEndArray { id: id1 },
                    Type::PtrToArray { id: id2, offset },
                ) if *id1 == *id2 => {
                    let Some(bound) = offset.checked_add_u64(1) else {
                        return Err(VerifierErrorKind::PointerOffsetOverflow);
                    };
                    let id_cloned = id1.clone();
                    context.update_array_bounds(id_cloned, bound);
                    (type1, type2)
                }
                (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToStack { offset: StackOffset(offset1) },
                    Type::PtrToStack { offset: StackOffset(offset2) },
                ) => {
                    let o1 = *offset1;
                    let o2 = *offset2;
                    return Self::constrain_equal_offsets(context, &type1, &type2, o1, o2);
                }
                (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToMemory { id: id1, offset: offset1, .. },
                    Type::PtrToMemory { id: id2, offset: offset2, .. },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToStruct { id: id1, offset: offset1, .. },
                    Type::PtrToStruct { id: id2, offset: offset2, .. },
                )
                | (
                    JumpWidth::W64,
                    JumpType::Eq,
                    Type::PtrToArray { id: id1, offset: offset1 },
                    Type::PtrToArray { id: id2, offset: offset2 },
                ) if *id1 == *id2 => {
                    let o1 = *offset1;
                    let o2 = *offset2;
                    return Self::constrain_equal_offsets(context, &type1, &type2, o1, o2);
                }
                _ => (type1, type2),
            };
        Ok(result)
    }

    #[doc(hidden)]
    pub fn match_parameter_type(
        &self,
        verification_context: &VerificationContext,
        context: &ComputationContext,
        parameter_type: &Type,
        index: usize,
        next: &mut ComputationContext,
    ) -> Result<(), VerifierErrorKind> {
        match (parameter_type, self) {
            (Type::NullOrParameter(t), Type::ScalarValue(data))
                if data.is_known() && data.value == 0 =>
            {
                Ok(())
            }
            (Type::NullOrParameter(t), _) => {
                self.match_parameter_type(verification_context, context, &**t, index, next)
            }
            (Type::ScalarValueParameter, Type::ScalarValue(data))
                if data.is_fully_initialized() =>
            {
                Ok(())
            }
            (
                Type::ConstPtrToMapParameter { filter, must_be_writable },
                Type::ConstPtrToMap { schema, .. },
            ) => {
                if *must_be_writable && schema.flags.contains(MapFlags::ProgReadOnly) {
                    return Err(VerifierErrorKind::WriteToReadOnlyMap);
                }
                let map_type = schema.map_type;
                if !filter.is_allowed(map_type) {
                    return Err(VerifierErrorKind::InvalidMapTypeForHelper(map_type));
                }
                Ok(())
            }
            (
                Type::MapKeyParameter { map_ptr_index },
                Type::PtrToMemory { offset, buffer_size, .. },
            ) => {
                let schema = match context.get_map_schema(*map_ptr_index) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                context.check_memory_access(*offset, *buffer_size, 0, schema.key_size as usize)
            }
            (Type::MapKeyParameter { map_ptr_index }, Type::PtrToStack { offset }) => {
                let schema = match context.get_map_schema(*map_ptr_index) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                context.stack.read_data_ptr(*offset, schema.key_size as u64)
            }
            (
                Type::MapValueParameter { map_ptr_index },
                Type::PtrToMemory { offset, buffer_size, .. },
            ) => {
                let schema = match context.get_map_schema(*map_ptr_index) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                context.check_memory_access(*offset, *buffer_size, 0, schema.value_size as usize)
            }
            (Type::MapValueParameter { map_ptr_index }, Type::PtrToStack { offset }) => {
                let schema = match context.get_map_schema(*map_ptr_index) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                context.stack.read_data_ptr(*offset, schema.value_size as u64)
            }
            (
                Type::MemoryParameter { size, output, .. },
                Type::PtrToMemory { offset, buffer_size, origin, .. },
            ) => {
                if *output && !origin.is_writable() {
                    return Err(VerifierErrorKind::WriteToReadOnlyMemory);
                }
                let expected_size = match size.size(context) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                let offset_max = offset.max();
                if offset_max > *buffer_size || offset_max == u64::MAX {
                    return Err(VerifierErrorKind::OutOfBoundsMemoryAccess);
                }
                let size_left = *buffer_size - offset_max;
                if expected_size > size_left || expected_size == u64::MAX {
                    return Err(VerifierErrorKind::OutOfBoundsMemoryAccess);
                }
                Ok(())
            }
            (Type::MemoryParameter { size, input, output }, Type::PtrToStack { offset }) => {
                let expected_size = match size.size(context) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                if expected_size > (BPF_STACK_SIZE as u64) {
                    return Err(VerifierErrorKind::OutOfBoundsMemoryAccess);
                }
                match offset.check_valid_offset() {
                    Ok(()) => {}
                    Err(err) => return Err(err),
                }
                let buffer_end = offset.add_u64(expected_size);
                if !buffer_end.is_within_stack() {
                    Err(VerifierErrorKind::OutOfBoundsMemoryAccess)
                } else {
                    if *output {
                        match next.stack.write_data_ptr(*offset, expected_size) {
                            Ok(()) => {}
                            Err(err) => return Err(err),
                        }
                    }
                    if *input {
                        match context.stack.read_data_ptr(*offset, expected_size) {
                            Ok(()) => {}
                            Err(err) => return Err(err),
                        }
                    }
                    Ok(())
                }
            }
            (
                Type::StructParameter { id: id1 },
                Type::PtrToMemory { id: id2, offset, .. }
                | Type::PtrToStruct { id: id2, offset, .. },
            ) if offset.is_zero() && id1.matches(id2) => Ok(()),
            (
                Type::ReleasableParameter { id: id1, inner: inner1 },
                Type::Releasable { id: id2, inner: inner2 },
            ) if id2.has_parent(id1) => {
                if next.resources.contains(id2) {
                    (**inner2).match_parameter_type(
                        verification_context,
                        context,
                        &**inner1,
                        index,
                        next,
                    )
                } else {
                    Err(VerifierErrorKind::UseAfterRelease)
                }
            }
            (Type::ContextParameter { parameter_index }, arg_type) => {
                match arg_type.check_not_released(&next.resources) {
                    Ok(()) => {}
                    Err(err) => return Err(err),
                }
                let idx = *parameter_index as usize;
                if idx < verification_context.calling_context.args.len()
                    && verification_context.calling_context.args[idx] == *arg_type
                {
                    Ok(())
                } else {
                    Err(VerifierErrorKind::ExpectedProgramArgument(*parameter_index))
                }
            }
            (Type::ReleaseParameter { id: id1 }, Type::Releasable { id: id2, inner })
                if id2.has_parent(id1) =>
            {
                if !next.resources.contains(id2) {
                    return Err(VerifierErrorKind::DoubleRelease);
                }
                // Only the unmodified base pointer of a releasable resource may be released.
                let base_offset = match &**inner {
                    Type::PtrToMemory { offset, .. } | Type::PtrToStruct { offset, .. } => offset,
                    _ => return Err(VerifierErrorKind::HelperArgumentTypeMismatch(index)),
                };
                if !base_offset.is_zero() {
                    return Err(VerifierErrorKind::HelperArgumentTypeMismatch(index));
                }
                next.resources.remove(id2);
                Ok(())
            }
            (_, Type::Releasable { id, inner }) => {
                if !next.resources.contains(id) {
                    return Err(VerifierErrorKind::UseAfterRelease);
                }
                (**inner).match_parameter_type(
                    verification_context,
                    context,
                    parameter_type,
                    index,
                    next,
                )
            }
            (Type::AnyParameter, _) => Ok(()),
            _ => Err(VerifierErrorKind::HelperArgumentTypeMismatch(index)),
        }
    }

    /// If this `Type` is an instance of NullOr with the given `null_id`, replace it wither either
    /// 0 or the subtype depending on `is_null`
    fn set_null(&mut self, null_id: &MemoryId, is_null: bool) {
        let replacement = match self {
            Type::NullOr { id, inner } if *id == *null_id => Some(if is_null {
                Type::ScalarValue(ScalarValueData::from_known(0))
            } else {
                (**inner).clone()
            }),
            _ => None,
        };
        if let Some(new_val) = replacement {
            *self = new_val;
        }
    }

    fn unregister_resource(&self, context: &mut ComputationContext) {
        if let Type::Releasable { id, .. } = self {
            context.resources.remove(id);
        }
    }

    fn stack_input_size(
        &self,
        comp: &ComputationContext,
    ) -> Result<Option<u64>, VerifierErrorKind> {
        match self {
            Type::MemoryParameter { size, input: true, .. } => match size.size(comp) {
                Ok(sz) => Ok(Some(sz)),
                Err(err) => Err(err),
            },
            Type::MapKeyParameter { map_ptr_index } => match comp.get_map_schema(*map_ptr_index) {
                Ok(schema) => Ok(Some(schema.key_size as u64)),
                Err(err) => Err(err),
            },
            Type::MapValueParameter { map_ptr_index } => {
                match comp.get_map_schema(*map_ptr_index) {
                    Ok(schema) => Ok(Some(schema.value_size as u64)),
                    Err(err) => Err(err),
                }
            }
            Type::NullOrParameter(inner) | Type::ReleasableParameter { inner, .. } => {
                (**inner).stack_input_size(comp)
            }
            _ => Ok(None),
        }
    }

    fn assert_valid_signature_type(&self, args: &[Type]) {
        match self {
            Type::MapKeyParameter { map_ptr_index } | Type::MapValueParameter { map_ptr_index } => {
                let idx = *map_ptr_index as usize;
                assert!(idx < args.len());
                assert!(matches!(args[idx], Type::ConstPtrToMapParameter { .. }));
            }
            Type::MemoryParameter { size: MemoryParameterSize::Reference { index }, .. } => {
                let idx = *index as usize;
                assert!(idx < args.len());
                assert!(matches!(args[idx], Type::ScalarValueParameter));
            }
            Type::ContextParameter { parameter_index } => {
                assert!(*parameter_index < ARGUMENT_COUNT);
            }
            Type::NullOrParameter(inner) => {
                assert!(matches!(
                    &**inner,
                    Type::PtrToMemory { .. }
                        | Type::PtrToStruct { .. }
                        | Type::ConstPtrToMapParameter { .. }
                        | Type::MapKeyParameter { .. }
                        | Type::MapValueParameter { .. }
                        | Type::MemoryParameter { .. }
                        | Type::StructParameter { .. }
                        | Type::ContextParameter { .. }
                        | Type::ReleasableParameter { .. }
                ));
                (**inner).assert_valid_signature_type(args);
            }
            Type::ReleasableParameter { inner, .. } => {
                assert!(matches!(
                    &**inner,
                    Type::PtrToMemory { .. }
                        | Type::PtrToStruct { .. }
                        | Type::MemoryParameter { .. }
                        | Type::MapKeyParameter { .. }
                        | Type::MapValueParameter { .. }
                        | Type::StructParameter { .. }
                ));
                (**inner).assert_valid_signature_type(args);
            }
            _ => {}
        }
    }

    /// Partially compares two register arrays element-by-element.
    fn compare_registers(
        r1: &[Self; GENERAL_REGISTER_COUNT as usize],
        r2: &[Self; GENERAL_REGISTER_COUNT as usize],
    ) -> Option<Ordering> {
        let mut result = Ordering::Equal;
        let mut i = 0usize;
        while i < (GENERAL_REGISTER_COUNT as usize) {
            let Some(ord) = r1[i].partial_cmp(&r2[i]) else {
                return None;
            };
            let Some(next) = associate_orderings(result, ord) else {
                return None;
            };
            result = next;
            i += 1;
        }
        Some(result)
    }
}

fn clone_types(types: &[Type]) -> Vec<Type> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < types.len() {
        out.push(types[i].clone());
        i += 1;
    }
    out
}

#[derive(Debug)]
pub struct FunctionSignature {
    args: Vec<Type>,
    return_value: Type,
    invalidate_array_bounds: bool,
}

impl Clone for FunctionSignature {
    fn clone(&self) -> Self {
        Self {
            args: clone_types(&self.args),
            return_value: self.return_value.clone(),
            invalidate_array_bounds: self.invalidate_array_bounds,
        }
    }
}

#[derive(Debug)]
pub struct FunctionSignatureBuilder {
    pub args: Vec<Type>,
    pub return_value: Type,
    pub invalidate_array_bounds: bool,
}

impl Clone for FunctionSignatureBuilder {
    fn clone(&self) -> Self {
        Self {
            args: clone_types(&self.args),
            return_value: self.return_value.clone(),
            invalidate_array_bounds: self.invalidate_array_bounds,
        }
    }
}

impl FunctionSignatureBuilder {
    pub fn build(self) -> FunctionSignature {
        assert!(self.args.len() <= (ARGUMENT_COUNT as usize));
        let mut i = 0;
        while i < self.args.len() {
            self.args[i].assert_valid_signature_type(&self.args);
            i += 1;
        }
        self.return_value.assert_valid_signature_type(&self.args);
        FunctionSignature {
            args: self.args,
            return_value: self.return_value,
            invalidate_array_bounds: self.invalidate_array_bounds,
        }
    }
}

#[derive(Debug)]
pub struct CoreCallingContext {
    /// List of map schemas of the associated map. The maps can be accessed using LDDW instruction
    /// with `src_reg=BPF_PSEUDO_MAP_IDX`.
    pub maps: Vec<MapSchema>,
    /// Registered `(helper_id, signature)` pairs for external helpers.
    pub helpers: Vec<(u32, FunctionSignature)>,
    /// The args of the program.
    pub args: Vec<Type>,
    /// Packet type. Normally it should be either `None` or `args[0]`.
    pub packet_type: Option<Type>,
    /// Whether pointer values may be coerced to scalars (`CAP_PERFMON` or `CAP_SYS_ADMIN`).
    pub allow_ptr_leaks: bool,
}

impl Default for CoreCallingContext {
    fn default() -> Self {
        Self {
            maps: Vec::new(),
            helpers: Vec::new(),
            args: Vec::new(),
            packet_type: None,
            allow_ptr_leaks: false,
        }
    }
}

enum ResolvedLddw {
    Imm(u64),
    MapPtr { map_index: u32, schema: MapSchema },
    MapValuePtr { map_index: u32, schema: MapSchema, offset: u32 },
}

impl CoreCallingContext {
    pub fn register_map(&mut self, schema: MapSchema) -> usize {
        let index = self.maps.len();
        self.maps.push(schema);
        index
    }

    pub fn find_helper(&self, index: u32) -> Option<&FunctionSignature> {
        let mut i = 0;
        while i < self.helpers.len() {
            if self.helpers[i].0 == index {
                return Some(&self.helpers[i].1);
            }
            i += 1;
        }
        None
    }

    fn resolve_lddw(
        &self,
        src: u8,
        lower: i32,
        next_imm: i32,
    ) -> Result<ResolvedLddw, VerifierErrorKind> {
        match src {
            0 => {
                let value = ((lower as u32) as u64) | (((next_imm as u32) as u64) << 32);
                Ok(ResolvedLddw::Imm(value))
            }
            BPF_PSEUDO_MAP_IDX => {
                let map_index = lower as u32;
                if lower < 0 || (lower as usize) >= self.maps.len() {
                    return Err(VerifierErrorKind::InvalidMapIndex(map_index));
                }
                let schema = self.maps[lower as usize];
                Ok(ResolvedLddw::MapPtr { map_index, schema })
            }
            BPF_PSEUDO_MAP_IDX_VALUE => {
                let map_index = lower as u32;
                if lower < 0 || (lower as usize) >= self.maps.len() {
                    return Err(VerifierErrorKind::InvalidMapIndex(map_index));
                }
                let schema = self.maps[lower as usize];
                if schema.map_type != BPF_MAP_TYPE_ARRAY {
                    return Err(VerifierErrorKind::LddwMapValueNotArray(
                        map_index,
                        schema.map_type,
                    ));
                }
                if schema.max_entries == 0 {
                    return Err(VerifierErrorKind::EmptyArrayMap);
                }
                if next_imm < 0 || (next_imm as u32) >= schema.value_size {
                    return Err(VerifierErrorKind::InvalidMapValueOffset(
                        next_imm,
                        schema.value_size,
                    ));
                }
                let offset = next_imm as u32;
                Ok(ResolvedLddw::MapValuePtr { map_index, schema, offset })
            }
            _ => Err(VerifierErrorKind::UnsupportedLddwSource(src)),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct StructAccess {
    pub pc: ProgramCounter,

    // Memory Id of the struct being accessed.
    pub memory_id: MemoryId,

    // Offset of the field being loaded.
    pub field_offset: usize,

    // Indicates that this is a 32-bit pointer load. These instructions must be remapped.
    pub is_32_bit_ptr_load: bool,
}

#[derive(Debug)]
pub struct VerifiedEbpfProgram {
    pub(crate) code: Vec<EbpfInstruction>,
    pub(crate) args: Vec<Type>,
    pub(crate) struct_access_instructions: Vec<StructAccess>,
    pub(crate) maps: Vec<MapSchema>,
}

impl Clone for VerifiedEbpfProgram {
    fn clone(&self) -> Self {
        let mut code = Vec::new();
        let mut i = 0;
        while i < self.code.len() {
            code.push(self.code[i]);
            i += 1;
        }
        let args = clone_types(&self.args);
        let mut struct_access_instructions = Vec::new();
        let mut k = 0;
        while k < self.struct_access_instructions.len() {
            struct_access_instructions.push(self.struct_access_instructions[k].clone());
            k += 1;
        }
        let mut cloned_maps = Vec::new();
        let mut m = 0;
        while m < self.maps.len() {
            cloned_maps.push(self.maps[m]);
            m += 1;
        }
        Self { code, args, struct_access_instructions, maps: cloned_maps }
    }
}

impl VerifiedEbpfProgram {
    pub fn code(&self) -> &[EbpfInstruction] {
        &self.code
    }

    pub fn args(&self) -> &[Type] {
        &self.args
    }

    pub fn struct_access_instructions(&self) -> &[StructAccess] {
        &self.struct_access_instructions
    }

    pub fn from_verified_code(
        code: Vec<EbpfInstruction>,
        args: Vec<Type>,
        struct_access_instructions: Vec<StructAccess>,
        maps: Vec<MapSchema>,
    ) -> Self {
        Self { code, args, struct_access_instructions, maps }
    }

    pub fn maps(&self) -> &[MapSchema] {
        &self.maps
    }
}

fn error_and_log<T, L: CoreVerifierLogger>(
    logger: &mut L,
    err: VerifierError,
) -> Result<T, VerifierError> {
    logger.log_error(&err);
    Err(err)
}

fn prescan_step(
    code: &[EbpfInstruction],
    scan_pc: usize,
    calling_context: &CoreCallingContext,
    is_prune_point: &mut Vec<bool>,
) -> Result<usize, VerifierErrorKind> {
    let code_len = code.len();
    let inst = code[scan_pc];
    if inst.code() == BPF_LDDW {
        if scan_pc + 1 >= code_len {
            return Err(VerifierErrorKind::IncompleteLddw);
        }
        // Enforcing `code == 0` on the second slot of every `BPF_LDDW` guarantees that any jump
        // into the middle of an `LDDW` is rejected during verification as an invalid opcode.
        let next_instruction = code[scan_pc + 1];
        if next_instruction.code() != 0
            || next_instruction.offset() != 0
            || next_instruction.src_reg() != 0
            || next_instruction.dst_reg() != 0
        {
            return Err(VerifierErrorKind::MalformedLddw);
        }
        match calling_context.resolve_lddw(inst.src_reg(), inst.imm(), next_instruction.imm()) {
            Ok(_) => Ok(2),
            Err(err) => Err(err),
        }
    } else {
        if inst.code() == (BPF_JMP | BPF_CALL) && inst.src_reg() != 0 {
            return Err(VerifierErrorKind::UnsupportedCallType(inst.src_reg()));
        }
        let class = inst.code() & BPF_CLS_MASK;
        let op = inst.code() & BPF_SUB_OP_MASK;
        // `cmpxchg` branches into two abstract states (equal vs non-equal), so its fallthrough
        // instruction is also recorded as a prune point alongside jump targets and conditional
        // branch fallthroughs.
        let is_cmpxchg = (inst.code() == (BPF_STX | BPF_ATOMIC | BPF_W)
            || inst.code() == (BPF_STX | BPF_ATOMIC | BPF_DW))
            && (inst.imm() as u8) == BPF_CMPXCHG;
        if (class == BPF_JMP || class == BPF_JMP32) && op != BPF_CALL && op != BPF_EXIT {
            if let Some(target_pc) = checked_add_signed_usize(scan_pc, (inst.offset() as isize) + 1)
            {
                if target_pc < code_len {
                    is_prune_point[target_pc] = true;
                }
            }
            if op != BPF_JA && scan_pc + 1 < code_len {
                is_prune_point[scan_pc + 1] = true;
            }
        } else if is_cmpxchg && scan_pc + 1 < code_len {
            is_prune_point[scan_pc + 1] = true;
        }
        Ok(1)
    }
}

/// Pure verification entry point independent of `std` collections and dynamic trait objects.
///
/// Returned `VerifiedEbpfProgram` should be linked before execution. Note that `code` may be
/// modified during verification by replacing unreachable instructions with `ja -1` so dead
/// branches are ignored during linking.
pub fn verify_program_core<L: CoreVerifierLogger>(
    code: Vec<EbpfInstruction>,
    calling_context: CoreCallingContext,
    logger: &mut L,
) -> Result<VerifiedEbpfProgram, VerifierError> {
    if code.len() > BPF_MAX_INSTS {
        return error_and_log(
            logger,
            VerifierError { kind: VerifierErrorKind::ProgramTooLarge, pc: None },
        );
    }

    // Pre-scan the program to ensure all `BPF_LDDW` and `BPF_JMP | BPF_CALL` instructions are
    // structurally well-formed and have valid map/call operands, even in unreachable code:
    // 1. Linking linearly scans and rewrites `BPF_LDDW` map loads and `BPF_JMP | BPF_CALL`
    //    helper calls across the instruction array.
    // 2. Enforcing `code == 0` on the second slot of every `BPF_LDDW` guarantees that any jump
    //    into the middle of an `LDDW` is rejected during verification as an invalid opcode.
    //
    // This pass also computes the prune points: the targets of jumps and the instructions
    // following a conditional jump or a `cmpxchg` (which also branches into two states). Every
    // loop and every join of two branches goes through one of them, and states are only compared
    // with and recorded as terminating contexts at these points.
    let code_len = code.len();
    let mut is_prune_point = Vec::new();
    let mut init_i = 0;
    while init_i < code_len {
        is_prune_point.push(false);
        init_i += 1;
    }

    let mut scan_err: Option<VerifierError> = None;
    let mut scan_pc = 0usize;
    while scan_pc < code_len && scan_err.is_none() {
        match prescan_step(&code, scan_pc, &calling_context, &mut is_prune_point) {
            Ok(advance) => {
                scan_pc += advance;
            }
            Err(kind) => {
                scan_err = Some(VerifierError { kind, pc: Some(scan_pc) });
            }
        }
    }
    if let Some(err) = scan_err {
        return error_and_log(logger, err);
    }

    let mut terminating_contexts = Vec::new();
    let mut tc_i = 0;
    while tc_i < code_len {
        if is_prune_point[tc_i] {
            terminating_contexts.push(Some(TerminatingContexts::default()));
        } else {
            terminating_contexts.push(None);
        }
        tc_i += 1;
    }

    let mut initial_context = ComputationContext::default();
    if calling_context.args.len() > (ARGUMENT_COUNT as usize) {
        return error_and_log(
            logger,
            VerifierError {
                kind: VerifierErrorKind::InvalidRegister(ARGUMENT_COUNT + 1),
                pc: None,
            },
        );
    }
    let mut arg_i = 0usize;
    while arg_i < calling_context.args.len() {
        initial_context.registers[arg_i + 1] = calling_context.args[arg_i].clone();
        arg_i += 1;
    }

    let mut visited = Vec::new();
    let mut vis_i = 0;
    while vis_i < code_len {
        visited.push(false);
        vis_i += 1;
    }

    let mut worklist = Worklist::new();
    worklist.push(WorklistEntry { context: initial_context, parent: None });
    let mut arena = ExplorerArena::new();
    let mut verification_context =
        VerificationContext::new(calling_context, logger.is_enabled(), code, terminating_contexts);

    let mut verify_err: Option<VerifierError> = None;
    let mut worklist_done = false;
    while !worklist_done && verify_err.is_none() {
        match worklist.pop() {
            Some(entry) => {
                match verify_worklist_entry(
                    entry,
                    &mut worklist,
                    &mut arena,
                    &mut visited,
                    &mut verification_context,
                    logger,
                ) {
                    Ok(()) => {}
                    Err(err) => {
                        verify_err = Some(err);
                    }
                }
            }
            None => {
                worklist_done = true;
            }
        }
    }
    if let Some(err) = verify_err {
        return Err(err);
    }

    let VerificationContext {
        calling_context: CoreCallingContext { maps, args, .. },
        mut code,
        memory_access_instructions,
        ..
    } = verification_context;

    // Replace unreachable instructions with `ja -1` (an unconditional jump to self, which is a
    // valid no-op from the linker's perspective) so that `link_program` does not trip on unverified
    // pseudo-instructions in dead branches.
    let dead_insn = EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, -1, 0);
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc].code() == BPF_LDDW {
            if !visited[pc] {
                code[pc] = dead_insn;
                code[pc + 1] = dead_insn;
            }
            pc += 2;
        } else {
            if !visited[pc] {
                code[pc] = dead_insn;
            }
            pc += 1;
        }
    }

    let mut struct_access_instructions = Vec::new();
    let mut ma_i = 0usize;
    while ma_i < memory_access_instructions.len() {
        if let MemoryAccess::Struct(ref s) = memory_access_instructions[ma_i] {
            struct_access_instructions.push(s.clone());
        }
        ma_i += 1;
    }

    Ok(VerifiedEbpfProgram { code, args, struct_access_instructions, maps })
}

/// An in-flight ancestor state in `ExplorerArena` waiting for all of its child
/// control-flow branches to terminate or prune before backward data-dependency
/// propagation can proceed through `context`.
#[doc(hidden)]
pub struct ExplorerNode {
    pub context: ComputationContext,
    pub parent: Option<usize>,
    pub pending_children: usize,
    pub dependencies: DataDependencies,
}

/// Index-addressed arena of live `ExplorerNode`s with LIFO slot recycling.
///
/// Reusing vacated slot indices prevents unbounded `Vec` growth on diamond
/// control-flow graphs without requiring `Rc`/`Cell` interior mutability. Both
/// `nodes` and `free_indices` use cursor-indexed operations rather than
/// `Vec::pop` to keep Charon/Aeneas LLBC translation simple.
#[doc(hidden)]
pub struct ExplorerArena {
    pub nodes: Vec<Option<ExplorerNode>>,
    free_indices: Vec<usize>,
    free_len: usize,
}

impl ExplorerArena {
    pub fn new() -> Self {
        Self { nodes: Vec::new(), free_indices: Vec::new(), free_len: 0 }
    }

    pub fn push(&mut self, node: ExplorerNode) -> usize {
        if self.free_len > 0 {
            self.free_len -= 1;
            let idx = self.free_indices[self.free_len];
            self.nodes[idx] = Some(node);
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Some(node));
            idx
        }
    }

    /// Merges `child_deps` into `parent_idx`, decrements its `pending_children`
    /// counter, and extracts + recycles the node once all child branches have
    /// completed.
    pub fn complete_child(
        &mut self,
        parent_idx: usize,
        child_deps: DataDependencies,
    ) -> Option<ExplorerNode> {
        let ready = match &mut self.nodes[parent_idx] {
            Some(parent_node) => {
                parent_node.dependencies = parent_node.dependencies.merge(child_deps);
                parent_node.pending_children -= 1;
                parent_node.pending_children == 0
            }
            None => false,
        };
        if !ready {
            return None;
        }
        let node = core::mem::replace(&mut self.nodes[parent_idx], None);
        if node.is_some() {
            if self.free_len < self.free_indices.len() {
                self.free_indices[self.free_len] = parent_idx;
            } else {
                self.free_indices.push(parent_idx);
            }
            self.free_len += 1;
        }
        node
    }
}

#[doc(hidden)]
pub struct WorklistEntry {
    pub context: ComputationContext,
    pub parent: Option<usize>,
}

/// Explicit LIFO DFS stack of states awaiting symbolic execution.
///
/// Uses cursor-indexed `Option` slot replacement rather than `Vec::pop` so
/// Charon/Aeneas LLBC translation avoids `Vec::pop` borrowing patterns while
/// reusing allocated capacity across pushes and pops.
#[doc(hidden)]
pub struct Worklist {
    pub entries: Vec<Option<WorklistEntry>>,
    len: usize,
}

impl Worklist {
    pub fn new() -> Self {
        Self { entries: Vec::new(), len: 0 }
    }

    pub fn push(&mut self, entry: WorklistEntry) {
        if self.len < self.entries.len() {
            self.entries[self.len] = Some(entry);
            self.len += 1;
        } else {
            self.entries.push(Some(entry));
            self.len += 1;
        }
    }

    pub fn pop(&mut self) -> Option<WorklistEntry> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            core::mem::replace(&mut self.entries[self.len], None)
        }
    }
}

fn clear_dead_registers(current: &mut ComputationContext, deps: DataDependencies) {
    let mut reg = 0usize;
    while reg < (GENERAL_REGISTER_COUNT as usize) {
        if !deps.has_reg(reg as u8) {
            current.registers[reg] = Type::UNINITIALIZED;
        }
        reg += 1;
    }
}

/// Propagates backward data dependencies and records broadened states at prune
/// points up the completed ancestor chain:
///
/// 1. Given `current_in` at `pc` and the union `current_deps` of registers and
///    stack slots read by its successors, `DependencyTracker` visits `inst` to
///    compute `next_deps` (removing overwritten locations and adding read ones).
/// 2. If `pc` is a prune point (`terminating_contexts[pc]` is `Some`), any
///    register or stack slot not in `next_deps` is broadened to `UNINITIALIZED`
///    (the top element of the abstract domain) and inserted into
///    `terminating_contexts[pc]` so later states with narrower/equal live types
///    can be pruned without re-exploring the subgraph.
/// 3. `next_deps` is merged into `parent_idx` in `arena`.
/// 4. If all child branches of `parent_idx` have now terminated or pruned
///    (`pending_children == 0`), the parent `ExplorerNode` is recycled from
///    `arena` and the loop continues up the ancestor chain iteratively without
///    recursion, `Rc`/`Cell` interior mutability, or deep state clones.
fn terminate_context_chain(
    initial_current: ComputationContext,
    mut current_deps: DataDependencies,
    mut parent_idx: Option<usize>,
    arena: &mut ExplorerArena,
    verification_context: &mut VerificationContext,
) -> Result<(), VerifierError> {
    let mut current_opt: Option<ComputationContext> = Some(initial_current);
    let mut loop_err: Option<VerifierError> = None;
    while current_opt.is_some() && loop_err.is_none() {
        if let Some(current_in) = core::mem::replace(&mut current_opt, None) {
            let pc = current_in.pc;
            let inst = verification_context.code[pc];
            let mut tracker = DependencyTracker { context: current_in, deps: current_deps };
            match tracker.visit(verification_context, inst) {
                Ok(()) => {
                    let DependencyTracker { context: mut current, deps: next_deps } = tracker;
                    if let Some(ref mut tc) = verification_context.terminating_contexts[pc] {
                        clear_dead_registers(&mut current, next_deps);
                        current.stack.retain(next_deps.stack);
                        tc.insert(current, next_deps);
                    }

                    if let Some(p_idx) = parent_idx {
                        if let Some(parent_node) = arena.complete_child(p_idx, next_deps) {
                            current_opt = Some(parent_node.context);
                            current_deps = parent_node.dependencies;
                            parent_idx = parent_node.parent;
                        }
                    }
                }
                Err(kind) => {
                    loop_err = Some(VerifierError { kind, pc: Some(pc) });
                }
            }
        }
    }
    match loop_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn verify_worklist_entry<L: CoreVerifierLogger>(
    entry: WorklistEntry,
    worklist: &mut Worklist,
    arena: &mut ExplorerArena,
    visited: &mut Vec<bool>,
    verification_context: &mut VerificationContext,
    logger: &mut L,
) -> Result<(), VerifierError> {
    let mut context = entry.context;
    let parent_idx = entry.parent;
    let code_len = verification_context.code.len();

    let pruned_deps = if context.pc < code_len {
        match &verification_context.terminating_contexts[context.pc] {
            Some(tc) => tc.find_greater_or_equal_deps(&context),
            None => None,
        }
    } else {
        None
    };

    if let Some(ending_deps) = pruned_deps {
        if let Some(p_idx) = parent_idx {
            if let Some(parent_node) = arena.complete_child(p_idx, ending_deps) {
                if let Err(err) = terminate_context_chain(
                    parent_node.context,
                    parent_node.dependencies,
                    parent_node.parent,
                    arena,
                    verification_context,
                ) {
                    return error_and_log(logger, err);
                }
            }
        }
        return Ok(());
    }

    if verification_context.iteration > 10 * BPF_MAX_INSTS {
        return error_and_log(
            logger,
            VerifierError { kind: VerifierErrorKind::ComplexityLimitExceeded, pc: None },
        );
    }
    if context.pc >= code_len {
        return error_and_log(
            logger,
            VerifierError { kind: VerifierErrorKind::FellOffProgramEnd, pc: Some(context.pc) },
        );
    }

    visited[context.pc] = true;
    verification_context.log_enabled = logger.is_enabled();
    verification_context.pending_log = None;
    verification_context.pending_successors.clear();
    let inst = verification_context.code[context.pc];
    let visit_res = context.visit(verification_context, inst);
    if let Some((log_pc, opcode, log_ev)) =
        core::mem::replace(&mut verification_context.pending_log, None)
    {
        logger.log_instruction(log_pc, opcode, log_ev);
    }
    if let Err(kind) = visit_res {
        verification_context.pending_successors.clear();
        return error_and_log(logger, VerifierError { kind, pc: Some(context.pc) });
    }

    let num_succ = verification_context.pending_successors.len();
    if num_succ > 0 {
        let node_idx = arena.push(ExplorerNode {
            context,
            parent: parent_idx,
            pending_children: num_succ,
            dependencies: DataDependencies::default(),
        });
        let mut s_i = 0;
        while s_i < num_succ {
            if let Some(succ) =
                core::mem::replace(&mut verification_context.pending_successors[s_i], None)
            {
                worklist.push(WorklistEntry { context: succ, parent: Some(node_idx) });
            }
            s_i += 1;
        }
        verification_context.pending_successors.clear();
    } else if let Err(err) = terminate_context_chain(
        context,
        DataDependencies::default(),
        parent_idx,
        arena,
        verification_context,
    ) {
        return error_and_log(logger, err);
    }
    verification_context.iteration += 1;
    Ok(())
}

#[derive(Debug)]
enum MemoryAccess {
    Unaccessed,
    NonStruct,
    Struct(StructAccess),
}

#[doc(hidden)]
pub struct VerificationContext {
    /// The type information for the program arguments and the registered functions.
    pub calling_context: CoreCallingContext,
    /// Whether structured instruction logging is currently enabled.
    pub log_enabled: bool,
    /// Pending structured log event emitted during the most recent instruction visit.
    pub pending_log: Option<(ProgramCounter, u8, VerifierLogEvent)>,
    /// Successor `ComputationContext`s produced while visiting the current instruction.
    pub pending_successors: Vec<Option<ComputationContext>>,
    /// The program being analyzed.
    pub code: Vec<EbpfInstruction>,
    /// A counter used to generated unique ids for memory buffers and maps.
    pub counter: u64,
    /// The current iteration of the verifier. Used to ensure termination by limiting the number of
    /// iteration before bailing out.
    pub iteration: usize,
    /// Keep track of the context that terminates at a given pc. The list of context will all be
    /// incomparables as each time a bigger context is computed, the smaller ones are removed from
    /// the list. Indexed directly by `ProgramCounter` (`0..code.len()`) for O(1) lookup, and
    /// `None` if the pc is not a prune point.
    pub terminating_contexts: Vec<Option<TerminatingContexts>>,
    /// Per-PC record of whether a memory instruction accesses a `PtrToStruct` (`Struct`) or a
    /// non-struct pointer (`NonStruct`). Indexed directly by `ProgramCounter` (`0..code.len()`) to
    /// ensure a single instruction PC never mixes `PtrToStruct` rewriting with raw stack, buffer,
    /// or packet accesses, or accesses inconsistent struct fields.
    memory_access_instructions: Vec<MemoryAccess>,
}

impl VerificationContext {
    pub fn new(
        calling_context: CoreCallingContext,
        log_enabled: bool,
        code: Vec<EbpfInstruction>,
        terminating_contexts: Vec<Option<TerminatingContexts>>,
    ) -> Self {
        let mut memory_access_instructions = Vec::new();
        let mut i = 0;
        while i < code.len() {
            memory_access_instructions.push(MemoryAccess::Unaccessed);
            i += 1;
        }
        Self {
            calling_context,
            log_enabled,
            pending_log: None,
            pending_successors: Vec::new(),
            code,
            counter: 0,
            iteration: 0,
            terminating_contexts,
            memory_access_instructions,
        }
    }

    fn push_successor(&mut self, context: ComputationContext) {
        self.pending_successors.push(Some(context));
    }

    pub fn next_id(&mut self) -> MemoryId {
        let id = self.counter;
        self.counter += 1;
        MemoryId { namespace: Namespace::Verification, id, parent: None }
    }

    fn emit_log(&mut self, pc: ProgramCounter, event: VerifierLogEvent) {
        if self.log_enabled {
            debug_assert!(self.pending_log.is_none());
            let opcode = self.code[pc].code();
            self.pending_log = Some((pc, opcode, event));
        }
    }

    /// Register an instruction that loads or stores memory, recording `Some(struct_access)` for
    /// struct field accesses and `None` for non-struct accesses.
    fn register_memory_access(
        &mut self,
        pc: ProgramCounter,
        struct_access: Option<StructAccess>,
    ) -> Result<(), VerifierErrorKind> {
        match (&self.memory_access_instructions[pc], struct_access) {
            (MemoryAccess::Unaccessed, Some(access)) => {
                self.memory_access_instructions[pc] = MemoryAccess::Struct(access);
            }
            (MemoryAccess::Unaccessed, None) => {
                self.memory_access_instructions[pc] = MemoryAccess::NonStruct;
            }
            (MemoryAccess::NonStruct, None) => {}
            (MemoryAccess::Struct(existing), Some(access)) => {
                if *existing != access {
                    return Err(VerifierErrorKind::InconsistentStructFieldAccess);
                }
            }
            (MemoryAccess::Struct(_), None) | (MemoryAccess::NonStruct, Some(_)) => {
                return Err(VerifierErrorKind::InconsistentMemoryAccess);
            }
        }
        Ok(())
    }
}

/// An offset inside the stack. The offset is from the end of the stack.
/// downward.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackOffset(pub ScalarValueData);

impl Default for StackOffset {
    fn default() -> Self {
        Self(ScalarValueData::from_known(BPF_STACK_SIZE as u64))
    }
}

impl StackOffset {
    pub const fn new(scalar: ScalarValueData) -> Self {
        Self(scalar)
    }

    pub const fn from_u64(value: u64) -> Self {
        Self(ScalarValueData::from_known(value))
    }

    /// Returns an error if the current offset is not valid for element access, distinguishing
    /// variable offsets from out of bounds ones.
    pub fn check_valid_offset(&self) -> Result<(), VerifierErrorKind> {
        if !self.0.is_known() {
            return Err(VerifierErrorKind::VariableStackOffset);
        }
        if self.0.value >= (BPF_STACK_SIZE as u64) {
            return Err(VerifierErrorKind::StackAccessOutOfBounds);
        }
        Ok(())
    }

    /// Whether the current offset is within the stack bounds, inclusive of `BPF_STACK_SIZE`.
    pub fn is_within_stack(&self) -> bool {
        self.0.is_known() && self.0.value <= (BPF_STACK_SIZE as u64)
    }

    /// The value of the register.
    pub fn reg(&self) -> ScalarValueData {
        self.0
    }

    /// The index into the stack array this offset points to. Can be called only if
    /// `is_within_stack()` is true.
    pub fn array_index(&self) -> usize {
        (self.0.value as usize) / STACK_ELEMENT_SIZE
    }

    /// The offset inside the aligned u64 in the stack.
    pub fn sub_index(&self) -> usize {
        (self.0.value as usize) % STACK_ELEMENT_SIZE
    }

    pub fn add_u64(self, rhs: u64) -> Self {
        Self(self.0.add_u64(rhs))
    }

    pub fn add_i16(self, rhs: i16) -> Self {
        Self(self.0.add_i16(rhs))
    }
}

// The initialized elements of the stack are tracked in a `u64` bitmask.
const _: () = assert!(BPF_STACK_SIZE / STACK_ELEMENT_SIZE <= u64::BITS as usize);

/// The state of the stack
///
/// Only the initialized elements are stored, densely and in increasing index order, making
/// stack comparison on the hot path of state pruning a compact linear scan.
#[doc(hidden)]
#[derive(Debug)]
pub struct Stack {
    /// Bit `i` is set iff element `i` is initialized.
    pub mask: u64,
    /// The initialized elements, in increasing index order.
    pub data: Vec<Type>,
}

impl Default for Stack {
    fn default() -> Self {
        Self { mask: 0, data: Vec::new() }
    }
}

impl Clone for Stack {
    fn clone(&self) -> Self {
        Self { mask: self.mask, data: clone_types(&self.data) }
    }
}

impl PartialEq for Stack {
    fn eq(&self, other: &Self) -> bool {
        if self.mask != other.mask || self.data.len() != other.data.len() {
            return false;
        }
        let mut i = 0;
        while i < self.data.len() {
            if self.data[i] != other.data[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Eq for Stack {}

impl Stack {
    fn bit(index: usize) -> u64 {
        assert!(index < 64);
        1u64.wrapping_shl(index as u32)
    }

    /// Returns the position of the element at `index` in `data`, or the position where it must be
    /// inserted if it is not initialized.
    fn position(&self, index: usize) -> usize {
        popcount_u64(self.mask & (Self::bit(index) - 1))
    }

    /// Replace all instances of the NullOr type with the given `null_id` to either 0 or the
    /// subtype depending on `is_null`
    pub fn set_null(&mut self, null_id: &MemoryId, is_null: bool) {
        let mut i = 0;
        while i < self.data.len() {
            self.data[i].set_null(null_id, is_null);
            i += 1;
        }
    }

    pub fn get(&self, index: usize) -> &Type {
        if (self.mask & Self::bit(index)) == 0 {
            &Type::UNINITIALIZED
        } else {
            &self.data[self.position(index)]
        }
    }

    pub fn set(&mut self, index: usize, t: Type) {
        let bit = Self::bit(index);
        let position = self.position(index);
        let is_set = (self.mask & bit) != 0;
        if t == Type::UNINITIALIZED {
            if is_set {
                let mut new_data = Vec::new();
                let mut i = 0;
                while i < self.data.len() {
                    if i != position {
                        new_data.push(self.data[i].clone());
                    }
                    i += 1;
                }
                self.data = new_data;
                self.mask &= !bit;
            }
        } else if is_set {
            if self.data[position] != t {
                self.data[position] = t;
            }
        } else {
            let mut new_data = Vec::new();
            let mut i = 0;
            while i < position {
                new_data.push(self.data[i].clone());
                i += 1;
            }
            new_data.push(t);
            while i < self.data.len() {
                new_data.push(self.data[i].clone());
                i += 1;
            }
            self.data = new_data;
            self.mask |= bit;
        }
    }

    /// Uninitializes all the elements whose bit is not set in `mask`.
    pub fn retain(&mut self, mask: u64) {
        if (self.mask & !mask) == 0 {
            return;
        }
        if mask == 0 {
            self.data = Vec::new();
            self.mask = 0;
            return;
        }
        let mut new_data = Vec::new();
        let mut data_pos = 0usize;
        let mut bit_idx = 0usize;
        while bit_idx < 64 {
            let b = Self::bit(bit_idx);
            if (self.mask & b) != 0 {
                if (mask & b) != 0 {
                    new_data.push(self.data[data_pos].clone());
                }
                data_pos += 1;
            }
            bit_idx += 1;
        }
        self.data = new_data;
        self.mask &= mask;
    }

    fn extract_sub_value(value: u64, offset: usize, byte_count: usize) -> u64 {
        if byte_count == 0 || offset >= 8 {
            return 0;
        }
        let shift = (offset * 8) as u32;
        let shifted = value.wrapping_shr(shift);
        if byte_count >= 8 {
            shifted
        } else {
            let mask = 1u64.wrapping_shl((byte_count * 8) as u32) - 1;
            shifted & mask
        }
    }

    fn insert_sub_value(original: u64, value: u64, width: DataWidth, offset: usize) -> u64 {
        let bit_width = width.bits() as u32;
        if bit_width >= 64 {
            return value;
        }
        let bit_offset = (offset * 8) as u32;
        let low_mask = 1u64.wrapping_shl(bit_width) - 1;
        let clear_mask = !low_mask.wrapping_shl(bit_offset);
        (original & clear_mask) | (value & low_mask).wrapping_shl(bit_offset)
    }

    pub fn write_data_ptr(
        &mut self,
        mut offset: StackOffset,
        bytes: u64,
    ) -> Result<(), VerifierErrorKind> {
        let mut i = 0u64;
        while i < bytes {
            match self.store(offset, Type::UNKNOWN_SCALAR, DataWidth::U8) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            offset = offset.add_u64(1);
            i += 1;
        }
        Ok(())
    }

    fn read_element(
        &self,
        index: usize,
        start_offset: usize,
        end_offset: usize,
    ) -> Result<(), VerifierErrorKind> {
        match self.get(index) {
            Type::ScalarValue(data) => {
                let unwritten_bits = Self::extract_sub_value(
                    data.unwritten_mask,
                    start_offset,
                    end_offset - start_offset,
                );
                if unwritten_bits == 0 {
                    Ok(())
                } else {
                    Err(VerifierErrorKind::UninitializedStackRead)
                }
            }
            _ => Err(VerifierErrorKind::NonScalarStackRead),
        }
    }

    pub fn read_data_ptr(&self, offset: StackOffset, bytes: u64) -> Result<(), VerifierErrorKind> {
        if bytes == 0 {
            return Ok(());
        }
        if bytes > (BPF_STACK_SIZE as u64) {
            return Err(VerifierErrorKind::StackAccessOutOfBounds);
        }
        match offset.check_valid_offset() {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        let end_offset = offset.add_u64(bytes);
        if !end_offset.is_within_stack() {
            return Err(VerifierErrorKind::StackAccessOutOfBounds);
        }
        // Handle the case where all the data is contained in a single element excluding the last
        // byte (the case when the read ends at an element edge, i.e. `end_offset.sub_index() == 0`,
        // is covered by the default path below).
        if offset.array_index() == end_offset.array_index() {
            return self.read_element(
                offset.array_index(),
                offset.sub_index(),
                end_offset.sub_index(),
            );
        }
        // Handle the first element, which might be partial.
        match self.read_element(offset.array_index(), offset.sub_index(), STACK_ELEMENT_SIZE) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        // Handle the last element, which might be partial.
        if end_offset.sub_index() != 0 {
            match self.read_element(end_offset.array_index(), 0, end_offset.sub_index()) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
        }
        // Handle any full elements between beginning and end.
        let mut i = offset.array_index() + 1;
        let end_idx = end_offset.array_index();
        while i < end_idx {
            match self.read_element(i, 0, STACK_ELEMENT_SIZE) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            i += 1;
        }
        Ok(())
    }

    pub fn store(
        &mut self,
        offset: StackOffset,
        value: Type,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        match offset.check_valid_offset() {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        if (offset.sub_index() % width.bytes()) != 0 {
            return Err(VerifierErrorKind::MisalignedStackAccess);
        }
        let index = offset.array_index();
        if width == DataWidth::U64 {
            self.set(index, value);
        } else {
            match value {
                Type::ScalarValue(data) => {
                    if Self::extract_sub_value(data.unwritten_mask, 0, width.bytes()) != 0 {
                        return Err(VerifierErrorKind::InvalidStoredValue);
                    }
                    let old_data = match self.get(index) {
                        Type::ScalarValue(d) => *d,
                        _ => {
                            // The value in the stack is not a scalar; treat it as an uninitialized
                            // scalar value with no written bits.
                            ScalarValueData::UNINITIALIZED
                        }
                    };
                    let sub_index = offset.sub_index();
                    let new_val =
                        Self::insert_sub_value(old_data.value, data.value, width, sub_index);
                    let unknown_mask = Self::insert_sub_value(
                        old_data.unknown_mask,
                        data.unknown_mask,
                        width,
                        sub_index,
                    );
                    let unwritten_mask = Self::insert_sub_value(
                        old_data.unwritten_mask,
                        data.unwritten_mask,
                        width,
                        sub_index,
                    );
                    let urange = U64Range::compute_range_for_bytes_swap(
                        old_data.urange,
                        data.urange,
                        sub_index,
                        0,
                        width.bytes(),
                    );
                    self.set(
                        index,
                        Type::ScalarValue(ScalarValueData::new(
                            new_val,
                            unknown_mask,
                            unwritten_mask,
                            urange,
                        )),
                    );
                }
                _ => return Err(VerifierErrorKind::PartialNonScalarStackStore),
            }
        }
        Ok(())
    }

    pub fn load(&self, offset: StackOffset, width: DataWidth) -> Result<Type, VerifierErrorKind> {
        match offset.check_valid_offset() {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        if (offset.sub_index() % width.bytes()) != 0 {
            return Err(VerifierErrorKind::MisalignedStackAccess);
        }
        let index = offset.array_index();
        let loaded_type = self.get(index).clone();
        let result = if width == DataWidth::U64 {
            loaded_type
        } else {
            match loaded_type {
                Type::ScalarValue(data) => {
                    let sub_index = offset.sub_index();
                    let val = Self::extract_sub_value(data.value, sub_index, width.bytes());
                    let unknown_mask =
                        Self::extract_sub_value(data.unknown_mask, sub_index, width.bytes());
                    let unwritten_mask =
                        Self::extract_sub_value(data.unwritten_mask, sub_index, width.bytes());
                    let urange = U64Range::compute_range_for_bytes_swap(
                        U64Range::from_scalar(0),
                        data.urange,
                        0,
                        sub_index,
                        width.bytes(),
                    );
                    Type::ScalarValue(ScalarValueData::new(
                        val,
                        unknown_mask,
                        unwritten_mask,
                        urange,
                    ))
                }
                _ => return Err(VerifierErrorKind::PartialNonScalarStackLoad),
            }
        };
        if !result.is_initialized() {
            return Err(VerifierErrorKind::UninitializedStackRead);
        }
        Ok(result)
    }
}

/// Two types are ordered with `t1` > `t2` if a proof that a program in a state `t1` finish is also
/// a proof that a program in a state `t2` finish.
impl PartialOrd for Stack {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let mut result = match ((self.mask & !other.mask) != 0, (!self.mask & other.mask) != 0) {
            (true, true) => return None,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        };
        let common = self.mask & other.mask;
        if common == 0 {
            return Some(result);
        }
        if self.mask == other.mask {
            let mut i = 0usize;
            while i < self.data.len() {
                let Some(ord) = self.data[i].partial_cmp(&other.data[i]) else {
                    return None;
                };
                let Some(next_res) = associate_orderings(result, ord) else {
                    return None;
                };
                result = next_res;
                i += 1;
            }
            return Some(result);
        }
        let mut pos_self = 0usize;
        let mut pos_other = 0usize;
        let mut index = 0usize;
        while index < 64 {
            let b = Self::bit(index);
            let in_self = (self.mask & b) != 0;
            let in_other = (other.mask & b) != 0;
            if in_self && in_other {
                let Some(ord) = self.data[pos_self].partial_cmp(&other.data[pos_other]) else {
                    return None;
                };
                let Some(next_res) = associate_orderings(result, ord) else {
                    return None;
                };
                result = next_res;
            }
            if in_self {
                pos_self += 1;
            }
            if in_other {
                pos_other += 1;
            }
            index += 1;
        }
        Some(result)
    }
}

fn clone_registers(
    regs: &[Type; GENERAL_REGISTER_COUNT as usize],
) -> [Type; GENERAL_REGISTER_COUNT as usize] {
    [
        regs[0].clone(),
        regs[1].clone(),
        regs[2].clone(),
        regs[3].clone(),
        regs[4].clone(),
        regs[5].clone(),
        regs[6].clone(),
        regs[7].clone(),
        regs[8].clone(),
        regs[9].clone(),
    ]
}

fn eq_registers(
    r1: &[Type; GENERAL_REGISTER_COUNT as usize],
    r2: &[Type; GENERAL_REGISTER_COUNT as usize],
) -> bool {
    let mut i = 0;
    while i < (GENERAL_REGISTER_COUNT as usize) {
        if r1[i] != r2[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The state of the computation as known by the verifier at a given point in time.
#[doc(hidden)]
#[derive(Debug)]
pub struct ComputationContext {
    /// The program counter.
    pub pc: ProgramCounter,
    /// Register 0 to 9.
    pub registers: [Type; GENERAL_REGISTER_COUNT as usize],
    /// The state of the stack.
    pub stack: Stack,
    /// The dynamically known bounds of buffers indexed by their ids.
    pub array_bounds: BoundsMap,
    /// The currently allocated resources.
    pub resources: IdSet,
}

impl Default for ComputationContext {
    fn default() -> Self {
        Self {
            pc: 0,
            registers: [
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
                Type::UNINITIALIZED,
            ],
            stack: Stack::default(),
            array_bounds: BoundsMap::new(),
            resources: IdSet::new(),
        }
    }
}

impl Clone for ComputationContext {
    fn clone(&self) -> Self {
        Self {
            pc: self.pc,
            registers: clone_registers(&self.registers),
            stack: self.stack.clone(),
            array_bounds: self.array_bounds.clone(),
            resources: self.resources.clone(),
        }
    }
}

impl PartialEq for ComputationContext {
    fn eq(&self, other: &Self) -> bool {
        self.pc == other.pc
            && eq_registers(&self.registers, &other.registers)
            && self.stack == other.stack
            && self.array_bounds == other.array_bounds
            && self.resources == other.resources
    }
}

impl Eq for ComputationContext {}

impl ComputationContext {
    /// Replace all instances of the NullOr type with the given `null_id` to either 0 or the
    /// subtype depending on `is_null`
    pub fn set_null(&mut self, null_id: &MemoryId, is_null: bool) {
        let mut i = 0;
        while i < self.registers.len() {
            self.registers[i].set_null(null_id, is_null);
            i += 1;
        }
        self.stack.set_null(null_id, is_null);
    }

    pub fn reg(&self, index: Register) -> Result<Type, VerifierErrorKind> {
        if index >= REGISTER_COUNT {
            return Err(VerifierErrorKind::InvalidRegister(index));
        }
        if index < GENERAL_REGISTER_COUNT {
            Ok(self.registers[index as usize].clone())
        } else {
            Ok(Type::PtrToStack { offset: StackOffset::default() })
        }
    }

    pub fn set_reg(&mut self, index: Register, reg_type: Type) -> Result<(), VerifierErrorKind> {
        if index >= REGISTER_COUNT {
            return Err(VerifierErrorKind::InvalidRegister(index));
        }
        if index >= GENERAL_REGISTER_COUNT {
            return Err(VerifierErrorKind::WriteToReadOnlyRegister(index));
        }
        self.registers[index as usize] = reg_type;
        Ok(())
    }

    pub fn update_array_bounds(&mut self, id: MemoryId, new_bound: ScalarValueData) {
        self.array_bounds.update_max(id, new_bound.min());
    }

    fn get_map_schema(&self, argument: u8) -> Result<MapSchema, VerifierErrorKind> {
        let Some(reg) = argument.checked_add(1) else {
            return Err(VerifierErrorKind::InvalidRegister(argument));
        };
        if reg > ARGUMENT_COUNT {
            return Err(VerifierErrorKind::InvalidRegister(reg));
        }
        let reg_val = match self.reg(reg) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        match reg_val {
            Type::ConstPtrToMap { schema, .. } => Ok(schema),
            _ => Err(VerifierErrorKind::HelperArgumentNotMap(argument)),
        }
    }

    fn next(&self) -> Result<Self, VerifierErrorKind> {
        self.jump_with_offset(0)
    }

    /// Returns a new `ComputationContext` where the pc has jump by `offset + 1`. In particular,
    /// the next instruction is reached with `jump_with_offset(0)`.
    ///
    /// This does not check that the target is inside the program: falling off the end of the
    /// program is reported by the main verification loop. Use `checked_jump_with_offset` for
    /// explicit jumps.
    fn jump_with_offset(&self, offset: i16) -> Result<Self, VerifierErrorKind> {
        let Some(step1) = checked_add_signed_usize(self.pc, offset as isize) else {
            return Err(VerifierErrorKind::JumpOutOfBounds);
        };
        let Some(pc) = step1.checked_add(1) else {
            return Err(VerifierErrorKind::JumpOutOfBounds);
        };
        Ok(Self {
            pc,
            registers: clone_registers(&self.registers),
            stack: self.stack.clone(),
            array_bounds: self.array_bounds.clone(),
            resources: self.resources.clone(),
        })
    }

    /// Same as `jump_with_offset`, but also fails if the target of the jump is outside of the
    /// program.
    fn checked_jump_with_offset(
        &self,
        verification_context: &VerificationContext,
        offset: i16,
    ) -> Result<Self, VerifierErrorKind> {
        let result = match self.jump_with_offset(offset) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        if result.pc >= verification_context.code.len() {
            return Err(VerifierErrorKind::JumpOutOfBounds);
        }
        Ok(result)
    }

    fn check_memory_access(
        &self,
        dst_offset: ScalarValueData,
        dst_buffer_size: u64,
        instruction_offset: i16,
        width: usize,
    ) -> Result<(), VerifierErrorKind> {
        let base_range = dst_offset.urange.add_i16(instruction_offset);
        let Some(end) = base_range.max.checked_add(width as u64) else {
            return Err(VerifierErrorKind::OutOfBoundsMemoryAccess);
        };
        if end > dst_buffer_size || base_range.max == u64::MAX {
            return Err(VerifierErrorKind::OutOfBoundsMemoryAccess);
        }
        Ok(())
    }

    /// Checks that the atomic access to `addr + field.offset` is allowed: it
    /// must target either the stack or a map value, and it must be naturally
    /// aligned for `field.width`.
    ///
    /// The stack and map values are at least 8-byte aligned, so it is
    /// sufficient to check the offset of the access relative to the start of
    /// the buffer. The offset is tracked as a `ScalarValueData`, so the check
    /// requires the low bits of the offset to be known and zero.
    pub fn check_atomic_access(&self, addr: &Type, field: Field) -> Result<(), VerifierErrorKind> {
        let addr_inner = match addr.inner(self) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        let base_offset = match *addr_inner {
            Type::PtrToStack { offset } => offset.reg(),
            Type::PtrToMemory { offset, origin, .. } if origin.is_map_value() => offset,
            _ => return Err(VerifierErrorKind::AtomicAccessNotAllowed),
        };
        let alignment_mask = (field.width.bytes() as u64) - 1;
        // Adding a constant preserves the low bits of the result only if the low bits of the
        // other operand are known.
        if (base_offset.unknown_mask & alignment_mask) != 0
            || (base_offset.value.wrapping_add(field.offset as i64 as u64) & alignment_mask) != 0
        {
            return Err(VerifierErrorKind::MisalignedAtomicAccess);
        }
        Ok(())
    }

    pub fn store_memory(
        &mut self,
        context: &mut VerificationContext,
        pc: ProgramCounter,
        addr: &Type,
        field: Field,
        value: Type,
    ) -> Result<(), VerifierErrorKind> {
        match addr.check_not_released(&self.resources) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        let addr_unwrapped = addr.unwrap_releasable();
        let is_initialized_scalar = match value {
            Type::ScalarValue(data) => data.is_fully_initialized(),
            _ => false,
        };
        let mut is_stack = false;
        let struct_access = match *addr_unwrapped {
            Type::PtrToStack { offset } => {
                is_stack = true;
                let offset_sum = offset.add_i16(field.offset);
                match self.stack.store(offset_sum, value, field.width) {
                    Ok(()) => None,
                    Err(err) => return Err(err),
                }
            }
            Type::PtrToMemory { offset, buffer_size, origin, .. } if origin.is_writable() => {
                match self.check_memory_access(
                    offset,
                    buffer_size,
                    field.offset,
                    field.width.bytes(),
                ) {
                    Ok(()) => None,
                    Err(err) => return Err(err),
                }
            }
            Type::PtrToMemory { .. } => {
                return Err(VerifierErrorKind::WriteToReadOnlyMemory);
            }
            Type::PtrToStruct { ref id, offset, ref descriptor } => {
                let Some(field_desc) = (**descriptor).find_field(offset, field) else {
                    return Err(VerifierErrorKind::InvalidStructFieldAccess);
                };
                if !matches!(field_desc.field_type, FieldType::MutableScalar { .. }) {
                    return Err(VerifierErrorKind::WriteToReadOnlyField);
                }
                Some(StructAccess {
                    pc,
                    memory_id: id.clone(),
                    field_offset: field_desc.offset,
                    is_32_bit_ptr_load: false,
                })
            }
            Type::PtrToArray { ref id, offset } => {
                let bound = self.array_bounds.get_or_zero(id);
                match self.check_memory_access(offset, bound, field.offset, field.width.bytes()) {
                    Ok(()) => None,
                    Err(err) => return Err(err),
                }
            }
            _ => return Err(VerifierErrorKind::StoreToNonPointer),
        };
        // Private data should not be leaked outside the stack.
        if !is_stack && !is_initialized_scalar {
            return Err(VerifierErrorKind::InvalidStoredValue);
        }
        context.register_memory_access(pc, struct_access)
    }

    fn load_memory(
        &self,
        context: &mut VerificationContext,
        addr: &Type,
        field: Field,
    ) -> Result<Type, VerifierErrorKind> {
        let addr_inner = match addr.inner(self) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        let (loaded_type, struct_access) = match *addr_inner {
            Type::PtrToStack { offset } => {
                let offset_sum = offset.add_i16(field.offset);
                let val = match self.stack.load(offset_sum, field.width) {
                    Ok(v) => v,
                    Err(err) => return Err(err),
                };
                (val, None)
            }
            Type::PtrToMemory { offset, buffer_size, .. } => {
                match self.check_memory_access(
                    offset,
                    buffer_size,
                    field.offset,
                    field.width.bytes(),
                ) {
                    Ok(()) => (Type::UNKNOWN_SCALAR, None),
                    Err(err) => return Err(err),
                }
            }
            Type::PtrToStruct { ref id, offset, ref descriptor } => {
                let Some(field_desc) = (**descriptor).find_field(offset, field) else {
                    return Err(VerifierErrorKind::InvalidStructFieldAccess);
                };
                let (return_type, is_32_bit_ptr_load) = match &field_desc.field_type {
                    FieldType::Scalar { .. } | FieldType::MutableScalar { .. } => {
                        (Type::UNKNOWN_SCALAR, false)
                    }
                    FieldType::PtrToArray { id: array_id, is_32_bit } => (
                        Type::PtrToArray {
                            id: array_id.prepended(id.clone()),
                            offset: ScalarValueData::from_known(0),
                        },
                        *is_32_bit,
                    ),
                    FieldType::PtrToEndArray { id: array_id, is_32_bit } => {
                        (Type::PtrToEndArray { id: array_id.prepended(id.clone()) }, *is_32_bit)
                    }
                    FieldType::PtrToMemory { id: memory_id, buffer_size, is_32_bit } => (
                        Type::PtrToMemory {
                            id: memory_id.prepended(id.clone()),
                            offset: ScalarValueData::from_known(0),
                            buffer_size: *buffer_size as u64,
                            origin: PointerOrigin::KernelReadOnly,
                        },
                        *is_32_bit,
                    ),
                    FieldType::NullablePtrToMemory { id: memory_id, buffer_size, is_32_bit } => {
                        let prep = memory_id.prepended(id.clone());
                        (
                            Type::NullOr {
                                id: prep.clone(),
                                inner: Box::new(Type::PtrToMemory {
                                    id: prep,
                                    offset: ScalarValueData::from_known(0),
                                    buffer_size: *buffer_size as u64,
                                    origin: PointerOrigin::KernelReadOnly,
                                }),
                            },
                            *is_32_bit,
                        )
                    }
                };
                (
                    return_type,
                    Some(StructAccess {
                        pc: self.pc,
                        memory_id: id.clone(),
                        field_offset: field_desc.offset,
                        is_32_bit_ptr_load,
                    }),
                )
            }
            Type::PtrToArray { ref id, offset } => {
                let bound = self.array_bounds.get_or_zero(id);
                match self.check_memory_access(offset, bound, field.offset, field.width.bytes()) {
                    Ok(()) => (Type::UNKNOWN_SCALAR, None),
                    Err(err) => return Err(err),
                }
            }
            _ => return Err(VerifierErrorKind::LoadFromNonPointer),
        };
        match context.register_memory_access(self.pc, struct_access) {
            Ok(()) => Ok(loaded_type),
            Err(err) => Err(err),
        }
    }

    /// Given `return_value` in a helper signature, return the concrete type to use, updating the
    /// `next` context as needed.
    pub fn resolve_return_value(
        &self,
        verification_context: &mut VerificationContext,
        return_value: &Type,
        next: &mut ComputationContext,
    ) -> Result<Type, VerifierErrorKind> {
        match return_value {
            Type::ReleasableParameter { id, inner } => {
                let rel_id = verification_context.next_id().prepended(id.clone());
                next.resources.insert(rel_id.clone());
                let resolved_inner =
                    match self.resolve_return_value(verification_context, &**inner, next) {
                        Ok(t) => t,
                        Err(err) => return Err(err),
                    };
                Ok(Type::Releasable { id: rel_id, inner: Box::new(resolved_inner) })
            }
            Type::NullOrParameter(t) => {
                let id = verification_context.next_id();
                let resolved_inner =
                    match self.resolve_return_value(verification_context, &**t, next) {
                        Ok(t) => t,
                        Err(err) => return Err(err),
                    };
                Ok(Type::NullOr { id, inner: Box::new(resolved_inner) })
            }
            Type::MapValueParameter { map_ptr_index } => {
                let schema = match self.get_map_schema(*map_ptr_index) {
                    Ok(s) => s,
                    Err(err) => return Err(err),
                };
                let id = verification_context.next_id();
                let origin = if schema.flags.contains(MapFlags::ProgReadOnly) {
                    PointerOrigin::MapReadOnly
                } else {
                    PointerOrigin::MapWritable
                };
                Ok(Type::PtrToMemory {
                    id,
                    offset: ScalarValueData::from_known(0),
                    buffer_size: schema.value_size as u64,
                    origin,
                })
            }
            Type::MemoryParameter { size, .. } => {
                let buffer_size = match size.size(self) {
                    Ok(sz) => sz,
                    Err(err) => return Err(err),
                };
                let id = verification_context.next_id();
                Ok(Type::PtrToMemory {
                    id,
                    offset: ScalarValueData::from_known(0),
                    buffer_size,
                    origin: PointerOrigin::MapWritable,
                })
            }
            Type::PtrToMemory { id, offset, buffer_size, origin } => {
                let new_id = id.prepended(verification_context.next_id());
                Ok(Type::PtrToMemory {
                    id: new_id,
                    offset: *offset,
                    buffer_size: *buffer_size,
                    origin: *origin,
                })
            }
            Type::PtrToStruct { id, offset, descriptor } => {
                let new_id = id.prepended(verification_context.next_id());
                Ok(Type::PtrToStruct {
                    id: new_id,
                    offset: *offset,
                    descriptor: Box::new((**descriptor).clone()),
                })
            }
            t => Ok(t.clone()),
        }
    }

    fn compute_source(&self, src: Source) -> Result<Type, VerifierErrorKind> {
        match src {
            Source::Reg(reg) => self.reg(reg),
            Source::Value(v) => Ok(Type::ScalarValue(ScalarValueData::from_known(v))),
        }
    }

    fn apply_computation(
        context: &ComputationContext,
        op1: Type,
        op2: Type,
        alu_type: AluType,
        alu_op: ScalarAluOp,
    ) -> Result<Type, VerifierErrorKind> {
        let inner1 = match op1.inner(context) {
            Ok(t) => t,
            Err(err) => return Err(err),
        };
        let inner2 = match op2.inner(context) {
            Ok(t) => t,
            Err(err) => return Err(err),
        };
        let result: Type = match (alu_type, inner1, inner2) {
            (_, Type::ScalarValue(data1), Type::ScalarValue(data2)) => {
                Type::ScalarValue(alu_op.eval(*data1, *data2))
            }
            (
                AluType::Add,
                Type::ScalarValue(_),
                Type::PtrToStack { .. }
                | Type::PtrToMemory { .. }
                | Type::PtrToStruct { .. }
                | Type::PtrToArray { .. },
            ) => {
                return Self::apply_computation(context, op2, op1, alu_type, alu_op);
            }
            // Pointer arithmetic with a scalar that is not fully initialized (either never
            // written, or derived from a pointer value) is prohibited: it would allow the
            // pointer value to leak through the resulting offset.
            (
                at,
                Type::PtrToStack { .. }
                | Type::PtrToMemory { .. }
                | Type::PtrToStruct { .. }
                | Type::PtrToArray { .. },
                Type::ScalarValue(data),
            ) if at.is_ptr_compatible() && !data.is_fully_initialized() => {
                return Err(VerifierErrorKind::UninitializedPointerArithmetic);
            }
            (at, Type::PtrToStack { offset: x }, Type::ScalarValue(data))
                if at.is_ptr_compatible() =>
            {
                const BPF_MAX_VAR_OFF: u64 = 1 << 29;
                let offset = alu_op.eval(x.reg(), *data);
                if offset.max() > BPF_MAX_VAR_OFF {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                }
                Type::PtrToStack { offset: StackOffset(offset) }
            }
            (
                at,
                Type::PtrToMemory { offset: x, .. } | Type::PtrToStruct { offset: x, .. },
                Type::ScalarValue(data),
            ) if at.is_ptr_compatible() => {
                const BPF_MAX_VAR_OFF: u64 = 1 << 29;
                let offset = alu_op.eval(*x, *data);
                if offset.max() > BPF_MAX_VAR_OFF {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                }
                op1.with_offset(offset)
            }
            (AluType::Add, Type::PtrToArray { offset: x, .. }, Type::ScalarValue(data)) => {
                let Some(offset) = x.checked_add(*data) else {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                };
                if offset.max() > MAX_PACKET_OFFSET {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                }
                op1.with_offset(offset)
            }
            (AluType::Sub, Type::PtrToArray { offset: x, .. }, Type::ScalarValue(data)) => {
                let Some(offset) = x.checked_sub(*data) else {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                };
                if offset.max() > MAX_PACKET_OFFSET {
                    return Err(VerifierErrorKind::PointerOffsetOverflow);
                }
                op1.with_offset(offset)
            }
            (
                AluType::Sub,
                Type::PtrToMemory { id: id1, offset: x1, .. },
                Type::PtrToMemory { id: id2, offset: x2, .. },
            )
            | (
                AluType::Sub,
                Type::PtrToStruct { id: id1, offset: x1, .. },
                Type::PtrToStruct { id: id2, offset: x2, .. },
            )
            | (
                AluType::Sub,
                Type::PtrToArray { id: id1, offset: x1 },
                Type::PtrToArray { id: id2, offset: x2 },
            ) if *id1 == *id2 => Type::ScalarValue(alu_op.eval(*x1, *x2)),
            (AluType::Sub, Type::PtrToStack { offset: x1 }, Type::PtrToStack { offset: x2 }) => {
                Type::ScalarValue(alu_op.eval(x1.reg(), x2.reg()))
            }
            (AluType::Sub, Type::PtrToArray { id: id1, .. }, Type::PtrToEndArray { id: id2 })
            | (AluType::Sub, Type::PtrToEndArray { id: id1 }, Type::PtrToArray { id: id2, .. })
                if *id1 == *id2 =>
            {
                Type::UNKNOWN_SCALAR
            }
            _ => Type::UNINITIALIZED,
        };
        Ok(result)
    }

    fn alu(
        &self,
        log_op: Option<AluLogOp>,
        verification_context: &mut VerificationContext,
        dst: Register,
        src: Source,
        alu_type: AluType,
        alu_op: ScalarAluOp,
    ) -> Result<(), VerifierErrorKind> {
        if let Some(op) = log_op {
            verification_context.emit_log(self.pc, VerifierLogEvent::Alu { op, dst, src });
        }
        let op1 = match self.reg(dst) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let op2 = match self.compute_source(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let result = match Self::apply_computation(self, op1, op2, alu_type, alu_op) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, result) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        verification_context.push_successor(next);
        Ok(())
    }

    fn log_atomic_operation(
        &self,
        op: AtomicLogOp,
        verification_context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) {
        verification_context
            .emit_log(self.pc, VerifierLogEvent::Atomic { op, fetch, dst, offset, src });
    }

    fn atomic_operation(
        &self,
        log_op: AtomicLogOp,
        verification_context: &mut VerificationContext,
        width: DataWidth,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
        alu_op: ScalarAluOp,
    ) -> Result<(), VerifierErrorKind> {
        self.log_atomic_operation(log_op, verification_context, fetch, dst, offset, src);
        let addr = match self.reg(dst) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let value = match self.reg(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let field = Field::new(offset, width);
        match self.check_atomic_access(&addr, field) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        let loaded_type = match self.load_memory(verification_context, &addr, field) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let Some(loaded_data) = loaded_type.written_scalar() else {
            return Err(VerifierErrorKind::NonScalarStackRead);
        };
        let Some(value_data) = value.written_scalar() else {
            return Err(VerifierErrorKind::InvalidStoredValue);
        };
        let result = Type::ScalarValue(alu_op.eval(loaded_data, value_data));
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.store_memory(verification_context, self.pc, &addr, field, result) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        if fetch {
            match next.set_reg(src, loaded_type) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
        }
        verification_context.push_successor(next);
        Ok(())
    }

    fn raw_atomic_cmpxchg(
        &self,
        log_op: AtomicLogOp,
        verification_context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
        jump_width: JumpWidth,
        cond_op: BranchCondOp,
    ) -> Result<(), VerifierErrorKind> {
        self.log_atomic_operation(log_op, verification_context, true, dst, offset, src);
        let width = match jump_width {
            JumpWidth::W32 => DataWidth::U32,
            JumpWidth::W64 => DataWidth::U64,
        };
        let addr = match self.reg(dst) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let field = Field::new(offset, width);
        match self.check_atomic_access(&addr, field) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        let dst_val = match self.load_memory(verification_context, &addr, field) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let value = match self.reg(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let r0 = match self.reg(0) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        if !dst_val.is_written_scalar() {
            return Err(VerifierErrorKind::NonScalarStackRead);
        }
        if !value.is_written_scalar() {
            return Err(VerifierErrorKind::InvalidStoredValue);
        }
        if !r0.is_written_scalar() {
            return Err(VerifierErrorKind::InvalidComparison);
        }
        let branch = match self.compute_branch(JumpType::Eq, jump_width, &dst_val, &r0, cond_op) {
            Ok(b) => b,
            Err(err) => return Err(err),
        };
        let take_eq = match branch {
            Some(b) => b,
            None => true,
        };
        let take_ne = match branch {
            Some(b) => !b,
            None => true,
        };
        // r0 = dst
        if take_eq {
            let mut next = match self.next() {
                Ok(n) => n,
                Err(err) => return Err(err),
            };
            let (c_dst, _) = match Type::constraint(
                &mut next,
                JumpType::Eq,
                jump_width,
                dst_val.clone(),
                r0.clone(),
            ) {
                Ok(pair) => pair,
                Err(err) => return Err(err),
            };
            match next.set_reg(0, c_dst) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            match next.store_memory(verification_context, self.pc, &addr, field, value) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            verification_context.push_successor(next);
        }
        // r0 != dst
        if take_ne {
            let mut next = match self.next() {
                Ok(n) => n,
                Err(err) => return Err(err),
            };
            let (c_dst, _) =
                match Type::constraint(&mut next, JumpType::Ne, jump_width, dst_val, r0) {
                    Ok(pair) => pair,
                    Err(err) => return Err(err),
                };
            match next.set_reg(0, c_dst.clone()) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            match next.store_memory(verification_context, self.pc, &addr, field, c_dst) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            verification_context.push_successor(next);
        }

        Ok(())
    }

    fn endianness(
        &self,
        to_be: bool,
        verification_context: &mut VerificationContext,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        verification_context.emit_log(self.pc, VerifierLogEvent::Endian { to_be, width, dst });
        let value = match self.reg(dst) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let val_inner = match value.inner(self) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        // Reject partially or uninitialized scalars (since 16- and 32-bit byte swaps
        // zero-extend the upper bits into `unwritten_mask == 0`). Pointers are rejected unless
        // `allow_ptr_leaks` is set, in which case they coerce to an unknown scalar.
        let new_value = match val_inner {
            Type::ScalarValue(data) if data.is_fully_initialized() => {
                Type::ScalarValue(ScalarValueData::new(
                    apply_endianness(to_be, width, data.value),
                    apply_endianness(to_be, width, data.unknown_mask),
                    0,
                    U64Range::max(),
                ))
            }
            Type::ScalarValue(_) => return Err(VerifierErrorKind::InvalidByteSwapTarget),
            _ if verification_context.calling_context.allow_ptr_leaks => Type::UNKNOWN_SCALAR,
            _ => return Err(VerifierErrorKind::InvalidByteSwapTarget),
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, new_value) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        verification_context.push_successor(next);
        Ok(())
    }

    fn eval_same_region_ptr(
        jump_type: JumpType,
        cond_op: BranchCondOp,
        sx: ScalarValueData,
        sy: ScalarValueData,
        in_bounds: bool,
    ) -> Result<Option<bool>, ()> {
        if !sx.is_fully_initialized() || !sy.is_fully_initialized() {
            return Err(());
        }
        match jump_type {
            JumpType::Eq | JumpType::Ne => cond_op.eval(sx, sy),
            JumpType::Gt | JumpType::Ge | JumpType::Lt | JumpType::Le => {
                if in_bounds {
                    cond_op.eval(sx, sy)
                } else {
                    Ok(None)
                }
            }
            JumpType::LooseComparaison | JumpType::StrictComparaison | JumpType::Unknown => {
                Ok(None)
            }
        }
    }

    fn compute_branch(
        &self,
        jump_type: JumpType,
        jump_width: JumpWidth,
        op1: &Type,
        op2: &Type,
        cond_op: BranchCondOp,
    ) -> Result<Option<bool>, VerifierErrorKind> {
        let inner1 = match op1.inner(self) {
            Ok(t) => t,
            Err(err) => return Err(err),
        };
        let inner2 = match op2.inner(self) {
            Ok(t) => t,
            Err(err) => return Err(err),
        };
        let res = match (jump_width, inner1, inner2) {
            (_, Type::ScalarValue(data1), Type::ScalarValue(data2)) => cond_op.eval(*data1, *data2),
            (JumpWidth::W64, Type::ScalarValue(data), Type::NullOr { .. }) if data.is_zero() => {
                Ok(None)
            }
            (JumpWidth::W64, Type::NullOr { .. }, Type::ScalarValue(data)) if data.is_zero() => {
                Ok(None)
            }
            (JumpWidth::W64, Type::ScalarValue(data), t) if data.is_zero() && t.is_non_zero() => {
                let non_zero =
                    ScalarValueData::UNKNOWN_WRITTEN.update_range(U64Range::new(1, u64::MAX));
                cond_op.eval(ScalarValueData::from_known(0), non_zero)
            }
            (JumpWidth::W64, t, Type::ScalarValue(data)) if data.is_zero() && t.is_non_zero() => {
                let non_zero =
                    ScalarValueData::UNKNOWN_WRITTEN.update_range(U64Range::new(1, u64::MAX));
                cond_op.eval(non_zero, ScalarValueData::from_known(0))
            }
            (JumpWidth::W64, Type::PtrToStack { offset: x }, Type::PtrToStack { offset: y }) => {
                let in_bounds = x.reg().max() <= (BPF_STACK_SIZE as u64)
                    && y.reg().max() <= (BPF_STACK_SIZE as u64);
                Self::eval_same_region_ptr(jump_type, cond_op, x.reg(), y.reg(), in_bounds)
            }
            (
                JumpWidth::W64,
                Type::PtrToMemory { id: id1, offset: x, buffer_size: b1, .. },
                Type::PtrToMemory { id: id2, offset: y, buffer_size: b2, .. },
            ) if id1 == id2 => {
                let in_bounds = x.max() <= *b1 && y.max() <= *b2;
                Self::eval_same_region_ptr(jump_type, cond_op, *x, *y, in_bounds)
            }
            (
                JumpWidth::W64,
                Type::PtrToStruct { id: id1, offset: x, .. },
                Type::PtrToStruct { id: id2, offset: y, .. },
            ) if id1 == id2 => {
                Self::eval_same_region_ptr(jump_type, cond_op, *x, *y, x.is_zero() && y.is_zero())
            }
            (
                JumpWidth::W64,
                Type::PtrToArray { id: id1, offset: x },
                Type::PtrToArray { id: id2, offset: y },
            ) if id1 == id2 => {
                let bound = self.array_bounds.get_or_zero(id1);
                Self::eval_same_region_ptr(
                    jump_type,
                    cond_op,
                    *x,
                    *y,
                    x.max() <= bound && y.max() <= bound,
                )
            }
            (JumpWidth::W64, Type::PtrToArray { id: id1, .. }, Type::PtrToEndArray { id: id2 })
                if id1 == id2 =>
            {
                Ok(None)
            }
            (JumpWidth::W64, Type::PtrToEndArray { id: id1 }, Type::PtrToArray { id: id2, .. })
                if id1 == id2 =>
            {
                Ok(None)
            }
            _ => Err(()),
        };
        match res {
            Ok(v) => Ok(v),
            Err(()) => Err(VerifierErrorKind::InvalidComparison),
        }
    }

    fn apply_constraints_and_register(
        mut next: Self,
        jump_type: JumpType,
        jump_width: JumpWidth,
        dst: Register,
        src: Source,
        op1: Type,
        op2: Type,
    ) -> Result<Self, VerifierErrorKind> {
        if jump_type != JumpType::Unknown {
            let (new_op1, new_op2) =
                match Type::constraint(&mut next, jump_type, jump_width, op1, op2) {
                    Ok(pair) => pair,
                    Err(err) => return Err(err),
                };
            if dst < GENERAL_REGISTER_COUNT {
                match next.set_reg(dst, new_op1) {
                    Ok(()) => {}
                    Err(err) => return Err(err),
                }
            }
            if let Source::Reg(r) = src {
                if r < GENERAL_REGISTER_COUNT {
                    match next.set_reg(r, new_op2) {
                        Ok(()) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
        Ok(next)
    }

    fn conditional_jump(
        &self,
        log_op: JmpLogOp,
        verification_context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
        jump_type: JumpType,
        jump_width: JumpWidth,
        cond_op: BranchCondOp,
    ) -> Result<(), VerifierErrorKind> {
        verification_context
            .emit_log(self.pc, VerifierLogEvent::CondJump { op: log_op, dst, src, offset });
        let op1 = match self.reg(dst) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let op2 = match self.compute_source(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let branch = match self.compute_branch(jump_type, jump_width, &op1, &op2, cond_op) {
            Ok(b) => b,
            Err(err) => return Err(err),
        };
        let take_jump = match branch {
            Some(b) => b,
            None => true,
        };
        let skip_jump = match branch {
            Some(b) => !b,
            None => true,
        };
        if take_jump {
            let jumped = match self.checked_jump_with_offset(verification_context, offset) {
                Ok(j) => j,
                Err(err) => return Err(err),
            };
            let constrained = match Self::apply_constraints_and_register(
                jumped,
                jump_type,
                jump_width,
                dst,
                src,
                op1.clone(),
                op2.clone(),
            ) {
                Ok(c) => c,
                Err(err) => return Err(err),
            };
            verification_context.push_successor(constrained);
        }
        if skip_jump {
            let fallthrough = match self.jump_with_offset(0) {
                Ok(f) => f,
                Err(err) => return Err(err),
            };
            let constrained = match Self::apply_constraints_and_register(
                fallthrough,
                jump_type.invert(),
                jump_width,
                dst,
                src,
                op1,
                op2,
            ) {
                Ok(c) => c,
                Err(err) => return Err(err),
            };
            verification_context.push_successor(constrained);
        }
        Ok(())
    }
}

fn compare_bounds_maps(m1: &BoundsMap, m2: &BoundsMap, mut result: Ordering) -> Option<Ordering> {
    let mut i1 = 0usize;
    let mut i2 = 0usize;
    let len1 = m1.entries.len();
    let len2 = m2.entries.len();
    while i1 < len1 && i2 < len2 {
        let (ref k1, v1) = m1.entries[i1];
        let (ref k2, v2) = m2.entries[i2];
        let ord = match k1.cmp_id(k2) {
            Ordering::Equal => {
                i1 += 1;
                i2 += 1;
                // Intentionally reversed (`cmp_u64(v2, v1)`): a smaller bound in `m1` (`v1`)
                // assumes fewer valid bytes in the array/packet buffer than `m2` (`v2`), so a
                // termination proof from `m1` also holds for `m2` (`m1` dominates / is Greater).
                cmp_u64(v2, v1)
            }
            Ordering::Less => {
                i1 += 1;
                Ordering::Less
            }
            Ordering::Greater => {
                i2 += 1;
                Ordering::Greater
            }
        };
        let Some(next) = associate_orderings(result, ord) else {
            return None;
        };
        result = next;
    }
    if i1 < len1 {
        associate_orderings(result, Ordering::Less)
    } else if i2 < len2 {
        associate_orderings(result, Ordering::Greater)
    } else {
        Some(result)
    }
}

/// Two types are ordered with `t1` > `t2` if a proof that a program in a state `t1` finish is also
/// a proof that a program in a state `t2` finish.
impl PartialOrd for ComputationContext {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.pc != other.pc || self.resources.len() != other.resources.len() {
            return None;
        }
        let Some(mut result) = Type::compare_registers(&self.registers, &other.registers) else {
            return None;
        };
        let Some(stack_ord) = self.stack.partial_cmp(&other.stack) else {
            return None;
        };
        let Some(res2) = associate_orderings(result, stack_ord) else {
            return None;
        };
        result = res2;
        if self.resources != other.resources {
            return None;
        }
        compare_bounds_maps(&self.array_bounds, &other.array_bounds, result)
    }
}

/// Represents the read data dependencies of an eBPF program branch.
///
/// This struct tracks which registers and stack positions are *read* by the
/// instructions within a branch of the eBPF program.  This information is used
/// during branch cutting optimization to broaden terminating contexts.
///
/// The verifier assumes that data not read by a terminated branch is irrelevant
/// for future execution paths and can be safely cleared.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataDependencies {
    /// A bitset of registers read by the children of a context.
    pub registers: u16,
    /// A bitset of stack positions read by the children of a context.
    pub stack: u64,
}

impl DataDependencies {
    #[inline]
    pub fn has_reg(&self, reg: Register) -> bool {
        (self.registers & (1_u16 << reg)) != 0
    }

    #[inline]
    pub fn add_reg(&mut self, reg: Register) {
        self.registers |= 1_u16 << reg;
    }

    #[inline]
    fn remove_reg(&mut self, reg: Register) -> bool {
        let mask = 1_u16 << reg;
        let was_set = (self.registers & mask) != 0;
        self.registers &= !mask;
        was_set
    }

    #[inline]
    fn add_stack(&mut self, slot: usize) {
        self.stack |= 1_u64 << slot;
    }

    #[inline]
    fn remove_stack(&mut self, slot: usize) -> bool {
        let mask = 1_u64 << slot;
        let was_set = (self.stack & mask) != 0;
        self.stack &= !mask;
        was_set
    }

    #[inline]
    fn add_stack_range(&mut self, start: usize, end: usize) {
        if end > start {
            let count = end - start;
            let mask = if count >= 64 { !0 } else { ((1_u64 << count) - 1) << start };
            self.stack |= mask;
        }
    }

    fn merge(self, other: Self) -> Self {
        Self { registers: self.registers | other.registers, stack: self.stack | other.stack }
    }

    fn alu(&mut self, dst: Register, src: Source) -> Result<(), VerifierErrorKind> {
        // Only do something if the dst is read, otherwise the computation doesn't matter.
        if self.has_reg(dst) {
            if let Source::Reg(src) = src {
                self.add_reg(src);
            }
        }
        Ok(())
    }

    fn jmp(&mut self, dst: Register, src: Source) -> Result<(), VerifierErrorKind> {
        self.add_reg(dst);
        if let Source::Reg(src) = src {
            self.add_reg(src);
        }
        Ok(())
    }

    fn atomic(
        &mut self,
        context: &ComputationContext,
        dst: Register,
        offset: i16,
        src: Register,
        is_cmpxchg: bool,
    ) -> Result<(), VerifierErrorKind> {
        let reg = match context.reg(dst) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        let addr = match reg.inner(context) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        if let Type::PtrToStack { offset: stack_offset } = *addr {
            let stack_offset = stack_offset.add_i16(offset);
            match stack_offset.check_valid_offset() {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            let slot = stack_offset.array_index();
            self.add_stack(slot);
        }
        if is_cmpxchg {
            self.add_reg(0);
        }
        self.add_reg(src);
        self.add_reg(dst);
        Ok(())
    }
}

/// Pairs an owned `ComputationContext` with `DataDependencies` so backward
/// data-dependency propagation can implement `BpfVisitor<VerificationContext>`
/// and inspect register/stack types without storing borrows inside the visitor.
struct DependencyTracker {
    context: ComputationContext,
    deps: DataDependencies,
}

impl BpfVisitor<VerificationContext> for DependencyTracker {
    fn add(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn add64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn and(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn and64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn arsh(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn arsh64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn div(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn div64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn lsh(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn lsh64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn r#mod(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn mod64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn mul(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn mul64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn or(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn or64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn rsh(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn rsh64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn sub(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn sub64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn xor(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }
    fn xor64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.alu(dst, src)
    }

    fn mov(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        if src == Source::Reg(dst) || !self.deps.has_reg(dst) {
            return Ok(());
        }
        if let Source::Reg(src) = src {
            self.deps.add_reg(src);
        }
        self.deps.remove_reg(dst);
        Ok(())
    }
    fn mov64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.mov(context, dst, src)
    }

    fn neg(
        &mut self,
        _context: &mut VerificationContext,
        _dst: Register,
    ) -> Result<(), VerifierErrorKind> {
        // This is reading and writing the same value. This induces no change in dependencies.
        Ok(())
    }
    fn neg64(
        &mut self,
        _context: &mut VerificationContext,
        _dst: Register,
    ) -> Result<(), VerifierErrorKind> {
        // This is reading and writing the same value. This induces no change in dependencies.
        Ok(())
    }

    fn be(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        _width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        // The type of `dst` is always checked.
        self.deps.add_reg(dst);
        Ok(())
    }
    fn le(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        _width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        // The type of `dst` is always checked.
        self.deps.add_reg(dst);
        Ok(())
    }

    fn call_external(
        &mut self,
        context: &mut VerificationContext,
        index: u32,
    ) -> Result<(), VerifierErrorKind> {
        let Some(signature) = context.calling_context.find_helper(index) else {
            return Err(VerifierErrorKind::UnknownHelper(index));
        };
        let num_args = signature.args.len();
        if num_args > (ARGUMENT_COUNT as usize) {
            return Err(VerifierErrorKind::InvalidRegister(ARGUMENT_COUNT + 1));
        }
        // A helper reads the stack region each stack-input parameter argument
        // points at, so those stack slots are data dependencies, like load/store.
        let mut arg_index = 0usize;
        while arg_index < num_args {
            let arg = &signature.args[arg_index];
            let reg = match self.context.reg((arg_index + 1) as Register) {
                Ok(r) => r,
                Err(err) => return Err(err),
            };
            let reg_inner = match reg.inner(&self.context) {
                Ok(i) => i,
                Err(err) => return Err(err),
            };
            if let Type::PtrToStack { offset } = *reg_inner {
                let maybe_input_size = match arg.stack_input_size(&self.context) {
                    Ok(sz) => sz,
                    Err(err) => return Err(err),
                };
                if let Some(input_size) = maybe_input_size {
                    if input_size > 0 {
                        match offset.check_valid_offset() {
                            Ok(()) => {}
                            Err(err) => return Err(err),
                        }
                        let end = offset.add_u64(input_size);
                        if input_size > (BPF_STACK_SIZE as u64) || !end.is_within_stack() {
                            return Err(VerifierErrorKind::StackAccessOutOfBounds);
                        }
                        let start_idx = offset.array_index();
                        let end_idx = end.array_index();
                        self.deps.add_stack_range(start_idx, end_idx);
                        if end.sub_index() != 0 {
                            self.deps.add_stack(end_idx);
                        }
                    }
                }
            }
            arg_index += 1;
        }
        // 0 is overwritten and 1 to 5 are scratch registers
        self.deps.registers &= !0b0011_1111;
        // 1 to k are parameters.
        self.deps.registers |= ((1_u16 << num_args) - 1) << 1;
        Ok(())
    }

    fn exit(&mut self, _context: &mut VerificationContext) -> Result<(), VerifierErrorKind> {
        // This read r0 unconditionally.
        self.deps.add_reg(0);
        Ok(())
    }

    fn jump(
        &mut self,
        _context: &mut VerificationContext,
        _offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        // This doesn't do anything with values.
        Ok(())
    }

    fn jeq(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jeq64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jne(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jne64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jge(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jge64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jgt(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jgt64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jle(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jle64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jlt(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jlt64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsge(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsge64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsgt(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsgt64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsle(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jsle64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jslt(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jslt64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jset(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }
    fn jset64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.jmp(dst, src)
    }

    fn atomic_add(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_add64(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_and(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_and64(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_or(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_or64(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_xor(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_xor64(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_xchg(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_xchg64(
        &mut self,
        _context: &mut VerificationContext,
        _fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, false)
    }

    fn atomic_cmpxchg(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, true)
    }

    fn atomic_cmpxchg64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.atomic(&self.context, dst, offset, src, true)
    }

    fn load(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
        _width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.remove_reg(dst);
        let reg = match self.context.reg(src) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        let addr = match reg.inner(&self.context) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        if let Type::PtrToStack { offset: stack_offset } = *addr {
            let stack_offset = stack_offset.add_i16(offset);
            match stack_offset.check_valid_offset() {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            self.deps.add_stack(stack_offset.array_index());
        }
        self.deps.add_reg(src);
        Ok(())
    }

    fn load64(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        _src: u8,
        _lower: u32,
    ) -> Result<(), VerifierErrorKind> {
        self.deps.remove_reg(dst);
        Ok(())
    }

    fn load_from_packet(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        src: Register,
        _offset: i32,
        register_offset: Option<Register>,
        _width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        // 1 to 5 are scratch registers (bits 1..=5)
        self.deps.registers &= !0b0011_1110;
        self.deps.remove_reg(dst);
        // The types of the packet and of the offset are always checked.
        self.deps.add_reg(src);
        if let Some(reg) = register_offset {
            self.deps.add_reg(reg);
        }
        Ok(())
    }

    fn store(
        &mut self,
        _context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Source,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let reg = match self.context.reg(dst) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        let addr = match reg.inner(&self.context) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        self.deps.add_reg(dst);
        let src_is_live = if let Type::PtrToStack { offset: stack_offset } = *addr {
            let stack_offset = stack_offset.add_i16(offset);
            match stack_offset.check_valid_offset() {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            width != DataWidth::U64 || self.deps.remove_stack(stack_offset.array_index())
        } else {
            true
        };
        if src_is_live {
            if let Source::Reg(src) = src {
                self.deps.add_reg(src);
            }
        }

        Ok(())
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct TerminatingContext {
    pub computation_context: ComputationContext,
    pub dependencies: DataDependencies,
    /// Known scalar register values of `computation_context`, cached to filter comparisons.
    pub known_scalars: KnownScalars,
}

impl Clone for TerminatingContext {
    fn clone(&self) -> Self {
        Self {
            computation_context: self.computation_context.clone(),
            dependencies: self.dependencies,
            known_scalars: self.known_scalars,
        }
    }
}

/// The contexts known to terminate at a given pc. The contexts are pairwise incomparable: each
/// time a bigger context is inserted, the smaller ones are removed.
#[doc(hidden)]
#[derive(Debug)]
pub struct TerminatingContexts {
    pub contexts: Vec<TerminatingContext>,
}

impl Default for TerminatingContexts {
    fn default() -> Self {
        Self { contexts: Vec::new() }
    }
}

fn compare_terminating_entry(
    c: &TerminatingContext,
    context: &ComputationContext,
    known_scalars: &KnownScalars,
) -> (bool, bool) {
    if c.known_scalars.partial_cmp(known_scalars).is_none() {
        (true, false)
    } else {
        match c.computation_context.partial_cmp(context) {
            Some(Ordering::Less) => (false, false),
            Some(Ordering::Equal) => (true, true),
            Some(Ordering::Greater) => (true, true),
            None => (true, false),
        }
    }
}

impl TerminatingContexts {
    /// Returns the most recently inserted context that is greater than or equal to `context`.
    pub fn find_greater_or_equal(
        &self,
        context: &ComputationContext,
    ) -> Option<&TerminatingContext> {
        if self.contexts.len() == 0 {
            return None;
        }
        let known_scalars = KnownScalars::new(context);
        let mut i = self.contexts.len();
        while i > 0 {
            i -= 1;
            let c = &self.contexts[i];
            if c.known_scalars >= known_scalars && c.computation_context >= *context {
                return Some(c);
            }
        }
        None
    }

    fn find_greater_or_equal_deps(&self, context: &ComputationContext) -> Option<DataDependencies> {
        match self.find_greater_or_equal(context) {
            Some(c) => Some(c.dependencies),
            None => None,
        }
    }

    /// Inserts `context` unless a context greater than or equal to it is already present, and
    /// removes the contexts smaller than it.
    pub fn insert(&mut self, context: ComputationContext, dependencies: DataDependencies) {
        let known_scalars = KnownScalars::new(&context);
        let mut any_removed = false;
        let mut new_contexts = Vec::new();
        let mut i = 0;
        while i < self.contexts.len() {
            let (keep, dominated_here) =
                compare_terminating_entry(&self.contexts[i], &context, &known_scalars);
            if dominated_here {
                return;
            }
            if !keep {
                if !any_removed {
                    let mut j = 0;
                    while j < i {
                        new_contexts.push(self.contexts[j].clone());
                        j += 1;
                    }
                    any_removed = true;
                }
            } else if any_removed {
                new_contexts.push(self.contexts[i].clone());
            }
            i += 1;
        }
        let entry =
            TerminatingContext { computation_context: context, dependencies, known_scalars };
        if any_removed {
            new_contexts.push(entry);
            self.contexts = new_contexts;
        } else {
            self.contexts.push(entry);
        }
    }
}

/// Tracks which general registers of a `ComputationContext` are constrained to a single known
/// constant scalar.
///
/// Used as a fast pre-filter before `ComputationContext::partial_cmp`: if `c1` has a known
/// constant `v` in register `i`, then `c1 >= c2` requires `c2` to also have `v` in register `i`.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct KnownScalars {
    /// Bitmask of the registers holding a known scalar.
    pub mask: u16,
    /// The value of the registers in `mask`. The other values are 0.
    pub values: [u64; GENERAL_REGISTER_COUNT as usize],
}

impl Default for KnownScalars {
    fn default() -> Self {
        Self { mask: 0, values: [0u64; GENERAL_REGISTER_COUNT as usize] }
    }
}

impl PartialEq for KnownScalars {
    fn eq(&self, other: &Self) -> bool {
        matches!(self.partial_cmp(other), Some(Ordering::Equal))
    }
}

impl Eq for KnownScalars {}

fn known_scalar_entry(reg_type: &Type, index: usize) -> (u16, u64) {
    match reg_type {
        Type::ScalarValue(data) => {
            if data.is_known() {
                return (1u16.wrapping_shl(index as u32), data.value);
            }
            (0u16, 0u64)
        }
        _ => (0u16, 0u64),
    }
}

impl KnownScalars {
    pub fn new(context: &ComputationContext) -> Self {
        let mut result = Self::default();
        let mut i = 0usize;
        while i < (GENERAL_REGISTER_COUNT as usize) {
            let (bit, val) = known_scalar_entry(&context.registers[i], i);
            result.mask |= bit;
            result.values[i] = val;
            i += 1;
        }
        result
    }
}

impl PartialOrd for KnownScalars {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let result = match ((self.mask & !other.mask) != 0, (!self.mask & other.mask) != 0) {
            (true, true) => return None,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        };
        let common = self.mask & other.mask;
        if common == 0 {
            return Some(result);
        }
        let mut i = 0usize;
        while i < (GENERAL_REGISTER_COUNT as usize) {
            if (common & 1u16.wrapping_shl(i as u32)) != 0 && self.values[i] != other.values[i] {
                return None;
            }
            i += 1;
        }
        Some(result)
    }
}

#[derive(Clone, Copy, Debug)]
enum AluType {
    Plain,
    Sub,
    Add,
}

impl AluType {
    /// Can this operation be done one a pointer and a scalar.
    fn is_ptr_compatible(self) -> bool {
        match self {
            Self::Sub | Self::Add => true,
            Self::Plain => false,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum JumpWidth {
    W32,
    W64,
}

impl JumpWidth {
    const fn tag(self) -> u8 {
        match self {
            Self::W32 => 0,
            Self::W64 => 1,
        }
    }
}

impl PartialEq for JumpWidth {
    fn eq(&self, other: &Self) -> bool {
        self.tag() == other.tag()
    }
}

impl Eq for JumpWidth {}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum JumpType {
    Eq,
    Ge,
    Gt,
    Le,
    LooseComparaison,
    Lt,
    Ne,
    StrictComparaison,
    Unknown,
}

impl JumpType {
    const fn tag(self) -> u8 {
        match self {
            Self::Eq => 0,
            Self::Ge => 1,
            Self::Gt => 2,
            Self::Le => 3,
            Self::LooseComparaison => 4,
            Self::Lt => 5,
            Self::Ne => 6,
            Self::StrictComparaison => 7,
            Self::Unknown => 8,
        }
    }

    pub fn invert(&self) -> Self {
        match self {
            Self::Eq => Self::Ne,
            Self::Ge => Self::Lt,
            Self::Gt => Self::Le,
            Self::Le => Self::Gt,
            Self::LooseComparaison => Self::StrictComparaison,
            Self::Lt => Self::Ge,
            Self::Ne => Self::Eq,
            Self::StrictComparaison => Self::LooseComparaison,
            Self::Unknown => Self::Unknown,
        }
    }

    pub fn is_strict(&self) -> bool {
        match self {
            Self::Gt | Self::Lt | Self::Ne | Self::StrictComparaison => true,
            _ => false,
        }
    }
}

impl PartialEq for JumpType {
    fn eq(&self, other: &Self) -> bool {
        self.tag() == other.tag()
    }
}

impl Eq for JumpType {}

#[derive(Clone, Copy, Debug)]
enum ScalarAluOp {
    Add32,
    Add64,
    And32,
    And64,
    Arsh32,
    Arsh64,
    Div32,
    Div64,
    Lsh32,
    Lsh64,
    Mod32,
    Mod64,
    Mul32,
    Mul64,
    Or32,
    Or64,
    Rsh32,
    Rsh64,
    Sub32,
    Sub64,
    Xor32,
    Xor64,
    Neg32,
    Neg64,
    Xchg,
}

impl ScalarAluOp {
    fn eval(self, x: ScalarValueData, y: ScalarValueData) -> ScalarValueData {
        match self {
            Self::Add64 => x.add_scalar(y),
            Self::And64 => x.bitand_scalar(y),
            Self::Arsh64 => x.ashr(y),
            Self::Div64 => x.div_scalar(y),
            Self::Lsh64 => x.shl_scalar(y),
            Self::Mod64 => x.rem_scalar(y),
            Self::Mul64 => x.mul_scalar(y),
            Self::Or64 => x.bitor_scalar(y),
            Self::Rsh64 => x.shr_scalar(y),
            Self::Sub64 => x.sub_scalar(y),
            Self::Xor64 => x.bitxor_scalar(y),
            Self::Neg64 => x.neg_scalar(),
            Self::Add32 => U32ScalarValueData::from_u64_scalar(x)
                .add_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::And32 => U32ScalarValueData::from_u64_scalar(x)
                .bitand_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Arsh32 => U32ScalarValueData::from_u64_scalar(x)
                .ashr(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Div32 => U32ScalarValueData::from_u64_scalar(x)
                .div_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Lsh32 => U32ScalarValueData::from_u64_scalar(x)
                .shl_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Mod32 => U32ScalarValueData::from_u64_scalar(x)
                .rem_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Mul32 => U32ScalarValueData::from_u64_scalar(x)
                .mul_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Or32 => U32ScalarValueData::from_u64_scalar(x)
                .bitor_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Rsh32 => U32ScalarValueData::from_u64_scalar(x)
                .shr_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Sub32 => U32ScalarValueData::from_u64_scalar(x)
                .sub_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Xor32 => U32ScalarValueData::from_u64_scalar(x)
                .bitxor_scalar(U32ScalarValueData::from_u64_scalar(y))
                .into_u64_scalar(),
            Self::Neg32 => U32ScalarValueData::from_u64_scalar(x).neg_scalar().into_u64_scalar(),
            Self::Xchg => y,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BranchCondOp {
    Eq32,
    Eq64,
    Ne32,
    Ne64,
    Ge32,
    Ge64,
    Gt32,
    Gt64,
    Le32,
    Le64,
    Lt32,
    Lt64,
    Sge32,
    Sge64,
    Sgt32,
    Sgt64,
    Sle32,
    Sle64,
    Slt32,
    Slt64,
    Set32,
    Set64,
}

#[doc(hidden)]
pub fn comp_eq64(x: ScalarValueData, y: ScalarValueData) -> Result<Option<bool>, ()> {
    if !x.is_fully_initialized() || !y.is_fully_initialized() {
        return Err(());
    }
    if x.min() == x.max() && x.min() == y.min() && x.min() == y.max() {
        return Ok(Some(true));
    }
    if !x.can_be_equal(y) {
        return Ok(Some(false));
    }
    Ok(None)
}

#[doc(hidden)]
pub fn comp_eq32(x: ScalarValueData, y: ScalarValueData) -> Result<Option<bool>, ()> {
    comp_eq64(
        U32ScalarValueData::from_u64_scalar(x).into_u64_scalar(),
        U32ScalarValueData::from_u64_scalar(y).into_u64_scalar(),
    )
}

impl BranchCondOp {
    fn eval(self, x: ScalarValueData, y: ScalarValueData) -> Result<Option<bool>, ()> {
        match self {
            Self::Eq64 => comp_eq64(x, y),
            Self::Eq32 => comp_eq32(x, y),
            Self::Ne64 => match comp_eq64(x, y) {
                Ok(Some(eq)) => Ok(Some(!eq)),
                Ok(None) => Ok(None),
                Err(()) => Err(()),
            },
            Self::Ne32 => match comp_eq32(x, y) {
                Ok(Some(eq)) => Ok(Some(!eq)),
                Ok(None) => Ok(None),
                Err(()) => Err(()),
            },
            Self::Ge64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if x.urange.min >= y.urange.max {
                    Ok(Some(true))
                } else if y.urange.min > x.urange.max {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Ge32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                let xr: U32Range = x32.urange;
                let yr: U32Range = y32.urange;
                if xr.min >= yr.max {
                    Ok(Some(true))
                } else if yr.min > xr.max {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Gt64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if x.urange.min > y.urange.max {
                    Ok(Some(true))
                } else if y.urange.min >= x.urange.max {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Gt32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                let xr: U32Range = x32.urange;
                let yr: U32Range = y32.urange;
                if xr.min > yr.max {
                    Ok(Some(true))
                } else if yr.min >= xr.max {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Le64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if x.urange.max <= y.urange.min {
                    Ok(Some(true))
                } else if y.urange.max < x.urange.min {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Le32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                let xr: U32Range = x32.urange;
                let yr: U32Range = y32.urange;
                if xr.max <= yr.min {
                    Ok(Some(true))
                } else if yr.max < xr.min {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Lt64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if x.urange.max < y.urange.min {
                    Ok(Some(true))
                } else if y.urange.max <= x.urange.min {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Lt32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                let xr: U32Range = x32.urange;
                let yr: U32Range = y32.urange;
                if xr.max < yr.min {
                    Ok(Some(true))
                } else if yr.max <= xr.min {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
            Self::Sge64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if !x.is_known() || !y.is_known() {
                    return Ok(None);
                }
                Ok(Some((x.value as i64) >= (y.value as i64)))
            }
            Self::Sge32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                if !x32.is_known() || !y32.is_known() {
                    return Ok(None);
                }
                Ok(Some((x32.value as i32) >= (y32.value as i32)))
            }
            Self::Sgt64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if !x.is_known() || !y.is_known() {
                    return Ok(None);
                }
                Ok(Some((x.value as i64) > (y.value as i64)))
            }
            Self::Sgt32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                if !x32.is_known() || !y32.is_known() {
                    return Ok(None);
                }
                Ok(Some((x32.value as i32) > (y32.value as i32)))
            }
            Self::Sle64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if !x.is_known() || !y.is_known() {
                    return Ok(None);
                }
                Ok(Some((x.value as i64) <= (y.value as i64)))
            }
            Self::Sle32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                if !x32.is_known() || !y32.is_known() {
                    return Ok(None);
                }
                Ok(Some((x32.value as i32) <= (y32.value as i32)))
            }
            Self::Slt64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if !x.is_known() || !y.is_known() {
                    return Ok(None);
                }
                Ok(Some((x.value as i64) < (y.value as i64)))
            }
            Self::Slt32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                if !x32.is_known() || !y32.is_known() {
                    return Ok(None);
                }
                Ok(Some((x32.value as i32) < (y32.value as i32)))
            }
            Self::Set64 => {
                if !x.is_fully_initialized() || !y.is_fully_initialized() {
                    return Err(());
                }
                if x.urange.min != x.urange.max || y.urange.min != y.urange.max {
                    return Ok(None);
                }
                Ok(Some((x.urange.min & y.urange.min) != 0))
            }
            Self::Set32 => {
                let x32 = U32ScalarValueData::from_u64_scalar(x);
                let y32 = U32ScalarValueData::from_u64_scalar(y);
                if !x32.is_fully_initialized() || !y32.is_fully_initialized() {
                    return Err(());
                }
                let xr: U32Range = x32.urange;
                let yr: U32Range = y32.urange;
                if xr.min != xr.max || yr.min != yr.max {
                    return Ok(None);
                }
                Ok(Some((xr.min & yr.min) != 0))
            }
        }
    }
}

/// Visiting an instruction pushes the successors of `self` to `VerificationContext::pending_successors`.
impl BpfVisitor<VerificationContext> for ComputationContext {
    fn add(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Add32), context, dst, src, AluType::Plain, ScalarAluOp::Add32)
    }
    fn add64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Add64), context, dst, src, AluType::Add, ScalarAluOp::Add64)
    }
    fn and(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::And32), context, dst, src, AluType::Plain, ScalarAluOp::And32)
    }
    fn and64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::And64), context, dst, src, AluType::Plain, ScalarAluOp::And64)
    }
    fn arsh(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Arsh32), context, dst, src, AluType::Plain, ScalarAluOp::Arsh32)
    }
    fn arsh64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Arsh64), context, dst, src, AluType::Plain, ScalarAluOp::Arsh64)
    }
    fn div(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Div32), context, dst, src, AluType::Plain, ScalarAluOp::Div32)
    }
    fn div64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Div64), context, dst, src, AluType::Plain, ScalarAluOp::Div64)
    }
    fn lsh(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Lsh32), context, dst, src, AluType::Plain, ScalarAluOp::Lsh32)
    }
    fn lsh64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Lsh64), context, dst, src, AluType::Plain, ScalarAluOp::Lsh64)
    }
    fn r#mod(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Mod32), context, dst, src, AluType::Plain, ScalarAluOp::Mod32)
    }
    fn mod64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Mod64), context, dst, src, AluType::Plain, ScalarAluOp::Mod64)
    }
    fn mov(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Alu { op: AluLogOp::Mov32, dst, src });
        let src_val = match self.compute_source(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let value = match src_val {
            Type::ScalarValue(data) => {
                let v = (data.value as u32) as u64;
                let unknown_mask = (data.unknown_mask as u32) as u64;
                let unwritten_mask = (data.unwritten_mask as u32) as u64;
                let urange = U64Range::compute_range_for_bytes_swap(
                    U64Range::from_scalar(0),
                    data.urange,
                    0,
                    0,
                    4,
                );
                Type::ScalarValue(ScalarValueData::new(v, unknown_mask, unwritten_mask, urange))
            }
            _ => Type::UNINITIALIZED,
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, value) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        context.push_successor(next);
        Ok(())
    }
    fn mov64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Alu { op: AluLogOp::Mov64, dst, src });
        let src_val = match self.compute_source(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, src_val) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        context.push_successor(next);
        Ok(())
    }
    fn mul(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Mul32), context, dst, src, AluType::Plain, ScalarAluOp::Mul32)
    }
    fn mul64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Mul64), context, dst, src, AluType::Plain, ScalarAluOp::Mul64)
    }
    fn or(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Or32), context, dst, src, AluType::Plain, ScalarAluOp::Or32)
    }
    fn or64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Or64), context, dst, src, AluType::Plain, ScalarAluOp::Or64)
    }
    fn rsh(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Rsh32), context, dst, src, AluType::Plain, ScalarAluOp::Rsh32)
    }
    fn rsh64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Rsh64), context, dst, src, AluType::Plain, ScalarAluOp::Rsh64)
    }
    fn sub(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Sub32), context, dst, src, AluType::Plain, ScalarAluOp::Sub32)
    }
    fn sub64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Sub64), context, dst, src, AluType::Sub, ScalarAluOp::Sub64)
    }
    fn xor(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Xor32), context, dst, src, AluType::Plain, ScalarAluOp::Xor32)
    }
    fn xor64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        self.alu(Some(AluLogOp::Xor64), context, dst, src, AluType::Plain, ScalarAluOp::Xor64)
    }

    fn neg(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Neg { is_32: true, dst });
        self.alu(None, context, dst, Source::Value(0), AluType::Plain, ScalarAluOp::Neg32)
    }
    fn neg64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Neg { is_32: false, dst });
        self.alu(None, context, dst, Source::Value(0), AluType::Plain, ScalarAluOp::Neg64)
    }

    fn be(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        self.endianness(true, context, dst, width)
    }
    fn le(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        self.endianness(false, context, dst, width)
    }

    fn call_external(
        &mut self,
        context: &mut VerificationContext,
        index: u32,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Call { index });
        let Some(signature) = context.calling_context.find_helper(index) else {
            return Err(VerifierErrorKind::UnknownHelper(index));
        };
        if signature.args.len() > (ARGUMENT_COUNT as usize) {
            return Err(VerifierErrorKind::InvalidRegister(ARGUMENT_COUNT + 1));
        }
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        let mut arg_index = 0usize;
        while arg_index < signature.args.len() {
            let reg = (arg_index + 1) as u8;
            let arg_val = match self.reg(reg) {
                Ok(v) => v,
                Err(err) => return Err(err),
            };
            match arg_val.match_parameter_type(
                context,
                self,
                &signature.args[arg_index],
                arg_index,
                &mut next,
            ) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
            arg_index += 1;
        }
        let return_value = signature.return_value.clone();
        let invalidate_array_bounds = signature.invalidate_array_bounds;
        if invalidate_array_bounds {
            next.array_bounds.clear();
        }
        let value = match self.resolve_return_value(context, &return_value, &mut next) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        match next.set_reg(0, value) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        next.registers[1] = Type::UNINITIALIZED;
        next.registers[2] = Type::UNINITIALIZED;
        next.registers[3] = Type::UNINITIALIZED;
        next.registers[4] = Type::UNINITIALIZED;
        next.registers[5] = Type::UNINITIALIZED;
        context.push_successor(next);
        Ok(())
    }

    fn exit(&mut self, context: &mut VerificationContext) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Exit);
        let r0 = match self.reg(0) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        if !r0.is_written_scalar() {
            return Err(VerifierErrorKind::InvalidReturnValue);
        }
        if self.resources.len() != 0 {
            return Err(VerifierErrorKind::ResourceLeak);
        }
        Ok(())
    }

    fn jump(
        &mut self,
        context: &mut VerificationContext,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Jump { offset });
        let next = match self.checked_jump_with_offset(context, offset) {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        context.push_successor(next);
        Ok(())
    }

    fn jeq(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jeq32,
            context,
            dst,
            src,
            offset,
            JumpType::Eq,
            JumpWidth::W32,
            BranchCondOp::Eq32,
        )
    }
    fn jeq64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jeq64,
            context,
            dst,
            src,
            offset,
            JumpType::Eq,
            JumpWidth::W64,
            BranchCondOp::Eq64,
        )
    }
    fn jne(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jne32,
            context,
            dst,
            src,
            offset,
            JumpType::Ne,
            JumpWidth::W32,
            BranchCondOp::Ne32,
        )
    }
    fn jne64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jne64,
            context,
            dst,
            src,
            offset,
            JumpType::Ne,
            JumpWidth::W64,
            BranchCondOp::Ne64,
        )
    }
    fn jge(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jge32,
            context,
            dst,
            src,
            offset,
            JumpType::Ge,
            JumpWidth::W32,
            BranchCondOp::Ge32,
        )
    }
    fn jge64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jge64,
            context,
            dst,
            src,
            offset,
            JumpType::Ge,
            JumpWidth::W64,
            BranchCondOp::Ge64,
        )
    }
    fn jgt(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jgt32,
            context,
            dst,
            src,
            offset,
            JumpType::Gt,
            JumpWidth::W32,
            BranchCondOp::Gt32,
        )
    }
    fn jgt64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jgt64,
            context,
            dst,
            src,
            offset,
            JumpType::Gt,
            JumpWidth::W64,
            BranchCondOp::Gt64,
        )
    }
    fn jle(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jle32,
            context,
            dst,
            src,
            offset,
            JumpType::Le,
            JumpWidth::W32,
            BranchCondOp::Le32,
        )
    }
    fn jle64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jle64,
            context,
            dst,
            src,
            offset,
            JumpType::Le,
            JumpWidth::W64,
            BranchCondOp::Le64,
        )
    }
    fn jlt(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jlt32,
            context,
            dst,
            src,
            offset,
            JumpType::Lt,
            JumpWidth::W32,
            BranchCondOp::Lt32,
        )
    }
    fn jlt64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jlt64,
            context,
            dst,
            src,
            offset,
            JumpType::Lt,
            JumpWidth::W64,
            BranchCondOp::Lt64,
        )
    }
    fn jsge(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsge32,
            context,
            dst,
            src,
            offset,
            JumpType::LooseComparaison,
            JumpWidth::W32,
            BranchCondOp::Sge32,
        )
    }
    fn jsge64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsge64,
            context,
            dst,
            src,
            offset,
            JumpType::LooseComparaison,
            JumpWidth::W64,
            BranchCondOp::Sge64,
        )
    }
    fn jsgt(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsgt32,
            context,
            dst,
            src,
            offset,
            JumpType::StrictComparaison,
            JumpWidth::W32,
            BranchCondOp::Sgt32,
        )
    }
    fn jsgt64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsgt64,
            context,
            dst,
            src,
            offset,
            JumpType::StrictComparaison,
            JumpWidth::W64,
            BranchCondOp::Sgt64,
        )
    }
    fn jsle(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsle32,
            context,
            dst,
            src,
            offset,
            JumpType::LooseComparaison,
            JumpWidth::W32,
            BranchCondOp::Sle32,
        )
    }
    fn jsle64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jsle64,
            context,
            dst,
            src,
            offset,
            JumpType::LooseComparaison,
            JumpWidth::W64,
            BranchCondOp::Sle64,
        )
    }
    fn jslt(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jslt32,
            context,
            dst,
            src,
            offset,
            JumpType::StrictComparaison,
            JumpWidth::W32,
            BranchCondOp::Slt32,
        )
    }
    fn jslt64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jslt64,
            context,
            dst,
            src,
            offset,
            JumpType::StrictComparaison,
            JumpWidth::W64,
            BranchCondOp::Slt64,
        )
    }
    fn jset(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jset32,
            context,
            dst,
            src,
            offset,
            JumpType::Unknown,
            JumpWidth::W32,
            BranchCondOp::Set32,
        )
    }
    fn jset64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        self.conditional_jump(
            JmpLogOp::Jset64,
            context,
            dst,
            src,
            offset,
            JumpType::Unknown,
            JumpWidth::W64,
            BranchCondOp::Set64,
        )
    }
    fn atomic_add(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Add32,
            context,
            DataWidth::U32,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Add32,
        )
    }

    fn atomic_add64(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Add64,
            context,
            DataWidth::U64,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Add64,
        )
    }

    fn atomic_and(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::And32,
            context,
            DataWidth::U32,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::And32,
        )
    }

    fn atomic_and64(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::And64,
            context,
            DataWidth::U64,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::And64,
        )
    }

    fn atomic_or(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Or32,
            context,
            DataWidth::U32,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Or32,
        )
    }

    fn atomic_or64(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Or64,
            context,
            DataWidth::U64,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Or64,
        )
    }

    fn atomic_xor(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Xor32,
            context,
            DataWidth::U32,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Xor32,
        )
    }

    fn atomic_xor64(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Xor64,
            context,
            DataWidth::U64,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Xor64,
        )
    }

    fn atomic_xchg(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Xchg32,
            context,
            DataWidth::U32,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Xchg,
        )
    }

    fn atomic_xchg64(
        &mut self,
        context: &mut VerificationContext,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_operation(
            AtomicLogOp::Xchg64,
            context,
            DataWidth::U64,
            fetch,
            dst,
            offset,
            src,
            ScalarAluOp::Xchg,
        )
    }

    fn atomic_cmpxchg(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.raw_atomic_cmpxchg(
            AtomicLogOp::Cmpxchg32,
            context,
            dst,
            offset,
            src,
            JumpWidth::W32,
            BranchCondOp::Eq32,
        )
    }
    fn atomic_cmpxchg64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.raw_atomic_cmpxchg(
            AtomicLogOp::Cmpxchg64,
            context,
            dst,
            offset,
            src,
            JumpWidth::W64,
            BranchCondOp::Eq64,
        )
    }

    fn load(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        context.emit_log(self.pc, VerifierLogEvent::Load { width, dst, src, offset });
        let addr = match self.reg(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let loaded_type = match self.load_memory(context, &addr, Field::new(offset, width)) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, loaded_type) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        context.push_successor(next);
        Ok(())
    }

    fn load64(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: u8,
        lower: u32,
    ) -> Result<(), VerifierErrorKind> {
        let Some(next_pc) = self.pc.checked_add(1) else {
            return Err(VerifierErrorKind::IncompleteLddw);
        };
        if next_pc >= context.code.len() {
            return Err(VerifierErrorKind::IncompleteLddw);
        }
        let next_imm = context.code[next_pc].imm();
        let resolved = match context.calling_context.resolve_lddw(src, lower as i32, next_imm) {
            Ok(r) => r,
            Err(err) => return Err(err),
        };
        let value = match resolved {
            ResolvedLddw::Imm(val) => {
                context.emit_log(self.pc, VerifierLogEvent::LddwImm { dst, value: val });
                Type::ScalarValue(ScalarValueData::from_known(val))
            }
            ResolvedLddw::MapPtr { map_index, schema } => {
                context.emit_log(self.pc, VerifierLogEvent::LddwMapByIndex { dst, map_index });
                Type::ConstPtrToMap { id: map_index as u64, schema }
            }
            ResolvedLddw::MapValuePtr { map_index, schema, offset } => {
                context.emit_log(
                    self.pc,
                    VerifierLogEvent::LddwMapValueByIndex { dst, map_index, offset },
                );
                let id = context.next_id();
                let origin = if schema.flags.contains(MapFlags::ProgReadOnly) {
                    PointerOrigin::MapReadOnly
                } else {
                    PointerOrigin::MapWritable
                };
                Type::PtrToMemory {
                    id,
                    offset: ScalarValueData::from_known(offset as u64),
                    buffer_size: schema.value_size as u64,
                    origin,
                }
            }
        };
        let mut next = match self.jump_with_offset(1) {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, value) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        context.push_successor(next);
        Ok(())
    }

    fn load_from_packet(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        src: Register,
        offset: i32,
        register_offset: Option<Register>,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        context
            .emit_log(self.pc, VerifierLogEvent::LoadFromPacket { width, offset, register_offset });
        let src_type = match self.reg(src) {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        let src_is_packet = match &context.calling_context.packet_type {
            Some(packet_type) => src_type == *packet_type,
            None => false,
        };
        if !src_is_packet {
            return Err(VerifierErrorKind::RegisterNotPacket(src));
        }
        if let Some(reg) = register_offset {
            let reg_val = match self.reg(reg) {
                Ok(v) => v,
                Err(err) => return Err(err),
            };
            if !reg_val.is_written_scalar() {
                return Err(VerifierErrorKind::UninitializedPacketOffset);
            }
        }
        // Handle the case where the load fails. Out-of-bounds packet loads terminate the
        // program and return `0` directly without reading `r0`, so only unreleased
        // resources need to be checked here.
        if self.resources.len() != 0 {
            return Err(VerifierErrorKind::ResourceLeak);
        }
        // `self` is not terminated here: it becomes the parent of `next`, and terminates with the
        // dependencies of both paths once `next` terminates. Terminating `self` here would record
        // it as a terminating context with the dependencies of the exit path only.

        // Handle the case where the load succeeds.
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        match next.set_reg(dst, Type::UNKNOWN_SCALAR) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        next.registers[1] = Type::UNINITIALIZED;
        next.registers[2] = Type::UNINITIALIZED;
        next.registers[3] = Type::UNINITIALIZED;
        next.registers[4] = Type::UNINITIALIZED;
        next.registers[5] = Type::UNINITIALIZED;
        context.push_successor(next);
        Ok(())
    }

    fn store(
        &mut self,
        context: &mut VerificationContext,
        dst: Register,
        offset: i16,
        src: Source,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let value = match src {
            Source::Reg(r) => {
                context
                    .emit_log(self.pc, VerifierLogEvent::StoreReg { width, dst, offset, src: r });
                match self.reg(r) {
                    Ok(v) => v,
                    Err(err) => return Err(err),
                }
            }
            Source::Value(v) => {
                context
                    .emit_log(self.pc, VerifierLogEvent::StoreImm { width, dst, offset, value: v });
                Type::ScalarValue(ScalarValueData::from_known(v & Type::mask(width)))
            }
        };
        let mut next = match self.next() {
            Ok(n) => n,
            Err(err) => return Err(err),
        };
        let addr = match self.reg(dst) {
            Ok(a) => a,
            Err(err) => return Err(err),
        };
        match next.store_memory(context, self.pc, &addr, Field::new(offset, width), value) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
        context.push_successor(next);
        Ok(())
    }
}
