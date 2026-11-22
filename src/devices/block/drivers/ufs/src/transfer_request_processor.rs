// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UTP transfer request submission and completion (UFSHCI 3.0/4.0 section 7.2) on the
//! single-doorbell UTRL (`UTRLDBR`).
//!
//! Two submission models share the slot pool:
//!
//! - Synchronous I/O ([`TransferRequestProcessor::submit_io`]) for block requests: the calling
//!   thread validates the transfer, reserves a slot (blocking for pushback when none is free),
//!   pins the buffer and builds the PRDT, writes the UCD/UTRD (DMA descriptors), rings the
//!   doorbell and returns. The completion path is interrupt-driven: the interrupt handler decodes
//!   the response, frees the slot and delivers the outcome through an [`IoCompletion`].
//! - Async UPIUs (`send_scsi_command`, `send_query_upiu`, `send_nop_out`) for
//!   administrative and driver-internal commands, which await the completion interrupt through a
//!   oneshot channel fed by the unified completion path.
//!
//! Slot ownership (`Free -> Reserved -> Scheduled -> Free`): a submitter owns a slot through a
//! [`ReservedSlot`] while `Reserved`. Once scheduled, the controller owns the slot until its
//! doorbell bit clears or `HCE` reads `0`. Client pages are unpinned only after DMA stops (and
//! read caches are invalidated), or quarantined without `unpin()` if `HCE = 0` times out.
//!
//! Data slots (`0..30`) are never aborted on elapsed time: unpinning a live PRDT risks DMA into
//! freed pages, and returning `ZX_ERR_TIMED_OUT` on a transient link delay forces Fxfs to
//! terminate its journal. Instead, overdue slots log controller and UIC error registers on each
//! [`COMMAND_TIMEOUT`] window while DMA stays pinned until hardware completes or `HCE` is
//! disabled. The admin slot (used synchronously for internal control commands during init,
//! discovery, and power transitions) returns `ZX_ERR_TIMED_OUT` after `COMMAND_TIMEOUT` while
//! keeping the slot quarantined in [`SlotState::Timeout`] with its DMA buffer pinned until
//! `UTRLDBR[31]` clears or `HCE == 0`.
//!
//! Lock order: see the top of `lib.rs`.

pub use crate::inspect::{BlockIoOp, BlockIoStats};

use crate::constants::{
    ADMIN_SLOT, COMMAND_TIMEOUT, COMMAND_TYPE_UFS_STORAGE, DWORD_ALIGN_MASK, INLINE_DATA_OFFSET,
    INLINE_DATA_SIZE, MAX_PRDT_ENTRIES, MAX_REQUEST_LIST_SIZE, OCS_OFFSET, OCS_RETRIES,
    REGISTER_POLL_INTERVAL, SCSI_RETRIES, SCSI_RETRY_DELAY, SDB_SLOTS,
    SENSE_DESCRIPTOR_FORMAT_CURRENT, SENSE_DESCRIPTOR_FORMAT_DEFERRED, SENSE_EOM, SENSE_FILEMARK,
    SENSE_FIXED_FORMAT_CURRENT, SENSE_FIXED_FORMAT_DEFERRED, SENSE_ILI, SENSE_RESPONSE_CODE_MASK,
    SMALL_TRANSFER_PAGES, TEST_UNIT_READY_ATTEMPTS,
};
use crate::controller::UfsController;
use crate::dma_buffer::ContiguousDmaBuffer;
use crate::inspect::TimeoutRegisterSnapshot;
use crate::power::{DevicePowerState, PowerGuard, PowerHooks};
use crate::query::QueryUpiu;
use crate::registers::{
    HostControllerStatusReg, HostUicErrorCodeDataLinkLayerReg, HostUicErrorCodeDmeReg,
    HostUicErrorCodeNetworkLayerReg, HostUicErrorCodePhyAdapterLayerReg,
    HostUicErrorCodeTransportLayerReg, InterruptEnableReg, InterruptStatusReg,
    UtrListBaseAddressReg, UtrListBaseAddressUpperReg, UtrListDoorBellReg, UtrListRunStopReg,
};
use crate::request_list::{
    self, DataDirection, OverallCommandStatus, RequestList, SlotCompletion, SlotState,
    TransferRequestDescriptor,
};
use crate::scsi::{ScsiOpcode, ScsiStatus, SenseKey};
use crate::transfer::{self, CommandDescriptorLayout, PrdtEntry};
use crate::upiu::{
    CommandUpiu, NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode,
};
use crate::{UfsError, uic};
use block_server::RequestId;
use fuchsia_async as fasync;
use fuchsia_sync::{Condvar, Mutex, MutexGuard};
use futures::channel::oneshot;
use futures::lock::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};
use futures::task::AtomicWaker;
use futures::{FutureExt, select_biased};
use log::{error, info, warn};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
use std::fmt;
use std::future::poll_fn;
use std::hint::spin_loop;
use std::pin::pin;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

/// Bitmask of all slots in the request list (`0..SDB_SLOTS`).
const ALL_SLOTS_MASK: u64 = request_list::slot_mask(SDB_SLOTS as u8);
/// Bitmask of usable I/O slots (`0..ADMIN_SLOT`).
const IO_SLOT_MASK: u64 = request_list::slot_mask(ADMIN_SLOT);

/// Guarantees an atomic waiter or transaction counter stays incremented while held so notifiers
/// holding the waited condition's lock never miss the caller.
///
/// Created by [`Self::new`] while holding that lock and before the final check. Dropping it
/// decrements the counter lock-free with `Release` ordering, pairing with `Acquire` loads.
// ORDERING: The lock orders every increment before a notifier's read. The lock-free decrement is
// `Release` and readers load with `Acquire`, so a reader that sees the count fall also sees the
// work the guard covered.
struct CountGuard<'a>(&'a AtomicUsize);

impl<'a> CountGuard<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Release);
        Self(count)
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        let prev = self.0.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0);
    }
}

/// Direction of a non-empty [`DataTransfer`] (`NoData` is unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Read,
    Write,
}

impl TransferDirection {
    pub const fn to_data_direction(self) -> DataDirection {
        match self {
            Self::Read => DataDirection::DeviceToHost,
            Self::Write => DataDirection::HostToDevice,
        }
    }

    fn pin_options(self) -> zx::BtiOptions {
        match self {
            Self::Write => zx::BtiOptions::PERM_READ,
            Self::Read => zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE,
        }
    }
}

/// Backing storage for a [`DataTransfer`].
#[derive(Clone, Debug)]
enum TransferBuffer<'a> {
    Vmo(Arc<zx::Vmo>),
    Inline(&'a [u8]),
}

/// Non-empty data phase of a UTP command (`Arc<zx::Vmo>` DMA or inline UCD parameter bytes).
#[derive(Clone, Debug)]
pub struct DataTransfer<'a> {
    buffer: TransferBuffer<'a>,
    offset: u64,
    length: u32,
    logical_bytes: u64,
    direction: TransferDirection,
}

impl DataTransfer<'static> {
    pub fn read(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self {
            buffer: TransferBuffer::Vmo(vmo),
            offset,
            length,
            logical_bytes: u64::from(length),
            direction: TransferDirection::Read,
        }
    }

    pub fn write(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self {
            buffer: TransferBuffer::Vmo(vmo),
            offset,
            length,
            logical_bytes: u64::from(length),
            direction: TransferDirection::Write,
        }
    }
}

impl<'a> DataTransfer<'a> {
    pub fn inline_write(bytes: &'a [u8]) -> Self {
        Self {
            buffer: TransferBuffer::Inline(bytes),
            offset: 0,
            length: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            logical_bytes: bytes.len() as u64,
            direction: TransferDirection::Write,
        }
    }

    pub fn inline_unmap(bytes: &'a [u8], unmapped_bytes: u64) -> Self {
        Self { logical_bytes: unmapped_bytes, ..Self::inline_write(bytes) }
    }

    fn pin_range(&self, page_size: u64) -> Result<(u64, u64), UfsError> {
        let start = self.offset & !(page_size - 1);
        let end = self
            .offset
            .checked_add(u64::from(self.length))
            .and_then(|end| end.checked_next_multiple_of(page_size))
            .ok_or(UfsError::InvalidParameter)?;
        Ok((start, end - start))
    }
}

/// Read-buffer range whose CPU data cache must be invalidated after `DeviceToHost` DMA.
#[derive(Debug)]
pub struct ReadBack {
    vmo: Arc<zx::Vmo>,
    offset: u64,
    length: u32,
}

impl ReadBack {
    fn invalidate_cache(&self) -> Result<(), zx::Status> {
        if self.length == 0 {
            return Ok(());
        }
        self.vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, self.offset, u64::from(self.length))
    }
}

/// How the outcome of a UTP command is delivered.
pub enum IoCompletion {
    /// A block server request, reported to the [`IoCompleter`] installed with
    /// [`TransferRequestProcessor::set_io_completer`]. Carries no per-request allocation.
    Request(RequestId),
    /// Reported through a oneshot channel (async UTP commands and tests).
    Oneshot(oneshot::Sender<ScsiOutcome>),
}

impl fmt::Debug for IoCompletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(id) => f.debug_tuple("Request").field(id).finish(),
            Self::Oneshot(_) => f.write_str("Oneshot"),
        }
    }
}

/// Receives the outcomes of [`IoCompletion::Request`] submissions.
pub trait IoCompleter: Send + Sync {
    /// Reports the outcome of `request_id`, which was submitted for `lun`.
    fn complete(&self, lun: u8, request_id: RequestId, status: Result<(), zx::Status>);
}

/// One slot's finishing work, held until the request list lock is released.
struct DeferredCompletion {
    /// Read buffer to cache-invalidate before the client sees the data.
    read_back: Option<ReadBack>,
    /// Pinned client pages. Unpinned only once the controller can no longer access them.
    pmt: Option<zx::Pmt>,
    /// `None` when nobody waits for the outcome (for example a reclaimed timed-out slot).
    completion: Option<SlotCompletion>,
    outcome: ScsiOutcome,
}

/// Completions collected under the request list lock and run after it is released.
///
/// Cache invalidation and unpinning are system calls and completion calls into the client
/// (`IoCompleter` or oneshot), so none of them run under the lock. Holds at most one entry per
/// slot. Dropping a non-empty list quarantines its PMTs instead of unpinning.
struct DeferredCompletions<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    inline: [Option<DeferredCompletion>; MAX_REQUEST_LIST_SIZE as usize],
    len: usize,
}

impl<'a, M: Mmio> DeferredCompletions<'a, M> {
    fn new(processor: &'a TransferRequestProcessor<M>) -> Self {
        Self { processor, inline: [const { None }; MAX_REQUEST_LIST_SIZE as usize], len: 0 }
    }

    fn push(
        &mut self,
        read_back: Option<ReadBack>,
        pmt: Option<zx::Pmt>,
        completion: Option<SlotCompletion>,
        outcome: ScsiOutcome,
    ) {
        self.inline[self.len] = Some(DeferredCompletion { read_back, pmt, completion, outcome });
        self.len += 1;
    }

    fn drain(&mut self) -> impl Iterator<Item = DeferredCompletion> + '_ {
        let len = self.len;
        self.len = 0;
        self.inline[..len].iter_mut().filter_map(Option::take)
    }

    /// Invalidates read caches, unpins PMTs, and delivers sync/async outcomes outside the lock.
    ///
    /// # Safety
    ///
    /// The controller must no longer access any buffer in `self` (doorbell bit 0 or `HCE == 0`).
    unsafe fn complete_after_dma(mut self) {
        let processor = self.processor;
        for DeferredCompletion { read_back, pmt, completion, mut outcome } in self.drain() {
            if let Some(rb) = read_back
                && let Err(err) = rb.invalidate_cache()
            {
                warn!(err:%; "Failed to invalidate transfer buffer cache after read");
                if matches!(outcome, ScsiOutcome::Good) {
                    outcome = ScsiOutcome::Fail(UfsError::IoError);
                }
            }
            // SAFETY: Guaranteed by the caller.
            unsafe { unpin_pmt(pmt) };
            if let Some(completion) = completion {
                processor.complete_io(completion, outcome);
            }
        }
    }

    /// Quarantines any pinned `zx::Pmt` handles (dropping without `unpin()`) and delivers `IO`
    /// errors without touching potentially live DMA read buffers.
    fn quarantine(&mut self) {
        let processor = self.processor;
        for DeferredCompletion { read_back: _, pmt, completion, outcome: _ } in self.drain() {
            if let Some(pmt) = pmt {
                // ORDERING: Standalone monotonic counter read by tests.
                processor.quarantined_pmt_count.fetch_add(1, Ordering::Relaxed);
                drop(pmt);
            }
            if let Some(completion) = completion {
                processor.complete_io(completion, ScsiOutcome::Fail(UfsError::IoError));
            }
        }
    }
}

impl<M: Mmio> Drop for DeferredCompletions<'_, M> {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        warn!("Deferred completions dropped without DMA stop confirmation; quarantining PMTs");
        self.quarantine();
    }
}

/// Unpins a PMT once the controller can no longer access its pages.
///
/// # Safety
///
/// The controller must no longer access the pinned pages: their doorbell bit reads 0 after
/// scheduling, the request was never submitted, or `HCE` reads 0.
unsafe fn unpin_pmt(pmt: Option<zx::Pmt>) {
    if let Some(pmt) = pmt {
        // SAFETY: Guaranteed by the caller.
        if let Err(status) = unsafe { pmt.unpin() } {
            error!(status:%; "Failed to unpin transfer buffer");
        }
    }
}

/// Converts a UCD byte offset or length into the dword count stored in a UTRD field.
///
/// # Panics
///
/// Panics if `bytes` is not a multiple of 4 or does not fit a 16-bit dword count. Callers pass
/// [`CommandDescriptorLayout`] values, which are bounded at compile time.
fn dwords(bytes: usize) -> u16 {
    assert_eq!(bytes % 4, 0, "UCD offset or length {bytes} is not dword aligned");
    u16::try_from(bytes / 4).expect("UCD offset or length exceeds a 16-bit dword count")
}

/// Owns a [`SlotState::Reserved`] slot until `schedule_slot_locked` transitions it to `Scheduled`.
#[must_use = "dropping the guard immediately releases the reserved slot"]
pub struct ReservedSlot<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    slot: u8,
    armed: bool,
}

impl<M: Mmio> ReservedSlot<'_, M> {
    pub fn slot(&self) -> u8 {
        self.slot
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<M: Mmio> Drop for ReservedSlot<'_, M> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = self.slot;
        let mut list = self.processor.request_list.lock();
        if let Ok(request_slot) = list.slot(slot) {
            debug_assert_eq!(
                request_slot.state,
                SlotState::Reserved,
                "Armed ReservedSlot must be Reserved",
            );
            debug_assert!(request_slot.pmt.is_none() && request_slot.read_back.is_none());
            if request_slot.state == SlotState::Reserved {
                self.processor.release_reserved_slot_locked(&mut list, slot);
            }
        } else {
            debug_assert!(false, "ReservedSlot holds an out-of-range slot index {slot}");
        }
    }
}

#[must_use = "dropping the guard immediately releases the admin slot and lock"]
struct AdminSlotGuard<'a, M: Mmio> {
    slot: ReservedSlot<'a, M>,
    _lock: AsyncMutexGuard<'a, ()>,
}

/// Guarantees a multi-command client sequence (such as RPMB `SECURITY PROTOCOL OUT`/`IN`) stays
/// counted in `active_client_transactions` so [`TransferRequestProcessor::drain_inflight`] waits
/// before suspending. Created by [`TransferRequestProcessor::begin_client_transaction`] once
/// admission passes. Dropping it releases its inner [`CountGuard`], decrementing the count.
///
/// Client queries and SCSI commands are sent through this guard, so a client path cannot send
/// one without waiting for admission.
#[must_use = "dropping the guard ends the active client transaction"]
pub struct ClientTransactionGuard<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    _count: CountGuard<'a>,
}

impl<M: Mmio> ClientTransactionGuard<'_, M> {
    /// Sends a client QUERY REQUEST UPIU on the admin slot and returns the QUERY RESPONSE UPIU.
    /// The `Query Response` code is not checked; callers interpret it.
    pub async fn send_query_upiu(&self, query_request: &QueryUpiu) -> Result<QueryUpiu, UfsError> {
        self.processor.send_query_upiu(query_request).await
    }

    /// Sends a client SCSI command to `lun` and awaits its completion, transferring `data` (if
    /// any) to or from the device.
    pub async fn send_scsi_command(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
    ) -> Result<(), UfsError> {
        self.processor.send_scsi_command(lun, cdb, data).await
    }
}

impl<M: Mmio> fmt::Debug for ClientTransactionGuard<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTransactionGuard").finish_non_exhaustive()
    }
}

/// Guarantees exclusive access to [`RequestList`] and its waiter and transaction counts under
/// one `request_list` lock. Created by [`TransferRequestProcessor::requests`]. Dropping it
/// releases that mutex.
pub struct Requests<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    list: MutexGuard<'a, RequestList>,
}

impl<M: Mmio> Requests<'_, M> {
    /// Returns the driver's recorded power state.
    pub fn power(&self) -> DevicePowerState {
        self.list.power()
    }

    /// Returns whether any live transfer slot or multi-command client transaction is active.
    ///
    /// Excludes a timed-out admin slot so system suspend can still proceed to `PowerDown` and
    /// disable the host controller to reclaim it.
    pub fn has_inflight_work(&self) -> bool {
        let inactive = self.list.free_mask() | (self.list.timeout_mask() & (1u64 << ADMIN_SLOT));
        ((!inactive) & ALL_SLOTS_MASK) != 0
            || self.processor.active_client_transactions.load(Ordering::Acquire) != 0
    }

    /// Returns the number of synchronous submitters waiting for a free IO slot.
    pub fn blocked_reservers(&self) -> usize {
        self.processor.blocked_reservers.load(Ordering::Acquire)
    }

    /// Returns the number of client requests parked on client IO admission.
    pub fn blocked_on_admission(&self) -> usize {
        self.processor.blocked_on_admission.load(Ordering::Acquire)
    }

    /// Bitmask of slots currently in [`SlotState::Free`].
    pub fn free_mask(&self) -> u64 {
        self.list.free_mask()
    }

    /// Bitmask of slots currently in [`SlotState::Timeout`].
    pub fn timeout_mask(&self) -> u64 {
        self.list.timeout_mask()
    }

    /// Number of slots currently owned by the controller (`Scheduled | Timeout`).
    pub fn inflight_io(&self) -> u32 {
        self.list.controller_owned_mask().count_ones()
    }
}

