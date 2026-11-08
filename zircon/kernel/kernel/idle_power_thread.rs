// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Platform-independent per-CPU idle/power thread and system suspend coordinator.

use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use debug::{dprintf, kernel_oops};
use idle_power_thread_bindings as bindings;
use kprint::kprintln;
use ksync::LockToken;
use lazy_init::{AtomicCheck, LazyInit};
use pin_init::PinInit;
use zr::static_assert;
use zx_status::Status;
use zx_types::{
    ZX_KOID_KERNEL, ZX_TIME_INFINITE, zx_duration_mono_t, zx_instant_boot_t, zx_instant_mono_t,
    zx_status_t,
};
#[cfg(console_enabled)]
use zx_types::{ZX_OK, zx_duration_boot_t};

use crate::arch_rs::{
    InterruptDisableGuard, blocking_disallowed, curr_cpu_num, enter_idle_state,
    flush_state_and_halt, ints_disabled,
};
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::mp::{self, AutounsignalMpUnplugEvent, platform_mp_cpu_hotplug};
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler::{SchedDuration, peek_active_mask};
use crate::kernel::thread::{
    AutoPreemptDisabler, Thread, ThreadPtr, current_preemption_state, preempt_is_enabled,
    reschedule, sleep,
};
use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t, cpu_num_t, cpu_num_to_mask, highest_cpu_set};
use crate::kernel::wait_queue::Interruptible;
use crate::pdev_power::PowerCpuState;
use crate::platform_rs::power::{
    PlatformAllowDomainPowerDown, platform_get_cpu_state, platform_supports_suspend_cpu,
    platform_supports_suspend_system, platform_suspend_cpu, platform_suspend_system,
};
#[cfg(console_enabled)]
use crate::platform_rs::timer::zx_nsec;
use crate::platform_rs::timer::{
    DurationMono, InstantBoot, InstantMono, current_boot_time, current_mono_time,
    timer_get_mono_ticks_offset, timer_is_monotonic_paused, timer_pause_monotonic,
    timer_unpause_monotonic, zx_min, zx_msec, zx_sec, zx_usec,
};
use crate::suspend_wakeup_timer::{Callback as SuspendWakeupTimerCallback, SuspendWakeupTimer};
use crate::userabi::vdso::VDso;
use crate::wake_vector::{Diagnostics, WakeEvent, WakeResult};

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Placement-constructs an `IdlePowerThread` in uninitialized storage.
    fn cpp_idle_power_thread_init(out: *mut MaybeUninit<IdlePowerThread>);
    /// Initializes the global `IdlePowerThread::resume_timer_wake_vector_` singleton.
    fn cpp_resume_timer_wake_vector_init();
    /// Returns a pointer to the `WakeEvent` embedded in `resume_timer_wake_vector_`.
    fn cpp_resume_timer_wake_vector_get_wake_event() -> *const WakeEvent;
}

/// Whether run-loop `ktrace` scopes for state transitions and idle periods are enabled.
const ENABLE_RUNLOOP_TRACING: bool = false;

/// How long to sleep between polls when waiting for a CPU to reach some state (e.g. waiting for
/// the platform to report that it is powered off).
const CPU_STATE_POLL_INTERVAL: zx_duration_mono_t = zx_usec(200);

/// How often to log a message while still waiting for a CPU to power off.
const CPU_POWER_OFF_WAIT_LOG_INTERVAL: zx_duration_mono_t = zx_sec(5);

/// Initial value of `SYSTEM_SUSPEND_STATE`: system is active with zero pending wake events.
const SYSTEM_SUSPEND_STATE_ACTIVE: u64 = 0;

/// High bit of `SYSTEM_SUSPEND_STATE` indicating that system suspend is currently active.
const SYSTEM_SUSPEND_STATE_SUSPENDED_BIT: u64 = 1u64 << 63;

/// Bitmask covering the pending wake event counter in `SYSTEM_SUSPEND_STATE`.
const SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK: u64 = SYSTEM_SUSPEND_STATE_SUSPENDED_BIT - 1;

crate::ktrace_rs::declare_interned_string!(STATE_OFFLINE, "offline");
crate::ktrace_rs::declare_interned_string!(STATE_SUSPEND, "suspend");
crate::ktrace_rs::declare_interned_string!(STATE_ACTIVE, "active");
crate::ktrace_rs::declare_interned_string!(STATE_WAKEUP, "wakeup");

/// The current or target high-level operational state of the CPU. The depth and type of
/// idle/suspend states depend on inputs from other subsystems, such as timer and run queues and
/// system power targets.
///
/// TODO(https://fxbug.dev/42086705): Rationalize this state with mp active / online states.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum State {
    /// CPU is actively participating in scheduling.
    #[default]
    Active = 0,
    /// Similar to `Active` with respect to scheduling, but signals that a wakeup occurred.
    Wakeup = 1,
    /// CPU is offline and halted.
    Offline = 2,
    /// CPU is in a low-power suspended state.
    Suspend = 3,
}

impl State {
    /// Converts a raw byte into a [`State`].
    ///
    /// Panics if `raw` does not correspond to a valid [`State`] discriminant.
    const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Active,
            1 => Self::Wakeup,
            2 => Self::Offline,
            3 => Self::Suspend,
            _ => panic!("invalid IdlePowerThread::State value"),
        }
    }

    /// Returns the interned `ktrace` string corresponding to `self`.
    fn as_interned_string(self) -> &'static crate::ktrace_rs::InternedString {
        match self {
            Self::Offline => STATE_OFFLINE,
            Self::Suspend => STATE_SUSPEND,
            Self::Active => STATE_ACTIVE,
            Self::Wakeup => STATE_WAKEUP,
        }
    }

    /// Returns the human-readable string name of `self`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Suspend => "suspend",
            Self::Active => "active",
            Self::Wakeup => "wakeup",
        }
    }
}

/// Evaluates to `true` if the snapshotted state indicates the power thread should be scheduled
/// instead of eligible work because it needs to perform a transition or power management function.
/// Provides accessors to the snapshotted state to facilitate efficient asserts in code paths that
/// have preconditions that should be checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingPowerWorkPredicate {
    current: State,
    target: State,
}

impl PendingPowerWorkPredicate {
    /// Creates a predicate from a snapshotted `(current, target)` state pair.
    #[inline]
    pub const fn new(current: State, target: State) -> Self {
        Self { current, target }
    }

    /// Returns `true` if pending power work requires scheduling the idle/power thread.
    #[inline]
    pub const fn is_pending(self) -> bool {
        (!matches!(self.current, State::Active | State::Wakeup))
            || (self.current as u8 != self.target as u8)
    }

    /// Returns the snapshotted current [`State`].
    #[inline]
    pub const fn current(self) -> State {
        self.current
    }

    /// Returns the snapshotted target [`State`].
    #[inline]
    pub const fn target(self) -> State {
        self.target
    }
}

/// The result of a transition request, including the status code of the request and the current
/// state when the request was made.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransitionResult {
    /// Status code of the transition request.
    pub(crate) status: Result<(), Status>,
    /// Observed state of the CPU when the transition started or completed.
    pub(crate) starting_state: State,
}

/// FFI representation of [`TransitionResult`] returned across `extern "C"` boundaries.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IdlePowerThreadTransitionResult {
    /// Raw `zx_status_t` status code.
    pub status: zx_status_t,
    /// Raw `State` discriminant (`u8`).
    pub starting_state: u8,
}

static_assert!(
    size_of::<IdlePowerThreadTransitionResult>()
        == size_of::<bindings::IdlePowerThreadTransitionResult>()
);
static_assert!(
    align_of::<IdlePowerThreadTransitionResult>()
        == align_of::<bindings::IdlePowerThreadTransitionResult>()
);
static_assert!(
    offset_of!(IdlePowerThreadTransitionResult, status)
        == offset_of!(bindings::IdlePowerThreadTransitionResult, status)
);
static_assert!(
    offset_of!(IdlePowerThreadTransitionResult, starting_state)
        == offset_of!(bindings::IdlePowerThreadTransitionResult, starting_state)
);

impl From<TransitionResult> for IdlePowerThreadTransitionResult {
    /// Converts a Rust [`TransitionResult`] into its C-ABI representation.
    fn from(res: TransitionResult) -> Self {
        Self {
            status: Status::result_into_raw(res.status),
            starting_state: res.starting_state as u8,
        }
    }
}

/// Whether `transition_active_to_offline_locked` must confirm that the CPU has powered off.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PowerOffConfirmation {
    /// If the platform does not support querying CPU power state, do not wait for confirmation.
    BestEffort,
    /// If the platform does not support querying CPU power state, fail with
    /// `Status::NOT_SUPPORTED` (leaving the CPU `Offline`).
    Required,
}

/// How `suspend_all_active` suspends the system.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SuspendMode {
    /// Transition the active non-boot CPUs to `Suspend`, then suspend the boot CPU with
    /// `platform_suspend_cpu`. See [`IdlePowerThread::transition_all_active_to_suspend`].
    Cpu,
    /// Transition the active non-boot CPUs to `Offline`, then suspend the system from the boot
    /// CPU with `platform_suspend_system`. See [`IdlePowerThread::suspend_system`].
    System,
}

/// Pair of `(current, target)` states stored atomically in [`IdlePowerThread::state`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct StateMachine {
    /// Current operational state of the CPU.
    current: State,
    /// Requested target state of the CPU.
    target: State,
}

impl StateMachine {
    /// Shorthand expression for `(Active, Active)`.
    const ACTIVE: Self = Self { current: State::Active, target: State::Active };
    /// Shorthand expression for `(Active, Suspend)`.
    const ACTIVE_TO_SUSPEND: Self = Self { current: State::Active, target: State::Suspend };
    /// Shorthand expression for `(Wakeup, Wakeup)`.
    const WAKEUP: Self = Self { current: State::Wakeup, target: State::Wakeup };
    /// Shorthand expression for `(Suspend, Suspend)`.
    const SUSPEND: Self = Self { current: State::Suspend, target: State::Suspend };
    /// Shorthand expression for `(Suspend, Wakeup)`.
    const SUSPEND_TO_WAKEUP: Self = Self { current: State::Suspend, target: State::Wakeup };

    /// Encodes `(current, target)` into a native-endian `u16` matching `struct StateMachine`.
    #[inline]
    const fn to_u16(self) -> u16 {
        u16::from_ne_bytes([self.current as u8, self.target as u8])
    }

