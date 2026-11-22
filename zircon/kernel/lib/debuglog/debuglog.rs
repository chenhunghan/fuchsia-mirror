// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use debug::dprintf;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode};
use kprint::{kformat, kprint};
use ksync::{ExclusiveLockPolicy, KCell, KMutex, guarded};
use lazy_init::{LazyInit, NoCheck};
use libc::stdio::StringFile;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::{FromZeros, IntoBytes};
use zx_status::Status;

use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
use crate::io::serial_write;
use crate::kconcurrent::ChainLockTransaction;
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::event::{AutounsignalEvent, Event};
use crate::kernel::relaxed_atomic::RelaxedAtomicBool;
use crate::kernel::thread::{
    HIGH_PRIORITY, Interruptible, Thread, ThreadPtr, create_with_priority, current_get,
};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::ksync_kernel::{NoIrqSavePolicy, TrackedMonitoredSpinlock, TrackedMutex};
use crate::persistent_debuglog_rs::{
    persistent_dlog_enabled, persistent_dlog_init_early, persistent_dlog_write,
};
use crate::platform_rs::debug::{
    platform_dputs_thread, platform_serial_discard_writes_while_suspended, platform_serial_enabled,
    platform_serial_wait_for_resume,
};
use crate::platform_rs::timer::{DurationMono, InstantMono, current_boot_time, zx_msec, zx_sec};
use crate::top::boot_constants::{bypass_debuglog, kernel_debug_ident};

pub use debuglog_types::{dlog_header_t, dlog_record_t};
pub const DLOG_MAX_RECORD: usize = debuglog_types::DLOG_MAX_RECORD;
pub const DLOG_MAX_DATA: usize = debuglog_types::DLOG_MAX_DATA;

/// Severity Levels
pub const DEBUGLOG_TRACE: u32 = zx_types::DEBUGLOG_TRACE as u32;
pub const DEBUGLOG_DEBUG: u32 = zx_types::DEBUGLOG_DEBUG as u32;
pub const DEBUGLOG_INFO: u32 = zx_types::DEBUGLOG_INFO as u32;
pub const DEBUGLOG_WARNING: u32 = zx_types::DEBUGLOG_WARNING as u32;
pub const DEBUGLOG_ERROR: u32 = zx_types::DEBUGLOG_ERROR as u32;
pub const DEBUGLOG_FATAL: u32 = zx_types::DEBUGLOG_FATAL as u32;

zr::static_assert!(size_of::<dlog_record_t>() == DLOG_MAX_RECORD);

const DLOG_SIZE: usize = 128 * 1024;
const DLOG_MASK: usize = DLOG_SIZE - 1;

/// Make sure that when `dlog_header_t` is in the fifo, it's 4 byte aligned and that the first 4
/// bytes (the `preamble` field) don't wrap.
const DLOG_HEADER_FIFO_ALIGNMENT: usize = 4;

const fn dlog_align_trunc(n: usize) -> usize {
    n & !3
}

const fn dlog_align(n: usize) -> usize {
    dlog_align_trunc(n + 3)
}

const fn dlog_hdr_set(fifosize: usize, readsize: usize) -> u32 {
    (((readsize & 0xFFF) << 12) | (fifosize & 0xFFF)) as u32
}

const fn dlog_hdr_get_fifolen(n: u32) -> usize {
    (n & 0xFFF) as usize
}

const fn dlog_hdr_get_readlen(n: u32) -> usize {
    ((n >> 12) & 0xFFF) as usize
}

/// Internal preamble bit marking records written with `PersistLog::No` so
/// `DLog::persist_existing` skips them during early-boot replay.
const DLOG_PREAMBLE_SKIP_PERSIST: u32 = 1 << 24;
zr::static_assert!((dlog_hdr_set(0xFFF, 0xFFF) & DLOG_PREAMBLE_SKIP_PERSIST) == 0);

/// Assembly buffer size with room for a maximum-length log payload, formatted header, and
/// trailing newline.
const RENDERED_LOG_BUF_SIZE: usize = DLOG_MAX_DATA + 128;

/// Controls whether a debuglog record is also written to the persistent debuglog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistLog {
    /// Enqueue the record in the in-memory debuglog only (e.g. `gStdoutNoPersist`).
    No,
    /// Enqueue the record and persist it to the persistent debuglog if enabled.
    Yes,
}

// must be power of two
zr::static_assert!((DLOG_SIZE & DLOG_MASK) == 0);
// wat
zr::static_assert!(DLOG_MAX_RECORD <= DLOG_SIZE);
// E_DONT_DO_THAT
zr::static_assert!((DLOG_MAX_RECORD & 3) == 0);

/// Accessor to quickly determine if the debuglog bypass is enabled.
pub fn dlog_bypass() -> bool {
    bypass_debuglog()
}

#[repr(C, align(4))]
struct DlogData([u8; DLOG_SIZE]);

zr::static_assert!(align_of::<DlogData>() == DLOG_HEADER_FIFO_ALIGNMENT);

impl DlogData {
    fn new_zeroed() -> impl PinInit<Self> {
        // SAFETY: `DlogData` is a plain byte array (`[u8; DLOG_SIZE]`), so all-zero bytes is a
        // valid representation. Writing directly to `slot` avoids constructing 128 KB on the
        // stack.
        unsafe {
            pin_init::pin_init_from_closure(|slot: *mut Self| {
                slot.write_bytes(0, 1);
                Ok(())
            })
        }
    }
}

#[pin_data]
struct ThreadState {
    /// Stored as an `AtomicPtr` so `DLog::sync` can load the thread handle without holding a
    /// lock while `DLog::start_threads` and `DLog::shutdown` store to it through `&self`.
    thread: AtomicPtr<Thread>,
    shutdown_requested: AtomicBool,
    #[pin]
    event: AutounsignalEvent,
}

impl ThreadState {
    fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            thread: AtomicPtr::new(core::ptr::null_mut()),
            shutdown_requested: AtomicBool::new(false),
            event <- AutounsignalEvent::init(false),
        })
    }
}

/// A small struct which holds a representation of a debuglog record, including:
///
/// 1) A reconstructed header, contiguous in memory.
/// 2) Two byte slice regions representing the data portion of the record as
///    it exists in the ring buffer.
/// 3) A flag computed during `read_record` indicating whether or not the data
///    region of this record ends with a newline or not.
struct Record<'a> {
    hdr: dlog_header_t,
    region1: &'a [u8],
    region2: &'a [u8],
    ends_with_newline: bool,
}

impl Record<'_> {
    fn new() -> Self {
        Self {
            hdr: dlog_header_t::new_zeroed(),
            region1: &[],
            region2: &[],
            ends_with_newline: false,
        }
    }
}

/// The lifecycle state of this `DLog` object.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Running = 0,
    /// Shutdown has been called, but has not yet completed. No new messages can
    /// be queued. Calls to `write` will fail. Already-queued messages may
    /// continue to be processed/emitted.
    ShutdownStarted = 1,
    /// Shutdown has completed. `dlog_serial_write` will drop data.
    ShutdownFinished = 2,
}

const DLOG_NOTIFIER_THREAD_NAME: &core::ffi::CStr = c"debuglog-notifier";
const DLOG_DUMPER_THREAD_NAME: &core::ffi::CStr = c"debuglog-dumper";

#[cfg(ktest)]
type OutputHook = (unsafe fn(*const (), &[u8]), *const ());

#[cfg(ktest)]
#[derive(Clone, Copy)]
struct PersistHook {
    is_enabled: unsafe fn(*const ()) -> bool,
    persist: unsafe fn(*const (), &[u8]),
    ctx: *const (),
}

/// The debuglog maintains a circular buffer of debuglog records,
/// consisting of a common header (`dlog_header_t`) followed by up
/// to 224 bytes of textual log message. Records are aligned on
/// `u32` boundaries, so the header word which indicates the
/// true size of the record and the space it takes in the fifo
/// can always be read with a single `u32` read (the header
/// or body may wrap but the initial header word never does).
///
/// The ring buffer position is maintained by continuously incrementing
/// head and tail pointers (type `usize`, so `u64` on 64-bit systems).
///
/// This allows readers to trivially compute if their local tail
/// pointer has "fallen out" of the fifo (an entire fifo's worth
/// of messages were written since they last tried to read) and then
/// they can snap their tail to the global tail and restart.
///
/// Tail indicates the oldest message in the debuglog to read
/// from, Head indicates the next space in the debuglog to write
/// a new message to. They are clipped to the actual buffer by
/// `DLOG_MASK`.
///
/// ```text
///       T                     T
///  [....XXXX....]  [XX........XX]
///           H         H
/// ```
#[repr(C)]
#[guarded]
#[pin_data(PinnedDrop)]
pub struct DLog {
    #[pin]
    notifier_state: ThreadState,
    #[pin]
    dumper_state: ThreadState,

    /// Use `TrackedMonitoredSpinlock` to provide lockup detector diagnostics for the critical
    /// sections protected by this lock.
    #[mutex]
    lock: KMutex<TrackedMonitoredSpinlock>,
    #[mutex]
    readers_lock: KMutex<TrackedMutex>,

    #[guarded_by(lock)]
    head: usize,
    #[guarded_by(lock)]
    tail: usize,

    /// Used to signal an event from a "safe" context. `set_deadline` calls are serialized by
    /// `lock` while `lifecycle == Running`, and `cancel` is called in `shutdown` (after
    /// transitioning `lifecycle` and draining `lock`) and `drop`. See `write` for details.
    #[pin]
    deferred_signal_timer: Timer,

    /// Indicates there is a pending `deferred_signal_timer`. Used to reduce the
    /// number of unnecessary timers.
    pending_deferred_signal: RelaxedAtomicBool,

    /// Indicates that the system has begun to panic. When true, `write` will
    /// immediately return an error.
    ///
    /// Accessed from multiple threads without a lock (see `dlog_panic_started`).
    /// Relaxed ordering suffices since it does not publish any other data; readers
    /// only use it to decide how to emit output.
    panic: RelaxedAtomicBool,

    /// The lifecycle state of this `DLog` object.
    /// `shutdown` transitions `Running -> ShutdownStarted` with `AcqRel` and `ShutdownStarted ->
    /// ShutdownFinished` with `Release`, pairing with `Acquire` loads in `write` and
    /// `shutdown_finished`.
    lifecycle: AtomicU32,

    /// The list of our current readers.
    #[pin]
    #[guarded_by(readers_lock)]
    readers: DoublyLinkedList<*mut DlogReader>,

    /// A counter incremented for each log message that enters the debuglog.
    #[guarded_by(lock)]
    sequence_count: u64,

    /// Set on the global `DLOG` singleton during early boot so records written between
    /// `LK_INIT_LEVEL_HEAP` and `LK_INIT_LEVEL_HEAP + 1` are replayed in order by
    /// `persist_existing` instead of being persisted ahead of earlier records and duplicated.
    #[guarded_by(lock)]
    defer_persist_until_replay: bool,

    /// Signaled when shutdown has completed.
    #[pin]
    shutdown_finished: Event,

    /// The sequence number up to which the dumper thread has dumped.
    /// Updated with `Release` in `dumper_thread` and read with `Acquire` in `sync`.
    dumper_sequence: AtomicU64,

    /// Signaled when the dumper thread has drained all currently available records.
    #[pin]
    dumper_drained_event: AutounsignalEvent,

    /// This array contains `dlog_header_t` objects so make sure it's properly aligned.
    #[pin]
    #[guarded_by(lock)]
    data: DlogData,

    #[cfg(ktest)]
    output_hook: UnsafeCell<Option<OutputHook>>,
    #[cfg(ktest)]
    persist_hook: UnsafeCell<Option<PersistHook>>,

    #[pin]
    _pinned: PhantomData<PhantomPinned>,
}

// SAFETY: `DLog` synchronizes access to its raw pointer fields (`readers:
// KCell<DoublyLinkedList<*mut DlogReader>>` guarded by `readers_lock`, and `#[cfg(ktest)]`
// `output_hook` / `persist_hook` set before use) and mutable state via `lock`, `readers_lock`,
// and atomic variables.
unsafe impl Send for DLog {}
// SAFETY: `DLog` synchronizes shared access to its raw pointer fields (`readers:
// KCell<DoublyLinkedList<*mut DlogReader>>` guarded by `readers_lock`, and `#[cfg(ktest)]`
// `output_hook` / `persist_hook` read-only once initialized) and mutable state across threads via
// `lock`, `readers_lock`, and atomic variables.
unsafe impl Sync for DLog {}

#[pinned_drop]
impl PinnedDrop for DLog {
    fn drop(self: Pin<&mut Self>) {
        // Cancel any pending timer first to avoid a potential use-after-free.
        self.deferred_signal_timer.cancel();
        let this = self.project();
        // Threads must have been stopped (or never started) before `DLog` is destroyed.
        assert!(this.notifier_state.thread.load(Ordering::SeqCst).is_null());
        assert!(this.dumper_state.thread.load(Ordering::SeqCst).is_null());
    }
}

impl DLog {
    #[cfg(ktest)]
    fn new() -> impl PinInit<Self> {
        Self::new_with_defer_persist(false)
    }

    fn new_with_defer_persist(defer_persist_until_replay: bool) -> impl PinInit<Self> {
        pin_init!(Self {
            notifier_state <- ThreadState::new(),
            dumper_state <- ThreadState::new(),
            lock <- KMutex::init(),
            readers_lock <- KMutex::init(),
            head: KCell::new(0),
            tail: KCell::new(0),
            deferred_signal_timer <- Timer::init(ZX_CLOCK_MONOTONIC),
            pending_deferred_signal: RelaxedAtomicBool::new(false),
            panic: RelaxedAtomicBool::new(false),
            lifecycle: AtomicU32::new(Lifecycle::Running as u32),
            readers <- ksync::kcell_init(DoublyLinkedList::new()),
            sequence_count: KCell::new(0),
            defer_persist_until_replay: KCell::new(defer_persist_until_replay),
            shutdown_finished <- Event::init(false),
            dumper_sequence: AtomicU64::new(0),
            dumper_drained_event <- AutounsignalEvent::init(false),
            data <- ksync::kcell_init(DlogData::new_zeroed()),
            #[cfg(ktest)]
            output_hook: UnsafeCell::new(None),
            #[cfg(ktest)]
            persist_hook: UnsafeCell::new(None),
            _pinned: PhantomData,
        })
    }