struct PreparedTransfer<'p, 'a> {
    pmt: Option<zx::Pmt>,
    read_back: Option<ReadBack>,
    prdt: &'p [PrdtEntry],
    inline_data: Option<&'a [u8]>,
    direction: DataDirection,
}

/// Submits UTP transfer requests and processes their completions.
pub struct TransferRequestProcessor<M: Mmio> {
    /// BTI used to pin client data buffers.
    bti: zx::Bti,
    controller: Arc<UfsController<M>>,
    /// Transfer slots and their DMA descriptors.
    request_list: Mutex<RequestList>,
    /// System page size cached at creation time.
    page_size: u64,
    /// Serializes Query/NOP requests on the admin slot across `DeviceManager` and `UfsServer`, and
    /// excludes them from [`PowerGuard::stop_for_power_down`].
    admin_lock: AsyncMutex<()>,
    /// Signalled (with `request_list` held) when an I/O slot is released, client IO is admitted,
    /// or the processor stops, waking threads blocked in [`Self::reserve_io_slot_locked`].
    slot_available: Condvar,
    /// Sink for [`IoCompletion::Request`] outcomes.
    io_completer: OnceLock<Arc<dyn IoCompleter>>,
    io_stats: BlockIoStats,
    /// Cumulative `zx::Pmt` handles quarantined without `unpin()` after an `HCE = 0` timeout.
    quarantined_pmt_count: AtomicU64,
    /// Overdue data-slot diagnostic log count recorded in unit tests.
    #[cfg(test)]
    data_timeout_logs: AtomicU64,
    /// Data-slot diagnostic reporting interval and admin slot timeout in nanoseconds.
    command_timeout_ns: AtomicI64,
    /// Wakes [`Self::run_io_timeout_task`] when a request is scheduled with an earlier deadline or
    /// when the processor is stopped.
    timeout_waker: AtomicWaker,
    /// Sync submitters blocked in [`Self::reserve_io_slot_locked`] waiting for a free I/O slot.
    blocked_reservers: AtomicUsize,
    /// Client submitters parked waiting for the power state to admit client IO.
    blocked_on_admission: AtomicUsize,
    /// Active multi-command client transactions holding [`ClientTransactionGuard`].
    active_client_transactions: AtomicUsize,
}

impl<M: Mmio> fmt::Debug for TransferRequestProcessor<M> {
    /// Reports only the lock-free fields; `request_list` and the controller are not locked so
    /// `fmt` never blocks or deadlocks.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransferRequestProcessor")
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> TransferRequestProcessor<M> {
    /// Creates a processor with [`SDB_SLOTS`] UCD slots; [`ADMIN_SLOT`] is reserved as the admin
    /// slot.
    ///
    /// # Errors
    ///
    /// Returns the DMA allocation or handle duplication error.
    pub fn new(bti: &zx::Bti, controller: Arc<UfsController<M>>) -> Result<Self, zx::Status> {
        let request_list = RequestList::new(bti, SDB_SLOTS as u8)?;

        Ok(Self {
            bti: bti.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            controller,
            request_list: Mutex::new(request_list),
            page_size: zx::system_get_page_size() as u64,
            admin_lock: AsyncMutex::new(()),
            slot_available: Condvar::new(),
            io_completer: OnceLock::new(),
            io_stats: BlockIoStats::default(),
            quarantined_pmt_count: AtomicU64::new(0),
            #[cfg(test)]
            data_timeout_logs: AtomicU64::new(0),
            command_timeout_ns: AtomicI64::new(COMMAND_TIMEOUT.into_nanos()),
            timeout_waker: AtomicWaker::new(),
            blocked_reservers: AtomicUsize::new(0),
            blocked_on_admission: AtomicUsize::new(0),
            active_client_transactions: AtomicUsize::new(0),
        })
    }

    /// Sets the data-slot diagnostic reporting interval and admin slot timeout (`> 0`).
    pub fn set_timeout(&self, timeout: zx::MonotonicDuration) {
        assert!(timeout.into_nanos() > 0, "command timeout must be positive");
        // ORDERING: Standalone timeout duration configuration read when computing deadlines.
        self.command_timeout_ns.store(timeout.into_nanos(), Ordering::Relaxed);
        self.timeout_waker.wake();
    }

    /// Returns the configured data-slot diagnostic reporting interval and admin slot timeout.
    pub fn command_timeout(&self) -> zx::MonotonicDuration {
        // ORDERING: Standalone timeout duration configuration written by `set_timeout`.
        zx::MonotonicDuration::from_nanos(self.command_timeout_ns.load(Ordering::Relaxed))
    }

    /// Locks [`RequestList`] and returns a view of its slot and admission queries.
    pub fn requests(&self) -> Requests<'_, M> {
        Requests { processor: self, list: self.request_list.lock() }
    }

    /// Returns the shared controller.
    pub fn controller(&self) -> &Arc<UfsController<M>> {
        &self.controller
    }

    /// Returns the block I/O statistics recorded by this processor.
    pub fn io_stats(&self) -> &BlockIoStats {
        &self.io_stats
    }

    /// Number of overdue data-slot diagnostic log events emitted so far.
    #[cfg(test)]
    pub fn data_timeout_logs(&self) -> u64 {
        // ORDERING: Standalone monotonic test counter read after `handle_expired_slots`.
        self.data_timeout_logs.load(Ordering::Relaxed)
    }

    /// Returns the request list mutex for test inspection and fake hardware mapping.
    #[cfg(test)]
    pub fn request_list(&self) -> &Mutex<RequestList> {
        &self.request_list
    }

    /// Cumulative `zx::Pmt` handles quarantined without `unpin()` after an `HCE = 0` timeout.
    #[cfg(test)]
    pub fn quarantined_pmt_count(&self) -> u64 {
        // ORDERING: Standalone monotonic counter read by tests.
        self.quarantined_pmt_count.load(Ordering::Relaxed)
    }

    /// Rejects new slot reservations and wakes blocked/async reservers while leaving `stopped`
    /// clear so in-flight `Scheduled` slots can drain cleanly via completion interrupts.
    pub fn close_submissions(&self) {
        let mut list = self.request_list.lock();
        list.begin_closing();
        self.wake_all_slot_waiters(&mut list);
    }

    /// Returns the number of transfer slots (including [`ADMIN_SLOT`]) not `Free`.
    #[cfg(test)]
    pub fn busy_slot_count(&self) -> usize {
        let list = self.request_list.lock();
        ((!list.free_mask()) & ALL_SLOTS_MASK).count_ones() as usize
    }

    /// Waits up to `timeout` for all live transfer slots and multi-command client transactions to
    /// finish (ignoring a timed-out admin slot; see [`Requests::has_inflight_work`]).
    pub async fn drain_inflight(&self, timeout: zx::MonotonicDuration) -> Result<(), UfsError> {
        let res = uic::poll_until_with_interval(timeout, REGISTER_POLL_INTERVAL, || {
            let _ = self.process_slots(ALL_SLOTS_MASK);
            (!self.requests().has_inflight_work()).then_some(())
        })
        .await;
        if res.is_err() {
            let _ = self.process_slots(ALL_SLOTS_MASK);
        }
        res
    }

    /// Installs the sink for [`IoCompletion::Request`] outcomes. Must be called before the first
    /// such submission; later calls are ignored.
    pub fn set_io_completer(&self, completer: Arc<dyn IoCompleter>) {
        if self.io_completer.set(completer).is_err() {
            warn!("I/O completer already installed; ignoring replacement");
        }
    }

    /// Programs the UTRL base address and sets `UTRLRSR`.
    ///
    /// # Errors
    ///
    /// Returns `IoError` after [`PowerGuard::quiesce`].
    pub fn init(&self) -> Result<(), UfsError> {
        let list = self.request_list.lock();
        if list.is_stopped() {
            return Err(UfsError::IoError);
        }
        let paddr = list.io_buffer().phys_address();
        self.controller.with_mmio(|mmio| {
            let mut addr_low = UtrListBaseAddressReg::from_raw(0);
            addr_low.set_address(paddr as u32);
            addr_low.write(mmio);

            let mut addr_high = UtrListBaseAddressUpperReg::from_raw(0);
            addr_high.set_address_upper((paddr >> 32) as u32);
            addr_high.write(mmio);

            let mut run_stop = UtrListRunStopReg::from_raw(0);
            run_stop.set_value(true);
            run_stop.write(mmio);
        });
        Ok(())
    }

    fn sweep_controller_owned_slots(
        &self,
        mark_stopped: bool,
        disable_res: Result<(), UfsError>,
    ) -> Result<(), UfsError> {
        let mut deferred = DeferredCompletions::new(self);
        {
            let mut list = self.request_list.lock();
            let swept = list.controller_owned_mask();
            let mut remaining = swept;
            while remaining != 0 {
                let slot = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                self.sweep_slot_locked(&mut list, slot, &mut deferred);
            }
            if mark_stopped {
                if disable_res.is_ok() {
                    // SAFETY: `HCE` reads 0, so the controller performs no DMA.
                    unsafe {
                        list.unpin_all_descriptors(|status| {
                            error!(status:%; "Failed to unpin descriptor buffer");
                        });
                    }
                }
            } else if swept != 0 {
                self.wake_slot_waiters_for_mask(&mut list, swept);
            }
        }
        if disable_res.is_ok() {
            // SAFETY: `HCE` reads 0, so the controller performs no DMA.
            unsafe { deferred.complete_after_dma() };
        } else {
            deferred.quarantine();
        }
        disable_res
    }

    /// Wakes every blocked and async slot reserver so it re-examines the slot pool, client IO
    /// admission, or `stopped` / `closing`.
    fn wake_all_slot_waiters(&self, list: &mut RequestList) {
        if self.blocked_on_admission.load(Ordering::Acquire) > 0
            || self.blocked_reservers.load(Ordering::Acquire) > 0
        {
            self.slot_available.notify_all();
        }
        list.wake_all_slot_wakers();
    }

    fn wake_slot_waiters_for_mask(&self, list: &mut RequestList, released_mask: u64) {
        if (released_mask & IO_SLOT_MASK) != 0 && self.blocked_reservers.load(Ordering::Acquire) > 0
        {
            self.slot_available.notify_all();
        }
        if (released_mask & ALL_SLOTS_MASK) != 0 {
            list.wake_all_slot_wakers();
        }
    }

