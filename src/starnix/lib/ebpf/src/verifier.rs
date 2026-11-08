// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::verifier::{
    FieldDescriptor, FieldType, FunctionSignature, FunctionSignatureBuilder, MAX_PACKET_OFFSET,
    MapTypeFilter, MemoryId, MemoryParameterSize, PointerOrigin, StructAccess, StructDescriptor,
    Type, VerifiedEbpfProgram,
};

use crate::{EbpfInstruction, MapSchema, VerifierError};
use ebpf_core::verifier::{
    AluLogOp, AtomicLogOp, CoreCallingContext, CoreVerifierLogger, JmpLogOp, VerifierLogEvent,
    verify_program_core,
};
use ebpf_core::visitor::{ProgramCounter, Register, Source};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// A counter that allows to generate new ids for parameters. The namespace is the same as for id
/// generated for types while verifying an ebpf program. Because different sources of identifiers
/// use different namespaces, this counter can start at 0.
static BPF_TYPE_IDENTIFIER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh `MemoryId` in the `Generated` namespace.
pub fn new_memory_id() -> MemoryId {
    MemoryId::from_generated(BPF_TYPE_IDENTIFIER_COUNTER.fetch_add(1, AtomicOrdering::Relaxed))
}

/// A trait to receive the log from the verifier.
pub trait VerifierLogger {
    /// Returns whether logging is currently enabled and can accept output.
    ///
    /// Verifiers check this before evaluating log messages or formatting strings.
    fn is_enabled(&self) -> bool {
        true
    }

    /// Log a line. The line is always a correct encoded ASCII string.
    fn log(&mut self, line: &[u8]);
}

/// A `VerifierLogger` that drops all its content.
pub struct NullVerifierLogger;

impl VerifierLogger for NullVerifierLogger {
    fn is_enabled(&self) -> bool {
        false
    }

    fn log(&mut self, line: &[u8]) {
        debug_assert!(line.is_ascii());
    }
}

#[derive(Clone, Debug)]
pub struct HelperDefinition {
    pub index: u32,
    pub name: &'static str,
    pub signature: FunctionSignature,
}

#[derive(Debug, Default)]
pub struct CallingContext {
    /// List of map schemas of the associated map. The maps can be accessed using LDDW instruction
    /// with `src_reg=BPF_PSEUDO_MAP_IDX`.
    pub maps: Vec<MapSchema>,
    /// The registered external functions.
    pub helpers: HashMap<u32, &'static HelperDefinition>,
    /// The args of the program.
    pub args: Vec<Type>,
    /// Packet type. Normally it should be either `None` or `args[0]`.
    pub packet_type: Option<Type>,
    /// Whether pointer values may be coerced to scalars (`CAP_PERFMON` or `CAP_SYS_ADMIN`).
    pub allow_ptr_leaks: bool,
}

impl CallingContext {
    pub fn register_map(&mut self, schema: MapSchema) -> usize {
        let index = self.maps.len();
        self.maps.push(schema);
        index
    }
}

/// Verify the given code depending on the type of the parameters and the registered external
/// functions. Returned `VerifiedEbpfProgram` should be linked in order to execute it.
///
/// Note that `code` may be modified during verification, in particular by replacing unreachable
/// instructions with `ja -1` so dead branches are ignored during linking.
pub fn verify_program(
    code: Vec<EbpfInstruction>,
    calling_context: CallingContext,
    logger: &mut dyn VerifierLogger,
) -> Result<VerifiedEbpfProgram, VerifierError> {
    let CallingContext { maps, helpers, args, packet_type, allow_ptr_leaks } = calling_context;
    let mut sorted_helpers: Vec<(u32, FunctionSignature)> =
        helpers.into_iter().map(|(id, def)| (id, def.signature.clone())).collect();
    sorted_helpers.sort_unstable_by_key(|(id, _)| *id);

    let core_ctx =
        CoreCallingContext { maps, helpers: sorted_helpers, args, packet_type, allow_ptr_leaks };
    let mut adapter = VerifierLoggerAdapter { inner: logger };
    verify_program_core(code, core_ctx, &mut adapter)
}

#[derive(Clone, Copy)]
struct DisplayRegister(Register);

impl std::fmt::Display for DisplayRegister {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "%r{}", self.0)
    }
}

fn display_register(register: Register) -> DisplayRegister {
    DisplayRegister(register)
}

#[derive(Clone, Copy)]
struct DisplaySource(Source);

impl std::fmt::Display for DisplaySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Source::Reg(r) => display_register(r).fmt(f),
            Source::Value(v) => write!(f, "0x{v:x}"),
        }
    }
}

fn display_source(src: Source) -> DisplaySource {
    DisplaySource(src)
}

#[derive(Clone, Copy)]
struct DisplayOffset {
    offset: i32,
    show_zero: bool,
}

impl std::fmt::Display for DisplayOffset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.offset > 0 {
            write!(f, "+{}", self.offset)
        } else if self.offset < 0 || self.show_zero {
            write!(f, "{}", self.offset)
        } else {
            Ok(())
        }
    }
}

fn print_offset<T: Into<i32>>(offset: T) -> DisplayOffset {
    DisplayOffset { offset: offset.into(), show_zero: false }
}

fn print_jump_offset<T: Into<i32>>(offset: T) -> DisplayOffset {
    DisplayOffset { offset: offset.into(), show_zero: true }
}

fn alu_op_str(op: AluLogOp) -> &'static str {
    match op {
        AluLogOp::Add32 => "add32",
        AluLogOp::Add64 => "add",
        AluLogOp::And32 => "and32",
        AluLogOp::And64 => "and",
        AluLogOp::Arsh32 => "arsh32",
        AluLogOp::Arsh64 => "arsh",
        AluLogOp::Div32 => "div32",
        AluLogOp::Div64 => "div",
        AluLogOp::Lsh32 => "lsh32",
        AluLogOp::Lsh64 => "lsh",
        AluLogOp::Mod32 => "mod32",
        AluLogOp::Mod64 => "mod",
        AluLogOp::Mov32 => "mov32",
        AluLogOp::Mov64 => "mov",
        AluLogOp::Mul32 => "mul32",
        AluLogOp::Mul64 => "mul",
        AluLogOp::Or32 => "or32",
        AluLogOp::Or64 => "or",
        AluLogOp::Rsh32 => "rsh32",
        AluLogOp::Rsh64 => "rsh",
        AluLogOp::Sub32 => "sub32",
        AluLogOp::Sub64 => "sub",
        AluLogOp::Xor32 => "xor32",
        AluLogOp::Xor64 => "xor",
    }
}

fn jmp_op_str(op: JmpLogOp) -> &'static str {
    match op {
        JmpLogOp::Jeq32 => "jeq32",
        JmpLogOp::Jeq64 => "jeq",
        JmpLogOp::Jne32 => "jne32",
        JmpLogOp::Jne64 => "jne",
        JmpLogOp::Jge32 => "jge32",
        JmpLogOp::Jge64 => "jge",
        JmpLogOp::Jgt32 => "jgt32",
        JmpLogOp::Jgt64 => "jgt",
        JmpLogOp::Jle32 => "jle32",
        JmpLogOp::Jle64 => "jle",
        JmpLogOp::Jlt32 => "jlt32",
        JmpLogOp::Jlt64 => "jlt",
        JmpLogOp::Jsge32 => "jsge32",
        JmpLogOp::Jsge64 => "jsge",
        JmpLogOp::Jsgt32 => "jsgt32",
        JmpLogOp::Jsgt64 => "jsgt",
        JmpLogOp::Jsle32 => "jsle32",
        JmpLogOp::Jsle64 => "jsle",
        JmpLogOp::Jslt32 => "jslt32",
        JmpLogOp::Jslt64 => "jslt",
        JmpLogOp::Jset32 => "jset32",
        JmpLogOp::Jset64 => "jset",
    }
}

fn atomic_op_str(op: AtomicLogOp) -> &'static str {
    match op {
        AtomicLogOp::Add32 => "add32",
        AtomicLogOp::Add64 => "add",
        AtomicLogOp::And32 => "and32",
        AtomicLogOp::And64 => "and",
        AtomicLogOp::Or32 => "or32",
        AtomicLogOp::Or64 => "or",
        AtomicLogOp::Xor32 => "xor32",
        AtomicLogOp::Xor64 => "xor",
        AtomicLogOp::Xchg32 => "xchg32",
        AtomicLogOp::Xchg64 => "xchg",
        AtomicLogOp::Cmpxchg32 => "cmpxchg32",
        AtomicLogOp::Cmpxchg64 => "cmpxchg",
    }
}

struct VerifierLoggerAdapter<'a> {
    inner: &'a mut dyn VerifierLogger,
}

