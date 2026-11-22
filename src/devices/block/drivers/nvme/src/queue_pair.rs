// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A paired NVMe submission queue and completion queue.

use crate::commands::{Completion, Submission};
use crate::queue::{DMA_RING_CACHE_OPTIONS, Queue};
use dma_buffer::{DmaU64, PagedBuffer, SharedPagedBuffer};
use fuchsia_runtime::vmar_root_self;
use log::{error, trace, warn};
use mmio::Mmio;
use std::fmt;
use std::sync::Arc;
use zx::sys::zx_paddr_t;
use zx::{Bti, BtiOptions, Pmt, Rights, Status, Vmo, VmoOp, system_get_page_size};

const _: () = assert!(size_of::<usize>() >= size_of::<u32>());
const _: () = assert!(size_of::<u64>() >= size_of::<usize>());

/// The largest number of pages of data that a single command can transfer.
///
/// This keeps the PRP list of a command within the single page that is preallocated for it.
pub const MAX_TRANSFER_PAGES: usize = 256;

/// The largest number of pages that the data of a single command can touch. A transfer of
/// [`MAX_TRANSFER_PAGES`] pages that does not start on a page boundary touches one more page.
pub const MAX_PINNED_PAGES: usize = MAX_TRANSFER_PAGES + 1;

/// The smallest memory page size that a controller can be configured with. See NVM Express Base
/// Specification 2.0, section 3.1.3.5 (CC.MPS).
const MIN_PAGE_SIZE: usize = 4096;

/// The size of a PRP entry. See NVM Express Base Specification 2.0, section 4.1.1 "Physical Region
/// Page Entry and List".
const PRP_ENTRY_SIZE: usize = size_of::<u64>();

/// The low bits of a PRP entry that must be zero, as the offset in it must be dword aligned. See
/// NVM Express Base Specification 2.0, section 4.1.1 "Physical Region Page Entry and List".
const PRP_OFFSET_ALIGNMENT_MASK: u64 = 0x3;

/// The offset of the submission queue tail doorbell in the doorbells of a queue pair.
const SUBMISSION_DOORBELL_OFFSET: usize = 0;

/// The size of a doorbell register.
const DOORBELL_SIZE: usize = size_of::<u32>();

/// The smallest number of entries in a queue. One entry of a queue is always left unused, to tell
/// a full queue from an empty one. See "Queue Size" in NVM Express Base Specification 2.0.
const MIN_QUEUE_ENTRIES: u32 = 2;

// The PRP list of a command holds the address of every page except the first, and must fit in the
// single page that is preallocated for it.
const _: () = assert!((MAX_PINNED_PAGES - 1) * PRP_ENTRY_SIZE <= MIN_PAGE_SIZE);

// Physical addresses are copied into PRP lists as they are, and PRP entries are 64-bit
// little-endian values.
const _: () = assert!(size_of::<zx_paddr_t>() == PRP_ENTRY_SIZE);
const _: () = assert!(cfg!(target_endian = "little"));

/// The direction in which a command transfers data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataDirection {
    /// The controller writes to the memory, as for a read from storage.
    FromDevice,

    /// The controller reads from the memory, as for a write to storage.
    ToDevice,
}

/// The data that a command transfers.
#[derive(Clone, Copy, Debug)]
pub struct DataTransfer<'a> {
    /// Backing unpinned VMO (`Arc::clone`d without a `zx_handle_duplicate` syscall when post-read
    /// cache invalidation is needed on non-x86_64 architectures).
    pub vmo: &'a Arc<Vmo>,

    /// The byte offset of the data within `vmo`, which must be dword aligned.
    pub offset: u64,

    /// The size in bytes of the data, which must be non-zero and at most [`MAX_TRANSFER_PAGES`]
    /// pages.
    pub bytes: usize,

    /// The direction of the transfer.
    pub direction: DataDirection,
}

impl<'a> DataTransfer<'a> {
    /// Constructs a [`DataTransfer`] backed by an unpinned [`Vmo`].
    pub fn vmo(vmo: &'a Arc<Vmo>, offset: u64, bytes: usize, direction: DataDirection) -> Self {
        Self { vmo, offset, bytes, direction }
    }
}

#[derive(Debug)]
struct PostReadCacheInvalidate {
    vmo: Arc<Vmo>,
    offset: u64,
    bytes: u64,
}

/// Deferred post-completion DMA cleanup (cache invalidation + `Pmt::unpin`).
///
/// Created exclusively by [`QueuePair::check_for_new_completion`] once hardware has posted the
/// command's completion queue entry. Callers holding a lock around [`QueuePair`] can move this
/// token out of the critical section so `pmt.unpin()` and post-read cache invalidation execute
/// after releasing the lock.
#[derive(Debug)]
pub struct CompletedTransfer {
    command_id: u16,
    pmt: Option<Pmt>,
    post_read_invalidate: Option<PostReadCacheInvalidate>,
}

impl Drop for CompletedTransfer {
    fn drop(&mut self) {
        if let Some(inv) = self.post_read_invalidate.take()
            && let Err(status) =
                inv.vmo.op_range(VmoOp::CACHE_CLEAN_INVALIDATE, inv.offset, inv.bytes)
        {
            warn!(
                "Failed to invalidate CPU cache after DMA read for command {}: {status}",
                self.command_id
            );
        }

        if let Some(pmt) = self.pmt.take() {
            // SAFETY: `CompletedTransfer` is constructed exclusively in
            // `QueuePair::check_for_new_completion` after observing the hardware completion queue
            // entry for `self.command_id` and removing the transaction from `self.transactions`.
            // By `PinnedTransfer::pmt`'s invariant and the NVM Express Base Specification, only the
            // controller was given these page addresses (for this single command) and hardware has
            // finished transferring its data before posting the completion queue entry.
            if let Err(status) = unsafe { pmt.unpin() } {
                error!("Failed to unpin the data of command {}: {status}", self.command_id);
            }
        }
    }
}

/// An entry that the controller added to the completion queue.
#[derive(Debug)]
pub enum CompletionEvent {
    /// An in-flight command completed and its command ID is available for reuse. Dropping
    /// `cleanup` performs any post-read cache invalidation and unpins the command's `Pmt`.
    Completed {
        /// The command ID of the completed command.
        cid: u16,

        /// The completion queue entry.
        completion: Completion,

        /// Deferred post-DMA cleanup token (`Some` when the command had a [`DataTransfer`]).
        cleanup: Option<CompletedTransfer>,
    },

    /// The entry does not identify an in-flight command.
    Unexpected {
        /// The completion queue entry.
        completion: Completion,
    },
}

/// Data buffer metadata retained for an in-flight DMA transfer.
struct PinnedTransfer {
    /// Keeps the data of the command pinned via `QueuePair`'s BTI.
    // Safety invariant: the addresses of the pages that `pmt` pins have been given to the
    // controller of this queue pair in the PRP entries of this one command, and to nothing else.
    pmt: Pmt,

    /// Retained on non-x86_64 architectures iff `direction == DataDirection::FromDevice` so CPU
    /// cache lines covering `[offset, offset + bytes)` can be invalidated after DMA completes.
    post_read_invalidate: Option<PostReadCacheInvalidate>,
}

/// A command that has been submitted and has not completed.
struct Transaction {
    transfer: Option<PinnedTransfer>,
}

