// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
// Copyright (c) 2012 Shantanu Gupta
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel mutex support, mirroring the C++ `Mutex` and `CriticalMutex` declared in
//! `<kernel/mutex.h>`, plus the `ksync` raw locks built on them ([`TrackedMutex`] and
//! [`TrackedCriticalMutex`]) and their lock policies, which are re-exported from
//! `crate::ksync_kernel`.
//!
//! The C++ `Mutex` and `CriticalMutex` are facades over opaque storage holding the Rust `Mutex`
//! and `CriticalMutex` in place. All of their methods, including their constructors and
//! destructors, forward to the `rust_*mutex_*` functions at the bottom of this file.

use affine::Round;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::{MaybeUninit, align_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use ksync::{
    AdoptableLockPolicy, ExclusiveLockPolicy, LockPolicy, LockdepTracked, RawAssertHeldLock,
    RawLock, RawUntrackedLock, RawUntrackedStatefulLock, impl_lockdep_tracked_lock_policy,
};
use mutex_bindings as bindings;
use pin_init::{PinInit, pin_data, pin_init, pin_init_from_closure, pinned_drop};
use zr::static_assert;
use zx_types::{ZX_KOID_INVALID, zx_duration_mono_t, zx_status_t};

use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{blocking_disallowed, curr_cpu_num};
use crate::kconcurrent::{
    ChainLockTransaction, ChainLockTransactionToken, ChainLocked, IRQ_SAVE_OPTION, OwnerGuard,
    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, TransactionResult, clt_tag,
};
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
use crate::kernel::deadline::Deadline;
use crate::kernel::lazy_owned_wait_queue::LazyOwnedWaitQueue;
use crate::kernel::lock_trace::{lock_trace_duration, lock_trace_flow_begin, lock_trace_flow_end};
use crate::kernel::owned_wait_queue::{OwnedWaitQueue, ThreadUnblockList, WakeOption};
use crate::kernel::owned_wait_queue_pool::{OwnedWaitQueuePool, OwnedWaitQueuePoolLock};
use crate::kernel::spin_tracing::{
    EncodedLockId, FinishType, IdStringRef, MUTEX_NAME_TRACING_ENABLED, MutexNameStorage,
    SCHEDULER_LOCK_SPIN_TRACING_ENABLED, SpinTracingTimestamp, Tracer,
};
use crate::kernel::task_runtime_timers::ContentionTimer;
use crate::kernel::thread::{self, PreemptDisabledToken, Thread, ThreadPtr};
use crate::kernel::types::{INVALID_CPU, cpu_num_t, cpu_num_to_mask};
use crate::kernel::wait_queue::{ForceInheritance, Interruptible, ResourceOwnership};
use crate::ksync_kernel::NoIrqSavePolicy;
use crate::ktrace_rs::{
    ArgValue, Argument, Context, EventType, InstantBootTicks, InternedString, KTrace,
    resolve_category, resolve_string,
};
use crate::platform_rs::timer::{
    DurationMono, InstantMonoTicks, timer_current_mono_ticks, timer_get_ticks_to_time_ratio,
};
use crate::top::debug::get_frame;

/// Magic value identifying a [`Mutex`] (`'mutx'`).
const MAGIC: u32 = fbl::magic(b"mutx");
/// The `val` address of an unowned [`Mutex`].
const STATE_FREE: usize = 0;
/// Flag bit in `val` indicating that at least one thread is blocked waiting on the [`Mutex`].
const STATE_FLAG_CONTESTED: usize = 1;

/// Whether `LK_DEBUGLEVEL > 0`.
const LK_DEBUGLEVEL_GT_0: bool = !cfg!(lk_debuglevel = "0");

/// The spin tracer used while contending a [`Mutex`].
type SpinTracer = Tracer<SCHEDULER_LOCK_SPIN_TRACING_ENABLED>;
/// The spin tracing timestamp type matching [`SpinTracer`].
type SpinTimestamp = SpinTracingTimestamp<SCHEDULER_LOCK_SPIN_TRACING_ENABLED>;

/// How much kernel mutex tracing [`KTracer`] does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KernelMutexTracingLevel {
    /// No tracing is ever done.  All code drops out at compile time.
    None,
    /// Trace events are only generated when mutexes are contested.
    #[allow(dead_code, reason = "selected by editing KERNEL_MUTEX_TRACING_LEVEL")]
    Contested,
    /// Trace events are generated for all mutex interactions.
    #[allow(
        dead_code,
        reason = "unsupported while the uncontended release fast path is untraced; see below"
    )]
    All,
}

/// By default, kernel mutex tracing is disabled.
const KERNEL_MUTEX_TRACING_LEVEL: KernelMutexTracingLevel = KernelMutexTracingLevel::None;

// The uncontended release fast path in `Mutex::release` does not emit trace events.
// Tracing all mutex interactions requires adding uncontended release tracing there.
static_assert!(!matches!(KERNEL_MUTEX_TRACING_LEVEL, KernelMutexTracingLevel::All));

/// The duration, in ticks, given to the synthetic complete events emitted by [`KTracer`].
const KERNEL_MUTEX_TRACE_EVENT_DURATION_TICKS: i64 = 50;

/// Emits kernel mutex trace events according to [`KERNEL_MUTEX_TRACING_LEVEL`].
struct KTracer {
    /// The timestamp at which this tracer was created. Unused (zero) when tracing is disabled.
    ts: InstantBootTicks,
}

impl KTracer {
    /// Creates a tracer, capturing the current timestamp if tracing is enabled.
    #[inline(always)]
    fn new() -> Self {
        let ts = if KERNEL_MUTEX_TRACING_LEVEL == KernelMutexTracingLevel::None {
            InstantBootTicks(0)
        } else {
            KTrace::timestamp()
        };
        Self { ts }
    }

    #[inline(always)]
    fn kernel_mutex_uncontested_acquire(&self, mutex_id: *const c_void) {
        if KERNEL_MUTEX_TRACING_LEVEL == KernelMutexTracingLevel::All {
            self.kernel_mutex_trace(resolve_string!("mutex_acquire"), mutex_id, None, 0);
        }
    }

    #[inline(always)]
    fn kernel_mutex_block(
        &self,
        mutex_id: *const c_void,
        blocker: Option<&Thread>,
        waiter_count: u32,
    ) {
        if KERNEL_MUTEX_TRACING_LEVEL != KernelMutexTracingLevel::None {
            self.kernel_mutex_trace(
                resolve_string!("mutex_block"),
                mutex_id,
                blocker,
                waiter_count,
            );
        }
    }

    #[inline(always)]
    fn kernel_mutex_wake(
        &self,
        mutex_id: *const c_void,
        new_owner: Option<&Thread>,
        waiter_count: u32,
    ) {
        if KERNEL_MUTEX_TRACING_LEVEL != KernelMutexTracingLevel::None {
            self.kernel_mutex_trace(
                resolve_string!("mutex_release"),
                mutex_id,
                new_owner,
                waiter_count,
            );
        }
    }

    /// Emits a `kernel:sched` complete event named `event_name` for the mutex `mutex_id`.
    fn kernel_mutex_trace(
        &self,
        event_name: &'static InternedString,
        mutex_id: *const c_void,
        thread: Option<&Thread>,
        waiter_count: u32,
    ) {
        let category = resolve_category!("kernel:sched");
        if KTrace::category_enabled(category) {
            let is_kernel_thread = |thread: &Thread| {
                // SAFETY: `thread` is a live `Thread`.
                unsafe { ThreadPtr::from(thread).user_thread_raw() }.is_null()
            };
            let tid_type = match thread {
                None => resolve_string!("none"),
                Some(thread) if is_kernel_thread(thread) => resolve_string!("kernel_mode"),
                Some(_) => resolve_string!("user_mode"),
            };

            let mutex_id_arg =
                Argument::new(resolve_string!("mutex_id"), ArgValue::Pointer(mutex_id.addr()));
            let tid_name_arg = Argument::new(
                resolve_string!("tid"),
                ArgValue::Koid(thread.map_or(ZX_KOID_INVALID, |thread| thread.tid())),
            );
            let tid_type_arg = Argument::new(resolve_string!("tid_type"), tid_type);
            let wait_count_arg = Argument::new(resolve_string!("waiter_count"), waiter_count);

            KTrace::emit_event(
                EventType::DurationComplete,
                category,
                event_name,
                self.ts,
                Context::Thread,
                Some((self.ts.0 + KERNEL_MUTEX_TRACE_EVENT_DURATION_TICKS) as u64),
                &[mutex_id_arg, tid_name_arg, tid_type_arg, wait_count_arg],
                &[],
            );
        }
    }
}

/// `TimesliceExtension` is used to control whether a timeslice extension will be
/// set and if so, what value will be used.
trait TimesliceExtension: Copy {
    /// Whether a timeslice extension will be set.
    const ENABLED: bool;

    /// The timeslice extension to set. Only meaningful when `ENABLED` is true.
    fn value(self) -> DurationMono;
}

/// No timeslice extension.
#[derive(Clone, Copy)]
struct NoTimesliceExtension;

impl TimesliceExtension for NoTimesliceExtension {
    const ENABLED: bool = false;

    #[inline(always)]
    fn value(self) -> DurationMono {
        DurationMono(0)
    }
}

/// A timeslice extension of `value` nanoseconds.
#[derive(Clone, Copy)]
struct WithTimesliceExtension {
    value: DurationMono,
}

impl TimesliceExtension for WithTimesliceExtension {
    const ENABLED: bool = true;

    #[inline(always)]
    fn value(self) -> DurationMono {
        self.value
    }
}

/// Kernel mutex support.
///
/// The C++ `Mutex` is a facade over opaque storage holding a Rust `Mutex` in place, whose size
/// and alignment are asserted against `mutex_bindings`.
///
/// Every byte of a `Mutex` is interior-mutable (atomics or `UnsafeCell`, with no padding) so
/// `&Mutex` carries no `Freeze` bytes. Once a releasing thread clears `val` (or detaches and
/// unlocks the contended [`OwnedWaitQueue`] in [`Mutex::release_contended_mutex`]), another CPU
/// may acquire, release, and destroy the `Mutex` (zeroing `magic` in `~Canary`) before the
/// releasing `&Mutex` call frame returns. Do not add a `Freeze` field. Methods must not hold a
/// `&Mutex` across a point where a racing destructor may run; see the handshake between
/// [`Mutex::destroy`] and the end of [`Mutex::release_contended_mutex`], where the destructor
/// waits for the releasing thread to drop the inner [`OwnedWaitQueue`] lock.
///
/// Instances are constructed, acquired, released and destroyed both through the C++ `Mutex` API
/// and, by Rust `ksync::KMutex`, through the Rust API below, which is covered by the unit tests in
/// this file, the `ksync` kernel tests, and `mutex_spin_time_tests.rs`.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Mutex {
    /// Storage for the lock name and encoded lock ID used by lock tracing, which is zero-sized
    /// when `MUTEX_NAME_TRACING_ENABLED` is false. Wrapped in `UnsafeCell` so `Mutex` has no
    /// `Freeze` bytes.
    name_storage: UnsafeCell<MutexNameStorage>,

    /// Structure guard (`'mutx'` while live, zeroed on drop). Wrapped in `UnsafeCell` so
    /// tearing down on another CPU cannot conflict with an unwinding `&Mutex` borrow.
    magic: UnsafeCell<fbl::Canary<MAGIC>>,

    /// See the comment near the start of [`Mutex::acquire_contended_mutex`] for details about
    /// this variable, and how it affects the spin phase behavior of
    /// [`Mutex::acquire_contended_mutex`].
    maybe_acquired_on_cpu: AtomicU32,
    /// Holds the owning `Thread` pointer bitwise-ORed with [`STATE_FLAG_CONTESTED`], or null
    /// ([`STATE_FREE`]) when unowned. Typed as `AtomicPtr<Thread>` to preserve pointer provenance.
    ///
    /// # Safety Invariant
    ///
    /// Whenever `val & !STATE_FLAG_CONTESTED` is non-null, it points to the live `Thread` that
    /// currently owns this mutex (which remains live for at least as long as it holds the mutex).
    val: AtomicPtr<Thread>,
    #[pin]
    wait: LazyOwnedWaitQueue,
}

// SAFETY: `Mutex` has no thread-local ownership invariants and is `!Unpin` (`wait` is), so
// ownership of a pinned `Mutex` can be transferred across threads.
unsafe impl Send for Mutex {}
// SAFETY: Shared accesses through `&Mutex` operate on the `maybe_acquired_on_cpu`, `val`, and
// `wait` atomics, or perform volatile reads of `magic` while the mutex is live. `name_storage` is
// only mutated during initialization / single-threaded lock-class setup before concurrent use, and
// `magic` is only mutated during teardown after the mutex is unlocked and any contended
// `OwnedWaitQueue` lock is released; wrapping both in `UnsafeCell` ensures `&Mutex` has no
// `Freeze` bytes if another CPU destroys the mutex before a releasing `&Mutex` call returns.
unsafe impl Sync for Mutex {}

impl Mutex {
    /// The maximum duration to spin before falling back to blocking.
    // TODO(https://fxbug.dev/42109976): Decide how to make this configurable per device/platform
    // and describe how to optimize this value.
    pub const SPIN_MAX_DURATION: DurationMono = DurationMono::from_micros(150);

    /// A constant for users of `AutoExpiringPreemptDisabler` to select a value
    /// consistent with the defaults for [`Mutex`]/[`CriticalMutex`]. Using this constant
    /// also improves readability, providing a link to the motivating use case for
    /// timeslice extension.
    pub const DEFAULT_TIMESLICE_EXTENSION: DurationMono = Self::SPIN_MAX_DURATION;

    /// Creates an unlocked `Mutex` with no lock class name.
    ///
    /// When mutex name tracing is enabled, this assigns the mutex a unique lock ID. This is why
    /// `Mutex` cannot be constructed in a `const` context: statically allocated mutexes must
    /// instead be initialized at runtime (for example, in a `LazyInit`).
    #[inline]
    pub fn new() -> Self {
        Self::with_name_storage(MutexNameStorage::<MUTEX_NAME_TRACING_ENABLED>::new())
    }

    /// Creates an unlocked `Mutex` with the lock class name `name`.
    ///
    /// When mutex name tracing is enabled, this assigns the mutex a unique lock ID and records
    /// `name` as its lock class name; otherwise `name` is ignored.
    #[inline]
    pub fn new_with_name(name: &'static InternedString) -> Self {
        Self::with_name_storage(MutexNameStorage::<MUTEX_NAME_TRACING_ENABLED>::with_class_name(
            name,
        ))
    }