    /// Starts the notifier and dumper threads for this `DLog`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self` remains valid and pinned until `shutdown` completes with
    /// `Ok(())` (or for the lifetime of the kernel for the static `DLOG`).
    unsafe fn start_threads(&self) {
        assert!(self.notifier_state.thread.load(Ordering::SeqCst).is_null());
        assert!(self.dumper_state.thread.load(Ordering::SeqCst).is_null());

        extern "C" fn notifier_thunk(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` was passed as a pointer to `self` (`DLog`), which outlives the
            // notifier thread by `start_threads`'s caller safety precondition.
            unsafe { (*arg.cast::<DLog>()).notifier_thread() }
        }
        // SAFETY: `notifier_thunk` is a valid thread entry function and `self` is valid for
        // the lifetime of the thread.
        if let Ok(thread) = unsafe {
            create_with_priority(
                DLOG_NOTIFIER_THREAD_NAME.as_ptr(),
                notifier_thunk,
                core::ptr::from_ref(self).cast_mut().cast::<c_void>(),
                HIGH_PRIORITY - 1,
            )
        } {
            self.notifier_state.thread.store(thread.as_raw(), Ordering::SeqCst);
            // SAFETY: The thread was just created and has not been resumed yet.
            unsafe { thread.resume() };
        }

        if platform_serial_enabled() {
            extern "C" fn dumper_thunk(arg: *mut c_void) -> i32 {
                // SAFETY: `arg` was passed as a pointer to `self` (`DLog`), which outlives the
                // dumper thread by `start_threads`'s caller safety precondition.
                unsafe { (*arg.cast::<DLog>()).dumper_thread() }
            }
            // SAFETY: `dumper_thunk` is a valid thread entry function and `self` is valid for
            // the lifetime of the thread.
            if let Ok(thread) = unsafe {
                create_with_priority(
                    DLOG_DUMPER_THREAD_NAME.as_ptr(),
                    dumper_thunk,
                    core::ptr::from_ref(self).cast_mut().cast::<c_void>(),
                    HIGH_PRIORITY - 2,
                )
            } {
                self.dumper_state.thread.store(thread.as_raw(), Ordering::SeqCst);
                // SAFETY: The thread was just created and has not been resumed yet.
                unsafe { thread.resume() };
            }
        }
    }

    /// Mark this `DLog` as shutting down, then shutdown all threads. Once called, subsequent
    /// `write` operations will fail, but already-queued messages will continue to be
    /// processed/emitted.
    fn shutdown(&self, deadline: InstantMono) -> Result<(), Status> {
        dprintf!(INFO, "Shutting down debuglog\n");

        // Are we the first to try to shutdown this instance? Try to claim the honor.
        // AcqRel synchronizes with the Acquire load in write(); Relaxed suffices on failure
        // because shutdown_finished.wait_deadline() provides synchronization.
        if self
            .lifecycle
            .compare_exchange(
                Lifecycle::Running as u32,
                Lifecycle::ShutdownStarted as u32,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // Nope. Either some other thread is shutting it down or it's already shutdown. Wait
            // for them to do the work on our behalf and just return whatever status they signal us
            // with.
            return self.shutdown_finished.wait_deadline(deadline, Interruptible::No);
        }

        // By changing the lifecycle state we have successfully claimed the responsibility of
        // shutting down this instance and have also stopped new records from being inserted into
        // the queue. The dumper thread will continue to read records and drain the queue even
        // after shutdown has started. If we don't stop the flow upstream, then a sufficiently
        // speedy write could prevent the dumper thread from terminating.

        // This closure will signal and join the thread referenced by `state`.
        let shutdown_thread = |state: &ThreadState, name: &core::ffi::CStr| -> Result<(), Status> {
            // Tell the thread that it's time to terminate.
            let already_requested = state.shutdown_requested.swap(true, Ordering::SeqCst);
            debug_assert!(!already_requested);

            state.event.signal();
            let thread_raw = state.thread.load(Ordering::SeqCst);
            // SAFETY: `state.thread` stores a valid `Thread` pointer until joined here.
            if let Some(thread) = unsafe { ThreadPtr::from_raw(thread_raw) } {
                // SAFETY: We have exclusive ownership of joining this thread during shutdown.
                let res = unsafe { thread.join(deadline) };
                if let Err(status) = res {
                    dprintf!(
                        INFO,
                        "Failed to join {} thread: {}\n",
                        name.to_str().unwrap_or(""),
                        status.into_raw()
                    );
                    return Err(status);
                }
                state.thread.store(core::ptr::null_mut(), Ordering::SeqCst);
            }
            Ok(())
        };

        // Shutdown the notifier thread first. Ordering is important because the
        // notifier thread is responsible for passing log records to the dumper.
        let notifier_status = shutdown_thread(&self.notifier_state, DLOG_NOTIFIER_THREAD_NAME);
        let dumper_status = shutdown_thread(&self.dumper_state, DLOG_DUMPER_THREAD_NAME);

        // Cancel any pending deferred signal timer after `lifecycle` has transitioned away from
        // `Running` (so `write` will not re-arm `deferred_signal_timer`) so that no timer callback
        // is still running when `DLog` is later dropped. Acquiring and immediately releasing
        // `self.lock` drains any in-flight `DLog::write` that observed `lifecycle == Running`
        // before calling `Timer::cancel` outside the spinlock.
        {
            ksync::lock!(let _guard = self.lock_lock(ksync::source_tag!()));
        }
        self.deferred_signal_timer.cancel();
        debug_assert!({
            // Relaxed suffices because only the single thread that won the compare_exchange above
            // modifies `lifecycle`.
            let prev = self.lifecycle.load(Ordering::Relaxed);
            prev == Lifecycle::ShutdownStarted as u32
        });
        // Release pairs with the Acquire load in `shutdown_finished()`.
        self.lifecycle.store(Lifecycle::ShutdownFinished as u32, Ordering::Release);

        // If one of them failed, report the first failing status we find.
        let result = if notifier_status.is_err() { notifier_status } else { dumper_status };
        dprintf!(
            INFO,
            "debuglog shutdown completed with status {}\n",
            Status::result_into_raw(result)
        );

        // Be sure to pass the status to any other threads that may be waiting here in `shutdown`.
        self.shutdown_finished.signal_result(result);
        result
    }

    /// Returns true iff we have finished shutting down this instance.
    fn shutdown_finished(&self) -> bool {
        // Acquire pairs with the Release store at the end of `shutdown()`.
        self.lifecycle.load(Ordering::Acquire) == Lifecycle::ShutdownFinished as u32
    }

    /// See `dlog_panic_start`.
    fn panic_start(&self) {
        // Stop processing log writes they'll fail over to kernel console and serial.
        self.panic.store(true);
    }

    /// See `dlog_panic_started`.
    fn panic_started(&self) -> bool {
        self.panic.load()
    }

    /// See `dlog_bluescreen_init`.
    fn bluescreen_init(&self) {
        // replay debuglog?

        // Print panic string.
        //
        // WARNING: This string is detected by external tools to detect
        // kernel panics during tests. Changes should be made with care.
        kprint!("\nZIRCON KERNEL PANIC\n\n");

        // Print uptime, current CPU, and version information.
        kprint!("UPTIME: {}ms, CPU: {}\n", current_boot_time().0 / zx_msec(1), curr_cpu_num());
        kprint!("{:s}", kernel_debug_ident());
    }

    /// See `dlog_render_to_crashlog`.
    fn render_to_crashlog(&self, target_span: &mut [u8]) -> usize {
        // Try to obtain the spinlock which protects the debuglog. If this fails, do
        // not proceed, simply render a message to the crashlog indicating that we are
        // unable to proceed.
        //
        // At this point in a panic, all bets are off. If we took an exception while
        // holding this lock, attempting to re-obtain the lock at this point in time
        // could result in either deadlock, or infinite exception recursion, either of
        // which would be Very Bad. Best to just say that we cannot actually recover
        // any portion of the debuglog to the crashlog and move on.
        let _irqd = InterruptDisableGuard::new();
        ksync::try_lock!(
            let maybe_guard = self.try_lock_lock_policy::<NoIrqSavePolicy>(ksync::source_tag!())
        );
        if let Ok(guard) = maybe_guard {
            self.render_to_crashlog_locked(&guard, target_span)
        } else {
            let mut target = StringFile::new(target_span);
            target.write(
                b"Cannot render debuglog to the crashlog! \
                  Failed to acquire the debuglog spinlock.\n",
            );
            target.used_region().len()
        }
    }

    #[cfg(ktest)]
    fn write(&self, severity: u32, flags: u32, bytes: &[u8]) -> Result<(), Status> {
        self.write_with_persist(severity, flags, bytes, PersistLog::Yes)
    }

    fn write_with_persist(
        &self,
        severity: u32,
        flags: u32,
        bytes: &[u8],
        persist: PersistLog,
    ) -> Result<(), Status> {
        let str_bytes = if bytes.len() > DLOG_MAX_DATA { &bytes[..DLOG_MAX_DATA] } else { bytes };
        let len = str_bytes.len();

        if self.panic.load() {
            return Err(Status::BAD_STATE);
        }

        // Our size "on the wire" must be a multiple of DLOG_HEADER_FIFO_ALIGNMENT, so
        // we know that worst case there will be room for a header preamble skipping
        // the last n bytes when the fifo wraps
        let wiresize = size_of::<dlog_header_t>() + dlog_align(len);

        // Prepare the record header before taking the lock
        let mut preamble = dlog_hdr_set(wiresize, size_of::<dlog_header_t>() + len);
        if persist == PersistLog::No {
            preamble |= DLOG_PREAMBLE_SKIP_PERSIST;
        }
        let mut hdr = dlog_header_t {
            preamble,
            datalen: len as u16,
            severity: severity as u8,
            flags: flags as u8,
            timestamp: current_boot_time().0,
            pid: 0,
            tid: 0,
            sequence: 0,
        };
        // SAFETY: `current_get()` returns either null (during very early boot) or a valid pointer
        // to the running thread.
        if let Some(t) = unsafe { ThreadPtr::from_raw(current_get()) } {
            // SAFETY: `t` points to the current running thread.
            hdr.pid = unsafe { t.pid() };
            // SAFETY: `t` points to the current running thread.
            hdr.tid = unsafe { t.tid() };
        }

        let mut do_signal = true;
        let can_persist_live;
        {
            ksync::lock!(let mut guard = self.lock_lock(ksync::source_tag!()));
            let mut fields = guard.as_mut().fields_mut();

            hdr.sequence = *fields.sequence_count;

            // Acquire pairs with the AcqRel compare_exchange in `shutdown()`.
            if self.lifecycle.load(Ordering::Acquire) != Lifecycle::Running as u32 {
                return Err(Status::BAD_STATE);
            }

            // We're about to place a log record in the buffer. Once we've done that
            // we'll need to signal the notifier thread. Ideally we'd just call
            // Event::signal after dropping our spinlock, however, Event::signal might
            // start a new chainlock transaction and we might have been called in a
            // context where there's *already* an active transaction. If there's an
            // active transaction we must instead defer calling signal to a point at
            // which there is no active transaction. We use a Timer for this because
            // Timers only fire when interrupts are enabled and it would be an error to
            // have an active chainlock transaction with interrupts enabled.
            if ChainLockTransaction::active().is_some() {
                do_signal = false;
                // As an optimization, we track whether or not there's a pending timer.
                // This check may race with the timer firing (and clearing the bool),
                // but that's OK. The worst case is that we observed false and set an
                // "extra" timer.
                if !self.pending_deferred_signal.load() {
                    self.pending_deferred_signal.store(true);
                    // SAFETY: `deferred_signal_timer` is structurally pinned inside `self` (which
                    // is `!Unpin` and only constructed via `PinInit`), and `DLog::shutdown` and
                    // `DLog::drop` cancel it before `self` (the callback's `arg`) is destroyed.
                    // Holding `lock` while `lifecycle == Running` serializes this with any other
                    // `set_deadline` and with `cancel` in `DLog::shutdown` (which transitions
                    // `lifecycle` away from `Running` and acquires/releases `lock` before calling
                    // `cancel`) and `DLog::drop`.
                    unsafe {
                        Pin::new_unchecked(&self.deferred_signal_timer).set_deadline(
                            &Deadline::no_slack(InstantMono::INFINITE_PAST.into()),
                            Self::deferred_signal_callback,
                            core::ptr::from_ref(self).cast_mut().cast::<c_void>(),
                        );
                    }
                }
            }

            let data = &mut fields.data.as_mut().get_mut().0;

            // Discard records at tail until there is enough
            // space for the new record.
            while fields.head.wrapping_sub(*fields.tail) > (DLOG_SIZE - wiresize) {
                let tail_offset = *fields.tail & DLOG_MASK;
                if tail_offset > DLOG_SIZE - 4 {
                    core::hint::cold_path();
                    *fields.tail = *fields.head;
                    break;
                }
                let preamble = u32::from_ne_bytes(
                    data[tail_offset..tail_offset + 4].try_into().expect("4-byte slice"),
                );
                let fifolen = dlog_hdr_get_fifolen(preamble);
                let used = fields.head.wrapping_sub(*fields.tail);
                if fifolen < size_of::<dlog_header_t>()
                    || fifolen > DLOG_MAX_RECORD
                    || (fifolen & (DLOG_HEADER_FIFO_ALIGNMENT - 1)) != 0
                    || fifolen > used
                {
                    core::hint::cold_path();
                    *fields.tail = *fields.head;
                    break;
                }
                *fields.tail = fields.tail.wrapping_add(fifolen);
            }

            let mut offset = *fields.head & DLOG_MASK;
            let mut fifospace = DLOG_SIZE - offset;

            let hdr_bytes = hdr.as_bytes();
            let hdr_size = hdr_bytes.len();

            if fifospace >= wiresize && fifospace >= hdr_size + len {
                // everything fits in one write, simple case!
                data[offset..offset + hdr_size].copy_from_slice(hdr_bytes);
                data[offset + hdr_size..offset + hdr_size + len].copy_from_slice(str_bytes);
            } else if fifospace < hdr_size {
                // the wrap happens in the header
                data[offset..offset + fifospace].copy_from_slice(&hdr_bytes[..fifospace]);
                let rem_hdr = hdr_size - fifospace;
                data[..rem_hdr].copy_from_slice(&hdr_bytes[fifospace..]);
                data[rem_hdr..rem_hdr + len].copy_from_slice(str_bytes);
            } else {
                // the wrap happens in the data
                data[offset..offset + hdr_size].copy_from_slice(hdr_bytes);
                offset += hdr_size;
                fifospace -= hdr_size;
                data[offset..offset + fifospace].copy_from_slice(&str_bytes[..fifospace]);
                data[..len - fifospace].copy_from_slice(&str_bytes[fifospace..]);
            }
            *fields.head = fields.head.wrapping_add(wiresize);
            *fields.sequence_count = fields.sequence_count.wrapping_add(1);
            can_persist_live = !*fields.defer_persist_until_replay;
        }

        if persist == PersistLog::Yes && can_persist_live && self.is_persist_log_enabled() {
            self.format_and_persist_record(&hdr, str_bytes);
        }

        if do_signal {
            self.notifier_state.event.signal();
        }

        Ok(())
    }

    // Kept out of line so `write_with_persist` does not reserve the 352-byte render buffer on its
    // stack frame when persistent debuglog is inactive.
    #[inline(never)]
    fn format_and_persist_record(&self, hdr: &dlog_header_t, bytes: &[u8]) {
        let mut pdlog_buf = [0u8; RENDERED_LOG_BUF_SIZE];
        let mut pdlog_file = StringFile::new(&mut pdlog_buf);
        let hdr_len = Self::format_header(Some(pdlog_file.available_region()), hdr);
        pdlog_file.skip(hdr_len);
        pdlog_file.write(bytes);
        if bytes.last() != Some(&b'\n') {
            pdlog_file.write(b"\n");
        }
        self.persist_log_message(pdlog_file.as_bytes());
    }

    /// Replays any records currently queued in the debuglog ring buffer to the persistent debuglog.
    fn persist_existing(&self) {
        if !self.is_persist_log_enabled() || self.panic.load() {
            ksync::lock!(let mut guard = self.lock_lock(ksync::source_tag!()));
            *guard.as_mut().defer_persist_until_replay_mut() = false;
            return;
        }

        let mut start_offset = None;
        let mut offset = 0;
        let mut rec = dlog_record_t::new_zeroed();
        loop {
            let skip_record = {
                ksync::lock!(let mut guard = self.lock_lock(ksync::source_tag!()));
                let head = *guard.head();
                let tail = *guard.tail();
                let start = *start_offset.get_or_insert_with(|| {
                    offset = tail;
                    tail
                });
                if head.wrapping_sub(tail) < head.wrapping_sub(offset) {
                    offset = tail;
                }
                let next_record = if offset == head
                    || head.wrapping_sub(tail) > DLOG_SIZE
                    || offset.wrapping_sub(start) >= DLOG_SIZE
                {
                    None
                } else {
                    self.read_record(&guard, offset, None).ok()
                };
                let Some(record) = next_record else {
                    *guard.as_mut().defer_persist_until_replay_mut() = false;
                    break;
                };
                offset = offset.wrapping_add(dlog_hdr_get_fifolen(record.hdr.preamble));
                if (record.hdr.preamble & DLOG_PREAMBLE_SKIP_PERSIST) != 0 {
                    true
                } else {
                    rec.hdr = record.hdr;
                    let data_out = rec.data.as_mut_bytes();
                    let r1_len = record.region1.len();
                    let r2_len = record.region2.len();
                    data_out[..r1_len].copy_from_slice(record.region1);
                    data_out[r1_len..r1_len + r2_len].copy_from_slice(record.region2);
                    false
                }
            };

            if !skip_record {
                self.format_and_persist_record(
                    &rec.hdr,
                    &rec.data.as_bytes()[..rec.hdr.datalen as usize],
                );
            }
        }
    }

    /// Blocks until the dumper thread has dumped all queued log messages up to the current
    /// sequence. Must only be called from a thread context where blocking is permitted.
    fn sync(&self) {
        if dlog_bypass() || self.shutdown_finished() {
            return;
        }

        let target_sequence = {
            ksync::lock!(let guard = self.lock_lock(ksync::source_tag!()));
            *guard.sequence_count()
        };

        // Acquire pairs with the Release store in `dumper_thread`.
        while self.dumper_sequence.load(Ordering::Acquire) < target_sequence {
            if self.dumper_state.thread.load(Ordering::SeqCst).is_null()
                || self.dumper_state.shutdown_requested.load(Ordering::SeqCst)
            {
                break;
            }
            self.dumper_state.event.signal();
            let _ = self
                .dumper_drained_event
                .wait(&Deadline::after_mono(DurationMono(zx_msec(50)), TimerSlack::none()));
        }
    }

    /// Writes a formatted log message to serial output (or the test hook when running unit tests).
    fn output_log_message(&self, log: &[u8]) {
        #[cfg(ktest)]
        // SAFETY: `output_hook` is only written in `DLogOutputTest::new` before starting threads.
        if let Some((hook, ctx)) = unsafe { *self.output_hook.get() } {
            // SAFETY: The test harness guarantees `ctx` is valid for the lifetime of the test.
            unsafe { hook(ctx, log) };
            return;
        }
        dlog_serial_write(log);
    }

    #[inline]
    fn is_persist_log_enabled(&self) -> bool {
        #[cfg(ktest)]
        // SAFETY: `persist_hook` is only written in `DLogPersistTest::new` before `self` is used.
        if let Some(hook) = unsafe { *self.persist_hook.get() } {
            // SAFETY: `DLogPersistTest::new` sets `hook.ctx` to a pinned `DLogPersistTest` that
            // outlives `self`.
            return unsafe { (hook.is_enabled)(hook.ctx) };
        }
        core::ptr::eq(core::ptr::from_ref(self), DLOG.as_ptr().cast_const())
            && persistent_dlog_enabled()
    }

    fn persist_log_message(&self, log: &[u8]) {
        #[cfg(ktest)]
        // SAFETY: `persist_hook` is only written in `DLogPersistTest::new` before `self` is used.
        if let Some(hook) = unsafe { *self.persist_hook.get() } {
            // SAFETY: `DLogPersistTest::new` sets `hook.ctx` to a pinned `DLogPersistTest` that
            // outlives `self`.
            unsafe { (hook.persist)(hook.ctx, log) };
            return;
        }
        if core::ptr::eq(core::ptr::from_ref(self), DLOG.as_ptr().cast_const()) {
            persistent_dlog_write(log);
        }
    }

    /// Attempt to format a debuglog record header into the memory location supplied in `target`.
    /// The value returned will depend on what is passed for `target`.
    ///
    /// 1) If `target` is `None`, this is a "measurement" operation. The
    ///    return value will be an indication of the length which *would be needed*
    ///    in order to properly render the header into the buffer supplied.
    /// 2) If `target` is `Some`, then this is a "render" operation. The
    ///    return value will be an indication of the *amount of the target buffer
    ///    which was consumed*. It will never exceed the length of the target
    ///    buffer.
    fn format_header(target: Option<&mut [u8]>, hdr: &dlog_header_t) -> usize {
        let mut buf = [0u8; 64];
        let rendered = kformat!(
            &mut buf,
            "[{:05}.{:03}] {:05u}:{:05u}> ",
            (hdr.timestamp / zx_sec(1)) as i32,
            ((hdr.timestamp / zx_msec(1)) as u64 % 1000) as i32,
            hdr.pid,
            hdr.tid
        );
        match target {
            None => rendered.len(),
            Some(out) => {
                let len = out.len().min(rendered.len());
                out[..len].copy_from_slice(&rendered[..len]);
                len
            }
        }
    }

    /// A small helper alias for `format_header` which helps to make it a bit more
    /// clear at the callsite what operation is being performed.
    fn measure_rendered_header(hdr: &dlog_header_t) -> usize {
        Self::format_header(None, hdr)
    }

    /// Attempt to read `target.len()` bytes from an absolute location in the
    /// debuglog buffer given by `offset`, storing the result in `target`
    /// and dealing with any wrapping of the ring buffer in the process. Note that
    /// this method is not specific to reading a `dlog_header_t`, or a record's
    /// payload. It simply attempts to read a contiguous sequence of bytes from
    /// the buffer, and automatically deals with the ring buffer wrapping for the
    /// user at the same time.
    fn reassemble_from_offset<P: ExclusiveLockPolicy<TrackedMonitoredSpinlock>>(
        &self,
        guard: &DLogLockGuard<'_, P>,
        mut offset: usize,
        target: &mut [u8],
    ) -> Result<(), Status> {
        // Attempting to read 0 bytes is simple, we are done already.
        if target.is_empty() {
            return Ok(());
        }

        // Attempting to read more data than can exist in the buffer is considered an error.
        if target.len() > DLOG_SIZE {
            return Err(Status::INVALID_ARGS);
        }

        let data = &guard.data().0;
        offset &= DLOG_MASK;
        let fifospace = DLOG_SIZE - offset;
        if target.len() <= fifospace {
            // The requested region exists contiguously in the circular buffer.
            target.copy_from_slice(&data[offset..offset + target.len()]);
        } else {
            // The requested region wraps in the circular buffer, and needs to be
            // copied as 2 chunks.
            target[..fifospace].copy_from_slice(&data[offset..offset + fifospace]);
            let rem = target.len() - fifospace;
            target[fifospace..].copy_from_slice(&data[..rem]);
        }

        Ok(())
    }

    /// Attempt to read a `Record` from the ring buffer located at `offset`,
    /// reporting any diagnostic information to `error_file` in the case of
    /// trouble.
    fn read_record<'a, P: ExclusiveLockPolicy<TrackedMonitoredSpinlock>>(
        &'a self,
        guard: &'a DLogLockGuard<'_, P>,
        offset: usize,
        error_file: Option<&mut StringFile<'_>>,
    ) -> Result<Record<'a>, Status> {
        // Attempt to reassemble the header from the specified offset.
        let mut ret = Record::new();
        if let Err(status) = self.reassemble_from_offset(guard, offset, ret.hdr.as_mut_bytes()) {
            core::hint::cold_path();
            if let Some(error_file) = error_file {
                let mut buf = [0u8; 128];
                error_file.write(kformat!(
                    &mut buf,
                    "Failed to read header at offset {:u} ({})\n",
                    offset,
                    status.into_raw()
                ));
            }
            return Err(status);
        }

        // Perform consistency checks of the lengths.
        let readlen = dlog_hdr_get_readlen(ret.hdr.preamble);
        let hdr_sz = size_of::<dlog_header_t>();
        if (readlen < hdr_sz)
            || (readlen > DLOG_MAX_RECORD)
            || ((readlen - hdr_sz) != ret.hdr.datalen as usize)
            || (dlog_hdr_get_fifolen(ret.hdr.preamble) != dlog_align(readlen))
        {
            core::hint::cold_path();
            if let Some(error_file) = error_file {
                let mut buf = [0u8; 128];
                error_file.write(kformat!(
                    &mut buf,
                    "Bad lengths (pre {:u}, hdr_sz {:u}, datalen {:u})\n",
                    readlen,
                    hdr_sz,
                    ret.hdr.datalen
                ));
            }
            return Err(Status::INTERNAL);
        }

        if ret.hdr.datalen != 0 {
            // Figure out if the payload portion of the record needs to be represented
            // by one contiguous region, or two. Start by computing the offset into
            // the ring buffer where the data region begins. Next compute the
            // distance from this offset to the end of the ring buffer. This is the
            // total amount of contiguous space we have for the payload. If the
            // length of the payload is larger than this region, we need to produce
            // two regions, one from the start of the payload to the end of the ring,
            // and the other from the start of the ring to the end of the payload.
            // Otherwise, we can just produce a single contiguous region and be done
            // with it.
            let data_offset = offset.wrapping_add(hdr_sz) & DLOG_MASK;
            let fifospace = DLOG_SIZE - data_offset;
            let data = &guard.data().0;
            let datalen = ret.hdr.datalen as usize;
            if datalen <= fifospace {
                ret.region1 = &data[data_offset..data_offset + datalen];
                ret.ends_with_newline = ret.region1.last() == Some(&b'\n');
            } else {
                ret.region1 = &data[data_offset..data_offset + fifospace];
                ret.region2 = &data[..datalen - fifospace];
                ret.ends_with_newline = ret.region2.last() == Some(&b'\n');
            }
        }

        Ok(ret)
    }

    fn render_to_crashlog_locked<P: ExclusiveLockPolicy<TrackedMonitoredSpinlock>>(
        &self,
        guard: &DLogLockGuard<'_, P>,
        target_span: &mut [u8],
    ) -> usize {
        if target_span.is_empty() {
            return 0;
        }

        let target_span_size = target_span.len();
        let mut target = StringFile::new(target_span);

        let head = *guard.head();
        let tail = *guard.tail();

        // Check for any obvious signs that the log may have become corrupted. Head
        // and tail are absolute offsets into the ring buffer, and old records are
        // destroyed to make room for new ones during write operations. Because of
        // this, it should not be possible for tail to ever be greater than head, and
        // the distance between head and tail should never be larger than the size of
        // the log buffer.
        if (tail > head) || ((head - tail) > DLOG_SIZE) {
            let mut buf = [0u8; 128];
            target.write(kformat!(
                &mut buf,
                "Debug log appears corrupt: (head, tail) = ({:u}, {:u})\n",
                head,
                tail
            ));
            return target.used_region().len();
        }

        // A small helper to compute the size of a record, were it to be rendered.
        let rendered_record_size = |r: &Record<'_>| -> usize {
            Self::measure_rendered_header(&r.hdr)
                + r.region1.len()
                + r.region2.len()
                + if r.ends_with_newline { 0 } else { 1 }
        };

        // Figure out how much space the whole log would take.
        let mut space_consumed = 0;
        let mut offset = tail;
        while offset < head {
            let Ok(record) = self.read_record(guard, offset, Some(&mut target)) else {
                return target.used_region().len();
            };

            space_consumed += rendered_record_size(&record);
            offset += dlog_hdr_get_fifolen(record.hdr.preamble);
        }

        // Starting from the end, skip records until we get to the point where we can
        // fit the rest of the rendered data into target_span, then render the rest of
        // the records.
        let mut offset = tail;
        while offset < head {
            let Ok(record) = self.read_record(guard, offset, Some(&mut target)) else {
                return target.used_region().len();
            };

            if space_consumed > target_span_size {
                space_consumed -= rendered_record_size(&record);
            } else {
                let hdr_len = Self::format_header(Some(target.available_region()), &record.hdr);
                target.skip(hdr_len);
                target.write(record.region1);
                target.write(record.region2);
                if !record.ends_with_newline {
                    target.write(b"\n");
                }
            }

            offset += dlog_hdr_get_fifolen(record.hdr.preamble);
        }

        target.used_region().len()
    }

    /// The debuglog notifier thread observes when the debuglog is
    /// written and calls the notify callback on any readers that
    /// have one so they can process new log messages.
    fn notifier_thread(&self) -> i32 {
        loop {
            if self.notifier_state.shutdown_requested.load(Ordering::SeqCst) {
                break;
            }
            let _ = self.notifier_state.event.wait(&Deadline::infinite());

            // notify readers that new DLOG items were posted
            {
                ksync::lock!(let guard = self.lock_readers_lock());
                for reader in guard.readers().iter() {
                    // SAFETY: We hold `self.readers_lock`, which synchronizes access to each
                    // reader's `notify` and `cookie` fields.
                    unsafe { reader.notify() };
                }
            }
        }
        zx_types::ZX_OK
    }

    fn dumper_thread(&self) -> i32 {
        // assembly buffer with room for log text plus header text
        let mut tmp = [0u8; RENDERED_LOG_BUF_SIZE];

        let mut rec = dlog_record_t::new_zeroed();
        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        /// # Safety
        ///
        /// `cookie` must be a valid pointer to `AutounsignalEvent`.
        unsafe fn notify_dumper(cookie: *mut c_void) {
            // SAFETY: The caller guarantees `cookie` points to a valid `AutounsignalEvent`.
            unsafe { (*cookie.cast::<AutounsignalEvent>()).signal() };
        }
        let event_ptr = core::ptr::from_ref(&self.dumper_state.event).cast_mut().cast::<c_void>();
        // SAFETY: `event_ptr` is valid for `notify_dumper` until `reader.disconnect()` runs via
        // `_disconnect`, and `self` outlives `reader`.
        unsafe { reader.initialize(Some(notify_dumper), event_ptr, Some(self)) };
        let _disconnect = zr::defer(|| reader.disconnect());

        let mut expected_sequence: u64 = 0;

        let mut done = false;
        while !done {
            let _ = self.dumper_state.event.wait(&Deadline::infinite());

            // If shutdown has been requested, this will be our last loop iteration.
            //
            // We do not break early because we guarantee that any messages logged prior to the
            // start of the shutdown sequence will be emitted.
            done = self.dumper_state.shutdown_requested.load(Ordering::SeqCst);

            // Read out all the records and dump them to the kernel console.
            while reader.read(0, &mut rec).is_ok() {
                let gap = rec.hdr.sequence.wrapping_sub(expected_sequence);
                if gap > 0 {
                    let sv = kformat!(&mut tmp, "debuglog: dropped {:u} messages\n", gap);
                    self.output_log_message(sv.as_bytes());
                }
                let mut tmp_file = StringFile::new(&mut tmp);
                expected_sequence = rec.hdr.sequence.wrapping_add(1);

                let hdr_len = Self::format_header(Some(tmp_file.available_region()), &rec.hdr);
                tmp_file.skip(hdr_len);
                tmp_file.write(&rec.data.as_bytes()[..rec.hdr.datalen as usize]);
                // If the record didn't end with a newline, add one now.
                if (rec.hdr.datalen == 0)
                    || (rec.data.as_bytes()[rec.hdr.datalen as usize - 1] != b'\n')
                {
                    tmp_file.write(b"\n");
                }
                let sv = tmp_file.used_region();
                self.output_log_message(sv);
            }

            // Release pairs with the Acquire load in `sync()`.
            self.dumper_sequence.store(expected_sequence, Ordering::Release);
            self.dumper_drained_event.signal();
        }

        0
    }

    /// # Safety
    ///
    /// `arg` must be a valid pointer to `DLog`.
    unsafe extern "C" fn deferred_signal_callback(_timer: *mut Timer, _now: i64, arg: *mut c_void) {
        let dlog = arg.cast::<Self>();
        // SAFETY: `arg` is a pointer to `DLog` passed to `Timer::set_deadline` in `write`, and
        // `DLog::shutdown` and `DLog`'s `PinnedDrop` cancel `deferred_signal_timer` before `DLog`
        // is destroyed. Place-projecting to the individual fields avoids forming `&DLog` across
        // the entire struct.
        unsafe {
            (*dlog).notifier_state.event.signal();
            (*dlog).pending_deferred_signal.store(false);
        }
    }
}

pub type NotifyCallback = unsafe fn(*mut c_void);

const INITIALIZING_LOG: *const DLog = core::ptr::without_provenance(1);
const DISCONNECTED_LOG: *const DLog = core::ptr::without_provenance(2);

fn is_valid_log_ptr(ptr: *const DLog) -> bool {
    !ptr.is_null() && ptr != INITIALIZING_LOG && ptr != DISCONNECTED_LOG
}

/// `DlogReader`s drain debuglogs. Owners of `DlogReader`s are called back as
/// messages are pushed through the debuglog, via the `notify` callback.
#[derive(DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct DlogReader {
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<DlogReader>,
    /// Atomic pointer to the associated `DLog` (or null / `INITIALIZING_LOG` / `DISCONNECTED_LOG`),
    /// synchronizing `initialize`, `read`, and `disconnect` across shared `&Self` references.
    log: zr::AtomicConstPtr<DLog>,
    /// Atomic read offset into `log`, allowing interior mutation through `&Self` while protected by
    /// `DLog::lock`.
    tail: AtomicUsize,
    /// Notification callback written in `initialize` and read in `DLog::notifier_thread` under
    /// `DLog::readers_lock`.
    notify: UnsafeCell<Option<NotifyCallback>>,
    /// Opaque argument for `notify`, written in `initialize` and read in `DLog::notifier_thread`
    /// under `DLog::readers_lock`.
    cookie: UnsafeCell<*mut c_void>,
    #[pin]
    _pinned: PhantomData<PhantomPinned>,
}

zr::static_assert_size_and_align!(DlogReader, 48, 8);

// SAFETY: `DlogReader` access is synchronized by `log` atomic operations, the owning
// `LogDispatcher` lock or single consumer thread, and `DLog::readers_lock`.
unsafe impl Send for DlogReader {}
// SAFETY: `DlogReader::log` is an `AtomicConstPtr<DLog>` synchronizing state transitions under
// `DLog::readers_lock`; `notify` and `cookie` are written under `DLog::readers_lock` in
// `initialize` and read under `DLog::readers_lock` in `DLog::notifier_thread`; `tail` is an
// `AtomicUsize` accessed under `DLog::lock`; `node` is protected by `DLog::readers_lock`.
unsafe impl Sync for DlogReader {}

#[pinned_drop]
impl PinnedDrop for DlogReader {
    fn drop(self: Pin<&mut Self>) {
        // Disconnect must have been called before the DlogReader is destroyed.
        assert!(!self.node.in_container());
    }
}

impl DlogReader {
    /// Creates an uninitialized `DlogReader` (`PinInit`).
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            node: DoublyLinkedListNode::new(),
            log: zr::AtomicConstPtr::new(core::ptr::null()),
            tail: AtomicUsize::new(0),
            notify: UnsafeCell::new(None),
            cookie: UnsafeCell::new(core::ptr::null_mut()),
            _pinned: PhantomData,
        })
    }

    /// Since `DlogReader`s typically capture containing objects via `cookie`, they
    /// use 2-phase initialization to avoid races in the construction of the
    /// `DlogReader` and the containing object.
    /// The optional parameter `log` is used mainly for testing purpose. It should be left as
    /// `None` for all other uses, in which case the static global `DLOG` will be used. If `log`
    /// takes the default value i.e. static global `DLOG`, it will outlive the `DlogReader` and
    /// hence there will be no lifetime issues. If `log` is specified, as it would be in case of
    /// tests, the test has to ensure that the `log` object outlives any `DlogReader`s associated
    /// to it.
    ///
    /// # Safety
    ///
    /// - If `log` is `None`, `dlog_init_early()` must have completed.
    /// - If `log` is `Some`, it must outlive this `DlogReader` (until `Drop`).
    /// - Calling `notify(cookie)` (synchronously during `initialize` and asynchronously from
    ///   `notifier_thread` until `disconnect` returns) must satisfy `notify`'s safety
    ///   preconditions, and `notify` must not re-entrantly acquire `DLog::readers_lock` (which is
    ///   held while `notify` is invoked).
    /// - `disconnect` must be called before `Drop`, and must not be called concurrently with
    ///   `initialize`.
    pub unsafe fn initialize(
        self: Pin<&Self>,
        notify: Option<NotifyCallback>,
        cookie: *mut c_void,
        log: Option<&DLog>,
    ) {
        // A DlogReader can only be initialized once. Relaxed suffices: the CAS only claims the
        // reader against double initialization, and nothing is published through
        // INITIALIZING_LOG. `log_ref` is published by the Release store below, under
        // `readers_lock`.
        assert!(
            self.log
                .compare_exchange(
                    core::ptr::null(),
                    INITIALIZING_LOG,
                    Ordering::Relaxed,
                    Ordering::Relaxed
                )
                .is_ok(),
            "DlogReader can only be initialized once"
        );

        let log_ref = log.unwrap_or_else(|| dlog());
        let this: &Self = self.get_ref();
        let reader_ptr = core::ptr::from_ref(this).cast_mut();

        ksync::lock!(let mut readers_guard = log_ref.lock_readers_lock());
        // SAFETY: `self.notify` and `self.cookie` are only accessed while holding
        // `log_ref.readers_lock`.
        unsafe {
            *this.notify.get() = notify;
            *this.cookie.get() = cookie;
        }
        // SAFETY: `reader_ptr` is a valid pinned `DlogReader` not currently in any list, and
        // the caller guarantees `disconnect` removes it before drop.
        unsafe { readers_guard.as_mut().readers_mut().push_back_raw(reader_ptr) };

        let do_notify = {
            ksync::lock!(let guard = log_ref.lock_lock(ksync::source_tag!()));
            // Ordering::Relaxed is sufficient because `tail` is protected by `log.lock`.
            this.tail.store(*guard.tail(), Ordering::Relaxed);
            *guard.tail() != *guard.head()
        };

        // Publish `log_ref` while still holding `readers_lock` so `disconnect` is serialized
        // with `initialize`. Release pairs with Acquire loads in `read` and `disconnect`.
        this.log.store(core::ptr::from_ref(log_ref), Ordering::Release);

        // simulate notify callback for events that arrived
        // before we were initialized
        if do_notify && let Some(notify) = notify {
            // SAFETY: The caller guaranteed `cookie` is valid for `notify`.
            unsafe { notify(cookie) };
        }
    }

    // TODO(https://fxbug.dev/576429701): support reading multiple messages at a time
    // TODO(https://fxbug.dev/576429701): filter with flags
    /// Read one record out of the log and store it in `record`.
    ///
    /// Upon success, returns `Ok(actual)` with the record's size.
    pub fn read(
        self: Pin<&Self>,
        _flags: u32,
        record: &mut dlog_record_t,
    ) -> Result<usize, Status> {
        let mut status = Err(Status::SHOULD_WAIT);
        // Acquire pairs with the Release stores in `initialize` and `disconnect`.
        let log_ptr = self.log.load(Ordering::Acquire);
        if !is_valid_log_ptr(log_ptr) {
            return Err(Status::BAD_STATE);
        }
        // SAFETY: `log_ptr` points to a valid `DLog` that outlives this `DlogReader`.
        let log = unsafe { &*log_ptr };

        {
            ksync::lock!(let mut guard = log.lock_lock(ksync::source_tag!()));

            let log_head = *guard.head();
            let log_tail = *guard.tail();
            // Ordering::Relaxed is sufficient because `tail` is protected by `log.lock`.
            let mut rtail = self.tail.load(Ordering::Relaxed);

            // If the read-tail is not within the range of log-tail..log-head
            // this reader has been lapped by a writer and we reset our read-tail
            // to the current log-tail.
            if log_head.wrapping_sub(log_tail) < log_head.wrapping_sub(rtail) {
                rtail = log_tail;
            }

            if rtail != log_head {
                // Attempt to read the header into the user supplied buffer.
                if let Err(err) =
                    log.reassemble_from_offset(&guard, rtail, record.hdr.as_mut_bytes())
                {
                    core::hint::cold_path();
                    self.tail.store(log_head, Ordering::Relaxed);
                    // Drop the dlog lock before panicking, or asserting anything.
                    guard.as_mut().release();
                    debug_assert!(
                        false,
                        "DLOG read failure at offset {}. Failed to reassemble header ({})",
                        rtail,
                        err.into_raw()
                    );
                    return Err(err);
                }

                // Perform consistency checks of the lengths.
                let readlen = dlog_hdr_get_readlen(record.hdr.preamble);
                let hdr_sz = size_of::<dlog_header_t>();
                if (readlen < hdr_sz)
                    || (readlen > DLOG_MAX_RECORD)
                    || ((readlen - hdr_sz) != record.hdr.datalen as usize)
                    || (dlog_hdr_get_fifolen(record.hdr.preamble) != dlog_align(readlen))
                {
                    core::hint::cold_path();
                    self.tail.store(log_head, Ordering::Relaxed);
                    // Drop the dlog lock before panicking, or asserting anything.
                    guard.as_mut().release();
                    debug_assert!(
                        false,
                        "DLOG read failure at offset {}. \
                         Bad lengths (pre {}, hdr_sz {}, datalen {})",
                        rtail, readlen, hdr_sz, record.hdr.datalen
                    );
                    return Err(Status::INTERNAL);
                }

                // Reassemble the data from the ring buffer.
                let datalen = record.hdr.datalen as usize;
                if let Err(err) = log.reassemble_from_offset(
                    &guard,
                    rtail.wrapping_add(hdr_sz),
                    &mut record.data.as_mut_bytes()[..datalen],
                ) {
                    core::hint::cold_path();
                    self.tail.store(log_head, Ordering::Relaxed);
                    // Drop the dlog lock before panicking, or asserting anything.
                    guard.as_mut().release();
                    debug_assert!(
                        false,
                        "DLOG read failure at offset {}. Failed to reassemble {} data bytes ({})",
                        rtail,
                        record.hdr.datalen,
                        err.into_raw()
                    );
                    return Err(Status::INTERNAL);
                }

                // Everything went well. Advance the tail pointer, report the actual length read,
                // and get out.
                rtail = rtail.wrapping_add(dlog_hdr_get_fifolen(record.hdr.preamble));
                let actual = dlog_hdr_get_readlen(record.hdr.preamble);
                record.hdr.preamble = 0;
                status = Ok(actual);
            }

            // Ordering::Relaxed is sufficient because `tail` is protected by `log.lock`.
            self.tail.store(rtail, Ordering::Relaxed);
        }

        status
    }

    /// Similar to `initialize`, `DlogReader`s are manually stopped via `disconnect`
    /// to avoid reentrancy issues in the `DlogReader` and its containing object.
    ///
    /// `disconnect` must be called before `Drop`, and is idempotent.
    pub fn disconnect(self: Pin<&Self>) {
        // Acquire pairs with the Release stores in `initialize` and `disconnect`.
        let log_ptr = self.log.load(Ordering::Acquire);
        if is_valid_log_ptr(log_ptr) {
            // SAFETY: `log_ptr` points to a valid `DLog` that outlives this `DlogReader`.
            let log = unsafe { &*log_ptr };
            ksync::lock!(let mut guard = log.lock_readers_lock());
            if self.node.in_container() {
                // SAFETY: `self` was inserted into `log.readers` in `initialize`.
                unsafe {
                    guard.as_mut().readers_mut().erase(self.get_ref());
                }
            }
            // Release pairs with the Acquire loads in `read` and `disconnect`, so a caller that
            // observes DISCONNECTED_LOG also observes the erase above (e.g. before drop checks
            // `in_container()`).
            self.log.store(DISCONNECTED_LOG, Ordering::Release);
        }
    }

    /// Invokes the reader's notification callback if set.
    ///
    /// # Safety
    ///
    /// The caller must hold `readers_lock` of the `DLog` this reader is linked into.
    unsafe fn notify(&self) {
        // SAFETY: The caller holds `DLog::readers_lock`, which serializes with the writes in
        // `initialize`.
        let (notify_opt, cookie) = unsafe { (*self.notify.get(), *self.cookie.get()) };
        if let Some(notify_fn) = notify_opt {
            // SAFETY: `cookie` was validated by the caller of `initialize`.
            unsafe { notify_fn(cookie) };
        }
    }
}

static DLOG: LazyInit<DLog, NoCheck> = LazyInit::uninit();

fn dlog() -> &'static DLog {
    // SAFETY: `DLOG` is initialized in `dlog_init_early` at the very start of `lk_main`.
    unsafe { DLOG.get_unchecked() }
}

/// Initialize the debuglog subsystem. Called once at extremely early boot.
/// Called first thing in init, so very early printfs can go to serial console.
///
/// # Safety
///
/// Must be called at most once during single-threaded early boot before any other `dlog_*`
/// functions are called.
unsafe fn dlog_init_early() {
    // Construct the debuglog. Done here so we can construct it manually before
    // the global constructors are run.
    // SAFETY: Caller guarantees this is called once during single-threaded early boot; `DLOG` is
    // in static storage and is never moved or dropped.
    let _ = unsafe { Pin::static_ref(&DLOG).init_pin(DLog::new_with_defer_persist(true)) };
    // SAFETY: Our caller guarantees that this runs once during single-threaded early boot.
    // `lk_main` calls this before the first printf, which is the earliest
    // `persistent_dlog_*` call.
    unsafe { persistent_dlog_init_early() };
}

/// Writes `bytes` to debuglog.
///
/// Returns `Status::BAD_STATE` if the dlog has already started shutting down.
pub fn dlog_write(
    severity: u32,
    flags: u32,
    bytes: &[u8],
    persist: PersistLog,
) -> Result<(), Status> {
    dlog().write_with_persist(severity, flags, bytes, persist)
}

// Common bottleneck between `sys_debug_write()` and `DLog::dumper_thread`
// to reduce interleaved messages between the serial console and the
// debuglog drainer.
#[guarded]
struct DlogSerialWriteLock {
    #[mutex]
    lock: KMutex<TrackedMutex>,
}

static DLOG_SERIAL_WRITE_LOCK: LazyInit<DlogSerialWriteLock, NoCheck> = LazyInit::uninit();

fn dlog_serial_write_lock_init(_level: init::LkInitLevel) {
    // SAFETY: lk init hooks run once during single-threaded early boot after global constructors
    // (`RustLockClassRegistrar`) and before any `dlog_serial_write*` caller runs (`dumper_thread`
    // starts at `LK_INIT_LEVEL_PLATFORM`, the kernel console at `LK_INIT_LEVEL_HEAP`, and
    // `sys_debug_write` from userspace).
    unsafe {
        let Ok(()) = Pin::static_ref(&DLOG_SERIAL_WRITE_LOCK)
            .init_pin(pin_init!(DlogSerialWriteLock { lock <- KMutex::init() }));
    }
}

init::lk_init_hook!(
    dlog_serial_write_lock,
    dlog_serial_write_lock_init,
    init::LK_INIT_LEVEL_EARLIEST
);

fn dlog_serial_write_internal(bytes: &[u8], interruptible: Interruptible) -> Result<(), Status> {
    if dlog_bypass() {
        // If LL DEBUG is enabled we take this path which uses a spinlock
        // and prevents the direct writes from the kernel from interleaving
        // with our output.
        serial_write(bytes);
        return Ok(());
    }

    loop {
        // If dlog has finished shutting down, just drop this message. See
        // https://fxbug.dev/42084162.
        if dlog().shutdown_finished() {
            return Ok(());
        }

        {
            // SAFETY: `DLOG_SERIAL_WRITE_LOCK` is initialized in `dlog_serial_write_lock_init` at
            // `LK_INIT_LEVEL_EARLIEST` before any `dlog_serial_write*` caller runs.
            let serial_lock = unsafe { DLOG_SERIAL_WRITE_LOCK.get_unchecked() };
            ksync::lock!(let _guard = serial_lock.lock_lock());
            if Status::ok(platform_dputs_thread(bytes)) != Err(Status::SHOULD_WAIT) {
                return Ok(());
            }
        }

        // The debug UART has been suspended. Wait for it to be resumed and try
        // again. Note that we must not hold DlogSerialWriteLock while waiting.
        // Acquiring the lock is not interruptible, so holding it here would
        // prevent other (interruptible) writers waiting for the lock from being
        // killed or suspended.
        platform_serial_wait_for_resume(interruptible)?;
    }
}

/// Writes `bytes` out the serial (debug) port.
///
/// This function is a no-op if the dlog has already been shutdown
/// (i.e. `dlog_shutdown` has completed).
///
/// If the debug serial port has been suspended (see `platform_serial_suspend`),
/// this function blocks (not interruptibly) until it has been resumed, or until
/// the system begins halting.
pub fn dlog_serial_write(bytes: &[u8]) {
    let status = dlog_serial_write_internal(bytes, Interruptible::No);
    debug_assert!(status.is_ok());
}

/// Like `dlog_serial_write`, except that if the calling thread must wait for the
/// debug serial port to be resumed, the wait may be interrupted by the thread
/// being killed (returning `Status::INTERNAL_INTR_KILLED`) or suspended (returning
/// `Status::INTERNAL_INTR_RETRY`). In either case nothing is written.
///
/// Used by `sys_debug_write()`.
pub fn dlog_serial_write_interruptible(bytes: &[u8]) -> Result<(), Status> {
    dlog_serial_write_internal(bytes, Interruptible::Yes)
}

/// Print the equivalent of a "blue screen" message. When called during a panic,
/// this function should be called after `dlog_panic_start` to ensure that its
/// printfs bypass the normal debuglog queue and drainer thread.
pub fn dlog_bluescreen_init() {
    dlog().bluescreen_init();
}

/// Tell the debuglog that the system has started to panic so that subsequent
/// writes won't wake any debuglog threads. This function should be called
/// *very* early in the process of panicking. In particular, it should be called
/// before `dlog_bluescreen_init`.
pub fn dlog_panic_start() {
    dlog().panic_start();
}

/// Returns true if `dlog_panic_start` has been called.
pub fn dlog_panic_started() -> bool {
    dlog().panic_started()
}

/// Shutdown the debuglog subsystem.
///
/// Blocks, waiting up to `deadline`, for dlog threads to terminate. It is safe
/// for `dlog_shutdown` to be called concurrently by multiple threads. Only one thread
/// will "win" and all others will simply wait for it to complete.
///
/// Once shutdown has begun, `dlog_write` will fail. However, calls to
/// `dlog_serial_write` will continue to work until shutdown has completed. At
/// which point, they will become no-ops.
///
/// On failure, the debuglog subsystem is left in an undefined state.
///
/// Returns `Ok(())` on success.
pub fn dlog_shutdown(deadline: InstantMono) -> Result<(), Status> {
    // If usermode has suspended the debug UART, the dumper thread may be blocked
    // (indefinitely) waiting for it to be resumed. Since the system is halting,
    // discard any output which cannot be written rather than waiting.
    platform_serial_discard_writes_while_suspended();
    dlog().shutdown(deadline)
}

/// Renders as many of the recent debug log entries as will fit into the memory
/// region specified by `target`. Returns the number of bytes of `target` which
/// were filled.
pub fn dlog_render_to_crashlog(target: &mut [u8]) -> usize {
    dlog().render_to_crashlog(target)
}

/// Blocks until the dumper thread has dumped all queued log messages up to the current sequence.
/// Must only be called from a thread context where blocking is permitted.
pub fn dlog_sync() {
    dlog().sync();
}

fn debuglog_persist_early_init(_level: init::LkInitLevel) {
    dlog().persist_existing();
}

fn debuglog_init(_level: init::LkInitLevel) {
    // SAFETY: `DLOG` is in static storage and remains valid and pinned for the lifetime of the
    // kernel.
    unsafe { dlog().start_threads() }
}

// Replays any records queued in `DLOG` prior to `persistent_dlog_heap` activating
// persistent RAM logging at `LK_INIT_LEVEL_HEAP`.
init::lk_init_hook!(
    debuglog_persist_early,
    debuglog_persist_early_init,
    init::LkInitLevel(init::LK_INIT_LEVEL_HEAP.0 + 1)
);
init::lk_init_hook!(debuglog, debuglog_init, init::LK_INIT_LEVEL_PLATFORM);

// C++ FFI trampoline.
// TODO(https://fxbug.dev/576429701): Remove this FFI entry point once `top/main.rs` calls
// `crate::debuglog_rs::dlog_init_early()` directly instead of `extern "C" { fn dlog_init_early(); }`.

/// FFI entry point for `dlog_init_early()`.
///
/// # Safety
///
/// Must be called at most once during single-threaded early boot before any other `dlog_*`
/// functions are called.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_dlog_init_early() {
    // SAFETY: Upheld by the caller.
    unsafe { dlog_init_early() };
}

#[cfg(ktest)]
/// Debuglog test
#[unittest::suite(name = "debuglog_tests")]
mod tests {
    use crate::kconcurrent::{ChainLock, IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard};
    use zx_types::ZX_KOID_INVALID;

    struct DLogOutputTestBuffer {
        /// Buffer to hold last rendered log - size = dlog max data + additional buffer for log
        /// header.
        output_log_buffer: [u8; RENDERED_LOG_BUF_SIZE],
        last_output_log_len: usize,
        prev_output_log_buffer: [u8; 64],
        prev_output_log_len: usize,
    }

    #[guarded]
    #[pin_data]
    struct DLogOutputTest {
        #[pin]
        log: DLog,
        #[pin]
        output: AutounsignalEvent,
        #[mutex]
        output_lock: KMutex<TrackedMutex>,
        #[guarded_by(output_lock)]
        buffer: DLogOutputTestBuffer,
    }

    impl DLogOutputTest {
        fn new() -> Result<Pin<kalloc::Box<Self>>, Status> {
            let instance = kalloc::Box::try_pin_init(pin_init!(Self {
                log <- DLog::new(),
                output <- AutounsignalEvent::init(false),
                output_lock <- KMutex::init(),
                buffer: KCell::new(DLogOutputTestBuffer {
                    output_log_buffer: [0u8; RENDERED_LOG_BUF_SIZE],
                    last_output_log_len: 0,
                    prev_output_log_buffer: [0u8; 64],
                    prev_output_log_len: 0,
                }),
            }))
            .map_err(|_| Status::NO_MEMORY)?;

            // SAFETY: We do not move out of `instance`; we only extract the raw pointer from
            // `kalloc::Box` and immediately re-pin `instance`.
            let instance = unsafe { Pin::into_inner_unchecked(instance) };
            let self_ptr = kalloc::Box::as_ptr(&instance).cast_const().cast::<()>();
            let instance = unsafe { Pin::new_unchecked(instance) };
            // SAFETY: We only set `output_hook` before starting threads, and `instance` is pinned
            // on the heap and outlives `log`'s threads.
            unsafe {
                *instance.log.output_hook.get() = Some((Self::output_log_message_hook, self_ptr));
            }
            Ok(instance)
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `DLogOutputTest` instance.
        unsafe fn output_log_message_hook(ctx: *const (), log: &[u8]) {
            // SAFETY: `ctx` points to the pinned `DLogOutputTest` instance.
            let this = unsafe { &*ctx.cast::<Self>() };
            this.output_log_message(log);
        }

        fn output_log_message(&self, log: &[u8]) {
            {
                ksync::lock!(let mut output_guard = self.lock_output_lock());
                let buf = output_guard.as_mut().buffer_mut();
                let prev_len = buf.last_output_log_len.min(buf.prev_output_log_buffer.len());
                buf.prev_output_log_buffer[..prev_len]
                    .copy_from_slice(&buf.output_log_buffer[..prev_len]);
                buf.prev_output_log_len = prev_len;
                buf.output_log_buffer[..log.len()].copy_from_slice(log);
                buf.last_output_log_len = log.len();
            }
            self.output.signal();
        }
    }

    /// Write a record to the debuglog and verify its header and payload layout in the buffer.
    #[test]
    fn log_format() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        let msg = b"Hello World\0";

        let _ = log.write(DEBUGLOG_WARNING, 0, msg);

        // log.data[0] is not necessarily aligned as a dlog_header_t would be so copy rather than
        // access them directly.
        let mut header = dlog_header_t::new_zeroed();
        zr::static_assert!(DLOG_SIZE >= size_of::<dlog_header_t>());
        {
            ksync::lock!(let guard = log.lock_lock(ksync::source_tag!()));
            let data = &guard.data().0;
            header.as_mut_bytes().copy_from_slice(&data[..size_of::<dlog_header_t>()]);

            unittest::expect_eq!(DEBUGLOG_WARNING as u8, header.severity);
            unittest::expect_eq!(0, header.flags);
            unittest::expect_eq!(ZX_KOID_INVALID, header.pid);
            unittest::expect_ne!(ZX_KOID_INVALID, header.tid);
            unittest::assert_eq!(12, header.datalen);

            let msg_bytes =
                &data[size_of::<dlog_header_t>()..size_of::<dlog_header_t>() + msg.len()];
            unittest::expect_true!(msg == msg_bytes);
        }
    }

    /// Write records wrapping around the end of the ring buffer and verify payload splitting.
    #[test]
    fn log_wrap() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        let msg = b"Hello World\0";
        // An unaligned length somewhere in the middle of msg.
        const TRUNCATE_TARGET: usize = DLOG_HEADER_FIFO_ALIGNMENT + 1;
        const PAD: usize =
            dlog_align_trunc(DLOG_SIZE - size_of::<dlog_header_t>() - TRUNCATE_TARGET);

        // We need to trigger some writes, but we don't care what we write, so we just need a
        // buffer large enough to be copied from. Doesn't much matter what's in it.
        let ignored_buffer = [0u8; DLOG_MAX_DATA];

        let mut to_write = PAD;
        while to_write != 0 {
            let mut write = to_write - size_of::<dlog_header_t>();
            if write > DLOG_MAX_DATA {
                write = DLOG_MAX_DATA;
            }

            let _ = log.write(DEBUGLOG_WARNING, 0, &ignored_buffer[..write]);
            to_write -= write + size_of::<dlog_header_t>();
        }

        {
            ksync::lock!(let guard = log.lock_lock(ksync::source_tag!()));
            unittest::expect_eq!(PAD, *guard.head());
        }
        let _ = log.write(DEBUGLOG_WARNING, 0, msg);

        // log.data[PAD] is not necessarily aligned as a dlog_header_t would be so copy rather than
        // access them directly.
        let mut header = dlog_header_t::new_zeroed();
        zr::static_assert!(DLOG_SIZE - PAD >= size_of::<dlog_header_t>());
        {
            ksync::lock!(let guard = log.lock_lock(ksync::source_tag!()));
            let data = &guard.data().0;
            header.as_mut_bytes().copy_from_slice(&data[PAD..PAD + size_of::<dlog_header_t>()]);

            unittest::expect_eq!(DEBUGLOG_WARNING as u8, header.severity);
            unittest::expect_eq!(0, header.flags);
            unittest::expect_eq!(ZX_KOID_INVALID, header.pid);
            unittest::expect_ne!(ZX_KOID_INVALID, header.tid);
            unittest::assert_eq!(msg.len() as u16, header.datalen);

            const TRUNCATED_SIZE: usize = DLOG_SIZE - PAD - size_of::<dlog_header_t>();
            let msg_bytes = &data[PAD + size_of::<dlog_header_t>()
                ..PAD + size_of::<dlog_header_t>() + TRUNCATED_SIZE];

            unittest::expect_true!(&msg[..TRUNCATED_SIZE] == msg_bytes);
            unittest::expect_true!(msg[8..] == data[..msg.len() - TRUNCATED_SIZE]);
        }
    }

    /// Read a record from the debuglog and verify its fields.
    #[test]
    fn log_reader_read() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        let now = current_boot_time().0;

        let msg = b"Message!\0";
        unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));

        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        // SAFETY: `log` outlives `reader` and is disconnected before drop.
        unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log)) };
        let _disconnect = zr::defer(|| reader.disconnect());
        let mut rec = dlog_record_t::new_zeroed();
        let res = reader.read(0, &mut rec);
        unittest::assert_ok!(res.map(|_| ()));
        let got = res.unwrap();
        unittest::assert_eq!(size_of::<dlog_header_t>() + msg.len(), got);
        unittest::expect_eq!(0u32, rec.hdr.preamble);
        // Sequence should start at 0.
        unittest::expect_eq!(0u64, rec.hdr.sequence);
        unittest::expect_eq!(msg.len() as u16, rec.hdr.datalen);
        unittest::expect_eq!(DEBUGLOG_WARNING as u8, rec.hdr.severity);
        unittest::expect_eq!(0, rec.hdr.flags);
        unittest::expect_ge!(rec.hdr.timestamp, now);
        unittest::expect_eq!(0u64, rec.hdr.pid);
        // SAFETY: A current thread is always set during test execution.
        let current_tid = unsafe { ThreadPtr::from_raw(current_get()).unwrap().tid() };
        unittest::expect_eq!(current_tid, rec.hdr.tid);
    }

    /// Write to the log, exceeding its capacity and see that data is lost.
    #[test]
    fn log_reader_dataloss() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        // SAFETY: `log` outlives `reader` and is disconnected before drop.
        unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log)) };
        let _disconnect = zr::defer(|| reader.disconnect());

        let msg = b"Hello World\0";

        let mut num_written: u64 = 0;

        let _ = log.write(DEBUGLOG_WARNING, 0, msg);
        num_written += 1;

        let mut rec = dlog_record_t::new_zeroed();
        let res = reader.read(0, &mut rec);
        unittest::assert_ok!(res.map(|_| ()));
        let got = res.unwrap();
        unittest::assert_eq!(size_of::<dlog_header_t>() + msg.len(), got);

        unittest::expect_eq!(DEBUGLOG_WARNING as u8, rec.hdr.severity);
        unittest::expect_eq!(0u32, rec.hdr.preamble);
        unittest::expect_eq!(0, rec.hdr.flags);
        unittest::expect_eq!(ZX_KOID_INVALID, rec.hdr.pid);
        unittest::expect_ne!(ZX_KOID_INVALID, rec.hdr.tid);
        unittest::assert_eq!(msg.len() as u16, rec.hdr.datalen);

        unittest::expect_true!(msg == &rec.data.as_bytes()[..msg.len()]);

        let mut i = 0;
        while i < DLOG_SIZE {
            let _ = log.write(DEBUGLOG_WARNING, 0, msg);
            num_written += 1;
            i += size_of::<dlog_header_t>() + msg.len();
        }

        let mut num_read: u64 = 0;
        let mut expected_sequence: u64 = 0;
        let mut dropped: u64 = 0;
        let mut res = reader.read(0, &mut rec);
        while let Ok(got) = res {
            num_read += 1;
            unittest::assert_eq!(size_of::<dlog_header_t>() + msg.len(), got);
            unittest::expect_eq!(DEBUGLOG_WARNING as u8, rec.hdr.severity);
            unittest::expect_eq!(0u32, rec.hdr.preamble);
            unittest::expect_eq!(0, rec.hdr.flags);
            unittest::expect_eq!(ZX_KOID_INVALID, rec.hdr.pid);
            unittest::expect_ne!(ZX_KOID_INVALID, rec.hdr.tid);
            unittest::assert_eq!(msg.len() as u16, rec.hdr.datalen);
            unittest::expect_true!(msg == &rec.data.as_bytes()[..msg.len()]);
            dropped += rec.hdr.sequence - expected_sequence;
            expected_sequence = rec.hdr.sequence + 1;
            res = reader.read(0, &mut rec);
        }
        unittest::expect_err!(res.map(|_| ()), Status::SHOULD_WAIT);

        // See that we read fewer records than we wrote.
        unittest::expect_lt!(num_read, num_written);

        unittest::expect_eq!(0, rec.hdr.flags);
        unittest::expect_eq!(ZX_KOID_INVALID, rec.hdr.pid);
        unittest::expect_ne!(ZX_KOID_INVALID, rec.hdr.tid);
        unittest::assert_eq!(msg.len() as u16, rec.hdr.datalen);

        unittest::expect_true!(msg == &rec.data.as_bytes()[..msg.len()]);

        // See that all messages are accounted for. That is, the number dropped matches the
        // difference.
        unittest::expect_eq!(dropped, num_written - num_read);
    }

    /// Verify that logs written are output correctly by `DLog::dumper_thread`.
    #[test]
    fn log_dumper_test() {
        let log = DLogOutputTest::new();
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        // Start the dumper thread.
        // SAFETY: `_shutdown` joins the threads before `log` is dropped.
        unsafe { log.log.start_threads() };
        let _shutdown = zr::defer(|| {
            let _ = log.log.shutdown(InstantMono::INFINITE);
        });

        // This is the size of the header.
        let min_output_size = {
            let empty_hdr = dlog_header_t::new_zeroed();
            DLog::format_header(None, &empty_hdr)
        };

        // A small helper which will count the number of occurrences of `msg` in
        // `str_bytes`.
        let count_occurrences = |str_bytes: &[u8], msg: &[u8]| -> usize {
            let mut count = 0;
            let mut offset = min_output_size;
            while let Some(pos) =
                str_bytes.get(offset..).unwrap_or(&[]).windows(msg.len()).position(|w| w == msg)
            {
                count += 1;
                offset += pos + 1;
            }
            count
        };

        // Helper function to verify that a message is output correctly.
        let write_log_and_check_output = unittest::subtest!(|msg: &[u8]| {
            let len = msg.len();
            unittest::assert_ok!(log.log.write(DEBUGLOG_INFO, 0, msg));
            // Wait for the log to get output by `DLog::dumper_thread`.
            unittest::assert_ok!(log.output.wait(&Deadline::infinite()));

            ksync::lock!(let output_guard = log.lock_output_lock());
            let buf = output_guard.buffer();
            let last_output_log = &buf.output_log_buffer[..buf.last_output_log_len];
            // Length of output log is at least header length + message length.
            unittest::expect_ge!(last_output_log.len(), min_output_size + len);
            unittest::assert_gt!(last_output_log.len(), 0usize);
            // Check that the log ends with a newline.
            unittest::expect_eq!(b'\n', last_output_log[last_output_log.len() - 1]);
            // Check that the log contains the message (if not empty) after the header.
            if len != 0 {
                unittest::assert_eq!(1usize, count_occurrences(last_output_log, msg));
            }
        });

        // Check that a simple message appears in the log dump.
        let msg0 = b"Hello World!\n";
        unittest::expect_true!(write_log_and_check_output(msg0));

        // Check that a message without newline appears in the log dump and contains newline.
        let msg1 = b"Hello!";
        unittest::expect_true!(write_log_and_check_output(msg1));

        // Check that a message containing only newline in the log dump.
        let msg2 = b"\n";
        unittest::expect_true!(write_log_and_check_output(msg2));

        // Check that an empty log still gets rendered and contains newline.
        let msg3 = b"";
        unittest::expect_true!(write_log_and_check_output(msg3));

        // Verify that when the ring buffer wraps before the dumper drains it (`gap > 0`),
        // `dumper_thread` emits the `"debuglog: dropped ..."` notice and then formats the next
        // record cleanly without duplicating the dropped notice in the record buffer.
        let drop_log = DLogOutputTest::new();
        unittest::assert_true!(drop_log.is_ok());
        let drop_log = drop_log.unwrap();
        let msg_after_drop = b"After drop\n";
        let record_size = dlog_align(size_of::<dlog_header_t>() + msg_after_drop.len());
        let overflow_writes = (DLOG_SIZE / record_size) + 1;
        for _ in 0..overflow_writes {
            unittest::assert_ok!(drop_log.log.write(DEBUGLOG_INFO, 0, msg_after_drop));
        }
        // Trim `tail` so only the final record remains queued after the sequence gap.
        {
            ksync::lock!(let mut guard = drop_log.log.lock_lock(ksync::source_tag!()));
            let head = *guard.head();
            *guard.as_mut().tail_mut() = head - record_size;
        }
        // SAFETY: `_drop_shutdown` joins the threads before `drop_log` is dropped.
        unsafe { drop_log.log.start_threads() };
        let _drop_shutdown = zr::defer(|| {
            let _ = drop_log.log.shutdown(InstantMono::INFINITE);
        });
        drop_log.log.dumper_state.event.signal();
        while drop_log.log.dumper_sequence.load(Ordering::Acquire) < overflow_writes as u64 {
            let _ = drop_log
                .log
                .dumper_drained_event
                .wait(&Deadline::after_mono(DurationMono(zx_msec(50)), TimerSlack::none()));
        }
        {
            ksync::lock!(let output_guard = drop_log.lock_output_lock());
            let buf = output_guard.buffer();
            let prev_output_log = &buf.prev_output_log_buffer[..buf.prev_output_log_len];
            let last_output_log = &buf.output_log_buffer[..buf.last_output_log_len];
            let dropped_prefix = b"debuglog: dropped ";
            unittest::expect_true!(prev_output_log.starts_with(dropped_prefix));
            unittest::expect_false!(
                last_output_log.windows(dropped_prefix.len()).any(|w| w == dropped_prefix)
            );
            unittest::expect_eq!(1usize, count_occurrences(last_output_log, msg_after_drop));
        }
    }

    /// Render debuglog records into crashlog target buffers of varying capacities.
    #[test]
    fn render_to_crashlog() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        // Define our test message and figure out an expected minimum size for the
        // message when rendered. While we are not 100% sure how large the header
        // for a given rendered record will be, we know that a record with a
        // timestamp, pid, and tid of zero will have the smallest rendered header
        // possible. Note that our test message does not end with a newline, but we
        // expect rendering to add one to each record which does not end with a
        // newline.
        let msg = b"Message!";
        let min_rendered_size = {
            let empty_hdr = dlog_header_t::new_zeroed();
            DLog::format_header(None, &empty_hdr) + msg.len() + 1
        };

        // Allocate two targets for rendering data into, one normal sized, and one
        // small. Using a dedicated small buffer (instead of simply artificially
        // limiting the length of a span wrapped around the large buffer) should
        // help ASAN in detecting any buffer overflows.
        const LARGE_RENDER_BUFFER_SIZE: usize = 1024;
        let large_storage = kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_RENDER_BUFFER_SIZE);
        unittest::assert_true!(large_storage.is_ok());
        let mut large_storage = large_storage.unwrap();
        let large_target: &mut [u8] = &mut large_storage;
        unittest::assert_le!(min_rendered_size, LARGE_RENDER_BUFFER_SIZE);

        const SMALL_RENDER_BUFFER_SIZE: usize = 1;
        let small_storage = kalloc::Box::<[u8]>::try_new_zeroed_slice(SMALL_RENDER_BUFFER_SIZE);
        unittest::assert_true!(small_storage.is_ok());
        let mut small_storage = small_storage.unwrap();
        let small_target: &mut [u8] = &mut small_storage;

        // A small helper which will count the number of occurrences of `msg` in
        // `str_bytes`.
        let count_occurrences = |str_bytes: &[u8], msg: &[u8]| -> usize {
            let mut count = 0;
            let mut offset = 0;
            while let Some(pos) = str_bytes[offset..].windows(msg.len()).position(|w| w == msg) {
                count += 1;
                offset += pos + 1;
            }
            count
        };

        /// A small helper which renders a debuglog into a slice provided, and gives
        /// back a subslice of the result.
        fn do_render<'a>(log: &DLog, target: &'a mut [u8]) -> &'a [u8] {
            let len = log.render_to_crashlog(target);
            &target[..len]
        }

        // Rendering from an empty log should produce nothing.
        let rendered = do_render(&log, large_target);
        unittest::assert_eq!(0usize, rendered.len());

        // Add record to the log, then render the log and verify that it contains at
        // least a minimum number of expected bytes, and exactly one occurrence of
        // the test string in the buffer.
        unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));
        let rendered = do_render(&log, large_target);
        unittest::assert_ge!(rendered.len(), min_rendered_size);
        unittest::assert_eq!(1usize, count_occurrences(rendered, msg));

        // Attempting to render into an empty target from a log with valid records
        // in should produce no data.
        let rendered = do_render(&log, &mut []);
        unittest::assert_eq!(0usize, rendered.len());

        // Attempting to render into target which is non-empty, but does not have
        // enough space to hold a rendered record, should also produce no data.
        let rendered = do_render(&log, small_target);
        unittest::assert_eq!(0usize, rendered.len());

        // Add two more instances of the test message and re-validate
        unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));
        unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));
        let rendered = do_render(&log, large_target);
        unittest::assert_ge!(rendered.len(), min_rendered_size * 3);
        unittest::assert_eq!(3usize, count_occurrences(rendered, msg));

        // Figure out how many instances of our message _should_ fit in the dlog
        // buffer, then add that many, forcing the log to wrap in the process. Note
        // that records are always padded out to a 4 bytes boundary.
        let record_size = (msg.len() + size_of::<dlog_header_t>() + 0x3) & !0x3;
        let record_count = DLOG_SIZE / record_size;
        for _ in 0..record_count {
            unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));
        }

        // It is not clear exactly how many records should fit into our render
        // buffer at this point, but the number should be more than 3.
        let rendered = do_render(&log, large_target);
        unittest::assert_gt!(rendered.len(), min_rendered_size * 3);
        unittest::assert_gt!(count_occurrences(rendered, msg), 3usize);
    }

    // This is a regression test introduced after discovering https://fxbug.dev/42071359. It
    // attempts to make sure that the internal methods `read_record` and
    // `reassemble_from_offset` return proper results, even when headers and payload
    // span the break in the ring buffer.
    /// Read and reassemble records spanning the wrap boundary and reject corrupt headers.
    #[test]
    fn read_record() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        const PAYLOAD_DATA: [u8; 32] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];
        const PAYLOAD_SIZE: usize = PAYLOAD_DATA.len();
        const TOTAL_RECORD_SIZE: usize = size_of::<dlog_header_t>() + PAYLOAD_SIZE;

        const TEST_SEVERITY: u8 = 0xba;
        const TEST_FLAGS: u8 = 0xad;
        // SAFETY: A current thread is always set during test execution.
        let our_tid = unsafe { ThreadPtr::from_raw(current_get()).unwrap().tid() };

        // Payload data must be a multiple of 4 bytes in order to be able to align its end exactly
        // with the end of the ring buffer
        zr::static_assert!(PAYLOAD_SIZE.is_multiple_of(4));

        const OFFSETS: [usize; 4] = [
            0,                                                  // Start of the buffer
            DLOG_SIZE - TOTAL_RECORD_SIZE,                      // Aligned with end
            DLOG_SIZE - TOTAL_RECORD_SIZE + (PAYLOAD_SIZE / 2), // Splitting the payload
            DLOG_SIZE - (size_of::<dlog_header_t>() / 2),       // Splitting the header
        ];

        for offset in OFFSETS {
            ksync::lock!(let mut guard = log.lock_lock(ksync::source_tag!()));

            // Set up the logs ring buffer so that it looks like it is empty, but
            // ready to write the next record starting at the test vector offset.
            // Also, zero out our entire ring buffer just so we are starting from a
            // known state with each vector.
            // Records must start on 4 byte boundaries.
            unittest::assert_true!(offset.is_multiple_of(4));
            unittest::assert_lt!(offset, DLOG_SIZE);
            {
                let mut fields = guard.as_mut().fields_mut();
                *fields.head = offset;
                *fields.tail = offset;
                fields.data.as_mut().get_mut().0.fill(0);
            }

            // Go ahead and write our record, then confirm that head and tail end up
            // where we expect them to. Note that these pointers are always absolute
            // pointers, to compute offsets from them, one needs to & with the
            // DLOG_MASK.
            let mut status = Err(Status::INTERNAL);

            let before_ts = current_boot_time().0;
            guard.as_mut().call_unlocked(|| {
                status = log.write(TEST_SEVERITY as u32, TEST_FLAGS as u32, &PAYLOAD_DATA);
            });
            let after_ts = current_boot_time().0;

            unittest::assert_ok!(status);
            unittest::expect_eq!(offset, *guard.tail());
            unittest::expect_eq!(offset + TOTAL_RECORD_SIZE, *guard.head());

            // Reassemble the header from the offset and make sure it looks right.
            let mut hdr = dlog_header_t::new_zeroed();
            let _ = log.reassemble_from_offset(&guard, offset, hdr.as_mut_bytes());

            unittest::expect_eq!(dlog_hdr_get_fifolen(hdr.preamble), TOTAL_RECORD_SIZE);
            unittest::expect_eq!(dlog_hdr_get_readlen(hdr.preamble), TOTAL_RECORD_SIZE);
            unittest::expect_eq!(PAYLOAD_SIZE as u16, hdr.datalen);
            unittest::expect_ge!(hdr.timestamp, before_ts);
            unittest::expect_le!(hdr.timestamp, after_ts);
            unittest::expect_eq!(TEST_SEVERITY, hdr.severity);
            unittest::expect_eq!(TEST_FLAGS, hdr.flags);
            unittest::expect_eq!(0u64, hdr.pid); // Kernel PID is always 0
            unittest::expect_eq!(our_tid, hdr.tid);
            unittest::expect_eq!(*guard.sequence_count() - 1, hdr.sequence);

            // Now attempt to read the record info at the same offset. We should get
            // the same header base, and regions which exist completely within the
            // ring buffer, and have the test vector pattern in them.
            let maybe_record = log.read_record(&guard, offset, None);
            unittest::assert_true!(maybe_record.is_ok());
            let record = maybe_record.unwrap();
            let log_data = guard.data().0.as_ptr() as usize;

            unittest::expect_true!(hdr.as_bytes() == record.hdr.as_bytes());
            unittest::expect_gt!(record.region1.len(), 0usize);
            unittest::expect_le!(record.region1.len(), DLOG_MAX_RECORD);
            unittest::assert_ge!(record.region1.as_ptr() as usize, log_data);
            unittest::assert_le!(
                record.region1.as_ptr() as usize + record.region1.len(),
                log_data + DLOG_SIZE
            );

            if !record.region2.is_empty() {
                unittest::expect_le!(record.region2.len(), DLOG_MAX_RECORD);
                unittest::assert_ge!(record.region2.as_ptr() as usize, log_data);
                unittest::assert_le!(
                    record.region2.as_ptr() as usize + record.region2.len(),
                    log_data + DLOG_SIZE
                );
            }

            unittest::assert_eq!(PAYLOAD_SIZE, record.region1.len() + record.region2.len());
            unittest::expect_true!(&PAYLOAD_DATA[..record.region1.len()] == record.region1);
            unittest::expect_true!(&PAYLOAD_DATA[record.region1.len()..] == record.region2);
            unittest::expect_false!(record.ends_with_newline);

            // Corrupt the header preamble in the ring buffer across all four header validation
            // predicates (`fifolen != dlog_align(readlen)`, `readlen < hdr_sz`,
            // `readlen > DLOG_MAX_RECORD`, and `(readlen - hdr_sz) != datalen`) and verify that
            // `read_record` rejects each with `Status::INTERNAL`.
            for bad_preamble in [
                dlog_hdr_set(TOTAL_RECORD_SIZE ^ 1, TOTAL_RECORD_SIZE),
                dlog_hdr_set(size_of::<dlog_header_t>() - 4, size_of::<dlog_header_t>() - 4),
                dlog_hdr_set(DLOG_MAX_RECORD + 4, DLOG_MAX_RECORD + 4),
                dlog_hdr_set(TOTAL_RECORD_SIZE + 4, TOTAL_RECORD_SIZE + 4),
            ] {
                let preamble_offset = offset & DLOG_MASK;
                guard.as_mut().fields_mut().data.as_mut().get_mut().0
                    [preamble_offset..preamble_offset + 4]
                    .copy_from_slice(&bad_preamble.to_ne_bytes());
                unittest::expect_err!(
                    log.read_record(&guard, offset, None).map(|_| ()),
                    Status::INTERNAL
                );
            }

            // In non-debug-assertion builds (where `DlogReader::read` does not trigger
            // `debug_assert!(false, ...)` on a corrupt header), verify that `DlogReader::read`
            // also returns `Status::INTERNAL` and advances `tail` to `log_head`.
            #[cfg(not(debug_assertions))]
            guard.as_mut().call_unlocked(|| {
                pin_init::stack_pin_init!(let reader = DlogReader::new());
                let reader = reader.into_ref();
                // SAFETY: `log` outlives `reader` and is disconnected before drop.
                unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log)) };
                let _disconnect = zr::defer(|| reader.disconnect());
                let mut rec = dlog_record_t::new_zeroed();
                unittest::expect_err!(reader.read(0, &mut rec).map(|_| ()), Status::INTERNAL);
                unittest::expect_err!(reader.read(0, &mut rec).map(|_| ()), Status::SHOULD_WAIT);
            });

            // Verify that `DLog::write` snaps `tail` to `head` when discarding an existing tail
            // record whose preamble is corrupt (`fifolen == 0 < size_of::<dlog_header_t>()`).
            {
                let mut fields = guard.as_mut().fields_mut();
                *fields.tail = offset;
                *fields.head = offset + DLOG_SIZE;
                let preamble_offset = offset & DLOG_MASK;
                fields.data.as_mut().get_mut().0[preamble_offset..preamble_offset + 4].fill(0);
            }
            guard.as_mut().call_unlocked(|| {
                status = log.write(TEST_SEVERITY as u32, TEST_FLAGS as u32, &PAYLOAD_DATA);
            });
            unittest::assert_ok!(status);
            unittest::expect_eq!(offset + DLOG_SIZE, *guard.tail());
            unittest::expect_eq!(offset + DLOG_SIZE + TOTAL_RECORD_SIZE, *guard.head());
        }
    }

    /// Test that write fails with an error once the `DLog` has been shutdown.
    #[test]
    fn shutdown() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        // Write one message and see that it succeeds.
        let msg = b"Message!\0";
        unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));

        // Now ask the DLog to shutdown, write another, see that it fails.
        let _ = log.shutdown(InstantMono(0));
        unittest::assert_err!(
            log.write(DEBUGLOG_WARNING, 0, &msg[..msg.len() - 1]),
            Status::BAD_STATE
        );

        // See that there is only one message in the DLog.
        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        let mut rec = dlog_record_t::new_zeroed();
        unittest::assert_err!(reader.read(0, &mut rec).map(|_| ()), Status::BAD_STATE);
        // SAFETY: `log` outlives `reader` and is disconnected before drop.
        unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log)) };
        let _disconnect = zr::defer(|| reader.disconnect());
        let res = reader.read(0, &mut rec);
        unittest::assert_ok!(res.map(|_| ()));
        let got = res.unwrap();
        unittest::assert_eq!(size_of::<dlog_header_t>() + msg.len(), got);
        unittest::assert_err!(reader.read(0, &mut rec).map(|_| ()), Status::SHOULD_WAIT);
        reader.disconnect();
        unittest::assert_err!(reader.read(0, &mut rec).map(|_| ()), Status::BAD_STATE);
        reader.disconnect();
    }

    /// Test that we can call write with an active chainlock transaction.
    #[test]
    fn write_with_active_chainlock_transaction() {
        let log = kalloc::Box::try_pin_init(DLog::new());
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        let msg = b"Message!\0";

        pin_init::stack_pin_init!(let lock = ChainLock::new());
        {
            single_chain_lock_guard!(_guard, IRQ_SAVE_OPTION, &*lock, clt_tag!("DebuglogTests"));
            unittest::assert_ok!(log.write(DEBUGLOG_WARNING, 0, msg));
        }

        let _ = log.shutdown(InstantMono::INFINITE);

        // See that the message is in the DLog.
        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        // SAFETY: `log` outlives `reader` and is disconnected before drop.
        unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log)) };
        let _disconnect = zr::defer(|| reader.disconnect());
        let mut rec = dlog_record_t::new_zeroed();
        let res = reader.read(0, &mut rec);
        unittest::assert_ok!(res.map(|_| ()));
        let got = res.unwrap();
        unittest::assert_eq!(size_of::<dlog_header_t>() + msg.len(), got);
        unittest::assert_err!(reader.read(0, &mut rec).map(|_| ()), Status::SHOULD_WAIT);
    }

    struct DLogPersistTestBuffer {
        persisted_buffer: [u8; RENDERED_LOG_BUF_SIZE],
        last_persisted_len: usize,
        history_buffer: [u8; 4096],
        history_len: usize,
        persist_calls: usize,
    }

    #[guarded]
    #[pin_data]
    struct DLogPersistTest {
        #[pin]
        log: DLog,
        enabled: RelaxedAtomicBool,
        #[mutex]
        persist_lock: KMutex<TrackedMutex>,
        #[guarded_by(persist_lock)]
        buffer: DLogPersistTestBuffer,
    }

    impl DLogPersistTest {
        fn new(defer_persist_until_replay: bool) -> Result<Pin<kalloc::Box<Self>>, Status> {
            let instance = kalloc::Box::try_pin_init(pin_init!(Self {
                log <- DLog::new_with_defer_persist(defer_persist_until_replay),
                enabled: RelaxedAtomicBool::new(true),
                persist_lock <- KMutex::init(),
                buffer: KCell::new(DLogPersistTestBuffer {
                    persisted_buffer: [0u8; RENDERED_LOG_BUF_SIZE],
                    last_persisted_len: 0,
                    history_buffer: [0u8; 4096],
                    history_len: 0,
                    persist_calls: 0,
                }),
            }))
            .map_err(|_| Status::NO_MEMORY)?;

            let self_ptr = core::ptr::from_ref(instance.as_ref().get_ref()).cast::<()>();
            // SAFETY: We only set `persist_hook` before calling any `DLog` methods, and `instance`
            // is pinned on the heap and outlives `instance.log`.
            unsafe {
                *instance.log.persist_hook.get() = Some(PersistHook {
                    is_enabled: Self::is_persist_enabled_hook,
                    persist: Self::persist_log_message_hook,
                    ctx: self_ptr,
                });
            }
            Ok(instance)
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `DLogPersistTest` instance.
        unsafe fn is_persist_enabled_hook(ctx: *const ()) -> bool {
            // SAFETY: `ctx` points to the pinned `DLogPersistTest` instance.
            let this = unsafe { &*ctx.cast::<Self>() };
            this.enabled.load()
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `DLogPersistTest` instance.
        unsafe fn persist_log_message_hook(ctx: *const (), log: &[u8]) {
            // SAFETY: `ctx` points to the pinned `DLogPersistTest` instance.
            let this = unsafe { &*ctx.cast::<Self>() };
            ksync::lock!(let mut guard = this.lock_persist_lock());
            let buf = guard.as_mut().buffer_mut();
            let copy_len = log.len().min(buf.persisted_buffer.len());
            buf.persisted_buffer[..copy_len].copy_from_slice(&log[..copy_len]);
            buf.last_persisted_len = copy_len;

            let avail = buf.history_buffer.len() - buf.history_len;
            let append_len = log.len().min(avail);
            buf.history_buffer[buf.history_len..buf.history_len + append_len]
                .copy_from_slice(&log[..append_len]);
            buf.history_len += append_len;
            buf.persist_calls += 1;
        }
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Verify live persistence, `PersistLog::No` filtering, and early-boot record replay.
    #[test]
    fn persist_log_test() {
        let log = DLogPersistTest::new(false);
        unittest::assert_true!(log.is_ok());
        let log = log.unwrap();

        let empty_hdr = dlog_header_t::new_zeroed();
        let min_hdr_size = DLog::format_header(None, &empty_hdr);

        // Records without a trailing newline get formatted with a header and trailing newline.
        const NO_NEWLINE: &[u8] = b"heartbeat stat_ms=3";
        unittest::assert_ok!(log.log.write(DEBUGLOG_INFO, 0, NO_NEWLINE));
        {
            ksync::lock!(let guard = log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            unittest::expect_eq!(1usize, buf.persist_calls);
            unittest::assert_ge!(last.len(), min_hdr_size + NO_NEWLINE.len() + 1);
            unittest::expect_eq!(b'\n', last[last.len() - 1]);
            unittest::expect_true!(find_subslice(last, NO_NEWLINE).is_some());
        }

        // Records that already end with a newline do not gain a duplicate newline.
        const WITH_NEWLINE: &[u8] = b"kernel line\n";
        unittest::assert_ok!(log.log.write(DEBUGLOG_INFO, 0, WITH_NEWLINE));
        {
            ksync::lock!(let guard = log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            unittest::expect_eq!(2usize, buf.persist_calls);
            unittest::assert_ge!(last.len(), min_hdr_size + WITH_NEWLINE.len());
            unittest::expect_eq!(b'\n', last[last.len() - 1]);
            unittest::expect_ne!(b'\n', last[last.len() - 2]);
        }

        // `PersistLog::No` skips persistent-dlog writes while still enqueuing the record.
        const NO_PERSIST: &[u8] = b"panic buffer line\n";
        unittest::assert_ok!(log.log.write_with_persist(
            DEBUGLOG_INFO,
            0,
            NO_PERSIST,
            PersistLog::No
        ));
        {
            ksync::lock!(let guard = log.lock_persist_lock());
            unittest::expect_eq!(2usize, guard.buffer().persist_calls);
        }

        // Records written before `persist_existing` (including between `LK_INIT_LEVEL_HEAP`
        // activation and `LK_INIT_LEVEL_HEAP + 1` replay) are replayed once in order before live
        // writes resume, while honoring `PersistLog::No` on deferred records.
        let early_log = DLogPersistTest::new(true);
        unittest::assert_true!(early_log.is_ok());
        let early_log = early_log.unwrap();
        early_log.enabled.store(false);
        unittest::assert_ok!(early_log.log.write(DEBUGLOG_INFO, 0, b"early line 1\n"));
        early_log.enabled.store(true);
        unittest::assert_ok!(early_log.log.write_with_persist(
            DEBUGLOG_INFO,
            0,
            b"early no-persist\n",
            PersistLog::No
        ));
        unittest::assert_ok!(early_log.log.write(DEBUGLOG_INFO, 0, b"early line 2"));
        {
            ksync::lock!(let guard = early_log.lock_persist_lock());
            unittest::expect_eq!(0usize, guard.buffer().persist_calls);
        }

        early_log.log.persist_existing();
        {
            ksync::lock!(let guard = early_log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            let history = &buf.history_buffer[..buf.history_len];
            unittest::assert_eq!(2usize, buf.persist_calls);
            unittest::assert_gt!(last.len(), 0usize);
            unittest::expect_eq!(b'\n', last[last.len() - 1]);
            unittest::expect_true!(find_subslice(last, b"early line 2").is_some());
            let pos1 = unittest::unwrap_some!(find_subslice(history, b"early line 1\n"));
            let pos2 = unittest::unwrap_some!(find_subslice(history, b"early line 2\n"));
            unittest::expect_lt!(pos1, pos2);
            unittest::expect_true!(find_subslice(history, b"early no-persist").is_none());
        }

        unittest::assert_ok!(early_log.log.write(DEBUGLOG_INFO, 0, b"live line 3\n"));
        {
            ksync::lock!(let guard = early_log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            unittest::assert_eq!(3usize, buf.persist_calls);
            unittest::expect_true!(find_subslice(last, b"live line 3").is_some());
        }

        // `persist_existing` when disabled clears `defer_persist_until_replay` without persisting.
        let disabled_log = DLogPersistTest::new(true);
        unittest::assert_true!(disabled_log.is_ok());
        let disabled_log = disabled_log.unwrap();
        disabled_log.enabled.store(false);
        unittest::assert_ok!(disabled_log.log.write(DEBUGLOG_INFO, 0, b"disabled early\n"));
        disabled_log.log.persist_existing();
        {
            ksync::lock!(let guard = disabled_log.lock_persist_lock());
            unittest::expect_eq!(0usize, guard.buffer().persist_calls);
        }
        disabled_log.enabled.store(true);
        unittest::assert_ok!(disabled_log.log.write(DEBUGLOG_INFO, 0, b"enabled after replay\n"));
        {
            ksync::lock!(let guard = disabled_log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            unittest::assert_eq!(1usize, buf.persist_calls);
            unittest::expect_true!(find_subslice(last, b"enabled after replay\n").is_some());
        }

        // `persist_existing` reassembles records whose payload wraps across the end of `data`.
        let wrap_log = DLogPersistTest::new(true);
        unittest::assert_true!(wrap_log.is_ok());
        let wrap_log = wrap_log.unwrap();
        const WRAPPED_PAYLOAD: &[u8] = b"0123456789abcdef";
        {
            ksync::lock!(let mut guard = wrap_log.log.lock_lock(ksync::source_tag!()));
            let fields = guard.as_mut().fields_mut();
            let wrap_start = DLOG_SIZE - size_of::<dlog_header_t>() - 8;
            *fields.head = wrap_start;
            *fields.tail = wrap_start;
        }
        unittest::assert_ok!(wrap_log.log.write(DEBUGLOG_INFO, 0, WRAPPED_PAYLOAD));
        wrap_log.log.persist_existing();
        {
            ksync::lock!(let guard = wrap_log.lock_persist_lock());
            let buf = guard.buffer();
            let last = &buf.persisted_buffer[..buf.last_persisted_len];
            unittest::assert_eq!(1usize, buf.persist_calls);
            unittest::expect_true!(find_subslice(last, b"0123456789abcdef\n").is_some());
        }

        pin_init::stack_pin_init!(let reader = DlogReader::new());
        let reader = reader.into_ref();
        // SAFETY: `log` outlives `reader` and is disconnected before drop.
        unsafe { reader.initialize(None, core::ptr::null_mut(), Some(&log.log)) };
        let _disconnect = zr::defer(|| reader.disconnect());
        let mut rec = dlog_record_t::new_zeroed();
        unittest::assert_ok!(reader.read(0, &mut rec).map(|_| ()));
        unittest::assert_ok!(reader.read(0, &mut rec).map(|_| ()));
        unittest::assert_ok!(reader.read(0, &mut rec).map(|_| ()));
        unittest::expect_eq!(0u32, rec.hdr.preamble);
        unittest::expect_eq!(NO_PERSIST.len() as u16, rec.hdr.datalen);
        unittest::assert_err!(reader.read(0, &mut rec).map(|_| ()), Status::SHOULD_WAIT);
    }
}
