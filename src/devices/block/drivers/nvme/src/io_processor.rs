// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronous `Mutex + Condvar` I/O submission processor, dedicated `zx::Interrupt::wait`
//! completion thread, runtime `CSTS.CFS`/surprise-removal watchdog, and Drop-safe unblocking.

use crate::commands::Submission;
use crate::namespace::{IoDispatcher, IoOperation, Namespace, WORKER_SCHEDULER_ROLE};
use crate::queue_pair::{
    CompletedTransfer, CompletionEvent, DataDirection, DataTransfer, QueuePair,
};
use crate::registers::{
    CapabilityReg, InterruptMaskClearReg, InterruptMaskSetReg, NvmeRegisterBlock,
    ShutdownNotification,
};
use block_server::RequestId;
use block_server::callback_interface::SessionManager;
use fidl_next::Client;
use fidl_next_fuchsia_hardware_pci::{self as fpci, InterruptMode};
use fuchsia_async::{self as fasync, Timer};
use fuchsia_inspect::{NumericProperty, UintProperty};
use fuchsia_sync::{Condvar, Mutex};
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::select_biased;
use futures::task::AtomicWaker;
use log::{debug, error, info, trace, warn};
use mmio::Mmio;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::task::Poll;
use std::thread::{Builder, JoinHandle};
use zx::{BootDuration, BootInstant, Interrupt, Status};

/// Minimum controller ready/shutdown timeout floor in milliseconds (`500 ms`).
pub const MIN_CONTROLLER_TIMEOUT_MS: u64 = 500;

/// Polling interval while waiting for `CSTS.RDY` or `CSTS.SHST` transitions (`1 ms`).
pub const RESET_POLL_INTERVAL: BootDuration = BootDuration::from_millis(1);

/// Watchdog interval in [`run_io_watchdog`] to detect `CSTS.CFS` or PCIe surprise removal while
/// I/O is active (`50 ms`, capping idle-to-active executor wakeups at 20 Hz).
const IO_WATCHDOG_INTERVAL: BootDuration = BootDuration::from_millis(50);

/// Maps a `fidl_next` transport error to a [`zx::Status`].
pub fn map_fidl_err(err: fidl_next::Error) -> Status {
    match err {
        fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed) => Status::PEER_CLOSED,
        _ => Status::INTERNAL,
    }
}

/// Enables or disables PCI bus mastering on `pci`.
pub async fn set_bus_mastering(pci: &Client<fpci::Device>, enabled: bool) -> Result<(), Status> {
    pci.set_bus_mastering(enabled).await.map_err(map_fidl_err)??;
    Ok(())
}

/// Waits up to `CAP.TO` (at least [`MIN_CONTROLLER_TIMEOUT_MS`]) for `CSTS.RDY` to reach
/// `desired_ready_state`.
pub async fn wait_for_reset<M: Mmio>(
    desired_ready_state: bool,
    caps_reg: CapabilityReg,
    regs: &Mutex<NvmeRegisterBlock<M>>,
) -> Result<(), Status> {
    let timeout_ms = caps_reg.controller_timeout_ms().max(MIN_CONTROLLER_TIMEOUT_MS);
    let timeout = BootDuration::from_millis(timeout_ms as i64);
    let start = BootInstant::get();
    let deadline = start + timeout;
    loop {
        let csts = regs.lock().csts().read();
        if csts.is_removed() {
            error!(
                "Controller surprise-removed (CSTS={:#x}) while waiting for ready state {desired_ready_state}",
                csts.0
            );
            return Err(Status::IO_NOT_PRESENT);
        }

        // Per NVMe Base Spec 2.0 section 3.1.3.6, CSTS.CFS clears alongside CSTS.RDY -> 0 during
        // controller reset; only abort early on CFS when waiting for the controller to become
        // ready.
        if desired_ready_state && csts.controller_fatal_status() {
            error!("Controller reported fatal status (CSTS.CFS=1) while waiting for ready state");
            return Err(Status::IO);
        }
        if csts.ready() == desired_ready_state {
            let elapsed_ms = (BootInstant::get() - start).into_millis();
            debug!("Controller reached ready state {desired_ready_state} (took {elapsed_ms} ms)");
            return Ok(());
        }
        if BootInstant::get() >= deadline {
            error!(
                "Timed out after {timeout_ms} ms waiting for controller ready state {desired_ready_state} (CSTS={:#x})",
                csts.0
            );
            return Err(Status::TIMED_OUT);
        }
        Timer::new(BootInstant::after(RESET_POLL_INTERVAL)).await;
    }
}

/// Metadata tracked per active hardware command in [`IoProcessor`].
struct InFlightIo {
    namespace_id: u32,
    request_id: RequestId,
    submitted_at: BootInstant,
    logged_overdue: bool,
}

/// Completion item moved out of [`IoProcessorState`] so `Pmt::unpin` / cache invalidation and
/// `SessionManager::complete_request` execute without holding the queue lock.
pub struct CompletedIo {
    namespace_id: u32,
    request_id: RequestId,
    outcome: Result<(), Status>,
    cleanup: Option<CompletedTransfer>,
}

impl CompletedIo {
    /// Unpins and cache-invalidates any completed DMA transfer before returning request metadata.
    pub fn take(self) -> (u32, RequestId, Result<(), Status>) {
        let Self { namespace_id, request_id, outcome, cleanup } = self;
        drop(cleanup);
        (namespace_id, request_id, outcome)
    }
}

struct IoProcessorState<M: Mmio> {
    io_queue: Option<QueuePair<M>>,
    in_flight: Vec<Option<InFlightIo>>,
    closed: bool,
    halted_status: Option<Status>,
    halting_dma: bool,
    watchdog_bme_disabled: bool,
}

impl<M: Mmio> IoProcessorState<M> {
    fn in_flight_count(&self) -> usize {
        self.io_queue.as_ref().map_or(0, QueuePair::in_flight_count)
    }

    fn take_all_in_flight(&mut self) -> Vec<(u32, RequestId)> {
        let mut out = Vec::new();
        for slot in &mut self.in_flight {
            if let Some(entry) = slot.take() {
                out.push((entry.namespace_id, entry.request_id));
            }
        }
        out
    }
}

type NamespaceEntry = (u32, Weak<SessionManager<Namespace>>);

/// Synchronous `Mutex + Condvar` I/O submission and interrupt completion processor shared between
/// per-session [`Namespace`] worker threads and the interrupt/watchdog tasks.
pub struct IoProcessor<M: Mmio> {
    namespaces: OnceLock<Box<[NamespaceEntry]>>,
    pending_namespaces: Mutex<Vec<NamespaceEntry>>,
    state: Mutex<IoProcessorState<M>>,
    slot_available: Condvar,
    watchdog_waker: AtomicWaker,
    watchdog_kicked: AtomicBool,
    irq_waker: AtomicWaker,
    drain_waker: AtomicWaker,
    command_timeout_ns: AtomicI64,
    irq_failed: AtomicBool,
}

