// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physically contiguous DMA ring memory for a single NVMe submission or completion queue.

use crate::commands::Completion;
use dma_buffer::{
    CacheOptions, ContiguousBuffer, DmaU32, SharedContiguousBuffer, device_read_barrier,
};
use fuchsia_runtime::vmar_root_self;
use std::fmt;
use std::marker::PhantomData;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx::sys::zx_paddr_t;
use zx::{Bti, Status};

const _: () = assert!(size_of::<usize>() >= size_of::<u32>());
const _: () = assert!(size_of::<u64>() >= size_of::<usize>());

/// Maximum number of entries supported by an NVMe I/O submission or completion queue
/// (NVM Express Base Specification 2.0, section 3.3.1: 64K entries).
pub const MAX_QUEUE_ENTRIES: u32 = 65536;

/// Cache policy used for shared NVMe submission/completion rings and PRP list buffers.
// On x86_64, PCIe DMA is hardware cache-coherent, so mapping rings and PRP lists with
// `CacheOptions::Coherent` lets the CPU read and write entries with ordinary `Relaxed` atomic
// loads/stores and zero cache-flush instructions or syscalls. On non-x86_64 architectures (such as
// arm64), PCIe DMA may not snoop CPU caches and 16-byte completion queue entries share cache lines,
// so map rings and PRP lists as `UncachedDevice` (`CacheOptions::Disabled`) for zero-syscall
// coherent access.
#[cfg(target_arch = "x86_64")]
pub const DMA_RING_CACHE_OPTIONS: CacheOptions = CacheOptions::Coherent;

/// Cache policy used for shared NVMe submission/completion rings and PRP list buffers.
#[cfg(not(target_arch = "x86_64"))]
pub const DMA_RING_CACHE_OPTIONS: CacheOptions = CacheOptions::Disabled;

/// A ring of fixed-size entries of type `E` in physically contiguous DMA memory shared with the
/// controller.
///
/// The ring spans one or more physically contiguous pages (`SharedContiguousBuffer`) up to
/// [`MAX_QUEUE_ENTRIES`] entries and stays pinned until [`Queue::unpin`] is called. Dropping the
/// queue without unpinning it leaves its memory pinned so the controller can never access memory
/// that has been reused.
pub struct Queue<E> {
    buffer: SharedContiguousBuffer<DmaU32>,
    entry_count: u32,
    next_index: u32,
    entry: PhantomData<E>,
}

impl<E> fmt::Debug for Queue<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Queue")
            .field("device_address", &self.buffer.phys())
            .field("entry_count", &self.entry_count)
            .field("next_index", &self.next_index)
            .finish()
    }
}

impl<E: FromBytes + IntoBytes + KnownLayout + Immutable> Queue<E> {
    /// Creates a physically contiguous queue with room for `max_entries` entries (clamped to
    /// [`MAX_QUEUE_ENTRIES`]). All entries are initially zero.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` if `max_entries == 0` or `size_of::<E>()` is not a non-zero multiple
    /// of 4 bytes, or the error from allocating and pinning the contiguous DMA buffer.
    pub fn new(bti: &Bti, max_entries: u32) -> Result<Self, Status> {
        let entry_size = size_of::<E>();
        if max_entries == 0 || entry_size == 0 || !entry_size.is_multiple_of(size_of::<u32>()) {
            return Err(Status::INVALID_ARGS);
        }

        let entry_count = max_entries.min(MAX_QUEUE_ENTRIES);
        let byte_size =
            (entry_count as usize).checked_mul(entry_size).ok_or(Status::OUT_OF_RANGE)?;

        // `Vmo::create_contiguous` guarantees zero-filled pages, so every completion queue entry
        // starts with its phase bit cleared to zero without needing a redundant zeroing pass or
        // cache flush.
        let buffer = ContiguousBuffer::new_with_options(
            bti,
            &vmar_root_self(),
            byte_size,
            0,
            DMA_RING_CACHE_OPTIONS,
        )?
        .into_shared();
        Ok(Self { buffer, entry_count, next_index: 0, entry: PhantomData })
    }

    /// Returns the physical address of the queue, for passing to the controller.
    pub fn device_address(&self) -> zx_paddr_t {
        self.buffer.phys()
    }

    /// Returns the number of entries in the queue.
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Returns the index of the next entry in the queue.
    pub fn next_index(&self) -> u32 {
        self.next_index
    }