    /// Creates an unlocked `Mutex` with the given lock name storage.
    #[inline]
    fn with_name_storage(name_storage: MutexNameStorage) -> Self {
        Self {
            name_storage: UnsafeCell::new(name_storage),
            magic: UnsafeCell::new(fbl::Canary::new()),
            maybe_acquired_on_cpu: AtomicU32::new(INVALID_CPU),
            val: AtomicPtr::new(ptr::null_mut()),
            wait: LazyOwnedWaitQueue::new(),
        }
    }

    /// Records the lock class name of `lcid` for mutex name tracing (when enabled).
    ///
    /// # Safety
    ///
    /// - For the duration of this call, the caller must ensure no other reference or concurrent
    ///   access to this mutex's `name_storage` exists (for example, during single-threaded lock
    ///   initialization before the mutex is shared).
    /// - `lcid` must satisfy all preconditions of [`MutexNameStorage::set_lock_class_id`].
    #[inline]
    pub unsafe fn set_lock_class_id(&self, lcid: *const c_void) {
        // SAFETY:
        // - `self.name_storage.get()` is non-null, properly aligned, and points to an initialized
        //   `MutexNameStorage`, and the caller guarantees exclusive access to `name_storage` for
        //   the duration of this call, so forming `&mut *self.name_storage.get()` is valid.
        // - The caller guarantees `lcid` satisfies `MutexNameStorage::set_lock_class_id`.
        unsafe { (&mut *self.name_storage.get()).set_lock_class_id(lcid) };
    }

    /// Acquire the mutex, spinning for at most `spin_max_duration` before blocking.
    ///
    /// # Safety
    ///
    /// The caller must be in blockable thread context (so the current thread is live while it
    /// runs), hold no spinlocks, have no `ChainLockTransaction` active on the current CPU, and not
    /// already hold this mutex.
    #[inline(always)]
    pub unsafe fn acquire(&self, spin_max_duration: DurationMono) {
        // SAFETY: Forwarded from the caller.
        unsafe { self.acquire_common(spin_max_duration, NoTimesliceExtension) };
    }

    /// Release the mutex. Must be held by the current thread.
    ///
    /// Uncontended releases are not traced (see the static assertion on
    /// [`KERNEL_MUTEX_TRACING_LEVEL`]).
    ///
    /// Takes a `NonNull` rather than `&self` because, once the release is published, another
    /// thread may acquire, release and destroy the mutex before this returns. This function holds
    /// no reference to the mutex across the point where the release is published. Callers that
    /// still hold a `&Mutex` across this call rely on `Mutex` having no `Freeze` bytes (see the
    /// type-level documentation); wrappers containing `Freeze` fields must not do so.
    ///
    /// # Safety
    ///
    /// `this` must point to a live `Mutex` held by the current thread. The caller must be in
    /// blockable thread context and have no `ChainLockTransaction` active. The storage of `this`
    /// must not be freed or reused until the mutex's destruction ([`Mutex::destroy`]) has
    /// returned, which waits for a contended release to stop accessing it.
    #[inline(always)]
    pub unsafe fn release(this: NonNull<Self>) {
        let val_ptr = {
            // SAFETY: The caller guarantees `this` is live, and the release has not yet been
            // published. The reference does not outlive this scope.
            let mutex = unsafe { this.as_ref() };
            mutex.magic().assert();
            debug_assert!(!blocking_disallowed());

            // Clear the guess as to which CPU the owner of the mutex is running on.
            //
            // Ordering: `Relaxed`, as `maybe_acquired_on_cpu` is only a best-effort hint for
            // spinners; see the comment near the start of `acquire_contended_mutex`.
            mutex.maybe_acquired_on_cpu.store(INVALID_CPU, Ordering::Relaxed);
            NonNull::from(&mutex.val)
        };

        let current_thread_ptr = thread::current_get();

        // Try the fast path.  Assume that we are locked, but uncontested.
        //
        // Success memory order is release: makes sure the lock state is ordered so the next thread
        // sees the content. Failure memory order is acquire: makes sure we re-observe the state of
        // the mutex before we try to wake up threads and unwind the state.
        //
        // SAFETY: The caller guarantees `this` is live until the release is published. No
        // reference to the whole `Mutex` is formed, and the reference to `val` is a temporary,
        // ending at this compare exchange, which publishes the release.
        let result = unsafe { val_ptr.as_ref() }.compare_exchange(
            Self::state_for_thread(current_thread_ptr),
            ptr::null_mut(),
            Ordering::Release,
            Ordering::Acquire,
        );
        if let Err(old_mutex_state) = result {
            // The mutex is contended.
            core::hint::cold_path();
            // SAFETY: The current thread is live while it runs, and holds the mutex `this`,
            // whose state was observed to be the contested `old_mutex_state`. The remaining
            // requirements are forwarded from the caller.
            unsafe {
                Self::release_contended_mutex(this, current_thread_ptr, old_mutex_state.addr())
            };
        }
        // Otherwise we're done.  Since this mutex was uncontested, we know that we were not
        // receiving any priority pressure from the wait queue, and there is nothing further to do.
    }

    /// Does the current thread hold the mutex?
    #[inline(always)]
    pub fn is_held(&self) -> bool {
        core::ptr::eq(self.holder(), thread::current_get())
    }

    /// Panic unless the given lock is held.
    ///
    /// Can be used when thread safety analysis can't prove you are holding
    /// a lock. The asserts may be optimized away in release builds.
    #[inline(always)]
    pub fn assert_held(&self) {
        debug_assert!(self.is_held());
    }

    /// Is the mutex contested i.e. is at least one thread blocked waiting on it?
    ///
    /// The contested flag does not track threads which are spin-waiting on the
    /// Mutex and have yet to enter a blocking phase.
    #[inline(always)]
    pub fn is_contested(&self) -> bool {
        (self.val().addr() & STATE_FLAG_CONTESTED) != 0
    }

    /// Returns the raw bytes of this mutex, for comparison against a C++ `Mutex` in tests.
    ///
    /// # Safety
    ///
    /// The mutex must not be acquired, released, initialized or destroyed while the returned
    /// slice is live, as every byte of a `Mutex` is interior-mutable.
    #[cfg(any(test, ktest))]
    pub unsafe fn raw_storage_slice(&self) -> &[u8] {
        // SAFETY: `self` is valid for reads of `size_of::<Self>()` bytes, all of which are
        // initialized since `Mutex` has no padding (see the static assertion below). The caller
        // guarantees that the bytes are not mutated while the slice is live.
        unsafe { core::slice::from_raw_parts(ptr::from_ref(self).cast::<u8>(), size_of::<Self>()) }
    }

    /// Returns this mutex's address, used as its identity in trace events.
    #[inline(always)]
    fn id(this: NonNull<Self>) -> *const c_void {
        this.as_ptr().cast_const().cast()
    }

    /// Returns the lock name storage.
    #[inline(always)]
    fn name_storage(&self) -> &MutexNameStorage {
        // SAFETY: `name_storage` is only mutated during initialization / single-threaded
        // lock-class setup, before the mutex is used concurrently.
        unsafe { &*self.name_storage.get() }
    }

    /// Returns the structure guard.
    #[inline(always)]
    fn magic(&self) -> &fbl::Canary<MAGIC> {
        // SAFETY: `magic` is only mutated during teardown, after the mutex is unlocked and no
        // thread uses it any more.
        unsafe { &*self.magic.get() }
    }

    /// Returns the encoded lock ID used by lock spin tracing.
    #[inline(always)]
    pub(crate) fn encoded_lock_id(&self) -> EncodedLockId {
        self.name_storage().encoded_lock_id()
    }

    /// Returns the string reference for this lock's class name.
    #[inline(always)]
    pub(crate) fn class_name_ref(&self) -> IdStringRef {
        self.name_storage().class_name_ref()
    }

    /// Records the CPU the current thread is running on as the best guess as to where the owner
    /// of the mutex is running.
    #[inline(always)]
    fn record_initial_assigned_cpu(&self) {
        // Ordering: `Relaxed`, as `maybe_acquired_on_cpu` is only a best-effort hint for spinners;
        // see the comment near the start of `acquire_contended_mutex`.
        self.maybe_acquired_on_cpu.store(curr_cpu_num(), Ordering::Relaxed);
    }

    /// Returns the raw mutex state, from which [`Mutex::holder`] extracts the holder pointer.
    #[inline(always)]
    fn val(&self) -> *mut Thread {
        // Ordering: `Relaxed`; callers only use the value to compare the holder against the current
        // thread or to test the contested flag, neither of which needs to synchronize.
        self.val.load(Ordering::Relaxed)
    }

    /// Returns the thread encoded in the mutex state `value`, or null.
    #[inline(always)]
    fn holder_from_val(value: *mut Thread) -> *const Thread {
        value.map_addr(|addr| addr & !STATE_FLAG_CONTESTED).cast_const()
    }

    /// Returns the thread currently holding the mutex, or null.
    #[inline(always)]
    fn holder(&self) -> *const Thread {
        Self::holder_from_val(self.val())
    }

    /// Returns the mutex state recording `thread` as the owner.
    #[inline(always)]
    fn state_for_thread(thread: *const Thread) -> *mut Thread {
        thread.cast_mut()
    }

    /// Returns the address of the mutex state recording `thread` as the owner, for comparing
    /// against observed integer states (such as the one passed to
    /// [`Mutex::release_contended_mutex`]).
    #[inline(always)]
    fn held_state_for_thread(thread: *const Thread) -> usize {
        thread.addr()
    }

    /// The `Mutex`-specific part of dropping a `Mutex`, run by its `PinnedDrop` before its
    /// members are dropped.
    ///
    /// # Safety
    ///
    /// The caller must be in thread context. The mutex must be in the process of being destroyed
    /// by the current thread: it must be unlocked, with no threads blocked on or attempting to
    /// acquire it, and no thread may use it after this returns. Other threads may only still be
    /// finishing a release of it (see [`Mutex::release_contended_mutex`]).
    #[inline]
    unsafe fn destroy(&self) {
        self.magic().assert();
        debug_assert!(!blocking_disallowed());

        // Wait until we are absolutely certain (with acquire semantics) that the inner OWQ lock
        // has been fully released before proceeding with destruction. See the comment at the end
        // of release_contended_mutex for more details as to why this is important.
        // Ordering: the `Acquire` load in `peek` synchronizes with the detach at the end of
        // release_contended_mutex, so observing no queue happens-after the releasing thread's
        // last access to this mutex.
        while let Some(queue) = self.wait.peek() {
            core::hint::cold_path();
            // SAFETY: Queues borrowed from the `OwnedWaitQueuePool` are never freed.
            if unsafe { queue.as_ref() }.lock().is_unlocked() {
                // `is_unlocked` is a `Relaxed` load. Pair it with the releasing thread's `Release`
                // unlock of the queue (which follows its detach) so that, as above, destruction
                // happens-after that thread's last access to this mutex.
                core::sync::atomic::fence(Ordering::Acquire);
                break;
            }
            libarch::r#yield();
        }

        if LK_DEBUGLEVEL_GT_0 && self.val().addr() != STATE_FREE {
            // SAFETY: The caller of `destroy` guarantees it runs in thread context.
            unsafe { self.panic_destroy_locked() };
        }

        // Ordering: `Relaxed`; the mutex is being destroyed and no other thread may use it.
        self.val.store(ptr::null_mut(), Ordering::Relaxed);
    }

    /// Panics because the current thread tried to destroy this locked mutex.
    ///
    /// Kept out of line so that the formatting machinery does not inflate the stack frame of
    /// [`Mutex::destroy`].
    ///
    /// # Safety
    ///
    /// The caller must be in thread context (so `thread::current_get()` points to a live
    /// `Thread`).
    #[cold]
    #[inline(never)]
    unsafe fn panic_destroy_locked(&self) -> ! {
        let holder = self.holder();
        // SAFETY: The caller guarantees this runs in thread context, where the current thread is
        // live while it runs.
        let current = unsafe { &*thread::current_get() };
        // SAFETY: Best-effort diagnostics on a fatal path: by the safety invariant on `self.val`,
        // a locked mutex records its holder, which stays live while it holds the mutex.
        let holder_name = unsafe { holder.as_ref() }.map(|holder| holder.name());
        panic!(
            "~Mutex(): thread {:p} ({}) tried to destroy locked mutex {:p}, locked by {:p} ({})",
            current,
            current.name(),
            self,
            holder,
            holder_name.as_ref().map_or("<none>", |name| name.as_str()),
        );
    }

    /// Used by both [`Mutex`] and [`CriticalMutex`].
    ///
    /// If `T::ENABLED` is true, attempt to set the timeslice
    /// extension after acquiring the mutex and return true if the extension was
    /// set.  Otherwise, return false.
    ///
    /// By parametrizing on whether we're going to set a timeslice extension or not
    /// we can shave a few cycles.
    ///
    /// # Safety
    ///
    /// The caller must be in blockable thread context (so the current thread is live while it
    /// runs), hold no spinlocks, have no `ChainLockTransaction` active on the current CPU, and not
    /// already hold this mutex.
    #[inline(always)]
    unsafe fn acquire_common<T: TimesliceExtension>(
        &self,
        spin_max_duration: DurationMono,
        timeslice_extension: T,
    ) -> bool {
        self.magic().assert();
        debug_assert!(!blocking_disallowed());
        debug_assert!(num_spinlocks_held() == 0);

        let current_thread_ptr = thread::current_get();
        let new_mutex_state = Self::state_for_thread(current_thread_ptr);

        {
            // Record whether we set a timeslice extension for later return/rollback. Is unused for
            // when there is no timeslice extension.
            let mut set_extension = false;
            if T::ENABLED {
                // To ensure there is no gap between acquiring the mutex, and setting the timeslice
                // extension, we optimistically set the timeslice extension first. Should acquiring
                // the mutex fail we will roll it back.
                set_extension =
                    thread::preempt_set_timeslice_extension(timeslice_extension.value());
            }

            // Fast path: The mutex is unlocked and uncontested. Try to acquire it immediately.
            //
            // We use the weak form of compare exchange here, which is faster on some
            // architectures (e.g. aarch64). In the rare case it spuriously fails, the slow
            // path will handle it.
            if self
                .val
                .compare_exchange_weak(
                    ptr::null_mut(),
                    new_mutex_state,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                self.record_initial_assigned_cpu();

                // TODO(maniscalco): Is this the right place to put the KTracer?  Seems like
                // it should be the very last thing we do.
                //
                // Don't bother to update the ownership of the wait queue. If another thread
                // attempts to acquire the mutex and discovers it to be already locked, it
                // will take care of updating the wait queue ownership.
                KTracer::new().kernel_mutex_uncontested_acquire(Self::id(NonNull::from(self)));

                return set_extension;
            }
            // The fast path is expected to succeed.
            core::hint::cold_path();

            // Any timeslice extension set above is rolled back by `acquire_contended_mutex`, which
            // keeps the rollback out of this inlined fast path.
            //
            // SAFETY: Forwarded from the caller; `current_thread_ptr` is the current thread, and
            // `set_extension` is true only if a timeslice extension was set above.
            unsafe {
                self.acquire_contended_mutex(
                    spin_max_duration,
                    current_thread_ptr,
                    timeslice_extension,
                    set_extension,
                )
            }
        }
    }

