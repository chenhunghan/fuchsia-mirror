// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UTP transfer request slots: the descriptor layout, their DMA buffers, and the state that
//! submitters, the interrupt thread, the timeout task, and power transitions share.
//!
//! This module holds data and its invariants. It takes no lock and touches no register.
//! [`TransferRequestProcessor`](crate::transfer_request_processor::TransferRequestProcessor)
//! owns the one [`RequestList`] behind its `request_list` mutex (lock 3 in the order at the top
//! of `lib.rs`). It makes every slot, mask, and deadline change, and it programs the list
//! registers and the doorbell through [`UfsController`](crate::controller::UfsController).
//! [`PowerGuard`](crate::power::PowerGuard) records the power state with
//! [`PowerGuard::update_power`](crate::power::PowerGuard::update_power), under the same mutex.
//!
//! Contents:
//!
//! - The UTP Transfer Request Descriptor ([`TransferRequestDescriptor`], UFSHCI 4.0 section
//!   6.1.1) and the DMA buffers the host controller reads and writes: the UTP Transfer Request
//!   List (UTRL) and one UTP Command Descriptor (UCD) per slot. Dropping the list never unpins
//!   them. Only [`RequestList::unpin_all_descriptors`] does, once the controller has stopped.
//! - Per-slot state ([`RequestSlot`]): the [`SlotState`] lifecycle, where the completion goes,
//!   the pinned client pages (`zx::Pmt`), and the deadline that the timeout task watches.
//! - Summary masks (free, scheduled, timeout) that mirror the slot states, so idle checks,
//!   completion scans, and Inspect read one word instead of walking every slot.
//! - List-wide state that must change in the same critical section as a slot reservation:
//!   whether submissions are closing or stopped, the wakers of async tasks waiting for a slot or
//!   admission, and the driver's power state ([`DevicePowerState`]). A submitter checks admission
//!   and registers as a waiter under the lock that a power transition takes to close admission,
//!   so neither can miss the other.

use crate::UfsError;
use crate::constants::{
    MAX_REQUEST_LIST_SIZE, UTP_COMMAND_DESCRIPTOR_SIZE, UTRD_COMMAND_TYPE_MASK,
    UTRD_COMMAND_TYPE_SHIFT, UTRD_DATA_DIRECTION_MASK, UTRD_DATA_DIRECTION_SHIFT,
    UTRD_INTERRUPT_BIT, UTRD_OCS_MASK,
};
use crate::dma_buffer::ContiguousDmaBuffer;
use crate::inspect::BlockIoOp;
use crate::power::DevicePowerState;
use crate::transfer_request_processor::{IoCompletion, ReadBack};
use static_assertions::const_assert_eq;
use std::mem::size_of;
use std::task::Waker;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Bitmask with the low `count` bits set (`0..=64`).
pub const fn slot_mask(count: u8) -> u64 {
    if count >= 64 { u64::MAX } else { (1u64 << count) - 1 }
}

/// Overall Command Status (`OCS`) written by the host controller into UTRD DW2
/// (UFSHCI 3.0/4.0 section 6.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OverallCommandStatus {
    Success = 0x00,
    InvalidCommandTableAttributes = 0x01,
    InvalidPrdtAttributes = 0x02,
    MismatchDataBufferSize = 0x03,
    MismatchResponseUpiuSize = 0x04,
    CommunicationFailure = 0x05,
    Aborted = 0x06,
    HostControllerFatalError = 0x07,
    DeviceFatalError = 0x08,
    InvalidCryptoConfiguration = 0x09,
    GeneralCryptoError = 0x0A,
    /// Written by the driver before ringing the doorbell; the controller overwrites it on
    /// completion.
    Invalid = 0x0F,
}

impl OverallCommandStatus {
    /// Decodes a raw OCS value, returning `None` for reserved values.
    pub fn from_raw(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Success,
            0x01 => Self::InvalidCommandTableAttributes,
            0x02 => Self::InvalidPrdtAttributes,
            0x03 => Self::MismatchDataBufferSize,
            0x04 => Self::MismatchResponseUpiuSize,
            0x05 => Self::CommunicationFailure,
            0x06 => Self::Aborted,
            0x07 => Self::HostControllerFatalError,
            0x08 => Self::DeviceFatalError,
            0x09 => Self::InvalidCryptoConfiguration,
            0x0A => Self::GeneralCryptoError,
            0x0F => Self::Invalid,
            _ => return None,
        })
    }
}