    /// Decodes a native-endian `u16` into its `(current, target)` `StateMachine` pair.
    #[inline]
    const fn from_u16(raw: u16) -> Self {
        let [current, target] = raw.to_ne_bytes();
        Self { current: State::from_u8(current), target: State::from_u8(target) }
    }
}

static_assert!(size_of::<StateMachine>() == size_of::<u16>());
static_assert!(offset_of!(StateMachine, current) == 0);
static_assert!(offset_of!(StateMachine, target) == 1);

/// Atomic wrapper around [`StateMachine`] backed by an [`AtomicU16`].
#[repr(transparent)]
struct StateMachineAtomic(AtomicU16);

impl StateMachineAtomic {
    /// Atomically loads the current [`StateMachine`] with memory ordering `order`.
    #[inline]
    fn load(&self, order: Ordering) -> StateMachine {
        StateMachine::from_u16(self.0.load(order))
    }

    /// Atomically stores `val` into the [`StateMachine`] with memory ordering `order`.
    #[inline]
    fn store(&self, val: StateMachine, order: Ordering) {
        self.0.store(val.to_u16(), order);
    }

    /// Performs a strong compare-and-exchange updating `expected` on failure.
    #[inline]
    fn compare_exchange_strong(
        &self,
        expected: &mut StateMachine,
        desired: StateMachine,
        success: Ordering,
        failure: Ordering,
    ) -> bool {
        match self.0.compare_exchange(expected.to_u16(), desired.to_u16(), success, failure) {
            Ok(_) => true,
            Err(actual) => {
                *expected = StateMachine::from_u16(actual);
                false
            }
        }
    }
}

crate::ksync_kernel::declare_singleton_mutex!(
    /// Guards the `state` of the subset of secondary (non-boot) CPUs that are actively
    /// transitioning in and out of suspension. Modifications to this participating secondary set
    /// must be performed under this global lock.
    ///
    /// `transition_all_active_to_suspend` and `suspend_system` obtain the global lock when they
    /// obtain the participating secondary set and hold it for the remainder of the method to
    /// prevent CPUs unexpectedly going offline.
    ///
    /// For example, unplugging a CPU by transitioning it from active to offline must be done under
    /// this lock, as otherwise it could modify the participating secondary set. On the other hand,
    /// transitioning a secondary CPU from offline to active need not be done under this lock, as
    /// `transition_all_active_to_suspend` itself ignores additional active CPUs after its
    /// participating set is acquired.
    ///
    /// Note: `suspend_system` cannot ignore additional active CPUs, since
    /// `platform_suspend_system` requires that all secondary CPUs be powered off. If a secondary
    /// CPU is brought online concurrently with `suspend_system`, `platform_suspend_system` will
    /// fail with `Status::BAD_STATE` and the boot CPU will fall back to `platform_suspend_cpu`.
    TransitionLock
);

/// Provides an interlock between pending wake events and system suspend, preventing suspend entry
/// until there are no pending wake events.
static SYSTEM_SUSPEND_STATE: AtomicU64 = AtomicU64::new(SYSTEM_SUSPEND_STATE_ACTIVE);

/// When `true`, the boot CPU's idle/power thread suspends with `platform_suspend_system` rather
/// than `platform_suspend_cpu` when it is in the `Suspend` state.
///
/// Set by `suspend_system` (while holding `TransitionLock`) before transitioning the boot CPU to
/// `Suspend`, and cleared by `suspend_system` once that transition has returned (i.e. once the
/// boot CPU has resumed, or the suspend attempt was aborted or timed out). The boot CPU's
/// idle/power thread also clears it if `platform_suspend_system` fails, so that a persistent
/// failure does not prevent it from suspending. Accesses are ordered with respect to the boot
/// CPU's transitions by `state`.
static SYSTEM_SUSPEND_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A timer on the boot timeline used to resume the system at the given boot time if no other wake
/// events occur by then.
static RESUME_TIMER: LazyInit<SuspendWakeupTimer, AtomicCheck> = LazyInit::uninit();

/// Returns a reference to the bespoke resume timer [`WakeEvent`].
///
/// # Safety
///
/// `LK_INIT_LEVEL_EARLIEST` (`init_hook`) must have completed so that
/// `IdlePowerThread::resume_timer_wake_vector_` is initialized.
#[inline]
unsafe fn resume_timer_wake_event() -> &'static WakeEvent {
    // SAFETY: Caller guarantees `resume_timer_wake_vector_` has been initialized at
    // `LK_INIT_LEVEL_EARLIEST`, and it lives in static storage for the lifetime of the kernel.
    unsafe { &*cpp_resume_timer_wake_vector_get_wake_event() }
}

/// Waits until the platform reports (via `platform_get_cpu_state`) that `cpu` is powered off or
/// stopped, or until `timeout_at`.
///
/// Returns `Ok(())` once the platform reports that `cpu` is powered off or stopped,
/// `Err(Status::TIMED_OUT)` if `timeout_at` passes first, or the error returned by
/// `platform_get_cpu_state` (e.g. `Err(Status::NOT_SUPPORTED)` if the platform cannot report CPU
/// power state).
///
/// Logs a message periodically while waiting so that a platform which never reports the CPU
/// powered off does not cause a silent hang (e.g. if `timeout_at` is `ZX_TIME_INFINITE`).
fn wait_for_cpu_power_off(cpu: cpu_num_t, timeout_at: zx_instant_mono_t) -> Result<(), Status> {
    let timeout_at = InstantMono(timeout_at);
    let start = current_mono_time();
    let log_interval = DurationMono(CPU_POWER_OFF_WAIT_LOG_INTERVAL);
    let poll_interval = DurationMono(CPU_STATE_POLL_INTERVAL);
    let mut log_at = start + log_interval;
    loop {
        let state = platform_get_cpu_state(cpu)?;
        if matches!(state, PowerCpuState::Off | PowerCpuState::Stopped) {
            return Ok(());
        }

        let now = current_mono_time();
        if now >= timeout_at {
            core::hint::cold_path();
            return Err(Status::TIMED_OUT);
        }
        if now >= log_at {
            core::hint::cold_path();
            dprintf!(
                INFO,
                "Still waiting for CPU {} to power off after {} ms (state {})\n",
                cpu,
                (now - start).0 / zx_msec(1),
                state as i32,
            );
            log_at = now + log_interval;
        }
        let _ = sleep((now + poll_interval).min(timeout_at));
    }
}

/// Platform-independent per-CPU idle/power thread state.
///
/// Manages the idle, suspend, and offline functions of a CPU. This thread is scheduled whenever
/// there is no eligible work in the run queues of a CPU or a power state management operation is
/// requested for the CPU. The idle/power thread provides the platform-independent run loop for
/// handling these functions and delegates to the platform-specific subsystems for the relevant
/// operations.
#[repr(C)]
pub struct IdlePowerThread {
    /// Current and target power state of this CPU (see `TransitionLock`).
    state: StateMachineAtomic,
    /// Completion event signaled when requested transitions finish.
    complete: AutounsignalMpUnplugEvent,
    /// Underlying kernel [`Thread`] executing [`IdlePowerThread::run`], wrapped in [`zr::Opaque`]
    /// because `ConstructFirstThread` / `Thread::CreateIdleThread` placement-news it in-place once
    /// during boot before the CPU can ever be observed active, and C++ accesses its interior state
    /// (`Thread::ReviveIdlePowerThread` during CPU hotplug mutates fields in-place under
    /// `thread->get_lock()`).
    thread: zr::Opaque<Thread>,
    /// CPU ID owning this `IdlePowerThread`, initialized once in `percpu` setup.
    this_cpu: UnsafeCell<cpu_num_t>,
}

// SAFETY: `state` and `complete` are internally synchronized via atomics and spinlocks, `Thread`
// is `Send + Sync`, and `this_cpu` is written once during single-threaded per-CPU initialization
// before secondary CPUs start and only read thereafter.
unsafe impl Send for IdlePowerThread {}
// SAFETY: Shared references only mutate state through atomic operations (`state`), internally
// locked primitives (`complete`), or `Thread`'s synchronized APIs.
unsafe impl Sync for IdlePowerThread {}

static_assert!(size_of::<IdlePowerThread>() == bindings::IdlePowerThreadLayout_kSize);
static_assert!(align_of::<IdlePowerThread>() == bindings::IdlePowerThreadLayout_kAlign);
static_assert!(
    size_of::<AutounsignalMpUnplugEvent>() == bindings::IdlePowerThreadLayout_kCompleteSize
);
static_assert!(
    align_of::<AutounsignalMpUnplugEvent>() == bindings::IdlePowerThreadLayout_kCompleteAlign
);
static_assert!(offset_of!(IdlePowerThread, state) == bindings::IdlePowerThreadLayout_kStateOffset);
static_assert!(
    offset_of!(IdlePowerThread, complete) == bindings::IdlePowerThreadLayout_kCompleteOffset
);
static_assert!(
    offset_of!(IdlePowerThread, thread) == bindings::IdlePowerThreadLayout_kThreadOffset
);
static_assert!(
    offset_of!(IdlePowerThread, this_cpu) == bindings::IdlePowerThreadLayout_kThisCpuOffset
);