    /// Acquire a lock held by another thread.
    ///
    /// This is a slowpath taken by [`Mutex::acquire_common`] if the mutex is found to already be
    /// held by another thread.
    ///
    /// If `T::ENABLED` is true, attempt to set the timeslice
    /// extension after acquiring the mutex and return true if the extension was
    /// set.  Otherwise, return false.
    ///
    /// If `rollback_extension` is true, the timeslice extension optimistically set by
    /// [`Mutex::acquire_common`] is cleared first.
    ///
    /// This function is deliberately moved out of line from [`Mutex::acquire_common`] to keep the
    /// stack set up, tear down in the [`Mutex::acquire_common`] fastpath small.
    ///
    /// # Safety
    ///
    /// `current_thread_ptr` must be the current thread. The caller must be in blockable thread
    /// context, hold no spinlocks, have no `ChainLockTransaction` active on the current CPU, and
    /// not already hold this mutex. If `rollback_extension` is true, the current thread must have
    /// a timeslice extension which it set itself and has not yet cleared.
    #[inline(never)]
    unsafe fn acquire_contended_mutex<T: TimesliceExtension>(
        &self,
        spin_max_duration: DurationMono,
        current_thread_ptr: *const Thread,
        timeslice_extension: T,
        rollback_extension: bool,
    ) -> bool {
        if T::ENABLED && rollback_extension {
            // Roll back the timeslice extension optimistically set by `acquire_common`.
            //
            // SAFETY: The caller guarantees `current_thread_ptr` is the current thread, which is
            // live while it runs, and `clear_timeslice_extension` is called on the current thread.
            unsafe { (*current_thread_ptr).preemption_state().clear_timeslice_extension() };
        }

        lock_trace_duration!("Mutex::AcquireContended", "name" => self.class_name_ref());

        // It looks like the mutex is most likely contested (at least, it was when we
        // just checked). Enter the adaptive mutex spin phase, where we spin on the
        // mutex hoping that the thread which owns the mutex is running on a different
        // CPU, and will release the mutex shortly.
        //
        // If we manage to acquire the mutex during the spin phase, we can simply
        // exit, having achieved our goal.  Otherwise, there are 3 reasons we may end
        // up terminating the spin phase and dropping into a block operation.
        //
        // 1) We exceed the system's configured |spin_max_duration|.
        // 2) The mutex is marked as CONTESTED, meaning that at least one other thread
        //    has dropped out of its spin phase and blocked on the mutex.
        // 3) We think that there is a reasonable chance that the owner of this mutex
        //    was assigned to the same core that we are running on.
        //
        // Notes about #3:
        //
        // In order to implement this behavior, the Mutex class maintains a variable
        // called |maybe_acquired_on_cpu|.  This is the system's best guess as to
        // which CPU the owner of the mutex may currently be assigned to. The value of
        // the variable is set when a thread successfully acquires the mutex, and
        // cleared when the thread releases the mutex later on.
        //
        // This behavior is best effort; the guess is just a guess and could be wrong
        // for several legitimate reasons.  The owner of the mutex will assign the
        // variable to the value of the CPU is it running on immediately after it
        // successfully mutates the mutex state to indicate that it owns the mutex.
        //
        // A spinning thread my observe:
        // 1) A value of INVALID_CPU, either because of weak memory ordering, or
        //    because the thread was preempted after updating the mutex state, but
        //    before recording the assigned CPU guess.
        // 2) An incorrect value of the assigned CPU, again either because of weak
        //    memory ordering, or because the thread either moved to a different CPU
        //    or blocked after the guess was recorded.
        //
        // So, it is possible to keep spinning when we probably shouldn't, and also
        // possible to drop out of a spin when we might want to stay in it.
        //
        // TODO(https://fxbug.dev/42109976): Optimize cache pressure of spinners and default spin
        // max.

        // SAFETY: The caller guarantees `current_thread_ptr` is the current thread, which is live
        // while it runs.
        let current_thread: &Thread = unsafe { &*current_thread_ptr };
        // Derive the stored state from the raw pointer, as in `acquire_common`, rather than
        // round-tripping through `current_thread`.
        let new_mutex_state = Self::state_for_thread(current_thread_ptr);

        // Make sure that we don't leave this scope with preemption disabled.  If
        // we've got a timeslice extension, we're going to disable preemption while
        // spinning to ensure that we can't get "preempted early" if we end up
        // acquiring the mutex in the spin phase.  However, if a preemption becomes
        // pending while spinning, we'll briefly enable then disable preemption to
        // allow a reschedule.
        let mut preempt_disabler = AutoPreemptDisabler::new_deferred();
        if T::ENABLED {
            preempt_disabler.disable();
        }

        // Remember the last call to current_ticks.
        let mut now_ticks = timer_current_mono_ticks();
        let spin_tracer = SpinTracer::from_raw_ticks(now_ticks.0);

        let time_to_ticks = timer_get_ticks_to_time_ratio().inverse();
        let spin_until_ticks = InstantMonoTicks(affine::clamp_add(
            now_ticks.0,
            time_to_ticks.scale::<{ Round::DOWN }>(spin_max_duration.0),
        ));
        loop {
            // Attempt to acquire the mutex by swapping out "STATE_FREE" for our current thread.
            //
            // We use the strong form of compare exchange here because strong is easier
            // to reason about and we're already in the slow path so any performance
            // advantage of weak is negligible.
            //
            // There is deliberately no `cold_path()` hint on the failure arm: failing is the
            // expected outcome while spinning, and the loop body falls through on it.
            let old_mutex_state = match self.val.compare_exchange(
                ptr::null_mut(),
                new_mutex_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    spin_tracer.finish(FinishType::LockAcquired, self.encoded_lock_id());
                    self.record_initial_assigned_cpu();

                    // Same as above in the fastest path: leave accounting to later contending
                    // threads.
                    KTracer::new().kernel_mutex_uncontested_acquire(Self::id(NonNull::from(self)));

                    if T::ENABLED {
                        return thread::preempt_set_timeslice_extension(
                            timeslice_extension.value(),
                        );
                    }
                    return false;
                }
                Err(old_mutex_state) => old_mutex_state,
            };

            // Stop spinning if the mutex is or becomes contested. All spinners convert
            // to blocking when the first one reaches the max spin duration.
            if (old_mutex_state.addr() & STATE_FLAG_CONTESTED) != 0 {
                break;
            }

            {
                // Stop spinning if it looks like we might be running on the same CPU which
                // was assigned to the owner of the mutex.
                //
                // Note: The accuracy of |current_cpu| depends on whether preemption is
                // currently enabled or not and whether we re-enable it below.
                let current_cpu: cpu_num_t = curr_cpu_num();
                // Ordering: `Relaxed`; `maybe_acquired_on_cpu` is only a best-effort hint (see
                // above).
                if current_cpu == self.maybe_acquired_on_cpu.load(Ordering::Relaxed) {
                    break;
                }

                if T::ENABLED {
                    // If this CPU has a preemption pending, briefly enable then disable
                    // preemption to give this CPU a chance to reschedule.
                    let current_cpu_mask = cpu_num_to_mask(curr_cpu_num());
                    if (current_thread.preemption_state().preempts_pending() & current_cpu_mask)
                        != 0
                    {
                        core::hint::cold_path();
                        // Reenable preemption to trigger a local reschedule and then disable it
                        // again.
                        preempt_disabler.enable();
                        preempt_disabler.disable();
                    }
                }
            }

            // Give the arch a chance to relax the CPU.
            libarch::r#yield();
            now_ticks = timer_current_mono_ticks();
            if now_ticks >= spin_until_ticks {
                break;
            }
        }

        // Capture the end-of-spin timestamp for our spin tracer, but do not finish
        // the event just yet. We don't actually know if we are going to block or not
        // yet; we have one last chance to grab the lock after we obtain a few more
        // spinlocks.  Once we have dropped into the final locks, we should be able to
        // produce our spin-record using the timestamp explicitly recorded here.
        let spin_end_ts = SpinTimestamp::new();

        if LK_DEBUGLEVEL_GT_0 && self.is_held() {
            Self::panic_acquire_already_owned(self, current_thread);
        }

        // SAFETY: The caller guarantees `current_thread` is the current thread.
        let _timer = unsafe { ContentionTimer::new(current_thread, now_ticks) };