/// UTRD Data Direction (`DD`) field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DataDirection {
    NoData = 0,
    /// Host to device (write).
    HostToDevice = 1,
    /// Device to host (read).
    DeviceToHost = 2,
}

/// UTP Transfer Request Descriptor (32 bytes, UFSHCI 3.0/4.0 section 6.1.1). Little-endian.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable,
)]
#[repr(C)]
pub struct TransferRequestDescriptor {
    /// DW0: Command Type (`CT`, 31:28), Data Direction (`DD`, 26:25), Interrupt (`I`, 24).
    pub dword0: u32,
    /// DW1: Data Unit Number Lower (`DUNL`).
    pub dword1: u32,
    /// DW2: Overall Command Status (`OCS`, 7:0).
    pub dword2: u32,
    /// DW3: Data Unit Number Upper (`DUNU`).
    pub dword3: u32,
    /// DW4: UTP Command Descriptor Base Address (`UCDBA`, 128-byte aligned).
    pub command_descriptor_base_addr: u32,
    /// DW5: UTP Command Descriptor Base Address Upper (`UCDBAU`).
    pub command_descriptor_base_addr_upper: u32,
    /// DW6: Response UPIU Offset (`RUO`, 31:16) and Length (`RUL`, 15:0), both in dwords.
    pub response_upiu_offset_and_length: u32,
    /// DW7: PRDT Offset (`PRDTO`, 31:16, in dwords) and Length (`PRDTL`, 15:0, in entries).
    pub prdt_offset_and_length: u32,
}

const_assert_eq!(size_of::<TransferRequestDescriptor>(), 32);

impl TransferRequestDescriptor {
    /// Returns the Interrupt (`I`) bit.
    #[cfg(test)]
    pub fn interrupt(&self) -> bool {
        (self.dword0 & UTRD_INTERRUPT_BIT) != 0
    }

    /// Sets or clears the Interrupt (`I`) bit.
    pub fn set_interrupt(&mut self, val: bool) {
        if val {
            self.dword0 |= UTRD_INTERRUPT_BIT;
        } else {
            self.dword0 &= !UTRD_INTERRUPT_BIT;
        }
    }

    /// Returns the raw 2-bit Data Direction (`DD`) field.
    #[cfg(test)]
    pub fn data_direction(&self) -> u32 {
        (self.dword0 >> UTRD_DATA_DIRECTION_SHIFT) & UTRD_DATA_DIRECTION_MASK
    }

    /// Sets the Data Direction (`DD`) field.
    pub fn set_data_direction(&mut self, dir: DataDirection) {
        self.dword0 = (self.dword0 & !(UTRD_DATA_DIRECTION_MASK << UTRD_DATA_DIRECTION_SHIFT))
            | ((dir as u32) << UTRD_DATA_DIRECTION_SHIFT);
    }

    /// Returns the Command Type (`CT`) field.
    #[cfg(test)]
    pub fn command_type(&self) -> u8 {
        ((self.dword0 >> UTRD_COMMAND_TYPE_SHIFT) & UTRD_COMMAND_TYPE_MASK) as u8
    }

    /// Sets the Command Type (`CT`) field.
    pub fn set_command_type(&mut self, val: u8) {
        self.dword0 = (self.dword0 & !(UTRD_COMMAND_TYPE_MASK << UTRD_COMMAND_TYPE_SHIFT))
            | ((u32::from(val) & UTRD_COMMAND_TYPE_MASK) << UTRD_COMMAND_TYPE_SHIFT);
    }

    /// Returns the raw Overall Command Status (`OCS`) field.
    #[cfg(test)]
    pub fn overall_command_status(&self) -> u8 {
        (self.dword2 & UTRD_OCS_MASK) as u8
    }

