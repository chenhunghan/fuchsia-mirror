// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use alloc::vec::Vec;

use crate::api::{
    BPF_ABS, BPF_CALL, BPF_CLS_MASK, BPF_DW, BPF_IND, BPF_JMP, BPF_LD, BPF_LDDW,
    BPF_MAP_TYPE_ARRAY, BPF_PSEUDO_MAP_IDX, BPF_PSEUDO_MAP_IDX_VALUE, BPF_SIZE_MASK,
    EbpfInstruction,
};
use crate::error::{LinkError, MapLinkageError, ProgramLinkError, StructMappingError};
use crate::verifier::{MemoryId, StructAccess, Type, VerifiedEbpfProgram};

/// A mapping for a field in a struct where the original ebpf program knows a different offset and
/// data size than the one it receives from the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldMapping {
    /// The offset of the field as known by the original ebpf program.
    pub source_offset: usize,
    /// The actual offset of the field in the data provided by the kernel.
    pub target_offset: usize,
}

pub type FieldMappings = Vec<FieldMapping>;

#[derive(Debug)]
pub struct StructMapping {
    /// Memory ID used in the struct definition.
    pub memory_id: MemoryId,

    /// The list of mappings in the buffer. The verifier must rewrite the actual ebpf to ensure
    /// the right offset and operand are use to access the mapped fields. Mappings are allowed
    /// only for pointer fields.
    pub fields: FieldMappings,
}

// Manual index-based `Clone` and `PartialEq` implementations on `Vec`-backed
// linkage ADTs avoid slice/iterator trait machinery during Charon/Aeneas LLBC
// translation.
impl Clone for StructMapping {
    fn clone(&self) -> Self {
        let mut fields = Vec::new();
        let mut i = 0;
        while i < self.fields.len() {
            fields.push(self.fields[i]);
            i += 1;
        }
        Self { memory_id: self.memory_id.clone(), fields }
    }
}