        // Blocking in an OwnedWaitQueue requires that preemption be disabled.
        //
        // TODO(johngro): should we find a way to transfer the responsibility for
        // preempt disabling to the CLT so preemption can be properly re-enabled
        // during a back-off and relax event?
        preempt_disabler.disable();
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<bool> {
            // SAFETY: `preempt_disabler` disabled preemption above, and it stays disabled until
            // `preempt_disabler` is dropped after the transaction.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            preempt_disabled_token.assert_held();
            // We contended with someone else, will probably need to block. Acquire an OWQ and then
            // hold its lock while we check if we need to block.
            let queue_ptr: NonNull<OwnedWaitQueue> = self.wait.get();

            // It is possible that at this point the pointer has been detached by another thread
            // in order to recycle the OWQ. We know that the OWQ is not going to be freed (see
            // OwnedWaitQueuePool::shrink) so it is safe to acquire the lock. We will release it
            // below if we lost the race and the OWQ was recycled.
            //
            // SAFETY: Queues borrowed from the `OwnedWaitQueuePool` are never freed, and all of
            // their mutable state is guarded by their lock.
            let queue: &OwnedWaitQueue = unsafe { queue_ptr.as_ref() };
            let mut queue_guard = OwnerGuard::new(queue, token);

            if self.wait.peek() == Some(queue_ptr) {
                // At this point, we know that `queue` is not going to be recycled out from under
                // us as long as we hold the OWQ's lock since `queue` would have been detached
                // before we were able to acquire the OWQ's lock.
            } else {
                core::hint::cold_path();
                // We were racing with a thread that was the last waiter in the middle of releasing
                // the mutex and they recycled the OWQ. Try again.
                queue_guard.release();
                return TransactionResult::BACKOFF;
            }

            // Check if the contested flag is currently set. The contested flag can only be changed
            // whilst the OWQ's lock is held, so we know we aren't racing with anyone here. This
            // is just an optimization and allows us to avoid redundantly doing the atomic OR.
            let mut old_mutex_state = self.val.load(Ordering::Relaxed);
            if (old_mutex_state.addr() & STATE_FLAG_CONTESTED) == 0 {
                core::hint::cold_path();
                // Set the queued flag to indicate that we're blocking.
                //
                // We may find the old state was |STATE_FREE| if we raced with the
                // holder as they dropped the mutex. We use acquire+release semantics
                // here to make sure we do not reorder before the acquisition of the OWQ.
                old_mutex_state = self.val.fetch_or(STATE_FLAG_CONTESTED, Ordering::AcqRel);

                if old_mutex_state.addr() == STATE_FREE {
                    core::hint::cold_path();
                    // Since we set the contested flag we know that there are no waiters and
                    // no one is able to perform fast path acquisition. Therefore we can
                    // just take the mutex, and remove the queued flag.  Relaxed semantic
                    // are fine here, we already observed the state with acquire semantics
                    // during the fetch_or.
                    ChainLockTransaction::finalize(token);
                    self.val.store(new_mutex_state, Ordering::Relaxed);
                    self.record_initial_assigned_cpu();

                    // We were the only waiter and we successfully acquired the mutex so we
                    // don't need the queue anymore. We need to detach the queue from LazyOWQ
                    // before releasing the OWQ's lock, otherwise other threads might try to
                    // block on the queue that we're about to recycle.
                    {
                        ksync::lock!(
                            let mut guard = OwnedWaitQueuePoolLock::lock_policy::<NoIrqSavePolicy>()
                        );
                        // SAFETY: No thread is blocked in the queue: the mutex was `STATE_FREE`
                        // when we set the contested flag, and threads only block after observing
                        // it held. We hold the queue's lock across the detach, so no other thread
                        // holds it having observed the queue attached.
                        let detached = unsafe { self.wait.detach() };
                        // Checked in release builds too: recycling a queue other than the one
                        // whose lock we hold would be unsound.
                        assert!(
                            detached == Some(queue_ptr),
                            "Mutex's OwnedWaitQueue changed while its lock was held"
                        );
                        queue_guard.release();
                        // SAFETY: `queue_ptr` was borrowed from the global pool by `self.wait`,
                        // was just detached from it so no other thread can find it, has no waiters
                        // (we were the only waiter and never blocked) and no owner (ownership is
                        // only assigned while threads are blocked in it), and is in no list.
                        unsafe {
                            OwnedWaitQueuePool::get()
                                .recycle_locked(queue_ptr, guard.as_mut().token_mut());
                        }
                    }

                    spin_tracer.finish_with_timestamp(
                        FinishType::LockAcquired,
                        self.encoded_lock_id(),
                        spin_end_ts,
                    );

                    if T::ENABLED {
                        return TransactionResult::Value(thread::preempt_set_timeslice_extension(
                            timeslice_extension.value(),
                        ));
                    }

                    return TransactionResult::Value(false);
                }
            }

            spin_tracer.finish_with_timestamp(
                FinishType::Blocked,
                self.encoded_lock_id(),
                spin_end_ts,
            );

            // Whether or not we just set the contested flag in our state, this mutex
            // currently has an owner.  Extract the owner and make sure we assign it to
            // our owned wait queue.
            let cur_owner = Self::holder_from_val(old_mutex_state);
            debug_assert!(!cur_owner.is_null());
            debug_assert!(!core::ptr::eq(cur_owner, current_thread));
            // SAFETY: A non-null `cur_owner` holds the mutex, and cannot hand it off while we hold
            // `queue`'s lock, so it stays live until we block below (after which `cur_owner_ref`
            // is not used).
            let cur_owner_ref: Option<&Thread> = unsafe { cur_owner.as_ref() };

            // Attempt to obtain the locks we need to block in the mutex.
            //
            // SAFETY: The transaction is not finalized; `current_thread` is the current, running
            // thread; we hold only `queue`'s lock, so neither `current_thread`'s lock nor any lock
            // in the PI chains of `queue` or `cur_owner` is held. On success the transaction is
            // finalized and the locks released by
            // `block_and_assign_owner_locked` below without intervening mutations.
            let locked = unsafe {
                queue_guard.proof().try_lock_for_baao_operation_locked(
                    current_thread,
                    cur_owner_ref,
                    token,
                )
            };
            let Some((details, mut current_thread_guard)) = locked else {
                core::hint::cold_path();
                // We failed to lock what we needed to lock in order to block.  Drop our
                // queue lock so that we can try again, but do _not_ put the lock state
                // back to the way it was before.  We just observed that the lock was
                // owned by someone, and while doing so we may have set the contested
                // flag.  We want to preserve the invariant: "once the contested flag has
                // been set (while holding the queue lock), only the owner of the lock is
                // allowed to clear the flag".
                queue_guard.release();
                return TransactionResult::BACKOFF;
            };

            // Ok, we are now committed.  Our state has been updated to indicate the
            // proper owner of the mutex, and we hold all of the locks we need in order
            // to block.  Drop into the block operation, releasing all of the locks we
            // hold as we do so.
            ChainLockTransaction::finalize(token);

            // Log the trace data now that we are committed to blocking.
            //
            // SAFETY: `current_thread` is a live `Thread`. `take_next_lock_flow_id` only mutates
            // the thread's `lock_flow_id`, which is an atomic, so it may be called through a
            // pointer derived from a shared reference.
            let flow_id = unsafe { ThreadPtr::from(current_thread).take_next_lock_flow_id() };
            lock_trace_flow_begin!("contend_mutex", flow_id);
            // Gated at the call site so that the waiter count is not computed when mutex tracing
            // is disabled.
            if KERNEL_MUTEX_TRACING_LEVEL != KernelMutexTracingLevel::None {
                KTracer::new().kernel_mutex_block(
                    Self::id(NonNull::from(self)),
                    cur_owner_ref,
                    queue_guard.proof().count() + 1,
                );
            }

            // Block the thread.  This will drop all of the PI related locks we have
            // been holding up until now.
            preempt_disabler.assert_disabled();
            // SAFETY: `current_thread` is the current thread and holds no spinlocks; `queue` is
            // never freed and stays attached to `self.wait` while `current_thread` is blocked in
            // it; `details` came from the successful BAAO lock acquisition above with no
            // intervening mutations, and the transaction is finalized with exactly those locks
            // held.
            let ret = unsafe {
                OwnedWaitQueue::block_and_assign_owner_locked(
                    queue_guard,
                    &mut current_thread_guard.proof(),
                    &const { Deadline::infinite() },
                    &details,
                    ResourceOwnership::Normal,
                    Interruptible::No,
                    ForceInheritance::No,
                    token,
                    &preempt_disabled_token,
                )
            };

            if let Err(ret) = ret {
                // mutexes are not interruptible and cannot time out, so it
                // is illegal to return with any error state.
                Self::panic_acquire_block_failed(
                    ret.into_raw(),
                    self,
                    current_thread,
                    get_frame!(),
                );
            }

            // Someone woke us up, we should be the holder of the mutex now.
            debug_assert!(core::ptr::eq(current_thread, self.holder()));

            // We need to manually drop our current thread's lock.
            // block_and_assign_owner_locked released all of the other required locks for
            // us, and the current thread's lock was actually released when we finally
            // blocked, but it was re-obtained for us when we unblocked and became
            // re-scheduled.
            current_thread_guard.release();

            if T::ENABLED {
                return TransactionResult::Value(thread::preempt_set_timeslice_extension(
                    timeslice_extension.value(),
                ));
            }
            TransactionResult::Value(false)
        };
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Mutex::AcquireContendedMutex"),
            do_transaction,
        )
    }

    /// Panics because `current_thread` tried to acquire the mutex `this` that it already owns.
    ///
    /// This and the other `panic_*` functions are kept out of line and cold so that the formatting
    /// machinery does not inflate the stack frames of the paths that call them.
    #[cold]
    #[inline(never)]
    fn panic_acquire_already_owned(this: &Self, current_thread: &Thread) -> ! {
        panic!(
            "Mutex::Acquire: thread {:p} ({}) tried to acquire mutex {:p} it already owns.",
            current_thread,
            current_thread.name(),
            this,
        );
    }

    /// Panics because blocking `current_thread` on the mutex `this` returned the error `status`.
    #[cold]
    #[inline(never)]
    fn panic_acquire_block_failed(
        status: zx_status_t,
        this: *const Self,
        current_thread: *const Thread,
        frame: *const c_void,
    ) -> ! {
        panic!(
            "Mutex::Acquire: wait queue block returns with error {} m {:p}, thr {:p}, sp {:p}",
            status, this, current_thread, frame,
        );
    }

    /// Release a lock contended by another thread.
    ///
    /// This is the slowpath taken by [`Mutex::release`] when releasing a lock that is being
    /// waited for by another thread.
    ///
    /// This function is deliberately kept out of line to keep the stack set up, tear down in the
    /// inlined release fastpath small.
    ///
    /// # Safety
    ///
    /// - `this` must point to a live `Mutex` held by `current_thread`, the current thread, whose
    ///   state was observed to be `old_mutex_state` (contested). `this` must remain live until
    ///   the mutex's inner `OwnedWaitQueue` lock is released or the queue is detached; it may be
    ///   destroyed (by another thread) after that, even before this function returns.
    /// - The caller must be in blockable thread context, and no `ChainLockTransaction` may be
    ///   active on the current CPU.
    #[inline(never)]
    unsafe fn release_contended_mutex(
        this: NonNull<Self>,
        current_thread: *const Thread,
        old_mutex_state: usize,
    ) {
        lock_trace_duration!(
            "Mutex::ReleaseContended",
            // SAFETY: The caller guarantees `this` is live; only the class name ID is copied out,
            // and only when lock tracing is enabled.
            "name" => unsafe { this.as_ref() }.class_name_ref(),
        );
        let mut default_hooks = OwnedWaitQueue::default_wake_hooks();

        // Lock our OWQ expecting to wake at most 1 thread.
        //
        // Disable preemption to prevent switching to the woken thread inside of
        // wake_threads_locked() if it is assigned to this CPU. If the woken thread is
        // assigned to a different CPU, the thread lock prevents it from observing
        // the inconsistent owner before the correct owner is recorded.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            // Raw pointers to the `wait` and `val` fields. No reference to the whole `Mutex`, whose
            // non-atomic fields a racing destructor may write, is ever formed. References to
            // `wait` and `val` are formed only as temporaries or in narrow scopes ending at their
            // last use: as soon as the queue is detached through `wait_ptr` (or its lock released)
            // the destructor stops waiting and may destroy the mutex, as described at the end of
            // this function.
            //
            // SAFETY: The caller guarantees `this` is live; no reference is formed.
            let (wait_ptr, val_ptr): (*const LazyOwnedWaitQueue, *const AtomicPtr<Thread>) =
                unsafe { (&raw const (*this.as_ptr()).wait, &raw const (*this.as_ptr()).val) };

            // This mutex is contended, so it must have a queue. Checked in release builds too:
            // dereferencing a missing queue would be unsound.
            //
            // SAFETY: The caller guarantees `this` stays live until its queue is detached or the
            // queue's lock is released below; the reference to `wait` is a temporary.
            let queue_ptr =
                unsafe { &*wait_ptr }.peek().expect("contended Mutex has no OwnedWaitQueue");
            // SAFETY: Queues borrowed from the `OwnedWaitQueuePool` are never freed, and all of
            // their mutable state is guarded by their lock.
            let queue: &OwnedWaitQueue = unsafe { queue_ptr.as_ref() };

            pin_init::stack_pin_init!(let wake_me = ThreadUnblockList::init());
            // SAFETY: The transaction is not finalized, no locks are held, and `wake_me` is empty.
            // On success, the transaction is finalized below and `wake_me` processed by
            // `wake_threads_locked` (or found empty) before `queue_guard` is released.
            let locked = unsafe {
                queue.lock_for_wake_operation_or_backoff(
                    1,
                    &mut default_hooks,
                    wake_me.as_mut(),
                    token,
                )
            };
            let Some(mut queue_guard) = locked else {
                core::hint::cold_path();
                return TransactionResult::BACKOFF;
            };

            ChainLockTransaction::finalize(token);

            // Pre-flight checks.  In order to get here, the mutex's state should indicate
            // that we are the owner and that the mutex is currently contested.  This may
            // not actually turn out to be the state (perhaps the thread contesting the
            // mutex has timed out), but once the contested flag has been set, this should
            // be the only place it can be cleared.
            if LK_DEBUGLEVEL_GT_0 {
                let expected_state =
                    Self::held_state_for_thread(current_thread) | STATE_FLAG_CONTESTED;

                if old_mutex_state != expected_state {
                    // SAFETY: `this` is live: the release has not been published and we hold the
                    // queue's lock. The reference to `val` is a temporary.
                    let current_state = unsafe { &*val_ptr }.load(Ordering::Relaxed);
                    // SAFETY: `current_state` was loaded from the live `this.val`, and the caller
                    // guarantees `current_thread` points to the live current thread.
                    unsafe {
                        Self::panic_release_inconsistent_state(
                            this.as_ptr(),
                            &*current_thread,
                            expected_state,
                            old_mutex_state,
                            current_state,
                        );
                    }
                }
            }

            // If there is no one to wake, we should be able to assert that the OWQ has no
            // owner, and we should be able to we can simply set our state to FREE and get
            // out.  Otherwise, we need to change the state of the queue to indicate it is
            // now owned by the thread we are waking, wake the thread, and make sure that
            // it is declared as the owner of the mutex's OWQ.
            //
            // We need to make sure that we set the state before we wake the thread.  As
            // soon as the thread wakes, it is on the hotpath out, and it needs the
            // mutex's state to be updated already in order to perform operations such as
            // releasing it, or asserting that it holds it.
            let tracer = KTracer::new();
            let new_owner: Option<ThreadPtr> = wake_me.front_ptr();
            let (new_mutex_state, still_waiting): (*mut Thread, u32) = if let Some(new_owner) =
                new_owner
            {
                debug_assert!(queue_guard.proof().count() >= 1);
                let still_waiting = queue_guard.proof().count() - 1;
                let mut new_mutex_state = Self::state_for_thread(new_owner.as_ptr());
                if still_waiting != 0 {
                    new_mutex_state = new_mutex_state.map_addr(|addr| addr | STATE_FLAG_CONTESTED);
                }
                (new_mutex_state, still_waiting)
            } else {
                debug_assert!(queue_guard.proof().queue_owner().is_none());
                (ptr::null_mut(), 0)
            };

            // It should be safe to pass `new_owner` to the tracer at this point in time.
            // We have not woken the thread yet, and we hold its lock (as well as its
            // queue's lock) meaning that it cannot successfully be woken (and therefore
            // must still be alive) until after we release the lock.
            tracer.kernel_mutex_wake(
                Self::id(this),
                // SAFETY: As above, `new_owner` is live while we hold its lock.
                new_owner.map(|thread| unsafe { thread.as_ref() }),
                still_waiting,
            );

            // Update our state with release semantics. The compare exchange only compares
            // addresses, so the expected state needs no provenance.
            //
            // SAFETY: `this` is live until the queue is detached or its lock released below. The
            // reference to `val` is a temporary, ending at this compare exchange, which publishes
            // the release.
            if let Err(observed_state) = unsafe { &*val_ptr }.compare_exchange(
                ptr::without_provenance_mut(old_mutex_state),
                new_mutex_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Self::panic_release_bad_state(this.as_ptr(), current_thread, observed_state.addr());
            }

            // Now that our state is updated, actually perform our wake operation if we
            // need to, then drop the queue lock and get out.
            if let Some(new_owner) = new_owner {
                // End the contention flow before the wake, while we still hold `new_owner`'s lock.
                // Once it is woken it may run, release the mutex and exit on another CPU.
                lock_trace_flow_end!(
                    "contend_mutex",
                    // SAFETY: `new_owner` cannot be woken, and so stays live, while we hold its
                    // lock.
                    unsafe { new_owner.lock_flow_id() }
                );

                // SAFETY: `wake_me` was populated by the successful lock-for-wake operation on
                // `queue` in this transaction (with at most one thread) with no intervening
                // modifications, no proof or guard for its thread is live, and the transaction is
                // finalized with preemption disabled.
                let wake_result = unsafe {
                    queue_guard.proof().wake_threads_locked(
                        wake_me.as_mut(),
                        &mut default_hooks,
                        WakeOption::AssignOwner,
                        ForceInheritance::No,
                        token,
                        &preempt_disabled_token,
                    )
                };
                debug_assert!(wake_result.woken == 1);
                if wake_result.still_waiting > 0 {
                    debug_assert!(wake_result.owner == Some(new_owner));
                    debug_assert!(queue_guard.proof().queue_owner() == Some(new_owner));
                } else {
                    debug_assert!(wake_result.owner.is_none());
                    debug_assert!(queue_guard.proof().queue_owner().is_none());
                }
            }

            // Finally release the OWQ's lock and we are finished.  Note: there are a few
            // tricky lifecycle issues here.  Immediately after the compare and exchange
            // strong (above), we have either set our Mutex state to be FREE, or to be
            // owned by the thread that we are about to wake.  Either immediately after we
            // set the state to FREE, or immediately after we wake the thread we assigned
            // ownership to, it is possible for the Mutex to become destroyed, even if
            // external code bounces through a Mutex acquire/release cycle before
            // destroying it (If there is no contention on the lock at that point, other
            // threads will not synchronize on the wait queue's lock before allowing the
            // object to become destroyed).
            //
            // We prevent this potential use after free by spinning in the Mutex's
            // destructor until it observes the inner queue lock as being unlocked state
            // before proceeding with destruction.  This will guarantee that any thread at
            // this point in the code will hold off destruction until it is past the point
            // where it will ever touch the object's memory. Additionally, it guarantees
            // that anything destroying the object is certain to see all of the changes
            // made to the inner OWQ before destruction is allowed to start.
            if still_waiting == 0 {
                // We were the last waiter, so we are now responsible for detaching and recycling
                // the OWQ from the LazyOWQ.
                //
                // We must ensure that no other thread can observe the OWQ as being attached to the
                // LazyOWQ after we've released the OWQ's lock. If they did, they might try to
                // block on it while it is being placed back in the pool.
                //
                // This means:
                //
                // 1. acquire the pool lock while still holding OWQ's lock
                // 2. detach the OWQ from the LazyOWQ
                // 3. release the OWQ's lock
                // 4. recycle the OWQ
                // 5. release the pool lock
                ksync::lock!(
                    let mut guard = OwnedWaitQueuePoolLock::lock_policy::<NoIrqSavePolicy>()
                );

                // SAFETY: No thread is blocked in the queue: `still_waiting == 0`, and
                // `wake_threads_locked` above woke the only waiter, if there was one. We hold the
                // queue's lock across the detach, so no other thread holds it having observed the
                // queue attached. `wait_ptr` points to the live `wait` field (we have not yet
                // detached the queue), and no reference to it outlives the detach.
                let detached = unsafe { LazyOwnedWaitQueue::detach_raw(wait_ptr) };
                // The queue can't have changed from under us in the process of unblocking, since
                // the queue can only change when the last waiter stops contending on the Mutex.
                // Since we're just now unblocking the queue must be the same it was when
                // release_contended_mutex was first called. Checked in release builds too:
                // recycling a queue other than the one whose lock we hold would be unsound.
                assert!(
                    detached == Some(queue_ptr),
                    "Mutex's OwnedWaitQueue changed while its lock was held"
                );

                queue_guard.release();

                // SAFETY: `queue_ptr` was borrowed from the global pool by `wait`, was just
                // detached from it so no other thread can find it, and has no waiters
                // (`still_waiting == 0`). It has no owner and so is in no list: a queue with no
                // thread to wake was already unowned, since ownership is only assigned while
                // threads are blocked in it, and waking the last waiter with
                // `WakeOption::AssignOwner` leaves the queue unowned.
                unsafe {
                    OwnedWaitQueuePool::get().recycle_locked(queue_ptr, guard.as_mut().token_mut());
                }
            } else {
                // Not recycling, just drop the lock.
                queue_guard.release();
            }

            TransactionResult::DONE
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Mutex::ReleaseContendedMutex"),
            do_transaction,
        );
    }

    /// Panics because `current_thread` tried to release the mutex `this`, expecting its state to
    /// be `expected_state` but observing `observed_state`. `current_state` is the state reloaded
    /// from the mutex, used to name the other holder.
    ///
    /// # Safety
    ///
    /// `current_state` must be a state value loaded from a live [`Mutex::val`].
    #[cold]
    #[inline(never)]
    unsafe fn panic_release_inconsistent_state(
        this: *const Self,
        current_thread: &Thread,
        expected_state: usize,
        observed_state: usize,
        current_state: *mut Thread,
    ) -> ! {
        // `observed_state` is passed as an integer by `Mutex::release`, so carries no
        // provenance. Only name the other holder recorded in the reloaded state if it still
        // matches.
        let other_holder = if current_state.addr() == observed_state {
            Self::holder_from_val(current_state)
        } else {
            ptr::null()
        };
        // SAFETY: Best-effort diagnostics on a fatal path: the caller guarantees `current_state`
        // came from a live `Mutex::val`, whose safety invariant guarantees a non-null holder stays
        // live while it holds the mutex.
        let other_name = unsafe { other_holder.as_ref() }.map(|thread| thread.name());
        panic!(
            "Mutex::ReleaseContendedMutex: consistency check failure.  Thread {:p} ({}) tried to \
             release mutex {:p}.  Expected state ({:x}) != observed state ({:x}).  Other holder \
             ({})",
            current_thread,
            current_thread.name(),
            this,
            expected_state,
            observed_state,
            other_name.as_ref().map_or("<none>", |name| name.as_str()),
        );
    }

    /// Panics because publishing the release of the mutex `this` by `current_thread` observed the
    /// unexpected state `observed_state`.
    #[cold]
    #[inline(never)]
    fn panic_release_bad_state(
        this: *const Self,
        current_thread: *const Thread,
        observed_state: usize,
    ) -> ! {
        panic!(
            "bad state ({:x} != {:x}) in mutex release {:p}, current thread {:p}",
            Self::held_state_for_thread(current_thread) | STATE_FLAG_CONTESTED,
            observed_state,
            this,
            current_thread,
        );
    }
}