    /// Sets the Overall Command Status (`OCS`) field.
    pub fn set_overall_command_status(&mut self, status: OverallCommandStatus) {
        self.dword2 = (self.dword2 & !UTRD_OCS_MASK) | status as u32;
    }

    /// Returns the Response UPIU offset in dwords.
    #[cfg(test)]
    pub fn response_upiu_offset(&self) -> u16 {
        (self.response_upiu_offset_and_length >> 16) as u16
    }

    /// Returns the Response UPIU length in dwords.
    #[cfg(test)]
    pub fn response_upiu_length(&self) -> u16 {
        (self.response_upiu_offset_and_length & 0xFFFF) as u16
    }

    /// Sets the Response UPIU offset and length, both in dwords.
    pub fn set_response_upiu_offset_and_length(&mut self, offset: u16, length: u16) {
        self.response_upiu_offset_and_length = ((offset as u32) << 16) | length as u32;
    }

    /// Returns the PRDT offset in dwords.
    #[cfg(test)]
    pub fn prdt_offset(&self) -> u16 {
        (self.prdt_offset_and_length >> 16) as u16
    }

    /// Returns the PRDT entry count.
    #[cfg(test)]
    pub fn prdt_length(&self) -> u16 {
        (self.prdt_offset_and_length & 0xFFFF) as u16
    }

    /// Sets the PRDT offset (in dwords) and entry count.
    pub fn set_prdt_offset_and_length(&mut self, offset: u16, length: u16) {
        self.prdt_offset_and_length = ((offset as u32) << 16) | length as u32;
    }
}

/// Lifecycle state of a UTRL slot:
/// `Free -> Reserved -> Scheduled -> (Timeout ->) Free`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Idle and available for reservation.
    Free,
    /// Owned by a submitter (`ReservedSlot`) that has not rung the doorbell yet.
    Reserved,
    /// Doorbell rung; the controller owns the slot until completion, timeout, or `HCE == 0`.
    Scheduled,
    /// Timed out while `UTRLDBR[slot]` remained set. The caller already received
    /// `ZX_ERR_TIMED_OUT`, while the slot and its `zx::Pmt` stay quarantined until the doorbell
    /// clears or `HCE == 0` stops DMA.
    Timeout,
}

/// Completion target and stats metadata for an in-flight (`Scheduled`) UTP command.
#[derive(Debug)]
pub struct SlotCompletion {
    /// LUN the command was submitted for.
    pub lun: u8,
    /// How the command's [`ScsiOutcome`](crate::transfer_request_processor::ScsiOutcome) is
    /// delivered once the slot completes.
    pub completion: IoCompletion,
    /// Block operation recorded in the I/O stats on successful completion.
    pub io_op: Option<BlockIoOp>,
}

/// Per-slot state and UCD buffer.
#[derive(Debug)]
pub struct RequestSlot {
    /// Current slot lifecycle state.
    pub state: SlotState,
    /// UTP Command Descriptor (`UCD`) DMA buffer holding the request UPIU, response UPIU, and
    /// PRDT.
    pub command_descriptor_io: ContiguousDmaBuffer,
    /// Completion state populated while the slot is in [`SlotState::Scheduled`], and `None` while
    /// `Free`, `Reserved`, or `Timeout`.
    pub completion: Option<SlotCompletion>,
    /// Read transfer whose CPU cache is invalidated after DMA completes (`DeviceToHost` only).
    pub read_back: Option<ReadBack>,
    /// SCSI opcode (`cdb[0]`) for SCSI commands, or `0` for administrative Query/NOP requests.
    pub scsi_opcode: u8,
    /// Instant after which the timeout task logs an overdue data slot or times out the admin slot.
    pub deadline: Option<zx::MonotonicInstant>,
    /// Physical Memory Token (`zx::Pmt`) pinning the client data VMO for the current request.
    pub pmt: Option<zx::Pmt>,
}

impl RequestSlot {
    /// Resets the slot to `Free` and takes `(read_back, pmt)` for post-lock cleanup.
    pub fn release(&mut self) -> (Option<ReadBack>, Option<zx::Pmt>) {
        self.state = SlotState::Free;
        self.completion = None;
        self.scsi_opcode = 0;
        self.deadline = None;
        (self.read_back.take(), self.pmt.take())
    }
}

