// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel timer subsystem.
//!
//! The timer subsystem allows functions to be scheduled for later execution.  Each timer object is
//! used to cause one function to be executed at a later time.
//!
//! Timer callback functions are called in interrupt context.
//!
//! Rules for Timers:
//! - Timer callbacks occur from interrupt context.
//! - Timers may be programmed or canceled from interrupt or thread context.
//! - Timers may be canceled or reprogrammed from within their callback.
//! - Setting and canceling timers is not thread safe and cannot be done concurrently.
//! - [`Timer::cancel`] may spin waiting for a pending timer to complete on another cpu.
//! - Timers aren't permanently bound to a particular CPU and may fire on a CPU other than the one
//!   on which they were set.
//!
//! The C++ `Timer` and `TimerQueue` classes in `kernel/timer.h` reserve opaque storage of the same
//! size and alignment for these types (so that timers can be embedded in C++ objects and
//! constructed in static storage) and forward to the `rust_*` functions at the bottom of this
//! file.

use core::cell::{Cell, UnsafeCell};
use core::ffi::{c_char, c_void};
use core::fmt::Write;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use affine::Round;
use debug::ltracef;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode};
use ksync::{KCell, LockToken, kcell_init, source_tag};
use libarch::device_memory_barrier;
use libc::stdio::StringFile;
use pin_init::{InPlaceWrite, PinInit, pin_data, pin_init, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::{ZX_TIME_INFINITE, zx_status_t};

use super::deadline::{Deadline, DurationUnknown, InstantUnknown, SlackMode};
use crate::arch_rs::{curr_cpu_num, ints_disabled};
use crate::counters::define_kcounter;
use crate::kconcurrent::{ChainLock, ChainLockTransaction};
use crate::kernel::mp::{INVALID_CPU, is_cpu_online};
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler::{SchedTime, rust_kernel_scheduler_timer_tick};
use crate::kernel::stats::{inc_timer_ints, inc_timers};
use crate::kernel::types::cpu_num_t;
use crate::ksync_kernel::{NoIrqSavePolicy, declare_singleton_monitored_spinlock};
use crate::platform_rs::timer::{
    InstantBoot, InstantMono, InstantMonoTicks, current_boot_time, current_mono_time,
    platform_set_oneshot_timer, platform_stop_timer, timer_convert_mono_to_raw_ticks,
    timer_get_boot_ticks_offset, timer_get_ticks_to_time_ratio,
};

const LOCAL_TRACE: u32 = 0;

/// The monotonic clock (`ZX_CLOCK_MONOTONIC`); a timer on this clock is set with monotonic
/// instants.
pub const ZX_CLOCK_MONOTONIC: u32 = 0;
/// The boot clock (`ZX_CLOCK_BOOT`); a timer on this clock is set with boot instants.
pub const ZX_CLOCK_BOOT: u32 = 1;

// Total number of timers set. Always increasing.
define_kcounter!(TIMER_CREATED_COUNTER, "timer.created", Sum);

// Number of timers merged into an existing timer because of slack.
define_kcounter!(TIMER_COALESCED_COUNTER, "timer.coalesced", Sum);

// Number of timers that have fired (i.e. callback was invoked).
define_kcounter!(TIMER_FIRED_COUNTER, "timer.fired", Sum);

// Number of timers that were successfully canceled. Attempts to cancel a timer that is currently
// firing are not counted.
define_kcounter!(TIMER_CANCELED_COUNTER, "timer.canceled", Sum);

// TODO(https://fxbug.dev/575806167): Rust singleton spinlocks have no spin-tracing lock name, so
// spin traces do not show "timer_lock".
declare_singleton_monitored_spinlock!(
    /// The global lock guarding every [`TimerQueue`]'s timer lists and the lock-guarded state of
    /// every [`Timer`].
    TimerLock,
    cpu_align_exclusive
);
// `cpu_align_exclusive` aligns `TimerLock` to 64 bytes, which must be a full cache line.
static_assert!(crate::arch_rs::MAX_CACHE_LINE == 64);

/// The function invoked when a [`Timer`] fires.
///
/// The timer function is declared as:
///   `unsafe extern "C" fn callback(timer: *mut Timer, now: i64, arg: *mut c_void) { ... }`
pub type Callback = unsafe extern "C" fn(timer: *mut Timer, now: i64, arg: *mut c_void);

const MAGIC: u32 = fbl::magic(b"timr");

/// The atomic types wrapped by [`TimerSyncVar`].
trait SyncVarAtomic {
    type Value: Copy;
    fn load(&self, order: Ordering) -> Self::Value;
    fn store(&self, val: Self::Value, order: Ordering);
}

impl SyncVarAtomic for AtomicU32 {
    type Value = u32;
    #[inline]
    fn load(&self, order: Ordering) -> u32 {
        AtomicU32::load(self, order)
    }
    #[inline]
    fn store(&self, val: u32, order: Ordering) {
        AtomicU32::store(self, val, order)
    }
}

impl SyncVarAtomic for AtomicBool {
    type Value = bool;
    #[inline]
    fn load(&self, order: Ordering) -> bool {
        AtomicBool::load(self, order)
    }
    #[inline]
    fn store(&self, val: bool, order: Ordering) {
        AtomicBool::store(self, val, order)
    }
}

/// `TimerSyncVar` is a small helper type used to help us follow some of the trickier rules when
/// it comes to synchronization and timer operations.  Most of timer and timer queue state is
/// protected by the global [`TimerLock`], which guarantees ordering.  There are some examples
/// which are a bit more complicated.  Consider the cancel operation.
///
/// When a timer needs to fire, the global lock is held and the timer's deadline is evaluated.  If
/// the deadline has arrived, the timer tick code will remove the timer from the appropriate
/// pending list, and then indicate that there is a callback-in-flight by setting the `active_cpu`
/// atomic to the current CPU value.  Then it drops the lock and performs the callback.  Both
/// dropping the lock and recording which cpu is handling the callback are important here because
/// the callback can choose to call into the timer's methods during the callback.  Calling "set" is
/// a good example of something someone might want to do from the timer callback.
///
/// Now consider another CPU attempting to cancel a timer T when there is a callback already in
/// flight.  Cancel is supposed to return a boolean:
///
/// 1) True means that the cancel succeeded.  The timer was canceled before any callback was made,
///    and now there is a guarantee that no callback will be made and that the timer's owner is
///    free to destroy the Timer.
/// 2) False means that the cancel failed.  When control is returned to the caller, the callback is
///    guaranteed to have finished.
///
/// So, when cancel runs, it grabs the lock and checks to see if the callback has already started.
/// If it has, then it need to drop the lock (so the timer tick code can reacquire the lock and
/// mark the callback done via `active_cpu`), _and wait for the callback to finish_.  It does this
/// by spinning on the `active_cpu` variable, read using atomic loads.
///
/// It is important, however, that when the callback has finished, that the cancel CPU is
/// guaranteed that not only is the callback done, but that all of the side effects of the callback
/// are visible to the cancel CPU.  So, it needs to load with acquire, and writes of the state (who
/// always hold the lock) need to store with release.  Anyone else reading the variable from
/// _inside_ of the lock, however, do not need to load with acquire.  The lock is already
/// satisfying any ordering requirements.
///
/// So: `TimerSyncVar` lets us control access to the two variables (`cancel` and `active_cpu`) we
/// have which are sometimes are called upon to provide synchronization guarantees without relying
/// on the guarantees provided by the lock.  It enforces the following rules, using a
/// [`TimerLock`] [`LockToken`] parameter to prove that the lock is held.
///
/// 1) Whenever a `TimerSyncVar` is written, it is always stored with release semantics, and also
///    demands that we hold the lock.
/// 2) Whenever a `TimerSyncVar` is read from inside of the lock, it is loaded with relaxed
///    semantics.  The fact that #1 happened with the lock held provides our synchronization
///    guarantees.
/// 3) Whenever a `TimerSyncVar` is read from outside of the lock, it is read using acquire
///    semantics, which should syncronize-with the release-store performed in #1.
///
/// Wrapping this all up in a type provides a good place for these rules, in addition to
/// guaranteeing that no one can make a mistake by directly accessing the actual atomic with the
/// wrong semantics.
#[repr(transparent)]
struct TimerSyncVar<A: SyncVarAtomic> {
    val: A,
}

impl<A: SyncVarAtomic> TimerSyncVar<A> {
    /// No default constructor.  Explicitly initialize your PODs folks!
    #[inline]
    const fn new(val: A) -> Self {
        Self { val }
    }

    #[inline]
    fn load_locked(&self, _token: &LockToken<'_, TimerLock>) -> A::Value {
        self.val.load(Ordering::Relaxed)
    }

    /// Loads the value with acquire semantics, for use outside of [`TimerLock`]. Calling this with
    /// the lock held is not unsound, but the acquire is redundant; use [`Self::load_locked`].
    #[inline]
    fn load_unlocked(&self) -> A::Value {
        self.val.load(Ordering::Acquire)
    }

    #[inline]
    fn store_locked(&self, val: A::Value, _token: &LockToken<'_, TimerLock>) {
        self.val.store(val, Ordering::Release);
    }
}

/// An atomically accessed, nullable [`Callback`].
#[repr(transparent)]
struct AtomicCallback(AtomicPtr<()>);

impl AtomicCallback {
    #[inline]
    fn new(callback: Option<Callback>) -> Self {
        Self(AtomicPtr::new(Self::to_ptr(callback)))
    }

    #[inline]
    fn to_ptr(callback: Option<Callback>) -> *mut () {
        callback.map_or(ptr::null_mut(), |f| f as *mut ())
    }

    #[inline]
    fn load(&self, order: Ordering) -> Option<Callback> {
        let callback = self.0.load(order);
        if callback.is_null() {
            None
        } else {
            // SAFETY: Every non-null value stored in `self.0` was converted from a valid
            // `Callback` by `to_ptr`, and function and data pointers have the same size and
            // representation on every architecture the kernel supports.
            Some(unsafe { core::mem::transmute::<*mut (), Callback>(callback) })
        }
    }

    #[inline]
    fn store(&self, callback: Option<Callback>, order: Ordering) {
        self.0.store(Self::to_ptr(callback), order);
    }

    /// Returns the address of the callback, or 0 if there is none.
    #[inline]
    fn addr(&self, order: Ordering) -> usize {
        self.0.load(order).addr()
    }
}

/// The tag of the [`TimerQueue`] lists. It is private, so a [`Timer`] can only ever be linked into
/// a [`TimerList`].
struct TimerQueueTag;

/// A kernel timer.
///
/// Timers may be removed from an arbitrary [`TimerQueue`], so they are removed from their list
/// with [`fbl::remove_from_container`].
#[derive(DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Timer {
    #[pin]
    #[dll_node(tag = TimerQueueTag)]
    node: DoublyLinkedListNode<Timer>,

    magic: u32,

    /// This field should be interpreted differently depending on the `clock_id` field.
    /// If `clock_id` is set to [`ZX_CLOCK_MONOTONIC`], this is a monotonic instant.
    /// If `clock_id` is set to [`ZX_CLOCK_BOOT`], this is a boot instant.
    scheduled_time: KCell<Cell<InstantUnknown>, TimerLock>,

    /// Stores the applied slack adjustment from the ideal `scheduled_time`.
    slack: KCell<Cell<DurationUnknown>, TimerLock>,

    /// `callback` and `arg` are written under [`TimerLock`] by [`Timer::set_deadline`], but are
    /// read without the lock while the callback runs and are cleared without the lock at the end
    /// of [`Timer::cancel`], so they are relaxed atomics.
    callback: AtomicCallback,
    arg: AtomicPtr<c_void>,

    /// The clock this timer is set on.
    clock_id: u32,

    /// [`INVALID_CPU`], if inactive.
    active_cpu: TimerSyncVar<AtomicU32>,

    /// true if cancel is pending
    cancel: TimerSyncVar<AtomicBool>,
}

