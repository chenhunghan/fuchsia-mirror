// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::program::*;

use crate::executor::execute;
use crate::verifier::VerifiedEbpfProgram;
use crate::{
    BPF_MAP_TYPE_ARRAY, CbpfConfig, DataWidth, EbpfError, EbpfInstruction, EbpfPtr, MapSchema, Type,
};
use derivative::Derivative;
use std::collections::HashMap;
use std::fmt::Formatter;
use std::mem::size_of;
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Trait that should be implemented for arguments passed to eBPF programs.
pub trait ProgramArgument: Into<BpfValue> {
    /// Returns eBPF type that corresponds to `Self`. Used when program argument types
    /// are checked statically.
    fn get_type() -> &'static Type;

    /// Returns eBPF type for a specific value of `Self`. For most types this is the
    /// same type that's returned by `get_type()`, but that's not always the case.
    /// In particular for scalar values this will return `Type::ScalarValue` with
    /// the actual value of the scalar and with `unknown_mask = 0`.
    fn get_value_type(&self) -> Type {
        Self::get_type().clone()
    }

    /// Returns the list of field mappings that should be applied with this argument.
    /// If not empty then `get_type()` must be a `PtrToStruct`.
    fn field_mappings() -> &'static [FieldMapping] {
        static NO_MAPPINGS: [FieldMapping; 0] = [];
        &NO_MAPPINGS
    }

    /// Returns the `StructMapping` that should be applied with this argument. Implementations
    /// should override `field_mappings()` and keep default implementation of `struct_mapping()`.
    fn struct_mapping() -> Option<StructMapping> {
        let fields = Self::field_mappings();
        if fields.is_empty() {
            return None;
        }
        Some(StructMapping {
            memory_id: match Self::get_type() {
                Type::PtrToStruct { id, .. } => id.clone(),
                _ => panic!("type must be PtrToStruct"),
            },
            fields: fields.iter().cloned().collect(),
        })
    }
}

/// Trait that should be implemented for types that can be converted from `BpfValue`.
/// Used to get a `Packet` when loading a value from the packet.
pub trait FromBpfValue<C>: Sized {
    /// # Safety
    /// Should be called only by the eBPF interpreter when executing verified eBPF code.
    unsafe fn from_bpf_value(context: &mut C, v: BpfValue) -> Self;
}

impl ProgramArgument for () {
    fn get_type() -> &'static Type {
        &Type::UNINITIALIZED
    }
}

impl<C> FromBpfValue<C> for () {
    unsafe fn from_bpf_value(_context: &mut C, _v: BpfValue) -> Self {
        unreachable!();
    }
}

impl ProgramArgument for usize {
    fn get_type() -> &'static Type {
        &Type::UNKNOWN_SCALAR
    }

    fn get_value_type(&self) -> Type {
        Type::from(*self as u64)
    }
}

/// Implements `ProgramArgument` for `EbpfPtr` when it's implemented for
/// `&'a mut T`.
impl<'a, T> ProgramArgument for EbpfPtr<'a, T>
where
    &'a mut T: ProgramArgument,
{
    fn get_type() -> &'static Type {
        <&'a mut T as ProgramArgument>::get_type()
    }

    fn field_mappings() -> &'static [FieldMapping] {
        <&'a mut T as ProgramArgument>::field_mappings()
    }

    fn struct_mapping() -> Option<StructMapping> {
        <&'a mut T as ProgramArgument>::struct_mapping()
    }
}

impl<'a, T, C> FromBpfValue<C> for &'a mut T
where
    &'a mut T: ProgramArgument,
{
    unsafe fn from_bpf_value(_context: &mut C, v: BpfValue) -> Self {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            &mut *v.as_ptr::<T>()
        }
    }
}

impl<'a, T, C> FromBpfValue<C> for &'a T
where
    &'a T: ProgramArgument,
{
    unsafe fn from_bpf_value(_context: &mut C, v: BpfValue) -> Self {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            &*v.as_ptr::<T>()
        }
    }
}

/// A strong reference to an eBPF map held for the lifetime of an eBPF linked
/// with the map. Can be converted to `BpfValue`, which is used by the program
/// to identify the map when it calls map helpers.
pub trait MapReference {
    fn schema(&self) -> &MapSchema;
    fn as_bpf_value(&self) -> BpfValue;

    /// Returns the address of the first element of this map.
    ///
    /// This will only ever be called on a map of type `BPF_MAP_TYPE_ARRAY` with at least one
    /// element.
    fn get_data_ptr(&self) -> Option<BpfValue>;
}

/// `MapReference` for `EbpfProgramContext` where maps are not used.
pub enum NoMap {}

impl MapReference for NoMap {
    fn schema(&self) -> &MapSchema {
        unreachable!()
    }
    fn as_bpf_value(&self) -> BpfValue {
        unreachable!()
    }
    fn get_data_ptr(&self) -> Option<BpfValue> {
        unreachable!()
    }
}

pub trait StaticHelperSet: EbpfProgramContext {
    /// Returns the set of helpers available to the program.
    fn helpers() -> &'static HelperSet<Self>;
}

pub trait EbpfProgramContext: 'static + Sized {
    /// Context for an invocation of an eBPF program.
    type RunContext<'a>;

    /// Packet used by the program.
    type Packet<'a>: Packet + FromBpfValue<Self::RunContext<'a>>;

    /// Arguments passed to the program
    type Arg1<'a>: ProgramArgument;
    type Arg2<'a>: ProgramArgument;
    type Arg3<'a>: ProgramArgument;
    type Arg4<'a>: ProgramArgument;
    type Arg5<'a>: ProgramArgument;

    /// Type used to reference eBPF maps for the lifetime of a program.
    type Map: MapReference;

    /// Returns the set of struct mappings that should be applied when linking a program. The
    /// default implementation collects mappings from all arguments.
    fn struct_mappings() -> StructMappings {
        let mut mappings = StructMappings::new();
        if let Some(mapping) = Self::Arg1::struct_mapping() {
            mappings.push(mapping);
        }
        if let Some(mapping) = Self::Arg2::struct_mapping() {
            mappings.push(mapping);
        }
        if let Some(mapping) = Self::Arg3::struct_mapping() {
            mappings.push(mapping);
        }
        if let Some(mapping) = Self::Arg4::struct_mapping() {
            mappings.push(mapping);
        }
        if let Some(mapping) = Self::Arg5::struct_mapping() {
            mappings.push(mapping);
        }
        mappings
    }
}

/// Trait that should be implemented by packets passed to eBPF programs.
pub trait Packet {
    fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue>;
}

impl Packet for () {
    fn load(&self, _offset: i32, _width: DataWidth) -> Option<BpfValue> {
        None
    }
}

/// Simple `Packet` implementation for packets that can be accessed directly.
impl<P: IntoBytes + Immutable> Packet for &P {
    fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue> {
        let data = (*self).as_bytes();
        if offset < 0 || offset as usize >= data.len() {
            return None;
        }
        let slice = &data[(offset as usize)..];
        match width {
            DataWidth::U8 => u8::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U16 => u16::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U32 => u32::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U64 => u64::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
        }
    }
}

/// A context for a BPF program that's compatible with eBPF and cBPF.
pub trait BpfProgramContext: 'static + Sized {
    type RunContext<'a>;
    type Packet<'a>: ProgramArgument + Packet + FromBpfValue<Self::RunContext<'a>>;
    type Map: MapReference;
    const CBPF_CONFIG: &'static CbpfConfig;

    fn get_arg_types() -> Vec<Type> {
        vec![<Self::Packet<'_> as ProgramArgument>::get_type().clone()]
    }
}

impl<T: BpfProgramContext> EbpfProgramContext for T {
    type RunContext<'a> = <T as BpfProgramContext>::RunContext<'a>;
    type Packet<'a> = T::Packet<'a>;
    type Arg1<'a> = T::Packet<'a>;
    type Arg2<'a> = ();
    type Arg3<'a> = ();
    type Arg4<'a> = ();
    type Arg5<'a> = ();
    type Map = T::Map;
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub struct BpfValue(u64);

static_assertions::const_assert_eq!(size_of::<BpfValue>(), size_of::<*const u8>());

impl Default for BpfValue {
    fn default() -> Self {
        Self::from(0)
    }
}

impl From<()> for BpfValue {
    fn from(_v: ()) -> Self {
        Self(0)
    }
}

impl From<i32> for BpfValue {
    fn from(v: i32) -> Self {
        Self((v as u32) as u64)
    }
}

impl From<u8> for BpfValue {
    fn from(v: u8) -> Self {
        Self::from(v as u64)
    }
}

impl From<u16> for BpfValue {
    fn from(v: u16) -> Self {
        Self::from(v as u64)
    }
}

impl From<u32> for BpfValue {
    fn from(v: u32) -> Self {
        Self::from(v as u64)
    }
}
impl From<u64> for BpfValue {
    fn from(v: u64) -> Self {
        Self(v)
    }
}
impl From<i64> for BpfValue {
    fn from(v: i64) -> Self {
        Self(v as u64)
    }
}

impl From<usize> for BpfValue {
    fn from(v: usize) -> Self {
        Self(v as u64)
    }
}

impl<T> From<*const T> for BpfValue {
    fn from(v: *const T) -> Self {
        Self(v as u64)
    }
}

impl<T> From<*mut T> for BpfValue {
    fn from(v: *mut T) -> Self {
        Self(v as u64)
    }
}

impl<T> From<&'_ T> for BpfValue {
    fn from(v: &'_ T) -> Self {
        Self((v as *const T) as u64)
    }
}