/// Writes `prp_entry` at `entry_index` within page `command_index` of `prp_buffers`.
fn write_prp_entry(
    prp_buffers: &SharedPagedBuffer<DmaU64>,
    command_index: usize,
    entry_index: usize,
    prp_entry: u64,
    page_size: usize,
) {
    let entries_per_page = page_size / PRP_ENTRY_SIZE;
    let index = command_index * entries_per_page + entry_index;
    prp_buffers.as_slice()[index].store_relaxed(prp_entry);
}

/// A submission queue and the completion queue that the controller reports its commands on.
///
/// While the specification allows many submission queues to share a completion queue, this always
/// pairs one with one.
///
/// The memory of the queues, of the PRP lists and of in-flight commands stays pinned until it is
/// explicitly released. Dropping a queue pair leaves all of it pinned.
pub struct QueuePair<M> {
    bti: Bti,
    queue_id: u16,
    submission: Queue<Submission>,
    completion: Queue<Completion>,

    /// In-flight command state indexed by command ID (`0..transactions.len()`).
    // Safety invariant: if `transactions[cid]` is `Some`, it is the only command that has been
    // submitted with command ID `cid` and has neither completed nor been cancelled.
    transactions: Vec<Option<Transaction>>,

    /// Stack of currently unused command IDs (`0..transactions.len()`).
    free_cids: Vec<u16>,

    /// Preallocated PRP list pages (`transactions.len()` pages, one page per command ID), when
    /// enabled.
    // Safety invariant: page `i` (`phys()[i]`) is given only to the controller of this queue pair
    // as `PRP2` of commands submitted with command ID `i`.
    prp_buffers: Option<SharedPagedBuffer<DmaU64>>,

    /// Completion queue entries whose phase tag has this value are new.
    ready_phase: bool,

    /// The last submission queue head that the controller reported.
    submission_head: u32,
    doorbells: M,
    completion_doorbell_offset: usize,
    page_size: usize,

    /// Scratch space for the physical addresses of the pages of a command.
    page_addresses: Vec<zx_paddr_t>,
}

impl<M> fmt::Debug for QueuePair<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePair")
            .field("queue_id", &self.queue_id)
            .field("submission", &self.submission)
            .field("completion", &self.completion)
            .field("in_flight_count", &self.in_flight_count())
            .field("ready_phase", &self.ready_phase)
            .field("submission_head", &self.submission_head)
            .finish_non_exhaustive()
    }
}

impl<M> QueuePair<M> {
    /// Returns the number of commands that have been submitted and have neither completed nor
    /// been cancelled.
    pub fn in_flight_count(&self) -> usize {
        self.transactions.len() - self.free_cids.len()
    }
}

impl<M: Mmio> QueuePair<M> {
    /// Creates the queue pair with ID `queue_id`, with at most `max_entries` entries in each
    /// queue.
    ///
    /// `doorbells` are the doorbell registers of the queue pair: the submission queue tail
    /// doorbell is at offset 0, and the completion queue head doorbell is at offset
    /// `completion_doorbell_offset`, which is `4 << CAP.DSTRD`. See NVM Express PCIe Transport
    /// Specification 1.0b, section 3.1.2.
    ///
    /// If `preallocate_prp` is false, the commands of the queue pair can only transfer data that
    /// is contained in a single page.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` if a queue would have fewer than two entries or if `doorbells` is
    /// misaligned or does not hold both doorbells, or the error from allocating and pinning memory.
    pub fn new(
        bti: &Bti,
        queue_id: u16,
        max_entries: u32,
        doorbells: M,
        completion_doorbell_offset: usize,
        preallocate_prp: bool,
    ) -> Result<Self, Status> {
        let has_doorbells = doorbells.align_offset(DOORBELL_SIZE) == 0
            && completion_doorbell_offset >= DOORBELL_SIZE
            && completion_doorbell_offset.is_multiple_of(DOORBELL_SIZE)
            && completion_doorbell_offset
                .checked_add(DOORBELL_SIZE)
                .is_some_and(|end| end <= doorbells.len());
        if !has_doorbells {
            error!(
                "Queue pair {queue_id} has invalid doorbells (align_offset={}, \
                 completion_offset={completion_doorbell_offset}, len={})",
                doorbells.align_offset(DOORBELL_SIZE),
                doorbells.len()
            );
            return Err(Status::INVALID_ARGS);
        }
        if max_entries < MIN_QUEUE_ENTRIES {
            error!("Queue pair {queue_id} cannot have {max_entries} entries");
            return Err(Status::INVALID_ARGS);
        }

        let page_size = system_get_page_size() as usize;
        let bti = bti.duplicate_handle(Rights::SAME_RIGHTS)?;

        let completion = Queue::<Completion>::new(&bti, max_entries)?;
        let submission = match Queue::<Submission>::new(&bti, max_entries) {
            Ok(submission) => submission,
            Err(status) => {
                // SAFETY: `completion` was just created above and its physical address was never
                // exposed to any hardware.
                if let Err(unpin_status) = unsafe { completion.unpin() } {
                    error!(
                        "Failed to unpin completion queue of queue pair {queue_id}: {unpin_status}"
                    );
                }
                return Err(status);
            }
        };

        let submission_entries = submission.entry_count();
        let completion_entries = completion.entry_count();

        // In NVMe (Base Specification 2.0, section 3.3.1), submission/completion queue
        // circular-buffer slots and active commands (`CID`) have independent lifetimes: an SQ slot
        // can be reused as soon as the controller fetches it (`SQHD` advances), whereas a `CID` and
        // its PRP/DMA state stay active until the command completes. As an implementation choice,
        // we bound `max_commands` to `submission_entries.min(completion_entries - 1)` so `CID`
        // space matches queue depth and a completion queue with `completion_entries` slots
        // (`(tail + 1) % completion_entries == head` denotes full) can never overflow even if all
        // active commands complete before the CQ is drained. `max_entries >= MIN_QUEUE_ENTRIES` was
        // checked above and `Queue::new` clamps to `MAX_QUEUE_ENTRIES` (`65536`), so `max_commands`
        // fits in `1..=65535` (`u16`).
        let max_commands = (submission_entries.min(completion_entries - 1)) as usize;

        let prp_buffers = if preallocate_prp {
            PagedBuffer::new_with_options(
                &bti,
                &vmar_root_self(),
                max_commands * page_size,
                DMA_RING_CACHE_OPTIONS,
            )
            .map(|buf| Some(buf.into_shared()))
        } else {
            Ok(None)
        };

        let prp_buffers = match prp_buffers {
            Ok(prp_buffers) => prp_buffers,
            Err(status) => {
                // SAFETY: `submission` was just created above and its physical address was never
                // exposed to any hardware.
                if let Err(unpin_status) = unsafe { submission.unpin() } {
                    error!(
                        "Failed to unpin submission queue of queue pair {queue_id}: {unpin_status}"
                    );
                }

                // SAFETY: `completion` was just created above and its physical address was never
                // exposed to any hardware.
                if let Err(unpin_status) = unsafe { completion.unpin() } {
                    error!(
                        "Failed to unpin completion queue of queue pair {queue_id}: {unpin_status}"
                    );
                }
                return Err(status);
            }
        };

        let transactions = (0..max_commands).map(|_| None).collect();
        let free_cids = (0..max_commands as u16).rev().collect();

        Ok(Self {
            bti,
            queue_id,

            // The controller starts at the first entry of the submission queue.
            submission_head: 0,
            submission,
            completion,
            transactions,
            free_cids,
            prp_buffers,
            ready_phase: true,
            doorbells,
            completion_doorbell_offset,
            page_size,
            page_addresses: Vec::with_capacity(MAX_PINNED_PAGES),
        })
    }