// The C++ `Timer` is `OpaqueStorage<72, 8>`, which its `constexpr` constructor fills with the
// initial state of `Timer::init`: zero bytes, except for `magic`, `clock_id` and `active_cpu`,
// which it writes at `Timer::kMagicOffset`, `Timer::kClockIdOffset` and `Timer::kActiveCpuOffset`.
// The `constinit_timer` test checks that the two initializers agree, and `percpu.rs` also checks
// the size and alignment against the bindgen view of the C++ class.
static_assert!(size_of::<Timer>() == 72);
static_assert!(align_of::<Timer>() == 8);
static_assert!(offset_of!(Timer, magic) == 16);
static_assert!(offset_of!(Timer, clock_id) == 56);
static_assert!(offset_of!(Timer, active_cpu) == 60);

// SAFETY: All interior mutability in `Timer` is synchronized across CPUs by `TimerLock`
// (`node`, `scheduled_time`, `slack`) or atomic operations (`callback`, `arg`, `active_cpu`,
// `cancel`), so transferring a `Timer` across threads is safe.
unsafe impl Send for Timer {}

// SAFETY: All interior mutability accessible through a shared `&Timer` reference is
// synchronized across CPUs: `scheduled_time` and `slack` are `KCell`s guarded by `TimerLock`;
// `callback`, `arg`, `active_cpu`, and `cancel` are atomics; and `node` (whose `UnsafeCell`s
// make `Timer` `!Sync` by default) is only modified or traversed under `TimerLock`. The only
// access to `node` outside `TimerLock` is in `teardown`, which runs when the `Timer` is being
// destroyed (`PinnedDrop::drop` or C++ `~Timer()`) and no concurrent references may exist. All
// safe methods on `&Timer` (`cancel`) therefore synchronize all shared state accesses via
// `TimerLock` or atomics.
unsafe impl Sync for Timer {}

