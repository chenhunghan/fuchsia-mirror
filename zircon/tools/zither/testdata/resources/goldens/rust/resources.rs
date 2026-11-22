// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT.
// Generated from FIDL library `zither.resources` by zither, a Fuchsia platform tool.

use zerocopy::{FromBytes, Immutable, IntoBytes};

#[repr(transparent)]
#[derive(Clone, Copy, Eq, FromBytes, Hash, Immutable, IntoBytes, PartialEq)]
pub struct Subtype(u32);

impl Subtype {
    pub const A: Self = Self(0);
    pub const B: Self = Self(1);

    /// Constructs a value from its underlying integer.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the underlying integer.
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

impl From<u32> for Subtype {
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<Subtype> for u32 {
    fn from(val: Subtype) -> Self {
        val.0
    }
}

impl core::fmt::Debug for Subtype {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::A => f.write_str("Subtype::A"),

            Self::B => f.write_str("Subtype::B"),

            Self(raw) => f.debug_tuple("Subtype").field(&raw).finish(),
        }
    }
}

/// This is a handle.
pub type Handle = u32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct StructWithHandleMembers {
    pub untyped_handle: Handle,
    pub handle_a: Handle,
    pub handle_b: Handle,
}