impl<T> From<&'_ mut T> for BpfValue {
    fn from(v: &'_ mut T) -> Self {
        Self((v as *const T) as u64)
    }
}

impl<'a, T> From<EbpfPtr<'a, T>> for BpfValue {
    fn from(value: EbpfPtr<'a, T>) -> Self {
        value.ptr().into()
    }
}

impl TryFrom<BpfValue> for u8 {
    type Error = core::num::TryFromIntError;
    fn try_from(v: BpfValue) -> Result<Self, Self::Error> {
        v.0.try_into()
    }
}

impl TryFrom<BpfValue> for u16 {
    type Error = core::num::TryFromIntError;
    fn try_from(v: BpfValue) -> Result<Self, Self::Error> {
        v.0.try_into()
    }
}

impl TryFrom<BpfValue> for u32 {
    type Error = core::num::TryFromIntError;
    fn try_from(v: BpfValue) -> Result<Self, Self::Error> {
        v.0.try_into()
    }
}

impl From<BpfValue> for u64 {
    fn from(v: BpfValue) -> u64 {
        v.0
    }
}

impl From<BpfValue> for usize {
    fn from(v: BpfValue) -> usize {
        v.0 as usize
    }
}

impl BpfValue {
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    pub fn as_u8(&self) -> u8 {
        self.0 as u8
    }

    pub fn as_u16(&self) -> u16 {
        self.0 as u16
    }

    pub fn as_u32(&self) -> u32 {
        self.0 as u32
    }

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn as_usize(&self) -> usize {
        self.0 as usize
    }

    pub fn as_ptr<T>(&self) -> *mut T {
        self.0 as *mut T
    }
}

impl From<BpfValue> for () {
    fn from(_v: BpfValue) -> Self {
        ()
    }
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct EbpfHelperImpl<C: EbpfProgramContext>(
    pub  for<'a> extern "C" fn(
        &mut C::RunContext<'a>,
        BpfValue,
        BpfValue,
        BpfValue,
        BpfValue,
        BpfValue,
    ) -> BpfValue,
);

impl<C: EbpfProgramContext> std::fmt::Debug for EbpfHelperImpl<C> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_tuple("EbpfHelperImpl").field(&(self.0 as *const ())).finish()
    }
}

impl<C: EbpfProgramContext> EbpfHelperImpl<C> {
    pub fn fn_addr(&self) -> u64 {
        self.0 as *const () as usize as u64
    }
}

/// Stores set of helpers for an eBPF program.
#[derive(Derivative)]
#[derivative(Clone(bound = ""), Debug(bound = ""), Default(bound = ""))]
pub struct HelperSet<C: EbpfProgramContext> {
    helpers: HashMap<u32, EbpfHelperImpl<C>>,
}

impl<C: EbpfProgramContext> FromIterator<(u32, EbpfHelperImpl<C>)> for HelperSet<C> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (u32, EbpfHelperImpl<C>)>,
    {
        Self { helpers: HashMap::from_iter(iter) }
    }
}

impl<C: EbpfProgramContext> HelperSet<C> {
    pub fn new(iter: impl IntoIterator<Item = (u32, EbpfHelperImpl<C>)>) -> Self {
        Self::from_iter(iter)
    }

    /// Returns the helper with the specified `id`.
    pub fn get(&self, id: u32) -> Option<&EbpfHelperImpl<C>> {
        self.helpers.get(&id)
    }

    /// Returns an iterator over all registered helpers and their IDs.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &EbpfHelperImpl<C>)> {
        self.helpers.iter().map(|(&id, helper)| (id, helper))
    }
}

pub trait ArgumentTypeChecker<C: EbpfProgramContext>: Sized {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError>;
    fn run_time_check<'a>(
        &self,
        arg1: &C::Arg1<'a>,
        arg2: &C::Arg2<'a>,
        arg3: &C::Arg3<'a>,
        arg4: &C::Arg4<'a>,
        arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError>;
}

pub struct StaticTypeChecker();

impl<C: EbpfProgramContext> ArgumentTypeChecker<C> for StaticTypeChecker {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError> {
        let arg_types = [
            C::Arg1::get_type().clone(),
            C::Arg2::get_type().clone(),
            C::Arg3::get_type().clone(),
            C::Arg4::get_type().clone(),
            C::Arg5::get_type().clone(),
        ];
        if let Err(i) = check_argument_types(program.args(), &arg_types) {
            let verified_type = program.args().get(i).unwrap_or(&Type::UNINITIALIZED);
            return Err(EbpfError::ProgramLinkError(format!(
                "Type of argument {} doesn't match. Verified type: {:?}. Context type: {:?}",
                i + 1,
                verified_type,
                arg_types[i],
            )));
        }

        Ok(Self())
    }

    fn run_time_check<'a>(
        &self,
        _arg1: &C::Arg1<'a>,
        _arg2: &C::Arg2<'a>,
        _arg3: &C::Arg3<'a>,
        _arg4: &C::Arg4<'a>,
        _arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError> {
        // No-op since argument types were checked in `link()`.
        Ok(())
    }
}

pub struct DynamicTypeChecker {
    types: Vec<Type>,
}

impl<C: EbpfProgramContext> ArgumentTypeChecker<C> for DynamicTypeChecker {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError> {
        Ok(Self { types: program.args().to_vec() })
    }

    fn run_time_check<'a>(
        &self,
        arg1: &C::Arg1<'a>,
        arg2: &C::Arg2<'a>,
        arg3: &C::Arg3<'a>,
        arg4: &C::Arg4<'a>,
        arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError> {
        let arg_types = [
            arg1.get_value_type(),
            arg2.get_value_type(),
            arg3.get_value_type(),
            arg4.get_value_type(),
            arg5.get_value_type(),
        ];
        if let Err(i) = check_argument_types(&self.types, &arg_types) {
            let verified_type = self.types.get(i).unwrap_or(&Type::UNINITIALIZED);
            return Err(EbpfError::ProgramLinkError(format!(
                "Type of argument {} doesn't match. Verified type: {:?}. Value type: {:?}",
                i + 1,
                verified_type,
                arg_types[i],
            )));
        }

        Ok(())
    }
}

/// Trampoline invoked by JIT-compiled legacy packet load instructions (`BPF_LD | BPF_ABS` and
/// `BPF_LD | BPF_IND`).
///
/// # Safety
/// `run_context` must be a valid pointer to `C::RunContext<'_>`, `skb_r6` must be a valid
/// packet value in `R6` as verified by the eBPF verifier, and `out_value` must be a valid pointer
/// to an 8-byte output slot on the JIT stack.
pub unsafe extern "C" fn packet_load_trampoline<C: EbpfProgramContext>(
    run_context: *mut (),
    skb_r6: u64,
    reg_offset: i32,
    imm_offset: i32,
    width_bytes: u32,
    out_value: *mut u64,
) -> u64 {
    // SAFETY: `run_context` is a valid pointer to `C::RunContext<'_>` passed by the JIT
    // caller, and `skb_r6` holds the verified packet value in `R6`.
    let packet = unsafe {
        let ctx = &mut *(run_context as *mut C::RunContext<'_>);
        C::Packet::from_bpf_value(ctx, BpfValue::from(skb_r6))
    };
    let load_packet = || -> Option<u64> {
        let width = match width_bytes {
            1 => DataWidth::U8,
            2 => DataWidth::U16,
            4 => DataWidth::U32,
            8 => DataWidth::U64,
            _ => return None,
        };
        let offset = reg_offset.checked_add(imm_offset)?;
        Some(packet.load(offset, width)?.as_u64())
    };

    let result = load_packet();
    // SAFETY: `out_value` points to a valid, aligned 8-byte output slot on the JIT stack.
    unsafe {
        *out_value = result.unwrap_or(0);
    }
    u64::from(result.is_some())
}

/// Collects process-local linkage metadata (`struct_mappings`, `helpers`, `maps`, and
/// `packet_load_fn_addr`) required to link an eBPF program for execution with context `C` in the
/// current address space.
pub fn collect_program_linkage<C: EbpfProgramContext + StaticHelperSet>(
    maps: &[C::Map],
) -> ProgramLinkageInfo {
    let mut struct_mappings = Vec::new();
    for mapping in C::struct_mappings() {
        for field in mapping.fields {
            struct_mappings.push(StructFieldMapping {
                memory_id: mapping.memory_id.clone(),
                source_offset: field.source_offset,
                target_offset: field.target_offset,
            });
        }
    }
    let helpers = C::helpers()
        .iter()
        .map(|(id, helper)| HelperLinkage { id, fn_addr: helper.fn_addr() })
        .collect();
    let maps = maps
        .iter()
        .map(|map| {
            let schema = map.schema();
            let data_addr = if schema.map_type == BPF_MAP_TYPE_ARRAY && schema.max_entries > 0 {
                map.get_data_ptr().map(|v| v.as_u64())
            } else {
                None
            };
            MapLinkage { map_addr: map.as_bpf_value().as_u64(), data_addr }
        })
        .collect();
    ProgramLinkageInfo {
        struct_mappings,
        helpers,
        maps,
        packet_load_fn_addr: Some(packet_load_trampoline::<C> as *const () as usize as u64),
    }
}

#[derive(Debug)]
enum ProgramBackend {
    Interpreted(Vec<EbpfInstruction>),
    #[cfg(target_os = "fuchsia")]
    Native(crate::jit::ExecutableMemory),
}

/// An abstraction over an eBPF program and its registered helper functions.
pub struct EbpfProgram<C: EbpfProgramContext, T: ArgumentTypeChecker<C> = StaticTypeChecker> {
    /// Execution backend (interpreter instructions or compiled native code).
    backend: ProgramBackend,

    /// List of references to the maps used by the program. This field is not used directly,
    /// but it's kept here to ensure that the maps outlive the program.
    _maps: Vec<C::Map>,

    type_checker: T,
}

impl<C: EbpfProgramContext, T: ArgumentTypeChecker<C>> std::fmt::Debug for EbpfProgram<C, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("EbpfProgram").field("backend", &self.backend).finish()
    }
}