impl Timer {
    /// Creates an in-place initializer for a `Timer` on the clock `clock_id`.
    ///
    /// The resulting state is identical to the one produced by the C++ `constexpr` constructor,
    /// which allows timers to be constructed in static storage: an unlinked node is all zero (null
    /// `next` and `prev`), and so are all other fields but `magic`, `clock_id` and `active_cpu`.
    pub fn init(clock_id: u32) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            node: DoublyLinkedListNode::new(),
            magic: MAGIC,
            scheduled_time: KCell::new(Cell::new(InstantUnknown(0))),
            slack: KCell::new(Cell::new(DurationUnknown(0))),
            callback: AtomicCallback::new(None),
            arg: AtomicPtr::new(ptr::null_mut()),
            clock_id,
            active_cpu: TimerSyncVar::new(AtomicU32::new(INVALID_CPU)),
            cancel: TimerSyncVar::new(AtomicBool::new(false)),
        })
    }

    #[inline]
    fn as_ptr(&self) -> NonNull<Timer> {
        NonNull::from(self)
    }

    #[inline]
    fn scheduled_time<'a>(
        &'a self,
        token: &'a LockToken<'_, TimerLock>,
    ) -> &'a Cell<InstantUnknown> {
        // SAFETY: `TimerLock` is a singleton, so `token` proves that the lock guarding
        // `scheduled_time` is held.
        unsafe { self.scheduled_time.get(token) }
    }

    #[inline]
    fn slack<'a>(&'a self, token: &'a LockToken<'_, TimerLock>) -> &'a Cell<DurationUnknown> {
        // SAFETY: `TimerLock` is a singleton, so `token` proves that the lock guarding `slack` is
        // held.
        unsafe { self.slack.get(token) }
    }

    /// Set up a timer that executes once
    ///
    /// This function specifies a callback function to be run after a specified
    /// deadline passes. The function will be called one time.
    ///
    /// - `deadline`: specifies when the timer should be executed
    /// - `callback`: the function to call when the timer expires
    /// - `arg`: the argument to pass to the callback
    ///
    /// The timer function is declared as:
    ///   `void callback(Timer *, zx_time_t now, void *arg) { ... }`
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` remains pinned in memory and is not dropped, moved, or deallocated while
    ///   scheduled; [`Self::cancel`] must be called before `self` is dropped if it may still be
    ///   scheduled or executing its callback.
    /// - `callback` is a valid FFI callback that does not cause undefined behavior when executed.
    /// - `arg` is either a valid pointer or a null pointer, and remains valid to pass to
    ///   `callback` until either `callback` completes or [`Self::cancel`] returns.
    /// - Neither [`Self::set_deadline`], [`Self::set_oneshot`], nor [`Self::cancel`] is called
    ///   concurrently on `self` (except from within `callback` itself).
    #[inline]
    pub unsafe fn set_deadline(
        self: Pin<&Self>,
        deadline: &Deadline,
        callback: Callback,
        arg: *mut c_void,
    ) {
        // SAFETY: The caller upholds the pinning, lifetime, callback, and synchronization
        // requirements of `set`.
        unsafe { self.get_ref().set(deadline, Some(callback), arg) }
    }

    /// Equivalent to [`Self::set_deadline`] with no slack.
    ///
    /// The deadline parameter should be interpreted differently depending on the clock of the
    /// timer. If the clock is [`ZX_CLOCK_MONOTONIC`], deadline is a monotonic instant. If the
    /// clock is [`ZX_CLOCK_BOOT`], deadline is a boot instant.
    ///
    /// # Safety
    ///
    /// Same as [`Self::set_deadline`].
    #[inline]
    pub unsafe fn set_oneshot(
        self: Pin<&Self>,
        deadline: i64,
        callback: Callback,
        arg: *mut c_void,
    ) {
        let dl = Deadline::no_slack(InstantUnknown(deadline));
        // SAFETY: The caller guarantees the safety preconditions of `set_deadline`.
        unsafe {
            self.set_deadline(&dl, callback, arg);
        }
    }

    /// Set up a timer that executes once
    ///
    /// This function specifies a callback function to be run after a specified
    /// deadline passes. The function will be called one time.
    ///
    /// - `deadline`: specifies when the timer should be executed
    /// - `callback`: the function to call when the timer expires
    /// - `arg`: the argument to pass to the callback
    ///
    /// The timer function is declared as:
    ///   `void callback(Timer *, zx_time_t now, void *arg) { ... }`
    ///
    /// # Safety
    ///
    /// Same as [`Self::set_deadline`], and `callback` must be `Some` by the time the timer fires.
    unsafe fn set(&self, deadline: &Deadline, callback: Option<Callback>, arg: *mut c_void) {
        ltracef!(
            "timer {:p} deadline.when {} deadline.slack.amount {} deadline.slack.mode {} \
             callback {} arg {:p}\n",
            self,
            deadline.when().0,
            deadline.slack().amount().0,
            deadline.slack().mode() as u32,
            PrintfPtr(AtomicCallback::to_ptr(callback).addr()),
            arg
        );

        debug_assert!(self.magic == MAGIC);
        debug_assert!(deadline.slack().mode() as u32 <= SlackMode::Late as u32);
        debug_assert!(deadline.slack().amount().0 >= 0);

        let latest_deadline = deadline.latest();
        let earliest_deadline = deadline.earliest();

        ksync::lock!(let mut guard = TimerLock::lock(source_tag!()));
        let token = guard.as_mut().token_mut();

        if self.node.in_container() {
            panic!("timer {:p} already in list\n", self);
        }

        let cpu = curr_cpu_num();
        let active_cpu = self.active_cpu.load_locked(token);

        let currently_active = active_cpu == cpu;
        if currently_active {
            // The timer is active on our own cpu, we must be inside the callback.
            if self.cancel.load_locked(token) {
                return;
            }
        } else if active_cpu != INVALID_CPU {
            panic!("timer {:p} currently active on a different cpu {}\n", self, active_cpu);
        }

        // Set up the structure.
        self.scheduled_time(token).set(deadline.when());
        self.callback.store(callback, Ordering::Relaxed);
        self.arg.store(arg, Ordering::Relaxed);
        self.cancel.store_locked(false, token);
        // We don't need to modify active_cpu because it is managed by timer_tick().

        ltracef!("scheduled time {}\n", self.scheduled_time(token).get().0);

        let timer_queue = &PerCpu::get_current().timer_queue;
        // SAFETY: `self` is not in any list (checked above), and the caller guarantees that it
        // stays pinned and alive until it is canceled or has fired.
        unsafe { timer_queue.insert(token, self, earliest_deadline, latest_deadline) };

        match self.clock_id {
            ZX_CLOCK_MONOTONIC => {
                let timer_list = TimerQueue::list(&timer_queue.monotonic_timer_list, token);
                if !timer_list.is_empty() && timer_list.front().is_some_and(|t| ptr::eq(t, self)) {
                    // SAFETY: `timer_queue` is the current CPU's queue, and holding `TimerLock`
                    // (acquired with `IrqSavePolicy`) keeps interrupts disabled.
                    unsafe {
                        timer_queue.update_platform_timer_mono(InstantMono(deadline.when().0))
                    };
                }
            }
            ZX_CLOCK_BOOT => {
                let timer_list = TimerQueue::list(&timer_queue.boot_timer_list, token);
                if !timer_list.is_empty() && timer_list.front().is_some_and(|t| ptr::eq(t, self)) {
                    // SAFETY: `timer_queue` is the current CPU's queue, and holding `TimerLock`
                    // (acquired with `IrqSavePolicy`) keeps interrupts disabled.
                    unsafe {
                        timer_queue.update_platform_timer_boot(InstantBoot(deadline.when().0))
                    };
                }
            }
            _ => {}
        }
        TIMER_CREATED_COUNTER.add(1);
    }

    /// Cancel a pending timer
    ///
    /// Returns true if the timer was canceled before it was scheduled in a cpu and false otherwise
    /// or if the timer was not scheduled at all.
    pub fn cancel(&self) -> bool {
        debug_assert!(self.magic == MAGIC);

        ksync::lock!(let mut guard = TimerLock::lock(source_tag!()));
        let token = guard.as_mut().token_mut();

        let cpu = curr_cpu_num();

        // Mark the timer as canceled. This visible state change allows other CPUs
        // (e.g., in trylock_or_cancel) to detect that the timer is being cancelled and
        // abort operations early.
        self.cancel.store_locked(true, token);
        // TODO(https://fxbug.dev/42142666): Consider whether this DeviceMemoryBarrier
        // is required
        device_memory_barrier();

        // Check if we are attempting to cancel the timer from within its own callback
        // on the current CPU. If so, we cannot wait for the timer to become un-busy
        // (as that would deadlock). Instead, we clear the callback and return false,
        // indicating that the callback was running.
        if self.active_cpu.load_locked(token) == cpu {
            // Zero out as defense-in-depth so that incorrect reuse results in a null
            // dereference rather than executing stale code.
            self.callback.store(None, Ordering::Relaxed);
            self.arg.store(ptr::null_mut(), Ordering::Relaxed);

            // We're done, so return back to the callback.
            return false;
        }

        let callback_not_running;

        // If this Timer is in a queue, remove it and adjust hardware timers if
        // needed.
        if self.node.in_container() {
            callback_not_running = true;

            let timer_queue = &PerCpu::get_current().timer_queue;

            // Save the old head of the *current* CPU's queue so later we can see if we
            // modified the head of that queue and need to update the local hardware
            // timer.
            let mut oldhead: Option<NonNull<Timer>> = None;
            let timer_list = if self.clock_id == ZX_CLOCK_MONOTONIC {
                &timer_queue.monotonic_timer_list
            } else {
                &timer_queue.boot_timer_list
            };
            if !TimerQueue::list(timer_list, token).is_empty() {
                oldhead = TimerQueue::list(timer_list, token).front().map(NonNull::from);
            }

            // Remove this Timer from whatever TimerQueue it's on, which may not be the
            // current CPU's queue.
            // SAFETY: `Timer`'s list node uses the private `TimerQueueTag`, so `self` can only be
            // in a `TimerList` (`NonTrackingSize`, guarded by `TimerLock`, which is held), and no
            // reference into any timer list is live.
            let _: Option<NonNull<Timer>> =
                unsafe { fbl::remove_from_container::<Timer, TimerQueueTag, _>(self) };
            TIMER_CANCELED_COUNTER.add(1);

            // TODO(cpu): If, after removing |timer| there is one other single Timer with
            // the same scheduled_time and slack non-zero, then it is possible to return
            // that timer to the ideal scheduled_time.

            // If the timer was the head of the *current* CPU's queue, we need to update
            // the hardware timer for this CPU.
            //
            // Note on cross-CPU cancellation: If the timer was on *another* CPU's
            // queue, we do not update that CPU's hardware timer here. The hardware
            // timer on that CPU will still fire at the scheduled time. When it does,
            // the tick handler will notice the queue head has changed (or is empty) and
            // will update the hardware timer accordingly. This "lazy" update avoids the
            // need to IPI the other CPU to update its hardware timer.
            if oldhead == Some(self.as_ptr()) {
                // The Timer we're canceling was at head of this queue, so see if we
                // should update platform timer.
                if !TimerQueue::list(timer_list, token).is_empty() {
                    // SAFETY: `timer_queue` is the current CPU's queue, and holding `TimerLock`
                    // (acquired with `IrqSavePolicy`) keeps interrupts disabled.
                    unsafe { timer_queue.update_platform_timer_locked(token) };
                } else {
                    // SAFETY: As above, `timer_queue` is the current CPU's queue and interrupts
                    // are disabled.
                    let next_timer_deadline = unsafe { timer_queue.next_timer_deadline() };
                    if next_timer_deadline == ZX_TIME_INFINITE {
                        ltracef!(
                            "clearing old hw timer, preempt timer not set, nothing in the queue\n"
                        );
                        platform_stop_timer();
                    }
                }
            }
        } else {
            // The timer was not in a queue. This means it either has already fired,
            // or is currently firing on another CPU.
            callback_not_running = false;
        }

        guard.as_mut().release();

        // Wait for the timer to become un-busy in case a callback is currently active
        // on another cpu. This ensures that after cancel() returns, it is safe to
        // destroy the timer or its arguments.
        while self.active_cpu.load_unlocked() != INVALID_CPU {
            libarch::r#yield();
        }

        // Zero out as defense-in-depth so that incorrect reuse results in a null
        // dereference rather than executing stale code.
        self.callback.store(None, Ordering::Relaxed);
        self.arg.store(ptr::null_mut(), Ordering::Relaxed);

        callback_not_running
    }

    /// Special helper routine to simultaneously try to acquire a [`ChainLock`] and check for timer
    /// cancel, which is needed in a few special cases. Returns `Ok(())` if `lock` was acquired, or
    /// `Err(Status::TIMED_OUT)` if the timer was canceled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - This method is called from within `self`'s own currently executing timer callback.
    /// - [`TimerLock`] is not held by the current CPU.
    /// - A [`ChainLockTransaction`] is currently active on the current CPU and has not yet been
    ///   finalized.
    /// - `lock` is not already held by the current transaction.
    /// - If `Ok(())` is returned, the caller assumes responsibility for releasing `lock`.
    pub unsafe fn trylock_or_cancel(&self, lock: &ChainLock) -> Result<(), Status> {
        // SAFETY: The caller guarantees that a `ChainLockTransaction` is active on the current CPU
        // for the duration of this call.
        let clt_token = unsafe { ChainLockTransaction::mark_active() };

        // Just like the `TrylockOrCancel(MonitoredSpinLock*)` overload, but this version
        // operates on a ChainLock instead of a MonitoredSpinLock, and is used by the wait queue
        // timeout mechanism.  Keep attempting to acquire the lock, yielding in between attempts,
        // and bailing out in the case that we notice that the cancel flag has been set.
        while !lock.try_acquire(&clt_token) {
            if self.cancel_pending() {
                return Err(Status::TIMED_OUT);
            }
            libarch::r#yield();
        }

        Ok(())
    }

    /// Returns true if a cancel of this timer is pending.
    ///
    /// This is meant to be called outside of [`TimerLock`].
    #[inline]
    fn cancel_pending(&self) -> bool {
        self.cancel.load_unlocked()
    }

    /// Returns the applied slack adjustment, for timer tests.
    ///
    /// # Safety
    ///
    /// No operation that may modify the timer's slack ([`Self::set_deadline`] or
    /// [`TimerQueue::transition_off_cpu`]) may run concurrently.
    unsafe fn slack_for_test(&self) -> DurationUnknown {
        // SAFETY: `KCell` and `UnsafeCell` are `repr(transparent)`, so `self.slack` has the layout
        // of a `Cell<DurationUnknown>`, and the caller guarantees that nothing writes it
        // concurrently.
        unsafe { &*ptr::from_ref(&self.slack).cast::<Cell<DurationUnknown>>() }.get()
    }

    /// Returns a monotonic instant if `expected_clock_id` is [`ZX_CLOCK_MONOTONIC`] and a boot
    /// instant if `expected_clock_id` is [`ZX_CLOCK_BOOT`], for timer tests.
    ///
    /// # Safety
    ///
    /// No operation that may modify the timer's scheduled time ([`Self::set_deadline`] or
    /// [`TimerQueue::transition_off_cpu`]) may run concurrently.
    unsafe fn scheduled_time_for_test(&self, expected_clock_id: u32) -> InstantUnknown {
        debug_assert!(self.clock_id == expected_clock_id);
        // SAFETY: `KCell` and `UnsafeCell` are `repr(transparent)`, so `self.scheduled_time` has
        // the layout of a `Cell<InstantUnknown>`, and the caller guarantees that nothing writes it
        // concurrently.
        unsafe { &*ptr::from_ref(&self.scheduled_time).cast::<Cell<InstantUnknown>>() }.get()
    }

    /// We ensure that timers are not on a list or an active cpu when destroyed.
    fn teardown(&self) {
        // Ensure that we are not on any TimerQueue's list.
        debug_assert!(!self.node.in_container());
        // Ensure that we are not active on some cpu.
        debug_assert!(self.active_cpu.load_unlocked() == INVALID_CPU);
    }
}

#[pinned_drop]
impl PinnedDrop for Timer {
    fn drop(self: Pin<&mut Self>) {
        self.teardown();
    }
}

type TimerList = DoublyLinkedList<NonNull<Timer>, TimerQueueTag>;

/// Preemption Timers
///
/// Each CPU has a dedicated preemption timer that's managed using specialized functions (prefixed
/// with `preempt_`).
///
/// Preemption timers are different from general timers. Preemption timers:
///
/// - are reset frequently by the scheduler so performance is important
/// - should not be migrated off their CPU when the CPU is shutdown
///
/// Note: A preemption timer may fire even after it has been canceled.
///
/// `TimerQueue`s are constructed in place by the C++ `percpu` constructor (through
/// [`rust_timer_queue_init`]) and live in [`PerCpu`] for the lifetime of the system, so they are
/// never moved.
#[pin_data]
#[repr(C)]
pub struct TimerQueue {
    /// Timers on the monotonic timeline are placed in this list.
    #[pin]
    monotonic_timer_list: KCell<TimerList, TimerLock>,

    /// Timers on the boot timeline are placed in this list.
    #[pin]
    boot_timer_list: KCell<TimerList, TimerLock>,

    /// This TimerQueue's preemption deadline. `ZX_TIME_INFINITE` means not set.
    ///
    /// Only accessed by the owning CPU with interrupts disabled (or, by
    /// [`TimerQueue::transition_off_cpu`], while the owning CPU is offline).
    preempt_timer_deadline: UnsafeCell<InstantMono>,

    /// This TimerQueue's deadline for its platform timer or `ZX_TIME_INFINITE` if not set.
    /// The deadline is stored in raw platform ticks, as that is the unit used by the
    /// `platform_set_oneshot_timer` API.
    ///
    /// Only accessed by the owning CPU with interrupts disabled (or, by
    /// [`TimerQueue::transition_off_cpu`], while the owning CPU is offline).
    next_timer_deadline: UnsafeCell<i64>,
}

// The C++ `TimerQueue` is `OpaqueStorage<32, 8>`. `percpu.rs` also checks the size and alignment
// against the bindgen view of the C++ class.
static_assert!(size_of::<TimerQueue>() == 32);
static_assert!(align_of::<TimerQueue>() == 8);

impl TimerQueue {
    /// Constructs a new empty `TimerQueue` in place.
    #[inline]
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            monotonic_timer_list <- kcell_init(TimerList::new()),
            boot_timer_list <- kcell_init(TimerList::new()),
            preempt_timer_deadline: UnsafeCell::new(InstantMono(ZX_TIME_INFINITE)),
            next_timer_deadline: UnsafeCell::new(ZX_TIME_INFINITE),
        })
    }

    /// Returns `timer_list` for reading.
    #[inline]
    fn list<'a>(
        timer_list: &'a KCell<TimerList, TimerLock>,
        token: &'a LockToken<'_, TimerLock>,
    ) -> &'a TimerList {
        // SAFETY: `TimerLock` is a singleton, so `token` proves that the lock guarding every timer
        // list is held.
        unsafe { timer_list.get(token) }
    }

    /// Returns `timer_list` for mutation.
    #[inline]
    fn list_mut<'a>(
        timer_list: &'a KCell<TimerList, TimerLock>,
        token: &'a mut LockToken<'_, TimerLock>,
    ) -> Pin<&'a mut TimerList> {
        // SAFETY: `TimerLock` is a singleton, so `token` proves that the lock guarding every timer
        // list is held. Timer lists only live in `TimerQueue`s, which are never moved.
        unsafe { timer_list.get_pinned_mut_locked(token) }
    }

    /// Returns the timer following `entry` in `timer_list`, or `None` if `entry` is the last one.
    ///
    /// # Safety
    ///
    /// `entry` must be a member of `timer_list`.
    #[inline]
    unsafe fn next_in_list(
        token: &mut LockToken<'_, TimerLock>,
        timer_list: &KCell<TimerList, TimerLock>,
        entry: &Timer,
    ) -> Option<NonNull<Timer>> {
        // SAFETY: The caller guarantees that `entry` is a member of `timer_list`.
        let mut cursor = unsafe { Self::list_mut(timer_list, token).cursor_at(entry) };
        cursor.move_next();
        cursor.get().map(NonNull::from)
    }

    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled, or the
    /// owning CPU must be offline.
    #[inline]
    unsafe fn preempt_timer_deadline(&self) -> InstantMono {
        // SAFETY: The caller guarantees exclusive access to the owning CPU's state.
        unsafe { *self.preempt_timer_deadline.get() }
    }

    /// # Safety
    ///
    /// Same as [`Self::preempt_timer_deadline`].
    #[inline]
    unsafe fn set_preempt_timer_deadline(&self, deadline: InstantMono) {
        // SAFETY: The caller guarantees exclusive access to the owning CPU's state.
        unsafe { *self.preempt_timer_deadline.get() = deadline };
    }

    /// # Safety
    ///
    /// Same as [`Self::preempt_timer_deadline`].
    #[inline]
    unsafe fn next_timer_deadline(&self) -> i64 {
        // SAFETY: The caller guarantees exclusive access to the owning CPU's state.
        unsafe { *self.next_timer_deadline.get() }
    }

    /// # Safety
    ///
    /// Same as [`Self::preempt_timer_deadline`].
    #[inline]
    unsafe fn set_next_timer_deadline(&self, deadline: i64) {
        // SAFETY: The caller guarantees exclusive access to the owning CPU's state.
        unsafe { *self.next_timer_deadline.get() = deadline };
    }

    /// Converts the given monotonic timestamp to the raw ticks value it corresponds to.
    /// Returns `None` if the monotonic clock is paused.
    fn convert_monotonic_time_to_raw_ticks(mono: InstantMono) -> Option<i64> {
        // Do not attempt to convert the sentinel value of ZX_TIME_INFINITE.
        if mono.0 == ZX_TIME_INFINITE {
            return Some(ZX_TIME_INFINITE);
        }
        let deadline_mono_ticks =
            timer_get_ticks_to_time_ratio().inverse().scale::<{ Round::UP }>(mono.0);
        timer_convert_mono_to_raw_ticks(InstantMonoTicks(deadline_mono_ticks))
    }

    /// Converts the given boot timestamp to the raw ticks value it corresponds to.
    fn convert_boot_time_to_raw_ticks(boot: InstantBoot) -> i64 {
        // Do not attempt to convert the sentinel value of ZX_TIME_INFINITE.
        if boot.0 == ZX_TIME_INFINITE {
            return ZX_TIME_INFINITE;
        }
        let deadline_boot_ticks =
            timer_get_ticks_to_time_ratio().inverse().scale::<{ Round::UP }>(boot.0);
        deadline_boot_ticks.saturating_sub(timer_get_boot_ticks_offset())
    }

    /// `update_platform_timer` updates the platform's oneshot timer to the minimum of:
    /// 1. The scheduled time of the head of the monotonic timer queue.
    /// 2. The scheduled time of the head of the boot timer queue.
    /// 3. The preemption timer deadline.
    ///
    /// This can only be called when interrupts are disabled.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled, and must
    /// not hold [`TimerLock`].
    pub(crate) unsafe fn update_platform_timer(&self) {
        ksync::lock!(let mut guard = TimerLock::lock_policy::<NoIrqSavePolicy>(source_tag!()));
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        unsafe { self.update_platform_timer_locked(guard.as_mut().token_mut()) };
    }

    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled.
    unsafe fn update_platform_timer_locked(&self, token: &mut LockToken<'_, TimerLock>) {
        debug_assert!(ints_disabled());
        let mut timer_deadline: i64 = ZX_TIME_INFINITE;

        // The monotonic deadline should be the minimum of the preemption timer and the front of
        // the monotonic timer list.
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        let mut mono_time_deadline = unsafe { self.preempt_timer_deadline() };
        let monotonic_timer_list = Self::list(&self.monotonic_timer_list, token);
        if let Some(front) = monotonic_timer_list.front() {
            mono_time_deadline =
                mono_time_deadline.min(InstantMono(front.scheduled_time(token).get().0));
        }
        let mono_ticks_deadline = Self::convert_monotonic_time_to_raw_ticks(mono_time_deadline);
        if let Some(mono_ticks_deadline) = mono_ticks_deadline {
            timer_deadline = mono_ticks_deadline;
        }

        // Check if we have a boot timer with a sooner scheduled time and update the timer deadline
        // accordingly.
        let boot_timer_list = Self::list(&self.boot_timer_list, token);
        if let Some(front) = boot_timer_list.front() {
            let boot_deadline = Self::convert_boot_time_to_raw_ticks(InstantBoot(
                front.scheduled_time(token).get().0,
            ));
            timer_deadline = timer_deadline.min(boot_deadline);
        }

        // Update the platform oneshot timer to the resulting timer deadline.
        ltracef!("rescheduling timer for {} ticks from update_platform_timer\n", timer_deadline);
        let _ = platform_set_oneshot_timer(timer_deadline);
        // SAFETY: As above.
        unsafe { self.set_next_timer_deadline(timer_deadline) };
    }

    /// The `update_platform_timer_*` methods are used to update the platform's oneshot timer to
    /// the minimum of the existing deadline (stored in `next_timer_deadline`) and the given
    /// `new_deadline`. The two separate variations of this method are provided for convenience,
    /// so that callers can provide either a montonic or a boot timestamp depending on the context
    /// they're operating in. Using one of these functions is a bit more efficient than calling
    /// [`Self::update_platform_timer`], as they do not need to check the head of both the boot
    /// and monotonic timer queues before updating the platform oneshot timer.
    ///
    /// These can only be called when interrupts are disabled.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled.
    unsafe fn update_platform_timer_boot(&self, new_deadline: InstantBoot) {
        debug_assert!(ints_disabled());

        // Do not set the platform timer if we were passed an infinite deadline.
        if new_deadline.0 == ZX_TIME_INFINITE {
            return;
        }

        let deadline_raw_ticks = Self::convert_boot_time_to_raw_ticks(new_deadline);
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        let next_timer_deadline = unsafe { self.next_timer_deadline() };
        if deadline_raw_ticks < next_timer_deadline {
            ltracef!(
                "rescheduling timer for {} ticks from update_platform_timer_boot, next: {} \n",
                deadline_raw_ticks,
                next_timer_deadline
            );
            let _ = platform_set_oneshot_timer(deadline_raw_ticks);
            // SAFETY: As above.
            unsafe { self.set_next_timer_deadline(deadline_raw_ticks) };
        }
    }

    /// The `update_platform_timer_*` methods are used to update the platform's oneshot timer to
    /// the minimum of the existing deadline (stored in `next_timer_deadline`) and the given
    /// `new_deadline`. The two separate variations of this method are provided for convenience,
    /// so that callers can provide either a montonic or a boot timestamp depending on the context
    /// they're operating in. Using one of these functions is a bit more efficient than calling
    /// [`Self::update_platform_timer`], as they do not need to check the head of both the boot
    /// and monotonic timer queues before updating the platform oneshot timer.
    ///
    /// These can only be called when interrupts are disabled.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled.
    unsafe fn update_platform_timer_mono(&self, new_deadline: InstantMono) {
        debug_assert!(ints_disabled());

        // Do not set the platform timer if we were passed an infinite deadline.
        if new_deadline.0 == ZX_TIME_INFINITE {
            return;
        }

        // Convert from monotonic time to a raw ticks value to set the platform timer to.
        let deadline_raw_ticks = Self::convert_monotonic_time_to_raw_ticks(new_deadline);
        // Return early if the monotonic clock is paused.
        let Some(deadline_raw_ticks) = deadline_raw_ticks else {
            return;
        };

        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        let next_timer_deadline = unsafe { self.next_timer_deadline() };
        if deadline_raw_ticks < next_timer_deadline {
            ltracef!(
                "rescheduling timer for {} ticks from update_platform_timer_mono, next: {}\n",
                deadline_raw_ticks,
                next_timer_deadline
            );
            let _ = platform_set_oneshot_timer(deadline_raw_ticks);
            // SAFETY: As above.
            unsafe { self.set_next_timer_deadline(deadline_raw_ticks) };
        }
    }

    /// Add `timer` to this TimerQueue, possibly coalescing deadlines as well.
    ///
    /// # Safety
    ///
    /// `timer` must not be in any list, and must stay pinned and alive until it is removed from
    /// the list.
    unsafe fn insert(
        &self,
        token: &mut LockToken<'_, TimerLock>,
        timer: &Timer,
        earliest_deadline: InstantUnknown,
        latest_deadline: InstantUnknown,
    ) {
        debug_assert!(ints_disabled());
        ltracef!(
            "timer {:p}, cpu {}, scheduled {}\n",
            timer,
            curr_cpu_num(),
            timer.scheduled_time(token).get().0
        );
        let timer_list = if timer.clock_id == ZX_CLOCK_MONOTONIC {
            &self.monotonic_timer_list
        } else {
            &self.boot_timer_list
        };
        // SAFETY: The caller upholds the requirements on `timer`.
        unsafe {
            Self::insert_into_timer_list(
                token,
                timer_list,
                timer,
                earliest_deadline,
                latest_deadline,
            )
        };
    }

    /// A helper function for [`Self::insert`] that inserts the given timer into the given timer
    /// list.
    ///
    /// # Safety
    ///
    /// Same as [`Self::insert`].
    unsafe fn insert_into_timer_list(
        token: &mut LockToken<'_, TimerLock>,
        timer_list: &KCell<TimerList, TimerLock>,
        timer: &Timer,
        earliest_deadline: InstantUnknown,
        latest_deadline: InstantUnknown,
    ) {
        // For inserting the timer we consider several cases. In general we
        // want to coalesce with the current timer unless we can prove that
        // either that:
        //  1- there is no slack overlap with current timer OR
        //  2- the next timer is a better fit.
        //
        // In diagrams that follow
        // - Let |e| be the current (existing) timer deadline
        // - Let |t| be the deadline of the timer we are inserting
        // - Let |n| be the next timer deadline if any
        // - Let |x| be the end of the list (not a timer)
        // - Let |(| and |)| the earliest_deadline and latest_deadline.

        let mut iter = Self::list(timer_list, token).front().map(NonNull::from);
        while let Some(entry) = iter {
            // SAFETY: `entry` is a member of `timer_list`, which is guarded by `TimerLock` (held),
            // so it stays alive while it is linked.
            let entry = unsafe { entry.as_ref() };
            // SAFETY: `entry` is a member of `timer_list`.
            let next = unsafe { Self::next_in_list(token, timer_list, entry) };
            iter = next;

            if entry.scheduled_time(token).get() > latest_deadline {
                // New timer latest is earlier than the current timer.
                // Just add upfront as is, without slack.
                //
                //   ---------t---)--e-------------------------------> time
                timer.slack(token).set(DurationUnknown(0));
                // SAFETY: `entry` is a member of `timer_list`, and the caller upholds the
                // requirements on `timer`.
                unsafe {
                    Self::list_mut(timer_list, token)
                        .cursor_at(entry)
                        .insert_before_raw(timer.as_ptr());
                }
                return;
            }

            if entry.scheduled_time(token).get() >= timer.scheduled_time(token).get() {
                //  New timer slack overlaps and is to the left (or equal). We
                //  coalesce with current by scheduling late.
                //
                //  --------(----t---e-)----------------------------> time
                timer
                    .slack(token)
                    .set(entry.scheduled_time(token).get() - timer.scheduled_time(token).get());
                timer.scheduled_time(token).set(entry.scheduled_time(token).get());
                TIMER_COALESCED_COUNTER.add(1);
                // SAFETY: `entry` is a member of `timer_list`, and the caller upholds the
                // requirements on `timer`.
                unsafe {
                    Self::list_mut(timer_list, token)
                        .cursor_at(entry)
                        .insert_after_raw(timer.as_ptr());
                }
                return;
            }

            if entry.scheduled_time(token).get() < earliest_deadline {
                // new timer earliest is later than the current timer. This case
                // is handled in a future iteration.
                //
                //   ----------------e--(---t-----------------------> time
                continue;
            }

            // New timer is to the right of current timer and there is overlap
            // with the current timer, but could the next timer (if any) be
            // a better fit?
            //
            //  -------------(--e---t-----?-------------------> time

            if let Some(next) = next {
                // SAFETY: `next` is a member of `timer_list`, which is guarded by `TimerLock`
                // (held), so it stays alive while it is linked.
                let next = unsafe { next.as_ref() };
                if next.scheduled_time(token).get() <= timer.scheduled_time(token).get() {
                    // The new timer is to the right of the next timer. There is no
                    // chance the current timer is a better fit.
                    //
                    //  -------------(--e---n---t----------------------> time
                    continue;
                }

                if next.scheduled_time(token).get() < latest_deadline {
                    // There is slack overlap with the next timer, and also with the
                    // current timer. Which coalescing is a better match?
                    //
                    //  --------------(-e---t---n-)-----------------------> time
                    let delta_entry =
                        timer.scheduled_time(token).get() - entry.scheduled_time(token).get();
                    let delta_next =
                        next.scheduled_time(token).get() - timer.scheduled_time(token).get();
                    if delta_next < delta_entry {
                        // New timer is closer to the next timer, handle it in the
                        // next iteration.
                        continue;
                    }
                }
            }

            // Handles the remaining cases, note that there is overlap with
            // the current timer.
            //
            //  1- this is the last timer (next == None) or
            //  2- there is no overlap with the next timer, or
            //  3- there is overlap with both current and next but
            //     current is closer.
            //
            //  So we coalesce by scheduling early.
            timer
                .slack(token)
                .set(entry.scheduled_time(token).get() - timer.scheduled_time(token).get());
            timer.scheduled_time(token).set(entry.scheduled_time(token).get());
            TIMER_COALESCED_COUNTER.add(1);
            // SAFETY: `entry` is a member of `timer_list`, and the caller upholds the requirements
            // on `timer`.
            unsafe {
                let mut cursor = Self::list_mut(timer_list, token).cursor_at(entry);
                cursor.insert_after_raw(timer.as_ptr());
            }
            return;
        }

        // Walked off the end of the list and there was no overlap.
        timer.slack(token).set(DurationUnknown(0));
        // SAFETY: The caller upholds the requirements on `timer`.
        unsafe { Self::list_mut(timer_list, token).push_back_raw(timer.as_ptr()) };
    }

    /// Set/reset/cancel the preemption timer.
    ///
    /// When the preemption timer fires, the scheduler's [`rust_kernel_scheduler_timer_tick`] is
    /// called. Set the deadline to `ZX_TIME_INFINITE` to cancel the preemption timer. The
    /// scheduler's timer tick may be called spuriously after cancellation.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled.
    pub(crate) unsafe fn preempt_reset(&self, deadline: InstantMono) {
        debug_assert!(ints_disabled());
        ltracef!("preempt timer cpu {} deadline {}\n", curr_cpu_num(), deadline.0);
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        unsafe { self.set_preempt_timer_deadline(deadline) };
        // SAFETY: As above.
        unsafe { self.update_platform_timer_mono(deadline) };
    }

    /// Returns true if the preemption deadline is set and will definitely fire in the future. A
    /// false value does not definitively mean the preempt timer will not fire, as a spurious
    /// expiration is allowed.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self` with interrupts disabled.
    pub(crate) unsafe fn preempt_armed(&self) -> bool {
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        unsafe { self.preempt_timer_deadline() }.0 != ZX_TIME_INFINITE
    }

    /// This is called periodically by [`timer_tick`], which itself is invoked periodically by
    /// some hardware timer.
    ///
    /// # Safety
    ///
    /// The caller must be running on CPU `cpu`, which owns `self`, with interrupts disabled, and
    /// must not hold [`TimerLock`].
    unsafe fn tick(&self, cpu: cpu_num_t) {
        let now = current_mono_time();
        let boot_now = current_boot_time();
        ltracef!("cpu {} now {}\n", cpu, now.0);

        // The platform timer has fired, so no deadline is set.
        // SAFETY: The caller guarantees that this runs on the CPU that owns `self` with interrupts
        // disabled.
        unsafe { self.set_next_timer_deadline(ZX_TIME_INFINITE) };

        // Service the preemption timer before acquiring the timer lock.
        // SAFETY: As above.
        if now >= unsafe { self.preempt_timer_deadline() } {
            // SAFETY: As above.
            unsafe { self.set_preempt_timer_deadline(InstantMono::INFINITE) };
            // SAFETY: `tick` only runs from `timer_tick`, in the current CPU's timer interrupt with
            // interrupts disabled, which is where `rust_kernel_scheduler_timer_tick` must be called
            // once the preemption deadline has passed.
            unsafe { rust_kernel_scheduler_timer_tick(SchedTime::from_raw(now.0)) };
        }

        // Tick both of the timer lists.
        Self::tick_internal(now.into(), cpu, &self.monotonic_timer_list);
        Self::tick_internal(boot_now.into(), cpu, &self.boot_timer_list);

        // Update the platform timer.
        // SAFETY: As above, and `TimerLock` is not held.
        unsafe { self.update_platform_timer() };
    }

    /// This is called by [`Self::tick`], and processes all timers with scheduled times less than
    /// `now`, which is either a monotonic or a boot instant depending on the timeline
    /// `timer_list` is operating on.
    fn tick_internal(
        now: InstantUnknown,
        cpu: cpu_num_t,
        timer_list: &KCell<TimerList, TimerLock>,
    ) {
        ksync::lock!(let mut guard = TimerLock::lock_policy::<NoIrqSavePolicy>(source_tag!()));

        loop {
            let token = guard.as_mut().token_mut();

            // See if there's an event to process.
            let Some(front) = Self::list(timer_list, token).front() else {
                break;
            };

            // SAFETY: `front` is a member of `timer_list`. Once it is removed from the list below,
            // it is marked active on this CPU, so `Timer::cancel` waits for the callback to finish
            // (and the timer to be marked inactive) before the timer may be destroyed.
            let timer = unsafe { NonNull::from(front).as_ref() };
            let scheduled_time = timer.scheduled_time(token).get();

            ltracef!(
                "next item on timer queue {:p} at {} now {} ({}, arg {:p})\n",
                timer,
                scheduled_time.0,
                now.0,
                PrintfPtr(timer.callback.addr(Ordering::Relaxed)),
                timer.arg.load(Ordering::Relaxed)
            );
            if now < scheduled_time {
                break;
            }

            // Process it.
            ltracef!("timer {:p}\n", timer);
            debug_assert!(
                timer.magic == MAGIC,
                "ASSERT: timer failed magic check: timer {:p}, magic {:#x}\n",
                timer,
                timer.magic
            );
            // SAFETY: `timer` is a member of `timer_list`.
            let _ = unsafe { Self::list_mut(timer_list, token).erase(timer) };

            // Mark the timer busy.
            timer.active_cpu.store_locked(cpu, token);
            // Unlocking the spinlock in call_unlocked acts as a release fence.

            // Now that the timer is off of the list, release the spinlock to handle
            // the callback, then re-acquire in case it is requeued.
            guard.as_mut().call_unlocked(|| {
                ltracef!("dequeued timer {:p}, scheduled {}\n", timer, scheduled_time.0);

                inc_timers();
                TIMER_FIRED_COUNTER.add(1);

                ltracef!(
                    "timer {:p} firing callback {}, arg {:p}\n",
                    timer,
                    PrintfPtr(timer.callback.addr(Ordering::Relaxed)),
                    timer.arg.load(Ordering::Relaxed)
                );
                let callback =
                    timer.callback.load(Ordering::Relaxed).expect("timer fired with no callback");
                // SAFETY: `callback` and `arg` were provided together by the timer's owner, which
                // guarantees that the callback is safe to invoke with them until the timer is
                // canceled, and `Timer::cancel` waits for this callback to finish.
                unsafe {
                    callback(timer.as_ptr().as_ptr(), now.0, timer.arg.load(Ordering::Relaxed))
                };

                debug_assert!(ints_disabled());
            });

            let token = guard.as_mut().token_mut();
            // Mark it not busy.
            timer.active_cpu.store_locked(INVALID_CPU, token);
            // TODO(https://fxbug.dev/42142666): Consider whether this DeviceMemoryBarrier is
            // required
            device_memory_barrier();
        }

        // Verify that the head of the timer queue has a scheduled time after now.
        let token = guard.as_mut().token_mut();
        let timer_list = Self::list(timer_list, token);
        if !timer_list.is_empty() {
            debug_assert!(timer_list.front().is_some_and(|t| t.scheduled_time(token).get() > now));
        }
    }

    /// A helper function for [`Self::transition_off_cpu`] that moves all timers from the
    /// `src_list` to the `dst_list`. Returns the Timer at the head of the `dst_list` if it
    /// changed, otherwise returns `None`.
    fn transition_timer_list(
        token: &mut LockToken<'_, TimerLock>,
        src_list: &KCell<TimerList, TimerLock>,
        dst_list: &KCell<TimerList, TimerLock>,
    ) -> Option<NonNull<Timer>> {
        // Keep track of what the first timer in the dst_list was.
        let mut old_head: Option<NonNull<Timer>> = None;
        if !Self::list(dst_list, token).is_empty() {
            old_head = Self::list(dst_list, token).front().map(NonNull::from);
        }

        // Move all the timers from the src_list to the dst_list.
        while let Some(timer) = Self::list_mut(src_list, token).pop_front() {
            // SAFETY: `timer` was a member of `src_list`, so it is alive and pinned until it is
            // removed from a timer list or canceled, which requires `TimerLock` (held).
            let timer = unsafe { timer.as_ref() };
            // We lost the original asymmetric slack information so when we combine them
            // with the other timer queue they are not coalesced again.
            // TODO(cpu): figure how important this case is.
            let deadline = timer.scheduled_time(token).get();
            // SAFETY: `timer` was just removed from `src_list`, so it is not in any list, and it
            // stays pinned and alive as described above.
            unsafe { Self::insert_into_timer_list(token, dst_list, timer, deadline, deadline) };
            // Note, we do not increment the "created" counter here because we are simply moving
            // these timers from one queue to another and we already counted them when they were
            // first created.
        }
        let mut new_head: Option<NonNull<Timer>> = None;
        if !Self::list(dst_list, token).is_empty() {
            new_head = Self::list(dst_list, token).front().map(NonNull::from);
        }

        // If the head of the timer list changed, then we need to return the new head.
        if new_head.is_some() && new_head != old_head {
            return new_head;
        }
        None
    }

    /// Internal routine used when bringing cpus online/offline.
    ///
    /// Moves `source`'s timers (except its preemption timer) to this TimerQueue.
    ///
    /// # Safety
    ///
    /// The caller must be running on the CPU that owns `self`, the CPU that owns `source` must be
    /// offline, and `source` must not be `self`.
    // TODO(https://fxbug.dev/575806167): The caller in idle_power_thread.rs picks `self` with
    // `PerCpu::get_current()` while preemption is enabled, so it may migrate before `TimerLock` is
    // taken. A follow-up should resolve the destination queue inside the lock.
    pub(crate) unsafe fn transition_off_cpu(&self, source: &TimerQueue) {
        assert!(!ptr::eq(self, source));
        ksync::lock!(let mut guard = TimerLock::lock(source_tag!()));
        let token = guard.as_mut().token_mut();

        // Transition both timer lists. This may update the platform timer.
        let new_mono_head = Self::transition_timer_list(
            token,
            &source.monotonic_timer_list,
            &self.monotonic_timer_list,
        );
        if let Some(new_mono_head) = new_mono_head {
            // SAFETY: `new_mono_head` is the head of `self.monotonic_timer_list`, which is guarded
            // by `TimerLock` (held).
            let scheduled_time = unsafe { new_mono_head.as_ref() }.scheduled_time(token).get();
            // SAFETY: The caller guarantees that this runs on the CPU that owns `self`, and
            // holding `TimerLock` (acquired with `IrqSavePolicy`) keeps interrupts disabled.
            unsafe { self.update_platform_timer_mono(InstantMono(scheduled_time.0)) };
        }
        let new_boot_head =
            Self::transition_timer_list(token, &source.boot_timer_list, &self.boot_timer_list);
        if let Some(new_boot_head) = new_boot_head {
            // SAFETY: `new_boot_head` is the head of `self.boot_timer_list`, which is guarded by
            // `TimerLock` (held).
            let scheduled_time = unsafe { new_boot_head.as_ref() }.scheduled_time(token).get();
            // SAFETY: As above.
            unsafe { self.update_platform_timer_boot(InstantBoot(scheduled_time.0)) };
        }

        // The old TimerQueue has no tasks left, so reset the deadlines.
        // SAFETY: The caller guarantees that the CPU that owns `source` is offline.
        unsafe { source.set_preempt_timer_deadline(InstantMono::INFINITE) };
        // SAFETY: As above.
        unsafe { source.set_next_timer_deadline(ZX_TIME_INFINITE) };
    }

    /// A helper function for [`Self::print_timer_queues`] that prints all of the timers in the
    /// given `timer_list` into the given buffer. Also takes in the current time, which is either a
    /// monotonic or a boot instant depending on the timeline the `timer_list` is operating on.
    fn print_timer_list(
        token: &LockToken<'_, TimerLock>,
        now: InstantUnknown,
        timer_list: &KCell<TimerList, TimerLock>,
        buffer: &mut StringFile<'_>,
    ) {
        let mut last = now;
        for t in Self::list(timer_list, token).iter() {
            let scheduled_time = t.scheduled_time(token).get();
            let delta_now = scheduled_time - now;
            let delta_last = scheduled_time - last;
            let _ = writeln!(
                buffer,
                "\ttime {} delta_now {} delta_last {} func {} arg {}",
                scheduled_time.0,
                delta_now.0,
                delta_last.0,
                PrintfPtr(t.callback.addr(Ordering::Relaxed)),
                PrintfPtr(t.arg.load(Ordering::Relaxed).addr())
            );
            last = scheduled_time;
        }
    }

    /// Prints the contents of all timer queues into `buf` and null terminates `buf`.
    fn print_timer_queues(buf: &mut [u8]) {
        let mut buffer = StringFile::new(buf);
        ksync::lock!(let mut guard = TimerLock::lock(source_tag!()));
        let token = guard.as_mut().token_mut();
        for (i, percpu) in PerCpu::iter() {
            if is_cpu_online(i) {
                let _ = writeln!(buffer, "cpu {}:", i);
                Self::print_timer_list(
                    token,
                    current_mono_time().into(),
                    &percpu.timer_queue.monotonic_timer_list,
                    &mut buffer,
                );
                let _ = writeln!(buffer, "boot timers:");
                Self::print_timer_list(
                    token,
                    current_boot_time().into(),
                    &percpu.timer_queue.boot_timer_list,
                    &mut buffer,
                );
            }
        }
        // Null terminate the buffer.
        buffer.take();
    }
}

