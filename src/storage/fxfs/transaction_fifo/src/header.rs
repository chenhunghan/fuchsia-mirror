// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use fxfs::object_store::transaction::TRANSACTION_METADATA_MAX_AMOUNT;
use std::fmt;

/// Byte offset from the start of the VMO where the slots region begins.
/// We allocate 64 bytes (one cacheline) at offset 0 for `shared_state`,
/// `available_metadata_space`, and `fifo_state`.
pub const HEADER_SIZE: usize = 64;

/// Byte offset within the 64-byte header where `available_metadata_space` `AtomicU64` is stored.
pub const METADATA_SPACE_OFFSET: usize = 8;

/// Byte offset within the 64-byte header where `fifo_state` `AtomicU64` is stored.
pub const FIFO_STATE_OFFSET: usize = 16;

/// Flag bit in `fifo_state` indicating that the FIFO has been fused closed.
pub const FIFO_STATE_CLOSED: u64 = 1 << 0;

/// The maximum allowed capacity slots (`1 << 15` = 32,768 slots) to prevent 16-bit sequence
/// difference aliasing.
pub const MAX_CAPACITY_SLOTS: u32 = 1 << 15;

/// Trait implemented by transaction types passed into [`crate::FifoWriter`].
/// Indicates whether a transaction requires metadata space reservation or borrows space.
pub trait ReservationBacked {
    /// Returns `true` if this transaction requires allocating metadata space,
    /// or `false` if it borrows existing metadata space (e.g. `unlink` or `update_attributes`).
    fn needs_reservation(&self) -> bool;
}

/// Helper around the single 64-bit atomic coordination word (`shared_state`) at offset 0 of the
/// VMO.
///
/// Bit layout (half-open bit ranges):
/// - Bits 32..64 (32 bits): `layer_seq` (Layer file sequence number)
/// - Bits 16..32 (16 bits): `write_ptr` (Transaction write sequence number / head)
/// - Bits  0..16 (16 bits): `read_ptr`  (Transaction read sequence number / tail)
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SharedStateWord(pub u64);

impl fmt::Debug for SharedStateWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedStateWord")
            .field("layer_seq", &self.layer_seq())
            .field("write_ptr", &self.write_ptr())
            .field("read_ptr", &self.read_ptr())
            .finish()
    }
}

impl SharedStateWord {
    /// Bit shift for the 32-bit `layer_seq` field.
    pub const LAYER_SHIFT: usize = 32;
    /// Bit shift for the 16-bit `write_ptr` field.
    pub const WRITE_SHIFT: usize = 16;
    /// Bit shift for the 16-bit `read_ptr` field.
    pub const READ_SHIFT: usize = 0;

    /// Bit mask for the 32-bit `layer_seq` field.
    pub const LAYER_MASK: u64 = 0xFFFF_FFFF_0000_0000;
    /// Bit mask for the 16-bit `write_ptr` field.
    pub const WRITE_MASK: u64 = 0x0000_0000_FFFF_0000;
    /// Bit mask for the 16-bit `read_ptr` field.
    pub const READ_MASK: u64 = 0x0000_0000_0000_FFFF;

    /// Returns the 32-bit layer file sequence number (`layer_seq`).
    #[inline]
    pub const fn layer_seq(self) -> u32 {
        (self.0 >> Self::LAYER_SHIFT) as u32
    }

    /// Returns the 16-bit write sequence number (`write_ptr`).
    #[inline]
    pub const fn write_ptr(self) -> u16 {
        ((self.0 & Self::WRITE_MASK) >> Self::WRITE_SHIFT) as u16
    }

    /// Returns the 16-bit read sequence number (`read_ptr`).
    #[inline]
    pub const fn read_ptr(self) -> u16 {
        (self.0 & Self::READ_MASK) as u16
    }

    /// Returns a copy of `self` with `write_ptr` incremented by 1 (wrapping at 16 bits).
    #[inline]
    pub const fn with_incremented_write_ptr(self) -> Self {
        let new_write = (self.write_ptr().wrapping_add(1)) as u64;
        Self((self.0 & !Self::WRITE_MASK) | (new_write << Self::WRITE_SHIFT))
    }

    /// Returns a copy of `self` with `layer_seq` incremented by 1 (wrapping at 32 bits).
    #[inline]
    pub const fn with_incremented_layer_seq(self) -> Self {
        let new_layer = (self.layer_seq().wrapping_add(1)) as u64;
        Self((self.0 & !Self::LAYER_MASK) | (new_layer << Self::LAYER_SHIFT))
    }

    /// Returns a copy of `self` with `read_ptr` set to `new_read`.
    #[inline]
    pub const fn with_read_ptr(self, new_read: u16) -> Self {
        Self((self.0 & !Self::READ_MASK) | (new_read as u64))
    }

    /// Returns a copy of `self` with `write_ptr` set to `new_write`.
    #[inline]
    pub const fn with_write_ptr(self, new_write: u16) -> Self {
        Self((self.0 & !Self::WRITE_MASK) | ((new_write as u64) << Self::WRITE_SHIFT))
    }

    /// Returns a copy of `self` with `layer_seq` set to `new_layer`.
    #[inline]
    pub const fn with_layer_seq(self, new_layer: u32) -> Self {
        Self((self.0 & !Self::LAYER_MASK) | ((new_layer as u64) << Self::LAYER_SHIFT))
    }

    /// Returns the number of slots currently used/occupied (`write_ptr - read_ptr`).
    #[inline]
    pub const fn used_slots(self) -> u32 {
        (self.write_ptr().wrapping_sub(self.read_ptr())) as u32
    }

    /// Returns true if the circular buffer is full (`used_slots >= capacity_slots`).
    #[inline]
    pub const fn is_full(self, capacity_slots: u32) -> bool {
        self.used_slots() >= capacity_slots
    }

    /// Returns true if the circular buffer is empty (`used_slots == 0`, `write_ptr == read_ptr`).
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.write_ptr() == self.read_ptr()
    }

    /// Compares 32-bit layer sequence numbers with modular difference to handle wrap-around.
    #[inline]
    pub const fn is_newer_layer(observed: u32, known: u32) -> bool {
        ((observed.wrapping_sub(known)) as i32) > 0
    }
}