    /// Returns `slot` to `Free` without waking waiters and returns its read-back descriptor and
    /// pinned [`zx::Pmt`] so the caller can invalidate caches and unpin outside
    /// `request_list.lock()`.
    fn release_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
    ) -> (Option<ReadBack>, Option<zx::Pmt>) {
        let request_slot = list.slot_mut(slot).expect("slot index out of range");
        debug_assert_ne!(request_slot.state, SlotState::Free);
        let released = request_slot.release();
        list.set_slot_state(slot, SlotState::Free);
        released
    }

    fn release_reserved_slot_locked(&self, list: &mut RequestList, slot: u8) {
        let _ = self.release_slot_locked(list, slot);
        self.wake_slot_waiters_for_mask(list, 1u64 << slot);
    }

    fn reserved_slot(&self, slot: u8) -> ReservedSlot<'_, M> {
        ReservedSlot { processor: self, slot, armed: true }
    }

    /// Marks the lowest `Free` I/O slot `Reserved` and returns it, if any.
    fn take_free_io_slot_locked(&self, list: &mut RequestList) -> Option<u8> {
        let free_io = list.free_mask() & IO_SLOT_MASK;
        if free_io == 0 {
            return None;
        }
        let slot = free_io.trailing_zeros() as u8;
        list.reserve_slot(slot);
        Some(slot)
    }

    /// Reserves an I/O slot for a synchronous request while holding `request_list`, blocking on
    /// client IO admission and waiting on [`Self::slot_available`] while all are busy (pushback)
    /// until a slot is released by the completion path or the processor closes/stops.
    fn reserve_io_slot_locked(
        &self,
        list: &mut MutexGuard<'_, RequestList>,
    ) -> Result<ReservedSlot<'_, M>, UfsError> {
        loop {
            list.check_open()?;
            if !list.power().allows_client_io() {
                let _waiter = CountGuard::new(&self.blocked_on_admission);
                while !list.is_stopped() && !list.is_closing() && !list.power().allows_client_io() {
                    self.slot_available.wait(list);
                }
                continue;
            }
            if let Some(slot) = self.take_free_io_slot_locked(list) {
                return Ok(self.reserved_slot(slot));
            }
            let _waiter = CountGuard::new(&self.blocked_reservers);
            self.slot_available.wait(list);
        }
    }

    /// Reserves a data slot (`0..ADMIN_SLOT`) for an async request, waiting until one is free.
    ///
    /// Does not check client IO admission directly: client callers go through
    /// [`ClientTransactionGuard`]; bring-up calls [`Self::send_test_unit_ready`] before client IO
    /// starts.
    async fn reserve_io_slot(&self) -> Result<ReservedSlot<'_, M>, UfsError> {
        poll_fn(|cx| {
            let mut list = self.request_list.lock();
            if let Err(err) = list.check_open() {
                return Poll::Ready(Err(err));
            }
            if let Some(slot) = self.take_free_io_slot_locked(&mut list) {
                return Poll::Ready(Ok(self.reserved_slot(slot)));
            }
            list.register_slot_waker(cx.waker());
            Poll::Pending
        })
        .await
    }

    /// Waits for client IO admission and starts a multi-command client transaction.
    ///
    /// Holds [`Self::drain_inflight`] open until the returned guard drops so system suspend cannot
    /// split a multi-command sequence.
    pub async fn begin_client_transaction(
        &self,
    ) -> Result<ClientTransactionGuard<'_, M>, UfsError> {
        let mut waiting: Option<CountGuard<'_>> = None;
        poll_fn(|cx| {
            let mut list = self.request_list.lock();
            if let Err(err) = list.check_open() {
                return Poll::Ready(Err(err));
            }
            if list.power().allows_client_io() {
                let _count = CountGuard::new(&self.active_client_transactions);
                return Poll::Ready(Ok(ClientTransactionGuard { processor: self, _count }));
            }
            if waiting.is_none() {
                waiting = Some(CountGuard::new(&self.blocked_on_admission));
            }
            list.register_slot_waker(cx.waker());
            Poll::Pending
        })
        .await
    }

    /// Serializes on `admin_lock` and waits for the admin slot to become free.
    ///
    /// Waiting on `slot_wakers` after taking `admin_lock` lets any earlier cancelled admin future
    /// finish on hardware first. Fails with `IoError` if closed or stopped, or `Timeout` if the
    /// admin slot is quarantined.
    async fn reserve_admin_slot(&self) -> Result<AdminSlotGuard<'_, M>, UfsError> {
        let slot = ADMIN_SLOT;
        let lock = self.admin_lock.lock().await;
        if (self.request_list.lock().timeout_mask() & (1u64 << slot)) != 0 {
            let _ = self.process_slots(1u64 << slot);
        }
        let guard = poll_fn(|cx| {
            let mut list = self.request_list.lock();
            list.check_open()?;
            match list.slot(slot)?.state {
                SlotState::Free => {
                    list.reserve_slot(slot);
                    return Poll::Ready(Ok(self.reserved_slot(slot)));
                }
                SlotState::Timeout => {
                    return Poll::Ready(Err(UfsError::Timeout));
                }
                SlotState::Reserved | SlotState::Scheduled => {}
            }
            list.register_slot_waker(cx.waker());
            Poll::Pending
        })
        .await?;
        Ok(AdminSlotGuard { slot: guard, _lock: lock })
    }

    /// Validates `transfer` before reserving a slot. Returns `Ok(Some((offset, len)))` for VMO
    /// transfers needing `bti.pin` and `Ok(None)` for inline UCD parameter transfers.
    fn validate_transfer(
        &self,
        transfer: &DataTransfer<'_>,
    ) -> Result<Option<(u64, u64)>, UfsError> {
        match &transfer.buffer {
            TransferBuffer::Vmo(_) => {
                if (transfer.offset & DWORD_ALIGN_MASK) != 0
                    || (u64::from(transfer.length) & DWORD_ALIGN_MASK) != 0
                {
                    return Err(UfsError::InvalidParameter);
                }
                let (offset, length) = transfer.pin_range(self.page_size)?;
                if (length / self.page_size) as usize > MAX_PRDT_ENTRIES {
                    return Err(UfsError::InvalidParameter);
                }
                Ok(Some((offset, length)))
            }
            TransferBuffer::Inline(bytes) => {
                if bytes.is_empty()
                    || bytes.len() > INLINE_DATA_SIZE
                    || !bytes.len().is_multiple_of(4)
                {
                    return Err(UfsError::InvalidParameter);
                }
                Ok(None)
            }
        }
    }

    /// Pins `transfer` outside `request_list.lock()`, cleans/invalidates the CPU cache before DMA,
    /// and invokes `f` with the prepared PRDT and PMT (unpinning if `f` fails before scheduling).
    fn with_prepared_transfer<'a, R>(
        &self,
        transfer: Option<DataTransfer<'a>>,
        f: impl FnOnce(&mut PreparedTransfer<'_, 'a>) -> Result<R, UfsError>,
    ) -> Result<R, UfsError> {
        let page_size = self.page_size;
        let pin_range = transfer.as_ref().map(|t| self.validate_transfer(t)).transpose()?.flatten();
        let inline_data = match &transfer {
            Some(DataTransfer { buffer: TransferBuffer::Inline(bytes), .. }) => Some(*bytes),
            _ => None,
        };
        let page_count = pin_range.map_or(0, |(_, length)| (length / page_size) as usize);

        // Only zero-initialize scratch PRDT arrays for the transfer size class actually used.
        let mut small_prdt: [PrdtEntry; SMALL_TRANSFER_PAGES];
        let mut large_prdt: [PrdtEntry; MAX_PRDT_ENTRIES];
        let prdt_table: &mut [PrdtEntry] = match page_count {
            0 => &mut [],
            1..=SMALL_TRANSFER_PAGES => {
                small_prdt = [PrdtEntry::default(); SMALL_TRANSFER_PAGES];
                &mut small_prdt[..page_count]
            }
            _ => {
                large_prdt = [PrdtEntry::default(); MAX_PRDT_ENTRIES];
                &mut large_prdt[..page_count]
            }
        };

        let mut pmt = None;
        let mut read_back = None;
        let mut prdt_count = 0;
        let mut direction = DataDirection::NoData;
        if let (Some(data), Some((offset, length))) = (transfer, pin_range) {
            let TransferBuffer::Vmo(vmo) = data.buffer else {
                unreachable!("validate_transfer returns Some only for Vmo transfers");
            };
            let mut small_phys: [usize; SMALL_TRANSFER_PAGES];
            let mut large_phys: [usize; MAX_PRDT_ENTRIES];
            let phys: &mut [usize] = match page_count {
                0 => &mut [],
                1..=SMALL_TRANSFER_PAGES => {
                    small_phys = [0usize; SMALL_TRANSFER_PAGES];
                    &mut small_phys[..page_count]
                }
                _ => {
                    large_phys = [0usize; MAX_PRDT_ENTRIES];
                    &mut large_phys[..page_count]
                }
            };
            let pinned = self
                .bti
                .pin(data.direction.pin_options(), &vmo, offset, length, phys)
                .map_err(|s| {
                    error!(status:% = s; "Failed to pin transfer buffer");
                    UfsError::IoError
                })?;
            match transfer::fill_prdt(prdt_table, phys, data.offset, data.length) {
                Ok(count) => prdt_count = count,
                Err(e) => {
                    // SAFETY: The pinned addresses were never written into a descriptor.
                    unsafe { unpin_pmt(Some(pinned)) };
                    return Err(e);
                }
            }
            let cache_op = match data.direction {
                TransferDirection::Read => zx::VmoOp::CACHE_CLEAN_INVALIDATE,
                TransferDirection::Write => zx::VmoOp::CACHE_CLEAN,
            };
            if let Err(s) = vmo.op_range(cache_op, data.offset, u64::from(data.length)) {
                error!(status:% = s; "Failed to clean/invalidate transfer buffer cache");
                // SAFETY: The pinned addresses were never written into a descriptor.
                unsafe { unpin_pmt(Some(pinned)) };
                return Err(UfsError::IoError);
            }
            direction = data.direction.to_data_direction();
            if data.direction == TransferDirection::Read && data.length > 0 {
                read_back = Some(ReadBack { vmo, offset: data.offset, length: data.length });
            }
            pmt = Some(pinned);
        } else if inline_data.is_some() {
            direction = DataDirection::HostToDevice;
        }

        let mut prepared = PreparedTransfer {
            pmt,
            read_back,
            prdt: &prdt_table[..prdt_count],
            inline_data,
            direction,
        };
        let result = f(&mut prepared);
        // SAFETY: Any PMT still in `prepared.pmt` was not stored in a scheduled slot.
        unsafe { unpin_pmt(prepared.pmt.take()) };
        result
    }

    /// Writes the UCD and UTRD for `Reserved` `slot` and rings its doorbell while holding
    /// `request_list`. Moves `prepared.pmt`, `prepared.read_back`, and `completion` into the slot
    /// only after the descriptors are written and the doorbell is rung, leaving `completion` in
    /// place on error for [`Self::schedule_slot`] to deliver.
    fn schedule_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        scsi_opcode: u8,
        layout: CommandDescriptorLayout,
        request: &[u8],
        prepared: &mut PreparedTransfer<'_, '_>,
        completion: &mut Option<SlotCompletion>,
    ) -> Result<(), UfsError> {
        if request.len() != layout.request_length() {
            return Err(UfsError::InvalidParameter);
        }
        list.check_open()?;
        let (io_buffer, request_slots) = list.io_buffer_and_slots_mut();
        let request_slot = match request_slots.get_mut(slot as usize) {
            Some(s) if s.state == SlotState::Reserved => s,
            Some(_) | None => return Err(UfsError::InvalidSlot(slot as u32)),
        };

        let io_error = |status: zx::Status| {
            error!(slot, status:%; "Failed to write transfer descriptors");
            UfsError::IoError
        };
        let ucd = &mut request_slot.command_descriptor_io;
        ucd.write(0, request).map_err(io_error)?;
        // Zero the whole response UPIU so a failed request cannot return a stale response. The
        // controller does not write the data segment (sense data) when the device sends none.
        // Every layout's response length is a multiple of 8, so `zero` uses 64-bit stores.
        ucd.zero(layout.response_offset(), layout.response_length()).map_err(io_error)?;
        let ucd_address = ucd.phys_address();
        let prdt_len = if let Some(inline_bytes) = prepared.inline_data {
            ucd.write(INLINE_DATA_OFFSET, inline_bytes).map_err(io_error)?;
            let mut inline_prdt = PrdtEntry::default();
            inline_prdt.set_dma_address(
                ucd_address + INLINE_DATA_OFFSET as u64,
                inline_bytes.len() as u32,
            )?;
            ucd.write_struct(layout.prdt_offset(), &inline_prdt).map_err(io_error)?;
            1u16
        } else {
            if !prepared.prdt.is_empty() {
                ucd.write(layout.prdt_offset(), prepared.prdt.as_bytes()).map_err(io_error)?;
            }
            prepared.prdt.len() as u16
        };

        let mut descriptor = TransferRequestDescriptor::default();
        descriptor.set_command_type(COMMAND_TYPE_UFS_STORAGE);
        descriptor.set_data_direction(prepared.direction);
        descriptor.set_interrupt(true);
        descriptor.set_overall_command_status(OverallCommandStatus::Invalid);
        descriptor.command_descriptor_base_addr = ucd_address as u32;
        descriptor.command_descriptor_base_addr_upper = (ucd_address >> 32) as u32;
        descriptor.set_response_upiu_offset_and_length(
            dwords(layout.response_offset()),
            dwords(layout.response_length()),
        );
        descriptor.set_prdt_offset_and_length(
            if prdt_len == 0 { 0 } else { dwords(layout.prdt_offset()) },
            prdt_len,
        );

        io_buffer
            .write_struct(slot as usize * size_of::<TransferRequestDescriptor>(), &descriptor)
            .map_err(io_error)?;
        self.controller.with_mmio(|mmio| {
            // Order the descriptor stores before the doorbell store.
            mmio.write_barrier();
            let mut doorbell = UtrListDoorBellReg::from_raw(0);
            doorbell.set_door_bell(1 << slot);
            doorbell.write(mmio);
        });

        let now = zx::MonotonicInstant::get();
        let deadline = now + self.command_timeout();
        request_slot.pmt = prepared.pmt.take();
        request_slot.read_back = prepared.read_back.take();
        request_slot.completion = completion.take();
        request_slot.scsi_opcode = scsi_opcode;
        request_slot.deadline = Some(deadline);
        list.set_slot_state(slot, SlotState::Scheduled);
        self.io_stats.on_request_submitted(now);
        if list.armed_deadline().is_none_or(|armed| deadline < armed) {
            list.set_armed_deadline(Some(deadline));
            self.timeout_waker.wake();
        }
        Ok(())
    }

    /// Pins `data` (if any), writes the UCD and UTRD for `reserved`, and rings its doorbell.
    ///
    /// Always consumes `reserved` and delivers `completion`: either from the completion path once
    /// the controller finishes (disarming `reserved`), or inline with [`ScsiOutcome::Fail`] after
    /// releasing `reserved` if preparation or scheduling fails.
    fn schedule_slot(
        &self,
        mut reserved: ReservedSlot<'_, M>,
        scsi_opcode: u8,
        layout: CommandDescriptorLayout,
        request: &[u8],
        data: Option<DataTransfer<'_>>,
        completion: SlotCompletion,
    ) {
        let slot = reserved.slot();
        let mut completion = Some(completion);
        let result = self.with_prepared_transfer(data, |prepared| {
            let mut list = self.request_list.lock();
            let scheduled = self.schedule_slot_locked(
                &mut list,
                slot,
                scsi_opcode,
                layout,
                request,
                prepared,
                &mut completion,
            );
            if scheduled.is_ok() {
                reserved.disarm();
            }
            scheduled
        });
        drop(reserved);
        if let Err(e) = result
            && let Some(completion) = completion
        {
            warn!(lun = completion.lun, err:% = e; "Failed to schedule UTP request");
            self.complete_io(completion, ScsiOutcome::Fail(e));
        }
    }

    /// Pins `data` (if any), schedules `request` on `reserved`, and awaits its [`ScsiOutcome`].
    async fn submit(
        &self,
        reserved: ReservedSlot<'_, M>,
        lun: u8,
        scsi_opcode: u8,
        layout: CommandDescriptorLayout,
        request: &[u8],
        data: Option<DataTransfer<'_>>,
    ) -> ScsiOutcome {
        let (tx, rx) = oneshot::channel();
        let completion = SlotCompletion { lun, completion: IoCompletion::Oneshot(tx), io_op: None };
        self.schedule_slot(reserved, scsi_opcode, layout, request, data, completion);
        rx.await.unwrap_or(ScsiOutcome::Fail(UfsError::IoError))
    }

    /// Builds a `CommandUpiu` for `lun` on `slot` with `cdb` and `data`'s transfer length and
    /// direction.
    fn build_command_upiu(
        &self,
        lun: u8,
        slot: u8,
        cdb: [u8; 16],
        data: Option<&DataTransfer<'_>>,
    ) -> CommandUpiu {
        let expected_transfer_len = data.map_or(0, |d| d.length);
        let direction = data.map_or(DataDirection::NoData, |d| d.direction.to_data_direction());
        let mut command = CommandUpiu::new(cdb, expected_transfer_len, direction);
        command.header.lun = lun;
        command.header.task_tag = slot;
        command
    }

    /// Submits a SCSI command to `lun` synchronously and returns as soon as its doorbell has been
    /// rung. Blocks the calling thread while client IO is suspended or all I/O slots are busy
    /// (pushback). The outcome is always delivered through `completion` (either inline on
    /// validation/reservation/pin failure, or from the completion path once the controller finishes
    /// or is quiesced); transient SCSI statuses are reported, not retried.
    pub fn submit_io(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
        completion: IoCompletion,
    ) {
        let data = data.filter(|d| !matches!(d.buffer, TransferBuffer::Vmo(_)) || d.length != 0);
        let expected_transfer_len = data.as_ref().map_or(0, |data| data.length);
        let io_op = match cdb[0] {
            x if x == ScsiOpcode::Read10 as u8 || x == ScsiOpcode::Read16 as u8 => {
                Some(BlockIoOp::Read(u64::from(expected_transfer_len)))
            }
            x if x == ScsiOpcode::Write10 as u8 || x == ScsiOpcode::Write16 as u8 => {
                Some(BlockIoOp::Write(u64::from(expected_transfer_len)))
            }
            x if x == ScsiOpcode::SynchronizeCache10 as u8 => Some(BlockIoOp::Flush),
            x if x == ScsiOpcode::Unmap as u8 => {
                Some(BlockIoOp::Trim(data.as_ref().map_or(0, |d| d.logical_bytes)))
            }
            _ => None,
        };
        let completion = SlotCompletion { lun, completion, io_op };
        if let Some(ref transfer) = data
            && let Err(e) = self.validate_transfer(transfer)
        {
            warn!(lun, err:% = e; "Failed to validate block request");
            self.complete_io(completion, ScsiOutcome::Fail(e));
            return;
        }

        // Reserve (and possibly block for) the slot before pinning, so pushback never holds
        // client pages pinned while waiting.
        let reserved = {
            let mut list = self.request_list.lock();
            match self.reserve_io_slot_locked(&mut list) {
                Ok(reserved) => reserved,
                Err(e) => {
                    drop(list);
                    warn!(lun, err:% = e; "Failed to reserve slot for block request");
                    self.complete_io(completion, ScsiOutcome::Fail(e));
                    return;
                }
            }
        };
        let command = self.build_command_upiu(lun, reserved.slot(), cdb, data.as_ref());

        self.schedule_slot(
            reserved,
            cdb[0],
            CommandDescriptorLayout::SCSI_COMMAND,
            command.as_bytes(),
            data,
            completion,
        );
    }

    /// Delivers an error `status` for a request for `lun` that was rejected before submission.
    pub fn reject_io(&self, lun: u8, completion: IoCompletion, status: zx::Status) {
        debug_assert_ne!(status.into_raw(), zx::sys::ZX_OK, "reject_io requires an error status");
        self.complete_io(
            SlotCompletion { lun, completion, io_op: None },
            ScsiOutcome::Rejected(status),
        );
    }

    /// Delivers `outcome` to `completion`, recording its block I/O stats on success.
    fn complete_io(&self, completion: SlotCompletion, outcome: ScsiOutcome) {
        let SlotCompletion { lun, completion, io_op } = completion;
        if matches!(outcome, ScsiOutcome::Good) {
            self.io_stats.record(io_op);
        }
        match completion {
            IoCompletion::Request(request_id) => match self.io_completer.get() {
                Some(completer) => completer.complete(lun, request_id, outcome.io_status()),
                None => error!(lun, request_id:?; "No I/O completer installed; dropping request"),
            },
            IoCompletion::Oneshot(sender) => {
                let _ = sender.send(outcome);
            }
        }
    }

    /// Captures a snapshot of controller status and read-to-clear UIC error registers.
    fn capture_timeout_registers(&self) -> TimeoutRegisterSnapshot {
        self.controller.with_mmio(|mmio| TimeoutRegisterSnapshot {
            hcs: HostControllerStatusReg::read(mmio).0,
            is: InterruptStatusReg::read(mmio).0,
            utrldbr: UtrListDoorBellReg::read(mmio).0,
            uecpa: HostUicErrorCodePhyAdapterLayerReg::read(mmio).0,
            uecdl: HostUicErrorCodeDataLinkLayerReg::read(mmio).0,
            uecn: HostUicErrorCodeNetworkLayerReg::read(mmio).0,
            uect: HostUicErrorCodeTransportLayerReg::read(mmio).0,
            uecdme: HostUicErrorCodeDmeReg::read(mmio).0,
        })
    }

    /// Logs controller and UIC error registers and re-arms the deadline for overdue data slots
    /// (`0..30`), and fails an overdue admin slot with [`UfsError::Timeout`] while keeping its DMA
    /// buffer quarantined in [`SlotState::Timeout`]; see the module docs for why. Overdue slots
    /// whose doorbell has already cleared without an interrupt are logged once and counted in
    /// [`UfsController::missed_interrupt_count`].
    pub async fn handle_expired_slots(&self) {
        // Executor time matches `run_io_timeout_task`'s timers (and the fake clock in tests).
        let now = fasync::MonotonicInstant::now().into_zx();
        let command_timeout = self.command_timeout();
        let mut deferred = DeferredCompletions::new(self);

        {
            let mut list = self.request_list.lock();
            if list.is_stopped() || list.next_deadline().is_none_or(|deadline| now < deadline) {
                return;
            }
            let mut expired_before = 0u64;
            let mut scan = list.scheduled_mask();
            while scan != 0 {
                let slot = scan.trailing_zeros() as u8;
                scan &= scan - 1;
                if list
                    .slot(slot)
                    .expect("scheduled_mask slot out of range")
                    .deadline
                    .is_some_and(|deadline| now >= deadline)
                {
                    expired_before |= 1u64 << slot;
                }
            }
            let _ = self.process_slots_locked(&mut list, ALL_SLOTS_MASK, &mut deferred);
            let missed_interrupt_mask = expired_before & !list.scheduled_mask();
            if missed_interrupt_mask != 0 {
                let count = u64::from(missed_interrupt_mask.count_ones());
                self.controller.record_missed_interrupts(count);
                let (is, ie, utrldbr, hcs) = self.controller.with_mmio(|mmio| {
                    // Read only: `IS` is not written and the read-to-clear UIC error registers
                    // are not read. `IS & IE != 0` shows the controller is asserting an
                    // interrupt, so the loss is below this driver.
                    (
                        InterruptStatusReg::read(mmio).0,
                        InterruptEnableReg::read(mmio).0,
                        UtrListDoorBellReg::read(mmio).0,
                        HostControllerStatusReg::read(mmio).0,
                    )
                });
                warn!(
                    slot_mask = missed_interrupt_mask,
                    count,
                    is,
                    ie,
                    utrldbr,
                    hcs;
                    "UTP slots completed by the timeout poll; completion interrupt did not arrive"
                );
            }
            let mut snapshot: Option<TimeoutRegisterSnapshot> = None;
            let mut remaining = list.scheduled_mask();
            while remaining != 0 {
                let slot = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                let request_slot = list.slot_mut(slot).expect("scheduled_mask slot out of range");
                debug_assert_eq!(request_slot.state, SlotState::Scheduled);
                if request_slot.deadline.is_none_or(|deadline| now < deadline) {
                    continue;
                }
                let regs = *snapshot.get_or_insert_with(|| self.capture_timeout_registers());
                let scsi_opcode = request_slot.scsi_opcode;
                if slot == ADMIN_SLOT {
                    let completion = request_slot.completion.take();
                    request_slot.read_back = None;
                    request_slot.deadline = None;
                    regs.log_error(
                        slot,
                        scsi_opcode,
                        command_timeout.into_millis(),
                        "UTP admin slot timed out with UTRLDBR still set; retaining DMA pinned",
                    );
                    list.set_slot_state(slot, SlotState::Timeout);
                    self.wake_slot_waiters_for_mask(&mut list, 1u64 << slot);
                    deferred.push(None, None, completion, ScsiOutcome::Fail(UfsError::Timeout));
                    continue;
                }
                regs.log_error(
                    slot,
                    scsi_opcode,
                    command_timeout.into_millis(),
                    "UTP transfer slot still scheduled past timeout; leaving DMA pinned",
                );
                #[cfg(test)]
                {
                    // ORDERING: Standalone monotonic test counter incremented under `request_list`.
                    self.data_timeout_logs.fetch_add(1, Ordering::Relaxed);
                }
                request_slot.deadline = Some(now + command_timeout);
            }
        }
        // SAFETY: Completed entries had their doorbell bit observed clear, and the timed-out admin
        // entry carries no `pmt` or `read_back` (`None, None`), so nothing is unpinned or
        // cache-invalidated while the controller still owns the admin slot.
        unsafe { deferred.complete_after_dma() };
    }

    /// Sweeps a controller-owned slot (`Scheduled` or `Timeout`) back to `Free` after
    /// `disable_host_controller()`, discarding `read_back` so potentially incomplete DMA buffers
    /// are never cache-invalidated.
    fn sweep_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        deferred: &mut DeferredCompletions<'_, M>,
    ) {
        let request_slot = list.slot_mut(slot).expect("slot index out of range");
        debug_assert!(matches!(request_slot.state, SlotState::Scheduled | SlotState::Timeout));
        let completion = request_slot.completion.take();
        let (_read_back, pmt) = self.release_slot_locked(list, slot);
        if completion.is_some() || pmt.is_some() {
            deferred.push(None, pmt, completion, ScsiOutcome::Fail(UfsError::IoError));
        }
    }

    /// Timeout watchdog for in-flight `Scheduled` slots; exits promptly on `quiesce()`.
    pub async fn run_io_timeout_task(&self) {
        loop {
            let Some(wake_at) = poll_fn(|cx| {
                self.timeout_waker.register(cx.waker());
                let mut list = self.request_list.lock();
                if list.is_stopped() {
                    list.set_armed_deadline(None);
                    return Poll::Ready(None);
                }
                match list.next_deadline() {
                    Some(wake_at) => {
                        list.set_armed_deadline(Some(wake_at));
                        Poll::Ready(Some(wake_at))
                    }
                    None => {
                        list.set_armed_deadline(None);
                        Poll::Pending
                    }
                }
            })
            .await
            else {
                return;
            };
            let mut timer = pin!(fasync::Timer::new(wake_at).fuse());
            let mut interrupted = pin!(
                poll_fn(|cx| {
                    self.timeout_waker.register(cx.waker());
                    let list = self.request_list.lock();
                    if list.is_stopped() {
                        return Poll::Ready(true);
                    }
                    match list.next_deadline() {
                        Some(earlier) if earlier < wake_at => Poll::Ready(false),
                        Some(_) => Poll::Pending,
                        None if list.armed_deadline().is_none() => Poll::Ready(false),
                        None => Poll::Pending,
                    }
                })
                .fuse()
            );
            select_biased! {
                stopped = interrupted => if stopped { return } else { continue },
                () = timer => {}
            }
            self.handle_expired_slots().await;
        }
    }

    /// Submits an administrative UPIU on `admin`'s slot, awaits completion while holding
    /// `AdminSlotGuard`, and reads the validated response UPIU directly from the admin UCD.
    async fn execute_admin<R: FromBytes + IntoBytes + Immutable>(
        &self,
        admin: AdminSlotGuard<'_, M>,
        layout: CommandDescriptorLayout,
        request: &[u8],
        expected_response: UpiuTransactionCode,
    ) -> Result<R, UfsError> {
        debug_assert_eq!(size_of::<R>(), layout.response_length());
        let AdminSlotGuard { slot: reserved, _lock: admin_lock } = admin;
        let slot = reserved.slot();
        match self.submit(reserved, 0, 0, layout, request, None).await {
            ScsiOutcome::Good => {}
            ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => return Err(error),
            ScsiOutcome::Rejected(_) => return Err(UfsError::IoError),
        }
        let response: R = {
            let list = self.request_list.lock();
            if list.is_stopped() {
                return Err(UfsError::IoError);
            }
            list.slot(slot)?
                .command_descriptor_io
                .read_struct(layout.response_offset())
                .map_err(|_| UfsError::IoError)?
        };
        drop(admin_lock);
        check_response_header(response.as_bytes(), expected_response, slot)?;
        Ok(response)
    }

    /// Reads the transfer doorbell register (`UTRLDBR`).
    fn read_doorbell(&self) -> u32 {
        self.controller.with_mmio(|mmio| UtrListDoorBellReg::read(mmio).door_bell())
    }

    /// Reads a slot's `OCS`, re-reading while it is still `Invalid` in case the controller's
    /// descriptor write is not yet visible.
    fn read_ocs(&self, list: &RequestList, slot: u8) -> u8 {
        let offset = slot as usize * size_of::<TransferRequestDescriptor>() + OCS_OFFSET;
        let read = || match list.io_buffer().read_struct::<u32>(offset) {
            Ok(dword2) => dword2 as u8,
            Err(_) => OverallCommandStatus::Invalid as u8,
        };
        let mut ocs = read();
        for _ in 1..OCS_RETRIES {
            if ocs != OverallCommandStatus::Invalid as u8 {
                break;
            }
            spin_loop();
            ocs = read();
        }
        ocs
    }

    /// Decodes the RESPONSE UPIU of a finished SCSI command in `slot`'s UCD.
    fn decode_io_response(list: &RequestList, slot: u8, lun: u8, scsi_opcode: u8) -> ScsiOutcome {
        let Ok(request_slot) = list.slot(slot) else {
            return ScsiOutcome::Fail(UfsError::InvalidSlot(u32::from(slot)));
        };
        let Ok((response, header)) = read_scsi_response(
            &request_slot.command_descriptor_io,
            CommandDescriptorLayout::SCSI_COMMAND.response_offset(),
        ) else {
            return ScsiOutcome::Fail(UfsError::IoError);
        };
        if let Err(err) =
            check_response_header(header.as_bytes(), UpiuTransactionCode::Response, slot)
        {
            return ScsiOutcome::Fail(err);
        }
        check_scsi_status(lun, scsi_opcode, &response)
    }

    /// Decodes the slot's [`ScsiOutcome`], releases `slot` to `Free`, and pushes read-back cache
    /// invalidation, `zx::Pmt` unpinning, and outcome delivery to `deferred` for execution outside
    /// `request_list.lock()`.
    fn finish_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        result: Result<(), UfsError>,
        deferred: &mut DeferredCompletions<'_, M>,
    ) {
        let request_slot = list.slot_mut(slot).expect("slot index out of range");
        let scsi_opcode = request_slot.scsi_opcode;
        let completion =
            request_slot.completion.take().expect("Scheduled slot must have a completion");
        let outcome = match result {
            Ok(()) if slot == ADMIN_SLOT => ScsiOutcome::Good,
            Ok(()) => Self::decode_io_response(list, slot, completion.lun, scsi_opcode),
            Err(err) => ScsiOutcome::Fail(err),
        };
        let (read_back, pmt) = self.release_slot_locked(list, slot);
        let read_back = read_back.filter(|_| result.is_ok());
        deferred.push(read_back, pmt, Some(completion), outcome);
    }

    /// Completes `slot` if `Scheduled` with a clear `doorbell` bit, or reclaims its slot and
    /// retained `zx::Pmt` if `Timeout` and `doorbell` later cleared.
    fn process_sdb_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        doorbell: u32,
        deferred: &mut DeferredCompletions<'_, M>,
    ) -> bool {
        debug_assert!((slot as usize) < SDB_SLOTS);
        let request_slot = list.slot(slot).expect("slot index out of range");
        let outstanding = doorbell & (1 << slot) != 0;
        match (request_slot.state, outstanding) {
            (SlotState::Scheduled, false) => {
                let ocs = self.read_ocs(list, slot);
                let result = match OverallCommandStatus::from_raw(ocs) {
                    Some(OverallCommandStatus::Success) => Ok(()),
                    Some(OverallCommandStatus::Invalid) => {
                        error!(slot; "Slot doorbell cleared with OCS still Invalid (0x0F)");
                        Err(UfsError::IoError)
                    }
                    status => {
                        error!(slot, ocs, status:?; "Slot failed with OCS error");
                        Err(UfsError::IoError)
                    }
                };
                self.finish_slot_locked(list, slot, result, deferred);
                true
            }
            (SlotState::Timeout, false) => {
                info!(slot; "Timed-out UTP transfer slot doorbell cleared; reclaiming slot");
                let (_read_back, pmt) = self.release_slot_locked(list, slot);
                if pmt.is_some() {
                    deferred.push(None, pmt, None, ScsiOutcome::Fail(UfsError::Timeout));
                }
                false
            }
            (SlotState::Scheduled | SlotState::Timeout, true)
            | (SlotState::Free | SlotState::Reserved, _) => false,
        }
    }

    /// Processes completions of `slot_mask`, then invalidates read caches, unpins PMTs, and
    /// delivers outcomes after releasing `request_list`.
    fn process_slots(&self, slot_mask: u64) -> u32 {
        let mut deferred = DeferredCompletions::new(self);
        let completed = {
            let mut list = self.request_list.lock();
            self.process_slots_locked(&mut list, slot_mask, &mut deferred)
        };
        // SAFETY: Every slot pushed into `deferred` had its doorbell bit observed clear (`0`).
        unsafe { deferred.complete_after_dma() };
        completed
    }

    /// Processes completions of `slot_mask` while holding `request_list`.
    fn process_slots_locked(
        &self,
        list: &mut RequestList,
        slot_mask: u64,
        deferred: &mut DeferredCompletions<'_, M>,
    ) -> u32 {
        if list.is_stopped() {
            return 0;
        }
        let active = (list.controller_owned_mask() & slot_mask) as u32;
        if active == 0 {
            return 0;
        }
        let doorbell = self.read_doorbell();
        let free_before = list.free_mask();
        let mut completed = 0;
        let mut remaining = active;
        while remaining != 0 {
            let slot = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1;
            if self.process_sdb_slot_locked(list, slot, doorbell, deferred) {
                completed += 1;
            }
        }
        let released_mask = (list.free_mask() & !free_before) & ALL_SLOTS_MASK;
        if released_mask != 0 {
            self.wake_slot_waiters_for_mask(list, released_mask);
        }
        completed
    }

    /// Processes a possible completion of `slot`. Returns whether it completed.
    #[cfg(test)]
    pub fn process_slot_completion(&self, slot: u8) -> bool {
        (slot as usize) < SDB_SLOTS && self.process_slots(1u64 << slot) > 0
    }

    /// Services an interrupt: clears and logs the handled `IS` bits, and processes SDB
    /// completions. Status is cleared before the state it reports is read, so concurrent calls
    /// lose no event. Returns `IS` as read.
    pub fn service_interrupt(&self) -> InterruptStatusReg {
        let status = self.controller.fetch_and_clear_interrupts();
        if status.utp_transfer_request_completion_status() {
            self.process_slots(ALL_SLOTS_MASK);
        }
        status
    }

    /// Sends a QUERY REQUEST UPIU on the admin slot and returns the QUERY RESPONSE UPIU.
    ///
    /// Bypasses client IO admission, for the driver's own queries at bring-up and in power
    /// transitions. A transition runs with client IO paused, so it cannot wait for a
    /// [`ClientTransactionGuard`]. Client queries go through
    /// [`ClientTransactionGuard::send_query_upiu`]. The response's `Query Response` code is not
    /// checked; callers interpret it.
    pub async fn send_query_upiu(&self, query_request: &QueryUpiu) -> Result<QueryUpiu, UfsError> {
        let admin = self.reserve_admin_slot().await?;
        let mut request = *query_request;
        request.header.task_tag = admin.slot.slot();
        self.execute_admin(
            admin,
            CommandDescriptorLayout::QUERY,
            request.as_bytes(),
            UpiuTransactionCode::QueryResponse,
        )
        .await
    }

    /// Sends a NOP OUT UPIU on the admin slot and checks the NOP IN response.
    ///
    /// Bypasses client IO admission as this path is used during link start-up and resume paths.
    pub async fn send_nop_out(&self) -> Result<(), UfsError> {
        let admin = self.reserve_admin_slot().await?;
        let mut nop = NopOutUpiu::new();
        nop.header.task_tag = admin.slot.slot();
        let response: NopInUpiu = self
            .execute_admin(
                admin,
                CommandDescriptorLayout::NOP,
                nop.as_bytes(),
                UpiuTransactionCode::NopIn,
            )
            .await?;
        if response.header.response != 0 {
            error!(response = response.header.response; "NOP IN reported non-zero response");
            return Err(UfsError::IoError);
        }
        Ok(())
    }

    /// Sends a SCSI command on the admin slot, bypassing client IO admission.
    ///
    /// Power transitions use the admin slot rather than data slots `0..30` so they never compete
    /// with client IO, and so a hung device command times out after `COMMAND_TIMEOUT`. In this case
    /// the admin slot is quarantined until the slot completes on its own, or the host controller is
    /// disabled. Retries up to [`SCSI_RETRIES`] times on transient SCSI statuses (e.g.
    /// `UNIT ATTENTION`, `NOT READY`, `BUSY`, `TASK SET FULL`).
    ///
    /// Unlike the IO path, this will return early on a timeout so we don't wedge the power worker.
    pub async fn send_admin_scsi_command(&self, lun: u8, cdb: [u8; 16]) -> Result<(), UfsError> {
        let mut retries = 0;
        loop {
            let AdminSlotGuard { slot: reserved, _lock: admin_lock } =
                self.reserve_admin_slot().await?;
            let slot = reserved.slot();
            let command = self.build_command_upiu(lun, slot, cdb, None);
            let submit_outcome = self
                .submit(
                    reserved,
                    lun,
                    cdb[0],
                    CommandDescriptorLayout::SCSI_COMMAND,
                    command.as_bytes(),
                    None,
                )
                .await;
            let outcome = match submit_outcome {
                ScsiOutcome::Good => {
                    let list = self.request_list.lock();
                    if list.is_stopped() {
                        return Err(UfsError::IoError);
                    }
                    Self::decode_io_response(&list, slot, lun, cdb[0])
                }
                other => other,
            };
            drop(admin_lock);
            match outcome {
                ScsiOutcome::Good => return Ok(()),
                ScsiOutcome::Retry { error: _, delay } if retries < SCSI_RETRIES => {
                    retries += 1;
                    if delay {
                        fasync::Timer::new(zx::MonotonicInstant::after(SCSI_RETRY_DELAY)).await;
                    }
                }
                ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => return Err(error),
                ScsiOutcome::Rejected(_) => return Err(UfsError::IoError),
            }
        }
    }

    /// Sends `TEST UNIT READY` on the admin slot; `send_admin_scsi_command` already retries
    /// transient `UNIT ATTENTION` and `NOT READY` statuses.
    pub async fn send_admin_test_unit_ready(&self, lun: u8) -> Result<(), UfsError> {
        self.send_admin_scsi_command(lun, ScsiOpcode::build_test_unit_ready_cdb()).await
    }

    /// Sends a SCSI command to `lun` and awaits its completion, transferring `data` (if any) to or
    /// from the device. Retries up to [`SCSI_RETRIES`] times on UNIT ATTENTION, NOT READY, BUSY,
    /// and TASK SET FULL. For block I/O use [`Self::submit_io`] instead.
    async fn send_scsi_command(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
    ) -> Result<(), UfsError> {
        let data = data.filter(|d| !matches!(d.buffer, TransferBuffer::Vmo(_)) || d.length != 0);
        if let Some(ref transfer) = data {
            self.validate_transfer(transfer)?;
        }
        let mut retries = 0;
        loop {
            let reserved = self.reserve_io_slot().await?;
            let command = self.build_command_upiu(lun, reserved.slot(), cdb, data.as_ref());
            match self
                .submit(
                    reserved,
                    lun,
                    cdb[0],
                    CommandDescriptorLayout::SCSI_COMMAND,
                    command.as_bytes(),
                    data.clone(),
                )
                .await
            {
                ScsiOutcome::Good => return Ok(()),
                ScsiOutcome::Retry { error: _, delay } if retries < SCSI_RETRIES => {
                    retries += 1;
                    if delay {
                        fasync::Timer::new(zx::MonotonicInstant::after(SCSI_RETRY_DELAY)).await;
                    }
                }
                ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => return Err(error),
                ScsiOutcome::Rejected(_) => return Err(UfsError::IoError),
            }
        }
    }

    /// Sends TEST UNIT READY to `lun`, retrying up to [`TEST_UNIT_READY_ATTEMPTS`] times to clear
    /// any initial Unit Attention condition after reset.
    pub async fn send_test_unit_ready(&self, lun: u8) -> Result<(), UfsError> {
        let mut result = Ok(());
        for attempt in 1..=TEST_UNIT_READY_ATTEMPTS {
            let cdb = ScsiOpcode::build_test_unit_ready_cdb();
            result = self.send_scsi_command(lun, cdb, None).await;
            match &result {
                Ok(()) => break,
                Err(e) => info!(
                    lun,
                    attempt,
                    max_attempts = TEST_UNIT_READY_ATTEMPTS,
                    err:% = e;
                    "TEST UNIT READY attempt failed"
                ),
            }
        }
        result
    }
}