/// Formats an address like the kernel `printf`'s `%p`: `0` for null, and `0x`-prefixed lowercase
/// hexadecimal otherwise.
struct PrintfPtr(usize);

impl core::fmt::Display for PrintfPtr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == 0 { f.write_str("0") } else { write!(f, "{:#x}", self.0) }
    }
}

/// called at interrupt time to process any pending timers
///
/// # Safety
///
/// Must be called from the timer interrupt, with interrupts disabled.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn timer_tick() {
    debug_assert!(ints_disabled());

    inc_timer_ints();

    let cpu = curr_cpu_num();
    // SAFETY: The caller guarantees that this runs in the timer interrupt with interrupts
    // disabled, so `PerCpu::get_current()` and `cpu` both refer to the calling CPU, which owns
    // its `TimerQueue`, and `TimerLock` is not held.
    unsafe { PerCpu::get_current().timer_queue.tick(cpu) };
}

/// C++ FFI entry point for `Timer::~Timer()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_destroy(timer: &Timer) {
    timer.teardown();
}

/// C++ FFI entry point for `Timer::Set()`.
///
/// # Safety
///
/// Same as [`Timer::set_deadline`], and `callback` must not be null by the time the timer fires.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_set(
    timer: &Timer,
    deadline: &Deadline,
    callback: Option<Callback>,
    arg: *mut c_void,
) {
    // SAFETY: The caller upholds the requirements of `Timer::set`.
    unsafe { timer.set(deadline, callback, arg) }
}

