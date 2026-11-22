// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT.
// Generated from FIDL library `zither.multifile` by zither, a Fuchsia platform tool.

use zerocopy::{FromBytes, Immutable, IntoBytes};

#[repr(transparent)]
#[derive(Clone, Copy, Eq, FromBytes, Hash, Immutable, IntoBytes, PartialEq)]
pub struct A(u32);

impl A {
    pub const MEMBER: Self = Self(0);

    /// Constructs a value from its underlying integer.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the underlying integer.
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

impl From<u32> for A {
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<A> for u32 {
    fn from(val: A) -> Self {
        val.0
    }
}

impl core::fmt::Debug for A {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::MEMBER => f.write_str("A::MEMBER"),

            Self(raw) => f.debug_tuple("A").field(&raw).finish(),
        }
    }
}