impl<C: EbpfProgramContext, T: ArgumentTypeChecker<C>> EbpfProgram<C, T> {
    #[inline(always)]
    fn execute_backend<'a>(&self, run_context: &mut C::RunContext<'a>, args: &[BpfValue]) -> u64
    where
        C: StaticHelperSet,
    {
        match &self.backend {
            ProgramBackend::Interpreted(code) => {
                execute(&code[..], C::helpers(), run_context, args)
            }
            #[cfg(target_os = "fuchsia")]
            ProgramBackend::Native(native) => {
                // SAFETY: `native` holds a valid executable mapping compiled for `C` whose maps
                // and helpers are kept alive by `self._maps` and `C::helpers()`, and `run_context`
                // and `args` have been validated by the program's `ArgumentTypeChecker`.
                unsafe { native.execute(run_context as *mut C::RunContext<'a> as *mut (), args) }
            }
        }
    }
}

impl<C, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    C: StaticHelperSet,
    C: for<'a> EbpfProgramContext<Arg2<'a> = (), Arg3<'a> = (), Arg4<'a> = (), Arg5<'a> = ()>,
{
    pub fn run_with_1_argument<'a>(
        &self,
        run_context: &mut C::RunContext<'a>,
        arg1: C::Arg1<'a>,
    ) -> u64 {
        self.type_checker
            .run_time_check(&arg1, &(), &(), &(), &())
            .expect("Failed argument type check");
        self.execute_backend(run_context, &[arg1.into()])
    }
}

impl<C, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    C: StaticHelperSet,
    C: for<'a> EbpfProgramContext<Arg3<'a> = (), Arg4<'a> = (), Arg5<'a> = ()>,
{
    pub fn run_with_2_arguments<'a>(
        &self,
        run_context: &mut C::RunContext<'a>,
        arg1: C::Arg1<'a>,
        arg2: C::Arg2<'a>,
    ) -> u64 {
        self.type_checker
            .run_time_check(&arg1, &arg2, &(), &(), &())
            .expect("Failed argument type check");
        self.execute_backend(run_context, &[arg1.into(), arg2.into()])
    }
}

impl<C: StaticHelperSet, T: ArgumentTypeChecker<C>> EbpfProgram<C, T> {
    pub fn run_with_5_arguments<'a>(
        &self,
        run_context: &mut C::RunContext<'a>,
        arg1: C::Arg1<'a>,
        arg2: C::Arg2<'a>,
        arg3: C::Arg3<'a>,
        arg4: C::Arg4<'a>,
        arg5: C::Arg5<'a>,
    ) -> u64 {
        self.type_checker
            .run_time_check(&arg1, &arg2, &arg3, &arg4, &arg5)
            .expect("Failed argument type check");
        self.execute_backend(
            run_context,
            &[arg1.into(), arg2.into(), arg3.into(), arg4.into(), arg5.into()],
        )
    }
}

impl<C: BpfProgramContext, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    C: BpfProgramContext + StaticHelperSet,
    C: for<'a> EbpfProgramContext<Arg2<'a> = (), Arg3<'a> = (), Arg4<'a> = (), Arg5<'a> = ()>,
{
    /// Executes the current program on the specified `packet`.
    /// The program receives a pointer to the `packet` and the size of the packet as the first
    /// two arguments.
    pub fn run<'a>(
        &self,
        run_context: &mut <C as EbpfProgramContext>::RunContext<'a>,
        packet: <C as EbpfProgramContext>::Arg1<'a>,
    ) -> u64 {
        self.run_with_1_argument(run_context, packet)
    }
}

#[cfg(target_os = "fuchsia")]
impl<C: EbpfProgramContext> EbpfProgram<C> {
    /// Maps a pre-compiled JIT VMO into the current process's root VMAR and returns a runnable
    /// `EbpfProgram<C>`.
    ///
    /// # Safety
    /// `compiled_vmo` must contain valid, verified, and linked native machine code whose argument
    /// types match context `C` and `maps`.
    pub unsafe fn from_compiled_vmo(
        compiled_vmo: zx::Vmo,
        maps: Vec<C::Map>,
    ) -> Result<Self, EbpfError> {
        // SAFETY: The caller guarantees `compiled_vmo` contains valid, verified, and linked native
        // machine code matching context `C` and `maps`.
        unsafe {
            Self::from_compiled_vmo_with_type_checker(compiled_vmo, maps, StaticTypeChecker())
        }
    }
}

#[cfg(target_os = "fuchsia")]
impl<C: EbpfProgramContext, T: ArgumentTypeChecker<C>> EbpfProgram<C, T> {
    /// Maps a pre-compiled JIT VMO into the current process's root VMAR and returns a runnable
    /// `EbpfProgram<C, T>`.
    ///
    /// # Safety
    /// `compiled_vmo` must contain valid, verified, and linked native machine code for context `C`
    /// and `maps`, and `type_checker` must enforce the argument types verified for the program.
    unsafe fn from_compiled_vmo_with_type_checker(
        compiled_vmo: zx::Vmo,
        maps: Vec<C::Map>,
        type_checker: T,
    ) -> Result<Self, EbpfError> {
        let native = crate::jit::ExecutableMemory::from_vmo(compiled_vmo)?;
        Ok(Self { backend: ProgramBackend::Native(native), _maps: maps, type_checker })
    }
}

impl<C: EbpfProgramContext> EbpfProgram<C> {
    /// Creates an interpreted `EbpfProgram<C>` from instructions that have already been verified
    /// and linked.
    ///
    /// # Safety
    /// `code` must have been verified and linked for context `C` and `maps`.
    pub unsafe fn from_linked_instructions(code: Vec<EbpfInstruction>, maps: Vec<C::Map>) -> Self {
        Self {
            backend: ProgramBackend::Interpreted(code),
            _maps: maps,
            type_checker: StaticTypeChecker(),
        }
    }
}

/// Rewrites the code to ensure mapped fields are correctly handled. Returns
/// runnable `EbpfProgram<C>`.
fn link_program_internal<C, T>(
    program: &VerifiedEbpfProgram,
    maps: Vec<C::Map>,
) -> Result<EbpfProgram<C, T>, EbpfError>
where
    C: EbpfProgramContext + StaticHelperSet,
    T: ArgumentTypeChecker<C>,
{
    let type_checker = T::link(program)?;
    if maps.len() != program.maps().len() {
        return Err(EbpfError::ProgramLinkError(format!(
            "Map count mismatch: expected {}, got {}",
            program.maps().len(),
            maps.len()
        )));
    }
    for (i, map) in maps.iter().enumerate() {
        if *map.schema() != program.maps()[i] {
            return Err(EbpfError::ProgramLinkError(format!(
                "Map schema mismatch at index {i}: expected {:?}, got {:?}",
                program.maps()[i],
                map.schema()
            )));
        }
    }

    let linkage = collect_program_linkage::<C>(&maps);
    let code = link_instructions(program, &linkage)?;

    Ok(EbpfProgram { backend: ProgramBackend::Interpreted(code), _maps: maps, type_checker })
}

/// Rewrites the code to ensure mapped fields are correctly handled. Returns
/// runnable `EbpfProgram<C>`.
pub fn link_program<C>(
    program: &VerifiedEbpfProgram,
    maps: Vec<C::Map>,
) -> Result<EbpfProgram<C>, EbpfError>
where
    C: EbpfProgramContext + StaticHelperSet,
{
    link_program_internal::<C, StaticTypeChecker>(program, maps)
}

/// Same as above, but allows to check argument types in runtime instead of in link time.
pub fn link_program_dynamic<C>(
    program: &VerifiedEbpfProgram,
    maps: Vec<C::Map>,
) -> Result<EbpfProgram<C, DynamicTypeChecker>, EbpfError>
where
    C: EbpfProgramContext + StaticHelperSet,
{
    link_program_internal::<C, DynamicTypeChecker>(program, maps)
}

#[macro_export]
macro_rules! static_helper_set {
    ($context:ty, $value:expr) => {
        impl $crate::StaticHelperSet for $context {
            fn helpers() -> &'static $crate::HelperSet<$context> {
                static HELPERS: std::sync::LazyLock<$crate::HelperSet<$context>> =
                    std::sync::LazyLock::new(|| $value);
                &HELPERS
            }
        }
    };
}