impl<M: Mmio> IoProcessor<M> {
    /// Creates an [`IoProcessor`] using `command_timeout` as the initial overdue-command threshold.
    pub fn new(command_timeout: BootDuration) -> Arc<Self> {
        Arc::new(Self {
            namespaces: OnceLock::new(),
            pending_namespaces: Mutex::new(Vec::new()),
            state: Mutex::new(IoProcessorState {
                io_queue: None,
                in_flight: Vec::new(),
                closed: false,
                halted_status: None,
                halting_dma: false,
                watchdog_bme_disabled: false,
            }),
            slot_available: Condvar::new(),
            watchdog_waker: AtomicWaker::new(),
            watchdog_kicked: AtomicBool::new(false),
            irq_waker: AtomicWaker::new(),
            drain_waker: AtomicWaker::new(),
            command_timeout_ns: AtomicI64::new(command_timeout.into_nanos()),
            irq_failed: AtomicBool::new(false),
        })
    }

    /// Updates the overdue-command diagnostic threshold.
    pub fn set_command_timeout(&self, timeout: BootDuration) {
        self.command_timeout_ns.store(timeout.into_nanos(), Ordering::Relaxed);
    }

    /// Freezes the registered namespace table and installs `io_queue` before IRQ or FIDL serving
    /// begins. Returns the queue's maximum in-flight command capacity.
    pub fn install_queue(&self, io_queue: QueuePair<M>) -> usize {
        {
            let mut pending = self.pending_namespaces.lock();
            let registered = std::mem::take(&mut *pending).into_boxed_slice();
            assert!(self.namespaces.set(registered).is_ok(), "install_queue called more than once");
        }
        let capacity = io_queue.capacity();
        let mut guard = self.state.lock();
        guard.in_flight = (0..capacity).map(|_| None).collect();
        guard.io_queue = Some(io_queue);
        capacity
    }

    fn prepare_submission(
        namespace_id: u32,
        operation: &IoOperation,
    ) -> (Submission, Option<DataTransfer<'_>>) {
        match operation {
            IoOperation::Flush => (Submission::nvm_flush(namespace_id), None),
            IoOperation::Read { start_lba, block_count, vmo, vmo_offset, bytes } => (
                Submission::nvm_read(namespace_id, *start_lba, *block_count),
                Some(DataTransfer::vmo(vmo, *vmo_offset, *bytes, DataDirection::FromDevice)),
            ),
            IoOperation::Write {
                start_lba,
                block_count,
                vmo,
                vmo_offset,
                bytes,
                force_unit_access,
            } => (
                Submission::nvm_write(namespace_id, *start_lba, *block_count, *force_unit_access),
                Some(DataTransfer::vmo(vmo, *vmo_offset, *bytes, DataDirection::ToDevice)),
            ),
        }
    }

    fn lookup_session_manager(&self, namespace_id: u32) -> Option<Arc<SessionManager<Namespace>>> {
        if let Some(entries) = self.namespaces.get() {
            return entries
                .iter()
                .find_map(|(id, sm)| if *id == namespace_id { sm.upgrade() } else { None });
        }
        let guard = self.pending_namespaces.lock();
        guard.iter().find_map(|(id, sm)| if *id == namespace_id { sm.upgrade() } else { None })
    }

    fn complete_request(
        &self,
        namespace_id: u32,
        request_id: RequestId,
        status: Result<(), Status>,
    ) {
        if let Some(sm) = self.lookup_session_manager(namespace_id) {
            sm.complete_request(request_id, status);
        }
    }

    /// Fails every `(namespace_id, request_id)` in `items` with `status`.
    #[inline(never)]
    pub fn complete_failed_batch(&self, items: Vec<(u32, RequestId)>, status: Status) {
        for (namespace_id, request_id) in items {
            self.complete_request(namespace_id, request_id, Err(status));
        }
    }

    /// Drains all ready CQEs into `completed`, rings `CQHDBL`, wakes slot/drain waiters, and runs
    /// DMA unpin + request completion outside the queue lock.
    pub fn drain_completions(&self, completed: &mut Vec<CompletedIo>) {
        completed.clear();
        let mut untracked_cleanups: Vec<CompletedTransfer> = Vec::new();
        let drained_any = {
            let mut guard = self.state.lock();
            let state = &mut *guard;
            let Some(io_queue) = state.io_queue.as_mut() else {
                return;
            };
            let in_flight = &mut state.in_flight;
            let mut ring_doorbell = false;

            while let Some(event) = io_queue.check_for_new_completion() {
                ring_doorbell = true;
                match event {
                    CompletionEvent::Unexpected { completion } => {
                        error!(
                            "Received unexpected IO completion for cid={}",
                            completion.dw3.command_id()
                        );
                    }
                    CompletionEvent::Completed { cid, completion, cleanup } => {
                        let outcome = if completion.is_success() {
                            trace!("Completed IO transaction cid={cid} OK");
                            Ok(())
                        } else {
                            warn!(
                                "Completed IO transaction cid={cid} with error: status_type={:#x}, status={:#x}",
                                u8::from(completion.dw3.status_code_type()),
                                completion.dw3.status_code()
                            );
                            Err(Status::IO)
                        };
                        match in_flight.get_mut(usize::from(cid)).and_then(Option::take) {
                            Some(entry) => completed.push(CompletedIo {
                                namespace_id: entry.namespace_id,
                                request_id: entry.request_id,
                                outcome,
                                cleanup,
                            }),
                            None => {
                                error!("Completed IO transaction cid={cid} has no tracked request");
                                if let Some(transfer) = cleanup {
                                    untracked_cleanups.push(transfer);
                                }
                            }
                        }
                    }
                }
            }

            if ring_doorbell {
                io_queue.ring_completion_db();
                self.slot_available.notify_all();
                if io_queue.in_flight_count() == 0 && !state.halting_dma {
                    self.drain_waker.wake();
                }
            }
            ring_doorbell
        };

        drop(untracked_cleanups);
        if !drained_any {
            return;
        }

        for item in completed.drain(..) {
            let (namespace_id, request_id, outcome) = item.take();
            self.complete_request(namespace_id, request_id, outcome);
        }
    }

    /// Records that the I/O interrupt thread failed and wakes the watchdog immediately.
    pub fn fail_irq(&self) {
        self.irq_failed.store(true, Ordering::Release);
        self.irq_waker.wake();
        self.watchdog_waker.wake();
    }

    fn has_irq_failed(&self) -> bool {
        self.irq_failed.load(Ordering::Acquire)
    }

    fn take_irq_failed(&self) -> bool {
        self.irq_failed.swap(false, Ordering::AcqRel)
    }

    /// Returns `true` if the processor has latched a fatal/removal halt status.
    pub fn is_halted(&self) -> bool {
        self.state.lock().halted_status.is_some()
    }