    /// Moves on to the entry after the next entry, wrapping around at the end of the queue.
    fn advance(&mut self) {
        self.next_index = (self.next_index + 1) % self.entry_count;
    }

    /// Writes `entry` into the current [`Queue::next_index`] slot, advances [`Queue::next_index`]
    /// (wrapping around at the end of the queue), and returns the slot index that was written.
    pub fn write(&mut self, entry: &E) -> usize {
        let slot = self.next_index as usize;
        self.buffer.write_at(slot, entry).expect("entry index in bounds");
        self.advance();
        slot
    }

    /// Returns a copy of the entry at `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not less than [`Queue::entry_count`].
    #[cfg(test)]
    pub fn read(&self, index: u32) -> E {
        self.buffer.read_at(self.checked_index(index)).expect("entry index in bounds")
    }

    /// Overwrites the entry at `index` without changing [`Queue::next_index`] (for simulating
    /// controller completion writes in unit tests).
    #[cfg(test)]
    pub fn write_at(&self, index: u32, entry: &E) {
        self.buffer.write_at(self.checked_index(index), entry).expect("entry index in bounds");
    }

    /// Unpins the memory of the queue.
    ///
    /// # Errors
    ///
    /// Returns the error from unpinning the memory, which is unpinned regardless.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the controller no longer reads or writes the queue memory at
    /// [`Queue::device_address`] and will not do so in future, for example because the controller
    /// was disabled after this queue was created.
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: `SharedContiguousBuffer::unpin` requires that hardware no longer accesses the
        // pinned memory. The only hardware that was given the address of this memory is the
        // controller, and the caller guarantees that the controller no longer accesses it.
        unsafe { self.buffer.unpin() }
    }

    #[cfg(test)]
    fn checked_index(&self, index: u32) -> usize {
        assert!(index < self.entry_count, "index {index} is not in a queue of {self:?}");
        index as usize
    }
}

