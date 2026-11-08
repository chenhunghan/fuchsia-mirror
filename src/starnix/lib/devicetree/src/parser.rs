// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::str::Utf8Error;

use zerocopy::{BigEndian, FromBytes, Immutable, KnownLayout, U32};

use crate::types::*;

/// The magic string that is expected to be present in `Header.magic`.
const FDT_MAGIC: u32 = 0xd00dfeed;

/// Marks the beginning of a node's representation. It is followed by the node's
/// unit name as extra data. The name is stored as a null-terminated string.
const FDT_BEGIN_NODE: u32 = 0x00000001;

/// Marks the end of a node's representation.
const FDT_END_NODE: u32 = 0x00000002;

/// Marks the beginning of a property in the device tree.
const FDT_PROP: u32 = 0x00000003;

/// A token that is meant to be ignored by the parser.
const FDT_NOP: u32 = 0x00000004;

/// Marks the end of the structure block. There should only be one, and it should be the
/// last token in the structure block.
const FDT_END: u32 = 0x00000009;

/// Maximum allowed nesting depth for devicetree nodes.
const MAX_NODE_DEPTH: usize = 64;

/// Maximum number of memory reservation map entries before termination.
const MAX_RESERVE_ENTRIES: usize = 1024;

#[derive(Debug)]
pub enum ParseError {
    InvalidMagicNumber,
    UnsupportedVersion,
    Utf8Error(Utf8Error),
    MalformedStructure(String),
    ZeroCopyError,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidMagicNumber => write!(f, "Invalid magic number"),
            ParseError::UnsupportedVersion => write!(f, "Unsupported version"),
            ParseError::Utf8Error(e) => write!(f, "Utf8 error: {}", e),
            ParseError::MalformedStructure(e) => write!(f, "Malformed structure: {}", e),
            ParseError::ZeroCopyError => write!(f, "Zero copy error"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::Utf8Error(e) => Some(e),
            _ => None,
        }
    }
}

/// Parses a full `Devicetree` instance from `data`.
pub fn parse_devicetree(data: &[u8]) -> Result<Devicetree, ParseError> {
    // Parse and verify the header.
    let header = *parse_item::<Header>(&data)?;
    verify_header(&header)?;
    let totalsize = header.totalsize.get() as usize;
    if totalsize < std::mem::size_of::<Header>() || totalsize > data.len() {
        return Err(ParseError::MalformedStructure(format!(
            "Invalid totalsize 0x{:x} (buffer len: 0x{:x})",
            totalsize,
            data.len()
        )));
    }
    let data = &data[..totalsize];

    // Parse the memory reservation block.
    let off_mem_rsvmap = header.off_mem_rsvmap.get() as usize;
    let rsvmap_data = data.get(off_mem_rsvmap..).ok_or_else(|| {
        ParseError::MalformedStructure(format!(
            "Memory reservation block offset 0x{:x} exceeds total length 0x{:x}",
            off_mem_rsvmap,
            data.len()
        ))
    })?;
    let reserve_entries = parse_reserve_entries(rsvmap_data)?;

    // Create the strings section.
    let off_dt_strings = header.off_dt_strings.get() as usize;
    let size_dt_strings = header.size_dt_strings.get() as usize;
    let string_data = verify_slice(&data, off_dt_strings, size_dt_strings)?;

    // Parse the structure block.
    let off_dt_struct = header.off_dt_struct.get() as usize;
    let size_dt_struct = header.size_dt_struct.get() as usize;
    let structure_data = verify_slice(&data, off_dt_struct, size_dt_struct)?;
    let mut struct_offset = 0;
    // When `parse_structure_block` returns, `struct_offset` will contain the number of bytes that
    // were parsed for the structure block.
    let root_node = parse_structure_block(&structure_data, &mut struct_offset, &string_data, 0)?;

    // Verify that the last token is `FDT_END`, skipping any trailing `FDT_NOP` tokens.
    loop {
        let token =
            parse_item_and_increment_offset::<U32<BigEndian>>(&structure_data, &mut struct_offset)?
                .get();
        match token {
            FDT_NOP => continue,
            FDT_END => break,
            token => {
                return Err(ParseError::MalformedStructure(format!(
                    "Expected FDT_END token at the end of structure block, got 0x{:x}",
                    token
                )));
            }
        }
    }

    Ok(Devicetree { header, reserve_entries, root_node })
}