impl IdlePowerThread {
    /// Constructs a default `IdlePowerThread` in place.
    #[inline]
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_idle_power_thread_init)
    }

    /// Initializes `this_cpu` for this [`IdlePowerThread`] instance during per-CPU construction.
    ///
    /// # Safety
    ///
    /// Must only be called once during per-CPU construction before concurrent access to `self`.
    #[inline]
    pub unsafe fn set_this_cpu(&self, cpu_num: cpu_num_t) {
        // SAFETY: Caller guarantees exclusive access during single-threaded per-CPU construction.
        unsafe { *self.this_cpu.get() = cpu_num };
    }

    /// Returns the CPU number that owns this `IdlePowerThread`.
    #[inline]
    fn cpu_num(&self) -> cpu_num_t {
        // SAFETY: `this_cpu` is written once during `percpu` construction before any
        // `IdlePowerThread` methods run, and is immutable thereafter.
        unsafe { *self.this_cpu.get() }
    }

    /// Performs a compare-and-exchange on `self.state` (see `TransitionLock`).
    #[inline]
    fn compare_exchange_state(&self, expected: &mut StateMachine, desired: StateMachine) -> bool {
        // `AcqRel` on success orders prior state/flag writes (such as
        // `SYSTEM_SUSPEND_REQUESTED`) with the transition; `Acquire` on failure synchronizes
        // with the writer that updated `self.state`.
        self.state.compare_exchange_strong(expected, desired, Ordering::AcqRel, Ordering::Acquire)
    }

    /// Returns a predicate indicating whether the power thread should be scheduled instead of
    /// eligible work because it needs to perform a transition or power management function.
    #[inline]
    pub fn pending_power_work(&self) -> PendingPowerWorkPredicate {
        // `Acquire` synchronizes with state transition requests and completions written to
        // `self.state`.
        let state = self.state.load(Ordering::Acquire);
        PendingPowerWorkPredicate::new(state.current, state.target)
    }

    /// Returns a raw mutable pointer to the underlying [`Thread`] storage.
    #[inline]
    pub fn thread_ptr(&self) -> *mut Thread {
        self.thread.get()
    }

    /// Returns a shared reference to the underlying [`Thread`] instance.
    ///
    /// # Safety
    ///
    /// This CPU's idle [`Thread`] must have been constructed (`ConstructFirstThread` for the boot
    /// CPU or `CreateIdleThread` for secondary CPUs during boot, before the CPU can ever be
    /// observed active).
    #[inline]
    pub unsafe fn thread(&self) -> &Thread {
        // SAFETY: Caller guarantees this CPU's idle `Thread` has been constructed.
        unsafe { &*self.thread.get() }
    }

    /// Returns `true` if the system is currently in the suspended state.
    #[inline]
    fn system_suspended() -> bool {
        // `Acquire` pairs with the `Release` updates to `SYSTEM_SUSPEND_STATE` in
        // `suspend_all_active`, ensuring callers observing the suspended flag also observe all
        // state writes published before suspend entry or teardown.
        (SYSTEM_SUSPEND_STATE.load(Ordering::Acquire) & SYSTEM_SUSPEND_STATE_SUSPENDED_BIT) != 0
    }

    /// Returns the number of currently pending system wake events.
    #[inline]
    pub(crate) fn pending_wake_events() -> u64 {
        // Note: wake-event counter updates in `trigger_system_wake_event` and
        // `acknowledge_system_wake_event` use `Ordering::Relaxed`, so `Acquire` here only pairs
        // with `Release` updates to the suspend bit in `suspend_all_active`, not wake-event
        // increments.
        SYSTEM_SUSPEND_STATE.load(Ordering::Acquire) & SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK
    }

    /// Updates the monotonic clock and platform timer according to `current_cpu` and
    /// `current_state`.
    ///
    /// - If `current_cpu` is the boot CPU and `current_state` is `Suspend`, pauses the monotonic
    ///   clock.
    /// - If `current_cpu` is the boot CPU and `current_state` is `Wakeup`, resumes the monotonic
    ///   clock and updates the boot CPU's platform timer.
    /// - If `current_cpu` is a secondary CPU and `current_state` is `Active`, updates that CPU's
    ///   platform timer.
    ///
    /// The boot CPU is responsible for pausing and resuming the clock since it is the last CPU to
    /// suspend and the first CPU to resume. However, each CPU has its own platform timer that it
    /// must update after the monotonic clock is resumed.
    ///
    /// # Safety
    ///
    /// - Interrupts must be disabled (`ints_disabled()`).
    /// - `current_cpu` must be the calling CPU (`curr_cpu_num()`).
    /// - The global `TimerLock` spinlock (`Timer::TimerLock::Get()`) must not be held on the
    ///   calling CPU.
    /// - If `current_cpu == BOOT_CPU_ID` and `current_state.current == State::Suspend`, the
    ///   monotonic clock must not currently be paused and no concurrent `timer_pause_monotonic` or
    ///   `timer_unpause_monotonic` calls may occur.
    /// - If `current_cpu == BOOT_CPU_ID` and `current_state.current == State::Wakeup`, the
    ///   monotonic clock must currently be paused, raw ticks must not have regressed since pause,
    ///   and no concurrent `timer_pause_monotonic` or `timer_unpause_monotonic` calls may occur.
    unsafe fn update_monotonic_clock(current_cpu: cpu_num_t, current_state: StateMachine) {
        // Currently, `update_platform_timer`, called below, requires interrupts to be
        // disabled as an optimization. Since this method, `update_monotonic_clock`,
        // happens to be called with interrupts disabled in the main `run` loop, nothing
        // else needs to be done here. Simply assert that interrupts are disabled to
        // ensure that any change to the interrupt disable behavior of the run loop is
        // handled appropriately.
        debug_assert!(ints_disabled());

        if current_cpu == BOOT_CPU_ID {
            if current_state.current == State::Suspend {
                // If we are suspending the system, the boot CPU has to pause the monotonic clock.
                // SAFETY: Caller guarantees the monotonic clock is unpaused and no concurrent
                // `timer_pause_monotonic` or `timer_unpause_monotonic` calls occur.
                unsafe { timer_pause_monotonic() };
                return;
            }

            if current_state.current == State::Wakeup {
                // If we are resuming the system, the boot CPU has to resume the monotonic clock.
                // It must also update its platform timer to account for any monotonic timers that
                // are present.
                // SAFETY: Caller guarantees the monotonic clock is paused, raw ticks have not
                // regressed, and no concurrent `timer_pause_monotonic` or
                // `timer_unpause_monotonic` calls occur.
                unsafe { timer_unpause_monotonic() };
                VDso::set_monotonic_ticks_offset(timer_get_mono_ticks_offset());
                // SAFETY: Caller guarantees interrupts are disabled and `TimerLock` is not held
                // on the calling CPU, and `PerCpu::get_current()` returns the calling CPU's
                // `PerCpu`.
                unsafe { PerCpu::get_current().timer_queue.update_platform_timer() };
                return;
            }

            return;
        }

        if current_state.current == State::Active {
            // If we are resuming the system, the secondary CPUs have to reset their platform
            // timers to account for any monotonic timers that are present.
            // SAFETY: Caller guarantees interrupts are disabled and `TimerLock` is not held on
            // the calling CPU, and `PerCpu::get_current()` returns the calling CPU's `PerCpu`.
            unsafe { PerCpu::get_current().timer_queue.update_platform_timer() };
        }
    }

    /// Called by `mp::unplug_current_cpu` to complete the last steps of taking the CPU offline.
    ///
    /// # Safety
    ///
    /// Must be called on the unplugging CPU with interrupts disabled while its target state is
    /// [`State::Offline`].
    pub(crate) unsafe fn flush_and_halt(&self) -> ! {
        debug_assert!(ints_disabled());

        // `Acquire` synchronizes with the transition request that set `target` to `Offline`.
        let state = self.state.load(Ordering::Acquire);
        debug_assert!(state.target == State::Offline);
        // `Release` publishes the completed `(Offline, Offline)` state before halting.
        self.state.store(
            StateMachine { current: State::Offline, target: State::Offline },
            Ordering::Release,
        );

        // SAFETY: Called on the unplugging CPU with interrupts disabled and a valid pointer to
        // this CPU's completion event; never returns.
        unsafe { flush_state_and_halt(self.complete.as_raw_mut()) }
    }

    /// Implements the run loop executed by the CPU's idle/power thread.
    ///
    /// # Safety
    ///
    /// Must be invoked exclusively as the entry point of a per-CPU kernel-only idle/power thread.
    unsafe fn run(_arg: *mut c_void) -> c_int {
        let cpu_num = curr_cpu_num();
        let this_idle_power_thread = &PerCpu::get_current().idle_power_thread;
        let this_scheduler = &PerCpu::get_current().scheduler;

        // The accumulated idle time should always be reset to zero when a CPU comes online. Make
        // sure that CPU hotplug does not leave this member in an inconsistent state with respect
        // to the scheduler bookkeeping when this thread is revived after going offline.
        debug_assert!(this_scheduler.processor_idle_time().raw_value() == 0);

        loop {
            // Disable preemption and interrupts to avoid races between idle power thread requests,
            // handling interrupts, and entering the processor idle state. All pending preemptions
            // are handled at the end of this loop iteration. Pending interrupts may be handled
            // either at the end of this loop iteration or within `enter_idle_state` on
            // architectures that re-enable interrupts there (e.g. x64).
            let _preempt_disabled = AutoPreemptDisabler::new();
            let _interrupt_disable = InterruptDisableGuard::new();

            // `Acquire` synchronizes with transition requests written to `this_idle_power_thread`.
            let mut state = this_idle_power_thread.state.load(Ordering::Acquire);
            let mut state_changed = false;
            let mut transition_success = false;
            if state.target != state.current {
                core::hint::cold_path();
                state_changed = true;
                let trace = crate::ktrace_rs::cpu_begin_scope_cond!(
                    ENABLE_RUNLOOP_TRACING,
                    "kernel:sched",
                    "transition",
                    "from" => state.current.as_interned_string(),
                    "to" => state.target.as_interned_string(),
                );
                dprintf!(
                    INFO,
                    "CPU {}: {} -> {}\n",
                    cpu_num,
                    state.current.as_str(),
                    state.target.as_str(),
                );

                match state.target {
                    State::Active | State::Wakeup | State::Suspend => {
                        // Complete the requested transition, which could be interrupted by a wake
                        // trigger.
                        //
                        // Take care to only update the monotonic clock after we have succeeded in
                        // transitioning to the desired state.
                        let mut expected = state;
                        let desired = StateMachine { current: state.target, target: state.target };
                        transition_success =
                            this_idle_power_thread.compare_exchange_state(&mut expected, desired);
                        if transition_success {
                            debug_assert!(
                                desired.current == desired.target,
                                "current={} target={}",
                                desired.current.as_str(),
                                desired.target.as_str(),
                            );
                            state = desired;
                            // SAFETY: Interrupts are disabled (`AutoInterruptDisable`), `cpu_num`
                            // is the calling CPU, `TimerLock` is not held, and only the boot CPU's
                            // idle/power thread pauses (`State::Suspend`) or unpauses
                            // (`State::Wakeup`) the monotonic clock on successful serialized state
                            // transitions while raw ticks do not regress.
                            unsafe { Self::update_monotonic_clock(cpu_num, desired) };
                        } else {
                            core::hint::cold_path();
                            debug_assert!(
                                expected == StateMachine::SUSPEND_TO_WAKEUP
                                    || expected == StateMachine::WAKEUP,
                                "current={} target={}",
                                expected.current.as_str(),
                                expected.target.as_str(),
                            );
                            state = expected;
                        }
                        // Do not signal completion when the boot CPU transitions to `Suspend`. The
                        // boot CPU is suspended last and should not prematurely signal completion
                        // to `transition_all_active_to_suspend` before actually entering
                        // suspension. If preemption is pending, the boot CPU will abort entering
                        // suspend and loop; delaying the signal allows `wait_deadline` in
                        // `transition_from_to` to observe the canceled state (e.g. `Wakeup`) on
                        // the next transition and return `Status::CANCELED`, or timeout if the
                        // canceled state is not observed.
                        if state.current != State::Suspend || cpu_num != BOOT_CPU_ID {
                            // SAFETY: `this_idle_power_thread.complete` is an initialized per-CPU
                            // `AutounsignalMpUnplugEvent`, the idle/power `Thread` is initialized,
                            // and no `ChainLockTransaction` is active.
                            unsafe { this_idle_power_thread.complete.signal() };
                        }
                    }

                    State::Offline => {
                        // Emit the complete event early, since `unplug_current_cpu` will not
                        // return.
                        drop(trace);

                        // Clear any accumulated idle time to ensure consistency with runtime vs.
                        // idle time asserts in the scheduler when the CPU goes back online.
                        this_scheduler.reset_processor_idle_time();

                        // Updating the state and signaling the complete event is handled by
                        // `mp::unplug_current_cpu` when it calls `flush_and_halt`.
                        debug_assert_ne!(cpu_num, BOOT_CPU_ID);
                        // SAFETY: Running on a non-boot unplugging CPU's idle/power thread after
                        // transitioning to `State::Offline`, with interrupts and preemption
                        // disabled, while the CPU is still marked active in the scheduler (before
                        // `MigrateUnpinnedThreads` deactivates it and sets
                        // `arch_set_blocking_disallowed(true)`), not inside an `mp_sync_exec`
                        // callback, and with no spinlocks, `OwnedWaitQueue`s, or active
                        // `ChainLockTransaction` held.
                        unsafe { mp::unplug_current_cpu() };
                    }
                }

                // If the power thread just transitioned to `Active` or `Wakeup`, reschedule to
                // ensure that the run queue is evaluated. Otherwise, this thread may continue to
                // run the idle loop until the next IPI.
                if state.current == State::Active || state.current == State::Wakeup {
                    reschedule();
                }
            }

            let should_suspend_or_idle =
                !state_changed || (transition_success && state.current == State::Suspend);

            if should_suspend_or_idle {
                debug_assert!(ints_disabled());
                debug_assert!(!preempt_is_enabled());

                // A preempt could become pending between disabling preemption and disabling
                // interrupts above. Handle the preemption instead of attempting to suspend or
                // enter the processor idle state.
                // SAFETY: Interrupts and preemption are disabled on the current CPU, so
                // `current_preemption_state()` points to the current thread's live
                // `PreemptionState`.
                let pending = unsafe { (*current_preemption_state()).preempts_pending() };
                if pending != 0 {
                    core::hint::cold_path();
                    continue;
                }

                // WARNING: Be careful not to do anything that could pend a preemption after the
                // check above.

                if state.current == State::Suspend {
                    core::hint::cold_path();
                    let _trace = crate::ktrace_rs::cpu_begin_scope!("kernel:sched", "suspend");

                    // Make sure our `RESUME_TIMER` has been started if we are the `BOOT_CPU_ID` and
                    // it has a deadline set.
                    if cpu_num == BOOT_CPU_ID {
                        RESUME_TIMER.ensure_started();
                    }

                    // `Relaxed` load suffices because `SYSTEM_SUSPEND_REQUESTED` writes are
                    // ordered before the boot CPU's `state` transition to `Suspend` (`AcqRel`).
                    if cpu_num == BOOT_CPU_ID && SYSTEM_SUSPEND_REQUESTED.load(Ordering::Relaxed) {
                        // `suspend_system` has taken all other CPUs offline. Suspend the whole
                        // system.
                        // SAFETY: Called on the kernel-only boot CPU idle/power thread with
                        // interrupts and preemption disabled after secondary CPUs powered off.
                        if let Err(err) = unsafe { platform_suspend_system() } {
                            core::hint::cold_path();
                            // Retrying is unlikely to help (e.g. another CPU was unexpectedly
                            // online). Fall back to `platform_suspend_cpu` for the remainder of
                            // this suspend operation.
                            //
                            // Logging may pend a preemption (see the warning above), but that is
                            // fine here since we will not suspend during this iteration of the
                            // loop.
                            dprintf!(INFO, "platform_suspend_system failed: {}\n", err.into_raw(),);
                            // `Relaxed` store suffices because only the boot CPU idle thread reads
                            // this flag during the current suspend cycle.
                            SYSTEM_SUSPEND_REQUESTED.store(false, Ordering::Relaxed);
                        }
                    } else if platform_supports_suspend_cpu() {
                        // SAFETY: Called on a kernel-only idle/power thread pinned to this CPU
                        // with interrupts and preemption disabled.
                        let status =
                            unsafe { platform_suspend_cpu(PlatformAllowDomainPowerDown::Yes) };
                        debug_assert!(
                            matches!(
                                status,
                                Ok(()) | Err(Status::ACCESS_DENIED) | Err(Status::INVALID_ARGS)
                            ),
                            "{}",
                            Status::result_into_raw(status),
                        );
                    } else {
                        // SAFETY: Called on this CPU's idle/power thread with interrupts and
                        // preemption disabled.
                        unsafe { enter_idle_state() };
                    }
                } else {
                    let _trace = crate::ktrace_rs::cpu_begin_scope_cond!(
                        ENABLE_RUNLOOP_TRACING,
                        "kernel:sched",
                        "idle",
                    );

                    let idle_start_time = current_mono_time();
                    // SAFETY: Called on this CPU's idle/power thread with interrupts and
                    // preemption disabled.
                    unsafe { enter_idle_state() };
                    let idle_finish_time = current_mono_time();

                    this_scheduler.add_processor_idle_time(SchedDuration::from_raw(
                        (idle_finish_time - idle_start_time).0,
                    ));
                }

                // END WARNING: Pending preemptions is safe again.
            }
        }
    }

    /// Requests to transition from `Active` to `Offline`.
    ///
    /// After the transition completes, waits until the platform reports (via
    /// `platform_get_cpu_state`) that the CPU is powered off or stopped. If the platform does not
    /// support querying CPU power state, this wait is skipped. If the platform never reports that
    /// the CPU is powered off, this waits until `timeout_at` (i.e. indefinitely, if `timeout_at`
    /// is `ZX_TIME_INFINITE`), logging a message periodically.
    ///
    /// Returns:
    ///
    ///   - `{Status::OK, State::Active}` if the transition was successful.
    ///   - `{Status::OK, State::Offline}` if the current state was already `Offline`.
    ///   - `{Status::BAD_STATE, current state}` if the current state was not `Active` or
    ///     `Offline`.
    ///   - `{Status::TIMED_OUT, State::Active}` if the current state was `Active` and transition
    ///     timeout expired. Note that in this case the request is not withdrawn (once the CPU's
    ///     idle/power thread has begun to take it offline, it cannot be stopped), so the CPU may
    ///     still go `Offline` later.
    ///   - `{Status::TIMED_OUT, starting state}` if the transition completed, but the timeout
    ///     expired before the platform reported that the CPU was powered off. Note that in this
    ///     case, the CPU's state is `Offline`.
    ///   - `{error, starting state}` if the transition completed, but querying the CPU's power
    ///     state failed with `error`. Note that in this case, the CPU's state is `Offline`.
    /// # Safety
    ///
    /// `self` must be an initialized per-CPU [`IdlePowerThread`] for a non-boot CPU, and the caller
    /// must execute in a normal thread context where blocking is permitted with no spinlocks,
    /// `TransitionLock`, or active `ChainLockTransaction` held.
    pub(crate) unsafe fn transition_active_to_offline(
        &self,
        timeout_at: zx_instant_mono_t,
    ) -> TransitionResult {
        ksync::lock!(let guard = TransitionLock::get().lock());
        self.transition_active_to_offline_locked(
            timeout_at,
            PowerOffConfirmation::BestEffort,
            guard.token(),
        )
    }

    /// Requests to transition from `Offline` to `Active`.
    ///
    /// Returns:
    ///
    ///   - `{Status::OK, State::Offline}` if the transition was successful.
    ///   - `{Status::OK, State::Active}` if the current state was already `Active`.
    ///   - `{Status::BAD_STATE, current state}` if the current state was not `Active` or
    ///     `Offline`.
    ///   - `{Status::TIMED_OUT, State::Offline}` if the current state was `Offline` and transition
    ///     timeout expired.
    #[inline]
    fn transition_offline_to_active(&self, timeout_at: zx_instant_mono_t) -> TransitionResult {
        self.transition_from_to(State::Offline, State::Active, timeout_at)
    }

    /// Transitions this CPU from `Active` to `Offline` while holding `TransitionLock`.
    fn transition_active_to_offline_locked(
        &self,
        timeout_at: zx_instant_mono_t,
        confirmation: PowerOffConfirmation,
        _token: &LockToken<'_, TransitionLock>,
    ) -> TransitionResult {
        debug_assert_ne!(self.cpu_num(), BOOT_CPU_ID);
        let result = self.transition_from_to(State::Active, State::Offline, timeout_at);
        if result.status.is_err() {
            return result;
        }

        // Wait until the platform reports that the CPU is actually powered off. If the platform
        // does not support querying CPU power state, return immediately (failing if confirmation
        // is required).
        let status = wait_for_cpu_power_off(self.cpu_num(), timeout_at);
        if status.is_ok()
            || (status == Err(Status::NOT_SUPPORTED)
                && confirmation == PowerOffConfirmation::BestEffort)
        {
            return result;
        }
        TransitionResult { status, starting_state: result.starting_state }
    }

    /// Requests a state transition from `expected_state` to `target_state` up to `timeout_at`
    /// (see `TransitionLock`).
    fn transition_from_to(
        &self,
        expected_state: State,
        target_state: State,
        timeout_at: zx_instant_mono_t,
    ) -> TransitionResult {
        // Clear any stale completion signals from previous transitions that may have timed out
        // or aborted early. If a stale signal were left pending, `wait_deadline` below would
        // return immediately before the target CPU begins processing the transition, racing with
        // concurrent wake events that modify `state.target` and causing premature cancellation.
        // SAFETY: `self.complete` is an initialized per-CPU `AutounsignalMpUnplugEvent`, the
        // calling thread is initialized, and no `ChainLockTransaction` is active.
        let _ = unsafe { self.complete.unsignal() };
        // Attempt to move from the expected state to the transitional state.
        let mut expected = StateMachine { current: expected_state, target: expected_state };
        let transitional = StateMachine { current: expected_state, target: target_state };
        if !self.compare_exchange_state(&mut expected, transitional) {
            core::hint::cold_path();
            // The move to the transitional state failed because the current state is already the
            // target state. Indicate that the transition did not occur but the target state is
            // achieved anyway by returning the original state.
            if expected.current == target_state {
                return TransitionResult { status: Ok(()), starting_state: target_state };
            }

            // The move to the transitional state failed because the current state is neither the
            // expected state nor the target state.
            dprintf!(
                INFO,
                "Failed power state transition, expected(current={}, target={}) \
                 transitional(current={}, target={})\n",
                expected.current.as_str(),
                expected.target.as_str(),
                transitional.current.as_str(),
                transitional.target.as_str(),
            );
            return TransitionResult {
                status: Err(Status::BAD_STATE),
                starting_state: expected.current,
            };
        }
        debug_assert!(expected.current == expected_state);

        {
            // Reschedule the CPU for the idle/power thread to expedite handling the transition.
            // Disable interrupts to ensure this thread doesn't migrate to another CPU after
            // sampling the current CPU.
            let _interrupt_disable = InterruptDisableGuard::new();
            let this_cpu = self.cpu_num();
            if this_cpu == curr_cpu_num() {
                reschedule();
            } else {
                mp::reschedule(cpu_num_to_mask(this_cpu), 0);
            }
        }

        // Wait for the transition to the target state to complete or timeout, looping to deal with
        // spurious wakeups.
        loop {
            // SAFETY: `self.complete` is an initialized per-CPU `AutounsignalMpUnplugEvent` in
            // `'static` per-CPU storage, and `transition_from_to` runs in normal thread context
            // where blocking is permitted with no spinlocks or `ChainLockTransaction` held.
            let status = unsafe { self.complete.wait_deadline(timeout_at, Interruptible::No) };
            if status.is_err() {
                core::hint::cold_path();
                return TransitionResult { status, starting_state: expected_state };
            }
            // `Acquire` synchronizes with the target CPU's state update before `complete.signal()`.
            let state = self.state.load(Ordering::Acquire);

            // If the current or target state changed while waiting, this transition may have been
            // reversed before the current thread could observe the successful transition. This can
            // happen if an interrupt transitions the current CPU from `Suspend` back to `Active`
            // after a successful transition to `Suspend`, but before this thread starts waiting
            // for completion.
            if state.target != target_state {
                core::hint::cold_path();
                return TransitionResult {
                    status: Err(Status::CANCELED),
                    starting_state: state.current,
                };
            }
            if state.current == target_state {
                break;
            }
        }

        // The transition from the expected state to the target state was successful.
        TransitionResult { status: Ok(()), starting_state: expected_state }
    }

    /// Attempts to transition all active CPUs from `Active` to `Suspend` until the specified
    /// resume time. CPUs that are `Offline` before suspend will remain `Offline` after resume
    /// until explicitly transitioned to `Active`.
    ///
    /// Returns the boot-time at which the suspend operation is formally considered to have
    /// started. This value can be used in conjunction with timestamps recorded in wake events to
    /// decide whether a wake vector became signaled before suspend had formally started, or not.
    ///
    /// # Safety
    ///
    /// - Must be called after `LK_INIT_LEVEL_PLATFORM` has completed (`RESUME_TIMER` and
    ///   `resume_timer_wake_vector_` initialized).
    /// - Must be called from a normal thread context where blocking and CPU affinity changes are
    ///   permitted, with no spinlocks, `TransitionLock`, or active `ChainLockTransaction` held.
    #[inline]
    pub unsafe fn transition_all_active_to_suspend(
        resume_at: zx_instant_boot_t,
    ) -> zx_instant_boot_t {
        // SAFETY: Caller upholds the post-`LK_INIT_LEVEL_PLATFORM`, normal-thread, and lock-free
        // preconditions.
        unsafe { Self::suspend_all_active(resume_at, SuspendMode::Cpu) }
    }

    /// Attempts to suspend the entire system using `platform_suspend_system` until the specified
    /// resume time.
    ///
    /// Similar to [`Self::transition_all_active_to_suspend`], except that rather than
    /// transitioning the active non-boot CPUs to `Suspend`, transitions them to `Offline`
    /// (powering them off). Once the platform has confirmed that every one of them has powered
    /// off, the boot CPU is transitioned to `Suspend` and its idle/power thread suspends the
    /// system with `platform_suspend_system`. If power off cannot be confirmed for every CPU, the
    /// suspend operation is aborted. Upon resume, the non-boot CPUs which were taken offline are
    /// brought back online. (If bringing a CPU back online fails, a `kernel_oops!` is logged and
    /// that CPU remains `Offline` until explicitly transitioned to `Active`. Its timers are
    /// migrated to the boot CPU, but its queued DPCs and pinned threads remain with it until it is
    /// brought back online.) CPUs that are `Offline` before suspend will remain `Offline` after
    /// resume until explicitly transitioned to `Active`.
    ///
    /// If the platform does not support system suspend, or cannot report CPU power state (see
    /// `platform_get_cpu_state`), this behaves like [`Self::transition_all_active_to_suspend`].
    ///
    /// Returns the boot-time at which the suspend operation is formally considered to have
    /// started. This value can be used in conjunction with timestamps recorded in wake events to
    /// decide whether a wake vector became signaled before suspend had formally started, or not.
    ///
    /// # Safety
    ///
    /// - Must be called after `LK_INIT_LEVEL_PLATFORM` has completed (`RESUME_TIMER` and
    ///   `resume_timer_wake_vector_` initialized).
    /// - Must be called from a normal thread context where blocking and CPU affinity changes are
    ///   permitted, with no spinlocks, `TransitionLock`, or active `ChainLockTransaction` held.
    /// - No concurrent CPU hotplug or unplug operations may occur for the duration of the call.
    pub unsafe fn suspend_system(resume_at: zx_instant_boot_t) -> zx_instant_boot_t {
        // System suspend requires that the platform both support it and be able to confirm that
        // the non-boot CPUs have powered off (see `suspend_all_active`).
        let can_confirm_power_off =
            platform_get_cpu_state(BOOT_CPU_ID) != Err(Status::NOT_SUPPORTED);
        let supports_system = platform_supports_suspend_system();
        // SAFETY: Caller upholds the post-`LK_INIT_LEVEL_PLATFORM`, normal-thread, lock-free,
        // and serialized hotplug/unplug preconditions.
        unsafe {
            Self::suspend_all_active(
                resume_at,
                if supports_system && can_confirm_power_off {
                    SuspendMode::System
                } else {
                    SuspendMode::Cpu
                },
            )
        }
    }

    /// Implements [`Self::transition_all_active_to_suspend`] and [`Self::suspend_system`].
    ///
    /// # Safety
    ///
    /// - Must be called after `LK_INIT_LEVEL_PLATFORM` has completed (`RESUME_TIMER` and
    ///   `resume_timer_wake_vector_` initialized).
    /// - Must be called from a normal thread context where blocking and CPU affinity changes are
    ///   permitted, with no spinlocks, `TransitionLock`, or active `ChainLockTransaction` held.
    /// - When `mode == SuspendMode::System`, no concurrent CPU hotplug or unplug operations may
    ///   occur for the duration of the call.
    unsafe fn suspend_all_active(
        resume_at: zx_instant_boot_t,
        mode: SuspendMode,
    ) -> zx_instant_boot_t {
        // Ensure CPUs in `cpus_active_before_suspend` are not modified during the execution of
        // `suspend_all_active`.
        ksync::lock!(let guard = TransitionLock::get().lock());

        // Ensure that the monotonic clock is running and is returned to the running state no
        // matter how we leave this function. This is a CYA check to ensure we don't inadvertently
        // leave the clock paused.
        debug_assert!(!timer_is_monotonic_paused());
        let _ensure_mono_not_paused = zr::defer(|| {
            debug_assert!(!timer_is_monotonic_paused());
        });

        let suspend_start_time = current_boot_time().0;
        if resume_at < suspend_start_time {
            core::hint::cold_path();
            // SAFETY: `suspend_all_active` runs long after `LK_INIT_LEVEL_EARLIEST` (`init_hook`),
            // so `resume_timer_wake_vector_` is initialized.
            unsafe { resume_timer_wake_event() }.strobe(current_boot_time().0);
            return suspend_start_time;
        }

        // Conditionally set the global suspended flag so that other subsystems can act
        // appropriately during suspend. If there are pending wake events, abort the suspend.
        {
            // `Release` on success publishes all prior state writes before setting
            // `SYSTEM_SUSPEND_STATE_SUSPENDED_BIT`, pairing with `Acquire` loads in
            // `system_suspended()` and `pending_wake_events()`. `Relaxed` suffices on failure
            // because suspend is aborted immediately.
            if let Err(expected) = SYSTEM_SUSPEND_STATE.compare_exchange(
                SYSTEM_SUSPEND_STATE_ACTIVE,
                SYSTEM_SUSPEND_STATE_SUSPENDED_BIT,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                core::hint::cold_path();
                debug_assert!(
                    (expected & SYSTEM_SUSPEND_STATE_SUSPENDED_BIT)
                        != SYSTEM_SUSPEND_STATE_SUSPENDED_BIT
                );
                let pending_wake_events = expected & SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK;
                dprintf!(
                    INFO,
                    "Aborting suspend due to {} pending wake events\n",
                    pending_wake_events,
                );

                // TODO(https://fxbug.dev/348668110): Revisit this logging. Still needed?
                kprintln!("begin dump of pending wake events");
                WakeEvent::dump(&mut debug::dprintf::KernelConsoleWriter, suspend_start_time);
                kprintln!("end dump of pending wake events");

                return suspend_start_time;
            }
        }

        let _restore_suspend_flag = zr::defer(|| {
            // `Release` ensures all suspend teardown stores complete before clearing
            // `SYSTEM_SUSPEND_STATE_SUSPENDED_BIT`, pairing with `Acquire` loads in
            // `system_suspended()` and `pending_wake_events()`.
            SYSTEM_SUSPEND_STATE.fetch_and(!SYSTEM_SUSPEND_STATE_SUSPENDED_BIT, Ordering::Release);
        });

        // Move to the boot CPU which will be suspended last.
        // SAFETY: Called from normal thread context with no spinlocks held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Called on unwind from normal thread context with no spinlocks held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });
        debug_assert!(curr_cpu_num() == BOOT_CPU_ID);

        // TODO(eieio): Consider temporarily pinning the `debuglog` notifier thread to the boot CPU
        // to give it a chance to notify readers during the non-boot CPU suspend sequence. This is
        // not needed for correctness, since the `debuglog` critical sections are protected by a
        // spinlock, which prevents the local CPU from being suspended while held. However, given
        // that readers might also get suspended on non-boot CPUs, this may not have much effect on
        // timeliness of log output.

        // Keep track of which non-boot CPUs to resume.
        let cpus_active_before_suspend = peek_active_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        dprintf!(INFO, "Active non-boot CPUs before suspend: {:#x}\n", cpus_active_before_suspend);

        // Suspend all of the active CPUs besides the boot CPU. For a system suspend, take them
        // offline instead, since `platform_suspend_system` requires that they be powered off.
        dprintf!(
            INFO,
            "{} non-boot CPUs...\n",
            if mode == SuspendMode::System { "Taking offline" } else { "Suspending" },
        );

        let suspend_timeout_at = Deadline::after_mono(DurationMono(zx_min(1)), TimerSlack::none());
        let mut cpus_to_suspend = cpus_active_before_suspend;
        let mut cpus_to_resume: cpu_mask_t = 0;
        while cpus_to_suspend != 0 {
            let cpu_id = highest_cpu_set(cpus_to_suspend);
            // SAFETY: `cpus_to_suspend` is a subset of `peek_active_mask()`, whose set bits only
            // ever name CPUs `< PerCpu::processor_count()`, and `cpus_to_suspend != 0`.
            let idle_power_thread = &unsafe { PerCpu::get(cpu_id) }.idle_power_thread;
            // For a system suspend, it is critical that each CPU has finished powering off (i.e.
            // completed PSCI CPU_OFF) before the boot CPU calls `platform_suspend_system`, so
            // require confirmation from the platform. If it cannot be confirmed, abort.
            let active_to_suspend_result = if mode == SuspendMode::System {
                idle_power_thread.transition_active_to_offline_locked(
                    suspend_timeout_at.when().0,
                    PowerOffConfirmation::Required,
                    guard.token(),
                )
            } else {
                idle_power_thread.transition_from_to(
                    State::Active,
                    State::Suspend,
                    suspend_timeout_at.when().0,
                )
            };
            if let Err(err) = active_to_suspend_result.status {
                core::hint::cold_path();
                // Taking a CPU offline may fail after it has gone offline (e.g. if it was not
                // reported to be powered off before the timeout), or the request may still be
                // pending (if the transition itself timed out), in which case the CPU may yet go
                // offline. Either way, try to bring it back online (see below).
                if mode == SuspendMode::System {
                    // `Acquire` synchronizes with state updates on `cpu_id`.
                    let state = idle_power_thread.state.load(Ordering::Acquire);
                    if state.current == State::Offline || state.target == State::Offline {
                        cpus_to_resume |= cpu_num_to_mask(cpu_id);
                    }
                }
                kernel_oops!(
                    "Failed to transition CPU {} to {:s}: {}\n",
                    cpu_id,
                    if mode == SuspendMode::System { "offline" } else { "suspend" },
                    err.into_raw(),
                );
                break;
            }

            cpus_to_suspend &= !cpu_num_to_mask(cpu_id);
            cpus_to_resume |= cpu_num_to_mask(cpu_id);
        }

        if cpus_to_suspend != 0 {
            dprintf!(INFO, "Aborting suspend and resuming non-boot CPUs...\n");
        } else {
            dprintf!(
                INFO,
                "Done {} non-boot CPUs.\n",
                if mode == SuspendMode::System { "taking offline" } else { "suspending" },
            );

            // Set the resume timer before suspending the boot CPU.
            if resume_at != ZX_TIME_INFINITE {
                dprintf!(INFO, "Setting boot CPU to resume at time {}\n", resume_at);
                RESUME_TIMER.set_resume_deadline(InstantBoot(resume_at));
            } else {
                // TODO(eieio): Check that at least one wake source is configured if no resume time
                // is set. Maybe this check needs require at least one wake source if the resume
                // time is too far in the future?
                dprintf!(
                    INFO,
                    "No kernel resume timeout set. System will only resume by wake vector.\n",
                );
            }

            // For a system suspend, have the boot CPU's idle/power thread suspend the system
            // rather than just the boot CPU. See `run`.
            if mode == SuspendMode::System {
                // `Relaxed` suffices because `transition_from_to` below performs an `AcqRel`
                // RMW on `boot_idle_power_thread.state`.
                SYSTEM_SUSPEND_REQUESTED.store(true, Ordering::Relaxed);
            }

            dprintf!(
                INFO,
                "Attempting to suspend {}...\n",
                if mode == SuspendMode::System { "system from boot CPU" } else { "boot CPU" },
            );

            // SAFETY: `BOOT_CPU_ID` is `0`, and `PerCpu::processor_count()` is initialized to a
            // non-zero count during early boot before `suspend_system` can be called.
            let boot_idle_power_thread = &unsafe { PerCpu::get(BOOT_CPU_ID) }.idle_power_thread;
            let active_to_suspend_result = boot_idle_power_thread.transition_from_to(
                State::Active,
                State::Suspend,
                suspend_timeout_at.when().0,
            );

            // Cancel the resume timer in case it wasn't the reason for the wakeup.
            RESUME_TIMER.cancel_timer();

            // `Relaxed` suffices because `boot_idle_power_thread.transition_from_to` has already
            // returned and `TransitionLock` serializes subsequent suspend cycles.
            SYSTEM_SUSPEND_REQUESTED.store(false, Ordering::Relaxed);

            match active_to_suspend_result.status {
                Err(Status::TIMED_OUT) => {
                    dprintf!(INFO, "Timed out waiting for boot CPU to suspend!\n");
                }

                Err(Status::CANCELED) => {
                    dprintf!(INFO, "Boot CPU resumed.\n");
                    debug_assert!(
                        active_to_suspend_result.starting_state == State::Wakeup,
                        "starting_state={}",
                        active_to_suspend_result.starting_state.as_str(),
                    );
                }

                Err(Status::BAD_STATE) => {
                    dprintf!(INFO, "Boot CPU suspend aborted.\n");
                    debug_assert!(
                        active_to_suspend_result.starting_state == State::Wakeup,
                        "starting_state={}",
                        active_to_suspend_result.starting_state.as_str(),
                    );
                }

                other => {
                    debug_assert!(
                        false,
                        "Unexpected result from boot CPU suspend: status={} starting_state={}",
                        Status::result_into_raw(other),
                        active_to_suspend_result.starting_state.as_str(),
                    );
                }
            }

            dprintf!(INFO, "Wake events triggered after/during suspend:\n");
            WakeEvent::dump(&mut debug::dprintf::KernelConsoleWriter, suspend_start_time);

            // If the boot CPU is in the `Wakeup` state, set it to `Active`.
            let mut expected = StateMachine::WAKEUP;
            let succeeded =
                boot_idle_power_thread.compare_exchange_state(&mut expected, StateMachine::ACTIVE);
            debug_assert!(
                succeeded || expected == StateMachine::ACTIVE,
                "current={} target={}",
                expected.current.as_str(),
                expected.target.as_str(),
            );
        }

        // Resume all non-boot CPUs that were successfully suspended (or taken offline).
        dprintf!(INFO, "Resuming non-boot CPUs...\n");

        let resume_timeout_at = Deadline::after_mono(DurationMono(zx_min(1)), TimerSlack::none());
        if mode == SuspendMode::System {
            // The CPUs were taken offline via their idle/power threads rather than unplugged (see
            // `suspend_system`), so their `TimerQueue`s, `DpcRunner`s, and pinned threads stayed
            // with them. If a CPU cannot be brought back online, migrate its timers to this CPU
            // (the boot CPU) so that they are not stranded. This is only safe once the CPU is
            // `Offline` (no longer processing tasks) and will not be restarted. Its queued DPCs
            // and pinned threads cannot be migrated (its `DpcRunner` was not shut down), so they
            // remain with it until it is brought back online.
            let migrate_timers_off = |cpu_id: cpu_num_t| {
                // SAFETY: Every caller below passes a `cpu_id` extracted from `cpus_to_resume` or
                // `cpus_started`, both of which are subsets of `peek_active_mask()`, so
                // `(cpu_id as usize) < PerCpu::processor_count()`.
                let cpu = unsafe { PerCpu::get(cpu_id) };
                // `Acquire` synchronizes with `flush_and_halt`'s `Release` store of `Offline`.
                if cpu.idle_power_thread.state.load(Ordering::Acquire).current != State::Offline {
                    // The CPU may still be processing tasks, so its timers cannot be migrated.
                    kernel_oops!("CPU {} is not offline.  Not migrating its timers.\n", cpu_id);
                    return;
                }
                // SAFETY: The current thread is pinned to the boot CPU (`get_current()`), and we
                // verified above that `cpu_id` is in the `Offline` state and will not be restarted.
                unsafe { PerCpu::get_current().timer_queue.transition_off_cpu(&cpu.timer_queue) };
            };

            // Start all of the CPUs first, and then wait for all of them, so that their bring-up
            // overlaps.
            let mut cpus_started: cpu_mask_t = 0;
            while cpus_to_resume != 0 {
                let cpu_id = highest_cpu_set(cpus_to_resume);
                cpus_to_resume &= !cpu_num_to_mask(cpu_id);

                // If taking the CPU offline was aborted before the platform confirmed that it had
                // powered off, it may still be powering off. Starting it before then would fail
                // (e.g. PSCI CPU_ON would return ALREADY_ON), so wait. Normally, the CPU has long
                // since powered off.
                //
                // If the CPU has not yet gone offline at all (its transition to `Offline` timed
                // out and is still pending), it is still processing tasks, so `migrate_timers_off`
                // will leave its timers alone.
                if let Err(err) = wait_for_cpu_power_off(cpu_id, resume_timeout_at.when().0) {
                    kernel_oops!(
                        "CPU {} did not power off: {}.  Leaving it offline.\n",
                        cpu_id,
                        err.into_raw(),
                    );
                    migrate_timers_off(cpu_id);
                    continue;
                }

                // SAFETY: Running in a normal thread context with interrupts enabled where memory
                // allocation and sleeping are permitted, serialized against concurrent
                // hotplug/unplug operations by the caller's contract and `TransitionLock`, and
                // `cpu_id` is a valid non-boot CPU ID confirmed powered off by
                // `wait_for_cpu_power_off`.
                if let Err(err) = unsafe { platform_mp_cpu_hotplug(cpu_id) } {
                    kernel_oops!(
                        "Failed to hotplug CPU {}: {}.  Leaving it offline.\n",
                        cpu_id,
                        err.into_raw(),
                    );
                    migrate_timers_off(cpu_id);
                    continue;
                }
                cpus_started |= cpu_num_to_mask(cpu_id);
            }

            // Wait for each started CPU to complete its transition to `Active`.
            while cpus_started != 0 {
                let cpu_id = highest_cpu_set(cpus_started);
                cpus_started &= !cpu_num_to_mask(cpu_id);

                // SAFETY: `cpus_started` is a subset of `cpus_to_resume` (and thus of
                // `peek_active_mask()`), whose set bits are `< PerCpu::processor_count()`, and
                // `cpus_started` was non-zero above.
                let idle_power_thread = &unsafe { PerCpu::get(cpu_id) }.idle_power_thread;
                // `Acquire` synchronizes with `cpu_id` completing its transition to `ACTIVE`.
                while idle_power_thread.state.load(Ordering::Acquire) != StateMachine::ACTIVE {
                    let now = current_mono_time().0;
                    if now >= resume_timeout_at.when().0 {
                        // If the platform reports that the CPU is (still) powered off, it never
                        // started, so it is safe to migrate its timers. Otherwise, it may yet come
                        // online.
                        let cpu_state = platform_get_cpu_state(cpu_id);
                        let powered_off = matches!(
                            cpu_state,
                            Ok(PowerCpuState::Off) | Ok(PowerCpuState::Stopped)
                        );
                        kernel_oops!(
                            "Timed out waiting for CPU {} to become active (powered off: {})\n",
                            cpu_id,
                            powered_off as i32,
                        );
                        if powered_off {
                            migrate_timers_off(cpu_id);
                        }
                        break;
                    }
                    let _ = sleep(InstantMono(
                        now.saturating_add(CPU_STATE_POLL_INTERVAL).min(resume_timeout_at.when().0),
                    ));
                }
            }
        } else {
            while cpus_to_resume != 0 {
                let cpu_id = highest_cpu_set(cpus_to_resume);
                // SAFETY: `cpus_to_resume` is a subset of `peek_active_mask()`, whose set bits are
                // `< PerCpu::processor_count()`, and `cpus_to_resume != 0`.
                let suspend_to_active_result = unsafe { PerCpu::get(cpu_id) }
                    .idle_power_thread
                    .transition_from_to(State::Suspend, State::Active, resume_timeout_at.when().0);
                if let Err(err) = suspend_to_active_result.status {
                    kernel_oops!(
                        "Failed to transition CPU {} to active: {}\n",
                        cpu_id,
                        err.into_raw(),
                    );
                    break;
                }

                cpus_to_resume &= !cpu_num_to_mask(cpu_id);
            }
            debug_assert!(cpus_to_resume == 0);
        }

        dprintf!(INFO, "Done resuming non-boot CPUs.\n");
        suspend_start_time
    }

    /// Transitions the boot CPU from `StateMachine::SUSPEND` (or `StateMachine::ACTIVE` /
    /// `StateMachine::ACTIVE_TO_SUSPEND`) to `Wakeup`.
    ///
    /// # Safety
    ///
    /// The calling CPU's current `Thread` pointer must be initialized, interrupts, preemption, and
    /// blocking (`blocking_disallowed()`) must all be disabled on the calling CPU, and
    /// `Self::system_suspended()` must be `true`.
    unsafe fn wake_boot_cpu() -> WakeResult {
        debug_assert!(Self::system_suspended());
        debug_assert!(ints_disabled());
        debug_assert!(!preempt_is_enabled());
        debug_assert!(blocking_disallowed());

        // Poke the boot CPU when preemption is re-enabled to ensure it responds to the potential
        // state change.
        // SAFETY: Caller guarantees the current `Thread` is initialized and interrupts,
        // preemption, and blocking are disabled on the current CPU, so
        // `current_preemption_state()` points to the current thread's live `PreemptionState` and
        // `preempt_set_pending` preconditions hold.
        unsafe { (*current_preemption_state()).preempt_set_pending(cpu_num_to_mask(BOOT_CPU_ID)) };

        // SAFETY: `BOOT_CPU_ID` is `0`, and `PerCpu::processor_count()` is initialized to a
        // non-zero count during early boot before `wake_boot_cpu` can be called.
        let boot_idle_power_thread = &unsafe { PerCpu::get(BOOT_CPU_ID) }.idle_power_thread;

        // Attempt to transition the boot CPU from `Suspend` to `Wakeup`. This method may be
        // running on the boot CPU and interrupting the context of the idle/power thread.
        let mut expected = StateMachine::SUSPEND;
        loop {
            let success = boot_idle_power_thread
                .compare_exchange_state(&mut expected, StateMachine::SUSPEND_TO_WAKEUP);
            if success {
                return WakeResult::Resumed;
            }

            // If the current state is `StateMachine::ACTIVE` or `StateMachine::ACTIVE_TO_SUSPEND`,
            // this wake up occurred before or during the `State::Active`-to-`State::Suspend`
            // transition of the boot CPU. Set the state to `StateMachine::WAKEUP` to prevent the
            // transition to `State::Suspend`, which would cause this wake up to be missed.
            if expected == StateMachine::ACTIVE || expected == StateMachine::ACTIVE_TO_SUSPEND {
                core::hint::cold_path();
                let aborted = boot_idle_power_thread
                    .compare_exchange_state(&mut expected, StateMachine::WAKEUP);
                if aborted {
                    return WakeResult::SuspendAborted;
                }
            }

            // Try again if the previous attempt collided with completing the Active-to-Suspend
            // transition.
            if expected != StateMachine::SUSPEND {
                break;
            }
        }

        // Racing with another wake trigger can cause the attempts above to gracefully fail.
        // However, colliding with other states is an error.
        debug_assert!(
            expected == StateMachine::SUSPEND_TO_WAKEUP || expected == StateMachine::WAKEUP,
            "current={} target={}",
            expected.current.as_str(),
            expected.target.as_str(),
        );
        WakeResult::Resumed
    }

    /// Triggers a system wake event, waking the boot CPU if the system is suspended and this is
    /// the first pending wake event.
    ///
    /// # Safety
    ///
    /// The calling CPU's current `Thread` pointer must be initialized, and interrupts, preemption,
    /// and blocking (`blocking_disallowed()`) must all be disabled on the calling CPU.
    #[inline]
    pub unsafe fn trigger_system_wake_event() -> WakeResult {
        debug_assert!(ints_disabled());
        debug_assert!(!preempt_is_enabled());
        debug_assert!(blocking_disallowed());

        // `Relaxed` suffices for incrementing the single-word interlock counter; waking the boot
        // CPU synchronizes via `AcqRel` on `IdlePowerThread::state`.
        let previous = SYSTEM_SUSPEND_STATE.fetch_add(1, Ordering::Relaxed);
        let previous_pending_wake_events = previous & SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK;
        let suspended = (previous & SYSTEM_SUSPEND_STATE_SUSPENDED_BIT) != 0;

        debug_assert!(
            previous_pending_wake_events != SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK,
            "Pending wake event count overflow!",
        );

        // Wake the boot CPU if the system is suspended and this is the first pending wake event.
        if suspended && previous_pending_wake_events == 0 {
            core::hint::cold_path();
            // SAFETY: Caller guarantees the current `Thread` is initialized and interrupts,
            // preemption, and blocking are disabled on the calling CPU, and `suspended` confirms
            // the system is currently suspended.
            return unsafe { Self::wake_boot_cpu() };
        }
        if suspended { WakeResult::Resumed } else { WakeResult::Active }
    }

    /// Acknowledges a pending system wake event.
    #[inline]
    pub fn acknowledge_system_wake_event() {
        // `Relaxed` suffices because the caller already synchronized with the wake event source.
        let previous = SYSTEM_SUSPEND_STATE.fetch_sub(1, Ordering::Relaxed);
        let previous_pending_wake_events = previous & SYSTEM_SUSPEND_STATE_WAKE_EVENT_PENDING_MASK;
        debug_assert!(previous_pending_wake_events != 0, "Pending wake event count underflow!");
    }

    /// Callback invoked at hard-IRQ time on the boot CPU when the resume timer fires.
    fn resume_from_timer_irq(now: InstantBoot, resume_at: InstantBoot) {
        // Verify this handler is running in the correct context.
        debug_assert!(curr_cpu_num() == BOOT_CPU_ID);

        // SAFETY: `RESUME_TIMER` is only initialized at `LK_INIT_LEVEL_PLATFORM` and armed by
        // `suspend_all_active`, both after `LK_INIT_LEVEL_EARLIEST` (`init_hook`) has initialized
        // `resume_timer_wake_vector_`.
        let wake_result = unsafe { resume_timer_wake_event() }.strobe(now.0);
        let message_prefix = if wake_result == WakeResult::SuspendAborted {
            "Wakeup before suspend completed. Aborting suspend"
        } else {
            "Resuming boot CPU"
        };

        let resume_delta = (now - resume_at).0;
        dprintf!(
            INFO,
            "{} at time {}, {}ns after target resume time.\n",
            message_prefix,
            now.0,
            resume_delta,
        );
    }
}