    /// Returns the ID of this queue pair.
    pub fn queue_id(&self) -> u16 {
        self.queue_id
    }

    /// Returns the submission queue.
    pub fn submission_queue(&self) -> &Queue<Submission> {
        &self.submission
    }

    /// Returns the completion queue.
    pub fn completion_queue(&self) -> &Queue<Completion> {
        &self.completion
    }

    /// Returns the maximum number of concurrent command IDs (`0..capacity()`) supported by this
    /// queue pair.
    pub fn capacity(&self) -> usize {
        self.transactions.len()
    }

    /// Submits `submission` to the controller, and returns the command ID that it was given. The
    /// controller reports the command ID in the completion of the command.
    ///
    /// The command ID and, if `data` is set, the PRP entries of `submission` are filled in. Its
    /// metadata pointer, fused operation and data transfer mode are cleared, as metadata, fused
    /// commands and SGLs are not supported.
    ///
    /// # Errors
    ///
    /// Nothing is submitted on error:
    ///  - `SHOULD_WAIT` if there is no room for another command until a command completes.
    ///  - `INVALID_ARGS` if the size or offset of `data` is not as [`DataTransfer`] requires, or
    ///    if the data spans more than one page and the queue pair has no PRP lists.
    ///  - The error from pinning `data`.
    pub fn submit(
        &mut self,
        mut submission: Submission,
        data: Option<DataTransfer<'_>>,
    ) -> Result<u16, Status> {
        let tail = self.submission.next_index();
        let entry_count = self.submission.entry_count();
        if (tail + 1) % entry_count == self.submission_head {
            return Err(Status::SHOULD_WAIT);
        }

        // Command IDs are allocated from `free_cids` in O(1) and are not tied to submission queue
        // entries, so one slow command cannot block the entry it was submitted in once the queue
        // wraps around.
        let Some(command_id) = self.free_cids.pop() else {
            return Err(Status::SHOULD_WAIT);
        };
        let command_index = usize::from(command_id);

        submission.cdw0.set_cid(command_id);
        submission.cdw0.set_fused(0);
        submission.cdw0.set_data_transfer_mode(0);
        submission.metadata_pointer = 0;

        let transfer = match data {
            Some(data) => match self.prepare_transfer(command_index, &data) {
                Ok(prepared) => {
                    submission.data_pointer = prepared.prps;
                    Some(prepared.transfer)
                }
                Err(status) => {
                    self.free_cids.push(command_id);
                    return Err(status);
                }
            },
            None => None,
        };

        trace!(
            "Submitting command {command_id} to queue {}: opcode={:#x}",
            self.queue_id,
            submission.cdw0.opcode()
        );

        let written_slot = self.submission.write(&submission);
        debug_assert_eq!(written_slot, tail as usize);

        // This upholds the invariants of `transactions` and `PinnedTransfer`: `command_id` was
        // popped from `free_cids` so `transactions[command_index]` was `None`, and `transfer` holds
        // any `pmt` pinned for this command whose addresses are only in `submission.data_pointer`
        // and page `command_index` of `self.prp_buffers`.
        self.transactions[command_index] = Some(Transaction { transfer });

        // The controller reads the submission queue entry, and the PRP list that it may point to,
        // once it sees the new tail. This orders the stores to both before the store to the
        // doorbell.
        self.doorbells.write_barrier();
        self.doorbells.store32(SUBMISSION_DOORBELL_OFFSET, self.submission.next_index());
        Ok(command_id)
    }

    /// Takes the next new entry from the completion queue, if there is one.
    ///
    /// [`QueuePair::ring_completion_db`] must be called after one or more entries have been
    /// taken.
    pub fn check_for_new_completion(&mut self) -> Option<CompletionEvent> {
        let completion = self.completion.pop(&mut self.ready_phase)?;

        let command_id = completion.dw3.command_id();
        let transaction = self.transactions.get_mut(usize::from(command_id)).and_then(Option::take);
        let Some(Transaction { transfer }) = transaction else {
            error!(
                "Queue {} completed command {command_id}, which is not in flight",
                self.queue_id
            );
            return Some(CompletionEvent::Unexpected { completion });
        };

        self.free_cids.push(command_id);

        let submission_head = u32::from(completion.dw2.sq_head());
        if submission_head < self.submission.entry_count() {
            self.submission_head = submission_head;
        } else {
            // Using a head that is not in the queue would let the queue be overfilled.
            warn!(
                "Queue {} reported submission queue head {submission_head}, which is not in the \
                 queue",
                self.queue_id
            );
        }

        let cleanup = transfer.map(|PinnedTransfer { pmt, post_read_invalidate }| {
            CompletedTransfer { command_id, pmt: Some(pmt), post_read_invalidate }
        });

        Some(CompletionEvent::Completed { cid: command_id, completion, cleanup })
    }

    /// Tells the controller that the completion queue entries that have been taken can be reused.
    pub fn ring_completion_db(&mut self) {
        // Ensure all completion queue entry reads retire before the controller sees the updated CQ
        // head doorbell and overwrites consumed completion queue slots.
        self.doorbells.read_barrier();
        self.doorbells.store32(self.completion_doorbell_offset, self.completion.next_index());
    }

    /// Cancels every in-flight command: unpins its data, makes its command ID available for
    /// reuse, and returns the number of cancelled commands.
    ///
    /// The queue pair must not be used to submit commands afterwards, as the controller may still
    /// hold the cancelled entries of the submission queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the controller no longer reads or writes the data of any
    /// command that was submitted to this queue pair and will not do so in future, for example
    /// because the controller was disabled (CC.EN was cleared and CSTS.RDY was then observed to
    /// be clear) after the last command was submitted.
    pub unsafe fn cancel_all(&mut self) -> usize {
        let mut cancelled = 0;
        for (command_id, transaction) in self.transactions.iter_mut().enumerate() {
            let Some(Transaction { transfer }) = transaction.take() else {
                continue;
            };
            if let Some(PinnedTransfer { pmt, .. }) = transfer {
                // SAFETY: `Pmt::unpin` requires that the device no longer accesses the pinned
                // pages. By the invariant of `PinnedTransfer::pmt`, only the controller was given
                // the addresses of these pages, as the data of a command that was submitted to
                // this queue pair. By the `# Safety` section of this function, the controller no
                // longer accesses the data of any such command. `pmt` is consumed, so the pages
                // are not unpinned twice.
                if let Err(status) = unsafe { pmt.unpin() } {
                    error!("Failed to unpin the data of command {command_id}: {status}");
                }
            }
            self.free_cids.push(command_id as u16);
            cancelled += 1;
        }
        debug_assert_eq!(self.free_cids.len(), self.transactions.len());
        cancelled
    }