impl Queue<Completion> {
    /// Pops the next completion queue entry at [`Queue::next_index`] if its phase tag matches
    /// `*ready_phase`, advancing [`Queue::next_index`] and inverting `*ready_phase` when the queue
    /// wraps around to index 0.
    ///
    /// Loads `DW3` first with [`DmaU32::load_relaxed`] and returns `None` immediately without any
    /// barrier when the phase bit does not match `*ready_phase`. Only once `DW3`'s phase bit
    /// matches `*ready_phase` does this call [`device_read_barrier`] before loading `DW0..DW2`,
    /// preventing speculative reads of `DW0..DW2` ahead of the `DW3` load across normal and uncached
    /// device mappings.
    pub fn pop(&mut self, ready_phase: &mut bool) -> Option<Completion> {
        let slot = self.next_index as usize;
        let dwords = &self.buffer.as_slice()[slot * 4..][..4];
        let dw3 = dwords[3].load_relaxed();
        if Completion::phase_from_dword3(dw3) != *ready_phase {
            return None;
        }

        device_read_barrier();
        let [dw0, dw1, dw2] =
            [dwords[0].load_relaxed(), dwords[1].load_relaxed(), dwords[2].load_relaxed()];
        self.advance();
        if self.next_index == 0 {
            // The controller inverts the phase tag each time it wraps around.
            *ready_phase = !*ready_phase;
        }
        Some(Completion::from_dwords([dw0, dw1, dw2, dw3]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{GenericStatus, StatusCodeType, Submission};
    use fake_bti::FakeBti;

    /// An entry that is half as large as a 4 KiB page.
    type HalfPageEntry = [u8; 2048];

    const MAGIC: u8 = 0xab;

    #[fuchsia::test]
    fn capped_to_max_queue_entries() {
        let bti = FakeBti::create().unwrap();
        let queue = Queue::<u32>::new(&bti, MAX_QUEUE_ENTRIES + 100).unwrap();
        assert_eq!(queue.entry_count(), MAX_QUEUE_ENTRIES);
    }

    #[fuchsia::test]
    fn respects_requested_entry_count() {
        let bti = FakeBti::create().unwrap();
        let queue = Queue::<u32>::new(&bti, 100).unwrap();
        assert_eq!(queue.entry_count(), 100);
    }

    #[fuchsia::test]
    fn rejects_empty_or_unaligned_queue() {
        let bti = FakeBti::create().unwrap();
        assert_eq!(Queue::<u32>::new(&bti, 0).unwrap_err(), Status::INVALID_ARGS);
        assert_eq!(Queue::<u8>::new(&bti, 16).unwrap_err(), Status::INVALID_ARGS);
    }

    #[fuchsia::test]
    fn wraps_around() {
        let bti = FakeBti::create().unwrap();
        let mut queue = Queue::<HalfPageEntry>::new(&bti, 2).unwrap();
        assert_eq!(queue.entry_count(), 2);

        // To start with, the next entry is the first entry, and all entries are zero.
        assert_eq!(queue.next_index(), 0);
        assert_eq!(queue.read(0), [0; 2048]);
        assert_eq!(queue.read(1), [0; 2048]);

        assert_eq!(queue.write(&[MAGIC; 2048]), 0);
        assert_eq!(queue.next_index(), 1);
        assert_eq!(queue.write(&[0; 2048]), 1);

        // The queue has wrapped around to the first entry, which is unchanged.
        assert_eq!(queue.next_index(), 0);
        assert_eq!(queue.read(0), [MAGIC; 2048]);
        assert_eq!(queue.read(1), [0; 2048]);
    }

    #[fuchsia::test]
    fn multi_page_submission_queue_wraps_around() {
        let bti = FakeBti::create().unwrap();

        // 256 * 64B = 16 KiB (4 pages on 4 KiB systems).
        let mut queue = Queue::<Submission>::new(&bti, 256).unwrap();
        assert_eq!(queue.entry_count(), 256);

        for i in 0..256u32 {
            let mut sub = Submission::new((i & 0xff) as u8);
            sub.cdw10 = 0x1000 + i;
            assert_eq!(queue.write(&sub), i as usize);
        }
        assert_eq!(queue.next_index(), 0);

        for i in [0u32, 63, 64, 127, 128, 191, 192, 255] {
            let sub = queue.read(i);
            assert_eq!(sub.cdw0.opcode(), (i & 0xff) as u8);
            assert_eq!(sub.cdw10, 0x1000 + i);
        }
    }

    #[fuchsia::test]
    fn pop_checks_phase_tag_and_flips_on_wraparound() {
        let bti = FakeBti::create().unwrap();
        let mut queue = Queue::<Completion>::new(&bti, 2).unwrap();
        let mut ready_phase = true;

        // Zero-initialized entries have phase == false, so ready_phase == true sees None without
        // advancing `next_index`.
        assert_eq!(queue.pop(&mut ready_phase), None);
        assert_eq!(queue.next_index(), 0);
        assert!(ready_phase);

        let mut cqe0 = Completion { command: [0xdead_beef, 0], ..Default::default() };
        cqe0.dw2.set_sq_head(3);
        cqe0.dw2.set_sq_id(1);
        cqe0.dw3.set_command_id(0x1234);
        cqe0.dw3.set_phase(true);
        cqe0.dw3.set_status_code_type(StatusCodeType::Generic);
        cqe0.dw3.set_status_code(GenericStatus::Success as u8);
        queue.write_at(0, &cqe0);

        let mut wrong_phase = false;
        assert_eq!(queue.pop(&mut wrong_phase), None);
        assert_eq!(queue.next_index(), 0);

        let ready = queue.pop(&mut ready_phase).expect("ready CQE 0");
        assert_eq!(ready, cqe0);
        assert_eq!(ready.command[0], 0xdead_beef);
        assert_eq!(ready.dw2.sq_head(), 3);
        assert_eq!(ready.dw2.sq_id(), 1);
        assert_eq!(ready.dw3.command_id(), 0x1234);
        assert!(ready.dw3.phase());
        assert!(ready.is_success());
        assert_eq!(queue.next_index(), 1);
        assert!(ready_phase);

        // Consuming the final entry wraps `next_index` back to 0 and inverts `ready_phase`, so
        // slot 0 (which still has `phase == true`) is not re-consumed until overwritten with
        // `phase == false`.
        let mut cqe1 = Completion::default();
        cqe1.dw3.set_command_id(0x5678);
        cqe1.dw3.set_phase(true);
        queue.write_at(1, &cqe1);

        assert_eq!(queue.pop(&mut ready_phase), Some(cqe1));
        assert_eq!(queue.next_index(), 0);
        assert!(!ready_phase);
        assert_eq!(queue.pop(&mut ready_phase), None);
    }
}