static_assert!(Mutex::SPIN_MAX_DURATION.into_nanos() == bindings::Mutex_SPIN_MAX_DURATION);
static_assert!(
    Mutex::DEFAULT_TIMESLICE_EXTENSION.into_nanos() == bindings::Mutex_DEFAULT_TIMESLICE_EXTENSION
);

// The low bit of `val` holds `STATE_FLAG_CONTESTED`, so it must always be clear in a `Thread`
// pointer.
static_assert!(align_of::<Thread>() > STATE_FLAG_CONTESTED);

// `maybe_acquired_on_cpu` holds a `cpu_num_t`.
static_assert!(size_of::<AtomicU32>() == size_of::<cpu_num_t>());

// Compile-time layout assertions against the opaque `Mutex` facade type generated by bindgen of
// `<kernel/mutex.h>`, as the C++ `Mutex` stores a Rust `Mutex` in place.
static_assert!(size_of::<Mutex>() == size_of::<bindings::Mutex>());
static_assert!(align_of::<Mutex>() == align_of::<bindings::Mutex>());
// `Mutex` has no padding bytes, so all of its bytes are initialized (see
// `Mutex::raw_storage_slice`).
static_assert!(
    size_of::<Mutex>()
        == size_of::<UnsafeCell<MutexNameStorage>>()
            + size_of::<UnsafeCell<fbl::Canary<MAGIC>>>()
            + size_of::<AtomicU32>()
            + size_of::<AtomicPtr<Thread>>()
            + size_of::<LazyOwnedWaitQueue>()
);

impl Default for Mutex {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[pinned_drop]
impl PinnedDrop for Mutex {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: Whether dropped via Rust ownership or `rust_*mutex_destroy`, every thread that
        // holds, is blocked on, or is attempting to acquire the mutex does so through a borrow or
        // under a contract keeping the mutex alive (`KMutexGuard::from_raw` or C++ `Mutex`
        // lifetime rules), so none remain once it is being dropped, and drops run in thread context
        // (debug-asserted by `destroy`, which also panics when `LK_DEBUGLEVEL > 0` if still
        // locked). Another thread may only still be finishing `release_contended_mutex` via raw
        // pointers, which `destroy` waits for with `Acquire` synchronization before `Unpin` field
        // drop glue (`Canary::drop`) runs; because `Mutex` is `!Unpin`, `Pin<&mut Self>` asserts no
        // uniqueness that conflicts with that thread's accesses.
        unsafe { self.destroy() };
    }
}

/// `CriticalMutex` is a mutex variant that uses a thread timeslice extension to
/// disable preemption for a limited time during the critical section.
///
/// Acquiring a `CriticalMutex` sets a timeslice extension that's cleared when the
/// mutex is released or the extension expires, whichever comes first.
///
/// This variant is useful to avoid a form of thrash where a thread holding a
/// mutex is preempted by another thread that subsequently blocks while trying to
/// acquire the already held mutex.  `CriticalMutex` is designed to mitigate this
/// type of thrash by preventing the holder from being preempted, up to some
/// maximum amount of time.  By limiting maximum amount of time the holder is
/// non-preemptible, we can limit the degree to which other, logically higher
/// priority tasks are delayed from executing.
///
/// Good candidates for `CriticalMutex` are global or widely shared locks that
/// typically, but not necessarily always, have very short critical sections
/// (tens of microseconds or less) and high contention under load.
///
/// `CriticalMutex` differs from `SpinLock` in the following ways:
/// * Threads contending a `CriticalMutex` will block after the spin interval is
///   exceeded, avoiding extended monopolization of multiple CPUs.
/// * Threads may block while holding a `CriticalMutex`, simplifying maintaining
///   invariants in slow paths.
/// * Interrupts may remain enabled while holding a `CriticalMutex`, avoiding
///   undesirable IRQ latency.
///
/// Like the C++ `Mutex`, the C++ `CriticalMutex` is a facade over opaque storage holding a Rust
/// `CriticalMutex` in place.
#[pin_data]
#[repr(transparent)]
pub struct CriticalMutex {
    /// The underlying [`Mutex`].
    #[pin]
    mutex: Mutex,
}

// Compile-time layout assertions against the opaque `CriticalMutex` facade type generated by
// bindgen of `<kernel/mutex.h>`.
static_assert!(size_of::<CriticalMutex>() == size_of::<bindings::CriticalMutex>());
static_assert!(align_of::<CriticalMutex>() == align_of::<bindings::CriticalMutex>());

// `PinnedDrop` and `rust_*mutex_destroy` rely on `Mutex` and `CriticalMutex` being `!Unpin` (via
// `#[pin] wait: LazyOwnedWaitQueue`).
const _: () = {
    trait AmbiguousIfUnpin<A> {
        const VALID: () = ();
    }
    impl<T: ?Sized> AmbiguousIfUnpin<()> for T {}
    impl<T: ?Sized + Unpin> AmbiguousIfUnpin<u8> for T {}
    let () = <Mutex as AmbiguousIfUnpin<_>>::VALID;
    let () = <CriticalMutex as AmbiguousIfUnpin<_>>::VALID;
};

impl CriticalMutex {
    /// Creates an unlocked `CriticalMutex` with no lock class name. See [`Mutex::new`].
    #[inline]
    pub fn new() -> Self {
        Self { mutex: Mutex::new() }
    }

    /// Creates an unlocked `CriticalMutex` with the lock class name `name`. See
    /// [`Mutex::new_with_name`].
    #[inline]
    pub fn new_with_name(name: &'static InternedString) -> Self {
        Self { mutex: Mutex::new_with_name(name) }
    }

    /// Views `mutex` as a `CriticalMutex`.
    ///
    /// A `CriticalMutex` is a `Mutex` that is acquired with a timeslice extension, so the same
    /// `Mutex` may be acquired as either, provided each acquisition is released the same way it
    /// was acquired.
    #[inline(always)]
    fn from_mutex(mutex: &Mutex) -> &Self {
        // SAFETY: `CriticalMutex` is a `repr(transparent)` wrapper around a `Mutex`.
        unsafe { &*ptr::from_ref(mutex).cast::<Self>() }
    }

    /// Acquire the mutex, spinning for at most `spin_max_duration` before blocking.
    ///
    /// Returns whether or not a time slice extension was set, and must be subsequently cleared on
    /// release. More simply, the return value must be passed into [`CriticalMutex::release`].
    ///
    /// # Safety
    ///
    /// As for [`Mutex::acquire`].
    #[must_use]
    #[inline(always)]
    pub unsafe fn acquire(&self, spin_max_duration: DurationMono) -> bool {
        // TODO(maniscalco): What's the right duration here?  Is it a function of
        // spin_max_duration?
        let timeslice_extension = WithTimesliceExtension { value: spin_max_duration };
        // SAFETY: Forwarded from the caller.
        unsafe { self.mutex.acquire_common(spin_max_duration, timeslice_extension) }
    }

    /// Release the mutex. Must be held by the current thread. `should_clear` is the return value
    /// of the matching [`CriticalMutex::acquire`] call.
    ///
    /// As for [`Mutex::release`], this holds no reference to the mutex across the point where the
    /// release is published, and it does not access `*this` after `Mutex::release` returns.
    ///
    /// # Safety
    ///
    /// As for [`Mutex::release`].
    #[inline(always)]
    pub unsafe fn release(this: NonNull<Self>, should_clear: bool) {
        // SAFETY: `CriticalMutex` is a `repr(transparent)` wrapper around a `Mutex`, so `this`
        // points to the underlying `Mutex`. The remaining requirements are forwarded from the
        // caller.
        unsafe { Mutex::release(this.cast::<Mutex>()) };

        if should_clear {
            thread::preempt_clear_timeslice_extension();
        }
    }

    /// See [`Mutex::is_held`].
    #[inline(always)]
    pub fn is_held(&self) -> bool {
        self.mutex.is_held()
    }

    /// See [`Mutex::assert_held`].
    #[inline(always)]
    pub fn assert_held(&self) {
        self.mutex.assert_held();
    }

    /// See [`Mutex::is_contested`].
    #[inline(always)]
    pub fn is_contested(&self) -> bool {
        self.mutex.is_contested()
    }
}