/// Populates diagnostic metadata for `IdlePowerThread::ResumeTimerWakeVector`.
fn resume_timer_wake_vector_get_diagnostics(diagnostics_out: &mut Diagnostics) {
    diagnostics_out.enabled = true;
    diagnostics_out.koid = ZX_KOID_KERNEL;
    diagnostics_out.set_extra("suspend timeout");
}

/// Initializes the global resume timer wake vector at `LK_INIT_LEVEL_EARLIEST` and the
/// `SuspendWakeupTimer` instance at `LK_INIT_LEVEL_PLATFORM`.
fn init_hook(level: init::LkInitLevel) {
    if level == init::LK_INIT_LEVEL_EARLIEST {
        // Register the resume timer wake event after global ctors have initialized the global
        // list.
        // SAFETY: Called once during single-threaded boot at `LK_INIT_LEVEL_EARLIEST`.
        unsafe { cpp_resume_timer_wake_vector_init() };
    } else if level == init::LK_INIT_LEVEL_PLATFORM {
        // Create the `SuspendWakeupTimer` after platform drivers have been initialized. This gives
        // systems which use special timer HW to exit suspend a chance to detect and initialize
        // their hardware before we start to use it.
        ksync::lock!(let _guard = TransitionLock::get().lock());
        // SAFETY: Called once at `LK_INIT_LEVEL_PLATFORM` while holding `TransitionLock` before
        // any suspend operation can start; `suspend_all_active` subsequently transitions the boot
        // CPU's `IdlePowerThread::state` with `AcqRel` before the boot CPU's
        // `IdlePowerThread::run` observes `State::Suspend` (`Acquire`) and accesses
        // `RESUME_TIMER`.
        let Ok(()) = unsafe {
            Pin::static_ref(&RESUME_TIMER).init_pin(SuspendWakeupTimer::init(
                SuspendWakeupTimerCallback::new(IdlePowerThread::resume_from_timer_irq),
            ))
        };
    }
}