// Verifies that the provided `Header` is a valid header.
fn verify_header(header: &Header) -> Result<(), ParseError> {
    if header.magic.get() != FDT_MAGIC {
        return Err(ParseError::InvalidMagicNumber);
    }
    if header.version.get() < 17 {
        return Err(ParseError::UnsupportedVersion);
    }

    Ok(())
}

/// Creates a slice from `data` from `offset` to `offset + size`.
///
/// Returns an error if the slice is out of bounds of `data`.
fn verify_slice(data: &[u8], offset: usize, size: usize) -> Result<&[u8], ParseError> {
    let end = offset.checked_add(size).ok_or_else(|| {
        ParseError::MalformedStructure(format!(
            "Slice offset 0x{:x} + size 0x{:x} overflowed",
            offset, size
        ))
    })?;
    data.get(offset..end).ok_or_else(|| {
        ParseError::MalformedStructure(format!(
            "Attempted to create slice from 0x{:x} to 0x{:x}, where total length was 0x{:x}",
            offset,
            end,
            data.len()
        ))
    })
}

/// Parses a &T out of `data`.
///
/// Returns an error if the parse failed.
fn parse_item<'a, T: FromBytes + KnownLayout + Immutable>(
    data: &'a [u8],
) -> Result<&'a T, ParseError> {
    let token = T::ref_from_prefix(&data).map_err(|_| ParseError::ZeroCopyError)?;
    Ok(token.0)
}

/// Parses a &T out of `data` at `offset`, and increments `offset` by `size_of::<T>()` if the parse
/// was successful.
///
/// Returns an error if the parse failed.
fn parse_item_and_increment_offset<'a, T: FromBytes + KnownLayout + Immutable>(
    data: &'a [u8],
    offset: &mut usize,
) -> Result<&'a T, ParseError> {
    let slice = data.get(*offset..).ok_or(ParseError::ZeroCopyError)?;
    let r = parse_item(slice)?;
    *offset = offset
        .checked_add(std::mem::size_of::<T>())
        .ok_or_else(|| ParseError::MalformedStructure("Offset overflow".to_string()))?;

    Ok(r)
}

/// Parses a null-terminated string from `data`.
///
/// When the function returns, `offset` is set to the index after the null character, aligned to a
/// 4-byte boundary.
///
/// Returns an error if the string is not null-terminated, or the string contains invalid utf8
/// characters.
fn parse_string(data: &[u8], offset: &mut usize) -> Result<String, ParseError> {
    let str_slice = data.get(*offset..).ok_or(ParseError::ZeroCopyError)?;
    let null_index = str_slice.iter().position(|c| *c == 0).ok_or(ParseError::ZeroCopyError)?;
    *offset = offset
        .checked_add(null_index)
        .and_then(|v| v.checked_add(1))
        .and_then(|v| v.checked_add(3))
        .map(|v| v & !3)
        .ok_or_else(|| ParseError::MalformedStructure("String offset overflow".to_string()))?;

    std::str::from_utf8(&str_slice[..null_index])
        .map_err(|e| ParseError::Utf8Error(e))
        .map(|s| s.to_string())
}