impl CoreVerifierLogger for VerifierLoggerAdapter<'_> {
    fn is_enabled(&self) -> bool {
        self.inner.is_enabled()
    }

    fn log_instruction(&mut self, pc: ProgramCounter, opcode: u8, event: VerifierLogEvent) {
        if !self.inner.is_enabled() {
            return;
        }
        let body = match event {
            VerifierLogEvent::Alu { op, dst, src } => {
                format!("{} {}, {}", alu_op_str(op), display_register(dst), display_source(src))
            }
            VerifierLogEvent::Neg { is_32, dst } => {
                format!("{} {}", if is_32 { "neg32" } else { "neg" }, display_register(dst))
            }
            VerifierLogEvent::Endian { to_be, width, dst } => {
                format!(
                    "{}{} {}",
                    if to_be { "be" } else { "le" },
                    width.bits(),
                    display_register(dst)
                )
            }
            VerifierLogEvent::Call { index } => format!("call 0x{index:x}"),
            VerifierLogEvent::Exit => "exit".to_string(),
            VerifierLogEvent::Jump { offset } => format!("ja {offset}"),
            VerifierLogEvent::CondJump { op, dst, src, offset } => {
                format!(
                    "{} {}, {}, {}",
                    jmp_op_str(op),
                    display_register(dst),
                    display_source(src),
                    print_jump_offset(offset)
                )
            }
            VerifierLogEvent::Atomic { op, fetch, dst, offset, src } => {
                format!(
                    "lock {}{} [{}{}], {}",
                    if fetch { "fetch " } else { "" },
                    atomic_op_str(op),
                    display_register(dst),
                    print_offset(offset),
                    display_register(src)
                )
            }
            VerifierLogEvent::Load { width, dst, src, offset } => {
                format!(
                    "ldx{} {}, [{}{}]",
                    width.str(),
                    display_register(dst),
                    display_register(src),
                    print_offset(offset)
                )
            }
            VerifierLogEvent::LddwImm { dst, value } => {
                format!("lddw {}, 0x{value:x}", display_register(dst))
            }
            VerifierLogEvent::LddwMapByIndex { dst, map_index } => {
                format!("lddw {}, map_by_index({map_index:x})", display_register(dst))
            }
            VerifierLogEvent::LddwMapValueByIndex { dst, map_index, offset } => {
                format!(
                    "lddw {}, map_value_by_index({map_index:x})+{offset}",
                    display_register(dst)
                )
            }
            VerifierLogEvent::LoadFromPacket { width, offset, register_offset } => {
                match register_offset {
                    Some(reg) => {
                        format!(
                            "ldp{} {}{}",
                            width.str(),
                            display_register(reg),
                            print_offset(offset)
                        )
                    }
                    None => format!("ldp{} {}", width.str(), print_offset(offset)),
                }
            }
            VerifierLogEvent::StoreReg { width, dst, offset, src } => {
                format!(
                    "stx{} [{}{}], {}",
                    width.str(),
                    display_register(dst),
                    print_offset(offset),
                    display_register(src)
                )
            }
            VerifierLogEvent::StoreImm { width, dst, offset, value } => {
                format!(
                    "st{} [{}{}], 0x{value:x}",
                    width.str(),
                    display_register(dst),
                    print_offset(offset)
                )
            }
        };
        let line = format!("{pc}: ({opcode:02x}) {body}");
        self.inner.log(line.as_bytes());
    }

    fn log_error(&mut self, error: &VerifierError) {
        self.inner.log(error.to_string().as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ARGUMENT_COUNT, BPF_ADD, BPF_ALU64, BPF_CALL, BPF_DW, BPF_EXIT, BPF_JA, BPF_JEQ, BPF_JMP,
        BPF_K, BPF_LDDW, BPF_LDX, BPF_MAP_TYPE_ARRAY, BPF_MEM, BPF_MOV, BPF_PSEUDO_MAP_IDX_VALUE,
        BPF_ST, BPF_STX, BPF_SUB, BPF_W, BPF_X, DataWidth, MapFlags, VerifierErrorKind,
    };
    use core::cmp::Ordering;
    use ebpf_core::scalar_value::{ScalarValueData, U64Range};
    use ebpf_core::verifier::{
        BoundsMap, ComputationContext, DataDependencies, ExplorerArena, ExplorerNode, Field, IdSet,
        JumpType, JumpWidth, KnownScalars, Namespace, NullCoreVerifierLogger, Stack, StackOffset,
        TerminatingContexts, VerificationContext, Worklist, WorklistEntry, comp_eq32, comp_eq64,
    };
    use linux_uapi::{EACCES, EINVAL};
    use std::collections::BTreeSet;
    use test_util::{assert_geq, assert_leq};

    #[test]
    fn test_type_ordering() {
        let t0 = Type::from(0);
        let t1 = Type::from(1);
        let random = Type::AnyParameter;
        let unknown_written = Type::UNKNOWN_SCALAR;
        let unwritten = Type::default();

        assert_eq!(t0.partial_cmp(&t0), Some(Ordering::Equal));
        assert_eq!(t0.partial_cmp(&t1), None);
        assert_eq!(t0.partial_cmp(&random), None);
        assert_eq!(t0.partial_cmp(&unknown_written), Some(Ordering::Less));
        assert_eq!(t0.partial_cmp(&unwritten), Some(Ordering::Less));

        assert_eq!(t1.partial_cmp(&t0), None);
        assert_eq!(t1.partial_cmp(&t1), Some(Ordering::Equal));
        assert_eq!(t1.partial_cmp(&random), None);
        assert_eq!(t1.partial_cmp(&unknown_written), Some(Ordering::Less));
        assert_eq!(t1.partial_cmp(&unwritten), Some(Ordering::Less));

        assert_eq!(random.partial_cmp(&t0), None);
        assert_eq!(random.partial_cmp(&t1), None);
        assert_eq!(random.partial_cmp(&random), Some(Ordering::Equal));
        assert_eq!(random.partial_cmp(&unknown_written), None);
        assert_eq!(random.partial_cmp(&unwritten), Some(Ordering::Less));

        assert_eq!(unknown_written.partial_cmp(&t0), Some(Ordering::Greater));
        assert_eq!(unknown_written.partial_cmp(&t1), Some(Ordering::Greater));
        assert_eq!(unknown_written.partial_cmp(&random), None);
        assert_eq!(unknown_written.partial_cmp(&unknown_written), Some(Ordering::Equal));
        assert_eq!(unknown_written.partial_cmp(&unwritten), Some(Ordering::Less));

        assert_eq!(unwritten.partial_cmp(&t0), Some(Ordering::Greater));
        assert_eq!(unwritten.partial_cmp(&t1), Some(Ordering::Greater));
        assert_eq!(unwritten.partial_cmp(&random), Some(Ordering::Greater));
        assert_eq!(unwritten.partial_cmp(&unknown_written), Some(Ordering::Greater));
        assert_eq!(unwritten.partial_cmp(&unwritten), Some(Ordering::Equal));
    }

    #[test]
    fn test_stack_ordering() {
        let mut s1 = Stack::default();
        let mut s2 = Stack::default();

        assert_eq!(s1.partial_cmp(&s2), Some(Ordering::Equal));
        s1.set(0, 0.into());
        assert_eq!(s1.partial_cmp(&s1.clone()), Some(Ordering::Equal));
        assert_eq!(s1.partial_cmp(&s2), Some(Ordering::Less));
        assert_eq!(s2.partial_cmp(&s1), Some(Ordering::Greater));
        s2.set(0, Type::UNKNOWN_SCALAR);
        assert_eq!(s1.partial_cmp(&s2), Some(Ordering::Less));
        assert_eq!(s2.partial_cmp(&s1), Some(Ordering::Greater));
        s1.set(1, 1.into());
        assert_eq!(s1.partial_cmp(&s2), Some(Ordering::Less));
        assert_eq!(s2.partial_cmp(&s1), Some(Ordering::Greater));
        s2.set(2, 1.into());
        assert_eq!(s1.partial_cmp(&s2), None);
        assert_eq!(s2.partial_cmp(&s1), None);
    }

    #[test]
    fn test_stack_set_and_retain() {
        let mut s = Stack::default();
        for index in [5, 63, 0, 9] {
            s.set(index, (index as u64).into());
        }
        let mut copy = s.clone();
        s.set(5, 5.into());
        s.set_null(&new_memory_id(), true);
        let mut copy_to_clear = copy.clone();
        copy_to_clear.retain(0);
        assert_eq!(copy_to_clear.mask, 0);
        assert!(copy_to_clear.data.is_empty());
        s.set(9, Type::UNINITIALIZED);
        s.set(10, Type::UNINITIALIZED);
        s.set(63, 1.into());
        assert_eq!(copy.get(9), &Type::from(9));
        assert_eq!(copy.get(63), &Type::from(63));
        let id = new_memory_id();
        copy.set(5, Type::NullOr { id: id.clone(), inner: Box::new(Type::from(42)) });
        copy.set_null(&id, false);
        assert_eq!(copy.get(5), &Type::from(42));
        s.retain(!(1 << 5));
        assert_eq!(s.mask, 1 | (1 << 63));
        assert_eq!(s.data, vec![Type::from(0), Type::from(1)]);
        for index in [5, 9, 10] {
            assert_eq!(s.get(index), &Type::UNINITIALIZED);
        }
        s.retain(0);
        assert_eq!(s.mask, 0);
        assert!(s.data.is_empty());
        assert_eq!(copy.mask, 1 | (1 << 5) | (1 << 9) | (1 << 63));
    }

    #[test]
    fn test_context_ordering() {
        let mut c1 = ComputationContext::default();
        let mut c2 = ComputationContext::default();

        assert_eq!(c1, c2);
        assert_eq!(c1.partial_cmp(&c2), Some(Ordering::Equal));

        c1.resources.insert(1.into());
        assert_ne!(c1, c2);
        assert_eq!(c1.partial_cmp(&c2), None);
        assert_eq!(c2.partial_cmp(&c1), None);
        c1.resources.clear();
        assert_eq!(c1, c2);

        c1.array_bounds.insert(1.into(), 5);
        assert_eq!(c1.partial_cmp(&c2), Some(Ordering::Less));
        assert_eq!(c2.partial_cmp(&c1), Some(Ordering::Greater));

        c2.array_bounds.insert(1.into(), 7);
        assert_eq!(c1.partial_cmp(&c2), Some(Ordering::Greater));
        assert_eq!(c2.partial_cmp(&c1), Some(Ordering::Less));

        c1.array_bounds.insert(2.into(), 9);
        assert_eq!(c1.partial_cmp(&c2), None);
        assert_eq!(c2.partial_cmp(&c1), None);

        c2.array_bounds.insert(2.into(), 9);
        assert_eq!(c1.partial_cmp(&c2), Some(Ordering::Greater));
        assert_eq!(c2.partial_cmp(&c1), Some(Ordering::Less));

        c2.array_bounds.insert(3.into(), 12);
        assert_eq!(c1.partial_cmp(&c2), Some(Ordering::Greater));
        assert_eq!(c2.partial_cmp(&c1), Some(Ordering::Less));

        c1.pc = 8;
        assert_eq!(c1.partial_cmp(&c2), None);
        assert_eq!(c2.partial_cmp(&c1), None);
    }

    /// A sample of types covering the cases distinguished by `Type::partial_cmp`.
    fn sample_types() -> Vec<Type> {
        let id = MemoryId::from(1);
        vec![
            Type::UNINITIALIZED,
            Type::UNKNOWN_SCALAR,
            Type::from(0),
            Type::from(1),
            Type::from(u64::MAX),
            Type::ScalarValue(ScalarValueData::new(0, 0xff, 0, U64Range::new(0, 0xff))),
            Type::ScalarValue(ScalarValueData::new(0, 0xff, 0, U64Range::new(0, 1))),
            Type::ScalarValue(ScalarValueData::new(0, 0xff, 0xff, U64Range::new(0, 0xff))),
            Type::ScalarValue(ScalarValueData::new(0x100, 0xff, 0, U64Range::new(0x100, 0x1ff))),
            Type::PtrToMemory {
                id: id.clone(),
                offset: 0.into(),
                buffer_size: 8,
                origin: PointerOrigin::MapWritable,
            },
            Type::PtrToArray { id, offset: 1.into() },
        ]
    }

    #[test]
    fn test_known_scalars_is_necessary_for_greater_or_equal() {
        let types = sample_types();
        // Check every pair of types in r0, and in r1 with a known scalar in r0.
        for t1 in types.iter() {
            for t2 in types.iter() {
                for r0 in [None, Some(Type::from(3))] {
                    let mut c1 = ComputationContext::default();
                    let mut c2 = ComputationContext::default();
                    let index = if r0.is_some() { 1 } else { 0 };
                    if let Some(r0) = r0 {
                        c1.set_reg(0, r0.clone()).unwrap();
                        c2.set_reg(0, r0).unwrap();
                    }
                    c1.set_reg(index, t1.clone()).unwrap();
                    c2.set_reg(index, t2.clone()).unwrap();
                    let k1 = KnownScalars::new(&c1);
                    let k2 = KnownScalars::new(&c2);
                    if matches!(c1.partial_cmp(&c2), Some(Ordering::Greater | Ordering::Equal)) {
                        assert!(k1 >= k2, "{t1:?} >= {t2:?}");
                    }
                    if matches!(c1.partial_cmp(&c2), Some(Ordering::Less | Ordering::Equal)) {
                        assert!(k1 <= k2, "{t1:?} <= {t2:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn test_known_scalars_filters_distinct_constants() {
        let mut c1 = ComputationContext::default();
        let mut c2 = ComputationContext::default();
        c1.set_reg(7, Type::from(1)).unwrap();
        c2.set_reg(7, Type::from(2)).unwrap();
        let k1 = KnownScalars::new(&c1);
        let k2 = KnownScalars::new(&c2);
        assert_eq!(k1.partial_cmp(&k2), None);
        assert_eq!(k2.partial_cmp(&k1), None);

        // An unknown or uninitialized register may be greater than a known one, but not less.
        c2.set_reg(7, Type::UNKNOWN_SCALAR).unwrap();
        let k2 = KnownScalars::new(&c2);
        assert!(k1 < k2);
        assert!(k1 < KnownScalars::default());
    }

    #[test]
    fn test_terminating_contexts_matches_unfiltered_search() {
        // Insert contexts in a fixed pseudo random order and check that the filtered search and
        // insertion behave exactly like a plain linear search over the full comparison.
        let types = sample_types();
        let mut contexts = TerminatingContexts::default();
        let mut reference: Vec<ComputationContext> = vec![];
        let mut seed = 17usize;
        for _ in 0..200 {
            let mut context = ComputationContext::default();
            for reg in 0..3 {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                context.set_reg(reg, types[(seed >> 16) % types.len()].clone()).unwrap();
            }

            let expected = reference.iter().rposition(|c| *c >= context);
            let found = contexts.find_greater_or_equal(&context).map(|found| {
                contexts.contexts.iter().position(|c| std::ptr::eq(c, found)).unwrap()
            });
            assert_eq!(found, expected);

            let dominated = reference.iter().any(|c| *c >= context);
            reference.retain(|c| c.partial_cmp(&context) != Some(Ordering::Less));
            if !dominated {
                reference.push(context.clone());
            }
            contexts.insert(context, DataDependencies::default());
            assert_eq!(contexts.contexts.len(), reference.len());
            for (c, r) in contexts.contexts.iter().zip(reference.iter()) {
                assert_eq!(&c.computation_context, r);
            }
        }
    }

    #[test]
    fn test_stack_access() {
        let mut s = Stack::default();

        // Store data in the range [8, 26) and verify that `read_data_ptr()` fails for any
        // reads outside of that range.
        assert!(s.store(StackOffset(8.into()), Type::UNKNOWN_SCALAR, DataWidth::U64).is_ok());
        assert!(s.store(StackOffset(16.into()), Type::UNKNOWN_SCALAR, DataWidth::U64).is_ok());
        assert!(s.store(StackOffset(24.into()), Type::UNKNOWN_SCALAR, DataWidth::U16).is_ok());

        for offset in 0..32 {
            for end in (offset + 1)..32 {
                assert_eq!(
                    s.read_data_ptr(StackOffset(offset.into()), (end - offset) as u64).is_ok(),
                    offset >= 8 && end <= 26
                );
            }
        }

        // Verify that overflows are handled properly.
        assert!(s.read_data_ptr(StackOffset(12.into()), u64::MAX - 2).is_err());
    }

    #[test]
    fn test_compute_range_for_bytes_swap() {
        // Build a list of interesting values. Interesting values are all possible combination of
        // bytes being either 0, max value, or an intermediary value.
        let mut values = BTreeSet::<u64>::default();
        for v1 in &[0x00, 0x1, u64::MAX] {
            for v2 in &[0x00, 0x1, u64::MAX] {
                for v3 in &[0x00, 0x1, u64::MAX] {
                    values.insert(U64Range::assemble_slices((*v1, *v2, *v3), 1, 1));
                }
            }
        }
        // Replace the second byte of old by the first byte of new and return the result.
        let store = |old: u64, new: u64| (old & !0xff00) | ((new & 0xff) << 8);

        for old in &values {
            for new in &values {
                let s = store(*old, *new);
                for min_old in values.iter().filter(|v| *v <= old) {
                    for min_new in values.iter().filter(|v| *v <= new) {
                        for max_old in values.iter().filter(|v| *v >= old) {
                            for max_new in values.iter().filter(|v| *v >= new) {
                                let range = U64Range::compute_range_for_bytes_swap(
                                    U64Range::new(*min_old, *max_old),
                                    U64Range::new(*min_new, *max_new),
                                    1,
                                    0,
                                    1,
                                );
                                assert_leq!(range.min, s);
                                assert_geq!(range.max, s);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_type_constraint_ge_gt() {
        let mut context = ComputationContext::default();
        let (new_lhs, new_rhs) = Type::constraint(
            &mut context,
            JumpType::Ge,
            JumpWidth::W64,
            Type::UNKNOWN_SCALAR,
            Type::from(10),
        )
        .unwrap();

        if let Type::ScalarValue(data1) = new_lhs {
            assert_eq!(data1.min(), 10);
            assert_eq!(data1.max(), u64::MAX);
        } else {
            panic!("Expected ScalarValue");
        }

        if let Type::ScalarValue(data2) = new_rhs {
            assert_eq!(data2.min(), 10);
            assert_eq!(data2.max(), 10);
        } else {
            panic!("Expected ScalarValue");
        }

        let (new_lhs, _) = Type::constraint(
            &mut context,
            JumpType::Gt,
            JumpWidth::W64,
            Type::UNKNOWN_SCALAR,
            Type::from(10),
        )
        .unwrap();

        if let Type::ScalarValue(data1) = new_lhs {
            assert_eq!(data1.min(), 11);
            assert_eq!(data1.max(), u64::MAX);
        } else {
            panic!("Expected ScalarValue");
        }
    }

    #[test]
    fn test_type_constraint_lt_le() {
        let mut context = ComputationContext::default();
        let (new_lhs, _) = Type::constraint(
            &mut context,
            JumpType::Lt,
            JumpWidth::W64,
            Type::UNKNOWN_SCALAR,
            Type::from(10),
        )
        .unwrap();

        if let Type::ScalarValue(data1) = new_lhs {
            assert_eq!(data1.min(), 0);
            assert_eq!(data1.max(), 9);
        } else {
            panic!("Expected ScalarValue");
        }

        let (new_lhs, _) = Type::constraint(
            &mut context,
            JumpType::Le,
            JumpWidth::W64,
            Type::UNKNOWN_SCALAR,
            Type::from(10),
        )
        .unwrap();

        if let Type::ScalarValue(data1) = new_lhs {
            assert_eq!(data1.min(), 0);
            assert_eq!(data1.max(), 10);
        } else {
            panic!("Expected ScalarValue");
        }
    }

    #[test]
    fn test_type_constraint_w32() {
        let mut context = ComputationContext::default();
        let (new_lhs, _) = Type::constraint(
            &mut context,
            JumpType::Eq,
            JumpWidth::W32,
            Type::ScalarValue(ScalarValueData::UNKNOWN_WRITTEN),
            Type::from(10),
        )
        .unwrap();
        if let Type::ScalarValue(data1) = new_lhs {
            assert_eq!(data1.min(), 10);
            assert_eq!(data1.max(), 0xffff_ffff_0000_000a);
        } else {
            panic!("Expected ScalarValue");
        }
    }

    #[test]
    fn test_type_constraint_w32_straddling_min() {
        // A 32-bit equality only constrains the low 32 bits. When the comparison
        // partner's range straddles 2^32 (min <= U32_MAX < max), its low 32 bits
        // can be smaller than its minimum, so the other register's minimum must
        // not be raised. `data1` is in [0, 0x40]; `data2` is in [0x30, u64::MAX].
        // On the equal branch `data1` can still be 0 (e.g. when `data2 == 2^32`,
        // whose low 32 bits are 0), so its minimum must remain 0.
        let mut context = ComputationContext::default();
        let data1 = ScalarValueData::new(0, u64::MAX, 0, U64Range::new(0, 0x40));
        let data2 = ScalarValueData::new(0, u64::MAX, 0, U64Range::new(0x30, u64::MAX));
        let (new_lhs, _) = Type::constraint(
            &mut context,
            JumpType::Eq,
            JumpWidth::W32,
            Type::ScalarValue(data1),
            Type::ScalarValue(data2),
        )
        .unwrap();
        if let Type::ScalarValue(data) = new_lhs {
            assert_eq!(data.min(), 0);
            assert_eq!(data.max(), 0x40);
        } else {
            panic!("Expected ScalarValue");
        }
    }

    #[test]
    fn test_type_constraint_null_or() {
        let mut context = ComputationContext::default();
        let id = new_memory_id();
        let null_or = Type::NullOr { id: id.clone(), inner: Box::new(Type::UNKNOWN_SCALAR) };
        let (new_lhs, _) =
            Type::constraint(&mut context, JumpType::Eq, JumpWidth::W64, null_or, Type::from(0))
                .unwrap();
        if let Type::ScalarValue(data) = new_lhs {
            assert_eq!(data.value, 0);
        } else {
            panic!("Expected zero ScalarValue");
        }
    }

    #[test]
    fn test_type_constraint_array_bounds() {
        let mut context = ComputationContext::default();
        let id = new_memory_id();
        let _ = Type::constraint(
            &mut context,
            JumpType::Le,
            JumpWidth::W64,
            Type::PtrToArray { id: id.clone(), offset: 22.into() },
            Type::PtrToEndArray { id: id.clone() },
        )
        .unwrap();
        assert_eq!(context.array_bounds.get(&id), Some(&22));
    }

    fn make_verification_context() -> VerificationContext {
        VerificationContext::new(
            CoreCallingContext::default(),
            false,
            vec![EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0)],
            vec![None],
        )
    }

    /// Verifies that multiple invocations returning PtrToMemory receive
    /// distinct MemoryIds to prevent ASLR pointer arithmetic between separate
    /// allocations, while still matching the static parameter templates.
    #[test]
    fn test_resolve_return_value_unique_ids() {
        let mut verification_context = make_verification_context();
        let comp_ctx = ComputationContext::default();

        let static_id = MemoryId::from_raw(42);
        let ret_template = Type::PtrToMemory {
            id: static_id.clone(),
            offset: 0.into(),
            buffer_size: 64,
            origin: PointerOrigin::KernelReadOnly,
        };

        let mut next1 = ComputationContext::default();
        let t1 = comp_ctx
            .resolve_return_value(&mut verification_context, &ret_template, &mut next1)
            .unwrap();
        let mut next2 = ComputationContext::default();
        let t2 = comp_ctx
            .resolve_return_value(&mut verification_context, &ret_template, &mut next2)
            .unwrap();

        assert_ne!(t1, t2);
        let (Type::PtrToMemory { id: id1, .. }, Type::PtrToMemory { id: id2, .. }) = (&t1, &t2)
        else {
            panic!("Expected PtrToMemory");
        };
        assert_ne!(id1, id2);
        assert!(static_id.matches(id1));
        assert!(static_id.matches(id2));
    }

    /// Verifies that multiple releasable allocations are tracked individually
    /// in resources, releasing one does not remove the other, and passing a
    /// released resource to a helper parameter is rejected.
    #[test]
    fn test_resolve_return_value_releasable_resource_tracking() {
        let mut verification_context = make_verification_context();
        let comp_ctx = ComputationContext::default();

        let static_id = MemoryId::from_raw(99);
        let releasable_template = Type::ReleasableParameter {
            id: static_id.clone(),
            inner: Box::new(Type::PtrToMemory {
                id: static_id.clone(),
                offset: 0.into(),
                buffer_size: 64,
                origin: PointerOrigin::KernelReadOnly,
            }),
        };

        let mut next = ComputationContext::default();
        let t1 = comp_ctx
            .resolve_return_value(&mut verification_context, &releasable_template, &mut next)
            .unwrap();
        let t2 = comp_ctx
            .resolve_return_value(&mut verification_context, &releasable_template, &mut next)
            .unwrap();

        assert_eq!(next.resources.len(), 2);
        let (Type::Releasable { id: id1, .. }, Type::Releasable { id: id2, .. }) = (&t1, &t2)
        else {
            panic!("Expected Releasable");
        };
        assert_ne!(id1, id2);
        assert!(next.resources.contains(id1));
        assert!(next.resources.contains(id2));

        // Release one
        let mut next_after_free = next.clone();
        t1.match_parameter_type(
            &verification_context,
            &comp_ctx,
            &Type::ReleaseParameter { id: static_id.clone() },
            0,
            &mut next_after_free,
        )
        .unwrap();
        assert_eq!(next_after_free.resources.len(), 1);
        assert!(!next_after_free.resources.contains(id1));
        assert!(next_after_free.resources.contains(id2));

        // Passing released t1 to general helper must fail
        let mut next_uaf = next_after_free.clone();
        let err = t1.match_parameter_type(
            &verification_context,
            &comp_ctx,
            &Type::MemoryParameter {
                size: MemoryParameterSize::Value(64),
                input: true,
                output: false,
            },
            0,
            &mut next_uaf,
        );
        assert_eq!(err, Err(VerifierErrorKind::UseAfterRelease));

        // Passing released t1 to ContextParameter or AnyParameter must also report UseAfterRelease.
        assert_eq!(
            t1.match_parameter_type(
                &verification_context,
                &comp_ctx,
                &Type::ContextParameter { parameter_index: 0 },
                0,
                &mut next_after_free.clone(),
            ),
            Err(VerifierErrorKind::UseAfterRelease),
        );
        assert_eq!(
            t1.match_parameter_type(
                &verification_context,
                &comp_ctx,
                &Type::AnyParameter,
                0,
                &mut next_after_free.clone(),
            ),
            Err(VerifierErrorKind::UseAfterRelease),
        );

        // Releasing an already-released resource reports DoubleRelease even at a non-zero offset.
        let t1_offset = t1.with_offset(8.into());
        assert_eq!(
            t1_offset.match_parameter_type(
                &verification_context,
                &comp_ctx,
                &Type::ReleaseParameter { id: static_id },
                0,
                &mut next_after_free.clone(),
            ),
            Err(VerifierErrorKind::DoubleRelease),
        );

        // Comparing an unreleased Releasable pointer for 64-bit equality with an unrelated pointer
        // must preserve the Releasable wrapper on `t2`.
        let other_ptr = Type::PtrToMemory {
            id: new_memory_id(),
            offset: 0.into(),
            buffer_size: 64,
            origin: PointerOrigin::KernelReadOnly,
        };
        assert_eq!(
            Type::constraint(
                &mut next_after_free,
                JumpType::Eq,
                JumpWidth::W64,
                other_ptr.clone(),
                t2.clone(),
            ),
            Ok((other_ptr, t2)),
        );
    }

    #[test]
    fn ptr_to_struct_ordering() {
        let desc1 = Box::new(StructDescriptor {
            fields: vec![
                FieldDescriptor { offset: 0, field_type: FieldType::Scalar { size: 4 } },
                FieldDescriptor { offset: 4, field_type: FieldType::MutableScalar { size: 4 } },
            ],
        });
        let desc1_same_contents = Box::new(StructDescriptor {
            fields: vec![
                FieldDescriptor { offset: 0, field_type: FieldType::Scalar { size: 4 } },
                FieldDescriptor { offset: 4, field_type: FieldType::MutableScalar { size: 4 } },
            ],
        });
        let desc_other = Box::new(StructDescriptor {
            fields: vec![FieldDescriptor { offset: 0, field_type: FieldType::Scalar { size: 4 } }],
        });

        let id = MemoryId::from_raw(1);
        let ptr_a =
            Type::PtrToStruct { id: id.clone(), offset: 0.into(), descriptor: desc1.clone() };
        let ptr_b_shared_arc =
            Type::PtrToStruct { id: id.clone(), offset: 0.into(), descriptor: desc1.clone() };
        let ptr_c_distinct_arc = Type::PtrToStruct {
            id: id.clone(),
            offset: 0.into(),
            descriptor: desc1_same_contents.clone(),
        };
        let ptr_different_offset =
            Type::PtrToStruct { id: id.clone(), offset: 4.into(), descriptor: desc1.clone() };
        let ptr_other_desc =
            Type::PtrToStruct { id, offset: 0.into(), descriptor: desc_other.clone() };

        // Cloned Box and distinct Box with equal fields both compare
        // Equal.
        assert_eq!(ptr_a.partial_cmp(&ptr_b_shared_arc), Some(Ordering::Equal));
        assert_eq!(ptr_a.partial_cmp(&ptr_c_distinct_arc), Some(Ordering::Equal));
        assert_eq!(ptr_a.partial_cmp(&ptr_different_offset), None);
        assert_eq!(ptr_a.partial_cmp(&ptr_other_desc), None);

        // Non-scalars are strictly narrower than UNINITIALIZED and incomparable with initialized
        // or narrowed uninitialized scalars.
        let narrowed_uninit = Type::ScalarValue(ScalarValueData::UNINITIALIZED & 7.into());
        assert_eq!(ptr_a.partial_cmp(&Type::UNINITIALIZED), Some(Ordering::Less));
        assert_eq!(Type::UNINITIALIZED.partial_cmp(&ptr_a), Some(Ordering::Greater));
        assert_eq!(ptr_a.partial_cmp(&Type::UNKNOWN_SCALAR), None);
        assert_eq!(Type::UNKNOWN_SCALAR.partial_cmp(&ptr_a), None);
        assert_eq!(ptr_a.partial_cmp(&narrowed_uninit), None);
        assert_eq!(narrowed_uninit.partial_cmp(&ptr_a), None);

        // Narrowed uninitialized scalar (`unwritten_mask == u64::MAX`, `urange == [0, 7]`) is
        // strictly narrower than `Type::UNINITIALIZED`, broader than `5`, and incomparable with
        // out-of-range `1000`.
        assert_eq!(Type::UNINITIALIZED.partial_cmp(&narrowed_uninit), Some(Ordering::Greater));
        assert_eq!(narrowed_uninit.partial_cmp(&Type::UNINITIALIZED), Some(Ordering::Less));
        assert_eq!(narrowed_uninit.partial_cmp(&Type::from(5)), Some(Ordering::Greater));
        assert_eq!(Type::from(5).partial_cmp(&narrowed_uninit), Some(Ordering::Less));
        assert_eq!(narrowed_uninit.partial_cmp(&Type::from(1000)), None);
        assert_eq!(Type::from(1000).partial_cmp(&narrowed_uninit), None);
    }

    #[test]
    fn stack_ordering_disjoint_and_overlapping_keys() {
        let mut s_broad = Stack::default();
        let mut s_narrow = Stack::default();

        // Populate s_narrow at keys 1, 3, 5; keep s_broad empty (all UNINITIALIZED).
        s_narrow.set(1, Type::from(10));
        s_narrow.set(3, Type::from(20));
        s_narrow.set(5, Type::from(30));
        assert_eq!(s_broad.partial_cmp(&s_narrow), Some(Ordering::Greater));
        assert_eq!(s_narrow.partial_cmp(&s_broad), Some(Ordering::Less));

        // Add middle key 3 to s_broad with a broader type (UNKNOWN_SCALAR > 20).
        // s_narrow has an earlier key (1 < 3) and a later key (5 > 3), exercising both
        // `Ordering::Greater` on `k1.cmp(k2)` and `(None, Some(_))` after `s_broad` is exhausted.
        s_broad.set(3, Type::UNKNOWN_SCALAR);
        assert_eq!(s_broad.partial_cmp(&s_narrow), Some(Ordering::Greater));
        assert_eq!(s_narrow.partial_cmp(&s_broad), Some(Ordering::Less));

        // Setting a slot to UNINITIALIZED removes it from the map and preserves the ordering,
        // whereas a narrowed uninitialized scalar stays in the map and is strictly narrower than
        // an empty (all UNINITIALIZED) stack.
        s_broad.set(3, Type::UNINITIALIZED);
        assert!(s_broad.data.is_empty());
        assert_eq!(s_broad.partial_cmp(&s_narrow), Some(Ordering::Greater));
        let mut s_narrowed_uninit = Stack::default();
        s_narrowed_uninit.set(0, Type::ScalarValue(ScalarValueData::UNINITIALIZED & 7.into()));
        assert_eq!(s_broad.partial_cmp(&s_narrowed_uninit), Some(Ordering::Greater));
        assert_eq!(s_narrowed_uninit.partial_cmp(&s_broad), Some(Ordering::Less));

        // Conflicting orderings at overlapping keys must return None.
        s_broad.set(1, Type::from(10));
        s_broad.set(3, Type::UNKNOWN_SCALAR);
        s_broad.set(5, Type::from(99));
        assert_eq!(s_broad.partial_cmp(&s_narrow), None);
        assert_eq!(s_narrow.partial_cmp(&s_broad), None);
    }

    #[test]
    fn test_verifier_logger_is_enabled() {
        struct LimitedLogger {
            max_lines: usize,
            lines: Vec<String>,
        }

        impl VerifierLogger for LimitedLogger {
            fn is_enabled(&self) -> bool {
                self.lines.len() < self.max_lines
            }

            fn log(&mut self, line: &[u8]) {
                assert!(line.is_ascii());
                self.lines.push(std::str::from_utf8(line).unwrap().to_string());
            }
        }

        let code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];

        let mut full_logger = LimitedLogger { max_lines: usize::MAX, lines: vec![] };
        verify_program(code.clone(), CallingContext::default(), &mut full_logger).unwrap();
        assert_eq!(
            full_logger.lines,
            vec!["0: (b7) mov %r0, 0x0", "1: (07) add %r0, 0x1", "2: (95) exit"]
        );

        let mut capped_logger = LimitedLogger { max_lines: 2, lines: vec![] };
        verify_program(code, CallingContext::default(), &mut capped_logger).unwrap();
        assert_eq!(capped_logger.lines, vec!["0: (b7) mov %r0, 0x0", "1: (07) add %r0, 0x1"]);
    }

    /// Verifies that memory store instructions to read-only memory pointers are
    /// rejected, while stores to writable pointers succeed.
    #[test]
    fn test_read_only_map_value_store() {
        let mut context = make_verification_context();
        let mut next = ComputationContext::default();

        let ptr_with_origin = |origin| Type::PtrToMemory {
            id: new_memory_id(),
            offset: 0.into(),
            buffer_size: 4,
            origin,
        };

        let u32_field = Field::new(0, DataWidth::U32);

        // Store to read-only memory must fail.
        let ro_map_ptr = ptr_with_origin(PointerOrigin::MapReadOnly);
        let err = next.store_memory(&mut context, 0, &ro_map_ptr, u32_field, Type::from(42));
        assert_eq!(err, Err(VerifierErrorKind::WriteToReadOnlyMemory));

        let ro_kernel_ptr = ptr_with_origin(PointerOrigin::KernelReadOnly);
        let err = next.store_memory(&mut context, 0, &ro_kernel_ptr, u32_field, Type::from(42));
        assert_eq!(err, Err(VerifierErrorKind::WriteToReadOnlyMemory));

        // Store to writable memory must succeed.
        let rw_map_ptr = ptr_with_origin(PointerOrigin::MapWritable);
        let res = next.store_memory(&mut context, 0, &rw_map_ptr, u32_field, Type::from(42));
        assert!(res.is_ok());

        let rw_kernel_ptr = ptr_with_origin(PointerOrigin::KernelWritable);
        let res = next.store_memory(&mut context, 0, &rw_kernel_ptr, u32_field, Type::from(42));
        assert!(res.is_ok());
    }

    /// Verifies that atomic operations are only allowed on naturally aligned
    /// addresses within the stack or a map value.
    #[test]
    fn test_atomic_access_checks() {
        let context = ComputationContext::default();

        let ptr_to_memory = |offset: ScalarValueData, origin| Type::PtrToMemory {
            id: new_memory_id(),
            offset,
            buffer_size: 64,
            origin,
        };
        let ptr_to_stack =
            |offset: i16| Type::PtrToStack { offset: StackOffset::default().add_i16(offset) };
        // An offset that is either 0 or 1: its low bit is unknown.
        let unknown_low_bit = ScalarValueData::new(0, 1, 0, U64Range::new(0, 1));
        // An offset that is either 0 or 8: its low 3 bits are known to be 0.
        let unknown_high_bits = ScalarValueData::new(0, 8, 0, U64Range::new(0, 8));

        let u32_field = |offset| Field::new(offset, DataWidth::U32);
        let u64_field = |offset| Field::new(offset, DataWidth::U64);

        // Aligned accesses to map values and the stack are allowed.
        for origin in [PointerOrigin::MapWritable, PointerOrigin::MapReadOnly] {
            let ptr = ptr_to_memory(0.into(), origin);
            assert_eq!(context.check_atomic_access(&ptr, u64_field(8)), Ok(()));
            assert_eq!(context.check_atomic_access(&ptr, u32_field(4)), Ok(()));
            let ptr = ptr_to_memory(4.into(), origin);
            assert_eq!(context.check_atomic_access(&ptr, u64_field(4)), Ok(()));
            assert_eq!(context.check_atomic_access(&ptr, u32_field(-4)), Ok(()));
            let ptr = ptr_to_memory(unknown_high_bits, origin);
            assert_eq!(context.check_atomic_access(&ptr, u64_field(16)), Ok(()));
        }
        assert_eq!(context.check_atomic_access(&ptr_to_stack(-8), u64_field(0)), Ok(()));
        assert_eq!(context.check_atomic_access(&ptr_to_stack(-4), u32_field(-4)), Ok(()));

        // Misaligned accesses are rejected, including when the alignment cannot
        // be proven because the low bits of the offset are unknown.
        let map_ptr = ptr_to_memory(0.into(), PointerOrigin::MapWritable);
        assert_eq!(
            context.check_atomic_access(&map_ptr, u64_field(4)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );
        assert_eq!(
            context.check_atomic_access(&map_ptr, u32_field(2)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );
        let map_ptr = ptr_to_memory(4.into(), PointerOrigin::MapWritable);
        assert_eq!(
            context.check_atomic_access(&map_ptr, u64_field(0)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );
        let map_ptr = ptr_to_memory(unknown_low_bit, PointerOrigin::MapWritable);
        assert_eq!(
            context.check_atomic_access(&map_ptr, u64_field(0)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );
        assert_eq!(
            context.check_atomic_access(&ptr_to_stack(-6), u32_field(0)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );
        assert_eq!(
            context.check_atomic_access(&ptr_to_stack(-8), u64_field(4)),
            Err(VerifierErrorKind::MisalignedAtomicAccess)
        );

        // Atomic accesses to other kinds of memory are rejected, even when
        // aligned.
        for origin in [PointerOrigin::KernelWritable, PointerOrigin::KernelReadOnly] {
            let ptr = ptr_to_memory(0.into(), origin);
            assert_eq!(
                context.check_atomic_access(&ptr, u64_field(8)),
                Err(VerifierErrorKind::AtomicAccessNotAllowed)
            );
        }
        assert_eq!(
            context.check_atomic_access(&Type::from(0), u64_field(0)),
            Err(VerifierErrorKind::AtomicAccessNotAllowed)
        );
    }

    #[test]
    fn test_struct_access_pc() {
        let descriptor = Box::new(StructDescriptor {
            fields: vec![
                FieldDescriptor { offset: 0, field_type: FieldType::MutableScalar { size: 4 } },
                FieldDescriptor { offset: 4, field_type: FieldType::Scalar { size: 4 } },
                FieldDescriptor { offset: 8, field_type: FieldType::MutableScalar { size: 4 } },
                FieldDescriptor {
                    offset: 12,
                    field_type: FieldType::PtrToMemory {
                        is_32_bit: true,
                        id: MemoryId::from_raw(20),
                        buffer_size: 64,
                    },
                },
            ],
        });
        let memory_id = MemoryId::from_raw(1);
        let ptr = Type::PtrToStruct { id: memory_id.clone(), offset: 0.into(), descriptor };
        let calling_context = CallingContext { args: vec![ptr], ..CallingContext::default() };

        // Instructions:
        // PC 0: r0 = 0
        // PC 1: r2 = 42
        // PC 2: stw [r1 + 0], 10 (BPF_ST - store to offset 0)
        // PC 3: ldxw r3, [r1 + 4] (BPF_LDX - load scalar from offset 4 right after store at PC 2)
        // PC 4: stxw [r1 + 8], r2 (BPF_STX - store to offset 8)
        // PC 5: ldxw r4, [r1 + 12] (BPF_LDX - load 32-bit ptr from offset 12 right after store at PC 4)
        // PC 6: exit
        let code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 42),
            EbpfInstruction::new(BPF_ST | BPF_MEM | BPF_W, 1, 0, 0, 10),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 3, 1, 4, 0),
            EbpfInstruction::new(BPF_STX | BPF_MEM | BPF_W, 1, 2, 8, 0),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 4, 1, 12, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];

        let program = verify_program(code, calling_context, &mut NullVerifierLogger)
            .expect("verification failed");

        assert_eq!(
            program.struct_access_instructions(),
            &[
                StructAccess {
                    pc: 2,
                    memory_id: memory_id.clone(),
                    field_offset: 0,
                    is_32_bit_ptr_load: false,
                },
                StructAccess {
                    pc: 3,
                    memory_id: memory_id.clone(),
                    field_offset: 4,
                    is_32_bit_ptr_load: false,
                },
                StructAccess {
                    pc: 4,
                    memory_id: memory_id.clone(),
                    field_offset: 8,
                    is_32_bit_ptr_load: false,
                },
                StructAccess {
                    pc: 5,
                    memory_id: memory_id.clone(),
                    field_offset: 12,
                    is_32_bit_ptr_load: true,
                },
            ]
        );
    }

    #[test]
    fn test_mixed_struct_and_non_struct_memory_access() {
        let make_context = || {
            let descriptor = Box::new(StructDescriptor {
                fields: vec![
                    FieldDescriptor { offset: 0, field_type: FieldType::MutableScalar { size: 4 } },
                    FieldDescriptor { offset: 4, field_type: FieldType::MutableScalar { size: 4 } },
                ],
            });
            CallingContext {
                args: vec![
                    Type::PtrToStruct { id: MemoryId::from_raw(1), offset: 0.into(), descriptor },
                    Type::PtrToMemory {
                        id: MemoryId::from_raw(2),
                        offset: 0.into(),
                        buffer_size: 8,
                        origin: PointerOrigin::KernelWritable,
                    },
                    Type::UNKNOWN_SCALAR,
                ],
                ..CallingContext::default()
            }
        };

        // Mixing `PtrToStruct` (`r1`) and `PtrToMemory` (`r2`) at a single load PC fails with
        // `InconsistentMemoryAccess` (`EINVAL`).
        let mixed_load = vec![
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 3, 0, 1, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 2, 0, 0),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 0, 1, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let err = verify_program(mixed_load, make_context(), &mut NullVerifierLogger).unwrap_err();
        assert_eq!(
            err,
            VerifierError { kind: VerifierErrorKind::InconsistentMemoryAccess, pc: Some(2) }
        );
        assert_eq!(err.errno(), EINVAL);

        // Mixing `PtrToStruct` (`r1`) and `PtrToMemory` (`r2`) at a single store PC fails with
        // `InconsistentMemoryAccess` (`EINVAL`).
        let mixed_store = vec![
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 3, 0, 1, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 2, 0, 0),
            EbpfInstruction::new(BPF_ST | BPF_MEM | BPF_W, 1, 0, 0, 7),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let err = verify_program(mixed_store, make_context(), &mut NullVerifierLogger).unwrap_err();
        assert_eq!(
            err,
            VerifierError { kind: VerifierErrorKind::InconsistentMemoryAccess, pc: Some(2) }
        );
        assert_eq!(err.errno(), EINVAL);

        // Accessing two different `PtrToStruct` field offsets at the same PC fails with
        // `InconsistentStructFieldAccess` (`EACCES`).
        let inconsistent_struct_field = vec![
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 3, 0, 1, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, 4),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 0, 1, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let err =
            verify_program(inconsistent_struct_field, make_context(), &mut NullVerifierLogger)
                .unwrap_err();
        assert_eq!(
            err,
            VerifierError { kind: VerifierErrorKind::InconsistentStructFieldAccess, pc: Some(2) }
        );
        assert_eq!(err.errno(), EACCES);
    }

    #[test]
    fn test_ptr_to_array_max_packet_offset() {
        let make_context = || {
            let id = new_memory_id();
            CallingContext {
                args: vec![
                    Type::PtrToArray { id: id.clone(), offset: 0.into() },
                    Type::PtrToEndArray { id },
                ],
                ..CallingContext::default()
            }
        };

        // `offset == MAX_PACKET_OFFSET` (`0xffff`) is accepted, as is subtracting back within
        // `[0, MAX_PACKET_OFFSET]`.
        let ok_at_max = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, MAX_PACKET_OFFSET as i32),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert!(verify_program(ok_at_max, make_context(), &mut NullVerifierLogger).is_ok());

        let ok_sub_from_max = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, MAX_PACKET_OFFSET as i32),
            EbpfInstruction::new(BPF_ALU64 | BPF_SUB | BPF_K, 1, 0, 0, MAX_PACKET_OFFSET as i32),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert!(verify_program(ok_sub_from_max, make_context(), &mut NullVerifierLogger).is_ok());

        // `offset == MAX_PACKET_OFFSET + 1` (`0x10000`) directly via `add %r1, 0x10000` fails with
        // `PointerOffsetOverflow`.
        let err_direct_over_max = vec![
            EbpfInstruction::new(
                BPF_ALU64 | BPF_ADD | BPF_K,
                1,
                0,
                0,
                (MAX_PACKET_OFFSET + 1) as i32,
            ),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_direct_over_max, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::PointerOffsetOverflow, pc: Some(0) })
        );

        // Reaching `0x10000` via `0xffff + 1` fails with `PointerOffsetOverflow` at the second
        // `add`.
        let err_step_over_max = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, MAX_PACKET_OFFSET as i32),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, 1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_step_over_max, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::PointerOffsetOverflow, pc: Some(1) })
        );

        // Negative immediate (`add %r1, -1`, sign-extended to `u64::MAX`) and `sub %r1, -1` at
        // `0xffff` both fail with `PointerOffsetOverflow`.
        let err_negative_add = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, -1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_negative_add, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::PointerOffsetOverflow, pc: Some(0) })
        );

        let err_negative_sub = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, MAX_PACKET_OFFSET as i32),
            EbpfInstruction::new(BPF_ALU64 | BPF_SUB | BPF_K, 1, 0, 0, -1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_negative_sub, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::PointerOffsetOverflow, pc: Some(1) })
        );
    }

    #[test]
    fn test_mem_param_stack_size_wraparound() {
        static READ_MEM_HELPER: std::sync::LazyLock<HelperDefinition> =
            std::sync::LazyLock::new(|| HelperDefinition {
                index: 1,
                name: "read_mem",
                signature: FunctionSignatureBuilder {
                    args: vec![
                        Type::MemoryParameter {
                            size: MemoryParameterSize::Reference { index: 1 },
                            input: true,
                            output: false,
                        },
                        Type::ScalarValueParameter,
                    ],
                    return_value: Type::UNKNOWN_SCALAR,
                    invalidate_array_bounds: false,
                }
                .build(),
            });
        static WRITE_MEM_HELPER: std::sync::LazyLock<HelperDefinition> =
            std::sync::LazyLock::new(|| HelperDefinition {
                index: 2,
                name: "write_mem",
                signature: FunctionSignatureBuilder {
                    args: vec![
                        Type::MemoryParameter {
                            size: MemoryParameterSize::Reference { index: 1 },
                            input: false,
                            output: true,
                        },
                        Type::ScalarValueParameter,
                    ],
                    return_value: Type::UNKNOWN_SCALAR,
                    invalidate_array_bounds: false,
                }
                .build(),
            });
        static UNTOUCHED_MEM_HELPER: std::sync::LazyLock<HelperDefinition> =
            std::sync::LazyLock::new(|| HelperDefinition {
                index: 3,
                name: "untouched_mem",
                signature: FunctionSignatureBuilder {
                    args: vec![
                        Type::MemoryParameter {
                            size: MemoryParameterSize::Reference { index: 1 },
                            input: false,
                            output: false,
                        },
                        Type::ScalarValueParameter,
                    ],
                    return_value: Type::UNKNOWN_SCALAR,
                    invalidate_array_bounds: false,
                }
                .build(),
            });

        let make_context = || CallingContext {
            args: vec![Type::UNKNOWN_SCALAR],
            helpers: HashMap::from([
                (1, &*READ_MEM_HELPER),
                (2, &*WRITE_MEM_HELPER),
                (3, &*UNTOUCHED_MEM_HELPER),
            ]),
            ..CallingContext::default()
        };

        // At offset `r10 - 8` (`504`), passing `size = -504` (`u64::MAX - 503`) would wrap
        // `504 + size` modulo 2^64 to `0 <= BPF_STACK_SIZE` without the `size > BPF_STACK_SIZE`
        // guard, returning `StackAccessOutOfBounds` from `read_data_ptr`/`write_data_ptr` (or `Ok`
        // when neither `input` nor `output` is set) instead of `OutOfBoundsMemoryAccess`.
        for helper_id in [1, 2, 3] {
            let code = vec![
                EbpfInstruction::new(BPF_ST | BPF_MEM | BPF_DW, 10, 0, -8, 0),
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 10, 0, 0),
                EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, -8),
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, -504),
                EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, helper_id),
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ];
            assert_eq!(
                verify_program(code, make_context(), &mut NullVerifierLogger).err(),
                Some(VerifierError {
                    kind: VerifierErrorKind::OutOfBoundsMemoryAccess,
                    pc: Some(4),
                }),
                "helper {helper_id} should reject wrapped stack buffer size with OutOfBoundsMemoryAccess",
            );
        }

        // `size == 0` at a valid constant stack offset (`r10 - 8`) succeeds, whereas `r10`
        // (`offset == BPF_STACK_SIZE`) and variable offsets are rejected by `check_valid_offset`.
        let ok_zero_size = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 10, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 1, 0, 0, -8),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert!(verify_program(ok_zero_size, make_context(), &mut NullVerifierLogger).is_ok());

        let err_zero_size_at_r10 = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 10, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_zero_size_at_r10, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::StackAccessOutOfBounds, pc: Some(2) })
        );

        let err_zero_size_var_offset = vec![
            EbpfInstruction::new(BPF_ALU64 | crate::BPF_AND | BPF_K, 1, 0, 0, 7),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 3, 10, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 3, 0, 0, -16),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_X, 3, 1, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 3, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            verify_program(err_zero_size_var_offset, make_context(), &mut NullVerifierLogger).err(),
            Some(VerifierError { kind: VerifierErrorKind::VariableStackOffset, pc: Some(6) })
        );
    }

    #[test]
    fn test_constraint_and_comp_eq_disjoint_known_bits() {
        let mut comp_ctx = ComputationContext::default();

        // Disjoint 64-bit scalars in `Type::constraint` must preserve `(type1, type2)` rather than
        // falling through to `(type1.clone(), type1)` and overwriting `type2`.
        let even = Type::from(2u64);
        let odd_1_or_3 = Type::ScalarValue(ScalarValueData::new(1, 2, 0, U64Range::new(1, 3)));
        assert_eq!(
            Type::constraint(
                &mut comp_ctx,
                JumpType::Eq,
                JumpWidth::W64,
                even.clone(),
                odd_1_or_3.clone()
            ),
            Ok((even, odd_1_or_3)),
        );

        // Disjoint same-base pointer offsets in `constrain_equal_offsets` must also preserve both
        // pointers' original offsets.
        let id = new_memory_id();
        let ptr1 = Type::PtrToMemory {
            id: id.clone(),
            offset: 2u64.into(),
            buffer_size: 16,
            origin: PointerOrigin::MapWritable,
        };
        let ptr2 = Type::PtrToMemory {
            id,
            offset: ScalarValueData::new(1, 2, 0, U64Range::new(1, 3)),
            buffer_size: 16,
            origin: PointerOrigin::MapWritable,
        };
        assert_eq!(
            Type::constraint(
                &mut comp_ctx,
                JumpType::Eq,
                JumpWidth::W64,
                ptr1.clone(),
                ptr2.clone()
            ),
            Ok((ptr1, ptr2)),
        );

        // `JumpWidth::W32` with `data1` straddling `2^32` (`[0xffff_ffff, 0x1_0000_0002]`): low
        // 32-bit comparison against `3` can match in 32-bit projections, yet `{k*2^32 + 3}` has
        // empty intersection with `[0xffff_ffff, 0x1_0000_0002]`, so `Type::constraint` must leave
        // `(type1, type2)` unchanged rather than constructing an invalid `ScalarValueData`.
        let straddling = Type::ScalarValue(ScalarValueData::new(
            0,
            u64::MAX,
            0,
            U64Range::new(0xffff_ffff, 0x1_0000_0002),
        ));
        let three = Type::from(3u64);
        assert_eq!(
            Type::constraint(
                &mut comp_ctx,
                JumpType::Eq,
                JumpWidth::W32,
                straddling.clone(),
                three.clone()
            ),
            Ok((straddling, three)),
        );

        // Shared `common_mask` prefix (`[4, 5]` vs `{2, 6}`) and bit-`b` gap (`[2, 4]` vs `{1, 5}`).
        let prefix_x = ScalarValueData::new(0, 7, 0, U64Range::new(4, 5));
        let prefix_y = ScalarValueData::new(2, 4, 0, U64Range::new(2, 6));
        assert_eq!(comp_eq64(prefix_x, prefix_y), Ok(Some(false)));
        assert_eq!(comp_eq32(prefix_x, prefix_y), Ok(Some(false)));

        let gap_x = ScalarValueData::new(1, 6, 0, U64Range::new(1, 4));
        let gap_y = ScalarValueData::new(0, 5, 0, U64Range::new(2, 5));
        assert_eq!(comp_eq64(gap_x, gap_y), Ok(Some(false)));
        assert_eq!(comp_eq32(gap_x, gap_y), Ok(Some(false)));

        // Singleton-range scalar whose `unknown_mask` was not cleared (`ScalarValueData::new`
        // clamps `[4, 6]` against `value = 6, unknown_mask = 1` to `[6, 6]`) still constant-folds
        // equality with `6` to `Some(true)`.
        let singleton_with_unk = ScalarValueData::new(6, 1, 0, U64Range::new(4, 6));
        assert!(!singleton_with_unk.is_known());
        assert_eq!(comp_eq64(singleton_with_unk, 6u64.into()), Ok(Some(true)));
        assert_eq!(comp_eq32(singleton_with_unk, 6u64.into()), Ok(Some(true)));
    }

    #[test]
    fn test_lddw_map_value_offset_and_dead_code_sanitization() {
        let make_array_context = || {
            let mut ctx = CallingContext::default();
            ctx.register_map(MapSchema {
                map_type: BPF_MAP_TYPE_ARRAY,
                key_size: 4,
                value_size: 8,
                max_entries: 1,
                flags: MapFlags::empty(),
            });
            ctx
        };

        // Negative or `>= value_size` offsets in `BPF_PSEUDO_MAP_IDX_VALUE` must fail with
        // `InvalidMapValueOffset` (`EINVAL`), both when reachable and when unreachable.
        for bad_offset in [-4, 8, i32::MIN] {
            let reachable = vec![
                EbpfInstruction::new(BPF_LDDW, 1, BPF_PSEUDO_MAP_IDX_VALUE, 0, 0),
                EbpfInstruction::new(0, 0, 0, 0, bad_offset),
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            ];
            let err = verify_program(reachable, make_array_context(), &mut NullVerifierLogger)
                .unwrap_err();
            assert_eq!(
                err,
                VerifierError {
                    kind: VerifierErrorKind::InvalidMapValueOffset(bad_offset, 8),
                    pc: Some(0),
                }
            );
            assert_eq!(err.errno(), linux_uapi::EINVAL);

            let unreachable = vec![
                EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
                EbpfInstruction::new(BPF_LDDW, 1, BPF_PSEUDO_MAP_IDX_VALUE, 0, 0),
                EbpfInstruction::new(0, 0, 0, 0, bad_offset),
            ];
            assert_eq!(
                verify_program(unreachable, make_array_context(), &mut NullVerifierLogger).err(),
                Some(VerifierError {
                    kind: VerifierErrorKind::InvalidMapValueOffset(bad_offset, 8),
                    pc: Some(2),
                })
            );
        }

        // Valid `BPF_PSEUDO_MAP_IDX_VALUE` offset (`4 < 8`) verifies, preserves a visited `LDDW`'s
        // second slot (`pc = 1`), and sanitizes unvisited instructions (including an unreachable
        // unknown helper call and an unreachable valid `LDDW`) to `ja -1`.
        let code = vec![
            EbpfInstruction::new(BPF_LDDW, 1, BPF_PSEUDO_MAP_IDX_VALUE, 0, 0),
            EbpfInstruction::new(0, 0, 0, 0, 4),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_W, 0, 1, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, 9999),
            EbpfInstruction::new(BPF_LDDW, 2, 0, 0, 0x1234),
            EbpfInstruction::new(0, 0, 0, 0, 0x5678),
        ];
        let verified = verify_program(code, make_array_context(), &mut NullVerifierLogger)
            .expect("program with dead unknown helper should verify");
        assert_eq!(verified.code()[0].code(), BPF_LDDW);
        assert_eq!(verified.code()[1], EbpfInstruction::new(0, 0, 0, 0, 4));
        let dead_insn = EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, -1, 0);
        assert_eq!(verified.code()[4], dead_insn);
        assert_eq!(verified.code()[5], dead_insn);
        assert_eq!(verified.code()[6], dead_insn);
    }

    #[test]
    #[should_panic]
    fn test_signature_too_many_args_panics() {
        FunctionSignatureBuilder {
            args: vec![Type::ScalarValueParameter; usize::from(ARGUMENT_COUNT) + 1],
            return_value: Type::UNKNOWN_SCALAR,
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    #[should_panic]
    fn test_signature_map_param_not_pointing_to_map_panics() {
        FunctionSignatureBuilder {
            args: vec![Type::ScalarValueParameter, Type::MapKeyParameter { map_ptr_index: 0 }],
            return_value: Type::UNKNOWN_SCALAR,
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    #[should_panic]
    fn test_signature_mem_size_ref_not_pointing_to_scalar_panics() {
        FunctionSignatureBuilder {
            args: vec![
                Type::MemoryParameter {
                    size: MemoryParameterSize::Reference { index: 1 },
                    input: true,
                    output: false,
                },
                Type::AnyParameter,
            ],
            return_value: Type::UNKNOWN_SCALAR,
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    #[should_panic]
    fn test_signature_nested_null_or_panics() {
        FunctionSignatureBuilder {
            args: vec![],
            return_value: Type::NullOrParameter(Box::new(Type::NullOrParameter(Box::new(
                Type::MemoryParameter {
                    size: MemoryParameterSize::Value(8),
                    input: true,
                    output: true,
                },
            )))),
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    #[should_panic]
    fn test_signature_non_pointer_null_or_panics() {
        FunctionSignatureBuilder {
            args: vec![],
            return_value: Type::NullOrParameter(Box::new(Type::UNINITIALIZED)),
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    #[should_panic]
    fn test_signature_nested_releasable_panics() {
        FunctionSignatureBuilder {
            args: vec![],
            return_value: Type::ReleasableParameter {
                id: new_memory_id(),
                inner: Box::new(Type::ReleasableParameter {
                    id: new_memory_id(),
                    inner: Box::new(Type::MemoryParameter {
                        size: MemoryParameterSize::Value(8),
                        input: true,
                        output: true,
                    }),
                }),
            },
            invalidate_array_bounds: false,
        }
        .build();
    }

    #[test]
    fn test_worklist_lifo_and_slot_reuse() {
        let mut worklist = Worklist::new();
        assert!(worklist.pop().is_none());

        for pc in [1usize, 2, 3] {
            let mut ctx = ComputationContext::default();
            ctx.pc = pc;
            worklist.push(WorklistEntry { context: ctx, parent: Some(pc * 10) });
        }
        assert_eq!(worklist.entries.len(), 3);

        let e3 = worklist.pop().expect("entry 3");
        assert_eq!((e3.context.pc, e3.parent), (3, Some(30)));
        let e2 = worklist.pop().expect("entry 2");
        assert_eq!((e2.context.pc, e2.parent), (2, Some(20)));

        // Pushing two new entries must reuse the vacated slots without growing `entries`.
        for pc in [4usize, 5] {
            let mut ctx = ComputationContext::default();
            ctx.pc = pc;
            worklist.push(WorklistEntry { context: ctx, parent: None });
        }
        assert_eq!(worklist.entries.len(), 3);

        assert_eq!(worklist.pop().expect("entry 5").context.pc, 5);
        assert_eq!(worklist.pop().expect("entry 4").context.pc, 4);
        assert_eq!(worklist.pop().expect("entry 1").context.pc, 1);
        assert!(worklist.pop().is_none());
    }

    #[test]
    fn test_explorer_arena_slot_reuse_and_complete_child() {
        let mut arena = ExplorerArena::new();

        for round in 0..8usize {
            let mut root_ctx = ComputationContext::default();
            root_ctx.pc = round;
            let root_idx = arena.push(ExplorerNode {
                context: root_ctx,
                parent: None,
                pending_children: 2,
                dependencies: DataDependencies::default(),
            });

            let mut dep1 = DataDependencies::default();
            dep1.add_reg(1);
            assert!(arena.complete_child(root_idx, dep1).is_none());

            let mut dep2 = DataDependencies::default();
            dep2.add_reg(3);
            let completed = arena
                .complete_child(root_idx, dep2)
                .expect("second child completion must extract parent node");
            assert_eq!(completed.context.pc, round);
            assert!(completed.dependencies.has_reg(1));
            assert!(completed.dependencies.has_reg(3));
            assert!(!completed.dependencies.has_reg(2));
        }

        // Recycling vacated slots keeps the backing `Vec` bounded to 1 slot across all 8 rounds.
        assert_eq!(arena.nodes.len(), 1);
    }

    #[test]
    fn test_diamond_pruning_propagates_dependencies_to_ancestor() {
        use crate::api::{BPF_ADD, BPF_ALU64, BPF_JEQ, BPF_K, BPF_MOV, BPF_X};

        // Diamond CFG where `r1` (argument 1) is unknown scalar and `r3` is only read after the
        // diamond join (`pc = 4`). When `r3` is initialized before the diamond, verification
        // succeeds and prunes the second arm at `pc = 4`.
        let make_scalar_arg_ctx =
            || CallingContext { args: vec![Type::UNKNOWN_SCALAR], ..Default::default() };

        let valid_diamond = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 3, 0, 0, 7),
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 2, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, 0, 0),
            // Join point (`pc = 4`, prune point): reads `r3`!
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 0, 3, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert!(
            verify_program(valid_diamond, make_scalar_arg_ctx(), &mut NullVerifierLogger).is_ok()
        );

        // Nested diamond where `r3`'s liveness at the outer prune point (`pc = 4`) depends
        // exclusively on `ending_deps` propagated through `if let Some(ending_deps) = pruned_deps`:
        // 1. Outer fallthrough (`pc = 1..3`) sets `r3 = 7`, widens `r1 = UNKNOWN`, and enters the
        //    outer join `pc = 4`.
        // 2. Inner fallthrough (`pc = 5..7`, explored first by LIFO) overwrites `r3 = r1`
        //    (`UNKNOWN`), killing `r3` in its own upward dependencies while recording
        //    `terminating_contexts[8]` with `deps = {r3}`.
        // 3. Inner taken edge (`pc = 4 -> pc = 8`, explored second) is pruned against
        //    `terminating_contexts[8]`, returning `pruned_deps = Some({r3})`. Only that `pruned_deps`
        //    propagation adds `r3` to `ExplorerNode(pc = 4).dependencies` and `terminating_contexts[4]`.
        // 4. Outer taken edge (`pc = 0 -> pc = 4`, where `r3` is uninitialized) must therefore NOT
        //    be pruned at `pc = 4` and must fail verification at `pc = 8`.
        let make_two_scalar_args_ctx = || CallingContext {
            args: vec![Type::UNKNOWN_SCALAR, Type::UNKNOWN_SCALAR],
            ..Default::default()
        };
        let bug_nested_diamond = vec![
            // pc = 0: if r1 == 0 goto pc = 4 (outer join, leaving r3 uninitialized)
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 3, 0),
            // pc = 1..3: outer fallthrough initializes r3 = 7 and widens r1 = r2 (UNKNOWN)
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 3, 0, 0, 7),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 1, 2, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, 0, 0),
            // pc = 4 (outer prune point): if r2 == 0 goto pc = 8 (inner join, keeping r3 = 7)
            EbpfInstruction::new(BPF_JMP | BPF_JEQ | BPF_K, 2, 0, 3, 0),
            // pc = 5..7: inner fallthrough overwrites r3 = r1 (killing r3 dependency on this arm)
            // and widens r2 = r1 before reaching pc = 8
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 3, 1, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 2, 1, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_JA, 0, 0, 0, 0),
            // pc = 8 (inner prune point): reads r3 and exits
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 0, 3, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert!(
            verify_program(bug_nested_diamond, make_two_scalar_args_ctx(), &mut NullVerifierLogger)
                .is_err(),
            "nested diamond arm leaving r3 uninitialized must not be pruned at outer join"
        );
    }

    #[test]
    fn test_id_set_sorted_insert_remove_contains_eq() {
        let id1 = MemoryId::from_raw(1);
        let id2 = MemoryId::from_raw(2);
        let id3 = MemoryId::from_raw(3);

        let mut set_a = IdSet::new();
        assert!(set_a.is_empty());
        assert!(set_a.insert(id3.clone()));
        assert!(set_a.insert(id1.clone()));
        assert!(set_a.insert(id2.clone()));
        assert!(!set_a.insert(id2.clone()));
        assert_eq!(set_a.len(), 3);
        assert!(set_a.contains(&id1));
        assert!(set_a.contains(&id2));
        assert!(set_a.contains(&id3));
        assert!(!set_a.contains(&MemoryId::from_raw(0)));
        assert!(!set_a.contains(&MemoryId::from_raw(4)));

        let mut set_b = IdSet::new();
        assert!(set_b.insert(id1.clone()));
        assert!(set_b.insert(id2.clone()));
        assert!(set_b.insert(id3.clone()));
        assert_eq!(set_a, set_b);

        assert!(set_a.remove(&id2));
        assert!(!set_a.remove(&id2));
        assert!(!set_a.contains(&id2));
        assert_eq!(set_a.len(), 2);
        assert_ne!(set_a, set_b);
    }

    #[test]
    fn test_bounds_map_insert_update_max_and_eq() {
        let k1 = MemoryId::from_raw(10);
        let k2 = MemoryId::from_raw(20);
        let k3 = MemoryId::from_raw(30);

        let mut m1 = BoundsMap::new();
        assert_eq!(m1.insert(k3.clone(), 300), None);
        assert_eq!(m1.insert(k1.clone(), 100), None);
        assert_eq!(m1.insert(k2.clone(), 200), None);
        assert_eq!(m1.insert(k2.clone(), 250), Some(200));

        assert_eq!(m1.get(&k1), Some(&100));
        assert_eq!(m1.get(&k2), Some(&250));
        assert_eq!(m1.get(&MemoryId::from_raw(15)), None);
        assert_eq!(m1.get_or_zero(&MemoryId::from_raw(15)), 0);

        m1.update_max(k1.clone(), 50);
        assert_eq!(m1.get_or_zero(&k1), 100);
        m1.update_max(k1.clone(), 150);
        assert_eq!(m1.get_or_zero(&k1), 150);

        let mut m2 = BoundsMap::new();
        m2.update_max(k1.clone(), 150);
        m2.update_max(k2.clone(), 250);
        m2.update_max(k3.clone(), 300);
        assert_eq!(m1, m2);
    }

    #[test]
    fn test_memory_id_ordering_namespaces_and_parents() {
        let hardcoded = MemoryId::from_raw(100);
        let generated = MemoryId::from_generated(0);
        let mut vctx =
            VerificationContext::new(CoreCallingContext::default(), false, vec![], vec![]);
        let verification = vctx.next_id();

        assert!(Namespace::Hardcoded < Namespace::Generated);
        assert!(Namespace::Generated < Namespace::Verification);
        assert!(hardcoded < generated);
        assert!(generated < verification);

        let p1 = MemoryId::from_raw(1);
        let p2 = MemoryId::from_raw(2);
        let base = MemoryId::from_raw(5);
        let with_p1 = base.prepended(p1.clone());
        let with_p2 = base.prepended(p2.clone());
        let with_p1_p2 = with_p1.prepended(p2);

        assert!(base < with_p1);
        assert!(with_p1 < with_p2);
        assert!(with_p1 < with_p1_p2);
        assert_eq!(base.cmp(&base.clone()), Ordering::Equal);
    }

    #[test]
    fn test_type_constraint_infeasible_lt_gt_bounds() {
        let mut ctx = ComputationContext::default();

        // `0 < 0` and `u64::MAX < 5` are infeasible; `Type::constraint` must return the original
        // types without panicking or inverting `U64Range` bounds.
        let (l1, r1) =
            Type::constraint(&mut ctx, JumpType::Lt, JumpWidth::W64, Type::from(0), Type::from(0))
                .expect("infeasible 0 < 0 must not error");
        assert_eq!((l1, r1), (Type::from(0), Type::from(0)));

        let (l2, r2) = Type::constraint(
            &mut ctx,
            JumpType::Lt,
            JumpWidth::W64,
            Type::from(u64::MAX),
            Type::from(5),
        )
        .expect("infeasible u64::MAX < 5 must not error");
        assert_eq!((l2, r2), (Type::from(u64::MAX), Type::from(5)));

        let (l3, r3) =
            Type::constraint(&mut ctx, JumpType::Gt, JumpWidth::W64, Type::from(0), Type::from(0))
                .expect("infeasible 0 > 0 must not error");
        assert_eq!((l3, r3), (Type::from(0), Type::from(0)));

        let (l4, r4) =
            Type::constraint(&mut ctx, JumpType::Le, JumpWidth::W64, Type::from(10), Type::from(5))
                .expect("infeasible 10 <= 5 must not error");
        assert_eq!((l4, r4), (Type::from(10), Type::from(5)));

        // When `lhs.min() < rhs.max()` holds (`2 < 5`), but `new_range_lhs = [2, 4]` contains no
        // integer compatible with `(value = 0, unknown_mask = 0b1001)` (`{0, 1, 8, 9}`),
        // `Type::constraint` must preserve the original types instead of producing an inverted
        // `U64Range`.
        let masked = Type::ScalarValue(ScalarValueData::new(0, 0b1001, 0, U64Range::new(2, 9)));
        let (l5, r5) =
            Type::constraint(&mut ctx, JumpType::Lt, JumpWidth::W64, masked.clone(), Type::from(5))
                .expect("masked-infeasible < 5 must not error");
        assert_eq!((l5, r5), (masked, Type::from(5)));
    }

    #[test]
    fn test_verify_program_core_smoke() {
        use crate::api::{BPF_ALU64, BPF_EXIT, BPF_JMP, BPF_K, BPF_MOV};

        let code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 42),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let verified = verify_program_core(
            code.clone(),
            CoreCallingContext::default(),
            &mut NullCoreVerifierLogger,
        )
        .expect("pure verify_program_core must accept valid program");
        assert_eq!(verified.code(), code.as_slice());
        assert!(verified.maps().is_empty());

        let too_many_args = CoreCallingContext {
            args: vec![Type::UNKNOWN_SCALAR; usize::from(ARGUMENT_COUNT) + 1],
            ..Default::default()
        };
        let err = verify_program_core(code, too_many_args, &mut NullCoreVerifierLogger)
            .expect_err("more than ARGUMENT_COUNT args must be rejected");
        assert_eq!(
            err,
            VerifierError {
                kind: VerifierErrorKind::InvalidRegister(ARGUMENT_COUNT + 1),
                pc: None,
            }
        );
    }

    #[test]
    fn test_stack_partial_cmp_masks_and_slots() {
        let empty1 = Stack::default();
        let empty2 = Stack::default();
        assert_eq!(empty1.partial_cmp(&empty2), Some(Ordering::Equal));

        let mut s_exact = Stack::default();
        s_exact.set(2, Type::from(5));
        s_exact.set(10, Type::from(9));
        assert_eq!(s_exact.partial_cmp(&empty1), Some(Ordering::Less));
        assert_eq!(empty1.partial_cmp(&s_exact), Some(Ordering::Greater));

        let mut s_wider = Stack::default();
        s_wider.set(2, Type::UNKNOWN_SCALAR);
        s_wider.set(10, Type::from(9));
        assert_eq!(s_exact.partial_cmp(&s_wider), Some(Ordering::Less));
        assert_eq!(s_wider.partial_cmp(&s_exact), Some(Ordering::Greater));

        let mut s_subset = Stack::default();
        s_subset.set(10, Type::UNKNOWN_SCALAR);
        assert_eq!(s_exact.partial_cmp(&s_subset), Some(Ordering::Less));
        assert_eq!(s_subset.partial_cmp(&s_exact), Some(Ordering::Greater));

        let mut s_disjoint = Stack::default();
        s_disjoint.set(5, Type::from(5));
        assert_eq!(s_exact.partial_cmp(&s_disjoint), None);
    }

    #[test]
    fn test_terminating_contexts_insert_antichain() {
        let mut tc = TerminatingContexts::default();

        let mut c_exact_1 = ComputationContext::default();
        c_exact_1.registers[1] = Type::from(1);
        tc.insert(c_exact_1.clone(), DataDependencies::default());
        assert_eq!(tc.contexts.len(), 1);

        // Inserting an incomparable state appends without removing `c_exact_1`.
        let mut c_exact_2 = ComputationContext::default();
        c_exact_2.registers[1] = Type::from(2);
        tc.insert(c_exact_2.clone(), DataDependencies::default());
        assert_eq!(tc.contexts.len(), 2);

        // Re-inserting a dominated state is a no-op.
        tc.insert(c_exact_1, DataDependencies::default());
        assert_eq!(tc.contexts.len(), 2);

        // Inserting a strictly wider state (`UNKNOWN_SCALAR`) replaces both smaller states.
        let mut c_wider = ComputationContext::default();
        c_wider.registers[1] = Type::UNKNOWN_SCALAR;
        tc.insert(c_wider, DataDependencies::default());
        assert_eq!(tc.contexts.len(), 1);
    }
}