    fn kick_watchdog(&self) {
        if !self.watchdog_kicked.swap(true, Ordering::SeqCst) {
            self.watchdog_waker.wake();
        }
    }

    fn has_watchdog_kick(&self) -> bool {
        self.watchdog_kicked.load(Ordering::SeqCst)
    }

    fn set_watchdog_kick(&self) {
        self.watchdog_kicked.store(true, Ordering::SeqCst);
    }

    fn clear_watchdog_kick(&self) {
        self.watchdog_kicked.store(false, Ordering::SeqCst);
    }

    /// Returns `true` if the processor is healthy and currently has in-flight hardware commands.
    pub fn should_arm_watchdog(&self) -> bool {
        let guard = self.state.lock();
        guard.halted_status.is_none() && guard.in_flight_count() > 0
    }

    fn is_drained_or_halted(&self) -> bool {
        let guard = self.state.lock();
        !guard.halting_dma && (guard.halted_status.is_some() || guard.in_flight_count() == 0)
    }

    fn is_cleanly_drained(&self) -> bool {
        let guard = self.state.lock();
        guard.halted_status.is_none() && guard.in_flight_count() == 0
    }

    /// Returns `true` if [`Self::halt_on_fatal_or_removed`] already disabled PCI bus mastering.
    pub fn watchdog_bme_disabled(&self) -> bool {
        self.state.lock().watchdog_bme_disabled
    }

    /// Returns the installed I/O queue ID if healthy and completely drained.
    pub fn drained_queue_id_if_healthy(&self) -> Option<u16> {
        let guard = self.state.lock();
        if guard.halted_status.is_none()
            && let Some(io_queue) = guard.io_queue.as_ref()
            && io_queue.in_flight_count() == 0
        {
            Some(io_queue.queue_id())
        } else {
            None
        }
    }

    /// Removes the installed I/O queue and any remaining in-flight request IDs during teardown.
    pub fn take_for_teardown(&self) -> (Option<QueuePair<M>>, Vec<(u32, RequestId)>) {
        let mut guard = self.state.lock();
        (guard.io_queue.take(), guard.take_all_in_flight())
    }

    /// Marks the processor closed to new submissions and wakes any session threads parked on
    /// `slot_available`.
    pub fn close_submissions(&self) {
        let mut guard = self.state.lock();
        guard.closed = true;
        self.slot_available.notify_all();
        self.drain_waker.wake();
    }

    /// Failsafe invoked by [`IoProcessorDropGuard`] when the driver is dropped without awaiting
    /// graceful `stop()`.
    ///
    /// Wakes any session threads parked in `submit_io` and fails any uncompleted request tokens
    /// (unblocking internal barrier/mapper waiters) WITHOUT unpinning `io_queue`, so live DMA
    /// mappings remain safely quarantined on the BTI when the controller drops.
    pub fn abort_for_unclean_drop(&self) {
        let pending = {
            let mut guard = self.state.lock();
            guard.closed = true;
            guard.halting_dma = false;
            let pending = guard.take_all_in_flight();
            self.slot_available.notify_all();
            self.drain_waker.wake();
            pending
        };
        self.complete_failed_batch(pending, Status::PEER_CLOSED);
    }

    /// Waits until all in-flight I/O commands complete, the processor halts, or `deadline` passes.
    /// Returns `true` if healthy and cleanly drained.
    pub async fn wait_until_drained_or_halted(&self, deadline: BootInstant) -> bool {
        if self.is_drained_or_halted() {
            return self.is_cleanly_drained();
        }
        if BootInstant::get() >= deadline {
            return self.is_cleanly_drained();
        }

        let mut wait_drained = pin!(
            poll_fn(|cx| {
                self.kick_watchdog();
                self.drain_waker.register(cx.waker());
                if self.is_drained_or_halted() { Poll::Ready(()) } else { Poll::Pending }
            })
            .fuse()
        );
        let mut timeout_fut = pin!(Timer::new(deadline).fuse());

        select_biased! {
            _ = wait_drained => {}
            _ = timeout_fut => {
                warn!("Timed out waiting for IO queue to drain in-flight commands before teardown");
            }
        }
        self.is_cleanly_drained()
    }

    fn log_overdue_commands(&self, now: BootInstant) {
        let timeout = BootDuration::from_nanos(self.command_timeout_ns.load(Ordering::Relaxed));
        let mut guard = self.state.lock();
        for (cid, slot) in guard.in_flight.iter_mut().enumerate() {
            if let Some(entry) = slot.as_mut()
                && !entry.logged_overdue
                && now - entry.submitted_at >= timeout
            {
                entry.logged_overdue = true;
                warn!(
                    "IO command cid={cid} on namespace {} has been in flight for {} ms without completion",
                    entry.namespace_id,
                    (now - entry.submitted_at).into_millis()
                );
            }
        }
    }

    /// Halts DMA (`CC.EN = 0` or `set_bus_mastering(false)`) and fails all in-flight I/O requests
    /// upon `CSTS.CFS`, surprise removal, or interrupt failure.
    pub async fn halt_on_fatal_or_removed(
        &self,
        removed: bool,
        regs: &Mutex<NvmeRegisterBlock<M>>,
        pci: &Client<fpci::Device>,
        caps_reg: CapabilityReg,
    ) {
        let mut completed = Vec::new();
        self.drain_completions(&mut completed);

        let fail_status = if removed { Status::IO_NOT_PRESENT } else { Status::IO };
        let should_halt_dma = {
            let mut guard = self.state.lock();
            if guard.halted_status.is_none() {
                guard.halted_status = Some(fail_status);
            }
            self.clear_watchdog_kick();
            self.slot_available.notify_all();
            if guard.halting_dma {
                false
            } else if guard.in_flight_count() == 0 {
                self.drain_waker.wake();
                false
            } else {
                guard.halting_dma = true;
                true
            }
        };
        if !should_halt_dma {
            return;
        }

        let mut rdy_cleared = false;
        if !removed {
            let mut cc_written = false;
            {
                let mut regs_guard = regs.lock();
                let cur_csts = regs_guard.csts().read();
                if !cur_csts.is_removed() {
                    let mut cc = regs_guard.cc().read();
                    if cc.enabled() || cur_csts.ready() {
                        cc.set_shutdown_notification(ShutdownNotification::None);
                        cc.set_enabled(false);
                        regs_guard.cc_mut().write(cc);
                        cc_written = true;
                    } else {
                        rdy_cleared = true;
                    }
                }
            }
            if cc_written && wait_for_reset(false, caps_reg, regs).await.is_ok() {
                let after_csts = regs.lock().csts().read();
                if !after_csts.is_removed() && !after_csts.ready() {
                    rdy_cleared = true;
                }
            }
        }

        let bme_disabled_now = !rdy_cleared && set_bus_mastering(pci, false).await.is_ok();

        let (failed_items, status_to_report, quarantined_queue) = {
            let mut guard = self.state.lock();
            guard.halting_dma = false;
            if bme_disabled_now {
                guard.watchdog_bme_disabled = true;
            }
            let dma_halted = rdy_cleared || guard.watchdog_bme_disabled;
            let items = guard.take_all_in_flight();

            if dma_halted {
                if let Some(io_queue) = guard.io_queue.as_mut() {
                    // SAFETY: `guard.halted_status` was set before halting DMA, preventing any new
                    // submissions to `io_queue`, and hardware DMA has ceased because either `CC.EN`
                    // was cleared and `CSTS.RDY == 0` was observed on a present controller
                    // (`rdy_cleared`), or PCI Bus Master Enable was cleared via
                    // `set_bus_mastering(false)` (`guard.watchdog_bme_disabled`).
                    let _: usize = unsafe { io_queue.cancel_all() };
                }
                self.drain_waker.wake();
                (items, fail_status, None)
            } else {
                error!(
                    "Controller failed to halt DMA after fatal/removal status; quarantining IO queue DMA buffers"
                );

                // Remove `QueuePair` without calling `cancel_all` or `unpin`: dropping it outside
                // the lock closes all `Pmt` handles into Zircon BTI quarantine rather than
                // unpinning active IOMMU mappings.
                let quarantined = guard.io_queue.take();
                self.drain_waker.wake();
                (items, Status::PEER_CLOSED, quarantined)
            }
        };

        drop(quarantined_queue);
        self.complete_failed_batch(failed_items, status_to_report);
    }
}