impl Default for CriticalMutex {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `Mutex::acquire` and `Mutex::release` enforce mutual exclusion on `Mutex`.
unsafe impl LockPolicy<Mutex> for Mutex {
    type AcquireArgs = ();
    type GuardState = ();

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel mutex is permitted.
    #[inline]
    unsafe fn acquire(lock: &Mutex, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe { lock.acquire(Mutex::SPIN_MAX_DURATION) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel mutex must be
    /// permitted.
    #[inline]
    unsafe fn reacquire(lock: &Mutex, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { <Self as LockPolicy<Mutex>>::acquire(lock, state, ()) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `state` produced
    /// by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &Mutex, state: &mut Self::GuardState) {
        // SAFETY: The caller upholds the `release_raw` contract: `lock` is live and held by the
        // current thread, and it borrows the mutex for the whole call, so the mutex outlives the
        // release. Holding `lock` across the point where the release is published is sound as
        // `Mutex` has no `Freeze` bytes.
        unsafe { Self::release_raw(NonNull::from(lock), state) };
    }

    /// Holds no reference to `*lock` across the point where the release is published. After
    /// that point another thread may acquire, release and drop the mutex.
    ///
    /// # Safety
    ///
    /// As for `release`, except that `*lock` only needs to remain valid until the release is
    /// published. Its storage must still not be freed or reused without first running the
    /// `Mutex` destructor, which waits for a contended release to stop accessing it.
    #[inline]
    unsafe fn release_raw(lock: NonNull<Mutex>, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees `lock` is live until the release is published and held
        // by the current thread, and the kernel only releases mutexes in blockable thread context
        // with no `ChainLockTransaction` active (as `Mutex::release` debug-asserts).
        // `Mutex::release` holds no reference to the mutex across the point where the release is
        // published, and per the caller its storage is only freed or reused after the `Mutex`
        // destructor, whose `destroy` waits for a contended release to stop accessing it.
        unsafe { Mutex::release(lock) };
    }
}

// SAFETY: `Mutex::acquire` and `Mutex::release` enforce exclusive mutual exclusion on `Mutex`.
unsafe impl ExclusiveLockPolicy<Mutex> for Mutex {}

// SAFETY: `Mutex` has `GuardState = ()` and carries no stack-LIFO CPU or interrupt state, so
// relocating acquisitions and releasing held mutexes in non-LIFO order on the acquiring thread
// preserves all synchronization invariants.
unsafe impl AdoptableLockPolicy<Mutex> for Mutex {}

/// Lock policy for acquiring and releasing a `Mutex`, spinning for at most the given monotonic
/// duration (in nanoseconds) before falling back to blocking.
///
/// This is the equivalent of the C++ `Guard<Mutex> guard{&lock, spin_max_duration}`. For
/// comparison, the default `Mutex` policy spins for at most [`Mutex::SPIN_MAX_DURATION`].
///
/// This policy can be used with [`TrackedMutex`] and an untracked [`Mutex`], but not with
/// [`TrackedCriticalMutex`], as it would acquire the lock as a plain `Mutex` and bypass the
/// [`CriticalMutex`] timeslice extension.
pub struct MutexSpinMaxDurationPolicy;

// SAFETY: `Mutex::acquire` and `Mutex::release` enforce mutual exclusion on `Mutex`.
unsafe impl LockPolicy<Mutex> for MutexSpinMaxDurationPolicy {
    type AcquireArgs = zx_duration_mono_t;
    type GuardState = zx_duration_mono_t;

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel mutex is permitted.
    #[inline]
    unsafe fn acquire(
        lock: &Mutex,
        state: &mut Self::GuardState,
        spin_max_duration: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe { lock.acquire(DurationMono::from_nanos(spin_max_duration)) };
        *state = spin_max_duration;
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel mutex must be
    /// permitted.
    #[inline]
    unsafe fn reacquire(lock: &Mutex, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract, and `*state` holds the
        // spin duration recorded by `acquire`.
        unsafe { Self::acquire(lock, state, *state) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `state` produced
    /// by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &Mutex, state: &mut Self::GuardState) {
        // SAFETY: As for the default `Mutex` policy's `release`.
        unsafe { Self::release_raw(NonNull::from(lock), state) };
    }

    /// Holds no reference to `*lock` across the point where the release is published. After
    /// that point another thread may acquire, release and drop the mutex.
    ///
    /// # Safety
    ///
    /// As for `release`, except that `*lock` only needs to remain valid until the release is
    /// published. Its storage must still not be freed or reused without first running the
    /// `Mutex` destructor, which waits for a contended release to stop accessing it.
    #[inline]
    unsafe fn release_raw(lock: NonNull<Mutex>, _state: &mut Self::GuardState) {
        // SAFETY: A lock acquired by this policy is released the same way as by the default
        // `Mutex` policy, whose `release_raw` justifies this call.
        unsafe { Mutex::release(lock) };
    }
}

// SAFETY: `Mutex::acquire` and `Mutex::release` enforce exclusive mutual exclusion on `Mutex`.
unsafe impl ExclusiveLockPolicy<Mutex> for MutexSpinMaxDurationPolicy {}

// SAFETY: `GuardState` stores only the `zx_duration_mono_t` spin timeout (relocated by value via
// `core::mem::take`) and carries no stack-LIFO CPU or interrupt state.
unsafe impl AdoptableLockPolicy<Mutex> for MutexSpinMaxDurationPolicy {}

// Restricted to `TrackedMutex`, so that `TrackedCriticalMutex` cannot be acquired as a plain
// `Mutex`.
impl_lockdep_tracked_lock_policy!(MutexSpinMaxDurationPolicy, Mutex, Mutex);

// SAFETY: `Mutex` is a physical kernel mutex with a unique storage address.
unsafe impl RawLock for Mutex {
    type DefaultPolicy = Self;

    /// Initializes the mutex in place, assigning it a unique lock ID and the lock class name of
    /// `class_id` for mutex name tracing (when enabled).
    ///
    /// # Safety
    ///
    /// `class_id` must be null or a valid static `LockClassId` that outlives the initialized
    /// mutex; the returned `PinInit` must be used to initialize pinned storage before the mutex is
    /// acquired.
    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY:
        // - `pin_init_from_closure` guarantees that `slot` is valid for writes and properly
        //   aligned for `Self`, so `slot.write(mutex)` is valid and initializes `slot` before the
        //   closure returns `Ok(())`.
        // - The caller guarantees that `class_id` is null or a valid static `LockClassId`, as
        //   `set_lock_class_id` requires.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                let mut mutex = Self::new();
                mutex.name_storage.get_mut().set_lock_class_id(class_id);
                slot.write(mutex);
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        core::ptr::from_ref(self).cast_mut().cast::<c_void>()
    }
}

impl RawAssertHeldLock for Mutex {
    #[inline]
    fn assert_held(&self) {
        Self::assert_held(self);
    }
}

impl RawUntrackedLock for Mutex {
    /// # Safety
    ///
    /// As for [`RawUntrackedLock::release`] and [`Mutex::release`].
    #[inline]
    unsafe fn release(&self) {
        // SAFETY: The caller upholds the `RawUntrackedLock::release` contract: the current thread
        // holds `self`, satisfies the preconditions of `Mutex::release` (blockable thread context
        // with no `ChainLockTransaction` active), and keeps the allocation backing `self` live
        // until this call returns. Holding `self` across the release is sound as `Mutex` has no
        // `Freeze` bytes.
        unsafe { Self::release(NonNull::from(self)) };
    }
}

/// Lock policy for acquiring and releasing a `Mutex` as a [`CriticalMutex`].
pub struct CriticalMutexPolicy;

// SAFETY: `CriticalMutex::acquire` and `CriticalMutex::release` enforce mutual exclusion on the
// `Mutex`.
unsafe impl LockPolicy<Mutex> for CriticalMutexPolicy {
    type AcquireArgs = ();
    type GuardState = bool;

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel critical mutex is permitted.
    #[inline]
    unsafe fn acquire(lock: &Mutex, should_clear: &mut Self::GuardState, _args: ()) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        *should_clear =
            unsafe { CriticalMutex::from_mutex(lock).acquire(Mutex::SPIN_MAX_DURATION) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel critical mutex must
    /// be permitted.
    #[inline]
    unsafe fn reacquire(lock: &Mutex, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { Self::acquire(lock, state, ()) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `should_clear`
    /// produced by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &Mutex, should_clear: &mut Self::GuardState) {
        // SAFETY: As for the default `Mutex` policy's `release`.
        unsafe { Self::release_raw(NonNull::from(lock), should_clear) };
    }

    /// Holds no reference to `*lock` across the point where the release is published. After
    /// that point another thread may acquire, release and drop the mutex.
    ///
    /// # Safety
    ///
    /// As for `release`, except that `*lock` only needs to remain valid until the release is
    /// published. Its storage must still not be freed or reused without first running the
    /// `Mutex` destructor, which waits for a contended release to stop accessing it.
    #[inline]
    unsafe fn release_raw(lock: NonNull<Mutex>, should_clear: &mut Self::GuardState) {
        // SAFETY: As for the default `Mutex` policy's `release_raw`. `CriticalMutex` is a
        // `repr(transparent)` wrapper around a `Mutex`, which was acquired as a `CriticalMutex` by
        // the matching `acquire` or `reacquire`, which returned `should_clear`.
        // `CriticalMutex::release` documents that it does not access the mutex after
        // `Mutex::release` returns.
        unsafe { CriticalMutex::release(lock.cast::<CriticalMutex>(), *should_clear) };
    }
}

// SAFETY: `CriticalMutex::acquire` and `CriticalMutex::release` enforce exclusive mutual exclusion
// on the `Mutex`.
unsafe impl ExclusiveLockPolicy<Mutex> for CriticalMutexPolicy {}

// SAFETY: `GuardState` stores only `should_clear: bool` for the scheduler timeslice-extension
// preemption hint (relocated by value via `core::mem::take`); releasing outer and inner
// `CriticalMutex` acquisitions in non-LIFO order (e.g. hand-over-hand traversal) is a benign
// scheduling hint with no synchronization or interrupt-state hazard.
unsafe impl AdoptableLockPolicy<Mutex> for CriticalMutexPolicy {}

// SAFETY: `CriticalMutex::acquire` and `CriticalMutex::release` enforce mutual exclusion on
// `CriticalMutex`.
unsafe impl LockPolicy<CriticalMutex> for CriticalMutex {
    type AcquireArgs = ();
    type GuardState = bool;

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel critical mutex is permitted.
    #[inline]
    unsafe fn acquire(lock: &CriticalMutex, should_clear: &mut Self::GuardState, _args: ()) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        *should_clear = unsafe { lock.acquire(Mutex::SPIN_MAX_DURATION) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel critical mutex must
    /// be permitted.
    #[inline]
    unsafe fn reacquire(lock: &CriticalMutex, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { <Self as LockPolicy<CriticalMutex>>::acquire(lock, state, ()) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `should_clear`
    /// produced by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &CriticalMutex, should_clear: &mut Self::GuardState) {
        // SAFETY: As for the default `Mutex` policy's `release`. Holding `lock` across the release
        // is sound as `CriticalMutex` is `repr(transparent)` over `Mutex` and has no `Freeze`
        // bytes.
        unsafe { Self::release_raw(NonNull::from(lock), should_clear) };
    }

    /// Holds no reference to `*lock` across the point where the release is published. After
    /// that point another thread may acquire, release and drop the mutex.
    ///
    /// # Safety
    ///
    /// As for `release`, except that `*lock` only needs to remain valid until the release is
    /// published. Its storage must still not be freed or reused without first running the
    /// `Mutex` destructor, which waits for a contended release to stop accessing it.
    #[inline]
    unsafe fn release_raw(lock: NonNull<CriticalMutex>, should_clear: &mut Self::GuardState) {
        // SAFETY: As for the default `Mutex` policy's `release_raw`, with `should_clear` produced
        // by the matching `acquire` or `reacquire`. `CriticalMutex::release` documents that it
        // does not access the mutex after `Mutex::release` returns.
        unsafe { CriticalMutex::release(lock, *should_clear) };
    }
}

// SAFETY: `CriticalMutex::acquire` and `CriticalMutex::release` enforce exclusive mutual exclusion
// on `CriticalMutex`.
unsafe impl ExclusiveLockPolicy<CriticalMutex> for CriticalMutex {}

// SAFETY: As for `AdoptableLockPolicy<Mutex> for CriticalMutexPolicy`.
unsafe impl AdoptableLockPolicy<CriticalMutex> for CriticalMutex {}

// SAFETY: `CriticalMutex` is a physical kernel critical mutex with a unique storage address.
unsafe impl RawLock for CriticalMutex {
    type DefaultPolicy = Self;

    /// # Safety
    ///
    /// `class_id` must be null or a valid static `LockClassId` that outlives the initialized
    /// mutex; the returned `PinInit` must be used to initialize pinned storage before the mutex is
    /// acquired.
    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: Forwarded from the caller.
            mutex <- unsafe { <Mutex as RawLock>::init(class_id) },
        })
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        <Mutex as RawLock>::as_mut_ptr(&self.mutex)
    }
}

impl RawAssertHeldLock for CriticalMutex {
    #[inline]
    fn assert_held(&self) {
        Self::assert_held(self);
    }
}

impl RawUntrackedStatefulLock for CriticalMutex {
    type State = bool;

    /// # Safety
    ///
    /// As for [`RawUntrackedStatefulLock::release_with`] and [`CriticalMutex::release`].
    #[inline]
    unsafe fn release_with(&self, should_clear: Self::State) {
        // SAFETY: The caller upholds the `RawUntrackedStatefulLock::release_with` contract: the
        // current thread holds `self` with `should_clear` from its acquisition, satisfies the
        // preconditions of `CriticalMutex::release` (blockable thread context with no
        // `ChainLockTransaction` active), and keeps the allocation backing `self` live until this
        // call returns. Holding `self` across the release is sound as `CriticalMutex` has no
        // `Freeze` bytes.
        unsafe { Self::release(NonNull::from(self), should_clear) };
    }
}

/// Lockdep-tracked kernel mutex matching the Zircon C++ `Lock<Mutex>` layout.
pub type TrackedMutex = LockdepTracked<Mutex>;

/// Lockdep-tracked kernel critical mutex matching the Zircon C++ `Lock<CriticalMutex>` layout.
pub type TrackedCriticalMutex = LockdepTracked<Mutex, CriticalMutexPolicy>;

// Must match `kExpectedMutexSize` in `zircon/kernel/lib/ksync/bindings.cc`.
const _: () = {
    #[cfg(with_lock_dep)]
    const EXPECTED_SIZE: usize = if MUTEX_NAME_TRACING_ENABLED { 40 } else { 32 };
    #[cfg(not(with_lock_dep))]
    const EXPECTED_SIZE: usize = if MUTEX_NAME_TRACING_ENABLED { 32 } else { 24 };

    assert!(size_of::<TrackedMutex>() == EXPECTED_SIZE);
    assert!(align_of::<TrackedMutex>() == 8);

    assert!(size_of::<TrackedCriticalMutex>() == EXPECTED_SIZE);
    assert!(align_of::<TrackedCriticalMutex>() == 8);
};

/// Lock acquisition policy for [`TrackedCriticalMutex`] with an explicit external lockdep order.
///
/// The mutex's lock class must be registered with
/// [`LOCK_FLAGS_NESTABLE`](ksync::LOCK_FLAGS_NESTABLE), which is checked at compile time. The
/// order is preserved across `call_unlocked` so that the re-acquisition is validated at the same
/// position in the nesting.
///
/// Unlike the C++ `lockdep::Guard`, which only allows unordered acquisition of a nestable lock
/// through a type-erased `Lock<CriticalMutex>*`, a `KMutex` with a nestable lock class may also be
/// acquired without an order through its default policy, which records order `0`. See
/// [`LOCK_FLAGS_NESTABLE`](ksync::LOCK_FLAGS_NESTABLE) for the validation consequences of mixing
/// unordered and ordered acquisitions of one class: do not hold an unordered guard across a
/// nonzero ordered acquisition of the same lock class unless that instance genuinely occupies
/// order `0` in the class's ordering.
pub type CriticalMutexOrderedPolicy = ksync::OrderedLockPolicy<CriticalMutexPolicy>;

/// C++ FFI entry point for `Mutex::Mutex()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_mutex_init(mutex: &mut MaybeUninit<Mutex>) {
    mutex.write(Mutex::new());
}

/// C++ FFI entry point for `Mutex::SetLockClassId()`.
///
/// # Safety
///
/// The caller must uphold all preconditions of [`Mutex::set_lock_class_id`] on `mutex` and `lcid`.
/// C++ calls this from the `lockdep::Lock<Mutex>` constructor, before the mutex is shared.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_mutex_set_lock_class_id(mutex: &Mutex, lcid: *const c_void) {
    // SAFETY: The caller upholds the exclusivity and `lcid` preconditions of
    // `Mutex::set_lock_class_id`.
    unsafe { mutex.set_lock_class_id(lcid) };
}

/// C++ FFI entry point for `Mutex::~Mutex()`, which drops the Rust `Mutex` in place.
///
/// Takes a `NonNull` rather than a reference because other threads may still be finishing a
/// release of the mutex (see [`Mutex::release_contended_mutex`]), which the drop waits for.
///
/// # Safety
///
/// - The caller must be in thread context.
/// - `mutex` must be properly aligned for `Mutex`, valid for reads and writes of
///   `size_of::<Mutex>()` bytes, and point to an initialized, pinned `Mutex`.
/// - `*mutex` must be unlocked, with no threads blocked on or attempting to acquire it. No live
///   references or concurrent accesses to `*mutex` may exist during this call, except for other
///   threads finishing [`Mutex::release_contended_mutex`] through raw pointers.
/// - `*mutex` is logically uninitialized once this call begins and must not be accessed or dropped
///   again.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_mutex_destroy(mutex: NonNull<Mutex>) {
    // SAFETY: `mutex` is non-null (`NonNull`), and the caller guarantees proper alignment,
    // read/write validity, an initialized pinned `Mutex` in thread context, and no subsequent use.
    // Because `Mutex` is `!Unpin`, `<Mutex as Drop>::drop(&mut self)` asserts no uniqueness over
    // any thread finishing `release_contended_mutex` via raw pointers, and `PinnedDrop::drop`
    // synchronizes (`Acquire`) with any such thread before `Unpin` field drop glue
    // (`Canary::drop`) runs, leaving `*mutex` logically uninitialized.
    unsafe { ptr::drop_in_place(mutex.as_ptr()) };
}

/// C++ FFI entry point for `Mutex::Acquire()`.
///
/// # Safety
///
/// `mutex` must point to a live `Mutex` which stays live for the call, and which the current
/// thread does not already hold. The caller must be in blockable thread context, hold no
/// spinlocks, and have no `ChainLockTransaction` active.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_mutex_acquire(mutex: &Mutex, spin_max_duration: DurationMono) {
    // SAFETY: Forwarded from the caller.
    unsafe { mutex.acquire(spin_max_duration) };
}

/// C++ FFI entry point for `Mutex::Release()`.
///
/// Takes a `NonNull` rather than a reference because, once the release is published, another
/// thread may acquire, release and destroy the mutex before this returns. C++ always passes
/// `this`, which is never null.
///
/// # Safety
///
/// `mutex` must point to a live `Mutex` held by the current thread. The caller must be in
/// blockable thread context and have no `ChainLockTransaction` active. The storage of `mutex`
/// must not be freed or reused until `rust_mutex_destroy` has returned for it (which waits for a
/// contended release to stop accessing it).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_mutex_release(mutex: NonNull<Mutex>) {
    // SAFETY: Forwarded from the caller.
    unsafe { Mutex::release(mutex) }
}

/// C++ FFI entry point for `Mutex::IsHeld()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_mutex_is_held(mutex: &Mutex) -> bool {
    mutex.is_held()
}

