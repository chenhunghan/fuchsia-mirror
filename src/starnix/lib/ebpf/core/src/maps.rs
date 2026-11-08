// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::api::EbpfMapType;

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MapFlags(u32);

#[allow(non_upper_case_globals)]
impl MapFlags {
    pub const NoPrealloc: Self = Self(1 << 0);
    pub const NoCommonLru: Self = Self(1 << 1);
    pub const NumaNode: Self = Self(1 << 2);
    pub const SyscallReadOnly: Self = Self(1 << 3);
    pub const SyscallWriteOnly: Self = Self(1 << 4);
    pub const StackBuildId: Self = Self(1 << 5);
    pub const ZeroSeed: Self = Self(1 << 6);
    pub const ProgReadOnly: Self = Self(1 << 7);
    pub const ProgWriteOnly: Self = Self(1 << 8);
    pub const Clone: Self = Self(1 << 9);
    pub const Mmapable: Self = Self(1 << 10);
    pub const PreserveElems: Self = Self(1 << 11);
    pub const InnerMap: Self = Self(1 << 12);
    pub const Link: Self = Self(1 << 13);
    pub const PathFd: Self = Self(1 << 14);
    pub const VtypeBtfObjFd: Self = Self(1 << 15);
    pub const TokenFd: Self = Self(1 << 16);
    pub const SegvOnFault: Self = Self(1 << 17);
    pub const NoUserConv: Self = Self(1 << 18);

    const ALL_BITS: u32 = Self::NoPrealloc.0
        | Self::NoCommonLru.0
        | Self::NumaNode.0
        | Self::SyscallReadOnly.0
        | Self::SyscallWriteOnly.0
        | Self::StackBuildId.0
        | Self::ZeroSeed.0
        | Self::ProgReadOnly.0
        | Self::ProgWriteOnly.0
        | Self::Clone.0
        | Self::Mmapable.0
        | Self::PreserveElems.0
        | Self::InnerMap.0
        | Self::Link.0
        | Self::PathFd.0
        | Self::VtypeBtfObjFd.0
        | Self::TokenFd.0
        | Self::SegvOnFault.0
        | Self::NoUserConv.0;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self(Self::ALL_BITS)
    }

    pub const fn bits(&self) -> u32 {
        self.0
    }

    pub const fn from_bits(bits: u32) -> Option<Self> {
        if (bits & !Self::ALL_BITS) == 0 { Some(Self(bits)) } else { None }
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::ALL_BITS)
    }

    pub const fn from_bits_retain(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub const fn is_all(&self) -> bool {
        (self.0 & Self::ALL_BITS) == Self::ALL_BITS
    }

    pub const fn intersects(&self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub const fn contains(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub fn set(&mut self, other: Self, value: bool) {
        if value {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn symmetric_difference(self, other: Self) -> Self {
        Self(self.0 ^ other.0)
    }

    pub const fn complement(self) -> Self {
        Self(!self.0 & Self::ALL_BITS)
    }
}

impl core::ops::BitOr for MapFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for MapFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAnd for MapFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl core::ops::BitAndAssign for MapFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl core::ops::BitXor for MapFlags {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        Self(self.0 ^ rhs.0)
    }
}

impl core::ops::BitXorAssign for MapFlags {
    fn bitxor_assign(&mut self, rhs: Self) {
        self.0 ^= rhs.0;
    }
}

impl core::ops::Sub for MapFlags {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 & !rhs.0)
    }
}

impl core::ops::SubAssign for MapFlags {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 &= !rhs.0;
    }
}

impl core::ops::Not for MapFlags {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0 & Self::ALL_BITS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapSchema {
    pub map_type: EbpfMapType,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub flags: MapFlags,
}