init::lk_init_hook!(idle_power_thread_earliest, init_hook, init::LK_INIT_LEVEL_EARLIEST);
init::lk_init_hook!(idle_power_thread_platform, init_hook, init::LK_INIT_LEVEL_PLATFORM);

/// FFI trampoline for [`IdlePowerThread::transition_active_to_offline`].
///
/// # Safety
///
/// `ipt` must reference a valid initialized per-CPU [`IdlePowerThread`] for a non-boot CPU, and
/// the caller must execute in a normal thread context where blocking is permitted with no
/// spinlocks, `TransitionLock`, or active `ChainLockTransaction` held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_idle_power_thread_transition_active_to_offline(
    ipt: &IdlePowerThread,
    timeout_at: zx_instant_mono_t,
) -> IdlePowerThreadTransitionResult {
    // SAFETY: Caller upholds all preconditions of `IdlePowerThread::transition_active_to_offline`.
    unsafe { ipt.transition_active_to_offline(timeout_at) }.into()
}

/// FFI trampoline for [`IdlePowerThread::transition_offline_to_active`].
///
/// # Safety
///
/// `ipt` must reference a valid initialized per-CPU [`IdlePowerThread`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_idle_power_thread_transition_offline_to_active(
    ipt: &IdlePowerThread,
    timeout_at: zx_instant_mono_t,
) -> IdlePowerThreadTransitionResult {
    ipt.transition_offline_to_active(timeout_at).into()
}