/// C++ FFI entry point for `Timer::Cancel()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_cancel(timer: &Timer) -> bool {
    timer.cancel()
}

/// C++ FFI entry point for `Timer::TrylockOrCancel(ChainLock&)`.
///
/// # Safety
///
/// Same as [`Timer::trylock_or_cancel`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_trylock_or_cancel(
    timer: &Timer,
    lock: &ChainLock,
) -> zx_status_t {
    // SAFETY: The caller upholds the requirements of `Timer::trylock_or_cancel`.
    Status::result_into_raw(unsafe { timer.trylock_or_cancel(lock) })
}

/// C++ FFI entry point used by `Timer::TrylockOrCancel(MonitoredSpinLock*)` to check for a pending
/// cancel.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_cancel_pending(timer: &Timer) -> bool {
    timer.cancel_pending()
}

/// C++ FFI entry point for `Timer::slack_for_test()`.
///
/// # Safety
///
/// Same as [`Timer::slack_for_test`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_slack_for_test(timer: &Timer) -> DurationUnknown {
    // SAFETY: The caller upholds the requirements of `Timer::slack_for_test`.
    unsafe { timer.slack_for_test() }
}

/// C++ FFI entry point for `Timer::scheduled_time_for_test()`.
///
/// # Safety
///
/// Same as [`Timer::scheduled_time_for_test`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_scheduled_time_for_test(
    timer: &Timer,
    expected_clock_id: u32,
) -> InstantUnknown {
    // SAFETY: The caller upholds the requirements of `Timer::scheduled_time_for_test`.
    unsafe { timer.scheduled_time_for_test(expected_clock_id) }
}