#[macro_export]
macro_rules! empty_static_helper_set {
    ($context:ty) => {
        $crate::static_helper_set!($context, $crate::HelperSet::default());
    };
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::api::*;
    use crate::conformance::test::parse_asm;
    use crate::{
        CallingContext, FieldDescriptor, FieldType, FunctionSignatureBuilder, HelperDefinition,
        LinkError, MapFlags, MapLinkageError, MemoryId, NullVerifierLogger, PointerOrigin,
        ProgramArgument, StructAccess, StructDescriptor, StructMappingError, Type, VerifierError,
        VerifierErrorKind, new_memory_id, verify_program,
    };
    use linux_uapi::{bpf_map_type_BPF_MAP_TYPE_ARRAY, bpf_map_type_BPF_MAP_TYPE_HASH};
    use std::mem::offset_of;
    use std::sync::{Arc, LazyLock};
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct TestArgument {
        // A field that should not be writable by the program.
        pub read_only_field: u32,
        pub _padding1: u32,
        /// Pointer to an array.
        pub data: u64,
        /// End of the array.
        pub data_end: u64,
        // A field that can be updated by the program.
        pub mutable_field: u32,
        pub _padding2: u32,
    }

    static TEST_ARG_TYPE: LazyLock<Type> = LazyLock::new(|| {
        let data_memory_id = new_memory_id();
        let descriptor = Box::new(StructDescriptor {
            fields: vec![
                FieldDescriptor {
                    offset: offset_of!(TestArgument, read_only_field),
                    field_type: FieldType::Scalar { size: 4 },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, data),
                    field_type: FieldType::PtrToArray {
                        is_32_bit: false,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, data_end),
                    field_type: FieldType::PtrToEndArray {
                        is_32_bit: false,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, mutable_field),
                    field_type: FieldType::MutableScalar { size: 4 },
                },
            ],
        });

        Type::PtrToStruct { id: new_memory_id(), offset: 0.into(), descriptor }
    });

    impl Default for TestArgument {
        fn default() -> Self {
            Self {
                read_only_field: 1,
                _padding1: 0,
                data: 0,
                data_end: 0,
                mutable_field: 2,
                _padding2: 0,
            }
        }
    }

    impl TestArgument {
        fn from_data(data: &[u64]) -> Self {
            let ptr_range = data.as_ptr_range();
            Self {
                data: ptr_range.start as u64,
                data_end: ptr_range.end as u64,
                ..Default::default()
            }
        }
    }

    impl ProgramArgument for &'_ mut TestArgument {
        fn get_type() -> &'static Type {
            &*TEST_ARG_TYPE
        }
    }

    // A version of TestArgument with 32-bit remapped pointers. It's used to define struct layout
    // for eBPF programs, but not used in the Rust code directly.
    #[repr(C)]
    struct TestArgument32 {
        pub read_only_field: u32,
        pub data: u32,
        pub data_end: u32,
        pub mutable_field: u32,
    }

    #[repr(C)]
    struct TestArgument32BitMapped(TestArgument);

    static TEST_ARG_32_BIT_MEMORY_ID: LazyLock<MemoryId> = LazyLock::new(|| new_memory_id());
    static TEST_ARG_32_BIT_TYPE: LazyLock<Type> = LazyLock::new(|| {
        let data_memory_id = new_memory_id();
        let descriptor = Box::new(StructDescriptor {
            fields: vec![
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, read_only_field),
                    field_type: FieldType::Scalar { size: 4 },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, data),
                    field_type: FieldType::PtrToArray {
                        is_32_bit: true,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, data_end),
                    field_type: FieldType::PtrToEndArray {
                        is_32_bit: true,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, mutable_field),
                    field_type: FieldType::MutableScalar { size: 4 },
                },
            ],
        });

        Type::PtrToStruct { id: TEST_ARG_32_BIT_MEMORY_ID.clone(), offset: 0.into(), descriptor }
    });

    impl ProgramArgument for &'_ mut TestArgument32BitMapped {
        fn get_type() -> &'static Type {
            &*TEST_ARG_32_BIT_TYPE
        }

        fn field_mappings() -> &'static [FieldMapping] {
            static FIELD_MAPPINGS: [FieldMapping; 3] = [
                FieldMapping {
                    source_offset: offset_of!(TestArgument32, data),
                    target_offset: offset_of!(TestArgument, data),
                },
                FieldMapping {
                    source_offset: offset_of!(TestArgument32, data_end),
                    target_offset: offset_of!(TestArgument, data_end),
                },
                FieldMapping {
                    source_offset: offset_of!(TestArgument32, mutable_field),
                    target_offset: offset_of!(TestArgument, mutable_field),
                },
            ];
            &FIELD_MAPPINGS
        }
    }

    struct TestMap(MapSchema);

    impl MapReference for TestMap {
        fn schema(&self) -> &MapSchema {
            &self.0
        }
        fn as_bpf_value(&self) -> BpfValue {
            BpfValue::from(0x1000u64)
        }
        fn get_data_ptr(&self) -> Option<BpfValue> {
            assert_eq!(self.0.map_type, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
            assert!(self.0.max_entries > 0);
            Some(BpfValue::from(0x2000u64))
        }
    }

    struct TestEbpfProgramContext {}

    impl EbpfProgramContext for TestEbpfProgramContext {
        type RunContext<'a> = ();

        type Packet<'a> = ();
        type Arg1<'a> = &'a mut TestArgument;
        type Arg2<'a> = ();
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();

        type Map = TestMap;
    }

    empty_static_helper_set!(TestEbpfProgramContext);

    fn test_vmex_resource() -> &'static zx::Resource {
        static TEST_VMEX_RESOURCE: std::sync::LazyLock<zx::Resource> =
            std::sync::LazyLock::new(|| {
                let (client, server) = zx::Channel::create();
                fdio::service_connect("/svc/fuchsia.kernel.VmexResource", server)
                    .expect("failed to connect to /svc/fuchsia.kernel.VmexResource");
                let vmex = fidl_fuchsia_kernel::VmexResourceSynchronousProxy::new(client);
                vmex.get(zx::MonotonicInstant::INFINITE)
                    .expect("failed to get VmexResource from fuchsia.kernel.VmexResource")
            });
        &TEST_VMEX_RESOURCE
    }

    pub(crate) fn link_program_jit_for_test<C, T>(
        program: &VerifiedEbpfProgram,
        maps: Vec<C::Map>,
    ) -> Result<EbpfProgram<C, T>, EbpfError>
    where
        C: EbpfProgramContext + StaticHelperSet,
        T: ArgumentTypeChecker<C>,
    {
        let type_checker = T::link(program)?;
        let linkage = collect_program_linkage::<C>(&maps);
        let code = link_instructions(program, &linkage)?;
        let vmo = crate::jit::compile_jit_vmo(&code, &linkage, test_vmex_resource())?;
        // SAFETY: `vmo` was just compiled from verified `program` linked against `C` and `maps`,
        // and `type_checker` was validated via `T::link(program)` above.
        unsafe { EbpfProgram::from_compiled_vmo_with_type_checker(vmo, maps, type_checker) }
    }

    // Prepares both an interpreter program and (when supported on the target) a JIT-compiled
    // program so tests exercise both execution backends.
    fn link_test_programs_with_context<C>(
        program: &VerifiedEbpfProgram,
    ) -> Result<Vec<EbpfProgram<C>>, EbpfError>
    where
        C: EbpfProgramContext + StaticHelperSet,
    {
        let mut programs = vec![link_program::<C>(program, vec![])?];
        if crate::jit::IS_SUPPORTED {
            programs.push(link_program_jit_for_test::<C, StaticTypeChecker>(program, vec![])?);
        }
        Ok(programs)
    }

    fn link_test_programs_with_maps<C>(
        program: &VerifiedEbpfProgram,
        maps: Vec<C::Map>,
    ) -> Result<Vec<EbpfProgram<C>>, EbpfError>
    where
        C: EbpfProgramContext + StaticHelperSet,
        C::Map: Clone,
    {
        let mut programs = vec![link_program::<C>(program, maps.clone())?];
        if crate::jit::IS_SUPPORTED {
            programs.push(link_program_jit_for_test::<C, StaticTypeChecker>(program, maps)?);
        }
        Ok(programs)
    }

    fn initialize_test_program(
        code: Vec<EbpfInstruction>,
    ) -> Result<EbpfProgram<TestEbpfProgramContext>, EbpfError> {
        let verified_program = verify_program(
            code,
            CallingContext { args: vec![TEST_ARG_TYPE.clone()], ..Default::default() },
            &mut NullVerifierLogger,
        )?;
        link_program(&verified_program, vec![])
    }

    fn initialize_test_programs(
        code: Vec<EbpfInstruction>,
    ) -> Result<Vec<EbpfProgram<TestEbpfProgramContext>>, EbpfError> {
        let verified_program = verify_program(
            code,
            CallingContext { args: vec![TEST_ARG_TYPE.clone()], ..Default::default() },
            &mut NullVerifierLogger,
        )?;
        link_test_programs_with_context(&verified_program)
    }

    struct TestEbpfProgramContext32BitMapped {}

    impl EbpfProgramContext for TestEbpfProgramContext32BitMapped {
        type RunContext<'a> = ();

        type Packet<'a> = ();
        type Arg1<'a> = &'a mut TestArgument32BitMapped;
        type Arg2<'a> = ();
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();

        type Map = NoMap;
    }

    empty_static_helper_set!(TestEbpfProgramContext32BitMapped);

    fn initialize_test_programs_for_32bit_arg(
        code: Vec<EbpfInstruction>,
    ) -> Result<Vec<EbpfProgram<TestEbpfProgramContext32BitMapped>>, EbpfError> {
        let verified_program = verify_program(
            code,
            CallingContext { args: vec![TEST_ARG_32_BIT_TYPE.clone()], ..Default::default() },
            &mut NullVerifierLogger,
        )?;
        link_test_programs_with_context(&verified_program)
    }

    #[test]
    fn test_data_end() {
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+8]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        for program in initialize_test_programs(parse_asm(program)).expect("load") {
            let v = [42];
            let mut data = TestArgument::from_data(&v[..]);
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
        }
    }

    #[test]
    fn test_past_data_end() {
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+8]
        # ensure data contains at least 4 bytes
        mov %r3, %r1
        add %r3, 0x4
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::OutOfBoundsMemoryAccess,
                pc: Some(6),
            })
        );
    }

    #[test]
    fn test_mapping() {
        let program = r#"
          # Return `TestArgument32.mutable_field`
          ldxw %r0, [%r1+12]
          exit
        "#;
        for program in initialize_test_programs_for_32bit_arg(parse_asm(program)).expect("load") {
            let mut data = TestArgument32BitMapped(TestArgument::default());
            assert_eq!(
                program.run_with_1_argument(&mut (), &mut data),
                data.0.mutable_field as u64
            );
        }
    }

    #[test]
    fn test_mapping_partial_load() {
        // Verify that we can access middle of a remapped scalar field.
        let program = r#"
          # Returns two upper bytes of `TestArgument32.mutable_filed`
          ldxh %r0, [%r1+14]
          exit
        "#;
        for program in initialize_test_programs_for_32bit_arg(parse_asm(program)).expect("load") {
            let mut data = TestArgument32BitMapped(TestArgument::default());
            data.0.mutable_field = 0x12345678;
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), 0x1234 as u64);
        }
    }

    #[test]
    fn test_mapping_ptr() {
        let program = r#"
        mov %r0, 0
        # Load data and data_end as 32 bits pointers in TestArgument32
        ldxw %r2, [%r1+8]
        ldxw %r1, [%r1+4]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        for program in initialize_test_programs_for_32bit_arg(parse_asm(program)).expect("load") {
            let v = [42];
            let mut data = TestArgument32BitMapped(TestArgument::from_data(&v[..]));
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
        }
    }

    #[test]
    fn test_mapping_with_offset() {
        let program = r#"
        mov %r0, 0
        add %r1, 0x8
        # Load data and data_end as 32 bits pointers in TestArgument32
        ldxw %r2, [%r1]
        ldxw %r1, [%r1-4]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        for program in initialize_test_programs_for_32bit_arg(parse_asm(program)).expect("load") {
            let v = [42];
            let mut data = TestArgument32BitMapped(TestArgument::from_data(&v[..]));
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
        }
    }

    #[test]
    fn test_ptr_diff() {
        let program = r#"
          mov %r0, %r1
          add %r0, 0x2
          # Substract 2 ptr to memory
          sub %r0, %r1

          mov %r2, %r10
          add %r2, 0x3
          # Substract 2 ptr to stack
          sub %r2, %r10
          add %r0, %r2

          ldxdw %r2, [%r1+16]
          ldxdw %r1, [%r1+8]
          # Substract ptr to array and ptr to array end
          sub %r2, %r1
          add %r0, %r2

          mov %r2, %r1
          add %r2, 0x4
          # Substract 2 ptr to array
          sub %r2, %r1
          add %r0, %r2

          exit
        "#;
        let code = parse_asm(program);

        for program in initialize_test_programs(code).expect("load") {
            let v = [42];
            let mut data = TestArgument::from_data(&v[..]);
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), 17);
        }
    }

    #[test]
    fn test_invalid_packet_load() {
        let program = r#"
        mov %r6, %r2
        mov %r0, 0
        ldpw
        exit
        "#;
        let args = vec![
            Type::PtrToMemory {
                id: new_memory_id(),
                offset: 0.into(),
                buffer_size: 16,
                origin: PointerOrigin::KernelReadOnly,
            },
            Type::PtrToMemory {
                id: new_memory_id(),
                offset: 0.into(),
                buffer_size: 16,
                origin: PointerOrigin::KernelReadOnly,
            },
        ];
        let verify_result = verify_program(
            parse_asm(program),
            CallingContext { args, ..Default::default() },
            &mut NullVerifierLogger,
        );

        assert_eq!(
            verify_result.expect_err("validation should fail"),
            VerifierError { kind: VerifierErrorKind::RegisterNotPacket(6), pc: Some(2) }
        );
    }

    #[test]
    fn test_invalid_field_size() {
        // Load with a field size too large fails validation.
        let program = r#"
          ldxdw %r0, [%r1]
          exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::InvalidStructFieldAccess,
                pc: Some(0),
            })
        );
    }

    #[test]
    fn test_unknown_field() {
        // Load outside of the know fields fails validation.
        let program = r#"
          ldxw %r0, [%r1 + 4]
          exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::InvalidStructFieldAccess,
                pc: Some(0),
            })
        );
    }

    #[test]
    fn test_partial_ptr_field() {
        // Partial loads of ptr fields are not allowed.
        let program = r#"
          ldxw %r0, [%r1 + 8]
          exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::InvalidStructFieldAccess,
                pc: Some(0),
            })
        );
    }

    #[test]
    fn test_readonly_field() {
        // Store to a read only field fails validation.
        let program = r#"
          stw [%r1], 0x42
          exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::WriteToReadOnlyField,
                pc: Some(0),
            })
        );
    }

    #[test]
    fn test_store_mutable_field() {
        // Store to a mutable field is allowed.
        let program = r#"
          stw [%r1 + 24], 0x42
          mov %r0, 1
          exit
        "#;
        for program in initialize_test_programs(parse_asm(program)).expect("load") {
            let mut data = TestArgument::default();
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), 1);
            assert_eq!(data.mutable_field, 0x42);
        }
    }

    #[test]
    fn test_mapping_store_mutable_field() {
        // Store to a remapped mutable field is rewritten at the store instruction's PC.
        let program = r#"
          stw [%r1 + 12], 0x42
          mov %r0, 1
          exit
        "#;
        for program in initialize_test_programs_for_32bit_arg(parse_asm(program)).expect("load") {
            let mut data = TestArgument32BitMapped(TestArgument::default());
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), 1);
            assert_eq!(data.0.mutable_field, 0x42);
        }
    }

    #[test]
    fn test_fake_array_bounds_check() {
        // Verify that negative offsets in memory ptrs are handled properly and cannot be used to
        // bypass array bounds checks.
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+8]
        # Subtract 8 from `data` and pretend checking array bounds.
        mov %r3, %r1
        sub %r3, 0x8
        jgt %r3, %r2, +1
        # Read 8 bytes from `data`. This should be rejected by the verifier.
        ldxdw %r0, [%r1]
        exit
        "#;
        let err = initialize_test_program(parse_asm(program)).expect_err("incorrect program");
        assert_eq!(
            err,
            EbpfError::ProgramVerifyError(VerifierError {
                kind: VerifierErrorKind::PointerOffsetOverflow,
                pc: Some(4),
            })
        );
    }

    #[test]
    fn test_five_argument_program_execution() {
        struct FiveArgContext {}

        impl EbpfProgramContext for FiveArgContext {
            type RunContext<'a> = ();
            type Packet<'a> = ();
            type Arg1<'a> = usize;
            type Arg2<'a> = usize;
            type Arg3<'a> = usize;
            type Arg4<'a> = usize;
            type Arg5<'a> = usize;
            type Map = NoMap;
        }

        empty_static_helper_set!(FiveArgContext);

        let code = parse_asm(
            r#"
            mov %r0, %r1
            add %r0, %r2
            add %r0, %r3
            add %r0, %r4
            add %r0, %r5
            exit
            "#,
        );
        let verified = verify_program(
            code,
            CallingContext { args: vec![Type::UNKNOWN_SCALAR; 5], ..Default::default() },
            &mut NullVerifierLogger,
        )
        .expect("5-argument program should verify");
        for linked in link_test_programs_with_context::<FiveArgContext>(&verified)
            .expect("5-argument program should link")
        {
            assert_eq!(linked.run_with_5_arguments(&mut (), 10, 20, 30, 40, 50), 150);
        }
    }

    #[test]
    fn test_atomic_operations_32_and_64() {
        // Exercise 32-bit and 64-bit atomic operations on stack memory `[%r10-8]` and `[%r10-16]`,
        // including `src == %r0` and checking upper 32-bit zero-extension.
        let code = parse_asm(
            r#"
            # Initialize [%r10-8] = 0x11223344_00000010 (little-endian: low32=0x10, high32=0x11223344)
            stw [%r10-8], 0x10
            stw [%r10-4], 0x11223344

            # 1) 32-bit atomic add (no fetch): [%r10-8] += 5 -> 0x15
            mov %r2, -1
            mov32 %r2, 5
            lock add32 [%r10-8], %r2

            # 2) 32-bit atomic fetch add with src == %r0:
            # Set %r0 = 10 with dirty upper bits first; [%r10-8] was 0x15 -> 0x1f, %r0 -> 0x15
            mov %r0, -1
            add32 %r0, 11
            lock fetch add32 [%r10-8], %r0
            jeq %r0, 0x15, +1
            exit

            # 3) 32-bit atomic fetch AND, OR, XOR, XCHG:
            # [%r10-8] is 0x1f; fetch-AND with 0x0f -> old=0x1f, new=0x0f
            mov %r3, -1
            add32 %r3, 0x10
            lock fetch and32 [%r10-8], %r3
            jeq %r3, 0x1f, +1
            exit

            # Fetch-OR with 0xf0 on %r0 (src == %r0): [%r10-8] was 0x0f -> 0xff, %r0 -> 0x0f
            mov32 %r0, 0xf0
            lock fetch or32 [%r10-8], %r0
            jeq %r0, 0x0f, +1
            exit

            # Fetch-XOR with 0x55: [%r10-8] was 0xff -> 0xaa, %r3 -> 0xff
            mov32 %r3, 0x55
            lock fetch xor32 [%r10-8], %r3
            jeq %r3, 0xff, +1
            exit

            # 32-bit XCHG with 0x80000001: [%r10-8] was 0xaa -> 0x80000001, %r3 -> 0xaa
            mov32 %r3, 0x80000001
            lock xchg32 [%r10-8], %r3
            jeq %r3, 0xaa, +1
            exit

            # 32-bit CMPXCHG hit when %r0 has dirty upper bits (0xffffffff_80000001):
            # Matches 0x80000001, stores 0x42, and zero-extends %r0 to 0x80000001
            mov %r0, -0x7fffffff
            mov32 %r4, 0x42
            lock cmpxchg32 [%r10-8], %r4
            rsh %r0, 32
            jeq %r0, 0, +1
            exit

            # Verify adjacent 32-bit word [%r10-4] is still 0x11223344
            ldxw %r5, [%r10-4]
            jeq %r5, 0x11223344, +1
            exit

            # 4) 64-bit atomic operations on [%r10-16]:
            stdw [%r10-16], 100
            mov %r2, 23
            lock fetch add [%r10-16], %r2
            jeq %r2, 100, +1
            exit

            # 64-bit CMPXCHG miss (%r0 = 999 != 123): %r0 becomes 123, memory stays 123
            mov %r0, 999
            mov %r3, 777
            lock cmpxchg [%r10-16], %r3
            jeq %r0, 123, +1
            exit

            # Now %r0 is 123, so a second CMPXCHG with %r3 = 777 hits
            lock cmpxchg [%r10-16], %r3
            ldxdw %r0, [%r10-16]
            exit
            "#,
        );

        for program in initialize_test_programs(code).expect("load") {
            let mut data = TestArgument::default();
            assert_eq!(program.run_with_1_argument(&mut (), &mut data), 777);
        }
    }

    #[test]
    fn test_link_rejects_malformed_call_and_lddw() {
        let try_link =
            |code: Vec<EbpfInstruction>, prog_maps: Vec<MapSchema>, maps: Vec<TestMap>| {
                let program = VerifiedEbpfProgram::from_verified_code(
                    code,
                    vec![TEST_ARG_TYPE.clone()],
                    vec![],
                    prog_maps,
                );
                link_program::<TestEbpfProgramContext>(&program, maps)
            };

        let array_schema = MapSchema {
            map_type: linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY,
            key_size: 4,
            value_size: 8,
            max_entries: 1,
            flags: crate::MapFlags::empty(),
        };
        let hash_schema =
            MapSchema { map_type: linux_uapi::bpf_map_type_BPF_MAP_TYPE_HASH, ..array_schema };

        let mut pseudo_call_code = parse_asm(
            r#"
            mov %r0, 0
            exit
            "#,
        );
        // `parse_asm` only supports helper calls (`src_reg = 0`), so append `BPF_PSEUDO_CALL`.
        pseudo_call_code.push(EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, BPF_PSEUDO_CALL, 0, 1));
        assert!(matches!(
            try_link(pseudo_call_code, vec![], vec![]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        let neg_map_code = parse_asm(
            r#"
            mov %r0, 0
            exit
            lddw %r1, map[-1]
            "#,
        );
        assert!(matches!(
            try_link(neg_map_code, vec![], vec![]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        // Mismatched `MapSchema` between `program.maps` and linked `maps`.
        let map_load_code = parse_asm(
            r#"
            lddw %r1, map[0]
            mov %r0, 0
            exit
            "#,
        );
        let wider_schema = MapSchema { value_size: 16, ..array_schema };
        assert!(matches!(
            try_link(map_load_code, vec![array_schema], vec![TestMap(wider_schema)]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        // Truncated `BPF_LDDW` at the end of `code` (both immediate and map load).
        let mut truncated_imm_lddw = parse_asm(
            r#"
            mov %r0, 0
            exit
            "#,
        );
        truncated_imm_lddw.push(EbpfInstruction::new(BPF_LDDW, 1, 0, 0, 42));
        assert!(matches!(
            try_link(truncated_imm_lddw, vec![], vec![]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        let mut truncated_map_lddw = parse_asm(
            r#"
            mov %r0, 0
            exit
            "#,
        );
        truncated_map_lddw.push(EbpfInstruction::new(BPF_LDDW, 1, BPF_PSEUDO_MAP_IDX, 0, 0));
        assert!(matches!(
            try_link(truncated_map_lddw, vec![array_schema], vec![TestMap(array_schema)]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        // `BPF_PSEUDO_MAP_IDX_VALUE` on a non-array map must return `ProgramLinkError` before
        // calling `MapReference::get_data_ptr()` (which asserts `BPF_MAP_TYPE_ARRAY`).
        let map_val_non_array = parse_asm(
            r#"
            lddw %r1, map_val[0]
            mov %r0, 0
            exit
            "#,
        );
        assert!(matches!(
            try_link(map_val_non_array, vec![hash_schema], vec![TestMap(hash_schema)]),
            Err(EbpfError::ProgramLinkError(_))
        ));

        // `BPF_PSEUDO_MAP_IDX_VALUE` with out-of-bounds value offset must return
        // `ProgramLinkError`.
        for map_val_oob in [
            r#"
            lddw %r1, map_val[0]-4
            mov %r0, 0
            exit
            "#,
            r#"
            lddw %r1, map_val[0]+8
            mov %r0, 0
            exit
            "#,
        ] {
            assert!(matches!(
                try_link(parse_asm(map_val_oob), vec![array_schema], vec![TestMap(array_schema)]),
                Err(EbpfError::ProgramLinkError(_))
            ));
        }

        // Dead-branch unknown helper call sanitized by `verify_program` links and runs cleanly.
        let dead_helper_code = parse_asm(
            r#"
            mov %r0, 7
            jeq %r0, 8, +1
            exit
            call 9999
            exit
            "#,
        );
        for linked in initialize_test_programs(dead_helper_code)
            .expect("dead-branch unknown helper should verify and link")
        {
            let mut arg = TestArgument::default();
            assert_eq!(linked.run_with_1_argument(&mut (), &mut arg), 7);
        }
    }

    #[repr(C)]
    struct TestPacket([u8; 10]);

    static TEST_PACKET_TYPE: LazyLock<Type> = LazyLock::new(|| Type::PtrToMemory {
        id: new_memory_id(),
        offset: 0.into(),
        buffer_size: 10,
        origin: PointerOrigin::KernelReadOnly,
    });

    struct PacketTestContext {}

    impl EbpfProgramContext for PacketTestContext {
        type RunContext<'a> = ();
        type Packet<'a> = &'a TestPacket;
        type Arg1<'a> = &'a TestPacket;
        type Arg2<'a> = usize;
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();
        type Map = NoMap;
    }

    empty_static_helper_set!(PacketTestContext);

    impl ProgramArgument for &'_ TestPacket {
        fn get_type() -> &'static Type {
            &*TEST_PACKET_TYPE
        }
    }

    impl Packet for &'_ TestPacket {
        fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue> {
            if offset < 0 {
                return None;
            }
            let offset = offset as usize;
            let bytes = self.0.get(offset..offset + width.bytes())?;
            let val = match width {
                DataWidth::U8 => u64::from(bytes[0]),
                DataWidth::U16 => u64::from(u16::from_be_bytes([bytes[0], bytes[1]])),
                DataWidth::U32 => {
                    u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                }
                DataWidth::U64 => u64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]),
            };
            Some(BpfValue::from(val))
        }
    }

    #[test]
    fn test_packet_load_trampoline() {
        type TrampolineFn = unsafe extern "C" fn(
            run_context: *mut (),
            skb_r6: u64,
            reg_offset: i32,
            imm_offset: i32,
            width_bytes: u32,
            out_value: *mut u64,
        ) -> u64;

        let linkage = collect_program_linkage::<PacketTestContext>(&[]);
        let fn_ptr = linkage.packet_load_fn_addr.expect("packet_load_fn_addr should be set");
        // SAFETY: `collect_program_linkage` populates `packet_load_fn_addr` with
        // `packet_load_trampoline::<PacketTestContext>`, which has the `TrampolineFn` ABI.
        let trampoline: TrampolineFn =
            unsafe { std::mem::transmute::<usize, TrampolineFn>(fn_ptr as usize) };

        let packet = TestPacket([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA]);
        let skb_r6 = BpfValue::from(&packet).as_u64();
        let mut ctx = ();
        let ctx_ptr = &mut ctx as *mut () as *mut ();

        let call = |reg_offset: i32, imm_offset: i32, width_bytes: u32| -> (u64, u64) {
            let mut out = 0xDEAD_BEEF_CAFE_BABEu64;
            // SAFETY: `ctx_ptr` points to a valid `PacketTestContext::RunContext` (`()`),
            // `skb_r6` holds the `BpfValue` for `&packet`, and `out` is a valid `u64` slot.
            let ok = unsafe {
                trampoline(ctx_ptr, skb_r6, reg_offset, imm_offset, width_bytes, &mut out)
            };
            (ok, out)
        };

        // 1-, 2-, 4-, and 8-byte in-bounds loads via `reg_offset + imm_offset`.
        assert_eq!(call(0, 0, 1), (1, 0x11));
        assert_eq!(call(1, 2, 2), (1, 0x4455));
        assert_eq!(call(5, -3, 4), (1, 0x33445566));
        assert_eq!(call(0, 2, 8), (1, 0x33445566778899AA));

        // Out-of-bounds, negative offset, and signed addition overflow return 0 and clear `out`.
        assert_eq!(call(0, -1, 1), (0, 0));
        assert_eq!(call(0, 10, 1), (0, 0));
        assert_eq!(call(2, 3, 8), (0, 0));
        assert_eq!(call(i32::MAX, 1, 1), (0, 0));

        // Invalid widths return 0 and clear `out`.
        assert_eq!(call(0, 0, 0), (0, 0));
        assert_eq!(call(0, 0, 3), (0, 0));
    }

    #[test]
    fn test_link_instructions_errors() {
        let mem_id = new_memory_id();
        let program = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                ldxw %r0, [%r1]
                exit
                "#,
            ),
            vec![],
            vec![StructAccess {
                pc: 0,
                memory_id: mem_id.clone(),
                field_offset: 0,
                is_32_bit_ptr_load: false,
            }],
            vec![MapSchema {
                map_type: linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY,
                key_size: 4,
                value_size: 8,
                max_entries: 1,
                flags: crate::MapFlags::empty(),
            }],
        );

        // Map count mismatch must return an error.
        let empty_maps_linkage = ProgramLinkageInfo {
            struct_mappings: vec![],
            helpers: vec![],
            maps: vec![],
            packet_load_fn_addr: None,
        };
        assert_eq!(
            link_instructions(&program, &empty_maps_linkage),
            Err(LinkError::InvalidMapLinkage(MapLinkageError::MapCountMismatch {
                expected: 1,
                actual: 0,
            }))
        );

        let valid_maps = vec![MapLinkage { map_addr: 0x1000, data_addr: Some(0x2000) }];

        // Out-of-bounds `sa.pc` must return `PcOutOfBounds` instead of panicking.
        let oob_pc_prog = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                exit
                "#,
            ),
            vec![],
            vec![StructAccess {
                pc: 5,
                memory_id: mem_id.clone(),
                field_offset: 0,
                is_32_bit_ptr_load: false,
            }],
            vec![],
        );
        assert_eq!(
            link_instructions(&oob_pc_prog, &ProgramLinkageInfo::default()),
            Err(LinkError::InvalidStructMapping(StructMappingError::PcOutOfBounds { pc: 5 }))
        );

        // Out-of-range target_offset must return `TargetOffsetOutOfRange` instead of panicking.
        let overflow_offset_linkage = ProgramLinkageInfo {
            struct_mappings: vec![StructFieldMapping {
                memory_id: mem_id.clone(),
                source_offset: 0,
                target_offset: usize::MAX,
            }],
            helpers: vec![],
            maps: valid_maps.clone(),
            packet_load_fn_addr: None,
        };
        assert_eq!(
            link_instructions(&program, &overflow_offset_linkage),
            Err(LinkError::InvalidStructMapping(StructMappingError::TargetOffsetOutOfRange {
                pc: 0,
                target_offset: usize::MAX,
            }))
        );

        // Difference exceeding `i16` range must return `OffsetDifferenceOverflow`.
        let diff_overflow_linkage = ProgramLinkageInfo {
            struct_mappings: vec![StructFieldMapping {
                memory_id: mem_id.clone(),
                source_offset: 0,
                target_offset: (i16::MAX as usize) + 1,
            }],
            helpers: vec![],
            maps: valid_maps.clone(),
            packet_load_fn_addr: None,
        };
        assert_eq!(
            link_instructions(&program, &diff_overflow_linkage),
            Err(LinkError::InvalidStructMapping(StructMappingError::OffsetDifferenceOverflow {
                pc: 0,
            }))
        );

        // `instruction.offset() + offset_diff` overflowing `i16` must return `OffsetOverflow`.
        let insn_offset_overflow_prog = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                ldxw %r0, [%r1+0x7fff]
                exit
                "#,
            ),
            vec![],
            vec![StructAccess {
                pc: 0,
                memory_id: mem_id.clone(),
                field_offset: 0,
                is_32_bit_ptr_load: false,
            }],
            vec![],
        );
        let plus_four_linkage = ProgramLinkageInfo {
            struct_mappings: vec![StructFieldMapping {
                memory_id: mem_id.clone(),
                source_offset: 0,
                target_offset: 4,
            }],
            helpers: vec![],
            maps: vec![],
            packet_load_fn_addr: None,
        };
        assert_eq!(
            link_instructions(&insn_offset_overflow_prog, &plus_four_linkage),
            Err(LinkError::InvalidStructMapping(StructMappingError::OffsetOverflow { pc: 0 }))
        );

        // Unmapped 32-bit pointer load must return `Unmapped32BitField`.
        let unmapped_ptr_prog = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                ldxw %r0, [%r1]
                exit
                "#,
            ),
            vec![],
            vec![StructAccess {
                pc: 0,
                memory_id: mem_id,
                field_offset: 0,
                is_32_bit_ptr_load: true,
            }],
            vec![],
        );
        assert_eq!(
            link_instructions(&unmapped_ptr_prog, &ProgramLinkageInfo::default()),
            Err(LinkError::InvalidStructMapping(StructMappingError::Unmapped32BitField { pc: 0 }))
        );

        // Missing helper must return LinkError::MissingHelper.
        let helper_prog = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                call 42
                exit
                "#,
            ),
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            link_instructions(&helper_prog, &ProgramLinkageInfo::default()),
            Err(LinkError::MissingHelper(42))
        );

        // Packet load instruction without packet_load_fn_addr must return
        // LinkError::MissingPacketLoadFn.
        let pkt_prog = VerifiedEbpfProgram::from_verified_code(
            parse_asm(
                r#"
                ldpw
                exit
                "#,
            ),
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            link_instructions(&pkt_prog, &ProgramLinkageInfo::default()),
            Err(LinkError::MissingPacketLoadFn)
        );
    }

    #[test]
    fn test_packet_load_abs_and_ind() {
        // %r1 = packet (`&TestPacket`), %r2 = dynamic index (`usize`).
        // Initialize %r0 = 0 (verifier requires %r0 to be initialized before BPF_ABS/BPF_IND in
        // case of early exit), copy %r1 -> %r6 (packet base register required by BPF_ABS/BPF_IND),
        // and save %r2 -> %r8 because BPF_ABS/BPF_IND clobber %r1..%r5 in the verifier.
        let code = parse_asm(
            r#"
            mov %r0, 0
            mov %r6, %r1
            mov %r8, %r2
            # Load byte at offset 0 (BPF_LD | BPF_ABS | BPF_B) -> 0x11
            ldpb
            mov %r7, %r0
            # Load halfword at offset 0 (BPF_LD | BPF_ABS | BPF_H) -> 0x1122
            ldph
            add %r7, %r0
            # Load word at offset 0 (BPF_LD | BPF_ABS | BPF_W) -> 0x11223344
            ldpw
            add %r7, %r0
            # Load dword at indirect offset %r8 + 2 (BPF_LD | BPF_IND | BPF_DW)
            ldpdw %r8 +2
            add %r0, %r7
            exit
            "#,
        );

        let verified = verify_program(
            code,
            CallingContext {
                args: vec![TEST_PACKET_TYPE.clone(), Type::UNKNOWN_SCALAR],
                packet_type: Some(TEST_PACKET_TYPE.clone()),
                ..Default::default()
            },
            &mut NullVerifierLogger,
        )
        .expect("verify");

        let programs =
            link_test_programs_with_context::<PacketTestContext>(&verified).expect("link");

        let packet = TestPacket([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA]);
        let expected_in_bounds = 0x11u64 + 0x1122u64 + 0x11223344u64 + 0x33445566778899AAu64;
        for program in &programs {
            // In-bounds: %r2 = 0 -> loads DW at offset 0 + 2 = 2..10
            assert_eq!(program.run_with_2_arguments(&mut (), &packet, 0), expected_in_bounds);
            // Out-of-bounds: %r2 = 1 -> tries to load DW at offset 1 + 2 = 3..11 -> returns 0
            assert_eq!(program.run_with_2_arguments(&mut (), &packet, 1), 0);
        }
    }

    #[derive(Clone)]
    struct RelocationTestMap {
        schema: MapSchema,
        map_ptr: u64,
        data: Option<Arc<[u8; 64]>>,
    }

    impl MapReference for RelocationTestMap {
        fn schema(&self) -> &MapSchema {
            &self.schema
        }

        fn as_bpf_value(&self) -> BpfValue {
            BpfValue::from(self.map_ptr)
        }

        fn get_data_ptr(&self) -> Option<BpfValue> {
            assert_eq!(self.schema.map_type, bpf_map_type_BPF_MAP_TYPE_ARRAY);
            self.data.as_ref().map(|buf| BpfValue::from(buf.as_ptr() as u64))
        }
    }

    struct MapAndHelperTestContext {}

    #[repr(C)]
    struct HelperRunContext {
        accumulated: u64,
    }

    const TEST_HELPER_ID: u32 = 1;

    extern "C" fn test_5arg_helper(
        context: &mut HelperRunContext,
        a1: BpfValue,
        a2: BpfValue,
        a3: BpfValue,
        a4: BpfValue,
        a5: BpfValue,
    ) -> BpfValue {
        let sum = a1
            .as_u64()
            .wrapping_add(a2.as_u64())
            .wrapping_add(a3.as_u64())
            .wrapping_add(a4.as_u64())
            .wrapping_add(a5.as_u64());
        context.accumulated = context.accumulated.wrapping_add(sum);
        BpfValue::from(context.accumulated)
    }

    impl EbpfProgramContext for MapAndHelperTestContext {
        type RunContext<'a> = HelperRunContext;
        type Packet<'a> = ();
        type Arg1<'a> = usize;
        type Arg2<'a> = usize;
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();
        type Map = RelocationTestMap;
    }

    impl StaticHelperSet for MapAndHelperTestContext {
        fn helpers() -> &'static HelperSet<Self> {
            static HELPERS: LazyLock<HelperSet<MapAndHelperTestContext>> = LazyLock::new(|| {
                HelperSet::new(std::iter::once((
                    TEST_HELPER_ID,
                    EbpfHelperImpl::<MapAndHelperTestContext>(test_5arg_helper),
                )))
            });
            &HELPERS
        }
    }

    #[test]
    fn test_map_relocations_and_non_array_map() {
        let mut array_buf = [0u8; 64];
        // Write 0xCAFEBABE_12345678 at offset 16 inside the array map buffer.
        array_buf[16..24].copy_from_slice(&0xCAFEBABE_12345678u64.to_ne_bytes());

        let array_map = RelocationTestMap {
            schema: MapSchema {
                map_type: bpf_map_type_BPF_MAP_TYPE_ARRAY,
                key_size: 4,
                value_size: 32,
                max_entries: 2,
                flags: MapFlags::empty(),
            },
            map_ptr: 0xDEAD_0001,
            data: Some(Arc::new(array_buf)),
        };
        // Include a non-ARRAY map (`BPF_MAP_TYPE_HASH`) whose `get_data_ptr()` would panic if
        // `collect_program_linkage` called it on non-array maps.
        let hash_map = RelocationTestMap {
            schema: MapSchema {
                map_type: bpf_map_type_BPF_MAP_TYPE_HASH,
                key_size: 4,
                value_size: 8,
                max_entries: 16,
                flags: MapFlags::empty(),
            },
            map_ptr: 0xDEAD_0002,
            data: None,
        };

        // 1) `BPF_PSEUDO_MAP_IDX_VALUE` for map 0 (`array_map`) with offset 16 -> reads u64 at
        //    `[%r1]`
        // 2) `BPF_PSEUDO_MAP_IDX` for map 1 (`hash_map`) -> exercises map pointer relocation
        let code = parse_asm(
            r#"
            lddw %r1, map_val[0]+16
            lddw %r2, map[1]
            ldxdw %r0, [%r1]
            exit
            "#,
        );

        let mut calling_context = CallingContext {
            args: vec![Type::UNKNOWN_SCALAR, Type::UNKNOWN_SCALAR],
            ..Default::default()
        };
        assert_eq!(calling_context.register_map(array_map.schema), 0);
        assert_eq!(calling_context.register_map(hash_map.schema), 1);

        let verified =
            verify_program(code, calling_context, &mut NullVerifierLogger).expect("verify");

        let programs = link_test_programs_with_maps::<MapAndHelperTestContext>(
            &verified,
            vec![array_map, hash_map],
        )
        .expect("link");

        for program in &programs {
            let mut ctx = HelperRunContext { accumulated: 0 };
            assert_eq!(program.run_with_2_arguments(&mut ctx, 0, 0), 0xCAFEBABE_12345678u64);
        }
    }

    #[test]
    fn test_helper_with_run_context_and_callee_saved_regs() {
        // Verify that %r6..%r9 and %r10 are preserved across helper calls, and that RunContext + 5
        // arguments are passed to the helper accurately in both interpreter and JIT mode.
        let program = r#"
          mov %r6, %r1
          mov %r7, %r2
          mov %r8, 100
          mov %r9, 200
          stdw [%r10 - 8], 300
          mov %r3, %r8
          mov %r4, %r9
          ldxdw %r5, [%r10 - 8]
          call 1
          # Call a second time using preserved %r6..%r10
          mov %r1, %r6
          mov %r2, %r7
          mov %r3, %r8
          mov %r4, %r9
          ldxdw %r5, [%r10 - 8]
          call 1
          add %r0, %r6
          add %r0, %r7
          add %r0, %r8
          add %r0, %r9
          ldxdw %r1, [%r10 - 8]
          add %r0, %r1
          exit
        "#;

        static TEST_HELPER_DEF: LazyLock<HelperDefinition> = LazyLock::new(|| HelperDefinition {
            index: TEST_HELPER_ID,
            name: "test_5arg_helper",
            signature: FunctionSignatureBuilder {
                args: vec![
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                ],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            }
            .build(),
        });

        let mut calling_context = CallingContext {
            args: vec![Type::UNKNOWN_SCALAR, Type::UNKNOWN_SCALAR],
            ..Default::default()
        };
        calling_context.helpers.insert(TEST_HELPER_ID, &*TEST_HELPER_DEF);

        let verified = verify_program(parse_asm(program), calling_context, &mut NullVerifierLogger)
            .expect("verify");

        let programs =
            link_test_programs_with_context::<MapAndHelperTestContext>(&verified).expect("link");

        for program in &programs {
            let mut ctx = HelperRunContext { accumulated: 10 };
            // Each call adds (11 + 22 + 100 + 200 + 300) = 633 to ctx.accumulated:
            // After call 1: accumulated = 643
            // After call 2: accumulated = 1276 (returned in %r0)
            // Then adds %r6..%r9 + [%r10 - 8] = 633 -> total 1909.
            let res = program.run_with_2_arguments(&mut ctx, 11, 22);
            assert_eq!(res, 1909);
            assert_eq!(ctx.accumulated, 1276);
        }
    }

    #[test]
    fn test_jit_unused_register_fields_and_untaken_oob_jumps() {
        // 1. Unused register nibbles (`11..=15`) in immediate, unary, store-immediate, and legacy
        // packet-load instructions are not checked by `BpfVisitor::visit` and must not cause JIT
        // compilation to panic or fail. Constructed with `EbpfInstruction::new` because
        // `parse_asm` cannot encode invalid `11..=15` unused register nibbles.
        let code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 15, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
            // `BPF_LD | BPF_ABS` ignores both `dst_reg` and `src_reg`; `BPF_LD | BPF_IND` ignores
            // `dst_reg`.
            EbpfInstruction::new(BPF_LD | BPF_ABS | BPF_B, 15, 15, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 14, 0, 1),
            EbpfInstruction::new(BPF_LD | BPF_IND | BPF_B, 13, 2, 0, 0),
            // Immediate ALU, unary NEG/END, and ST_MEM ignore `src_reg`.
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 0, 12, 0, 5),
            EbpfInstruction::new(BPF_ALU64 | BPF_NEG, 0, 11, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_NEG, 0, 15, 0, 0),
            EbpfInstruction::new(BPF_ALU | BPF_END | BPF_TO_LE, 0, 14, 0, 16),
            EbpfInstruction::new(BPF_ST | BPF_MEM | BPF_DW, 10, 13, -8, 10),
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_DW, 3, 10, -8, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_X, 0, 3, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 15, 15, 0, 0),
        ];

        let verified = verify_program(
            code,
            CallingContext {
                args: vec![TEST_PACKET_TYPE.clone(), Type::UNKNOWN_SCALAR],
                packet_type: Some(TEST_PACKET_TYPE.clone()),
                ..Default::default()
            },
            &mut NullVerifierLogger,
        )
        .expect("verify program with unused register nibbles");

        let packet = TestPacket([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA]);
        for program in link_test_programs_with_context::<PacketTestContext>(&verified)
            .expect("link program with unused register nibbles")
        {
            // packet[1] = 0x22 (34) + 5 + 10 = 49.
            assert_eq!(program.run_with_2_arguments(&mut (), &packet, 0), 49);
        }

        // 2. A conditional jump whose condition is statically known to be `false` is visited by
        // `verify_program()`, but its target offset is not validated by the verifier when the
        // branch is untaken. JIT compilation must reject both out-of-bounds jump offsets and jumps
        // into the second instruction slot of a 2-slot `BPF_LDDW` with `EbpfError::ProgramLinkError`.
        if crate::jit::IS_SUPPORTED {
            for bad_asm in [
                r#"
                mov %r0, 0
                jeq %r0, 1, +100
                exit
                "#,
                r#"
                mov %r0, 0
                jeq %r0, 1, -3
                exit
                "#,
                r#"
                mov %r0, 0
                jeq %r0, 1, -10
                exit
                "#,
                r#"
                mov %r0, 0
                jeq %r0, 1, +1
                lddw %r1, 0x123456789abcdef0
                exit
                "#,
            ] {
                let verified_bad = verify_program(
                    parse_asm(bad_asm),
                    CallingContext { args: vec![TEST_ARG_TYPE.clone()], ..Default::default() },
                    &mut NullVerifierLogger,
                )
                .expect("verifier allows untaken conditional jump with invalid target");
                assert!(matches!(
                    link_program_jit_for_test::<TestEbpfProgramContext, StaticTypeChecker>(
                        &verified_bad,
                        vec![],
                    ),
                    Err(EbpfError::ProgramLinkError(_))
                ));
            }
        }
    }
}