    /// Unpins the memory of both queues, of the PRP lists, and of any command that is still in
    /// flight.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the controller no longer reads or writes the queues, or the
    /// PRP lists or data of any command that was submitted to this queue pair, and will not do so
    /// in future, for example because the controller was disabled (CC.EN was cleared and CSTS.RDY
    /// was then observed to be clear) after the last command was submitted.
    pub unsafe fn unpin(mut self) {
        // SAFETY: `cancel_all` requires that the controller no longer accesses the data of any
        // command that was submitted to this queue pair, which the caller guarantees.
        let cancelled = unsafe { self.cancel_all() };
        if cancelled > 0 {
            error!("Queue pair {} released with {cancelled} commands in flight", self.queue_id);
        }
        let queue_id = self.queue_id;
        if let Some(prp_buffers) = self.prp_buffers {
            // SAFETY: `SharedPagedBuffer::unpin` requires that hardware no longer accesses the
            // pinned memory. By `prp_buffers`'s invariant, only the controller was given the
            // addresses of these pages as PRP2 lists, and the caller guarantees the controller no
            // longer accesses them.
            if let Err(status) = unsafe { prp_buffers.unpin() } {
                error!("Failed to unpin PRP buffers of queue pair {queue_id}: {status}");
            }
        }

        // SAFETY: `Queue::unpin` requires that the controller no longer accesses the memory of
        // the queue, which the `# Safety` section of this function requires of the caller.
        if let Err(status) = unsafe { self.submission.unpin() } {
            error!("Failed to unpin submission queue of queue pair {queue_id}: {status}");
        }

        // SAFETY: As for the submission queue above.
        if let Err(status) = unsafe { self.completion.unpin() } {
            error!("Failed to unpin completion queue of queue pair {queue_id}: {status}");
        }
    }

    /// Validates, pins, cleans/invalidates CPU caches on non-coherent targets, and populates PRP
    /// entries for `data` in `command_index`.
    fn prepare_transfer(
        &mut self,
        command_index: usize,
        data: &DataTransfer<'_>,
    ) -> Result<PreparedTransfer, Status> {
        let queue_id = self.queue_id;
        let max_bytes = MAX_TRANSFER_PAGES * self.page_size;
        if data.bytes == 0 || data.bytes > max_bytes {
            error!(
                "Queue {queue_id}: invalid transfer size {} (must be non-zero and at most \
                 {max_bytes} bytes)",
                data.bytes
            );
            return Err(Status::INVALID_ARGS);
        }
        let bytes_u64 = data.bytes as u64;
        if data.offset.checked_add(bytes_u64).is_none() {
            error!("Queue {queue_id}: data offset {} overflows with size {bytes_u64}", data.offset);
            return Err(Status::INVALID_ARGS);
        }
        let page_size_u64 = self.page_size as u64;
        let byte_offset = (data.offset % page_size_u64) as usize;
        if (byte_offset as u64) & PRP_OFFSET_ALIGNMENT_MASK != 0 {
            error!("Queue {queue_id}: data offset {} is not dword aligned", data.offset);
            return Err(Status::INVALID_ARGS);
        }

        // A transfer of the largest size that does not start on a page boundary touches
        // `MAX_PINNED_PAGES` pages.
        let page_count = (byte_offset + data.bytes).div_ceil(self.page_size);
        assert!((1..=MAX_PINNED_PAGES).contains(&page_count));

        if self.prp_buffers.is_none() && page_count != 1 {
            error!("Queue {queue_id}: data spans {page_count} pages, but there are no PRP lists");
            return Err(Status::INVALID_ARGS);
        }

        let page_offset = data.offset - (byte_offset as u64);
        let options = match data.direction {
            DataDirection::FromDevice => BtiOptions::PERM_WRITE,
            DataDirection::ToDevice => BtiOptions::PERM_READ,
        };
        let pinned_bytes = (page_count * self.page_size) as u64;

        let mut page_addresses = std::mem::take(&mut self.page_addresses);
        let mut inline_addrs = [0 as zx_paddr_t; 2];
        let result = (|| {
            let addrs: &mut [zx_paddr_t] = if page_count <= 2 {
                &mut inline_addrs[..page_count]
            } else {
                page_addresses.clear();
                page_addresses.resize(page_count, 0);
                &mut page_addresses
            };
            let pmt = self
                .bti
                .pin(options, data.vmo, page_offset, pinned_bytes, addrs)
                .inspect_err(|status| error!("Queue {queue_id}: failed to pin data: {status}"))?;

            if let Err(status) = prepare_vmo_cache(data.vmo, data.offset, bytes_u64, data.direction)
            {
                error!("Queue {queue_id}: failed to prepare data cache: {status}");

                // SAFETY: `pmt` was just pinned above and its physical addresses were never
                // written to any submission queue entry or exposed to hardware.
                if let Err(unpin_status) = unsafe { pmt.unpin() } {
                    error!(
                        "Queue {queue_id}: failed to unpin data after cache error: {unpin_status}"
                    );
                }
                return Err(status);
            }

            let prps = self.setup_prps(command_index, byte_offset, addrs);
            Ok((pmt, prps))
        })();
        self.page_addresses = page_addresses;
        let (pmt, prps) = result?;

        Ok(PreparedTransfer {
            transfer: PinnedTransfer {
                pmt,
                post_read_invalidate: post_read_cache_invalidate(
                    data.vmo,
                    data.offset,
                    bytes_u64,
                    data.direction,
                ),
            },
            prps,
        })
    }

    /// Populates `PRP1` and `PRP2` (writing page `command_index` of `self.prp_buffers` when the
    /// transfer touches 3 or more controller pages) from `paddrs` (`1..=MAX_PINNED_PAGES`
    /// `self.page_size`-aligned physical page base addresses).
    ///
    /// PRP1 holds the first page's base address plus `byte_offset`. PRP2 holds the second page's
    /// address directly when the transfer spans two pages, or the physical address of
    /// `prp_buffers.phys()[command_index]` when the transfer spans three or more pages (NVM Express
    /// Base Specification 2.0, section 4.1.1).
    fn setup_prps(
        &self,
        command_index: usize,
        byte_offset: usize,
        paddrs: &[zx_paddr_t],
    ) -> [u64; 2] {
        debug_assert!((1..=MAX_PINNED_PAGES).contains(&paddrs.len()));
        debug_assert!(byte_offset < self.page_size);
        debug_assert_eq!((byte_offset as u64) & PRP_OFFSET_ALIGNMENT_MASK, 0);

        let prp1 = (paddrs[0] + byte_offset) as u64;
        let prp2 = match paddrs.len() {
            1 => 0,
            2 => paddrs[1] as u64,
            _ => {
                let prp_buffers = self
                    .prp_buffers
                    .as_ref()
                    .expect("PRP buffers verified for multi-page transfer");
                for (entry_index, &addr) in paddrs[1..].iter().enumerate() {
                    write_prp_entry(
                        prp_buffers,
                        command_index,
                        entry_index,
                        addr as u64,
                        self.page_size,
                    );
                }
                prp_buffers.phys()[command_index] as u64
            }
        };
        [prp1, prp2]
    }

    #[cfg(test)]
    fn read_prp_entry(&self, command_index: usize, entry_index: usize) -> u64 {
        let prp_buffers = self.prp_buffers.as_ref().expect("PRP buffers allocated");
        let entries_per_page = self.page_size / PRP_ENTRY_SIZE;
        let index = command_index * entries_per_page + entry_index;
        prp_buffers.as_slice()[index].load_relaxed()
    }
}