/// C++ FFI entry point for `Mutex::AssertHeld()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_mutex_assert_held(mutex: &Mutex) {
    mutex.assert_held();
}

/// C++ FFI entry point for `CriticalMutex::CriticalMutex()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_critical_mutex_init(mutex: &mut MaybeUninit<CriticalMutex>) {
    mutex.write(CriticalMutex::new());
}

/// C++ FFI entry point for `CriticalMutex::SetLockClassId()`.
///
/// # Safety
///
/// The caller must uphold all preconditions of [`Mutex::set_lock_class_id`] on `mutex`'s inner
/// `Mutex` and `lcid`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_critical_mutex_set_lock_class_id(
    mutex: &CriticalMutex,
    lcid: *const c_void,
) {
    // SAFETY: `&mutex.mutex` projects the inner `Mutex` of the valid `CriticalMutex` reference,
    // and the caller upholds the exclusivity and `lcid` preconditions of `set_lock_class_id`.
    unsafe { mutex.mutex.set_lock_class_id(lcid) };
}

/// C++ FFI entry point for `CriticalMutex::~CriticalMutex()`, which drops the Rust
/// `CriticalMutex` in place.
///
/// # Safety
///
/// - The caller must be in thread context.
/// - `mutex` must be properly aligned for `CriticalMutex`, valid for reads and writes of
///   `size_of::<CriticalMutex>()` bytes, and point to an initialized, pinned `CriticalMutex`.
/// - `*mutex` must be unlocked, with no threads blocked on or attempting to acquire it. No live
///   references or concurrent accesses to `*mutex` may exist during this call, except for other
///   threads finishing [`Mutex::release_contended_mutex`] through raw pointers.
/// - `*mutex` is logically uninitialized once this call begins and must not be accessed or dropped
///   again.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_critical_mutex_destroy(mutex: NonNull<CriticalMutex>) {
    // SAFETY: `mutex` is non-null (`NonNull`), and the caller guarantees proper alignment,
    // read/write validity, an initialized pinned `CriticalMutex` in thread context, and no
    // subsequent use. `CriticalMutex` is `#[repr(transparent)]` over `Mutex` and `!Unpin`, so
    // dropping it in place delegates to `Mutex`'s `PinnedDrop` without asserting uniqueness over
    // any racing `release_contended_mutex` raw-pointer accesses, leaving `*mutex` uninitialized.
    unsafe { ptr::drop_in_place(mutex.as_ptr()) };
}

/// C++ FFI entry point for `CriticalMutex::Acquire()`.
///
/// # Safety
///
/// As for [`rust_mutex_acquire`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_critical_mutex_acquire(
    mutex: &CriticalMutex,
    spin_max_duration: DurationMono,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { mutex.acquire(spin_max_duration) }
}

/// C++ FFI entry point for `CriticalMutex::Release()`.
///
/// # Safety
///
/// As for [`rust_mutex_release`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_critical_mutex_release(
    mutex: NonNull<CriticalMutex>,
    should_clear: bool,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { CriticalMutex::release(mutex, should_clear) }
}

/// C++ FFI entry point for `CriticalMutex::IsHeld()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_critical_mutex_is_held(mutex: &CriticalMutex) -> bool {
    mutex.is_held()
}

/// C++ FFI entry point for `CriticalMutex::AssertHeld()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_critical_mutex_assert_held(mutex: &CriticalMutex) {
    mutex.assert_held();
}

/// C++ FFI entry point for `CriticalMutex::IsContested()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/mutex.h`.
#[unsafe(no_mangle)]
extern "C" fn rust_critical_mutex_is_contested(mutex: &CriticalMutex) -> bool {
    mutex.is_contested()
}