/// UTRL DMA buffer plus per-slot state.
///
/// Dropping the list never unpins: pages the controller might still access are quarantined by
/// the kernel. See [`crate::power::PowerGuard::quiesce`].
#[derive(Debug)]
pub struct RequestList {
    /// UTP Transfer Request List (`UTRL`) DMA buffer of [`TransferRequestDescriptor`]s.
    io_buffer: ContiguousDmaBuffer,
    /// Per-slot state and UTP Command Descriptor (`UCD`) DMA buffers.
    request_slots: Vec<RequestSlot>,
    /// Bitmask of slots currently in [`SlotState::Free`].
    free_mask: u64,
    /// Bitmask of slots currently in [`SlotState::Scheduled`].
    scheduled_mask: u64,
    /// Bitmask of slots currently in [`SlotState::Timeout`].
    timeout_mask: u64,
    /// Set by [`crate::transfer_request_processor::TransferRequestProcessor::close_submissions`];
    /// rejects new reservations while in-flight `Scheduled` slots finish via interrupts.
    closing: bool,
    /// Set once the controller is quiesced; no further submissions or completions run.
    stopped: bool,
    /// Earliest deadline currently armed in the timeout task so newly scheduled slots wake the
    /// timer only when no deadline is armed or the new deadline expires earlier.
    armed_deadline: Option<zx::MonotonicInstant>,
    /// Tasks to wake when any slot is released (async I/O and admin reservations).
    slot_wakers: Vec<Waker>,
    /// The driver's power state, which decides client IO admission.
    ///
    /// Kept under this lock so submitters check admission and register as waiters atomically;
    /// only `PowerGuard` changes it.
    power: DevicePowerState,
}