/// FFI trampoline for [`IdlePowerThread::run`].
///
/// # Safety
///
/// Must be invoked exclusively as the entry point of a per-CPU kernel-only idle/power thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_idle_power_thread_run(arg: *mut c_void) -> c_int {
    // SAFETY: Caller guarantees this is the per-CPU idle/power thread entry point.
    unsafe { IdlePowerThread::run(arg) }
}

/// FFI trampoline for [`IdlePowerThread::flush_and_halt`].
///
/// # Safety
///
/// Must be called on the unplugging CPU owning `ipt` with interrupts disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_idle_power_thread_flush_and_halt(ipt: &IdlePowerThread) -> ! {
    // SAFETY: Caller guarantees execution on `ipt`'s unplugging CPU with interrupts disabled.
    unsafe { ipt.flush_and_halt() }
}

/// FFI trampoline populating diagnostics for `IdlePowerThread::ResumeTimerWakeVector`.
///
/// # Safety
///
/// `diagnostics_out` must reference a valid writable [`Diagnostics`] struct.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_resume_timer_wake_vector_get_diagnostics(
    diagnostics_out: &mut Diagnostics,
) {
    resume_timer_wake_vector_get_diagnostics(diagnostics_out);
}

/// Kernel console command handler for `suspend`.
///
/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`crate::console::CmdArgs`] elements,
/// where `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_suspend(
    argc: c_int,
    argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> c_int {
    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} enter <resume delay> <m|s|ms|us|ns> [cpu|system]", argv[0].as_str());
        kprintln!("{:s} wake-events", argv[0].as_str());
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        return usage();
    }

    if argv[1].as_str() == "enter" {
        if !(4..=5).contains(&argc) {
            return usage();
        }

        let mut suspend_system = false;
        if argc == 5 {
            if argv[4].as_str() == "system" {
                suspend_system = true;
            } else if argv[4].as_str() != "cpu" {
                kprintln!("Invalid mode: {:s}", argv[4].as_str());
                return usage();
            }
        }

        let mut delay: zx_duration_boot_t = argv[2].arg_int;
        let units = argv[3].as_str();
        if units == "m" {
            delay = zx_min(delay);
        } else if units == "s" {
            delay = zx_sec(delay);
        } else if units == "ms" {
            delay = zx_msec(delay);
        } else if units == "us" {
            delay = zx_usec(delay);
        } else if units == "ns" {
            delay = zx_nsec(delay);
        } else {
            kprintln!("Invalid units: {:s}", units);
            return usage();
        }

        let resume_at = current_boot_time().0.saturating_add(delay);
        if suspend_system {
            // SAFETY: `CMD_SUSPEND` is registered with `CMD_AVAIL_NORMAL`, so `cmd_suspend` runs
            // in normal thread context after `LK_INIT_LEVEL_PLATFORM` with no locks held.
            unsafe { IdlePowerThread::suspend_system(resume_at) };
        } else {
            // SAFETY: `CMD_SUSPEND` is registered with `CMD_AVAIL_NORMAL`, so `cmd_suspend` runs
            // in normal thread context after `LK_INIT_LEVEL_PLATFORM` with no locks held.
            unsafe { IdlePowerThread::transition_all_active_to_suspend(resume_at) };
        }
        return ZX_OK;
    }

    if argv[1].as_str() == "wake-events" {
        if argc != 2 {
            return usage();
        }

        kprintln!("Wake events:");
        WakeEvent::dump(&mut debug::dprintf::KernelConsoleWriter, 0);
        return ZX_OK;
    }

    kprintln!("unknown command");
    usage()
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_SUSPEND,
    c"suspend".as_ptr(),
    c"processor suspend commands".as_ptr(),
    cmd_suspend,
    crate::console::CMD_AVAIL_NORMAL
);