/// Mutex tests.
#[cfg(ktest)]
#[unittest::suite(name = "mutex")]
mod tests {
    use crate::kernel::spin_tracing::LockType;
    use crate::kernel::thread::DEFAULT_PRIORITY;
    use crate::platform_rs::timer::InstantMono;
    use core::cell::Cell;
    use core::sync::atomic::AtomicBool;
    use debug::dprintf;
    use ksync::KMutex;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, expect_ne, expect_true, unwrap_ok};

    /// Verifies acquiring and releasing a `Mutex`.
    #[test]
    fn mutex_lock_unlock() {
        let mutex = Mutex::new();

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released immediately below.
        unsafe {
            mutex.acquire(Mutex::SPIN_MAX_DURATION);
        }
        // SAFETY: `mutex` is held by the current thread in blockable thread context with no
        // `ChainLockTransaction` active, and stays live until `release` returns.
        unsafe {
            Mutex::release(NonNull::from(&mutex));
        }

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released immediately below.
        unsafe {
            mutex.acquire(Mutex::SPIN_MAX_DURATION);
        }
        // SAFETY: `mutex` is held by the current thread in blockable thread context with no
        // `ChainLockTransaction` active, and stays live until `release` returns.
        unsafe {
            Mutex::release(NonNull::from(&mutex));
        }
    }

    /// Verifies the lock name storage set up by `Mutex` and `CriticalMutex` constructors.
    #[test]
    fn mutex_new_with_name() {
        let name = resolve_string!("mutex_new_with_name_test");
        let unnamed = Mutex::new();
        let named = Mutex::new_with_name(name);
        let unnamed_critical = CriticalMutex::new();
        let named_critical = CriticalMutex::new_with_name(name);
        let all = [&unnamed, &named, &unnamed_critical.mutex, &named_critical.mutex];

        if MUTEX_NAME_TRACING_ENABLED {
            // Each mutex is assigned a unique non-zero lock ID with `LockType::Mutex`, and only
            // the named mutexes have a lock class name.
            for (i, m) in all.iter().enumerate() {
                expect_ne!(m.encoded_lock_id().id(), 0);
                expect_eq!(m.encoded_lock_id().lock_type(), LockType::Mutex);
                for other in &all[i + 1..] {
                    expect_ne!(m.encoded_lock_id().id(), other.encoded_lock_id().id());
                }
            }
            for m in [&unnamed, &unnamed_critical.mutex] {
                expect_eq!(m.encoded_lock_id().class_name(), IdStringRef::INVALID_ID);
                expect_eq!(m.class_name_ref().id(), IdStringRef::invalid().id());
            }
            for m in [&named, &named_critical.mutex] {
                expect_eq!(m.encoded_lock_id().class_name(), name.id());
                expect_eq!(m.class_name_ref().id(), name.id());
            }
        } else {
            for m in all {
                expect_eq!(m.encoded_lock_id().value(), EncodedLockId::INVALID.value());
                expect_eq!(m.class_name_ref().id(), IdStringRef::disabled().id());
            }
        }
    }

    /// Verifies `set_lock_class_id` and the `rust_*mutex_*` FFI lifecycle entry points.
    #[test]
    fn mutex_ffi_lifecycle_and_set_lock_class_id() {
        let class_name = resolve_string!("mutex_ffi_class_name");
        let metadata_slot: *const InternedString = ptr::from_ref(class_name);
        let null_metadata_slot: *const InternedString = ptr::null();

        let mut unnamed_slot = MaybeUninit::<Mutex>::uninit();
        let mut unnamed_crit_slot = MaybeUninit::<CriticalMutex>::uninit();

        rust_mutex_init(&mut unnamed_slot);
        rust_critical_mutex_init(&mut unnamed_crit_slot);

        // SAFETY: Both slots were just initialized by `rust_*mutex_init` above and remain
        // pinned on the stack until `rust_*mutex_destroy` drops them in place below.
        let (unnamed, unnamed_crit) =
            unsafe { (unnamed_slot.assume_init_ref(), unnamed_crit_slot.assume_init_ref()) };

        // Null `lcid` and a metadata slot holding a null `InternedString` pointer are no-ops.
        // SAFETY: Each mutex is uniquely owned on this thread with no concurrent accesses to its
        // `name_storage`; `null_metadata_slot` is an aligned, initialized local `*const` pointer.
        unsafe {
            rust_mutex_set_lock_class_id(unnamed, ptr::null());
            rust_mutex_set_lock_class_id(unnamed, ptr::from_ref(&null_metadata_slot).cast());
            rust_critical_mutex_set_lock_class_id(unnamed_crit, ptr::null());
            rust_critical_mutex_set_lock_class_id(
                unnamed_crit,
                ptr::from_ref(&null_metadata_slot).cast(),
            );
        }
        if MUTEX_NAME_TRACING_ENABLED {
            expect_eq!(unnamed.encoded_lock_id().class_name(), IdStringRef::INVALID_ID);
            expect_eq!(unnamed_crit.mutex.encoded_lock_id().class_name(), IdStringRef::INVALID_ID);
        }

        // Valid `lcid` populates unnamed mutexes.
        // SAFETY: Each mutex is uniquely owned on this thread with no concurrent accesses to its
        // `name_storage`, and `metadata_slot` is an aligned, initialized local pointing to
        // `class_name` in `__fxt_interned_string_table`.
        unsafe {
            rust_mutex_set_lock_class_id(unnamed, ptr::from_ref(&metadata_slot).cast());
            rust_critical_mutex_set_lock_class_id(
                unnamed_crit,
                ptr::from_ref(&metadata_slot).cast(),
            );
        }

        if MUTEX_NAME_TRACING_ENABLED {
            for m in [unnamed, &unnamed_crit.mutex] {
                expect_eq!(m.encoded_lock_id().class_name(), class_name.id());
                expect_eq!(m.class_name_ref().id(), class_name.id());
            }
        } else {
            for m in [unnamed, &unnamed_crit.mutex] {
                expect_eq!(m.encoded_lock_id().value(), EncodedLockId::INVALID.value());
                expect_eq!(m.class_name_ref().id(), IdStringRef::disabled().id());
            }
        }

        // Exercise acquire, queries, and release through the FFI shims before in-place destroy.
        expect_false!(rust_mutex_is_held(unnamed));
        // SAFETY: Blockable thread context, no spinlocks or `ChainLockTransaction` active.
        unsafe { rust_mutex_acquire(unnamed, Mutex::SPIN_MAX_DURATION) };
        expect_true!(rust_mutex_is_held(unnamed));
        rust_mutex_assert_held(unnamed);
        // SAFETY: Held by current thread in blockable thread context; stays live across release.
        unsafe { rust_mutex_release(NonNull::from(unnamed)) };
        expect_false!(rust_mutex_is_held(unnamed));

        expect_false!(rust_critical_mutex_is_held(unnamed_crit));
        expect_false!(rust_critical_mutex_is_contested(unnamed_crit));
        // SAFETY: Blockable thread context, no spinlocks or `ChainLockTransaction` active.
        let should_clear =
            unsafe { rust_critical_mutex_acquire(unnamed_crit, Mutex::SPIN_MAX_DURATION) };
        expect_true!(rust_critical_mutex_is_held(unnamed_crit));
        rust_critical_mutex_assert_held(unnamed_crit);
        // SAFETY: Held by current thread with `should_clear` from `acquire`; stays live.
        unsafe { rust_critical_mutex_release(NonNull::from(unnamed_crit), should_clear) };
        expect_false!(rust_critical_mutex_is_held(unnamed_crit));

        let unnamed_ptr = NonNull::from(&mut unnamed_slot).cast::<Mutex>();
        let unnamed_crit_ptr = NonNull::from(&mut unnamed_crit_slot).cast::<CriticalMutex>();

        // SAFETY: Each `MaybeUninit` slot holds an initialized, pinned, unlocked mutex in thread
        // context with no live references, waiters, or subsequent accesses as a `Mutex`.
        unsafe {
            rust_mutex_destroy(unnamed_ptr);
            rust_critical_mutex_destroy(unnamed_crit_ptr);
        }

        // Verify that full in-place drop glue (`Canary::drop`) ran and zeroed `magic`, rather than
        // only `Mutex::destroy` (which leaves `magic` intact).
        for slot in [unnamed_ptr, unnamed_crit_ptr.cast::<Mutex>()] {
            let ptr = slot.as_ptr();
            // SAFETY: `ptr` points to a live, aligned stack `MaybeUninit` slot (`CriticalMutex` is
            // `#[repr(transparent)]` over `Mutex`), so `&raw const (*ptr).magic` stays in bounds
            // without forming a reference. `UnsafeCell<fbl::Canary<MAGIC>>` has the layout of
            // `u32` (`UnsafeCell` is `#[repr(transparent)]` and `Canary` is `#[repr(C)]` wrapping
            // a single `u32` initialized to `MAGIC` by `rust_*mutex_init*` and overwritten with
            // `0` by `Canary::drop`).
            let magic = unsafe { (&raw const (*ptr).magic).cast::<u32>().read_volatile() };
            expect_eq!(magic, 0);
        }
    }

    /// Verifies `Mutex::is_held` before, during, and after holding a `Mutex`.
    #[test]
    fn mutex_is_held() {
        let mutex = Mutex::new();

        expect_false!(mutex.is_held(), "Lock not held");
        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released below.
        unsafe {
            mutex.acquire(Mutex::SPIN_MAX_DURATION);
        }
        expect_true!(mutex.is_held(), "Lock held");
        // SAFETY: `mutex` is held by the current thread in blockable thread context with no
        // `ChainLockTransaction` active, and stays live until `release` returns.
        unsafe {
            Mutex::release(NonNull::from(&mutex));
        }
        expect_false!(mutex.is_held(), "Lock not held");
    }

    /// Verifies `Mutex::assert_held` succeeds when the `Mutex` is held.
    #[test]
    fn mutex_assert_held() {
        let mutex = Mutex::new();

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released below.
        unsafe {
            mutex.acquire(Mutex::SPIN_MAX_DURATION);
        }
        mutex.assert_held(); // Lock is held: this should be a no-op.
        // SAFETY: `mutex` is held by the current thread in blockable thread context with no
        // `ChainLockTransaction` active, and stays live until `release` returns.
        unsafe {
            Mutex::release(NonNull::from(&mutex));
        }
    }

    /// A struct with a guarded value.
    #[ksync::guarded]
    struct ObjectWithMutex {
        #[mutex]
        mu: KMutex<Mutex>,
        #[guarded_by(mu)]
        val: i32,
    }

    impl ObjectWithMutex {
        fn new() -> impl PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                mu <- KMutex::init(),
                val: 0.into(),
            })
        }

        /// # Safety
        ///
        /// As for [`Mutex::acquire`] on `self.mu`.
        unsafe fn take_lock(&self) {
            // SAFETY: Forwarded from the caller.
            unsafe { self.mu.raw_mutex().acquire(Mutex::SPIN_MAX_DURATION) };
        }
    }

    /// Verifies that `assert_held` grants compile-time access to `ksync`-guarded `Mutex` state.
    #[test]
    fn mutex_assert_held_compile() {
        stack_pin_init!(let object = ObjectWithMutex::new());

        // This shouldn't compile with ksync's compile-time lock checking.
        #[cfg(enable_errors)]
        {
            object.val = 3;
        }

        // We take the lock, but Rust's type system can't see it.
        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `object.mu` is not held and is released below.
        unsafe {
            object.take_lock();
        }

        // Without the assertion, Rust will object to setting "val".
        #[cfg(not(enable_errors))]
        // SAFETY: `take_lock()` acquired `object.mu` on the current thread above, `token` does not
        // outlive the lock hold, and no other borrows of guarded fields on `object` exist.
        let mut token = unsafe { object.mu.assert_held() };
        // SAFETY: `token` was obtained from `object.mu` on this `object` instance.
        *unsafe { object.guard_mu_mut(&mut token) }.val_mut() = 3;

        // Without the assertion, Rust will object to releasing the lock.
        // SAFETY: `object.mu` is held by the current thread in blockable thread context with no
        // `ChainLockTransaction` active, no borrows of data guarded by this acquisition remain
        // live, and `object` stays live until `release` returns.
        unsafe {
            object.mu.release(token);
        }
    }

    /// Verifies acquiring and releasing a `CriticalMutex`.
    #[test]
    fn critical_mutex_lock_unlock() {
        let mutex = CriticalMutex::new();

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released immediately below.
        let mut should_clear = unsafe { mutex.acquire(Mutex::SPIN_MAX_DURATION) };
        // SAFETY: `mutex` is held by the current thread with `should_clear` from `acquire`, in
        // blockable thread context with no `ChainLockTransaction` active, and stays live until
        // `release` returns.
        unsafe {
            CriticalMutex::release(NonNull::from(&mutex), should_clear);
        }

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released immediately below.
        should_clear = unsafe { mutex.acquire(Mutex::SPIN_MAX_DURATION) };
        // SAFETY: `mutex` is held by the current thread with `should_clear` from `acquire`, in
        // blockable thread context with no `ChainLockTransaction` active, and stays live until
        // `release` returns.
        unsafe {
            CriticalMutex::release(NonNull::from(&mutex), should_clear);
        }
    }

    /// Verifies `CriticalMutex::is_held` before, during, and after holding a `CriticalMutex`.
    #[test]
    fn critical_mutex_is_held() {
        let mutex = CriticalMutex::new();

        expect_false!(mutex.is_held(), "Lock not held");
        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released below.
        let should_clear = unsafe { mutex.acquire(Mutex::SPIN_MAX_DURATION) };
        expect_true!(mutex.is_held(), "Lock held");
        // SAFETY: `mutex` is held by the current thread with `should_clear` from `acquire`, in
        // blockable thread context with no `ChainLockTransaction` active, and stays live until
        // `release` returns.
        unsafe {
            CriticalMutex::release(NonNull::from(&mutex), should_clear);
        }
        expect_false!(mutex.is_held(), "Lock not held");
    }

    /// Verifies `CriticalMutex::assert_held` succeeds when the `CriticalMutex` is held.
    #[test]
    fn critical_mutex_assert_held() {
        let mutex = CriticalMutex::new();

        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `mutex` is not held and is released below.
        let should_clear = unsafe { mutex.acquire(Mutex::SPIN_MAX_DURATION) };
        mutex.assert_held(); // Lock is held: this should be a no-op.
        // SAFETY: `mutex` is held by the current thread with `should_clear` from `acquire`, in
        // blockable thread context with no `ChainLockTransaction` active, and stays live until
        // `release` returns.
        unsafe {
            CriticalMutex::release(NonNull::from(&mutex), should_clear);
        }
    }

    /// A struct with a guarded value.
    #[ksync::guarded]
    struct ObjectWithCriticalMutex {
        #[mutex]
        mu: KMutex<CriticalMutex>,
        #[guarded_by(mu)]
        val: i32,
    }

    impl ObjectWithCriticalMutex {
        fn new() -> impl PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                mu <- KMutex::init(),
                val: 0.into(),
            })
        }

        /// # Safety
        ///
        /// As for [`CriticalMutex::acquire`] on `self.mu`.
        unsafe fn take_lock(&self) -> bool {
            // SAFETY: Forwarded from the caller.
            unsafe { self.mu.raw_mutex().acquire(Mutex::SPIN_MAX_DURATION) }
        }
    }

    /// Verifies `assert_held` compile-time access to `ksync`-guarded `CriticalMutex` state.
    #[test]
    fn critical_mutex_assert_held_compile() {
        stack_pin_init!(let object = ObjectWithCriticalMutex::new());

        // This shouldn't compile with ksync's compile-time lock checking.
        #[cfg(enable_errors)]
        {
            object.val = 3;
        }

        // We take the lock, but Rust's type system can't see it.
        // SAFETY: Kernel unit tests run in blockable thread context with no spinlocks held and no
        // `ChainLockTransaction` active, and `object.mu` is not held and is released below.
        let should_clear = unsafe { object.take_lock() };

        // Without the assertion, Rust will object to setting "val".
        #[cfg(not(enable_errors))]
        // SAFETY: `take_lock()` acquired `object.mu` on the current thread above, `token` does not
        // outlive the lock hold, and no other borrows of guarded fields on `object` exist.
        let mut token = unsafe { object.mu.assert_held() };
        // SAFETY: `token` was obtained from `object.mu` on this `object` instance.
        *unsafe { object.guard_mu_mut(&mut token) }.val_mut() = 3;

        // Without the assertion, Rust will object to releasing the lock.
        // SAFETY: `object.mu` is held by the current thread with `should_clear` from `take_lock`
        // in blockable thread context with no `ChainLockTransaction` active, no borrows of data
        // guarded by this acquisition remain live, and `object` stays live until `release_with`
        // returns.
        unsafe {
            object.mu.release_with(should_clear, token);
        }
    }

    #[ksync::guarded]
    struct TestSingletonMutex {
        #[mutex]
        lock: KMutex<TrackedMutex>,
    }

    static TEST_SINGLETON_MUTEX: lazy_init::LazyInit<TestSingletonMutex, lazy_init::NoCheck> =
        lazy_init::LazyInit::uninit();

    fn test_singleton_mutex_init(_level: init::LkInitLevel) {
        // SAFETY: lk init hooks run once during single-threaded early boot before any test runs.
        unsafe {
            let Ok(()) = Pin::static_ref(&TEST_SINGLETON_MUTEX)
                .init_pin(pin_init!(TestSingletonMutex { lock <- KMutex::init() }));
        }
    }
    init::lk_init_hook!(
        test_singleton_mutex,
        test_singleton_mutex_init,
        init::LK_INIT_LEVEL_EARLIEST
    );

    /// Ensure that acquiring a singleton mutex is thread-safe the first time it is acquired.
    #[test]
    fn singleton_mutex_threadsafe() {
        // We've previously had bugs where singleton mutexes were defined static and lazily
        // initialised, leading to a bug where the first time a mutex was acquired, it could be
        // held by multiple threads simultaneously.
        //
        // This test sets up N threads and races them acquiring the singleton mutex
        // "TestSingletonMutex". While the test is safe to run multiple times, it can only exercise
        // static initialisation once per boot.

        // If we have already run print a warning that this test is unlikely to exercise anything.
        static ALREADY_RUN: AtomicBool = AtomicBool::new(false);
        if ALREADY_RUN.load(Ordering::SeqCst) {
            dprintf!(
                INFO,
                "Test has already run this boot. \
                 Subsequent runs will not exercise the mutex init code path again.\n"
            );
        }
        ALREADY_RUN.store(true, Ordering::SeqCst);

        // Start multiple threads, all attempting to race to acquire the singleton mutex.
        struct ThreadState<'a> {
            ready: AtomicBool,
            should_start: &'a AtomicBool,
            in_critical_section: &'a AtomicBool,
        }

        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to this worker's element of `states` on the stack of
            // `singleton_mutex_threadsafe`, which joins all spawned workers before `states`,
            // `should_start`, and `in_critical_section` go out of scope.
            let state = unsafe { &*arg.cast::<ThreadState<'_>>() };

            // Tell parent we are ready.
            state.ready.store(true, Ordering::SeqCst);

            // Spin until all threads are ready to start.
            //
            // We busy-wait here without yielding to try and synchronise threads on
            // different CPUs as much as possible, so that they all race to acquire
            // the mutex below.
            //
            // Ordering: `Relaxed` suffices as we wait for the parent's start flag; synchronization
            // of the critical section is provided by `TestSingletonMutex` below.
            while !state.should_start.load(Ordering::Relaxed) {}

            {
                // Acquire the mutex.
                // SAFETY: `TEST_SINGLETON_MUTEX` is initialized at `LK_INIT_LEVEL_EARLIEST`
                // before any test runs.
                ksync::lock!(
                    let _guard = unsafe { TEST_SINGLETON_MUTEX.get_unchecked() }.lock_lock()
                );

                // Ensure no other thread already has the mutex.
                let other_thread_in_critical_section =
                    state.in_critical_section.swap(true, Ordering::SeqCst);
                assert!(
                    !other_thread_in_critical_section,
                    "Another thread was already in the critical section."
                );

                // Delay before releasing the mutex, to give other threads a chance to
                // notice we are holding it.
                let _ = thread::sleep_relative(DurationMono::from_millis(1));

                let still_hold_critical_section =
                    state.in_critical_section.swap(false, Ordering::SeqCst);
                assert!(
                    still_hold_critical_section,
                    "Another thread released our critical section."
                );
            }

            0
        }

        // Create worker threads and start them up.
        const NUM_THREADS: usize = 4;
        let should_start = AtomicBool::new(false);
        let in_critical_section = AtomicBool::new(false);
        let states: [ThreadState<'_>; NUM_THREADS] = core::array::from_fn(|_| ThreadState {
            ready: AtomicBool::new(false),
            should_start: &should_start,
            in_critical_section: &in_critical_section,
        });
        let threads: [Cell<Option<ThreadPtr>>; NUM_THREADS] =
            [const { Cell::new(None) }; NUM_THREADS];
        let _cleanup = zr::defer(|| {
            should_start.store(true, Ordering::SeqCst);
            for thread in threads.iter().filter_map(Cell::take) {
                // SAFETY: Each stored `ThreadPtr` was created below and has not been joined yet.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }
        });
        for (state, thread_slot) in states.iter().zip(&threads) {
            // SAFETY: `worker_body` only accesses `state` through shared references, and `state`
            // (along with `should_start` and `in_critical_section`) stays live until the thread is
            // joined below (or by `_cleanup` on early return).
            let thread = unsafe {
                thread::create_with_priority(
                    c"test_singleton_mutex".as_ptr(),
                    worker_body,
                    ptr::from_ref(state).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            };
            let thread = unwrap_ok!(thread, "Thread::Create failed.");
            // SAFETY: `thread` was just created and has not been resumed or joined yet.
            unsafe { thread.resume() };
            thread_slot.set(Some(thread));
        }

        // Wait for all the threads to start.
        for state in &states {
            while !state.ready.load(Ordering::SeqCst) {
                thread::r#yield();
            }
        }

        // Let all the threads race.
        should_start.store(true, Ordering::SeqCst);

        // Wait for all the threads to finish.
        for thread in threads.iter().filter_map(Cell::take) {
            // SAFETY: Each stored `ThreadPtr` was created above and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }
}