impl<M: Mmio, H: PowerHooks> PowerGuard<'_, M, H> {
    /// Updates the recorded power state under the `request_list` lock and wakes parked client
    /// submitters when admission transitions from closed to open. Returns the previous state.
    ///
    /// Runs `f` with `request_list` held.
    pub fn update_power(
        &mut self,
        f: impl FnOnce(DevicePowerState) -> DevicePowerState,
    ) -> DevicePowerState {
        let processor = self.processor();
        let mut list = processor.request_list.lock();
        let prev = list.power();
        let next = f(prev);
        list.set_power(next);
        if !prev.allows_client_io() && next.allows_client_io() {
            processor.wake_all_slot_waiters(&mut list);
        }
        prev
    }

    /// Returns whether the admin slot is quarantined after a timeout while the power state is
    /// held.
    ///
    /// Polls the admin slot once first so a late hardware completion reclaims it immediately.
    pub fn is_admin_slot_timed_out(&self) -> bool {
        let processor = self.processor();
        let admin_mask = 1u64 << ADMIN_SLOT;
        if (processor.request_list.lock().timeout_mask() & admin_mask) == 0 {
            return false;
        }
        let _ = processor.process_slots(admin_mask);
        (processor.request_list.lock().timeout_mask() & admin_mask) != 0
    }

    async fn stop_queues_and_disable_controller(
        &mut self,
        mark_stopped: bool,
    ) -> Result<(), UfsError> {
        let processor = self.processor();
        let access = self.pause_interrupts_for_power_off();
        {
            let mut list = processor.request_list.lock();
            if mark_stopped {
                list.mark_stopped();
                if access.is_reachable() {
                    processor.controller.with_mmio(|mmio| {
                        UtrListRunStopReg::from_raw(0).write(mmio);
                    });
                }
                processor.wake_all_slot_waiters(&mut list);
            } else {
                // Disarm slot deadlines before stopping the controller so `handle_expired_slots`
                // cannot touch controller registers after `power_off_after` marks them off.
                for slot in 0..MAX_REQUEST_LIST_SIZE as u8 {
                    if let Ok(request_slot) = list.slot_mut(slot) {
                        request_slot.deadline = None;
                    }
                }
                list.set_armed_deadline(None);
                // Stop UTRL and mark the controller powered off atomically to prevent late
                // doorbells.
                self.power_off_after(|mmio| {
                    UtrListRunStopReg::from_raw(0).write(mmio);
                });
            }
        }
        processor.timeout_waker.wake();
        if let Some(res) = access.latched_stop_result() {
            if res.is_err() {
                error!("Host controller disable failed before power-off; quarantining DMA memory");
            }
            return res;
        }
        processor.controller.disable_host_controller().await.inspect_err(|e| {
            error!(err:? = e; "Failed to disable host controller; quarantining DMA memory");
        })
    }

    /// Stops the transfer list and disables the host controller for `PowerDown`.
    ///
    /// Marks the controller powered off so no late doorbell can slip in, sweeps any remaining
    /// controller-owned slots (quarantining client DMA buffers if disabling the controller times
    /// out), and keeps descriptor ring DMA buffers pinned for [`Self::reinit_after_power_down`].
    pub async fn stop_for_power_down(&mut self) -> Result<(), UfsError> {
        let processor = self.processor();
        let _admin_lock = processor.admin_lock.lock().await;
        if let Some(res) = self.register_access().latched_stop_result() {
            return res;
        }
        let disable_res = self.stop_queues_and_disable_controller(false).await;
        processor.sweep_controller_owned_slots(false, disable_res)
    }

    /// Reprograms the transfer request list base address, starts the list, and re-enables
    /// controller interrupts after a `PowerDown` reset.
    pub fn reinit_after_power_down(&mut self) -> Result<(), UfsError> {
        self.processor().init()?;
        self.enable_interrupts();
        Ok(())
    }

    /// Stops the controller so it can no longer access any buffer, fails outstanding requests
    /// (synchronous I/O completes with `zx::Status::IO`, async requests with `UfsError::IoError`),
    /// wakes blocked submitters, and unpins all DMA memory (or quarantines PMTs if `HCE` fails to
    /// clear). Later submissions fail with `IoError`.
    pub async fn quiesce(&mut self) -> Result<(), UfsError> {
        let disable_res = self.stop_queues_and_disable_controller(true).await;
        self.processor().sweep_controller_owned_slots(true, disable_res)
    }
}