/// Unit tests for [`IdlePowerThread`] system suspend and resume behavior.
#[cfg(ktest)]
#[unittest::suite(name = "idle_power_thread_rust")]
mod tests {
    #[cfg(not(target_arch = "riscv64"))]
    use crate::kernel::thread::sleep_relative;
    #[cfg(not(target_arch = "riscv64"))]
    use unittest::expect_eq;
    #[cfg(target_arch = "aarch64")]
    use unittest::expect_ne;

    /// Verifies that `SuspendMode::System` restores all active CPUs after system suspend/resume.
    #[test]
    fn suspend_all_active_system_mode_restores_cpus() {
        // Exercises the `System` mode of `suspend_all_active` (used by `suspend_system`)
        // regardless of whether the platform supports `platform_suspend_system`. The non-boot CPUs
        // are taken offline (waiting for the platform to confirm that they have powered off), the
        // boot CPU suspends the system (or, if system suspend is not supported, falls back to
        // suspending just itself) until the resume timer fires, and then the non-boot CPUs are
        // brought back online. Verifies that every CPU which was online and active beforehand is
        // online and active again afterwards.
        #[cfg(target_arch = "riscv64")]
        {
            // TODO(https://fxbug.dev/42086046): Re-enable when RISC-V supports CPU hotplug.
            kprintln!("skipping test, hotplug only supported on x64 and arm64");
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            // System mode requires that the platform be able to confirm that CPUs have powered
            // off.
            if platform_get_cpu_state(BOOT_CPU_ID) == Err(Status::NOT_SUPPORTED) {
                kprintln!("platform_get_cpu_state is not supported, skipping test");
                return true;
            }

            let online_before = mp::get_online_mask();
            let active_before = peek_active_mask();

            // SAFETY: Kernel unit tests run in normal thread context after
            // `LK_INIT_LEVEL_PLATFORM` with no locks held.
            unsafe {
                IdlePowerThread::suspend_all_active(
                    current_boot_time().0.saturating_add(zx_msec(100)),
                    SuspendMode::System,
                );
            }

            // `suspend_all_active` waits for each CPU's idle/power thread to become active again,
            // but the online and scheduler active masks may be updated slightly later, so allow
            // some time for them to catch up.
            let deadline = Deadline::after_mono(DurationMono(zx_sec(10)), TimerSlack::none());
            while (mp::get_online_mask() != online_before || peek_active_mask() != active_before)
                && current_mono_time().0 < deadline.when().0
            {
                let _ = sleep_relative(DurationMono(zx_usec(200)));
            }
            expect_eq!(online_before, mp::get_online_mask());
            expect_eq!(active_before, peek_active_mask());

            #[cfg(target_arch = "aarch64")]
            {
                // Kernel code runs with PSTATE.PAN set (when FEAT_PAN is implemented), but PAN is
                // not preserved across a CPU power-down, so make sure it was restored on every CPU
                // that may have powered down. PSTATE.PAN is not saved and restored per-thread, so
                // if a CPU had resumed with PAN clear, the threads it switches to would observe it
                // clear as well. Run on each CPU and check.
                // SAFETY: `arm64_feature_init` on the boot CPU completed during `arch_early_init`.
                if unsafe { crate::arch_rs::arm64::arm64_mmu_features_pan() } {
                    // SAFETY: Called from normal thread context with no spinlocks held.
                    let previous_affinity = unsafe { ThreadPtr::current().get_cpu_affinity() };
                    let _restore = zr::defer(move || {
                        // SAFETY: Restores the original CPU affinity on unwind.
                        let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
                    });
                    for cpu in 0..(PerCpu::processor_count() as cpu_num_t) {
                        if (active_before & cpu_num_to_mask(cpu)) == 0 {
                            continue;
                        }
                        // SAFETY: Pins the test thread to `cpu` with no spinlocks held.
                        let _ =
                            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(cpu)) };
                        let pan: u64;
                        // SAFETY: Reading `S3_0_C4_C2_3` (`PAN`) is valid at EL1 when
                        // `arm64_mmu_features.pan` is true.
                        unsafe {
                            core::arch::asm!("mrs {}, s3_0_c4_c2_3", out(reg) pan);
                        }
                        if pan == 0 {
                            kprintln!("PSTATE.PAN is clear on CPU {}", cpu);
                        }
                        expect_ne!(0u64, pan);
                    }
                }
            }
        }
    }
}