/// Parses the structure block from `data`.
///
/// The `offset` is used for recursive calls, and will be set to the end of the parsed node when
/// the function returns.
///
/// `strings_block` is a reference to the strings data, and offsets read from properties will be
/// used to index into this data.
fn parse_structure_block(
    data: &[u8],
    offset: &mut usize,
    strings_block: &[u8],
    depth: usize,
) -> Result<Node, ParseError> {
    if depth >= MAX_NODE_DEPTH {
        return Err(ParseError::MalformedStructure(
            "Maximum devicetree node depth exceeded".to_string(),
        ));
    }

    let token = parse_item_and_increment_offset::<U32<BigEndian>>(data, offset)?;
    if *token != FDT_BEGIN_NODE {
        return Err(ParseError::MalformedStructure(format!(
            "Expected FDT_BEGIN_NODE token at offset 0x{:x}, got 0x{:x}",
            offset, token
        )));
    }

    let name = parse_string(&data, offset)?;
    let mut node = Node::new(name);

    loop {
        let token = parse_item_and_increment_offset::<U32<BigEndian>>(data, offset)?.get();
        match token {
            token if token == FDT_BEGIN_NODE => {
                // Reset the offset to parse it again in the recursive call.
                *offset -= std::mem::size_of_val(&token);

                let child_node = parse_structure_block(data, offset, strings_block, depth + 1)?;
                node.children.push(child_node);
            }
            token if token == FDT_PROP => {
                let length =
                    parse_item_and_increment_offset::<U32<BigEndian>>(data, offset)?.get() as usize;

                let mut prop_name_offset =
                    parse_item_and_increment_offset::<U32<BigEndian>>(data, offset)?.get() as usize;
                let prop_name = parse_string(&strings_block, &mut prop_name_offset)?;

                // Parse the property value, and align the offset to the next 4-byte boundary.
                let value = verify_slice(data, *offset, length)?.to_vec();
                *offset = offset
                    .checked_add(length)
                    .and_then(|v| v.checked_add(3))
                    .map(|v| v & !3)
                    .ok_or_else(|| {
                        ParseError::MalformedStructure("Property offset overflow".to_string())
                    })?;

                node.properties.push(Property { name: prop_name, value });
            }
            token if token == FDT_END_NODE => {
                return Ok(node);
            }
            token if token == FDT_NOP => {}
            token if token == FDT_END => {
                return Err(ParseError::MalformedStructure("FDT_END inside of node".to_string()));
            }
            token => {
                return Err(ParseError::MalformedStructure(format!(
                    "Expected valid token at offset 0x{:x}, got 0x{:x}",
                    offset, token
                )));
            }
        }
    }
}