/// Reads the RESPONSE UPIU of a SCSI command at `offset` in `ucd`, returning it with its header.
fn read_scsi_response(
    ucd: &ContiguousDmaBuffer,
    offset: usize,
) -> Result<(ResponseUpiu, UpiuHeader), UfsError> {
    // Read 16 bytes (two 64-bit loads: the 12-byte UPIU header plus the 4-byte
    // `residual_transfer_count`) without copying the remaining 40 bytes (reserved fields, sense
    // data, and padding) on the fast path.
    let words: [u64; 2] = ucd.read_struct(offset).map_err(|_| UfsError::IoError)?;
    let (header, _) =
        UpiuHeader::read_from_prefix(words.as_bytes()).map_err(|_| UfsError::IoError)?;
    if header.status == ScsiStatus::Good.as_u8() && header.response == 0 && header.flags == 0 {
        let mut response = ResponseUpiu::new_zeroed();
        response.as_mut_bytes()[..size_of_val(&words)].copy_from_slice(words.as_bytes());
        Ok((response, header))
    } else {
        let full: ResponseUpiu = ucd.read_struct(offset).map_err(|_| UfsError::IoError)?;
        Ok((full, header))
    }
}

/// Checks the transaction code and task tag of a response UPIU.
fn check_response_header(
    response: &[u8],
    expected: UpiuTransactionCode,
    task_tag: u8,
) -> Result<(), UfsError> {
    let (header, _) = UpiuHeader::read_from_prefix(response).map_err(|_| UfsError::IoError)?;
    if header.trans_code() != expected as u8 {
        error!(
            trans_code = header.trans_code(),
            expected = expected as u8;
            "Unexpected response transaction code"
        );
        return Err(UfsError::IoError);
    }
    if header.task_tag != task_tag {
        error!(
            task_tag = header.task_tag, slot = task_tag;
            "Response task tag does not match slot"
        );
        return Err(UfsError::IoError);
    }
    Ok(())
}

/// Decoded fields from fixed- or descriptor-format SCSI sense data (SPC-4 section 4.5).
struct ParsedSense {
    deferred: bool,
    /// True only for current fixed-format sense (`0x70`) with `FILEMARK`, `EOM`, and `ILI` clear
    /// in byte 2 (SPC-4 section 4.5.3), matching C++ `scsi::Controller::CheckSenseData()`.
    /// Descriptor-format sense (`0x72`) carries `ILI`/`EOM`/`FILEMARK` in optional sense data
    /// descriptors that UFS does not parse, so `NoSense`/`RecoveredError` never complete as
    /// `Good` from descriptor format.
    can_complete_with_sense: bool,
    sense_key: u8,
    asc: u8,
    ascq: u8,
}

/// Decodes descriptor-format (SPC-4 section 4.5.2: bytes 1..=3) or fixed-format (SPC-4 section
/// 4.5.3: bytes 2, 12, 13) sense data, or returns `None` if sense data is absent, truncated, or
/// carries an unrecognized response code.
fn parse_sense(response: &ResponseUpiu) -> Option<ParsedSense> {
    let length = (response.sense_data_length.get() as usize).min(response.sense_data.len());
    let sense = &response.sense_data[..length];
    let byte = |index: usize| sense.get(index).copied().unwrap_or(0);
    let response_code = sense.first().copied()? & SENSE_RESPONSE_CODE_MASK;
    match response_code {
        SENSE_DESCRIPTOR_FORMAT_CURRENT | SENSE_DESCRIPTOR_FORMAT_DEFERRED if sense.len() >= 2 => {
            Some(ParsedSense {
                deferred: response_code == SENSE_DESCRIPTOR_FORMAT_DEFERRED,
                can_complete_with_sense: false,
                sense_key: sense[1] & 0xF,
                asc: byte(2),
                ascq: byte(3),
            })
        }
        SENSE_FIXED_FORMAT_CURRENT | SENSE_FIXED_FORMAT_DEFERRED if sense.len() >= 3 => {
            let deferred = response_code == SENSE_FIXED_FORMAT_DEFERRED;
            let status_bits = sense[2] & (SENSE_FILEMARK | SENSE_EOM | SENSE_ILI);
            Some(ParsedSense {
                deferred,
                can_complete_with_sense: !deferred && status_bits == 0,
                sense_key: sense[2] & 0xF,
                asc: byte(12),
                ascq: byte(13),
            })
        }
        _ => None,
    }
}

/// Classification of a UTP command outcome (and SCSI RESPONSE UPIU on slots `0..30`).
#[derive(Debug)]
pub enum ScsiOutcome {
    /// GOOD status and no transport-level response error.
    Good,
    /// A transient failure (UNIT ATTENTION, NOT READY, BUSY, TASK SET FULL) worth retrying.
    Retry { error: UfsError, delay: bool },
    /// A non-retryable transport, target, or sense failure.
    Fail(UfsError),
    /// A block request rejected before submission with an exact `zx::Status`.
    Rejected(zx::Status),
}

impl ScsiOutcome {
    /// Maps the outcome to the `zx::Status` reported to block clients.
    pub fn io_status(&self) -> Result<(), zx::Status> {
        match self {
            Self::Good => Ok(()),
            Self::Retry { error, .. } | Self::Fail(error) => Err(zx::Status::from(*error)),
            Self::Rejected(status) => Err(*status),
        }
    }
}

/// Validates UPIU overflow/underflow flags and `residual_transfer_count` on a completed SCSI
/// command (`Good` or `RecoveredError`/`NoSense`), allowing underflow only on SPC-4
/// allocation-length opcodes.
fn check_scsi_residual(lun: u8, scsi_opcode: u8, response: &ResponseUpiu) -> ScsiOutcome {
    let flags = response.header.flags;
    let residual = response.residual_transfer_count.get();
    if (flags & ResponseUpiu::FLAG_OVERFLOW) != 0 {
        error!(lun, scsi_opcode, flags, residual; "SCSI response reported residual overflow");
        return ScsiOutcome::Fail(UfsError::IoError);
    }
    let underflow = (flags & ResponseUpiu::FLAG_UNDERFLOW) != 0 || residual != 0;
    if underflow && !ScsiOpcode::from_raw(scsi_opcode).is_some_and(ScsiOpcode::allows_underflow) {
        error!(
            lun,
            scsi_opcode,
            flags,
            residual;
            "SCSI response reported unexpected residual underflow"
        );
        return ScsiOutcome::Fail(UfsError::IoError);
    }
    ScsiOutcome::Good
}