#[cfg(target_arch = "x86_64")]
fn prepare_vmo_cache(
    _vmo: &Vmo,
    _offset: u64,
    _bytes: u64,
    _direction: DataDirection,
) -> Result<(), Status> {
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
fn prepare_vmo_cache(
    vmo: &Vmo,
    offset: u64,
    bytes: u64,
    direction: DataDirection,
) -> Result<(), Status> {
    let cache_op = match direction {
        DataDirection::FromDevice => VmoOp::CACHE_CLEAN_INVALIDATE,
        DataDirection::ToDevice => VmoOp::CACHE_CLEAN,
    };
    vmo.op_range(cache_op, offset, bytes)
}

#[cfg(target_arch = "x86_64")]
fn post_read_cache_invalidate(
    _vmo: &Arc<Vmo>,
    _offset: u64,
    _bytes: u64,
    _direction: DataDirection,
) -> Option<PostReadCacheInvalidate> {
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn post_read_cache_invalidate(
    vmo: &Arc<Vmo>,
    offset: u64,
    bytes: u64,
    direction: DataDirection,
) -> Option<PostReadCacheInvalidate> {
    (direction == DataDirection::FromDevice).then(|| PostReadCacheInvalidate {
        vmo: Arc::clone(vmo),
        offset,
        bytes,
    })
}

/// Prepared transfer state and `[PRP1, PRP2]` entries for a submitted command.
struct PreparedTransfer {
    transfer: PinnedTransfer,
    prps: [u64; 2],
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fake_bti::FakeBti;
    use mmio::MmioSplit;
    use mmio::region::MmioRegion;
    use mmio::vmo::{VmoMapping, VmoMemory};
    use zerocopy::IntoBytes;
    use zx::CachePolicy;

    /// The offset of the completion queue head doorbell that the tests use.
    const COMPLETION_DOORBELL_OFFSET: usize = 4;

    /// The value that both doorbells hold until they are rung.
    const NOT_RUNG: u32 = u32::MAX;

    /// Number of entries used by default queue-pair tests.
    const MAX_ENTRIES: u32 = 100;

    /// An arbitrary opcode for commands that transfer data from the device.
    const OPCODE: u8 = 0xa9;

    type TestQueuePair = QueuePair<MmioRegion<VmoMemory>>;

    struct Fixture {
        bti: FakeBti,
        doorbell_vmo: Vmo,
        page_size: usize,
    }

    impl Fixture {
        fn new() -> Self {
            let page_size = system_get_page_size() as usize;
            let doorbell_vmo = Vmo::create(page_size as u64).unwrap();
            let fixture = Self { bti: FakeBti::create().unwrap(), doorbell_vmo, page_size };
            fixture.reset_doorbells();
            fixture
        }

        fn queue_pair(
            &self,
            queue_id: u16,
            max_entries: u32,
            preallocate_prp: bool,
        ) -> TestQueuePair {
            let doorbells = VmoMapping::map_with_cache_policy(
                0,
                COMPLETION_DOORBELL_OFFSET + DOORBELL_SIZE,
                self.doorbell_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                CachePolicy::Cached,
            )
            .unwrap();
            QueuePair::new(
                &self.bti,
                queue_id,
                max_entries,
                doorbells,
                COMPLETION_DOORBELL_OFFSET,
                preallocate_prp,
            )
            .unwrap()
        }

        fn reset_doorbells(&self) {
            self.doorbell_vmo.write([NOT_RUNG, NOT_RUNG].as_bytes(), 0).unwrap();
        }

        /// Returns the values of the submission and completion doorbells.
        fn doorbells(&self) -> (u32, u32) {
            let mut doorbells = [0u32; 2];
            self.doorbell_vmo.read(doorbells.as_mut_bytes(), 0).unwrap();
            (doorbells[0], doorbells[1])
        }

        /// Returns the number of ranges that are pinned.
        fn pinned_count(&self) -> u64 {
            self.bti.info().unwrap().pmo_count
        }

        fn data_vmo(&self, pages: usize) -> Arc<Vmo> {
            Arc::new(Vmo::create((pages * self.page_size) as u64).unwrap())
        }
    }

    fn transfer(vmo: &Arc<Vmo>, offset: u64, bytes: usize) -> DataTransfer<'_> {
        DataTransfer::vmo(vmo, offset, bytes, DataDirection::FromDevice)
    }

    /// Adds an entry to the completion queue, as the controller does.
    fn post_completion(
        pair: &TestQueuePair,
        index: u32,
        command_id: u16,
        phase: bool,
        sq_head: u16,
    ) {
        let mut completion = Completion::default();
        completion.dw3.set_command_id(command_id);
        completion.dw3.set_phase(phase);
        completion.dw2.set_sq_head(sq_head);
        pair.completion.write_at(index, &completion);
    }

    fn fake_address() -> u64 {
        FakeBti::phys_addr() as u64
    }

    #[fuchsia::test]
    fn submit() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        for expected_tail in 1..=2u32 {
            pair.submit(Submission::new(0x9f), None).unwrap();
            assert_eq!(fixture.doorbells(), (expected_tail, NOT_RUNG));
            assert_eq!(pair.submission.read(expected_tail - 1).cdw0.opcode(), 0x9f);
        }
        assert_eq!(pair.in_flight_count(), 2);
    }

    #[fuchsia::test]
    fn submit_clears_unsupported_fields() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        let mut submission = Submission::new(OPCODE);
        submission.metadata_pointer = 0x1000;
        submission.cdw0.set_fused(1);
        submission.cdw0.set_data_transfer_mode(1);
        submission.data_pointer = [0x2000, 0x3000];
        pair.submit(submission, None).unwrap();

        // PRP entries that the caller filled in are kept when no data is passed, as they are for
        // the commands that create queues.
        let submitted = pair.submission.read(0);
        assert_eq!(submitted.metadata_pointer, 0);
        assert_eq!(submitted.cdw0.fused(), 0);
        assert_eq!(submitted.cdw0.data_transfer_mode(), 0);
        assert_eq!(submitted.data_pointer, [0x2000, 0x3000]);
    }

    #[fuchsia::test]
    fn submit_reports_command_id() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        assert_eq!(pair.submit(Submission::new(0), None).unwrap(), 0);
        assert_eq!(pair.submit(Submission::new(0), None).unwrap(), 1);
        assert_eq!(pair.submission.read(0).cdw0.cid(), 0);
        assert_eq!(pair.submission.read(1).cdw0.cid(), 1);
        assert_eq!(pair.in_flight_count(), 2);
    }

    #[fuchsia::test]
    fn submit_when_full() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, 4, false);

        // One entry of the submission queue is always left unused.
        for _ in 0..3 {
            pair.submit(Submission::new(0), None).unwrap();
        }
        let status = pair.submit(Submission::new(0), None).unwrap_err();
        assert_eq!(status, Status::SHOULD_WAIT);
        assert_eq!(pair.in_flight_count(), 3);
        assert_eq!(fixture.doorbells(), (3, NOT_RUNG));
    }

    #[fuchsia::test]
    fn check_completions_nothing_ready() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        assert_matches!(pair.check_for_new_completion(), None);
        assert_eq!(fixture.doorbells(), (NOT_RUNG, NOT_RUNG));
    }

    #[fuchsia::test]
    fn check_completions_one_ready() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        let expected_cid = pair.submit(Submission::new(0), None).unwrap();
        post_completion(&pair, 0, expected_cid, true, 0);

        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, completion, .. })
                if cid == expected_cid
                    && completion.dw3.command_id() == expected_cid
                    && completion.is_success()
        );
        assert_matches!(pair.check_for_new_completion(), None);
        assert_eq!(pair.in_flight_count(), 0);

        // The completion doorbell is only rung when asked.
        assert_eq!(fixture.doorbells(), (1, NOT_RUNG));
        pair.ring_completion_db();
        assert_eq!(fixture.doorbells(), (1, 1));
    }

    #[fuchsia::test]
    fn check_completions_multiple_ready() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        let cid0 = pair.submit(Submission::new(0), None).unwrap();
        let cid1 = pair.submit(Submission::new(0), None).unwrap();
        post_completion(&pair, 0, cid0, true, 0);
        post_completion(&pair, 1, cid1, true, 1);

        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == cid0
        );
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == cid1
        );
        assert_matches!(pair.check_for_new_completion(), None);

        // One ring of the doorbell covers both entries.
        pair.ring_completion_db();
        assert_eq!(fixture.doorbells(), (2, 2));
    }

    #[fuchsia::test]
    fn check_completions_inactive_command() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        let cid0 = pair.submit(Submission::new(0), None).unwrap();
        post_completion(&pair, 0, cid0, true, 0);

        // A second completion for the same command, which is no longer in flight.
        post_completion(&pair, 1, cid0, true, 0);

        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == cid0
        );
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == cid0
        );
        assert_eq!(pair.in_flight_count(), 0);
    }

    #[fuchsia::test]
    fn check_completions_invalid_command_id() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);

        // The first command ID that is out of range, and one that is far out of range.
        let max_commands = u16::try_from(pair.capacity()).unwrap();
        post_completion(&pair, 0, max_commands, true, 0);
        post_completion(&pair, 1, 0x9999, true, 0);

        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == max_commands
        );
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == 0x9999
        );
        assert_matches!(pair.check_for_new_completion(), None);
        assert_eq!(pair.in_flight_count(), 0);
    }

    #[fuchsia::test]
    fn check_completions_ignores_invalid_submission_head() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, 4, false);
        let entry_count = u16::try_from(pair.submission.entry_count()).unwrap();
        assert_eq!(entry_count, 4);

        // The controller reports a submission queue head that is not in the queue.
        pair.submit(Submission::new(0), None).unwrap();
        post_completion(&pair, 0, 0, true, entry_count);
        assert_matches!(pair.check_for_new_completion(), Some(CompletionEvent::Completed { .. }));
        assert_eq!(pair.submission_head, 0);

        // The head is still taken to be entry 0, so the queue fills up once entries 1 and 2 have
        // been used, even though no command is in flight after that.
        pair.submit(Submission::new(0), None).unwrap();
        pair.submit(Submission::new(0), None).unwrap();
        let status = pair.submit(Submission::new(0), None).unwrap_err();
        assert_eq!(status, Status::SHOULD_WAIT);

        // A head that is in the queue is used.
        post_completion(&pair, 1, 1, true, entry_count - 1);
        assert_matches!(pair.check_for_new_completion(), Some(CompletionEvent::Completed { .. }));
        assert_eq!(pair.submission_head, 3);
        pair.submit(Submission::new(0), None).unwrap();
    }

    #[fuchsia::test]
    fn submit_wraps_around_with_slow_command() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, 4, false);

        // The slow command is in entry 0 with command ID 0, and the fast one in entry 1 with
        // command ID 1.
        let slow_cid = pair.submit(Submission::new(0x02), None).unwrap();
        let fast1_cid = pair.submit(Submission::new(0x02), None).unwrap();
        assert_eq!(slow_cid, 0);
        assert_eq!(fast1_cid, 1);

        // The fast command completes first. The controller has consumed entries 0 and 1.
        post_completion(&pair, 0, fast1_cid, true, 2);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == fast1_cid
        );

        // Two more commands use entries 2 and 3 and complete, so that the tail wraps around to
        // entry 0 while command ID 0 is still in flight.
        let fast2_cid = pair.submit(Submission::new(0x02), None).unwrap();
        let fast3_cid = pair.submit(Submission::new(0x02), None).unwrap();
        post_completion(&pair, 1, fast2_cid, true, 3);
        post_completion(&pair, 2, fast3_cid, true, 0);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == fast2_cid
        );
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == fast3_cid
        );
        pair.ring_completion_db();
        assert_eq!(pair.submission.next_index(), 0);

        // The next command goes in entry 0, and gets a free command ID rather than waiting for
        // command ID 0.
        let wrapped_cid = pair.submit(Submission::new(0x02), None).unwrap();
        assert_ne!(wrapped_cid, slow_cid);
        assert_eq!(pair.submission.read(0).cdw0.cid(), wrapped_cid);

        // The completion queue wraps around after entry 3, which inverts the phase tag.
        post_completion(&pair, 3, wrapped_cid, true, 1);
        post_completion(&pair, 0, slow_cid, false, 1);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == wrapped_cid
        );
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == slow_cid
        );
        assert_matches!(pair.check_for_new_completion(), None);
        assert_eq!(pair.in_flight_count(), 0);
    }

    #[fuchsia::test]
    fn check_completions_multi_lap_phase_wraparound() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, 4, false);
        let cq_entries = pair.completion.entry_count();
        let sq_entries = pair.submission.entry_count();
        assert_eq!(cq_entries, 4);
        assert_eq!(sq_entries, 4);

        // Run more than 3 full laps across the 4-entry CQ so `ready_phase` flips
        // `true -> false -> true -> false`.
        for step in 0..14u32 {
            // Before posting the next completion, any stale entry from the previous lap carries
            // the inverted phase tag and must not be consumed.
            assert_matches!(pair.check_for_new_completion(), None);

            let expected_cid = pair.submit(Submission::new(0x02), None).unwrap();
            let cq_index = step % cq_entries;
            let lap = step / cq_entries;
            let phase = lap.is_multiple_of(2);
            let next_sq_head = u16::try_from((step + 1) % sq_entries).unwrap();
            post_completion(&pair, cq_index, expected_cid, phase, next_sq_head);

            assert_matches!(
                pair.check_for_new_completion(),
                Some(CompletionEvent::Completed { cid, .. }) if cid == expected_cid
            );
            assert_matches!(pair.check_for_new_completion(), None);
            pair.ring_completion_db();
            assert_eq!(fixture.doorbells().1, (step + 1) % cq_entries);
        }
        assert_eq!(pair.in_flight_count(), 0);
    }

    #[fuchsia::test]
    fn completion_capacity_bounds_in_flight_commands() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, 4, false);
        assert_eq!(pair.submission.entry_count(), 4);
        assert_eq!(pair.completion.entry_count(), 4);
        assert_eq!(pair.capacity(), 3);

        // Submit 3 commands (command IDs 0, 1, 2).
        for _ in 0..3 {
            pair.submit(Submission::new(0x02), None).unwrap();
        }
        assert_eq!(pair.in_flight_count(), 3);

        // Complete command ID 2 while reporting `sq_head = 3`: hardware has fetched all 3 SQEs so
        // the submission ring now has room for 2 entries, while commands 0 and 1 remain in flight.
        post_completion(&pair, 0, 2, true, 3);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid: 2, .. })
        );
        assert_eq!(pair.in_flight_count(), 2);

        // Submitting one more command reaches `completion_entries - 1 == 3` in flight and advances
        // the SQ tail to 0 (`(0 + 1) % 4 != submission_head (3)` so the SQ itself is not full).
        pair.submit(Submission::new(0x02), None).unwrap();
        assert_eq!(pair.in_flight_count(), 3);
        assert_ne!((pair.submission.next_index() + 1) % pair.submission.entry_count(), 3);

        // A 4th concurrent command must be rejected with `SHOULD_WAIT` so the 4-entry CQ can never
        // overflow if all in-flight commands complete before the CQ is drained.
        let status = pair.submit(Submission::new(0x02), None).unwrap_err();
        assert_eq!(status, Status::SHOULD_WAIT);
    }

    #[fuchsia::test]
    fn unexpected_completion_preserves_submission_head() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, 4, false);

        // Submit two commands (`cid = 0` and `cid = 1`); `cid = 2` remains inactive and
        // `submission_head` is still 0.
        assert_eq!(pair.submit(Submission::new(0), None).unwrap(), 0);
        assert_eq!(pair.submit(Submission::new(0), None).unwrap(), 1);
        assert_eq!(pair.submission_head, 0);

        // Post spurious CQEs carrying `sq_head = 2` for both an inactive in-range CID (`2`) and an
        // out-of-range CID (`99`). Neither may mutate `submission_head`.
        post_completion(&pair, 0, 2, true, 2);
        post_completion(&pair, 1, 99, true, 2);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == 2
        );
        assert_eq!(pair.submission_head, 0);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == 99
        );
        assert_eq!(pair.submission_head, 0);

        // Because `submission_head` stayed at 0, only one more SQ entry (index 2) is available
        // before `(tail + 1) % 4 == submission_head`.
        pair.submit(Submission::new(0), None).unwrap();
        let status = pair.submit(Submission::new(0), None).unwrap_err();
        assert_eq!(status, Status::SHOULD_WAIT);
    }

    #[fuchsia::test]
    fn submit_with_data_one_page() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);
        let vmo = fixture.data_vmo(1);
        let pinned_count = fixture.pinned_count();

        let data = transfer(&vmo, 0, fixture.page_size);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();

        let submitted = pair.submission.read(0);
        assert_eq!(submitted.cdw0.opcode(), OPCODE);
        assert_eq!(submitted.data_pointer, [fake_address(), 0]);
        assert_eq!(fixture.pinned_count(), pinned_count + 1);
        assert_eq!(fixture.doorbells(), (1, NOT_RUNG));
    }

    #[fuchsia::test]
    fn submit_with_data_two_pages() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(2);
        fixture.bti.set_paddrs(&[0x10_0000, 0x30_0000]);

        // The second page is passed in PRP2 itself, rather than in a PRP list.
        let data = transfer(&vmo, 512, fixture.page_size);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();

        let submitted = pair.submission.read(0);
        assert_eq!(submitted.data_pointer, [0x10_0000 + 512, 0x30_0000]);
    }

    #[fuchsia::test]
    fn submit_with_data_three_pages_uses_two_entry_prp_list() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(3);
        fixture.bti.set_paddrs(&[0x10_0000, 0x30_0000, 0x70_0000]);

        // Touching 3 pages is the minimal transfer that requires a PRP list (2 entries in PRP2).
        let data = transfer(&vmo, 256, 2 * fixture.page_size);
        let cid = pair.submit(Submission::new(OPCODE), Some(data)).unwrap();

        let submitted = pair.submission.read(0);
        let command_index = usize::from(cid);
        let prp_list_paddr = pair.prp_buffers.as_ref().unwrap().phys()[command_index] as u64;
        assert_eq!(submitted.data_pointer, [0x10_0000 + 256, prp_list_paddr]);
        assert_eq!(pair.read_prp_entry(command_index, 0), 0x30_0000);
        assert_eq!(pair.read_prp_entry(command_index, 1), 0x70_0000);
    }

    #[fuchsia::test]
    fn submit_with_offset_beyond_first_vmo_page() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(4);
        fixture.bti.set_paddrs(&[0x20_0000, 0x50_0000, 0x90_0000]);

        // Offset starts in page 1 (`page_size + 512`) and touches 3 pages (pages 1, 2, and 3).
        let offset = (fixture.page_size + 512) as u64;
        let data = transfer(&vmo, offset, 2 * fixture.page_size);
        let cid = pair.submit(Submission::new(OPCODE), Some(data)).unwrap();

        let submitted = pair.submission.read(0);
        let command_index = usize::from(cid);
        let prp_list_paddr = pair.prp_buffers.as_ref().unwrap().phys()[command_index] as u64;
        assert_eq!(submitted.data_pointer, [0x20_0000 + 512, prp_list_paddr]);
        assert_eq!(pair.read_prp_entry(command_index, 0), 0x50_0000);
        assert_eq!(pair.read_prp_entry(command_index, 1), 0x90_0000);
    }

    #[fuchsia::test]
    fn submit_to_device_uses_perm_read() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(2);
        vmo.write(&[0xa5; 64], 0).unwrap();
        let read_only_vmo =
            Arc::new(vmo.duplicate_handle(Rights::READ | Rights::MAP | Rights::DUPLICATE).unwrap());
        let pinned_count = fixture.pinned_count();
        fixture.bti.set_paddrs(&[0x20_0000, 0x40_0000]);

        // A `FromDevice` read requires `BtiOptions::PERM_WRITE`, which fails with `ACCESS_DENIED`
        // when `Rights::WRITE` is absent and must roll back the allocated CID.
        let from_dev =
            DataTransfer::vmo(&read_only_vmo, 0, 2 * fixture.page_size, DataDirection::FromDevice);
        let status = pair.submit(Submission::new(OPCODE), Some(from_dev)).unwrap_err();
        assert_eq!(status, Status::ACCESS_DENIED);
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);

        // A `ToDevice` write pins with `BtiOptions::PERM_READ`, succeeding on the same read-only
        // handle and reusing CID 0 that was rolled back above.
        let to_dev =
            DataTransfer::vmo(&read_only_vmo, 0, 2 * fixture.page_size, DataDirection::ToDevice);
        let expected_cid = pair.submit(Submission::new(0x01), Some(to_dev)).unwrap();
        assert_eq!(expected_cid, 0);
        assert_eq!(pair.in_flight_count(), 1);
        assert_eq!(fixture.pinned_count(), pinned_count + 1);
        let submitted = pair.submission.read(0);
        assert_eq!(submitted.data_pointer, [0x20_0000, 0x40_0000]);

        post_completion(&pair, 0, expected_cid, true, 1);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == expected_cid
        );
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
    }

    #[fuchsia::test]
    fn submit_with_data_spanning_pages_without_prp_lists() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);
        let vmo = fixture.data_vmo(2);
        let pinned_count = fixture.pinned_count();

        let data = transfer(&vmo, 512, fixture.page_size);
        let status = pair.submit(Submission::new(OPCODE), Some(data)).unwrap_err();
        assert_eq!(status, Status::INVALID_ARGS);
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
        assert_eq!(fixture.doorbells(), (NOT_RUNG, NOT_RUNG));
    }

    #[fuchsia::test]
    fn submit_with_misaligned_data_offset() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(0, MAX_ENTRIES, false);
        let vmo = fixture.data_vmo(1);
        let pinned_count = fixture.pinned_count();

        // The offset in PRP1 must be dword aligned.
        for offset in [1, 2, 3, 513] {
            let data = transfer(&vmo, offset, 512);
            let status = pair.submit(Submission::new(OPCODE), Some(data)).unwrap_err();
            assert_eq!(status, Status::INVALID_ARGS, "offset={offset}");
        }

        // The failed submissions had no effect and rolled back `free_cids`.
        assert_eq!(pair.submission.next_index(), 0);
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
        assert_eq!(fixture.doorbells(), (NOT_RUNG, NOT_RUNG));

        // An offset that is dword aligned, but not page aligned, is passed on in PRP1.
        let data = transfer(&vmo, 516, 512);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        let submitted = pair.submission.read(0);
        assert_eq!(submitted.data_pointer, [fake_address() + 516, 0]);
        assert_eq!(pair.in_flight_count(), 1);
    }

    #[fuchsia::test]
    fn submit_with_invalid_transfer_size_or_offset_overflow() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let max_transfer_bytes = MAX_TRANSFER_PAGES * fixture.page_size;
        let vmo = fixture.data_vmo(MAX_TRANSFER_PAGES);
        let pinned_count = fixture.pinned_count();

        let invalid = [
            // An empty transfer, which must not pin the page that a non-zero offset falls in.
            (0, 0),
            (512, 0),
            // Transfers that are larger than the maximum, including a page-aligned transfer that
            // would fit in `MAX_PINNED_PAGES` pages.
            (0, max_transfer_bytes + 512),
            (0, max_transfer_bytes + fixture.page_size),
            (512, max_transfer_bytes + fixture.page_size),
            // Sizes for which calculating the number of pages touched would overflow.
            (0, usize::MAX),
            (512, usize::MAX - 511),
            // Offsets where `offset + bytes` overflows `u64`.
            (u64::MAX - 511, 512),
            (u64::MAX, 4),
        ];
        for (offset, bytes) in invalid {
            let data = transfer(&vmo, offset, bytes);
            let status = pair.submit(Submission::new(OPCODE), Some(data)).unwrap_err();
            assert_eq!(status, Status::INVALID_ARGS, "offset={offset} bytes={bytes}");
        }

        // The failed submissions had no effect.
        assert_eq!(pair.submission.next_index(), 0);
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
        assert_eq!(fixture.doorbells(), (NOT_RUNG, NOT_RUNG));

        // A page-aligned transfer of exactly the maximum size is accepted.
        let data = transfer(&vmo, 0, max_transfer_bytes);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        assert_eq!(pair.in_flight_count(), 1);
        assert_eq!(fixture.pinned_count(), pinned_count + 1);
    }

    #[fuchsia::test]
    fn submit_max_transfer_with_page_offset() {
        const OFFSET: u64 = 512;
        const FIRST_PAGE_ADDRESS: usize = 0x1000_0000;

        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let max_transfer_bytes = MAX_TRANSFER_PAGES * fixture.page_size;
        let vmo = fixture.data_vmo(MAX_PINNED_PAGES);

        // Give every page of the data a different address, none of which are adjacent.
        let addresses: Vec<zx_paddr_t> = (0..MAX_PINNED_PAGES)
            .map(|page| FIRST_PAGE_ADDRESS + 2 * page * fixture.page_size)
            .collect();
        fixture.bti.set_paddrs(&addresses);

        // A transfer of the maximum size that does not start on a page boundary touches one more
        // page than `MAX_TRANSFER_PAGES`.
        let data = transfer(&vmo, OFFSET, max_transfer_bytes);
        let command_id = pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        assert_eq!(pair.in_flight_count(), 1);

        // PRP1 is the first page and the offset into it, and PRP2 is the list of the other pages.
        let submitted = pair.submission.read(0);
        let command_index = usize::from(command_id);
        let prp_list_paddr = pair.prp_buffers.as_ref().unwrap().phys()[command_index] as u64;
        assert_eq!(submitted.data_pointer, [(addresses[0] as u64) + OFFSET, prp_list_paddr]);
        for (entry, &address) in addresses[1..].iter().enumerate() {
            assert_eq!(
                pair.read_prp_entry(command_index, entry),
                address as u64,
                "PRP list entry {entry}"
            );
        }
    }

    #[fuchsia::test]
    fn check_completions_releases_transaction() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(1);
        let pinned_count = fixture.pinned_count();

        let data = transfer(&vmo, 0, fixture.page_size);
        let expected_cid = pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        assert_eq!(pair.in_flight_count(), 1);
        assert_eq!(fixture.pinned_count(), pinned_count + 1);

        post_completion(&pair, 0, expected_cid, true, 0);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Completed { cid, .. }) if cid == expected_cid
        );

        // The data has been unpinned and the command ID can be used again, with its PRP list.
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
        assert!(pair.transactions[0].is_none());
        assert!(pair.prp_buffers.is_some());
    }

    #[fuchsia::test]
    fn cancel_all() {
        let fixture = Fixture::new();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(1);
        let pinned_count = fixture.pinned_count();

        // Two commands with data and one without.
        let data = transfer(&vmo, 0, fixture.page_size);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        pair.submit(Submission::new(OPCODE), None).unwrap();
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        assert_eq!(pair.in_flight_count(), 3);
        assert_eq!(fixture.pinned_count(), pinned_count + 2);

        // SAFETY: There is no controller, so nothing accesses the data of these commands.
        let cancelled = unsafe { pair.cancel_all() };
        assert_eq!(cancelled, 3);
        assert_eq!(pair.in_flight_count(), 0);
        assert_eq!(fixture.pinned_count(), pinned_count);
        assert!(pair.transactions.iter().all(Option::is_none));

        // Nothing is in flight any more, so cancelling again returns zero.
        // SAFETY: As above.
        let cancelled = unsafe { pair.cancel_all() };
        assert_eq!(cancelled, 0);

        // A completion for a cancelled command is not matched to a command.
        post_completion(&pair, 0, 0, true, 0);
        assert_matches!(
            pair.check_for_new_completion(),
            Some(CompletionEvent::Unexpected { completion })
                if completion.dw3.command_id() == 0
        );
        assert_eq!(pair.in_flight_count(), 0);
    }

    #[fuchsia::test]
    fn drop_leaves_memory_pinned() {
        let fixture = Fixture::new();
        let empty_count = fixture.pinned_count();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(1);
        let data = transfer(&vmo, 0, fixture.page_size);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        let pinned_count = fixture.pinned_count();
        assert!(pinned_count > empty_count);

        // The controller may still access the memory, so dropping must not unpin any of it.
        drop(pair);
        assert_eq!(fixture.pinned_count(), pinned_count);
    }

    #[fuchsia::test]
    fn unpin() {
        let fixture = Fixture::new();
        let empty_count = fixture.pinned_count();
        let mut pair = fixture.queue_pair(1, MAX_ENTRIES, true);
        let vmo = fixture.data_vmo(1);
        let data = transfer(&vmo, 0, fixture.page_size);
        pair.submit(Submission::new(OPCODE), Some(data)).unwrap();
        assert!(fixture.pinned_count() > empty_count);

        // SAFETY: There is no controller, so nothing accesses the memory of this queue pair.
        unsafe { pair.unpin() };
        assert_eq!(fixture.pinned_count(), empty_count);
    }

    #[fuchsia::test]
    fn new_rejects_missing_or_misaligned_doorbells() {
        let fixture = Fixture::new();
        let empty_count = fixture.pinned_count();
        let doorbells = || {
            VmoMapping::map_with_cache_policy(
                0,
                COMPLETION_DOORBELL_OFFSET + DOORBELL_SIZE + 4,
                fixture.doorbell_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                CachePolicy::Cached,
            )
            .unwrap()
        };

        // The completion doorbell cannot overlap the submission doorbell, be misaligned, or lie
        // outside of the doorbells.
        for offset in [0, 2, 10, 12] {
            let result = QueuePair::new(&fixture.bti, 0, MAX_ENTRIES, doorbells(), offset, false);
            assert_eq!(result.unwrap_err(), Status::INVALID_ARGS, "offset={offset}");
        }

        // Misaligned base doorbell region (`align_offset(4) != 0`) must also be rejected.
        let mut misaligned_doorbells = doorbells().into_split_send();
        let _ = misaligned_doorbells.split_off(2);
        let result = QueuePair::new(
            &fixture.bti,
            0,
            MAX_ENTRIES,
            misaligned_doorbells,
            COMPLETION_DOORBELL_OFFSET,
            false,
        );
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);

        // A queue needs at least two entries.
        let result =
            QueuePair::new(&fixture.bti, 0, 1, doorbells(), COMPLETION_DOORBELL_OFFSET, false);
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
        assert_eq!(fixture.pinned_count(), empty_count);
    }
}