/// Parses an array of `ReserveEntry`'s from `data`.
fn parse_reserve_entries(data: &[u8]) -> Result<Vec<ReserveEntry>, ParseError> {
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let slice = data.get(offset..).ok_or_else(|| {
            ParseError::MalformedStructure(
                "Memory reservation map entry too small or misaligned.".to_string(),
            )
        })?;
        let entry: &ReserveEntry = ReserveEntry::ref_from_prefix(slice)
            .map_err(|_| {
                ParseError::MalformedStructure(
                    "Memory reservation map entry too small or misaligned.".to_string(),
                )
            })?
            .0;

        // If both the address and the size are zero, this is the end of the entries.
        if entry.address.get() == 0 && entry.size.get() == 0 {
            return Ok(entries);
        }

        if entries.len() >= MAX_RESERVE_ENTRIES {
            return Err(ParseError::MalformedStructure(
                "Too many memory reservation map entries.".to_string(),
            ));
        }

        entries.push(*entry);
        offset = offset.checked_add(std::mem::size_of::<ReserveEntry>()).ok_or_else(|| {
            ParseError::MalformedStructure("Memory reservation map offset overflow.".to_string())
        })?;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn build_minimal_fdt(
        totalsize: u32,
        off_dt_struct: u32,
        size_dt_struct: u32,
        off_dt_strings: u32,
        size_dt_strings: u32,
        off_mem_rsvmap: u32,
        body: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        buf.extend_from_slice(&totalsize.to_be_bytes());
        buf.extend_from_slice(&off_dt_struct.to_be_bytes());
        buf.extend_from_slice(&off_dt_strings.to_be_bytes());
        buf.extend_from_slice(&off_mem_rsvmap.to_be_bytes());
        buf.extend_from_slice(&17u32.to_be_bytes()); // version
        buf.extend_from_slice(&16u32.to_be_bytes()); // last_comp_version
        buf.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
        buf.extend_from_slice(&size_dt_strings.to_be_bytes());
        buf.extend_from_slice(&size_dt_struct.to_be_bytes());
        buf.extend_from_slice(body);
        buf
    }

    #[test]
    fn test_vim3() {
        let contents = std::fs::read("pkg/test-data/test.dtb").expect("failed to read file");
        crate::parser::parse_devicetree(&contents).expect("failed to parse devicetree");
    }

    #[test]
    fn test_rejects_oob_rsvmap_and_overflowing_slices() {
        // off_mem_rsvmap beyond totalsize must return Err instead of panicking.
        let fdt = build_minimal_fdt(64, 56, 8, 64, 0, 128, &[0u8; 24]);
        assert!(parse_devicetree(&fdt).is_err());

        // off_dt_struct + size_dt_struct overflowing u32 must return Err.
        let fdt_overflow = build_minimal_fdt(64, 56, u32::MAX, 64, 0, 40, &[0u8; 24]);
        assert!(parse_devicetree(&fdt_overflow).is_err());

        // totalsize larger than actual buffer length must return Err.
        let fdt_truncated = build_minimal_fdt(128, 56, 8, 64, 0, 40, &[0u8; 24]);
        assert!(parse_devicetree(&fdt_truncated).is_err());

        // Offsets beyond totalsize must be rejected even if the caller's slice is larger.
        let mut fdt_oversized_buf = build_minimal_fdt(56, 56, 16, 56, 0, 40, &[0u8; 16]);
        fdt_oversized_buf.extend_from_slice(&[0u8; 32]);
        assert!(parse_devicetree(&fdt_oversized_buf).is_err());
    }

    #[test]
    fn test_rejects_deep_nesting_and_missing_fdt_end() {
        // Missing FDT_END after root node must fail.
        let mut struct_no_end = Vec::new();
        struct_no_end.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        struct_no_end.extend_from_slice(&0u32.to_be_bytes()); // empty root name + padding
        struct_no_end.extend_from_slice(&FDT_END_NODE.to_be_bytes());

        let mut body = vec![0u8; 16]; // empty ReserveEntry terminator at offset 40..56
        let struct_len = struct_no_end.len() as u32;
        body.extend_from_slice(&struct_no_end);
        let total = 40 + body.len() as u32;
        let fdt_no_end = build_minimal_fdt(total, 56, struct_len, total, 0, 40, &body);
        assert!(parse_devicetree(&fdt_no_end).is_err());

        // Trailing FDT_NOP tokens before FDT_END must be accepted.
        let mut struct_with_nops = struct_no_end;
        struct_with_nops.extend_from_slice(&FDT_NOP.to_be_bytes());
        struct_with_nops.extend_from_slice(&FDT_NOP.to_be_bytes());
        struct_with_nops.extend_from_slice(&FDT_END.to_be_bytes());
        let mut body_with_nops = vec![0u8; 16];
        let struct_with_nops_len = struct_with_nops.len() as u32;
        body_with_nops.extend_from_slice(&struct_with_nops);
        let total_with_nops = 40 + body_with_nops.len() as u32;
        let fdt_with_nops = build_minimal_fdt(
            total_with_nops,
            56,
            struct_with_nops_len,
            total_with_nops,
            0,
            40,
            &body_with_nops,
        );
        assert!(parse_devicetree(&fdt_with_nops).is_ok());

        // Excessive nesting (> MAX_NODE_DEPTH) must return Err without stack overflow.
        let mut deep_struct = Vec::new();
        for i in 0..70 {
            deep_struct.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
            if i == 0 {
                deep_struct.extend_from_slice(&0u32.to_be_bytes());
            } else {
                deep_struct.extend_from_slice(b"n\0\0\0");
            }
        }
        let mut deep_body = vec![0u8; 16];
        let deep_struct_len = deep_struct.len() as u32;
        deep_body.extend_from_slice(&deep_struct);
        let deep_total = 40 + deep_body.len() as u32;
        let fdt_deep =
            build_minimal_fdt(deep_total, 56, deep_struct_len, deep_total, 0, 40, &deep_body);
        assert!(parse_devicetree(&fdt_deep).is_err());
    }
}