impl<M: Mmio + Send + 'static> IoDispatcher for IoProcessor<M> {
    fn register_namespace(
        &self,
        namespace_id: u32,
        session_manager: Weak<SessionManager<Namespace>>,
    ) {
        let mut pending = self.pending_namespaces.lock();
        assert!(
            self.namespaces.get().is_none(),
            "register_namespace called after install_queue froze namespaces"
        );
        pending.push((namespace_id, session_manager));
    }

    fn submit_io(&self, namespace_id: u32, operation: IoOperation, request_id: RequestId) {
        let (submission, transfer) = Self::prepare_submission(namespace_id, &operation);
        let mut guard = self.state.lock();

        loop {
            if let Some(status) = guard.halted_status {
                drop(guard);
                self.complete_request(namespace_id, request_id, Err(status));
                return;
            }
            if guard.closed || guard.io_queue.is_none() {
                drop(guard);
                self.complete_request(namespace_id, request_id, Err(Status::PEER_CLOSED));
                return;
            }

            let submit_result = guard
                .io_queue
                .as_mut()
                .expect("checked io_queue is Some")
                .submit(submission, transfer);

            match submit_result {
                Ok(cid) => {
                    let was_idle = guard
                        .io_queue
                        .as_ref()
                        .expect("checked io_queue is Some")
                        .in_flight_count()
                        == 1;
                    let prev = guard.in_flight[usize::from(cid)].replace(InFlightIo {
                        namespace_id,
                        request_id,
                        submitted_at: BootInstant::get(),
                        logged_overdue: false,
                    });
                    assert!(prev.is_none(), "duplicate in-flight CID {cid}");
                    // Latch `watchdog_kicked` before releasing `self.state` so `0 -> 1` and
                    // `watchdog_kicked = true` occur atomically with respect to `state` readers,
                    // while deferring any `zx_port_queue` wakeup until after unlocking.
                    let wake_watchdog =
                        was_idle && !self.watchdog_kicked.swap(true, Ordering::SeqCst);
                    drop(guard);
                    if wake_watchdog {
                        self.watchdog_waker.wake();
                    }
                    return;
                }
                Err(Status::SHOULD_WAIT) => {
                    if guard.in_flight_count() == 0 {
                        error!(
                            "IO submission queue reported full with zero commands in flight; failing IO request"
                        );
                        drop(guard);
                        self.complete_request(namespace_id, request_id, Err(Status::IO));
                        return;
                    }
                    self.slot_available.wait(&mut guard);
                }
                Err(status) => {
                    error!("Failed to submit IO transaction: {status}");
                    let reply_status = if status == Status::INVALID_ARGS {
                        Status::INVALID_ARGS
                    } else {
                        Status::INTERNAL
                    };
                    drop(guard);
                    self.complete_request(namespace_id, request_id, Err(reply_status));
                    return;
                }
            }
        }
    }
}

/// Drop guard placed BEFORE `Scope` and controller fields on the driver so session/mapper threads
/// can never remain parked in [`IoProcessor::submit_io`] or internal waits if the driver is dropped
/// without awaiting `stop()`.
pub struct IoProcessorDropGuard<M: Mmio> {
    io_processor: Arc<IoProcessor<M>>,
}

impl<M: Mmio> IoProcessorDropGuard<M> {
    /// Creates a drop guard bound to `io_processor`.
    pub fn new(io_processor: Arc<IoProcessor<M>>) -> Self {
        Self { io_processor }
    }
}

impl<M: Mmio> Drop for IoProcessorDropGuard<M> {
    fn drop(&mut self) {
        self.io_processor.abort_for_unclean_drop();
    }
}

/// State shared with the dedicated I/O interrupt thread.
pub struct IoIrqContext<M: Mmio> {
    pub io_processor: Arc<IoProcessor<M>>,
    pub regs: Arc<Mutex<NvmeRegisterBlock<M>>>,
    pub irq_mode: InterruptMode,
    pub io_irq_vector: u32,
    pub interrupt_count: Arc<UintProperty>,
    pub queue_capacity: usize,
}

/// Dedicated OS thread that waits on the I/O Completion Queue interrupt with
/// `zx::Interrupt::wait` and drains completions directly, without hopping through the async
/// executor.
pub struct IoInterruptThread {
    /// Duplicate of the interrupt handle, destroyed to unblock `zx::Interrupt::wait` with
    /// `Status::CANCELED`.
    irq: Option<Interrupt>,
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl IoInterruptThread {
    /// Spawns the `"nvme-irq"` OS thread servicing `irq`.
    pub fn spawn<M: Mmio + Send + 'static>(
        irq: Interrupt,
        context: IoIrqContext<M>,
    ) -> Result<Self, Status> {
        if context.irq_mode != InterruptMode::MsiX && context.io_irq_vector >= u32::BITS {
            return Err(Status::INVALID_ARGS);
        }
        let irq_for_stop = irq.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_requested);
        let thread = Builder::new()
            .name("nvme-irq".to_string())
            .spawn(move || run_io_irq_loop(&irq, &context, &worker_stop))
            .map_err(|err| {
                error!("Failed to spawn NVMe IO IRQ thread: {err}");
                Status::NO_RESOURCES
            })?;
        Ok(Self { irq: Some(irq_for_stop), stop_requested, thread: Some(thread) })
    }

    /// Destroys the interrupt so a blocked `wait()` returns `Status::CANCELED`, and returns the
    /// thread join handle.
    fn take_and_destroy(&mut self) -> Option<JoinHandle<()>> {
        let thread = self.thread.take()?;
        self.stop_requested.store(true, Ordering::Release);
        if let Some(irq) = self.irq.take()
            && let Err(status) = irq.destroy()
        {
            warn!("Failed to destroy NVMe IO interrupt on stop: {status}");
        }
        Some(thread)
    }

    /// Stops the interrupt thread and joins it off the async executor.
    pub async fn stop(mut self) {
        if let Some(thread) = self.take_and_destroy() {
            fasync::unblock(move || join_irq_thread(thread)).await;
        }
    }
}