/// C++ FFI entry point for `TimerQueue::TimerQueue()`.
///
/// # Safety
///
/// `timer_queue` must point to valid, aligned, writable storage for a [`TimerQueue`] that does not
/// hold a live `TimerQueue`. The initialized `TimerQueue` must not be moved until it is destroyed
/// by [`rust_timer_queue_destroy`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_init(timer_queue: *mut MaybeUninit<TimerQueue>) {
    // SAFETY: The caller guarantees that `timer_queue` points to valid, aligned, writable storage
    // that stays pinned once initialized.
    let slot = unsafe { &mut *timer_queue };
    let _ = slot.write_pin_init(TimerQueue::init());
}

/// C++ FFI entry point for `TimerQueue::~TimerQueue()`.
///
/// # Safety
///
/// `timer_queue` must point to an initialized [`TimerQueue`] with empty timer lists that is not
/// used again.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_destroy(timer_queue: *mut TimerQueue) {
    // SAFETY: The caller guarantees that `timer_queue` points to an initialized `TimerQueue` that
    // is not used again.
    unsafe { ptr::drop_in_place(timer_queue) };
}

/// C++ FFI entry point for `TimerQueue::PreemptReset()`.
///
/// # Safety
///
/// Same as [`TimerQueue::preempt_reset`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_preempt_reset(
    timer_queue: &TimerQueue,
    deadline: InstantMono,
) {
    // SAFETY: The caller upholds the requirements of `TimerQueue::preempt_reset`.
    unsafe { timer_queue.preempt_reset(deadline) }
}

