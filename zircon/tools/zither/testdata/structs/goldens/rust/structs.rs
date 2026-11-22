// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT.
// Generated from FIDL library `zither.structs` by zither, a Fuchsia platform tool.

use bitflags::bitflags;
use zerocopy::{FromBytes, Immutable, IntoBytes, TryFromBytes};

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct Empty {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct Singleton {
    pub value: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct Doubtleton {
    pub first: Singleton,
    pub second: Singleton,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Immutable, PartialEq, TryFromBytes)]
pub struct PrimitiveMembers {
    pub i64: i64,
    pub u64: u64,
    pub i32: i32,
    pub u32: u32,
    pub i16: i16,
    pub u16: u16,
    pub i8: i8,
    pub u8: u8,
    pub b: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct ArrayMembers {
    pub u8s: [u8; 10],
    pub singletons: [Singleton; 6],
    pub nested_arrays1: [[u8; 10]; 20],
    pub nested_arrays2: [[[i8; 1]; 2]; 3],
}

#[repr(transparent)]
#[derive(Clone, Copy, Eq, FromBytes, Hash, Immutable, IntoBytes, PartialEq)]
pub struct Enum(i32);

impl Enum {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(1);

    /// Constructs a value from its underlying integer.
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the underlying integer.
    pub const fn as_raw(self) -> i32 {
        self.0
    }
}

impl From<i32> for Enum {
    fn from(raw: i32) -> Self {
        Self(raw)
    }
}

impl From<Enum> for i32 {
    fn from(val: Enum) -> Self {
        val.0
    }
}

impl core::fmt::Debug for Enum {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::ZERO => f.write_str("Enum::ZERO"),

            Self::ONE => f.write_str("Enum::ONE"),

            Self(raw) => f.debug_tuple("Enum").field(&raw).finish(),
        }
    }
}

#[repr(C)]
#[derive(
    Immutable, IntoBytes, FromBytes, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct Bits(u16);

bitflags! {
    impl Bits : u16 {
        const ONE = 1 << 0;
        const TWO = 1 << 1;
  }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Immutable, PartialEq, TryFromBytes)]
pub struct EnumAndBitsMembers {
    pub e: Enum,
    pub b: Bits,
}

/// Struct with a one-line comment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Immutable, PartialEq, TryFromBytes)]
pub struct StructWithOneLineComment {
    /// Struct member with one-line comment.
    pub member_with_one_line_comment: u32,

    /// Struct member
    ///     with a
    ///         many-line
    ///           comment.
    pub member_with_many_line_comment: bool,
}

/// Struct
///
///     with a
///         many-line
///           comment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct StructWithManyLineComment {
    pub member: u16,
}