impl Drop for IoInterruptThread {
    fn drop(&mut self) {
        if let Some(thread) = self.take_and_destroy() {
            join_irq_thread(thread);
        }
    }
}

fn join_irq_thread(thread: JoinHandle<()>) {
    if thread.join().is_err() {
        error!("NVMe IO IRQ thread panicked");
    }
}

/// Body of the dedicated I/O interrupt thread.
fn run_io_irq_loop<M: Mmio>(
    irq: &Interrupt,
    context: &IoIrqContext<M>,
    stop_requested: &AtomicBool,
) {
    if let Err(err) = fuchsia_scheduler::set_role_for_this_thread(WORKER_SCHEDULER_ROLE) {
        warn!("Failed to set NVMe IO IRQ thread role: {err:?}");
    }
    let IoIrqContext {
        io_processor,
        regs,
        irq_mode,
        io_irq_vector,
        interrupt_count,
        queue_capacity,
    } = context;
    let mask_vector = *irq_mode != InterruptMode::MsiX;
    let mut completed = Vec::with_capacity(*queue_capacity);
    loop {
        if let Err(status) = irq.wait() {
            if status == Status::CANCELED || stop_requested.load(Ordering::Acquire) {
                info!("Interrupt cancelled, exiting NVMe IO IRQ thread");
            } else {
                error!("Failed to wait for interrupt: {status}");
                io_processor.fail_irq();
            }
            return;
        }
        if stop_requested.load(Ordering::Acquire) {
            return;
        }
        interrupt_count.add(1);
        if mask_vector && !io_processor.is_halted() {
            regs.lock().intms_mut().write(InterruptMaskSetReg::for_vector(*io_irq_vector));
        }
        io_processor.drain_completions(&mut completed);
        if mask_vector && !io_processor.is_halted() {
            regs.lock().intmc_mut().write(InterruptMaskClearReg::for_vector(*io_irq_vector));
        }
    }
}

/// Parameters passed to [`run_io_watchdog`].
pub struct IoWatchdogConfig<M: Mmio> {
    pub io_processor: Arc<IoProcessor<M>>,
    pub regs: Arc<Mutex<NvmeRegisterBlock<M>>>,
    pub pci: Client<fpci::Device>,
    pub caps_reg: CapabilityReg,
}

