// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT.
// Generated from FIDL library `zither.aliases` by zither, a Fuchsia platform tool.

use zerocopy::{FromBytes, Immutable, IntoBytes, TryFromBytes};

pub type BoolAlias = bool;

pub type Int8Alias = i8;

pub type Int16Alias = i16;

pub type Int32Alias = i32;

pub type Int64Alias = i64;

pub type Uint8Alias = u8;

pub type Uint16Alias = u16;

pub type Uint32Alias = u32;

pub type Uint64Alias = u64;

/// TODO(https://fxbug.dev/42057022): The IR currently does not propagate enough
/// information for bindings to express this type as an alias.
pub const CONST_FROM_ALIAS: u8 = 0xff;

#[repr(transparent)]
#[derive(Clone, Copy, Eq, FromBytes, Hash, Immutable, IntoBytes, PartialEq)]
pub struct Enum(i16);

impl Enum {
    pub const MEMBER: Self = Self(0);

    /// Constructs a value from its underlying integer.
    pub const fn from_raw(raw: i16) -> Self {
        Self(raw)
    }

    /// Returns the underlying integer.
    pub const fn as_raw(self) -> i16 {
        self.0
    }
}

impl From<i16> for Enum {
    fn from(raw: i16) -> Self {
        Self(raw)
    }
}

impl From<Enum> for i16 {
    fn from(val: Enum) -> Self {
        val.0
    }
}

impl core::fmt::Debug for Enum {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::MEMBER => f.write_str("Enum::MEMBER"),

            Self(raw) => f.debug_tuple("Enum").field(&raw).finish(),
        }
    }
}

pub type EnumAlias = Enum;

#[repr(transparent)]
#[derive(Clone, Copy, Eq, FromBytes, Hash, Immutable, IntoBytes, PartialEq)]
pub struct Bits(u16);

impl Bits {
    pub const ONE: Self = Self(1);

    /// Constructs a value from its underlying integer.
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Returns the underlying integer.
    pub const fn as_raw(self) -> u16 {
        self.0
    }
}

impl From<u16> for Bits {
    fn from(raw: u16) -> Self {
        Self(raw)
    }
}

impl From<Bits> for u16 {
    fn from(val: Bits) -> Self {
        val.0
    }
}

impl core::fmt::Debug for Bits {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::ONE => f.write_str("Bits::ONE"),

            Self(raw) => f.debug_tuple("Bits").field(&raw).finish(),
        }
    }
}

pub type BitsAlias = Bits;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Immutable, PartialEq, TryFromBytes)]
pub struct Struct {
    pub x: u64,
    pub y: u64,
    pub e: EnumAlias,
}

pub type StructAlias = Struct;

pub type ArrayAlias = [u32; 4];

pub type NestedArrayAlias = [[Struct; 8]; 4];

/// Alias with a one-line comment.
pub type AliasWithOneLineComment = bool;

/// Alias
///     with
///         a
///           many-line
///             comment.
pub type AliasWithManyLineComment = u8;