impl RequestList {
    /// Allocates a list of `slot_count` UTRDs and their UCDs.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` if `slot_count` is 0 or above [`MAX_REQUEST_LIST_SIZE`], or the
    /// DMA allocation error.
    pub fn new(bti: &zx::Bti, slot_count: u8) -> Result<Self, zx::Status> {
        if slot_count == 0 || slot_count > MAX_REQUEST_LIST_SIZE {
            return Err(zx::Status::INVALID_ARGS);
        }
        let io_buffer = ContiguousDmaBuffer::new(
            bti,
            slot_count as usize * size_of::<TransferRequestDescriptor>(),
        )?;
        let mut request_slots = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            request_slots.push(RequestSlot {
                state: SlotState::Free,
                command_descriptor_io: ContiguousDmaBuffer::new(bti, UTP_COMMAND_DESCRIPTOR_SIZE)?,
                completion: None,
                read_back: None,
                scsi_opcode: 0,
                deadline: None,
                pmt: None,
            });
        }
        Ok(Self {
            io_buffer,
            request_slots,
            free_mask: slot_mask(slot_count),
            scheduled_mask: 0,
            timeout_mask: 0,
            closing: false,
            stopped: false,
            armed_deadline: None,
            slot_wakers: Vec::new(),
            power: DevicePowerState::ACTIVE,
        })
    }

    /// Returns the UTRL descriptor DMA buffer.
    pub fn io_buffer(&self) -> &ContiguousDmaBuffer {
        &self.io_buffer
    }

    /// Returns the UTRL descriptor DMA buffer and per-slot array together for split borrows.
    pub fn io_buffer_and_slots_mut(&mut self) -> (&mut ContiguousDmaBuffer, &mut [RequestSlot]) {
        (&mut self.io_buffer, &mut self.request_slots)
    }

    /// Returns the per-slot array for test inspection and fake hardware mapping.
    #[cfg(test)]
    pub fn request_slots(&self) -> &[RequestSlot] {
        &self.request_slots
    }

    /// Bitmask of slots currently in [`SlotState::Free`].
    pub fn free_mask(&self) -> u64 {
        self.free_mask
    }

    /// Bitmask of slots currently in [`SlotState::Scheduled`].
    pub fn scheduled_mask(&self) -> u64 {
        self.scheduled_mask
    }

    /// Bitmask of slots currently in [`SlotState::Timeout`].
    pub fn timeout_mask(&self) -> u64 {
        self.timeout_mask
    }

    /// Bitmask of slots whose DMA buffers may still be accessed by the host controller
    /// (`Scheduled | Timeout`).
    pub fn controller_owned_mask(&self) -> u64 {
        self.scheduled_mask | self.timeout_mask
    }

    /// Returns whether new reservations have been closed by `close_submissions`.
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// Marks new reservations closed.
    pub fn begin_closing(&mut self) {
        self.closing = true;
    }

    /// Returns whether the processor has been stopped by `quiesce`.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Marks the processor stopped, closes new reservations, and clears any armed deadline.
    pub fn mark_stopped(&mut self) {
        self.closing = true;
        self.stopped = true;
        self.armed_deadline = None;
    }

    /// Fails with `IoError` once submissions are closed or the processor is stopped.
    pub fn check_open(&self) -> Result<(), UfsError> {
        if self.stopped || self.closing { Err(UfsError::IoError) } else { Ok(()) }
    }

    /// Returns the earliest deadline currently armed in the timeout task.
    pub fn armed_deadline(&self) -> Option<zx::MonotonicInstant> {
        self.armed_deadline
    }

    /// Updates the armed deadline tracked for the timeout task.
    pub fn set_armed_deadline(&mut self, deadline: Option<zx::MonotonicInstant>) {
        self.armed_deadline = deadline;
    }

    /// Returns the driver's power state.
    pub fn power(&self) -> DevicePowerState {
        self.power
    }

    /// Records the driver's power state.
    pub fn set_power(&mut self, power: DevicePowerState) {
        self.power = power;
    }

    /// Registers `waker` to be woken when a slot is released, unless an equivalent waker is
    /// already queued.
    pub fn register_slot_waker(&mut self, waker: &Waker) {
        if !self.slot_wakers.iter().any(|existing| existing.will_wake(waker)) {
            self.slot_wakers.push(waker.clone());
        }
    }

    /// Wakes every task waiting for a slot to become available.
    pub fn wake_all_slot_wakers(&mut self) {
        for waker in self.slot_wakers.drain(..) {
            waker.wake();
        }
    }

    /// Unpins the UTRL and every slot's UCD buffer once the caller has confirmed `HCE == 0`.
    ///
    /// # Safety
    ///
    /// Host controller DMA must be disabled (`HCE == 0`).
    pub unsafe fn unpin_all_descriptors(&mut self, mut on_error: impl FnMut(zx::Status)) {
        for buffer in std::iter::once(&mut self.io_buffer)
            .chain(self.request_slots.iter_mut().map(|s| &mut s.command_descriptor_io))
        {
            // SAFETY: Guaranteed by the caller (`HCE == 0`).
            if let Err(status) = unsafe { buffer.unpin() } {
                on_error(status);
            }
        }
    }

    pub fn set_slot_state(&mut self, slot: u8, state: SlotState) {
        debug_assert!((slot as usize) < self.request_slots.len());
        let bit = 1u64 << slot;
        self.request_slots[slot as usize].state = state;
        self.free_mask &= !bit;
        self.scheduled_mask &= !bit;
        self.timeout_mask &= !bit;
        match state {
            SlotState::Free => self.free_mask |= bit,
            SlotState::Reserved => {}
            SlotState::Scheduled => self.scheduled_mask |= bit,
            SlotState::Timeout => self.timeout_mask |= bit,
        }
        #[cfg(any(debug_assertions, test))]
        self.assert_masks_consistent();
    }

    pub fn reserve_slot(&mut self, slot: u8) {
        debug_assert_eq!(self.request_slots[slot as usize].state, SlotState::Free);
        self.set_slot_state(slot, SlotState::Reserved);
    }

    #[cfg(any(debug_assertions, test))]
    pub fn assert_masks_consistent(&self) {
        let mut expected_free = 0u64;
        let mut expected_scheduled = 0u64;
        let mut expected_timeout = 0u64;
        for (i, slot) in self.request_slots.iter().enumerate() {
            let bit = 1u64 << i;
            match slot.state {
                SlotState::Free => expected_free |= bit,
                SlotState::Reserved => {}
                SlotState::Scheduled => expected_scheduled |= bit,
                SlotState::Timeout => expected_timeout |= bit,
            }
        }
        debug_assert_eq!(self.free_mask, expected_free, "free_mask out of sync");
        debug_assert_eq!(self.scheduled_mask, expected_scheduled, "scheduled_mask out of sync");
        debug_assert_eq!(self.timeout_mask, expected_timeout, "timeout_mask out of sync");
    }

    pub fn slot(&self, slot: u8) -> Result<&RequestSlot, UfsError> {
        self.request_slots.get(slot as usize).ok_or(UfsError::InvalidSlot(slot as u32))
    }

    pub fn slot_mut(&mut self, slot: u8) -> Result<&mut RequestSlot, UfsError> {
        self.request_slots.get_mut(slot as usize).ok_or(UfsError::InvalidSlot(slot as u32))
    }

    #[cfg(test)]
    pub fn slot_count(&self) -> u8 {
        self.request_slots.len() as u8
    }

    /// Returns the earliest deadline among currently [`SlotState::Scheduled`] slots.
    pub fn next_deadline(&self) -> Option<zx::MonotonicInstant> {
        let mut earliest: Option<zx::MonotonicInstant> = None;
        let mut remaining = self.scheduled_mask;
        while remaining != 0 {
            let slot = &self.request_slots[remaining.trailing_zeros() as usize];
            remaining &= remaining - 1;
            debug_assert_eq!(slot.state, SlotState::Scheduled);
            if let Some(candidate) = slot.deadline {
                earliest = Some(earliest.map_or(candidate, |e| e.min(candidate)));
            }
        }
        earliest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_bti::FakeBti;

    #[fuchsia::test]
    fn test_transfer_request_descriptor_fields() {
        let mut desc = TransferRequestDescriptor::default();
        assert_eq!(desc.data_direction(), DataDirection::NoData as u32);
        desc.set_data_direction(DataDirection::HostToDevice);
        assert_eq!(desc.data_direction(), DataDirection::HostToDevice as u32);
        desc.set_data_direction(DataDirection::DeviceToHost);
        assert_eq!(desc.data_direction(), DataDirection::DeviceToHost as u32);
        desc.set_command_type(1);
        desc.set_interrupt(true);
        assert_eq!(desc.dword0, 0x1500_0000);

        desc.set_overall_command_status(OverallCommandStatus::Invalid);
        assert_eq!(desc.overall_command_status(), 0x0F);
        desc.set_response_upiu_offset_and_length(8, 14);
        assert_eq!(desc.response_upiu_offset_and_length, 0x0008_000E);
        desc.set_prdt_offset_and_length(22, 3);
        assert_eq!((desc.prdt_offset(), desc.prdt_length()), (22, 3));
    }

    #[fuchsia::test]
    fn test_overall_command_status_decode() {
        assert_eq!(OverallCommandStatus::from_raw(0x00), Some(OverallCommandStatus::Success));
        assert_eq!(OverallCommandStatus::from_raw(0x06), Some(OverallCommandStatus::Aborted));
        assert_eq!(
            OverallCommandStatus::from_raw(0x0A),
            Some(OverallCommandStatus::GeneralCryptoError)
        );
        assert_eq!(OverallCommandStatus::from_raw(0x0F), Some(OverallCommandStatus::Invalid));
        assert_eq!(OverallCommandStatus::from_raw(0x0B), None);
    }

    #[fuchsia::test]
    fn test_request_list_creation() {
        let bti = FakeBti::create().expect("Failed to create fake bti");
        let list = RequestList::new(&bti, 32).expect("Failed to create RequestList");
        assert_eq!(list.slot_count(), 32);
        let slot = list.slot(0).unwrap();
        assert_eq!(slot.state, SlotState::Free);
        assert_eq!(slot.command_descriptor_io.size(), 8192);
        assert_eq!(list.slot(32).unwrap_err(), UfsError::InvalidSlot(32));

        assert!(RequestList::new(&bti, 0).is_err());
        assert!(RequestList::new(&bti, 65).is_err());
    }
}