/// Periodically checks for `CSTS.CFS`, PCIe surprise removal, and overdue commands while I/O is
/// active, and halts the controller immediately if the I/O interrupt thread fails.
pub async fn run_io_watchdog<M: Mmio + Send + 'static>(
    config: IoWatchdogConfig<M>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let IoWatchdogConfig { io_processor, regs, pci, caps_reg } = config;
    let mut shutdown_rx = pin!(shutdown_rx.fuse());
    let mut next_watchdog: Option<BootInstant> = None;

    loop {
        if io_processor.take_irq_failed() {
            next_watchdog = None;
            io_processor.clear_watchdog_kick();
            io_processor.halt_on_fatal_or_removed(false, &regs, &pci, caps_reg).await;
        }

        if next_watchdog.is_none()
            && !io_processor.is_halted()
            && (io_processor.has_watchdog_kick() || io_processor.should_arm_watchdog())
        {
            io_processor.set_watchdog_kick();
            next_watchdog = Some(BootInstant::after(IO_WATCHDOG_INTERVAL));
        }

        let current_deadline = next_watchdog;
        let fired_timer = {
            let mut wait_step = pin!(
                async {
                    if let Some(deadline) = current_deadline {
                        let mut timer = pin!(Timer::new(deadline).fuse());
                        let mut irq_err = pin!(
                            poll_fn(|cx| {
                                io_processor.irq_waker.register(cx.waker());
                                if io_processor.has_irq_failed() {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            })
                            .fuse()
                        );
                        select_biased! {
                            _ = irq_err => false,
                            _ = timer => true,
                        }
                    } else {
                        poll_fn(|cx| {
                            io_processor.watchdog_waker.register(cx.waker());
                            if (!io_processor.is_halted()
                                && (io_processor.has_watchdog_kick()
                                    || io_processor.should_arm_watchdog()))
                                || io_processor.has_irq_failed()
                            {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                        false
                    }
                }
                .fuse()
            );

            select_biased! {
                _ = shutdown_rx => {
                    info!("Shutting down NVMe IO watchdog");
                    break;
                }
                fired_timer = wait_step => fired_timer,
            }
        };

        if fired_timer {
            next_watchdog = None;
            io_processor.clear_watchdog_kick();
            if io_processor.should_arm_watchdog() {
                let csts = regs.lock().csts().read();
                if csts.is_removed() {
                    error!("Controller surprise-removed (CSTS={:#x}) during active IO", csts.0);
                    io_processor.halt_on_fatal_or_removed(true, &regs, &pci, caps_reg).await;
                } else if csts.controller_fatal_status() {
                    error!(
                        "Controller reported fatal status (CSTS={:#x}) during active IO",
                        csts.0
                    );
                    io_processor.halt_on_fatal_or_removed(false, &regs, &pci, caps_reg).await;
                } else {
                    io_processor.log_overdue_commands(BootInstant::get());
                    io_processor.set_watchdog_kick();
                    next_watchdog = Some(BootInstant::after(IO_WATCHDOG_INTERVAL));
                }
            }
        } else if !io_processor.has_irq_failed() && !io_processor.is_halted() {
            // Woke from idle (`next_watchdog` was `None`): keep `watchdog_kicked == true` across
            // one full `IO_WATCHDOG_INTERVAL` even if a fast QD=1 request completed before
            // `"fuchsia-async"` polled `run_io_watchdog`, avoiding `watchdog_waker.wake()` calls
            // from `submit_io` during that 50 ms window and capping wakeups at 20 Hz.
            io_processor.set_watchdog_kick();
            next_watchdog = Some(BootInstant::after(IO_WATCHDOG_INTERVAL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{
        Completion, GenericStatus, IdentifyNamespace, LbaFormat, StatusCodeType,
    };
    use crate::namespace::{ControllerLimits, NamespaceServer};
    use crate::registers::{NVME_REG_CSTS, NVME_REG_DOORBELL_BASE};
    use block_client::{BlockClient, BufferSlice, RemoteBlockClient};
    use diagnostics_assertions::assert_data_tree;
    use fake_bti::FakeBti;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_storage_block::BlockMarker;
    use fidl_next::{Request, Responder};
    use fuchsia_async::Scope;
    use fuchsia_inspect::Inspector;
    use futures::task::{ArcWake, noop_waker_ref, waker_ref};
    use mmio::MmioSplit;
    use mmio::region::MmioRegion;
    use mmio::vmo::{VmoMapping, VmoMemory};
    use std::future::Future;
    use std::mem::size_of;
    use std::sync::atomic::AtomicUsize;
    use std::task::Context;
    use zerocopy::transmute;
    use zx::sys::zx_paddr_t;
    use zx::{NullableHandle, Rights, VirtualInterrupt, Vmo, system_get_page_size};

    type TestMmio = MmioRegion<VmoMemory, Arc<VmoMemory>>;

    struct MinimalPciServer {
        bme_disabled: Arc<AtomicBool>,
    }

    impl fpci::DeviceServerHandler for MinimalPciServer {
        async fn get_device_info(&mut self, responder: Responder<fpci::device::GetDeviceInfo>) {
            let info = fpci::DeviceInfo {
                vendor_id: 0,
                device_id: 0,
                base_class: 0,
                sub_class: 0,
                program_interface: 0,
                revision_id: 0,
                bus_id: 0,
                dev_id: 0,
                func_id: 0,
                padding: (),
            };
            let _: Result<(), _> = responder.respond(&info).await;
        }
        async fn get_bar(
            &mut self,
            _req: Request<fpci::device::GetBar>,
            responder: Responder<fpci::device::GetBar>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn set_bus_mastering(
            &mut self,
            req: Request<fpci::device::SetBusMastering>,
            responder: Responder<fpci::device::SetBusMastering>,
        ) {
            if !req.payload().enabled {
                self.bme_disabled.store(true, Ordering::Release);
            }
            let _: Result<(), _> = responder.respond(()).await;
        }
        async fn reset_device(&mut self, responder: Responder<fpci::device::ResetDevice>) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn map_interrupt(
            &mut self,
            _req: Request<fpci::device::MapInterrupt>,
            responder: Responder<fpci::device::MapInterrupt>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn get_interrupt_modes(
            &mut self,
            responder: Responder<fpci::device::GetInterruptModes>,
        ) {
            let _: Result<(), _> = responder
                .respond(&fpci::InterruptModes { has_legacy: true, msi_count: 0, msix_count: 0 })
                .await;
        }
        async fn set_interrupt_mode(
            &mut self,
            _req: Request<fpci::device::SetInterruptMode>,
            responder: Responder<fpci::device::SetInterruptMode>,
        ) {
            let _: Result<(), _> = responder.respond(()).await;
        }
        async fn read_config8(
            &mut self,
            _req: Request<fpci::device::ReadConfig8>,
            responder: Responder<fpci::device::ReadConfig8>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn read_config16(
            &mut self,
            _req: Request<fpci::device::ReadConfig16>,
            responder: Responder<fpci::device::ReadConfig16>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn read_config32(
            &mut self,
            _req: Request<fpci::device::ReadConfig32>,
            responder: Responder<fpci::device::ReadConfig32>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn write_config8(
            &mut self,
            _req: Request<fpci::device::WriteConfig8>,
            responder: Responder<fpci::device::WriteConfig8>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn write_config16(
            &mut self,
            _req: Request<fpci::device::WriteConfig16>,
            responder: Responder<fpci::device::WriteConfig16>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn write_config32(
            &mut self,
            _req: Request<fpci::device::WriteConfig32>,
            responder: Responder<fpci::device::WriteConfig32>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
        async fn get_capabilities(
            &mut self,
            _req: Request<fpci::device::GetCapabilities>,
            responder: Responder<fpci::device::GetCapabilities>,
        ) {
            let _: Result<(), _> = responder.respond(&Vec::<u8>::new()).await;
        }
        async fn get_extended_capabilities(
            &mut self,
            _req: Request<fpci::device::GetExtendedCapabilities>,
            responder: Responder<fpci::device::GetExtendedCapabilities>,
        ) {
            let _: Result<(), _> = responder.respond(&Vec::<u16>::new()).await;
        }
        async fn get_bti(
            &mut self,
            _req: Request<fpci::device::GetBti>,
            responder: Responder<fpci::device::GetBti>,
        ) {
            let _: Result<(), _> = responder.respond_err(Status::NOT_SUPPORTED).await;
        }
    }

    fn make_namespace_identify() -> IdentifyNamespace {
        let mut ns =
            IdentifyNamespace { nsze: 64, ncap: 64, nuse: 64, ..IdentifyNamespace::default() };
        ns.lba_formats[0] = LbaFormat::from_parts(9, 0, 0);
        ns
    }

    struct TestProcessorSetup {
        bti: FakeBti,
        processor: Arc<IoProcessor<TestMmio>>,
        ns_server: Arc<NamespaceServer>,
        regs: Arc<Mutex<NvmeRegisterBlock<TestMmio>>>,
        bar_vmo: Vmo,
        cq_phys: usize,
        capacity: usize,
    }

    fn setup_processor(entries: u32) -> TestProcessorSetup {
        let page = system_get_page_size() as usize;
        let bti = FakeBti::create().expect("create fake bti");
        let paddrs: Vec<zx_paddr_t> = (1..256).map(|i| i * 16 * page).collect();
        bti.set_paddrs(&paddrs);

        let bar_vmo = Vmo::create(0x2000).expect("bar vmo");
        let bar_dup = bar_vmo.duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        let bar_policy = bar_dup.info().expect("bar vmo info").cache_policy();
        let mut mmio = VmoMapping::map_with_cache_policy(0, 0x2000, bar_dup, bar_policy)
            .expect("map bar")
            .into_split_send();
        let ctrl_mmio = mmio.try_split_off(NVME_REG_DOORBELL_BASE).expect("split ctrl");
        let io_db = mmio.try_split_off(0x100).expect("split db");
        let regs = Arc::new(Mutex::new(NvmeRegisterBlock::new(ctrl_mmio)));

        let processor = IoProcessor::new(BootDuration::from_seconds(10));
        let dispatcher: Arc<dyn IoDispatcher> = Arc::clone(&processor) as Arc<dyn IoDispatcher>;
        let ns_server = NamespaceServer::new(
            1,
            &make_namespace_identify(),
            &ControllerLimits::default(),
            dispatcher,
            None,
        )
        .expect("create namespace");

        let bti_handle = bti.duplicate_handle(Rights::SAME_RIGHTS).expect("dup bti");
        let io_queue =
            QueuePair::new(&bti_handle, 1, entries, io_db, 4, false).expect("create io queue");
        let cq_phys = io_queue.completion_queue().device_address();
        let capacity = processor.install_queue(io_queue);
        TestProcessorSetup { bti, processor, ns_server, regs, bar_vmo, cq_phys, capacity }
    }

    fn write_cqe(bti: &FakeBti, cq_phys: usize, slot: usize, cid: u16, phase: bool) {
        let pinned = bti.get_pinned_vmo();
        let (cq_vmo, range) = pinned.find_phys(cq_phys).expect("find cq vmo");
        let dup = cq_vmo.duplicate_handle(Rights::SAME_RIGHTS).expect("dup cq vmo");
        let policy = dup.info().expect("cq vmo info").cache_policy();
        let mut mapping = VmoMapping::map_with_cache_policy(
            usize::try_from(range.start).unwrap(),
            system_get_page_size() as usize,
            dup,
            policy,
        )
        .expect("map cq");

        let mut cqe = Completion::default();
        cqe.dw2.set_sq_id(1);
        cqe.dw3.set_command_id(cid);
        cqe.dw3.set_phase(phase);
        cqe.dw3.set_status_code_type(StatusCodeType::Generic);
        cqe.dw3.set_status_code(GenericStatus::Success as u8);
        let dwords: [u32; size_of::<Completion>() / size_of::<u32>()] = transmute!(cqe);
        let offset = slot * size_of::<Completion>();
        for (i, &dw) in dwords[..3].iter().enumerate() {
            mapping.store32(offset + i * 4, dw);
        }
        mapping.write_barrier();
        mapping.store32(offset + 12, dwords[3]);
        mapping.write_barrier();
    }

    #[fuchsia::test]
    async fn irq_thread_drains_completions_and_updates_interrupt_count() {
        let TestProcessorSetup { bti, processor, ns_server, regs, bar_vmo: _, cq_phys, capacity } =
            setup_processor(8);
        let bti = Arc::new(bti);
        let inspector = Inspector::default();
        let interrupt_count = Arc::new(inspector.root().create_uint("interrupt_count", 0));

        let virq = VirtualInterrupt::create_virtual().expect("create virtual irq");
        let irq_for_thread = Interrupt::from(NullableHandle::from(
            virq.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
        ));
        let irq_thread = IoInterruptThread::spawn(
            irq_for_thread,
            IoIrqContext {
                io_processor: Arc::clone(&processor),
                regs: Arc::clone(&regs),
                irq_mode: InterruptMode::Legacy,
                io_irq_vector: 0,
                interrupt_count,
                queue_capacity: capacity,
            },
        )
        .expect("spawn irq thread");

        let scope = Scope::new_with_name("io-processor-test");
        let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
        let ns_bg = Arc::clone(&ns_server);
        scope.spawn(async move {
            ns_bg.serve_block(stream).await;
        });

        let client = RemoteBlockClient::new(proxy).await.expect("block client");
        let write_fut = client.write_at(BufferSlice::Memory(&[0xa5u8; 512]), 0);
        let processor_bg = Arc::clone(&processor);
        let bti_bg = Arc::clone(&bti);
        let completer = Scope::current().compute(async move {
            while !processor_bg.should_arm_watchdog() {
                Timer::new(BootInstant::after(BootDuration::from_millis(1))).await;
            }
            write_cqe(&bti_bg, cq_phys, 0, 0, true);
            virq.trigger(BootInstant::get()).expect("trigger irq");
        });

        write_fut.await.expect("write completed by IRQ thread");
        completer.await;
        client.close().await.expect("close client");
        drop(client);
        scope.cancel().await;
        fasync::unblock(move || ns_server.terminate()).await;
        assert_data_tree!(inspector, root: {
            interrupt_count: 1u64,
        });
        irq_thread.stop().await;

        let (mut io_queue, remaining) = processor.take_for_teardown();
        assert!(remaining.is_empty());
        if let Some(ref mut q) = io_queue {
            // SAFETY: No hardware DMA is active in this unit test.
            unsafe {
                let _: usize = q.cancel_all();
                io_queue.take().unwrap().unpin();
            }
        }
        assert_eq!(bti.info().expect("bti info").pmo_count, 0);
    }

    #[fuchsia::test]
    async fn drop_guard_fails_in_flight_requests_without_unpinning_dma() {
        let TestProcessorSetup { bti, processor, ns_server, .. } = setup_processor(4);
        let guard = IoProcessorDropGuard::new(Arc::clone(&processor));

        let scope = Scope::new_with_name("drop-guard-test");
        let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
        let ns_bg = Arc::clone(&ns_server);
        scope.spawn(async move {
            ns_bg.serve_block(stream).await;
        });

        let client = RemoteBlockClient::new(proxy).await.expect("block client");
        let pinned_initial = bti.info().expect("bti info").pmo_count;
        let processor_wait = Arc::clone(&processor);
        let dropper = Scope::current().compute(async move {
            while !processor_wait.should_arm_watchdog() {
                Timer::new(BootInstant::after(BootDuration::from_millis(1))).await;
            }
            drop(guard);
        });

        let err = client.write_at(BufferSlice::Memory(&[0x5au8; 512]), 0).await.unwrap_err();
        assert_eq!(err, Status::PEER_CLOSED);
        dropper.await;

        // Data VMO and queue rings remain pinned/quarantined when aborted via
        // `IoProcessorDropGuard`.
        assert!(bti.info().expect("bti info").pmo_count > pinned_initial);
        let _ = client.close().await;
        drop(client);
        scope.cancel().await;
        fasync::unblock(move || ns_server.terminate()).await;
    }

    #[fuchsia::test]
    async fn watchdog_halts_immediately_on_irq_failure() {
        let TestProcessorSetup { bti, processor, ns_server, regs, bar_vmo, .. } =
            setup_processor(4);
        // Simulate stuck `CSTS.RDY = 1` so `halt_on_fatal_or_removed` falls back to disabling BME.
        bar_vmo.write(&1u32.to_le_bytes(), NVME_REG_CSTS as u64).expect("set CSTS.RDY=1");

        let bme_disabled = Arc::new(AtomicBool::new(false));
        let pci_scope = Scope::new_with_name("pci-test");
        let (pci_client, pci_server) = fidl_next::fuchsia::create_channel();
        pci_server
            .spawn_on(MinimalPciServer { bme_disabled: Arc::clone(&bme_disabled) }, &pci_scope);
        let pci = pci_client.spawn();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let watchdog = Scope::current().compute(run_io_watchdog(
            IoWatchdogConfig {
                io_processor: Arc::clone(&processor),
                regs: Arc::clone(&regs),
                pci,
                caps_reg: CapabilityReg(0),
            },
            shutdown_rx,
        ));

        let scope = Scope::new_with_name("watchdog-test");
        let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
        let ns_bg = Arc::clone(&ns_server);
        scope.spawn(async move {
            ns_bg.serve_block(stream).await;
        });

        let client = RemoteBlockClient::new(proxy).await.expect("block client");
        let processor_trigger = Arc::clone(&processor);
        let failer = Scope::current().compute(async move {
            while !processor_trigger.should_arm_watchdog() {
                Timer::new(BootInstant::after(BootDuration::from_millis(1))).await;
            }
            processor_trigger.fail_irq();
        });

        assert_eq!(client.flush().await, Err(Status::IO));
        failer.await;
        assert!(bme_disabled.load(Ordering::Acquire));
        assert!(processor.watchdog_bme_disabled());

        let _ = shutdown_tx.send(());
        watchdog.await;
        pci_scope.cancel().await;
        let _ = client.close().await;
        drop(client);
        scope.cancel().await;
        fasync::unblock(move || ns_server.terminate()).await;

        let (io_queue, _) = processor.take_for_teardown();
        if let Some(q) = io_queue {
            // SAFETY: Bus mastering was disabled and no hardware DMA is active.
            unsafe {
                q.unpin();
            }
        }
        assert_eq!(bti.info().expect("bti info").pmo_count, 0);
    }

    struct CountingWaker(AtomicUsize);

    impl ArcWake for CountingWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn complete_fast_qd1_flush(
        client: &RemoteBlockClient,
        processor: &Arc<IoProcessor<TestMmio>>,
        bti: &Arc<FakeBti>,
        cq_phys: usize,
        slot: usize,
        waker_count: &Arc<CountingWaker>,
        expect_wake: bool,
    ) {
        let flush_fut = client.flush();
        let processor_bg = Arc::clone(processor);
        let bti_bg = Arc::clone(bti);
        let waker_bg = Arc::clone(waker_count);
        let completer = Scope::current().compute(async move {
            let cid = loop {
                let in_flight_cid =
                    processor_bg.state.lock().in_flight.iter().position(Option::is_some);
                if let Some(cid) = in_flight_cid
                    && (!expect_wake || waker_bg.0.load(Ordering::SeqCst) == 1)
                {
                    break u16::try_from(cid).unwrap();
                }
                Timer::new(BootInstant::after(BootDuration::from_millis(1))).await;
            };
            write_cqe(&bti_bg, cq_phys, slot, cid, true);
            let mut completed = Vec::new();
            processor_bg.drain_completions(&mut completed);
        });
        flush_fut.await.expect("flush completed");
        completer.await;
    }

    #[fuchsia::test]
    async fn watchdog_throttles_wakeups_across_fast_qd1_completions() {
        let TestProcessorSetup { bti, processor, ns_server, regs, cq_phys, .. } =
            setup_processor(8);
        let bti = Arc::new(bti);

        let pci_scope = Scope::new_with_name("pci-qd1-test");
        let (pci_client, pci_server) = fidl_next::fuchsia::create_channel();
        pci_server.spawn_on(
            MinimalPciServer { bme_disabled: Arc::new(AtomicBool::new(false)) },
            &pci_scope,
        );
        let pci = pci_client.spawn();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut watchdog_fut = pin!(run_io_watchdog(
            IoWatchdogConfig {
                io_processor: Arc::clone(&processor),
                regs: Arc::clone(&regs),
                pci,
                caps_reg: CapabilityReg(0),
            },
            shutdown_rx,
        ));

        let scope = Scope::new_with_name("qd1-block-test");
        let (proxy, stream) = create_proxy_and_stream::<BlockMarker>();
        let ns_bg = Arc::clone(&ns_server);
        scope.spawn(async move {
            ns_bg.serve_block(stream).await;
        });
        let client = RemoteBlockClient::new(proxy).await.expect("block client");

        let waker_count = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let counting_waker = waker_ref(&waker_count);
        let mut counting_cx = Context::from_waker(&counting_waker);
        let mut noop_cx = Context::from_waker(noop_waker_ref());

        // 1. Park `run_io_watchdog` in its idle `poll_fn`.
        assert!(watchdog_fut.as_mut().poll(&mut counting_cx).is_pending());
        assert_eq!(waker_count.0.load(Ordering::SeqCst), 0);
        assert!(!processor.has_watchdog_kick());

        // 2. Submit (`0 -> 1`) and complete (`1 -> 0`) a fast QD=1 request before `run_io_watchdog`
        //    is polled again.
        complete_fast_qd1_flush(&client, &processor, &bti, cq_phys, 0, &waker_count, true).await;
        assert!(!processor.should_arm_watchdog());
        assert!(processor.has_watchdog_kick());
        assert_eq!(waker_count.0.load(Ordering::SeqCst), 1);

        // 3. Even though `in_flight` is already `0`, polling `run_io_watchdog` wakes from idle on
        //    `watchdog_kicked` and arms the 50 ms cooldown window with `watchdog_kicked == true`.
        assert!(watchdog_fut.as_mut().poll(&mut noop_cx).is_pending());
        assert!(processor.has_watchdog_kick());

        // 4. Subsequent QD=1 requests while `watchdog_kicked == true` do NOT call
        //    `watchdog_waker.wake()`.
        waker_count.0.store(0, Ordering::SeqCst);
        processor.watchdog_waker.register(&counting_waker);
        complete_fast_qd1_flush(&client, &processor, &bti, cq_phys, 1, &waker_count, false).await;
        assert_eq!(waker_count.0.load(Ordering::SeqCst), 0);

        // 5. Once the 50 ms timer expires with zero commands in flight, `watchdog_kicked` clears
        //    and `run_io_watchdog` parks back in its idle `poll_fn`.
        Timer::new(BootInstant::after(IO_WATCHDOG_INTERVAL + BootDuration::from_millis(5))).await;
        waker_count.0.store(0, Ordering::SeqCst);
        assert!(watchdog_fut.as_mut().poll(&mut counting_cx).is_pending());
        assert!(!processor.has_watchdog_kick());
        assert_eq!(waker_count.0.load(Ordering::SeqCst), 0);

        // 6. The next idle-to-active QD=1 request wakes `watchdog_waker` again.
        complete_fast_qd1_flush(&client, &processor, &bti, cq_phys, 2, &waker_count, true).await;
        assert_eq!(waker_count.0.load(Ordering::SeqCst), 1);

        let _ = shutdown_tx.send(());
        assert!(watchdog_fut.as_mut().poll(&mut noop_cx).is_ready());
        pci_scope.cancel().await;
        let _ = client.close().await;
        drop(client);
        scope.cancel().await;
        fasync::unblock(move || ns_server.terminate()).await;

        let (mut io_queue, remaining) = processor.take_for_teardown();
        assert!(remaining.is_empty());
        if let Some(ref mut q) = io_queue {
            // SAFETY: No hardware DMA is active in this unit test.
            unsafe {
                let _: usize = q.cancel_all();
                io_queue.take().unwrap().unpin();
            }
        }
        assert_eq!(bti.info().expect("bti info").pmo_count, 0);
    }
}