impl PartialEq for StructMapping {
    fn eq(&self, other: &Self) -> bool {
        if self.memory_id != other.memory_id || self.fields.len() != other.fields.len() {
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

impl Eq for StructMapping {}

pub type StructMappings = Vec<StructMapping>;

/// Checks that each actual argument type in `actual_args` (up to 5) is a subtype of the
/// corresponding verified argument type in `program_args` (or `Type::UNINITIALIZED` if absent).
/// Returns `Err(index)` with the 0-based argument index on mismatch.
pub fn check_argument_types(program_args: &[Type], actual_args: &[Type; 5]) -> Result<(), usize> {
    let uninitialized = Type::UNINITIALIZED;
    let mut i = 0;
    while i < 5 {
        let verified_type = if i < program_args.len() { &program_args[i] } else { &uninitialized };
        if !actual_args[i].is_subtype(verified_type) {
            return Err(i);
        }
        i += 1;
    }
    Ok(())
}

#[derive(Debug)]
pub struct StructFieldMapping {
    pub memory_id: MemoryId,
    pub source_offset: usize,
    pub target_offset: usize,
}

impl Clone for StructFieldMapping {
    fn clone(&self) -> Self {
        Self {
            memory_id: self.memory_id.clone(),
            source_offset: self.source_offset,
            target_offset: self.target_offset,
        }
    }
}

impl PartialEq for StructFieldMapping {
    fn eq(&self, other: &Self) -> bool {
        self.memory_id == other.memory_id
            && self.source_offset == other.source_offset
            && self.target_offset == other.target_offset
    }
}

impl Eq for StructFieldMapping {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperLinkage {
    pub id: u32,
    pub fn_addr: u64,
}

#[derive(Copy, Debug)]
pub struct MapLinkage {
    pub map_addr: u64,
    pub data_addr: Option<u64>,
}

impl Clone for MapLinkage {
    fn clone(&self) -> Self {
        *self
    }
}

impl PartialEq for MapLinkage {
    fn eq(&self, other: &Self) -> bool {
        if self.map_addr != other.map_addr {
            return false;
        }
        match (self.data_addr, other.data_addr) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for MapLinkage {}

#[derive(Debug)]
pub struct ProgramLinkageInfo {
    pub struct_mappings: Vec<StructFieldMapping>,
    pub helpers: Vec<HelperLinkage>,
    pub maps: Vec<MapLinkage>,
    pub packet_load_fn_addr: Option<u64>,
}

impl Default for ProgramLinkageInfo {
    fn default() -> Self {
        Self {
            struct_mappings: Vec::new(),
            helpers: Vec::new(),
            maps: Vec::new(),
            packet_load_fn_addr: None,
        }
    }
}

impl Clone for ProgramLinkageInfo {
    fn clone(&self) -> Self {
        let mut struct_mappings = Vec::new();
        let mut i = 0;
        while i < self.struct_mappings.len() {
            struct_mappings.push(self.struct_mappings[i].clone());
            i += 1;
        }
        let mut helpers = Vec::new();
        let mut j = 0;
        while j < self.helpers.len() {
            helpers.push(self.helpers[j]);
            j += 1;
        }
        let mut maps = Vec::new();
        let mut k = 0;
        while k < self.maps.len() {
            maps.push(self.maps[k]);
            k += 1;
        }
        Self { struct_mappings, helpers, maps, packet_load_fn_addr: self.packet_load_fn_addr }
    }
}

impl PartialEq for ProgramLinkageInfo {
    fn eq(&self, other: &Self) -> bool {
        if self.struct_mappings.len() != other.struct_mappings.len()
            || self.helpers.len() != other.helpers.len()
            || self.maps.len() != other.maps.len()
        {
            return false;
        }
        match (self.packet_load_fn_addr, other.packet_load_fn_addr) {
            (Some(a), Some(b)) => {
                if a != b {
                    return false;
                }
            }
            (None, None) => {}
            _ => return false,
        }
        let mut i = 0;
        while i < self.struct_mappings.len() {
            if self.struct_mappings[i] != other.struct_mappings[i] {
                return false;
            }
            i += 1;
        }
        let mut j = 0;
        while j < self.helpers.len() {
            if self.helpers[j] != other.helpers[j] {
                return false;
            }
            j += 1;
        }
        let mut k = 0;
        while k < self.maps.len() {
            if self.maps[k] != other.maps[k] {
                return false;
            }
            k += 1;
        }
        true
    }
}

impl Eq for ProgramLinkageInfo {}

fn find_struct_field_target_offset(
    struct_mappings: &[StructFieldMapping],
    memory_id: &MemoryId,
    field_offset: usize,
) -> Option<usize> {
    let mut i = 0;
    while i < struct_mappings.len() {
        if struct_mappings[i].memory_id == *memory_id
            && struct_mappings[i].source_offset == field_offset
        {
            return Some(struct_mappings[i].target_offset);
        }
        i += 1;
    }
    None
}

fn has_linked_helper(helpers: &[HelperLinkage], helper_id: u32) -> bool {
    let mut i = 0;
    while i < helpers.len() {
        if helpers[i].id == helper_id && helpers[i].fn_addr != 0 {
            return true;
        }
        i += 1;
    }
    false
}

fn apply_struct_access_mapping(
    sa: &StructAccess,
    struct_mappings: &[StructFieldMapping],
    code: &mut Vec<EbpfInstruction>,
) -> Result<(), LinkError> {
    let pc = sa.pc;
    if pc >= code.len() {
        return Err(LinkError::InvalidStructMapping(StructMappingError::PcOutOfBounds { pc }));
    }
    match find_struct_field_target_offset(struct_mappings, &sa.memory_id, sa.field_offset) {
        Some(target_offset) => {
            // Note that `instruction.off` may be different from `field_offset`. It's adjusted
            // by the difference between `target_offset` and `field_offset` to ensure the
            // instruction accesses the right field.
            if target_offset > (i64::MAX as usize) {
                return Err(LinkError::InvalidStructMapping(
                    StructMappingError::TargetOffsetOutOfRange { pc, target_offset },
                ));
            }
            if sa.field_offset > (i64::MAX as usize) {
                return Err(LinkError::InvalidStructMapping(
                    StructMappingError::SourceOffsetOutOfRange {
                        pc,
                        source_offset: sa.field_offset,
                    },
                ));
            }
            let diff_i64 = (target_offset as i64) - (sa.field_offset as i64);
            if diff_i64 < (i16::MIN as i64) || diff_i64 > (i16::MAX as i64) {
                return Err(LinkError::InvalidStructMapping(
                    StructMappingError::OffsetDifferenceOverflow { pc },
                ));
            }
            let offset_diff = diff_i64 as i16;
            let mut instruction = code[pc];
            let Some(new_offset) = instruction.offset().checked_add(offset_diff) else {
                return Err(LinkError::InvalidStructMapping(StructMappingError::OffsetOverflow {
                    pc,
                }));
            };
            instruction.set_offset(new_offset);

            // 32-bit pointer loads must be updated to 64-bit loads.
            if sa.is_32_bit_ptr_load {
                instruction.set_code((instruction.code() & !BPF_SIZE_MASK) | BPF_DW);
            }
            code[pc] = instruction;
            Ok(())
        }
        None => {
            if sa.is_32_bit_ptr_load {
                Err(LinkError::InvalidStructMapping(StructMappingError::Unmapped32BitField { pc }))
            } else {
                Ok(())
            }
        }
    }
}

fn link_single_instruction(
    pc: usize,
    program: &VerifiedEbpfProgram,
    linkage: &ProgramLinkageInfo,
    has_packet_load_fn: bool,
    code: &mut Vec<EbpfInstruction>,
) -> Result<usize, LinkError> {
    let opcode = code[pc].code();

    // Check that we have implementations for all helper calls.
    if opcode == (BPF_JMP | BPF_CALL) {
        let instruction = code[pc];
        if instruction.src_reg() != 0 {
            return Err(LinkError::InvalidProgram(ProgramLinkError::UnsupportedCallSrcReg {
                src_reg: instruction.src_reg(),
                pc,
            }));
        }
        let helper_id = instruction.imm() as u32;
        if !has_linked_helper(&linkage.helpers, helper_id) {
            return Err(LinkError::MissingHelper(helper_id));
        }
        return Ok(1);
    }

    if (opcode & BPF_CLS_MASK) == BPF_LD
        && ((opcode & 0xe0) == BPF_ABS || (opcode & 0xe0) == BPF_IND)
        && !has_packet_load_fn
    {
        return Err(LinkError::MissingPacketLoadFn);
    }

    if opcode == BPF_LDDW {
        if pc + 1 >= code.len() {
            return Err(LinkError::InvalidProgram(ProgramLinkError::IncompleteLddw { pc }));
        }
        let mut instruction = code[pc];
        let mut next_instruction = code[pc + 1];
        if next_instruction.code() != 0
            || next_instruction.offset() != 0
            || next_instruction.src_reg() != 0
            || next_instruction.dst_reg() != 0
        {
            return Err(LinkError::InvalidProgram(ProgramLinkError::MalformedLddw { pc }));
        }

        // If the instruction references BPF_PSEUDO_MAP_IDX or BPF_PSEUDO_MAP_IDX_VALUE, then
        // look up the map and rewrite the 64-bit immediate in place.
        match instruction.src_reg() {
            0 => {}
            src_reg @ (BPF_PSEUDO_MAP_IDX | BPF_PSEUDO_MAP_IDX_VALUE) => {
                let imm = instruction.imm();
                if imm < 0 {
                    return Err(LinkError::InvalidMapLinkage(MapLinkageError::NegativeMapIndex {
                        imm,
                        pc,
                    }));
                }
                let map_index = imm as usize;
                if map_index >= program.maps.len() || map_index >= linkage.maps.len() {
                    return Err(LinkError::InvalidMapLinkage(MapLinkageError::InvalidMapIndex {
                        map_index,
                    }));
                }
                let schema = &program.maps[map_index];
                let map = &linkage.maps[map_index];

                let value = if src_reg == BPF_PSEUDO_MAP_IDX {
                    map.map_addr
                } else {
                    // BPF_PSEUDO_MAP_IDX_VALUE
                    if schema.map_type != BPF_MAP_TYPE_ARRAY || schema.max_entries == 0 {
                        return Err(LinkError::InvalidMapLinkage(
                            MapLinkageError::MapValueRequiresNonEmptyArray { map_index },
                        ));
                    }
                    let next_imm = next_instruction.imm();
                    if next_imm < 0 || (next_imm as u32) >= schema.value_size {
                        return Err(LinkError::InvalidMapLinkage(
                            MapLinkageError::InvalidMapValueOffset { offset: next_imm, map_index },
                        ));
                    }
                    let offset = next_imm as u32;
                    let Some(base_ptr) = map.data_addr else {
                        return Err(LinkError::InvalidMapLinkage(
                            MapLinkageError::MissingMapDataAddr { map_index },
                        ));
                    };
                    let Some(addr) = base_ptr.checked_add(offset as u64) else {
                        return Err(LinkError::InvalidMapLinkage(
                            MapLinkageError::MapValueOffsetOverflow,
                        ));
                    };
                    addr
                };

                let high = (value >> 32) as i32;
                let low = value as i32;
                instruction.set_src_reg(0);
                instruction.set_imm(low);
                next_instruction.set_imm(high);
                code[pc] = instruction;
                code[pc + 1] = next_instruction;
            }
            src_reg => {
                return Err(LinkError::InvalidProgram(ProgramLinkError::UnsupportedLddwSrcReg {
                    src_reg,
                }));
            }
        }
        return Ok(2);
    }

    Ok(1)
}

/// Links a verified eBPF program's instructions for a target process using `linkage` by
/// updating struct access offsets/widths and resolving `BPF_LDDW` map pointers.
pub fn link_instructions(
    program: &VerifiedEbpfProgram,
    linkage: &ProgramLinkageInfo,
) -> Result<Vec<EbpfInstruction>, LinkError> {
    if linkage.maps.len() != program.maps.len() {
        return Err(LinkError::InvalidMapLinkage(MapLinkageError::MapCountMismatch {
            expected: program.maps.len(),
            actual: linkage.maps.len(),
        }));
    }

    let has_packet_load_fn = match linkage.packet_load_fn_addr {
        Some(addr) => addr != 0,
        None => false,
    };

    let mut code = Vec::new();
    let mut idx = 0;
    while idx < program.code.len() {
        code.push(program.code[idx]);
        idx += 1;
    }

    // Update offsets in the instructions that access structs.
    let mut loop_err: Option<LinkError> = None;
    let mut sa_idx = 0;
    while sa_idx < program.struct_access_instructions.len() && loop_err.is_none() {
        match apply_struct_access_mapping(
            &program.struct_access_instructions[sa_idx],
            &linkage.struct_mappings,
            &mut code,
        ) {
            Ok(()) => {
                sa_idx += 1;
            }
            Err(err) => {
                loop_err = Some(err);
            }
        }
    }
    if let Some(err) = loop_err {
        return Err(err);
    }

    let mut pc = 0;
    while pc < code.len() && loop_err.is_none() {
        match link_single_instruction(pc, program, linkage, has_packet_load_fn, &mut code) {
            Ok(step) => {
                pc += step;
            }
            Err(err) => {
                loop_err = Some(err);
            }
        }
    }
    if let Some(err) = loop_err {
        return Err(err);
    }

    Ok(code)
}