/// Classifies a RESPONSE UPIU and logs failures.
fn check_scsi_status(lun: u8, scsi_opcode: u8, response: &ResponseUpiu) -> ScsiOutcome {
    let raw_status = response.header.status;
    let Some(status) = ScsiStatus::from_raw(raw_status) else {
        error!(lun, status = raw_status; "SCSI error status");
        return ScsiOutcome::Fail(UfsError::IoError);
    };
    let outcome = match status {
        ScsiStatus::Good if response.header.response == 0 => ScsiOutcome::Good,
        ScsiStatus::Good => {
            warn!(lun, response = response.header.response; "Target failure");
            ScsiOutcome::Fail(UfsError::IoError)
        }
        ScsiStatus::CheckCondition => {
            let Some(parsed) = parse_sense(response) else {
                warn!(lun; "CHECK CONDITION without valid sense data");
                return ScsiOutcome::Fail(UfsError::CheckCondition);
            };
            match SenseKey::from_raw(parsed.sense_key) {
                // Per SPC-4 sections 4.5.3, 4.5.5, and 4.5.6 (matching C++
                // `scsi::Controller::CheckSenseData()`), only current fixed-format sense (`0x70`)
                // with `FILEMARK`, `EOM`, and `ILI` clear indicates full command completion.
                Some(SenseKey::NoSense) if parsed.can_complete_with_sense => ScsiOutcome::Good,
                Some(SenseKey::RecoveredError) if parsed.can_complete_with_sense => {
                    warn!(lun, asc = parsed.asc, ascq = parsed.ascq; "SCSI recovered error");
                    ScsiOutcome::Good
                }
                Some(SenseKey::UnitAttention) => {
                    info!(lun, asc = parsed.asc, ascq = parsed.ascq; "UNIT ATTENTION");
                    ScsiOutcome::Retry { error: UfsError::CheckCondition, delay: false }
                }
                Some(SenseKey::NotReady) => {
                    info!(lun, asc = parsed.asc, ascq = parsed.ascq; "NOT READY");
                    ScsiOutcome::Retry { error: UfsError::CheckCondition, delay: true }
                }
                Some(SenseKey::NoSense | SenseKey::RecoveredError | SenseKey::IllegalRequest)
                | None => {
                    warn!(
                        lun,
                        deferred = parsed.deferred,
                        sense_key = parsed.sense_key,
                        asc = parsed.asc,
                        ascq = parsed.ascq;
                        "CHECK CONDITION"
                    );
                    ScsiOutcome::Fail(UfsError::CheckCondition)
                }
            }
        }
        ScsiStatus::Busy | ScsiStatus::TaskSetFull => {
            warn!(lun, status = status.as_u8(); "Transient SCSI status");
            ScsiOutcome::Retry { error: UfsError::IoError, delay: true }
        }
    };
    if matches!(outcome, ScsiOutcome::Good) {
        check_scsi_residual(lun, scsi_opcode, response)
    } else {
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::RegisterAccess;
    use crate::device_manager::DeviceManager;
    use crate::power::{PowerControl, PowerState};
    use crate::query::QueryOpcode;
    use crate::quirks::HostQuirks;
    use crate::registers::HostControllerEnableReg;
    use crate::scsi::{InquiryData, ScsiOpcode};
    use crate::testing::fake_ufs::{FakeMmio, FakeUfs, FakeUtp, wait_until};
    use crate::testing::{
        ADMIN_SLOT, IoOutcomeReceiver, OCS_SUCCESS, PAGE_SIZE, check_condition,
        create_test_processor, delivered, expire_deadline, fake_utp, io_callback, nop_in,
        scsi_response, slot_pinned, slot_state, wait_async, wait_for_any_doorbell,
    };
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_inspect::Inspector;
    use futures::future::join_all;
    use futures::poll;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use zerocopy::byteorder::big_endian::U32;

    /// UCD byte offset of a NOP IN response.
    const NOP_RESPONSE_OFFSET: usize = CommandDescriptorLayout::NOP.response_offset();

    #[fuchsia::test]
    async fn test_nop_out_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            let utrd = utp.utrd(ADMIN_SLOT);
            let request: NopOutUpiu = utp.request(ADMIN_SLOT);
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
            (utrd, request)
        });

        assert_eq!(processor.send_nop_out().await, Ok(()));
        let (utrd, request) = hardware.join().unwrap();
        assert_eq!(utrd.command_type(), COMMAND_TYPE_UFS_STORAGE);
        assert_eq!(utrd.data_direction(), DataDirection::NoData as u32);
        assert!(utrd.interrupt());
        assert_eq!((utrd.response_upiu_offset(), utrd.response_upiu_length()), (8, 8));
        assert_eq!(utrd.prdt_length(), 0);
        assert_eq!(request.header.trans_code(), UpiuTransactionCode::NopOut as u8);
        assert_eq!(request.header.task_tag, ADMIN_SLOT);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_admin_ocs_and_response_validation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            // 1: success, leaving a valid NOP IN in the UCD.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
            // 2: communication failure without a new response.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.set_ocs(ADMIN_SLOT, OverallCommandStatus::CommunicationFailure as u8);
            utp.clear_doorbell(ADMIN_SLOT);
            utp.raise_completion_interrupt();
            // 3: OCS success but no response written.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.set_ocs(ADMIN_SLOT, OCS_SUCCESS);
            utp.clear_doorbell(ADMIN_SLOT);
            utp.raise_completion_interrupt();
            // 4: response with the wrong task tag.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(3).as_bytes(), OCS_SUCCESS);
        });

        assert_eq!(processor.send_nop_out().await, Ok(()));
        assert_eq!(processor.send_nop_out().await, Err(UfsError::IoError));
        assert_eq!(processor.send_nop_out().await, Err(UfsError::IoError));
        assert_eq!(processor.send_nop_out().await, Err(UfsError::IoError));
        hardware.join().unwrap();
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_scsi_ocs_and_response_validation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            // 1: GOOD response but invalid PRDT attributes.
            let slot = wait_for_any_doorbell(&utp);
            let response = scsi_response(slot, 0);
            utp.complete(
                slot,
                response.as_bytes(),
                OverallCommandStatus::InvalidPrdtAttributes as u8,
            );
            // 2: REJECT UPIU instead of RESPONSE.
            let slot = wait_for_any_doorbell(&utp);
            let mut response = scsi_response(slot, 0);
            response.header.trans_type = UpiuTransactionCode::RejectUpiu as u8;
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            // 3: wrong task tag.
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot + 1, 0).as_bytes(), OCS_SUCCESS);
            // 4: target failure.
            let slot = wait_for_any_doorbell(&utp);
            let mut response = scsi_response(slot, 0);
            response.header.response = 1;
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            // 5: success.
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        for _ in 0..4 {
            assert_eq!(
                processor.send_scsi_command(0, cdb, None).await.err(),
                Some(UfsError::IoError)
            );
        }
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_concurrent_queries_serialize() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            for _ in 0..4 {
                assert!(utp.wait_for_doorbell(ADMIN_SLOT));
                let request: QueryUpiu = utp.request(ADMIN_SLOT);
                let mut response = request;
                response.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
                response.value = U32::new(request.idn as u32 * 10);
                utp.complete(ADMIN_SLOT, response.as_bytes(), OCS_SUCCESS);
            }
        });

        let requests: Vec<_> =
            (1..=4).map(|idn| QueryUpiu::new(QueryOpcode::ReadAttribute, idn, 0, 0)).collect();
        let results = join_all(requests.iter().map(|r| processor.send_query_upiu(r))).await;
        for (idn, result) in (1..=4).zip(results) {
            assert_eq!(result.unwrap().value(), idn * 10);
        }
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_dropped_request_frees_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Dropped after the completion interrupt was delivered: `finish_slot_locked` already
        // released the slot immediately upon completion.
        let mut request = Box::pin(processor.send_nop_out());
        assert!(poll!(request.as_mut()).is_pending());
        utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        drop(request);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);

        // Dropped while the controller still owns the slot: the completion interrupt frees it,
        // invalidates read caches, and unpins the orphaned buffer.
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());
        drop(request);
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));

        // The admin slot is still usable.
        let hardware = thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        });
        assert_eq!(processor.send_nop_out().await, Ok(()));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_ocs_invalid_after_doorbell_clear() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Async UPIU: doorbell bit cleared while `OCS` stays `0x0F` fails immediately with
        // `Err(UfsError::IoError)` and releases the slot.
        let mut request = Box::pin(processor.send_nop_out());
        assert!(poll!(request.as_mut()).is_pending());
        utp.write_ucd(ADMIN_SLOT, NOP_RESPONSE_OFFSET, nop_in(ADMIN_SLOT).as_bytes());
        utp.clear_doorbell(ADMIN_SLOT);
        assert!(processor.process_slot_completion(ADMIN_SLOT));
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        assert_eq!(request.await, Err(UfsError::IoError));

        // Sync I/O: doorbell bit cleared while `OCS` stays `0x0F` delivers `Err(zx::Status::IO)`.
        let mut rx = submit_tur(&processor, 0);
        utp.clear_doorbell(0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_scsi_command_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let data_vmo_clone = Arc::clone(&data_vmo);

        let hardware = thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let utrd = utp.utrd(slot);
            let command: CommandUpiu = utp.request(slot);
            let prdt_offset = CommandDescriptorLayout::SCSI_COMMAND.prdt_offset();
            let prdt_bytes = utp.read_ucd(slot, prdt_offset, size_of::<PrdtEntry>());
            let prdt = PrdtEntry::read_from_bytes(&prdt_bytes).unwrap();

            // The fake BTI's addresses are not real, so place the data directly in the VMO.
            let data =
                FakeUfs::new().execute_scsi_cmd(&command.cdb, DataDirection::DeviceToHost).unwrap();
            data_vmo_clone.write(&data, 0).unwrap();
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
            (slot, utrd, command, prdt)
        });

        let cdb = ScsiOpcode::build_inquiry_cdb(36);
        processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 36)))
            .await
            .unwrap();

        let (slot, utrd, command, prdt) = hardware.join().unwrap();
        assert_eq!(utrd.data_direction(), DataDirection::DeviceToHost as u32);
        assert_eq!((utrd.response_upiu_offset(), utrd.response_upiu_length()), (8, 14));
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        assert_eq!(prdt.byte_count(), 36);
        assert_eq!(command.header.task_tag, slot);
        assert_eq!(command.header.flags, CommandUpiu::FLAG_READ);
        assert_eq!(command.expected_data_transfer_length.get(), 36);
        assert_eq!(command.cdb[0], ScsiOpcode::Inquiry as u8);
        assert!(!slot_pinned(&processor, slot));

        let mut inquiry_bytes = [0u8; 36];
        data_vmo.read(&mut inquiry_bytes, 0).unwrap();
        let inquiry = InquiryData::read_from_bytes(&inquiry_bytes[..]).unwrap();
        assert_eq!(inquiry.vendor_id, *b"FUCHSIA ");
        assert_eq!(inquiry.product_id, *b"UFS MOCK DEVICE ");
    }

    #[fuchsia::test]
    async fn test_scsi_write_descriptors() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(3 * PAGE_SIZE).unwrap());

        let hardware = thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let utrd = utp.utrd(slot);
            let command: CommandUpiu = utp.request(slot);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
            (utrd, command)
        });

        let cdb = ScsiOpcode::build_write_10_cdb(0, 2);
        processor
            .send_scsi_command(0, cdb, Some(DataTransfer::write(Arc::clone(&data_vmo), 4096, 8192)))
            .await
            .unwrap();
        let (utrd, command) = hardware.join().unwrap();
        assert_eq!(utrd.data_direction(), DataDirection::HostToDevice as u32);
        assert_eq!(utrd.prdt_length(), 2);
        assert_eq!(command.header.flags, CommandUpiu::FLAG_WRITE);
    }

    #[fuchsia::test]
    async fn test_scsi_rejects_invalid_buffers() {
        let (_mmio_vmo, processor) = create_test_processor();
        let data_vmo = Arc::new(zx::Vmo::create(258 * PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let err = Some(UfsError::InvalidParameter);

        let unaligned = processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 2, 4096)))
            .await;
        assert_eq!(unaligned.err(), err);
        let too_large = processor
            .send_scsi_command(
                0,
                cdb,
                Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 257 * 4096)),
            )
            .await;
        assert_eq!(too_large.err(), err);
        let overflow = processor
            .send_scsi_command(
                0,
                cdb,
                Some(DataTransfer::read(Arc::clone(&data_vmo), u64::MAX - 8, 4096)),
            )
            .await;
        assert_eq!(overflow.err(), err);

        for slot in 0..SDB_SLOTS as u8 {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
            assert!(!slot_pinned(&processor, slot));
        }
        assert!(!processor.process_slot_completion(40));
    }

    #[fuchsia::test]
    async fn test_scsi_transient_status_retry() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Busy.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );
            let slot = wait_for_any_doorbell(&utp);
            let response = check_condition(slot, SenseKey::UnitAttention.as_u8());
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_send_scsi_command_retries_not_ready_check_condition() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            for _ in 0..2 {
                let slot = wait_for_any_doorbell(&utp);
                let response = check_condition(slot, SenseKey::NotReady.as_u8());
                utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            }
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);

            for _ in 0..(SCSI_RETRIES + 1) {
                let slot = wait_for_any_doorbell(&utp);
                let response = check_condition(slot, SenseKey::NotReady.as_u8());
                utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            }
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let start = zx::MonotonicInstant::get();
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        assert!(zx::MonotonicInstant::get() - start >= SCSI_RETRY_DELAY * 2);

        assert_eq!(
            processor.send_scsi_command(0, cdb, None).await.unwrap_err(),
            UfsError::CheckCondition
        );
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_scsi_check_condition_not_retried() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let response = check_condition(slot, SenseKey::IllegalRequest.as_u8());
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            utp
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let result = processor.send_scsi_command(0, cdb, None).await;
        assert_eq!(result.err(), Some(UfsError::CheckCondition));
        let utp = hardware.join().unwrap();
        assert_eq!(utp.doorbell(), 0);
    }

    #[fuchsia::test]
    async fn test_scsi_check_condition_recovered_error_and_no_sense_succeed() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            // 1. Plain fixed-format (0x70) RECOVERED ERROR (0x01) on READ(10) completes Ok(()).
            let slot1 = wait_for_any_doorbell(&utp);
            let mut resp1 = check_condition(slot1, SenseKey::RecoveredError.as_u8());
            resp1.sense_data[12] = 0x17;
            resp1.sense_data[13] = 0x01;
            utp.complete(slot1, resp1.as_bytes(), OCS_SUCCESS);

            // 2. Plain fixed-format (0x70) NO SENSE (0x00) completes Ok(()).
            let slot2 = wait_for_any_doorbell(&utp);
            let resp2 = check_condition(slot2, SenseKey::NoSense.as_u8());
            utp.complete(slot2, resp2.as_bytes(), OCS_SUCCESS);

            // 3. Fixed-format NO SENSE with ILI (0x20) set on READ(10) fails with CheckCondition.
            let slot3 = wait_for_any_doorbell(&utp);
            let mut resp3 = check_condition(slot3, SenseKey::NoSense.as_u8());
            resp3.sense_data[2] |= SENSE_ILI;
            utp.complete(slot3, resp3.as_bytes(), OCS_SUCCESS);

            // 4. Fixed-format RECOVERED ERROR with EOM (0x40) set fails with CheckCondition.
            let slot4 = wait_for_any_doorbell(&utp);
            let mut resp4 = check_condition(slot4, SenseKey::RecoveredError.as_u8());
            resp4.sense_data[2] |= SENSE_EOM;
            utp.complete(slot4, resp4.as_bytes(), OCS_SUCCESS);

            // 5. Fixed-format NO SENSE with FILEMARK (0x80) set fails with CheckCondition.
            let slot5 = wait_for_any_doorbell(&utp);
            let mut resp5 = check_condition(slot5, SenseKey::NoSense.as_u8());
            resp5.sense_data[2] |= SENSE_FILEMARK;
            utp.complete(slot5, resp5.as_bytes(), OCS_SUCCESS);

            // 6. Descriptor-format (0x72) NO SENSE fails with CheckCondition (matching C++
            //    `scsi::Controller::CheckSenseData()`).
            let slot6 = wait_for_any_doorbell(&utp);
            let mut resp6 = scsi_response(slot6, ScsiStatus::CheckCondition.as_u8());
            resp6.sense_data_length = zerocopy::byteorder::big_endian::U16::new(8);
            resp6.sense_data[0] = SENSE_DESCRIPTOR_FORMAT_CURRENT;
            resp6.sense_data[1] = SenseKey::NoSense.as_u8();
            utp.complete(slot6, resp6.as_bytes(), OCS_SUCCESS);
        });

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let (done1, rx1) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_read_10_cdb(0, 1),
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            done1,
        );
        assert_eq!(rx1.await.unwrap().io_status(), Ok(()));

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert_eq!(processor.send_scsi_command(0, cdb, None).await, Ok(()));

        let (done3, rx3) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_read_10_cdb(0, 1),
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            done3,
        );
        assert_eq!(rx3.await.unwrap().io_status(), Err(zx::Status::IO_REFUSED));

        let (done4, rx4) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_read_10_cdb(0, 1),
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            done4,
        );
        assert_eq!(rx4.await.unwrap().io_status(), Err(zx::Status::IO_REFUSED));

        assert_eq!(processor.send_scsi_command(0, cdb, None).await, Err(UfsError::CheckCondition));
        assert_eq!(processor.send_scsi_command(0, cdb, None).await, Err(UfsError::CheckCondition));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_scsi_residual_underflow_and_overflow_validation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = thread::spawn(move || {
            // 1. READ(10) with FLAG_UNDERFLOW fails with IoError.
            let slot1 = wait_for_any_doorbell(&utp);
            let mut resp1 = scsi_response(slot1, ScsiStatus::Good.as_u8());
            resp1.header.flags = ResponseUpiu::FLAG_UNDERFLOW;
            resp1.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(2048);
            utp.complete(slot1, resp1.as_bytes(), OCS_SUCCESS);

            // 2. WRITE(10) with non-zero residual_transfer_count and flags == 0 (fast path) fails.
            let slot2 = wait_for_any_doorbell(&utp);
            let mut resp2 = scsi_response(slot2, ScsiStatus::Good.as_u8());
            resp2.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(4096);
            utp.complete(slot2, resp2.as_bytes(), OCS_SUCCESS);

            // 3. INQUIRY with FLAG_UNDERFLOW (SPC-4 allocation-length truncation) succeeds.
            let slot3 = wait_for_any_doorbell(&utp);
            let mut resp3 = scsi_response(slot3, ScsiStatus::Good.as_u8());
            resp3.header.flags = ResponseUpiu::FLAG_UNDERFLOW;
            resp3.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(60);
            utp.complete(slot3, resp3.as_bytes(), OCS_SUCCESS);

            // 4. INQUIRY with FLAG_OVERFLOW still fails.
            let slot4 = wait_for_any_doorbell(&utp);
            let mut resp4 = scsi_response(slot4, ScsiStatus::Good.as_u8());
            resp4.header.flags = ResponseUpiu::FLAG_OVERFLOW;
            resp4.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(16);
            utp.complete(slot4, resp4.as_bytes(), OCS_SUCCESS);

            // 5. READ BUFFER with FLAG_UNDERFLOW (short error-history directory) succeeds.
            let slot5 = wait_for_any_doorbell(&utp);
            let mut resp5 = scsi_response(slot5, ScsiStatus::Good.as_u8());
            resp5.header.flags = ResponseUpiu::FLAG_UNDERFLOW;
            resp5.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(4032);
            utp.complete(slot5, resp5.as_bytes(), OCS_SUCCESS);

            // 6. WRITE BUFFER with FLAG_UNDERFLOW (data-out parameter list length) fails.
            let slot6 = wait_for_any_doorbell(&utp);
            let mut resp6 = scsi_response(slot6, ScsiStatus::Good.as_u8());
            resp6.header.flags = ResponseUpiu::FLAG_UNDERFLOW;
            resp6.residual_transfer_count = zerocopy::byteorder::big_endian::U32::new(2048);
            utp.complete(slot6, resp6.as_bytes(), OCS_SUCCESS);
        });

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let (done1, rx1) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_read_10_cdb(0, 1),
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            done1,
        );
        assert_eq!(rx1.await.unwrap().io_status(), Err(zx::Status::IO));

        let (done2, rx2) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_write_10_cdb(0, 1),
            Some(DataTransfer::write(Arc::clone(&data_vmo), 0, 4096)),
            done2,
        );
        assert_eq!(rx2.await.unwrap().io_status(), Err(zx::Status::IO));

        let inquiry_cdb = ScsiOpcode::build_inquiry_cdb(96);
        assert_eq!(
            processor
                .send_scsi_command(
                    0,
                    inquiry_cdb,
                    Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 96)),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            processor
                .send_scsi_command(
                    0,
                    inquiry_cdb,
                    Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 96)),
                )
                .await,
            Err(UfsError::IoError)
        );
        let read_buf_cdb =
            ScsiOpcode::build_buffer_cdb(DataDirection::DeviceToHost, 0x1C, 0, 0, 4096);
        assert_eq!(
            processor
                .send_scsi_command(
                    0,
                    read_buf_cdb,
                    Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
                )
                .await,
            Ok(())
        );
        let write_buf_cdb =
            ScsiOpcode::build_buffer_cdb(DataDirection::HostToDevice, 0x02, 0, 0, 4096);
        assert_eq!(
            processor
                .send_scsi_command(
                    0,
                    write_buf_cdb,
                    Some(DataTransfer::write(Arc::clone(&data_vmo), 0, 4096)),
                )
                .await,
            Err(UfsError::IoError)
        );
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_scsi_reused_slot_ignores_stale_sense() {
        // The controller writes only the part before the data segment when there is no sense data.
        let without_sense = |response: &ResponseUpiu| {
            response.as_bytes()[..std::mem::offset_of!(ResponseUpiu, sense_data_length)].to_vec()
        };

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Leave UNIT ATTENTION sense data in the response area of the slot.
        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let response = check_condition(slot, SenseKey::UnitAttention.as_u8());
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            assert_eq!(wait_for_any_doorbell(&utp), slot);
            utp.complete(slot, &without_sense(&scsi_response(slot, 0)), OCS_SUCCESS);
            (utp, slot)
        });
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        let (mut utp, first_slot) = hardware.join().unwrap();

        // A CHECK CONDITION without sense data must fail instead of being retried as the stale
        // UNIT ATTENTION.
        let stop = Arc::new(AtomicBool::new(false));
        let hardware = std::thread::spawn({
            let stop = stop.clone();
            move || {
                let mut completions = 0;
                while !stop.load(Ordering::Acquire) {
                    if utp.doorbell() == 0 {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                        continue;
                    }
                    let slot = utp.doorbell().trailing_zeros() as u8;
                    assert_eq!(slot, first_slot);
                    let response = scsi_response(slot, ScsiStatus::CheckCondition.as_u8());
                    utp.complete(slot, &without_sense(&response), OCS_SUCCESS);
                    completions += 1;
                }
                completions
            }
        });
        let result = processor.send_scsi_command(0, cdb, None).await;
        stop.store(true, Ordering::Release);
        assert_eq!(result.err(), Some(UfsError::CheckCondition));
        assert_eq!(hardware.join().unwrap(), 1);
    }

    #[fuchsia::test]
    async fn test_slot_waiter_woken_on_release() {
        let (_mmio_vmo, processor) = create_test_processor();

        let mut guards = Vec::new();
        for _ in 0..ADMIN_SLOT {
            guards.push(processor.reserve_io_slot().await.unwrap());
        }
        assert!(guards.iter().all(|guard| guard.slot() < ADMIN_SLOT));

        let mut waiter = Box::pin(processor.reserve_io_slot());
        assert!(poll!(waiter.as_mut()).is_pending());
        let released = guards.remove(5).slot();
        match poll!(waiter.as_mut()) {
            Poll::Ready(Ok(guard)) => assert_eq!(guard.slot(), released),
            other => panic!("Waiter not woken: {:?}", other.map(|r| r.map(|g| g.slot()))),
        }
    }

    #[fuchsia::test]
    async fn test_quiesce_stops_controller_and_fails_requests() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        assert!(!UtrListRunStopReg::read(&utp.regs).value());
        assert!(!HostControllerEnableReg::read(&utp.regs).host_controller_enable());

        assert_eq!(request.await.err(), Some(UfsError::IoError));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert!(!processor.request_list.lock().io_buffer().is_pinned());

        assert_eq!(processor.send_nop_out().await, Err(UfsError::IoError));
        assert_eq!(processor.init(), Err(UfsError::IoError));
    }

    /// Submits a synchronous TEST UNIT READY on `lun` and returns its outcome receiver.
    fn submit_tur(processor: &TransferRequestProcessor<FakeMmio>, lun: u8) -> IoOutcomeReceiver {
        let (completion, rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        processor.submit_io(lun, cdb, None, completion);
        rx
    }

    #[fuchsia::test]
    async fn test_submit_io_completes_through_callback() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // The submitter returns as soon as the doorbell is rung and does not wait.
        let (completion, mut rx) = io_callback();
        let transfer = DataTransfer::read(Arc::clone(&data_vmo), 0, 4096);
        processor.submit_io(3, cdb, Some(transfer), completion);
        assert_eq!(utp.doorbell(), 1);
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().scheduled_mask().count_ones(), 1);
        assert!(rx.try_recv().unwrap().is_none());

        let utrd = utp.utrd(0);
        assert_eq!(utrd.data_direction(), DataDirection::DeviceToHost as u32);
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        let command: CommandUpiu = utp.request(0);
        assert_eq!(command.header.lun, 3);
        assert_eq!(command.header.task_tag, 0);
        assert_eq!(command.header.flags, CommandUpiu::FLAG_READ);
        assert_eq!(command.expected_data_transfer_length.get(), 4096);

        // The completion interrupt decodes the response, frees the slot and delivers the outcome.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().scheduled_mask().count_ones(), 0);
    }

    #[fuchsia::test]
    async fn test_submit_io_reports_failures_without_retrying() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Transient statuses, CHECK CONDITION, OCS and header failures are reported as-is.
        let mut busy = submit_tur(&processor, 0);
        let mut unit_attention = submit_tur(&processor, 0);
        let mut illegal = submit_tur(&processor, 0);
        let mut ocs = submit_tur(&processor, 0);
        let mut wrong_tag = submit_tur(&processor, 0);
        let mut good = submit_tur(&processor, 0);
        assert_eq!(utp.doorbell(), 0b11_1111);
        utp.complete(0, scsi_response(0, ScsiStatus::Busy.as_u8()).as_bytes(), OCS_SUCCESS);
        utp.complete(
            1,
            check_condition(1, SenseKey::UnitAttention.as_u8()).as_bytes(),
            OCS_SUCCESS,
        );
        utp.complete(
            2,
            check_condition(2, SenseKey::IllegalRequest.as_u8()).as_bytes(),
            OCS_SUCCESS,
        );
        utp.complete(3, scsi_response(3, 0).as_bytes(), OverallCommandStatus::Aborted as u8);
        utp.complete(4, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);

        // Each completion interrupt delivered its outcome.
        assert_eq!(delivered(&mut busy), Err(zx::Status::IO));
        assert_eq!(delivered(&mut unit_attention), Err(zx::Status::IO_REFUSED));
        assert_eq!(delivered(&mut illegal), Err(zx::Status::IO_REFUSED));
        assert_eq!(delivered(&mut ocs), Err(zx::Status::IO));
        assert_eq!(delivered(&mut wrong_tag), Err(zx::Status::IO));
        assert_eq!(delivered(&mut good), Ok(()));
        // Nothing was resubmitted.
        assert_eq!(utp.doorbell(), 0);
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
    }

    #[fuchsia::test]
    async fn test_submit_io_rejects_invalid_transfer_without_reserving_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // Fill every I/O slot (`0..ADMIN_SLOT`) so `reserve_io_slot_locked` would block on Condvar
        // if `submit_io` attempted reservation before validating `data`.
        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();

        let (completion, mut rx) = io_callback();
        let unaligned = DataTransfer::read(Arc::clone(&data_vmo), 2, 4096);
        processor.submit_io(0, cdb, Some(unaligned), completion);
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
        assert_eq!(processor.requests().blocked_reservers(), 0);

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        for rx in &mut inflight {
            assert_eq!(delivered(rx), Err(zx::Status::IO));
        }
    }

    #[fuchsia::test]
    async fn test_submit_io_blocks_until_slot_frees() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Fill every I/O slot.
        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();
        assert_eq!(utp.doorbell(), (1u32 << ADMIN_SLOT) - 1);

        // The next submission blocks on its thread until a slot is released.
        let submitted = Arc::new(AtomicBool::new(false));
        let blocked = {
            let processor = Arc::clone(&processor);
            let submitted = Arc::clone(&submitted);
            thread::spawn(move || {
                let rx = submit_tur(&processor, 0);
                submitted.store(true, Ordering::SeqCst);
                rx
            })
        };
        assert!(wait_until(|| processor.requests().blocked_reservers() == 1));
        thread::sleep(Duration::from_millis(20));
        assert!(!submitted.load(Ordering::SeqCst));

        // Completing slot 5 hands it to the blocked submitter, which rings it again.
        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut inflight[5]), Ok(()));
        let mut rx = blocked.join().unwrap();
        assert!(submitted.load(Ordering::SeqCst));
        assert_eq!(processor.requests().blocked_reservers(), 0);
        assert_eq!(slot_state(&processor, 5), SlotState::Scheduled);
        assert_eq!(utp.doorbell(), (1u32 << ADMIN_SLOT) - 1);

        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_quiesce_fails_blocked_and_inflight_io() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);

        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();
        let blocked = {
            let processor = Arc::clone(&processor);
            thread::spawn(move || {
                let (completion, rx) = io_callback();
                let cdb = ScsiOpcode::build_test_unit_ready_cdb();
                processor.submit_io(0, cdb, None, completion);
                rx
            })
        };
        assert!(wait_until(|| processor.requests().blocked_reservers() == 1));

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        let mut rx = blocked.join().unwrap();
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        for rx in &mut inflight {
            assert_eq!(delivered(rx), Err(zx::Status::IO));
        }
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
        assert_eq!(processor.request_list.lock().scheduled_mask().count_ones(), 0);

        // Later synchronous submissions fail immediately.
        let (completion, mut rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        processor.submit_io(0, cdb, None, completion);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_submit_io_inline_write() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let payload = [0xAAu8; 24];
        let cdb = ScsiOpcode::build_unmap_cdb(payload.len() as u16);

        let (completion, mut rx) = io_callback();
        processor.submit_io(0, cdb, Some(DataTransfer::inline_write(&payload)), completion);
        assert_eq!(utp.doorbell(), 1);
        assert!(!slot_pinned(&processor, 0));

        let utrd = utp.utrd(0);
        assert_eq!(utrd.data_direction(), DataDirection::HostToDevice as u32);
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        let ucd_phys = u64::from(utrd.command_descriptor_base_addr)
            | (u64::from(utrd.command_descriptor_base_addr_upper) << 32);
        let prdt_offset = CommandDescriptorLayout::SCSI_COMMAND.prdt_offset();
        let prdt =
            PrdtEntry::read_from_bytes(&utp.read_ucd(0, prdt_offset, size_of::<PrdtEntry>()))
                .unwrap();
        assert_eq!(prdt.dma_address(), ucd_phys + INLINE_DATA_OFFSET as u64);
        assert_eq!(prdt.byte_count(), payload.len() as u32);
        assert_eq!(utp.read_ucd(0, INLINE_DATA_OFFSET, payload.len()), payload);

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));

        // Non-dword-aligned or oversized inline slices fail and release the reserved slot.
        let (completion, mut rx) = io_callback();
        processor.submit_io(0, cdb, Some(DataTransfer::inline_write(&[1u8; 6])), completion);
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));

        let (completion, mut rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::inline_write(&[1u8; INLINE_DATA_SIZE + 4])),
            completion,
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert_eq!(processor.busy_slot_count(), 0);
    }

    /// Polls `request` once and returns the slot whose doorbell it rang.
    async fn poll_and_find_slot(request: &mut Pin<Box<impl Future>>, utp: &FakeUtp) -> u8 {
        let before = utp.doorbell();
        assert!(poll!(request.as_mut()).is_pending());
        let rung = utp.doorbell() & !before;
        assert_ne!(rung, 0, "No doorbell rung");
        rung.trailing_zeros() as u8
    }

    #[fuchsia::test]
    async fn test_io_paused_blocks_client_io_and_allows_internal_scsi() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;

        guard.set_state(PowerState::SystemSleep, false);
        assert!(guard.power().io_paused);

        // Normal client IO blocks before reserving a slot.
        let mut client_req = Box::pin(async {
            let tx = processor.begin_client_transaction().await?;
            tx.send_scsi_command(0, cdb, None).await
        });
        assert!(poll!(client_req.as_mut()).is_pending());
        assert_eq!(utp.doorbell(), 0);

        // Driver-internal SCSI command (`send_scsi_command`) bypasses client IO admission.
        let mut internal_req = Box::pin(processor.send_scsi_command(0, cdb, None));
        let slot = poll_and_find_slot(&mut internal_req, &utp).await;
        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert!(internal_req.await.is_ok());

        // Reopening client IO wakes the waiting client request.
        guard.set_state(PowerState::Active, false);
        let slot = poll_and_find_slot(&mut client_req, &utp).await;
        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert!(client_req.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_io_paused_blocks_submit_io_until_resumed() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;

        guard.set_state(PowerState::SystemSleep, false);

        // A synchronous block request blocks its session thread before reserving a slot.
        let submitted = Arc::new(AtomicBool::new(false));
        let blocked = {
            let processor = Arc::clone(&processor);
            let submitted = Arc::clone(&submitted);
            thread::spawn(move || {
                let rx = submit_tur(&processor, 0);
                submitted.store(true, Ordering::SeqCst);
                rx
            })
        };
        assert!(wait_until(|| processor.requests().blocked_on_admission() == 1));
        thread::sleep(Duration::from_millis(20));
        assert!(!submitted.load(Ordering::SeqCst));
        assert_eq!(utp.doorbell(), 0);
        assert_eq!(processor.busy_slot_count(), 0);

        // Driver-internal SCSI commands still bypass client IO admission.
        let mut internal_req = Box::pin(processor.send_scsi_command(0, cdb, None));
        let slot = poll_and_find_slot(&mut internal_req, &utp).await;
        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert!(internal_req.await.is_ok());
        assert!(!submitted.load(Ordering::SeqCst));

        // Reopening client IO wakes the blocked submitter, which rings its doorbell.
        guard.set_state(PowerState::Active, false);
        let mut rx = blocked.join().unwrap();
        assert!(submitted.load(Ordering::SeqCst));
        let slot = wait_for_any_doorbell(&utp);
        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_reserved_slot_schedules_after_suspend_and_drains() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;
        // Simulate `submit_io` reserving a slot right before power state pauses client IO.
        let reserved = {
            let mut list = processor.request_list.lock();
            processor.reserve_io_slot_locked(&mut list).unwrap()
        };
        let slot = reserved.slot();
        assert_eq!(processor.busy_slot_count(), 1);

        guard.set_state(PowerState::SystemSleep, false);
        let mut drain = Box::pin(processor.drain_inflight(COMMAND_TIMEOUT));
        assert!(poll!(&mut drain).is_pending());

        // Finishing `schedule_slot` while `io_paused` is set must succeed so the reserved
        // request completes and `drain_inflight` waits for it.
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let mut command = CommandUpiu::new(cdb, 0, DataDirection::NoData);
        command.header.lun = 0;
        command.header.task_tag = slot;
        let (completion, mut rx) = io_callback();
        processor.schedule_slot(
            reserved,
            cdb[0],
            CommandDescriptorLayout::SCSI_COMMAND,
            command.as_bytes(),
            None,
            SlotCompletion { lun: 0, completion, io_op: None },
        );
        assert_eq!(utp.doorbell(), 1u32 << slot);
        assert!(poll!(&mut drain).is_pending());

        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
        assert_eq!(drain.await, Ok(()));
    }

    #[fuchsia::test]
    async fn test_quiesce_fails_submit_io_blocked_on_admission() {
        let (_mmio_vmo, processor) = create_test_processor();
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;

        guard.set_state(PowerState::SystemSleep, false);
        let blocked = {
            let processor = Arc::clone(&processor);
            thread::spawn(move || {
                let (completion, rx) = io_callback();
                let cdb = ScsiOpcode::build_test_unit_ready_cdb();
                processor.submit_io(0, cdb, None, completion);
                rx
            })
        };
        assert!(wait_until(|| processor.requests().blocked_on_admission() == 1));

        // Stopping the processor fails the suspended submitter instead of leaving it blocked.
        assert_eq!(guard.quiesce().await, Ok(()));
        let mut rx = blocked.join().unwrap();
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_io_timeout_task_wakes_promptly_on_quiesce() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let finished = Arc::new(AtomicBool::new(false));
        let timeout_task = {
            let processor = Arc::clone(&processor);
            let finished = Arc::clone(&finished);
            fasync::Task::local(async move {
                processor.run_io_timeout_task().await;
                finished.store(true, Ordering::SeqCst);
            })
        };

        // An in-flight request parks the task on a timer at its `COMMAND_TIMEOUT` deadline.
        let mut rx = submit_tur(&processor, 0);
        fasync::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(10)))
            .await;
        assert!(!finished.load(Ordering::SeqCst));

        // Quiescing cuts that timer short instead of letting it run out.
        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        assert!(wait_async(|| finished.load(Ordering::SeqCst)).await);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        drop(timeout_task);
    }

    #[fuchsia::test]
    async fn test_async_and_sync_read_completion_unifies_cache_invalidation_and_pmt_unpin() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // 1. Async read: `finish_slot_locked` + `DeferredCompletions::complete_after_dma` frees
        //    the slot and unpins the `zx::Pmt` inside `utp.complete(...)` BEFORE the async future
        //    is polled.
        let mut async_req = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(async_req.as_mut()).is_pending());
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert!(async_req.await.is_ok());

        // 2. Sync read: same `finish_slot_locked` + `DeferredCompletions::complete_after_dma` path.
        let (completion, mut rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            completion,
        );
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(delivered(&mut rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_overdue_data_slot_logs_and_completes_cleanly_on_late_irq() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let timeout_task = {
            let processor = Arc::clone(&processor);
            fasync::Task::local(async move { processor.run_io_timeout_task().await })
        };

        let (completion, mut rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            completion,
        );
        expire_deadline(&processor, 0);
        processor.timeout_waker.wake();

        assert!(wait_async(|| processor.data_timeout_logs() >= 1).await);
        // Slot stays `Scheduled` with its `zx::Pmt` pinned while hardware still owns it.
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));

        // Late completion interrupt frees the slot, unpins the PMT, and delivers `Ok(())`.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(delivered(&mut rx), Ok(()));

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        timeout_task.await;
    }

    #[fuchsia::test]
    fn test_timeout_watchdog_monitors_new_slot_while_prior_slot_overdue() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let mut watchdog = pin!(processor.run_io_timeout_task());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), None);

        // 1. Schedule slot 0 and expire it: slot 0 logs, stays `Scheduled`, and re-arms its
        //    deadline by `command_timeout`.
        let mut rx0 = submit_tur(&processor, 0);
        let d0 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d0)));

        expire_deadline(&processor, 0);
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(d0));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert!(rx0.try_recv().unwrap().is_none());
        assert_eq!(processor.data_timeout_logs(), 1);
        let recheck_d0 = {
            let list = processor.request_list.lock();
            assert_eq!(list.scheduled_mask(), 0b01);
            list.armed_deadline().unwrap()
        };
        assert_eq!(
            fasync::TestExecutor::next_timer(),
            Some(fasync::MonotonicInstant::from_zx(recheck_d0))
        );

        // 2. While slot 0 remains overdue in `Scheduled`, shorten `timeout` and submit slot 1: the
        //    earlier deadline `d1 < recheck_d0` interrupts the watchdog timer to monitor slot 1.
        processor.set_timeout(zx::MonotonicDuration::from_millis(50));
        let mut rx1 = submit_tur(&processor, 0);
        let d1 = recheck_d0 - zx::MonotonicDuration::from_seconds(5);
        processor.request_list.lock().slot_mut(1).unwrap().deadline = Some(d1);
        processor.timeout_waker.wake();
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d1)));

        expire_deadline(&processor, 1);
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(d1));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(processor.data_timeout_logs(), 2);

        utp.complete(1, scsi_response(1, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx1), Ok(()));
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx0), Ok(()));

        let power = crate::testing::test_power(&processor);
        let mut lock_fut = pin!(power.lock());
        let Poll::Ready(mut guard) = exec.run_until_stalled(&mut lock_fut) else {
            panic!("uncontended PowerControl::lock must complete immediately");
        };
        let mut quiesce = pin!(guard.quiesce());
        assert_eq!(exec.run_until_stalled(&mut quiesce), Poll::Ready(Ok(())));
        assert_eq!(exec.run_until_stalled(&mut watchdog), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn test_reject_io_delivers_exact_status_without_recording_stats() {
        let (_mmio_vmo, processor) = create_test_processor();

        let (completion, mut rx) = io_callback();
        processor.reject_io(2, completion, zx::Status::ACCESS_DENIED);
        assert!(matches!(
            rx.try_recv(),
            Ok(Some(ScsiOutcome::Rejected(zx::Status::ACCESS_DENIED)))
        ));

        let (completion2, mut rx2) = io_callback();
        processor.reject_io(2, completion2, zx::Status::NOT_SUPPORTED);
        assert_eq!(delivered(&mut rx2), Err(zx::Status::NOT_SUPPORTED));

        assert_eq!(processor.io_stats.read_ops.load(Ordering::SeqCst), 0);
        assert_eq!(processor.io_stats.write_ops.load(Ordering::SeqCst), 0);
        assert_eq!(processor.io_stats.flush_ops.load(Ordering::SeqCst), 0);
        assert_eq!(processor.io_stats.trim_ops.load(Ordering::SeqCst), 0);
    }

    #[fuchsia::test]
    fn test_timeout_watchdog_stays_armed_across_idle_gap_and_rearms_on_overdue_slot() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let mut watchdog = pin!(processor.run_io_timeout_task());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert!(processor.request_list.lock().armed_deadline().is_none());
        assert_eq!(fasync::TestExecutor::next_timer(), None);

        // 1. Scheduling slot 0 sets `armed_deadline` and arms `Timer::new(d0)`.
        let mut rx0 = submit_tur(&processor, 0);
        let d0 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();
        assert_eq!(processor.request_list.lock().armed_deadline(), Some(d0));
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d0)));

        // 2. Completing slot 0 drains `scheduled_mask` to 0 while `armed_deadline` stays `Some`.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx0), Ok(()));
        {
            let list = processor.request_list.lock();
            assert_eq!(list.scheduled_mask(), 0);
            assert_eq!(list.armed_deadline(), Some(d0));
        }

        // 3. Submitting a new request with a later deadline while `armed_deadline == Some(d0)` does
        //    not wake or replace the existing `d0` timer.
        let mut rx1 = submit_tur(&processor, 0);
        let d1 = zx::MonotonicInstant::after(zx::MonotonicDuration::from_hours(1))
            .max(d0 + zx::MonotonicDuration::from_seconds(1));
        processor.request_list.lock().slot_mut(0).unwrap().deadline = Some(d1);
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d0)));

        // 4. Advancing fake time to `d0` wakes the watchdog; slot 0 is not yet past `d1`, so the
        //    watchdog leaves it unlogged and re-arms at `d1`.
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(d0));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(processor.data_timeout_logs(), 0);
        assert_eq!(processor.request_list.lock().armed_deadline(), Some(d1));
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d1)));

        // 5. Expiring `d1` logs the overdue slot and re-arms for the next `command_timeout` window.
        expire_deadline(&processor, 0);
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(d1));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(processor.data_timeout_logs(), 1);
        let retry_deadline = processor.request_list.lock().armed_deadline().unwrap();
        assert_eq!(
            fasync::TestExecutor::next_timer(),
            Some(fasync::MonotonicInstant::from_zx(retry_deadline))
        );

        // Completing slot 0 and firing that timer disarms the idle watchdog cleanly.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx1), Ok(()));
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(retry_deadline));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert!(processor.request_list.lock().armed_deadline().is_none());
        assert_eq!(fasync::TestExecutor::next_timer(), None);

        let power = crate::testing::test_power(&processor);
        let mut lock_fut = pin!(power.lock());
        let Poll::Ready(mut guard) = exec.run_until_stalled(&mut lock_fut) else {
            panic!("uncontended PowerControl::lock must complete immediately");
        };
        let mut quiesce = pin!(guard.quiesce());
        assert_eq!(exec.run_until_stalled(&mut quiesce), Poll::Ready(Ok(())));
        assert_eq!(exec.run_until_stalled(&mut watchdog), Poll::Ready(()));
    }

    #[fuchsia::test]
    async fn test_close_submissions_and_drain_inflight() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let mut inflight_rx = submit_tur(&processor, 0);
        assert_eq!(processor.busy_slot_count(), 1);

        processor.close_submissions();

        // New sync and async submissions are rejected immediately while in-flight slots drain.
        let (completion, mut rejected_rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        processor.submit_io(0, cdb, None, completion);
        assert_eq!(delivered(&mut rejected_rx), Err(zx::Status::IO));
        assert_eq!(processor.send_nop_out().await, Err(UfsError::IoError));

        let hardware = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        });
        assert_eq!(processor.drain_inflight(zx::MonotonicDuration::from_seconds(1)).await, Ok(()));
        hardware.join().unwrap();
        assert_eq!(processor.busy_slot_count(), 0);
        assert_eq!(delivered(&mut inflight_rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_quiesce_quarantines_pmt_and_delivers_io_error_when_disable_fails() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // Set `HCE = 1` and freeze `HCE` so `disable_host_controller()` times out.
        processor.controller().with_mmio(|mmio| {
            HostControllerEnableReg::from_raw(1).write(mmio);
            mmio.set_hce_stuck(true);
        });
        processor.controller().set_hce_timeout(zx::MonotonicDuration::from_millis(5));

        let mut async_req = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(async_req.as_mut()).is_pending());

        let (completion, mut sync_rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            completion,
        );
        assert!(slot_pinned(&processor, 0) && slot_pinned(&processor, 1));

        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Err(UfsError::Timeout));
        assert_eq!(async_req.await, Err(UfsError::IoError));
        assert_eq!(delivered(&mut sync_rx), Err(zx::Status::IO));
        assert_eq!(processor.quarantined_pmt_count(), 2);
        assert!(processor.request_list.lock().io_buffer().is_pinned());
        assert!(processor.request_list.lock().slot(0).unwrap().command_descriptor_io.is_pinned());
    }

    #[fuchsia::test]
    async fn test_stop_for_power_down_timeout_preserves_quiesce_quarantine() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // Set `HCE = 1` and freeze `HCE` so `stop_for_power_down()` times out on `HCE = 0`.
        processor.controller().with_mmio(|mmio| {
            HostControllerEnableReg::from_raw(1).write(mmio);
            mmio.set_hce_stuck(true);
        });
        processor.controller().set_hce_timeout(zx::MonotonicDuration::from_millis(5));

        let mut async_req = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(async_req.as_mut()).is_pending());

        let (completion, mut sync_rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            completion,
        );
        assert!(slot_pinned(&processor, 0) && slot_pinned(&processor, 1));

        // `stop_for_power_down()` fails with `Timeout`, marks register access as `OffUnconfirmed`,
        // sweeps and quarantines both in-flight slot PMTs, and keeps descriptor DMA buffers pinned.
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;
        assert_eq!(guard.stop_for_power_down().await, Err(UfsError::Timeout));
        assert_eq!(guard.register_access(), RegisterAccess::OffUnconfirmed);
        assert_eq!(async_req.await, Err(UfsError::IoError));
        assert_eq!(delivered(&mut sync_rx), Err(zx::Status::IO));
        assert_eq!(processor.quarantined_pmt_count(), 2);
        assert_eq!(processor.busy_slot_count(), 0);
        assert!(!slot_pinned(&processor, 0) && !slot_pinned(&processor, 1));
        assert!(processor.request_list.lock().io_buffer().is_pinned());

        // Repeated `stop_for_power_down()` while powered off preserves the latched `Timeout`.
        assert_eq!(guard.stop_for_power_down().await, Err(UfsError::Timeout));

        // Unstick `HCE` and set `UTRLRSR = 1` in the backing VMO so any MMIO access by
        // `quiesce()` would be observable; `quiesce()` must not touch MMIO while off, yet must
        // still return `Timeout` and keep every descriptor buffer pinned.
        processor.controller().with_mmio_unchecked(|mmio| mmio.set_hce_stuck(false));
        UtrListRunStopReg::from_raw(1).write(&mut utp.regs);

        assert_eq!(guard.quiesce().await, Err(UfsError::Timeout));
        assert!(UtrListRunStopReg::read(&utp.regs).value());
        assert!(HostControllerEnableReg::read(&utp.regs).host_controller_enable());
        {
            let list = processor.request_list.lock();
            assert!(list.io_buffer().is_pinned());
            for slot in 0..SDB_SLOTS as u8 {
                assert!(list.slot(slot).unwrap().command_descriptor_io.is_pinned());
            }
        }
    }

    #[fuchsia::test]
    async fn test_stop_for_power_down_clears_cancelled_admin_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        processor.controller().with_mmio(|mmio| {
            HostControllerEnableReg::from_raw(1).write(mmio);
        });

        // Cancel an in-flight admin command after its doorbell was rung so the admin slot stays
        // `Scheduled` without an `AdminSlotGuard`.
        let mut cancelled = Box::pin(processor.send_nop_out());
        assert!(poll!(cancelled.as_mut()).is_pending());
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Scheduled);
        assert_ne!(utp.doorbell() & (1 << ADMIN_SLOT), 0);
        drop(cancelled);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Scheduled);
        assert!(processor.request_list.lock().armed_deadline().is_some());

        // `stop_for_power_down()` disables `HCE`, disarms `armed_deadline`, and sweeps the admin
        // slot back to `Free` while keeping UTRL/UCD descriptor buffers pinned for
        // `reinit_after_power_down()`.
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;
        assert_eq!(guard.stop_for_power_down().await, Ok(()));
        assert_eq!(guard.register_access(), RegisterAccess::Off);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        assert_eq!(processor.busy_slot_count(), 0);
        {
            let list = processor.request_list.lock();
            assert!(list.armed_deadline().is_none());
            assert!(list.io_buffer().is_pinned());
            assert!(list.slot(ADMIN_SLOT).unwrap().command_descriptor_io.is_pinned());
        }

        // Emulate hardware `HCE = 0` clearing `UTRLDBR`, then re-enable and reinitialize.
        utp.clear_doorbell(ADMIN_SLOT);
        assert_eq!(processor.controller().reset_and_enable().await, Ok(()));
        assert_eq!(guard.register_access(), RegisterAccess::Reachable);
        assert_eq!(guard.reinit_after_power_down(), Ok(()));

        let hardware = thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        });
        assert_eq!(processor.send_nop_out().await, Ok(()));
        hardware.join().unwrap();
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    fn test_stop_for_power_down_disarms_watchdog_timer_and_rearms_after_reinit() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        processor.controller().with_mmio(|mmio| {
            HostControllerEnableReg::from_raw(1).write(mmio);
        });
        let mut watchdog = pin!(processor.run_io_timeout_task());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), None);

        let mut rx0 = submit_tur(&processor, 0);
        let d0 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d0)));

        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut lock_fut = pin!(power.lock());
        let Poll::Ready(mut guard) = exec.run_until_stalled(&mut lock_fut) else {
            panic!("uncontended PowerControl::lock must complete immediately");
        };
        {
            let mut stop_pd = pin!(guard.stop_for_power_down());
            assert_eq!(exec.run_until_stalled(&mut stop_pd), Poll::Ready(Ok(())));
        }
        assert_eq!(delivered(&mut rx0), Err(zx::Status::IO));
        assert!(processor.request_list.lock().armed_deadline().is_none());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), None);

        utp.clear_doorbell(0);
        let mut reset = pin!(processor.controller().reset_and_enable());
        assert_eq!(exec.run_until_stalled(&mut reset), Poll::Ready(Ok(())));
        assert_eq!(guard.reinit_after_power_down(), Ok(()));

        let mut rx1 = submit_tur(&processor, 0);
        assert!(processor.request_list.lock().armed_deadline().is_some());
        let d1 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fasync::TestExecutor::next_timer(), Some(fasync::MonotonicInstant::from_zx(d1)));

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx1), Ok(()));
        let mut quiesce = pin!(guard.quiesce());
        assert_eq!(exec.run_until_stalled(&mut quiesce), Poll::Ready(Ok(())));
        assert_eq!(exec.run_until_stalled(&mut watchdog), Poll::Ready(()));
    }

    #[fuchsia::test]
    async fn test_overdue_data_slot_never_aborts_and_logs_on_each_timeout_window() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        utp.regs.store32(HostUicErrorCodePhyAdapterLayerReg::OFFSET, 0x8000_0001);
        utp.regs.store32(HostUicErrorCodeDataLinkLayerReg::OFFSET, 0x8000_0002);

        let (completion, mut rx) = io_callback();
        processor.submit_io(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            completion,
        );

        // First expiry captures the read-to-clear UIC error registers once while keeping slot 0
        // scheduled and its DMA buffer pinned.
        expire_deadline(&processor, 0);
        processor.handle_expired_slots().await;
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        assert_eq!(utp.doorbell(), 1);
        assert_eq!(processor.controller().with_mmio(|m| m.uic_error_reads()), 5);
        assert_eq!(utp.regs.load32(HostUicErrorCodePhyAdapterLayerReg::OFFSET), 0);
        assert_eq!(processor.controller().with_mmio(|m| m.utrlclr_writes()), 0);
        assert!(rx.try_recv().unwrap().is_none());
        assert_eq!(processor.data_timeout_logs(), 1);

        // Subsequent overdue passes log and capture `UECPA..UECDME` again on each timeout window
        // without aborting the slot or unpinning DMA.
        for expected_logs in 2..=5 {
            expire_deadline(&processor, 0);
            processor.handle_expired_slots().await;
            assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
            assert!(slot_pinned(&processor, 0));
            assert_eq!(utp.doorbell(), 1);
            assert_eq!(
                processor.controller().with_mmio(|m| m.uic_error_reads()),
                expected_logs as u32 * 5
            );
            assert_eq!(processor.controller().with_mmio(|m| m.utrlclr_writes()), 0);
            assert!(rx.try_recv().unwrap().is_none());
            assert_eq!(processor.data_timeout_logs(), expected_logs);
        }

        utp.complete(0, scsi_response(0, 0).as_bytes(), OverallCommandStatus::Aborted as u8);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
    }

    #[fuchsia::test]
    fn test_handle_expired_slots_counts_only_overdue_missed_interrupts() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let inspector = Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.controller().record_inspect(&ufs_node);
        inspector.root().record(ufs_node);

        // Slot 0 expires after 50 ms; slot 1 stays before its 60 s deadline during the same sweep.
        processor.set_timeout(zx::MonotonicDuration::from_millis(50));
        let mut rx0 = submit_tur(&processor, 0);
        let d0 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();

        processor.set_timeout(zx::MonotonicDuration::from_seconds(60));
        let mut rx1 = submit_tur(&processor, 0);
        let d1 = processor.request_list.lock().slot(1).unwrap().deadline.unwrap();
        assert!(d1 > d0);

        // Emulate hardware completing both slots without delivering `service_interrupt()`.
        utp.write_completion(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        utp.write_completion(1, scsi_response(1, 0).as_bytes(), OCS_SUCCESS);

        exec.set_fake_time(fasync::MonotonicInstant::from_zx(d0));
        let mut poll_fut = pin!(processor.handle_expired_slots());
        assert_eq!(exec.run_until_stalled(&mut poll_fut), Poll::Ready(()));

        assert_eq!(delivered(&mut rx0), Ok(()));
        assert_eq!(delivered(&mut rx1), Ok(()));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert_eq!(slot_state(&processor, 1), SlotState::Free);
        assert_eq!(processor.controller().missed_interrupt_count(), 1);
        assert_eq!(processor.data_timeout_logs(), 0);
        assert_eq!(processor.controller().with_mmio(|m| m.uic_error_reads()), 0);

        let mut inspect_fut = pin!(async {
            assert_data_tree!(inspector, root: {
                ufs: contains {
                    missed_interrupt_count: 1u64,
                }
            });
        });
        assert_eq!(exec.run_until_stalled(&mut inspect_fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    async fn test_multiple_overdue_slots_share_uic_snapshot_and_forced_stop_quarantines_pmt() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        // Set `HCE = 1` and freeze `HCE` so the final `quiesce()` has to quarantine.
        processor.controller().with_mmio(|m| {
            HostControllerEnableReg::from_raw(1).write(m);
            m.set_hce_stuck(true);
        });
        processor.controller().set_hce_timeout(zx::MonotonicDuration::from_millis(2));
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());

        let (c0, mut rx0) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_read_10_cdb(0, 1),
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
            c0,
        );
        let (c1, mut rx1) = io_callback();
        processor.submit_io(
            0,
            ScsiOpcode::build_write_10_cdb(0, 1),
            Some(DataTransfer::write(Arc::clone(&data_vmo), 0, 4096)),
            c1,
        );

        // Expiring multiple slots in one pass reads the read-to-clear UIC error registers only
        // once so the second slot's log line does not see zeroed registers.
        expire_deadline(&processor, 0);
        expire_deadline(&processor, 1);
        processor.handle_expired_slots().await;
        assert_eq!(processor.controller().with_mmio(|m| m.uic_error_reads()), 5);
        assert_eq!(processor.data_timeout_logs(), 2);

        // Since `HCE` fails to clear, sweeping overdue slots quarantines their PMTs rather than
        // unpinning live DMA buffers.
        let power = crate::testing::test_power(&processor);
        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Err(UfsError::Timeout));
        assert_eq!(delivered(&mut rx0), Err(zx::Status::IO));
        assert_eq!(delivered(&mut rx1), Err(zx::Status::IO));
        assert_eq!(processor.quarantined_pmt_count(), 2);
        assert!(!slot_pinned(&processor, 0) && !slot_pinned(&processor, 1));
    }

    #[fuchsia::test]
    async fn test_admin_slot_timeout_quarantines_slot_until_doorbell_clears_or_quiesced() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let inspector = Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.record_inspect(&ufs_node);
        inspector.root().record(ufs_node);
        processor.controller().with_mmio(|m| {
            HostControllerEnableReg::from_raw(1).write(m);
        });

        // Expiring the admin slot while `UTRLDBR[31]` is set never writes `UTRLCLR`, returns
        // `Err(UfsError::Timeout)` to the caller, and keeps the admin slot in `SlotState::Timeout`
        // so subsequent admin reservations fail fast until `UTRLDBR[31]` clears or `HCE == 0`.
        let mut nop1 = Box::pin(processor.send_nop_out());
        assert!(poll!(nop1.as_mut()).is_pending());
        expire_deadline(&processor, ADMIN_SLOT);
        processor.handle_expired_slots().await;
        assert_eq!(nop1.await, Err(UfsError::Timeout));
        assert_eq!(processor.controller().with_mmio(|m| m.utrlclr_writes()), 0);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Timeout);
        assert_eq!(processor.data_timeout_logs(), 0);
        assert_data_tree!(inspector, root: {
            ufs: contains {
                timeout_mask: 1u64 << ADMIN_SLOT,
            }
        });
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let guard = power.lock().await;
        assert_eq!(processor.send_nop_out().await, Err(UfsError::Timeout));
        assert!(guard.is_admin_slot_timed_out());
        drop(guard);
        assert_eq!(processor.drain_inflight(zx::MonotonicDuration::from_millis(2)).await, Ok(()));

        // Once hardware clears `UTRLDBR[31]`, the next admin command reclaims the admin slot.
        utp.clear_doorbell(ADMIN_SLOT);
        let hardware = thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
            utp
        });
        assert_eq!(processor.send_nop_out().await, Ok(()));
        let mut utp = hardware.join().unwrap();

        // Also verify that a second caller parked in `reserve_admin_slot()` behind an earlier
        // cancelled admin future wakes with `Err(UfsError::Timeout)` if the stuck slot enters
        // `SlotState::Timeout`, and `quiesce()` (`HCE == 0`) sweeps it back to `Free`.
        HostControllerEnableReg::from_raw(1).write(&mut utp.regs);
        UtrListRunStopReg::from_raw(1).write(&mut utp.regs);
        let mut dropped_nop = Box::pin(processor.send_nop_out());
        assert!(poll!(dropped_nop.as_mut()).is_pending());
        drop(dropped_nop);

        let mut waiter_nop = Box::pin(processor.send_nop_out());
        assert!(poll!(waiter_nop.as_mut()).is_pending());
        expire_deadline(&processor, ADMIN_SLOT);
        processor.handle_expired_slots().await;
        assert_eq!(waiter_nop.await, Err(UfsError::Timeout));
        assert_eq!(processor.controller().with_mmio(|m| m.utrlclr_writes()), 0);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Timeout);

        let mut guard = power.lock().await;
        assert_eq!(guard.quiesce().await, Ok(()));
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_update_power_preserves_state_and_wakes_on_admission_open() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut guard = power.lock().await;

        let paused = guard.update_power(|cur| DevicePowerState { io_paused: true, ..cur });
        assert_eq!(
            paused,
            DevicePowerState { mode: PowerState::Active, io_paused: false, needs_reset: false }
        );
        assert_eq!(
            processor.requests().power(),
            DevicePowerState { mode: PowerState::Active, io_paused: true, needs_reset: false }
        );

        let submitted = Arc::new(AtomicBool::new(false));
        let blocked = {
            let processor = Arc::clone(&processor);
            let submitted = Arc::clone(&submitted);
            thread::spawn(move || {
                let rx = submit_tur(&processor, 0);
                submitted.store(true, Ordering::SeqCst);
                rx
            })
        };
        assert!(wait_until(|| processor.requests().blocked_on_admission() == 1));

        // Transitioning while keeping admission closed preserves the parked waiter.
        let before_sleep = guard.update_power(|cur| DevicePowerState {
            mode: PowerState::SystemSleep,
            io_paused: true,
            needs_reset: cur.needs_reset,
        });
        assert_eq!(before_sleep.mode, PowerState::Active);
        assert!(before_sleep.io_paused);
        thread::sleep(Duration::from_millis(20));
        assert!(!submitted.load(Ordering::SeqCst));
        assert_eq!(processor.requests().blocked_on_admission(), 1);
        assert_eq!(utp.doorbell(), 0);

        // Opening admission from closed wakes the parked waiter and preserves `needs_reset: false`.
        let before_open = guard.update_power(|cur| {
            assert_eq!(cur.mode, PowerState::SystemSleep);
            assert!(cur.io_paused);
            DevicePowerState {
                mode: PowerState::Active,
                io_paused: false,
                needs_reset: cur.needs_reset,
            }
        });
        assert_eq!(before_open.mode, PowerState::SystemSleep);
        let mut rx = blocked.join().unwrap();
        assert!(submitted.load(Ordering::SeqCst));
        let slot = wait_for_any_doorbell(&utp);
        utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
    }
}