/// C++ FFI entry point for `TimerQueue::PreemptArmed()`.
///
/// # Safety
///
/// Same as [`TimerQueue::preempt_armed`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_preempt_armed(timer_queue: &TimerQueue) -> bool {
    // SAFETY: The caller upholds the requirements of `TimerQueue::preempt_armed`.
    unsafe { timer_queue.preempt_armed() }
}

/// C++ FFI entry point for `TimerQueue::TransitionOffCpu()`.
///
/// # Safety
///
/// Same as [`TimerQueue::transition_off_cpu`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_transition_off_cpu(
    timer_queue: &TimerQueue,
    source: &TimerQueue,
) {
    // SAFETY: The caller upholds the requirements of `TimerQueue::transition_off_cpu`.
    unsafe { timer_queue.transition_off_cpu(source) }
}

/// C++ FFI entry point for `TimerQueue::PrintTimerQueues()`.
///
/// # Safety
///
/// If `len > 0`, `buf` must be non-null and valid for writes of `len` bytes.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_print_timer_queues(buf: *mut c_char, len: usize) {
    let buf: &mut [u8] = if len == 0 {
        &mut []
    } else {
        // SAFETY: `len > 0`, so the caller guarantees that `buf` is non-null and valid for writes
        // of `len` bytes.
        unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) }
    };
    TimerQueue::print_timer_queues(buf);
}

/// C++ FFI entry point for `TimerQueue::UpdatePlatformTimer()`.
///
/// # Safety
///
/// Same as [`TimerQueue::update_platform_timer`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/timer.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_timer_queue_update_platform_timer(timer_queue: &TimerQueue) {
    // SAFETY: The caller upholds the requirements of `TimerQueue::update_platform_timer`.
    unsafe { timer_queue.update_platform_timer() }
}

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_timers(
    _argc: core::ffi::c_int,
    _argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    use core::ffi::CStr;
    use kprint::kprint;
    use page::SIZE as PAGE_SIZE;

    let timer_buffer_size = PAGE_SIZE;

    // allocate a buffer to dump the timer queue into to avoid reentrancy issues with the
    // timer spinlock
    let Ok(mut buf) = kalloc::Box::<[u8]>::try_new_zeroed_slice(timer_buffer_size) else {
        return Status::NO_MEMORY.into_raw();
    };

    TimerQueue::print_timer_queues(&mut buf);

    // SAFETY: `TimerQueue::print_timer_queues` null-terminates `buf`.
    kprint!("{:s}", unsafe { CStr::from_ptr(buf.as_ptr().cast()) });

    drop(buf);

    0
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_TIMERS,
    c"timers".as_ptr(),
    c"dump the current kernel timer queues".as_ptr(),
    cmd_timers,
    crate::console::CMD_AVAIL_NORMAL
);

/// Kernel timer tests.
#[cfg(ktest)]
#[unittest::suite(name = "timer_rust")]
mod tests {
    use crate::kernel::deadline::{SlackMode, TimerSlack};
    use crate::platform_rs::timer::{
        CurrentTicksObservation, DurationMono, DurationUnknown, current_boot_time,
        current_mono_time, timer_current_boot_ticks, timer_current_mono_and_boot_ticks,
        timer_current_mono_ticks, zx_hour, zx_msec, zx_usec,
    };
    use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_false, assert_ge, assert_le, assert_lt, assert_true};
    use zx_types::{ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST};

    #[derive(Default)]
    struct TimerArgs {
        result: AtomicBool,
        timer_fired: AtomicBool,
        remaining: AtomicI32,
    }

    unsafe extern "C" fn oneshot_cb(_timer: *mut Timer, _now: i64, arg: *mut core::ffi::c_void) {
        // SAFETY: arg is a valid pointer to AtomicBool.
        let fired = unsafe { &*(arg as *const AtomicBool) };
        fired.store(true, Ordering::Release);
    }

    unsafe extern "C" fn timer_cb(_timer: *mut Timer, _now: i64, void_arg: *mut core::ffi::c_void) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` for the duration of the timer.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        arg.timer_fired.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn timer_cancel_cb(
        t: *mut Timer,
        _now: i64,
        void_arg: *mut core::ffi::c_void,
    ) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` and `t` points to a valid pinned
        // `Timer`.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        // SAFETY: `t` is a valid pinned `Timer` for the duration of the callback.
        let t = unsafe { &*t };
        arg.result.store(t.cancel(), Ordering::SeqCst);
        arg.timer_fired.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn timer_set_cb(t: *mut Timer, _now: i64, void_arg: *mut core::ffi::c_void) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` for the duration of the timer.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        if arg.remaining.fetch_sub(1, Ordering::SeqCst) >= 1 {
            let deadline = Deadline::after_mono(DurationMono(zx_usec(10)), TimerSlack::none());
            // SAFETY: `t` is a valid pinned `Timer` and `void_arg` remains valid until `t` is
            // cancelled at the end of `set_from_callback`.
            unsafe {
                let t = core::pin::Pin::new_unchecked(&*t);
                t.set_deadline(&deadline, timer_set_cb, void_arg);
            }
        }
    }

    /// Verifies that a scheduled oneshot timer fires and executes its callback.
    #[test]
    fn test_timer_oneshot() {
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let fired = AtomicBool::new(false);
        let arg = &fired as *const AtomicBool as *mut core::ffi::c_void;

        // Schedule timer to fire in 1ms (1,000,000 ns) from now.
        let now = current_mono_time().0;
        // SAFETY: oneshot_cb and arg are valid.
        unsafe {
            timer.as_ref().set_oneshot(now + 1_000_000, oneshot_cb, arg);
        }

        // Poll up to 100ms (100 iterations of 1ms sleep) for the timer to fire.
        let mut success = false;
        for _ in 0..100 {
            if fired.load(Ordering::Acquire) {
                success = true;
                break;
            }
            let _ = crate::kernel::thread::sleep_relative(DurationMono(1_000_000)); // 1ms
        }

        unittest::expect_true!(success);
    }

    /// Verifies that a scheduled timer can be successfully cancelled.
    #[test]
    fn test_timer_cancel() {
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let fired = AtomicBool::new(false);
        let arg = &fired as *const AtomicBool as *mut core::ffi::c_void;

        // Schedule timer to fire in 10 seconds (in the future).
        let now = current_mono_time().0;
        // SAFETY: oneshot_cb and arg are valid.
        unsafe {
            timer.as_ref().set_oneshot(now + 10_000_000_000, oneshot_cb, arg);
        }

        // Cancel it immediately.
        let cancelled = timer.cancel();
        // Cancel should return true.
        unittest::expect_true!(cancelled);

        // Sleep for a short duration to ensure no callback is running.
        let _ = crate::kernel::thread::sleep_relative(DurationMono(1_000_000)); // 1ms

        // Verify that it did not fire.
        unittest::expect_false!(fired.load(Ordering::Acquire));
    }

    // Set a timer and cancel it before the deadline has elapsed.
    /// cancel_before_deadline
    #[test]
    fn cancel_before_deadline() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::after_mono(DurationMono(zx_hour(5)), TimerSlack::none());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        assert_true!(t.cancel());
        assert_false!(arg.timer_fired.load(Ordering::SeqCst));
    }

    // Set a timer and cancel it after it has fired.
    /// cancel_after_fired
    #[test]
    fn cancel_after_fired() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            libarch::r#yield();
        }
        assert_false!(t.cancel());
    }

    // Set a timer and cancel it from its own callback.
    /// cancel_from_callback
    #[test]
    fn cancel_from_callback() {
        let arg = TimerArgs { result: AtomicBool::new(true), ..Default::default() };
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_cancel_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cancel_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            libarch::r#yield();
        }
        assert_false!(t.cancel());
        assert_false!(arg.result.load(Ordering::SeqCst));
    }

    // Set a timer that re-sets itself from its own callback.
    /// set_from_callback
    #[test]
    fn set_from_callback() {
        let arg = TimerArgs { remaining: AtomicI32::new(5), ..Default::default() };
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_set_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_set_cb, (&raw const arg).cast_mut().cast());
        }
        while arg.remaining.load(Ordering::SeqCst) > 0 {
            libarch::r#yield();
        }

        // We cannot assert the return value below because we don't know if the last timer has
        // fired.
        t.cancel();
    }

    /// A timer callback that does nothing.
    ///
    /// # Safety
    ///
    /// Nothing: the arguments are never dereferenced. The function is `unsafe` only to match
    /// [`Callback`].
    unsafe extern "C" fn noop_cb(_timer: *mut Timer, _now: i64, _arg: *mut core::ffi::c_void) {}

    /// print_timer_queue
    #[test]
    fn print_timer_queue() {
        // Allocate a bunch of timers and a small buffer.  Set the timers then see that
        // `TimerQueue::print_timer_queues` doesn't overflow the buffer.
        const NUM_TIMERS: usize = 1000;
        let timers = kalloc::Box::<[Timer]>::try_new_uninit_slice(NUM_TIMERS);
        assert_true!(timers.is_ok());
        let Ok(mut timers) = timers else { return false };
        for slot in timers.iter_mut() {
            // SAFETY: `slot` is valid, uninitialized storage for a `Timer`. The `Timer` is never
            // moved out of this heap allocation, and it is dropped before the allocation is
            // freed, so it stays pinned for as long as it lives.
            let slot: &'static mut MaybeUninit<Timer> = unsafe { &mut *ptr::from_mut(slot) };
            let _ = slot.write_pin_init(Timer::init(ZX_CLOCK_MONOTONIC));
        }
        // SAFETY: Every element was initialized above.
        let timers = unsafe { timers.assume_init() };
        const BUFFER_SIZE: usize = 4096;
        let buffer = kalloc::Box::<[u8]>::try_new_zeroed_slice(BUFFER_SIZE);
        assert_true!(buffer.is_ok());
        let Ok(mut buffer) = buffer else { return false };
        // Fill the buffer with a pattern so we can detect overflow.
        buffer.fill(b'X');

        for timer in timers.iter() {
            // SAFETY: `timer` stays pinned in its heap allocation and is canceled by `cleanup`
            // before it is dropped, and `noop_cb` never dereferences its arguments.
            unsafe {
                Pin::new_unchecked(timer).set_deadline(
                    &Deadline::infinite(),
                    noop_cb,
                    core::ptr::null_mut(),
                );
            }
        }
        let _cleanup = zr::defer(|| {
            for timer in timers.iter() {
                timer.cancel();
            }
        });

        // Tell `TimerQueue::print_timer_queues` the buffer is one less than it really is.
        TimerQueue::print_timer_queues(&mut buffer[..BUFFER_SIZE - 1]);

        // See that our sentinel was not overwritten.
        assert_eq!(b'X', buffer[BUFFER_SIZE - 1]);

        // See that a null terminator was written to the last available position.
        assert_eq!(0, buffer[BUFFER_SIZE - 2]);
    }

    /// Deadline::after
    #[test]
    fn deadline_after() {
        let slack_modes: [Option<TimerSlack>; 5] = [
            None,                     // None is used for testing the default mode ("none").
            Some(TimerSlack::none()), // an explicit test of "none"
            Some(TimerSlack::new(DurationUnknown(zx_usec(100)), SlackMode::Center)),
            Some(TimerSlack::new(DurationUnknown(zx_usec(200)), SlackMode::Early)),
            Some(TimerSlack::new(DurationUnknown(zx_usec(200)), SlackMode::Late)),
        ];

        // Test to make sure that a relative timeout which is an infinite amount of
        // time from now produces an infinite deadline.
        for slack in &slack_modes {
            let deadline = match slack {
                Some(s) => Deadline::after_mono(DurationMono(ZX_TIME_INFINITE), *s),
                None => Deadline::after_mono(DurationMono(ZX_TIME_INFINITE), TimerSlack::none()),
            };
            assert_eq!(ZX_TIME_INFINITE, deadline.when().0);

            // Default slack should be "none"
            let expected = slack.unwrap_or_else(TimerSlack::none);
            assert_eq!(expected.amount().0, deadline.slack().amount().0);
            assert_eq!(expected.mode(), deadline.slack().mode());
        }

        // While we cannot control the precise deadline which will be produced from
        // our call to Deadline::after, we _can_ bound the range it might exist in.
        // Test for this as well.
        for slack in &slack_modes {
            const TIMEOUT: DurationMono = DurationMono(zx_msec(10));
            let before = current_mono_time() + TIMEOUT;
            let deadline = match slack {
                Some(s) => Deadline::after_mono(TIMEOUT, *s),
                None => Deadline::after_mono(TIMEOUT, TimerSlack::none()),
            };
            let after = current_mono_time() + TIMEOUT;
            assert_le!(before.0, deadline.when().0);
            assert_ge!(after.0, deadline.when().0);

            // Default slack should be "none"
            let expected = slack.unwrap_or_else(TimerSlack::none);
            assert_eq!(expected.amount().0, deadline.slack().amount().0);
            assert_eq!(expected.mode(), deadline.slack().mode());
        }
    }

    /// mono_to_raw_ticks_overflow
    #[test]
    fn mono_to_raw_ticks_overflow() {
        // Verify that converting ZX_TIME_INFINITE and ZX_TIME_INFINTE - 1 returns ZX_TIME_INFINITE
        // instead of overflowing.
        let raw_ticks = timer_convert_mono_to_raw_ticks(InstantMonoTicks(ZX_TIME_INFINITE));
        assert_true!(raw_ticks.is_some());
        assert_eq!(raw_ticks.unwrap_or_default(), ZX_TIME_INFINITE);

        let raw_ticks = timer_convert_mono_to_raw_ticks(InstantMonoTicks(ZX_TIME_INFINITE - 1));
        assert_true!(raw_ticks.is_some());
        assert_ge!(raw_ticks.unwrap_or_default(), ZX_TIME_INFINITE - 1);

        // Verify that 0 gives us a raw ticks greater than or equal to 0, as the conversion
        // function should add an offset that is greater than or equal to 0.
        let raw_ticks = timer_convert_mono_to_raw_ticks(InstantMonoTicks(0));
        assert_true!(raw_ticks.is_some());
        assert_ge!(raw_ticks.unwrap_or(-1), 0);

        // Verify that ZX_TIME_INFINITE_PAST and ZX_TIME_INFINITE_PAST + 1 return negative numbers,
        // since the mono_ticks_modifier should be much smaller than this value.
        let raw_ticks = timer_convert_mono_to_raw_ticks(InstantMonoTicks(ZX_TIME_INFINITE_PAST));
        assert_true!(raw_ticks.is_some());
        assert_lt!(raw_ticks.unwrap_or_default(), 0);

        let raw_ticks =
            timer_convert_mono_to_raw_ticks(InstantMonoTicks(ZX_TIME_INFINITE_PAST + 1));
        assert_true!(raw_ticks.is_some());
        assert_lt!(raw_ticks.unwrap_or_default(), 0);
    }

    /// test_timer_current_mono_and_boot_ticks
    #[test]
    fn test_timer_current_mono_and_boot_ticks() {
        // Get the current monotonic and boot ticks. This should occur prior to our observation of
        // both below, providing us with a lower bound on those values.
        let boot_before = timer_current_boot_ticks();
        let mono_before = timer_current_mono_ticks();

        // Perform a synchronized read of the monotonic and boot ticks.
        let obs: CurrentTicksObservation = timer_current_mono_and_boot_ticks();

        // Get the current monotonic and boot ticks. This should occur after our observation of
        // both above, providing us with an upper bound on those values.
        let mono_after = timer_current_mono_ticks();
        let boot_after = timer_current_boot_ticks();

        // Ensure that the monotonic ticks are less than or equal to the boot ticks.
        assert_le!(obs.mono_now, obs.boot_now);

        // Ensure that our observations are monotonic, meaning that they are greater than or equal
        // to our before observations and less than or equal to our after observations.
        assert_ge!(obs.mono_now, mono_before.0);
        assert_ge!(obs.boot_now, boot_before.0);
        assert_le!(obs.mono_now, mono_after.0);
        assert_le!(obs.boot_now, boot_after.0);
    }

    // Ensure that a boot timer fires.
    /// boot_timer
    #[test]
    fn boot_timer() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_BOOT));
        let deadline = Deadline::no_slack(current_boot_time().into());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            libarch::r#yield();
        }
    }

    unsafe extern "C" {
        /// Returns a never-set [`ZX_CLOCK_MONOTONIC`] `Timer` whose storage was
        /// constant-initialized by the C++ `constexpr` `Timer` constructor.
        fn cpp_timer_constinit_monotonic_timer_for_test() -> *const Timer;

        /// Returns a never-set [`ZX_CLOCK_BOOT`] `Timer` whose storage was constant-initialized by
        /// the C++ `constexpr` `Timer` constructor.
        fn cpp_timer_constinit_boot_timer_for_test() -> *const Timer;
    }

    // Check that a C++ Timer constant-initialized by the constexpr constructor is in the state
    // produced by `Timer::init`, which validates the offsets that the C++ initializer writes.
    /// constinit_timer
    #[test]
    fn constinit_timer() {
        // SAFETY: These functions have no preconditions.
        let timers = unsafe {
            [
                (ZX_CLOCK_MONOTONIC, cpp_timer_constinit_monotonic_timer_for_test()),
                (ZX_CLOCK_BOOT, cpp_timer_constinit_boot_timer_for_test()),
            ]
        };
        for (clock_id, timer) in timers {
            assert_false!(timer.is_null());
            // SAFETY: `timer` is non-null, so it points to a static `Timer`.
            let timer = unsafe { &*timer };

            assert_eq!(MAGIC, timer.magic);
            assert_eq!(clock_id, timer.clock_id);
            assert_eq!(INVALID_CPU, timer.active_cpu.load_unlocked());
            assert_false!(timer.cancel.load_unlocked());
            // SAFETY: The timer is never set, so nothing modifies its scheduled time.
            let scheduled_time = unsafe { timer.scheduled_time_for_test(clock_id) };
            assert_eq!(0, scheduled_time.0);
            // SAFETY: The timer is never set, so nothing modifies its slack.
            let slack = unsafe { timer.slack_for_test() };
            assert_eq!(0, slack.0);
            assert_true!(timer.callback.load(Ordering::Relaxed).is_none());
            assert_true!(timer.arg.load(Ordering::Relaxed).is_null());
            assert_false!(timer.node.in_container());
        }
    }

    // Check that `rust_timer_queue_init`, which backs the C++ TimerQueue constructor, creates an
    // empty TimerQueue with no deadlines set.
    /// timer_queue_init
    #[test]
    fn timer_queue_init() {
        let mut storage = MaybeUninit::<TimerQueue>::uninit();
        // SAFETY: `storage` is valid storage for a `TimerQueue`, and is not moved before
        // `rust_timer_queue_destroy` below.
        unsafe { rust_timer_queue_init(&mut storage) };
        // SAFETY: `rust_timer_queue_init` initialized `storage`.
        let timer_queue = unsafe { storage.assume_init_ref() };

        {
            ksync::lock!(let mut guard = TimerLock::lock(source_tag!()));
            let token = guard.as_mut().token_mut();
            assert_true!(TimerQueue::list(&timer_queue.monotonic_timer_list, token).is_empty());
            assert_true!(TimerQueue::list(&timer_queue.boot_timer_list, token).is_empty());
        }

        // SAFETY: Only this test can see `timer_queue`, so nothing else accesses its deadlines.
        let preempt_timer_deadline = unsafe { timer_queue.preempt_timer_deadline() };
        assert_eq!(ZX_TIME_INFINITE, preempt_timer_deadline.0);
        // SAFETY: Only this test can see `timer_queue`, so nothing else accesses its deadlines.
        let next_timer_deadline = unsafe { timer_queue.next_timer_deadline() };
        assert_eq!(ZX_TIME_INFINITE, next_timer_deadline);

        // SAFETY: `storage` holds an initialized `TimerQueue` with empty timer lists, which is not
        // used again.
        unsafe { rust_timer_queue_destroy(storage.as_mut_ptr()) };
    }
}
