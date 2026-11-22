// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-CPU state for the lockup detector.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32};

use crate::arch_rs::{CpuAlignMarker, MAX_CACHE_LINE};
use crate::kernel::event_limiter::EventLimiter;
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::types::{INVALID_CPU, cpu_num_t};
use crate::platform_rs::timer::zx_sec;

/// Per-CPU lockup detector state used for detecting the "heartbeat" condition.
pub(super) struct HeartbeatState {
    /// A flag used to indicate that this CPU is participating in the heartbeat
    /// mechanism.  IOW - it is periodically recording that it is still running in
    /// the `last_heartbeat` field, and after doing so it is checking on its peer
    /// CPUs.
    ///
    /// TODO(johngro): Should we merge this field with `last_heartbeat`?  Seems
    /// like we should be able to use a sentinel value, such as 0, or
    /// `ZX_TIME_INFINITE` to indicate that the mechanism is disabled.
    pub(super) active: AtomicBool,

    /// The last time at which this CPU checked in.
    pub(super) last_heartbeat: AtomicI64,

    /// The largest gap between `last_heartbeat` and now ever observed by a checker.
    /// Note that writes to the field are "protected" by the exclusive role of
    /// "checker".
    pub(super) max_gap: AtomicI64,

    /// limiter for the rate at which heartbeat failures are reported.  "Protected"
    /// by the exclusive role of "checker".
    pub(super) alert_limiter: EventLimiter<{ zx_sec(1) }>,
}

impl HeartbeatState {
    /// Creates the initial, inactive heartbeat state.
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            last_heartbeat: AtomicI64::new(0),
            max_gap: AtomicI64::new(0),
            alert_limiter: EventLimiter::new(),
        }
    }
}

/// Per-CPU lockup detector state used for detecting the "critical section"
/// condition.
pub(super) struct CriticalSectionState {
    /// Critical sections may be nested, so `lockup_timed_begin` and
    /// `lockup_timed_end` (called as code enters and exits critical sections) must
    /// keep track of the depth.  This variable is only ever accessed by the code
    /// entering and exiting the CS, and always on the same CPU, so there is no
    /// need for atomic read-modify-write operations; relaxed loads and stores
    /// suffice.  However, because an interrupt may fire as a thread enters a
    /// critical section and the interrupt handler itself may enter a critical
    /// section, compiler fences must be used when accessing to ensure that
    /// compiler reordering does not lead to problem.
    ///
    /// Accessed only by this CPU.
    pub(super) depth: AtomicU32,

    /// The name of the active critical section, if any.  May be null.
    ///
    /// Accessed by both this CPU and observers.
    pub(super) name: AtomicPtr<c_char>,

    /// The worst case CS time ever observed by the critical section thread as it
    /// exits the critical section. This variable is written only by the CPU
    /// exiting the critical section, but is read by other CPUs during the
    /// heartbeat sanity checks. While the thread exiting the critical section
    /// reports the worst cast time via this variable, only the threads
    /// performing heartbeat sanity checks will ever report issues (via an OOPS)
    /// as a result of a new worst case value.
    ///
    /// Accessed by both this CPU and observers.
    pub(super) worst_case_ticks: AtomicI64,

    /// The time (tick count) at which the CPU entered the critical section.
    ///
    /// This field is used to establish Release-Acquire ordering of changes made
    /// by critical section threads and observed by observers.
    ///
    /// Accessed by both this CPU and observers.
    pub(super) begin_ticks: AtomicI64,

    /// State variable used to de-dupe the critical section lockup events for the
    /// purposes of updating kcounters.
    ///
    /// Accessed only by observers.
    pub(super) last_counted_begin_ticks: AtomicI64,

    /// The largest worst case value ever _reported_ by a heartbeat checker.
    /// This variable is only ever used by the current checker, and the
    /// acquire/release semantics of the `current_checker_id` variable should
    /// ensure that it is coherent on architectures with weak memory ordering.
    ///
    /// Accessed only by observers.
    pub(super) reported_worst_case_ticks: AtomicI64,

    /// The alert limiter used to rate limit warnings printed for ongoing
    /// critical section times (eg; CPUs which enter critical sections, but don't
    /// exit them for so long that a heartbeat checker notices them)
    ///
    /// Accessed only by observers.
    pub(super) ongoing_call_alert_limiter: EventLimiter<{ zx_sec(1) }>,

    /// The alert limiter used to rate limit warnings printed when the heartbeat
    /// monitor notices new, unreported, worst case values.
    ///
    /// Accessed only by observers.
    pub(super) worst_case_alert_limiter: EventLimiter<{ zx_sec(1) }>,
}

impl CriticalSectionState {
    /// Creates the initial critical section state: not in a section, nothing observed.
    const fn new() -> Self {
        Self {
            depth: AtomicU32::new(0),
            name: AtomicPtr::new(ptr::null_mut()),
            worst_case_ticks: AtomicI64::new(0),
            begin_ticks: AtomicI64::new(0),
            last_counted_begin_ticks: AtomicI64::new(0),
            reported_worst_case_ticks: AtomicI64::new(0),
            ongoing_call_alert_limiter: EventLimiter::new(),
            worst_case_alert_limiter: EventLimiter::new(),
        }
    }
}

/// Per CPU state for lockup detector.
#[repr(C)]
pub(super) struct LockupDetectorState {
    /// ZST to force alignment to target arch cache lines.
    _align: CpuAlignMarker,

    /////////////////////////////////////////////////////////////////////////////
    //
    // Common per-cpu lockup detector state
    //
    /////////////////////////////////////////////////////////////////////////////
    /// The ID of CPU who is currently performing a check of this CPUs conditions,
    /// or `INVALID_CPU` if no CPU currently is. Used to prevent multiple CPUs from
    /// recognizing and reporting the same condition on the same CPU concurrently.
    ///
    /// TODO(johngro): This really should just be a spinlock which is only ever
    /// try-locked during the check.  If spinlocks had a trylock operation which
    /// was compatible with clang static analysis, we could make use of it here
    /// along with static annotations to catch mistakes.
    pub(super) current_checker_id: AtomicU32,

    /////////////////////////////////////////////////////////////////////////////
    //
    // Per-cpu lockup detector state used for detecting the "heartbeat" condition.
    //
    /////////////////////////////////////////////////////////////////////////////
    /// See [`HeartbeatState`].
    pub(super) heartbeat: HeartbeatState,

    /////////////////////////////////////////////////////////////////////////////
    //
    // Per-cpu lockup detector state used for detecting the "critical section"
    // condition.
    //
    /////////////////////////////////////////////////////////////////////////////
    /// See [`CriticalSectionState`].
    pub(super) critical_section: CriticalSectionState,
}

impl LockupDetectorState {
    /// Creates the initial state of a CPU that is not being checked, not emitting heartbeats,
    /// and not in a critical section.
    const fn new() -> Self {
        Self {
            _align: CpuAlignMarker,
            current_checker_id: AtomicU32::new(INVALID_CPU),
            heartbeat: HeartbeatState::new(),
            critical_section: CriticalSectionState::new(),
        }
    }

    /// Returns the state of the CPU numbered `cpu`.
    #[inline]
    pub(super) fn get(cpu: cpu_num_t) -> &'static Self {
        &LOCKUP_DETECTOR_PER_CPU_STATE[cpu as usize]
    }

    /// Returns the state of the CPU numbered `cpu` without bounds checking, for critical section
    /// fast paths.
    ///
    /// # Safety
    ///
    /// `(cpu as usize)` must be less than `SMP_MAX_CPUS`.
    #[inline(always)]
    pub(super) unsafe fn get_unchecked(cpu: cpu_num_t) -> &'static Self {
        let cpu = cpu as usize;
        debug_assert!(cpu < SMP_MAX_CPUS);
        // SAFETY: Guaranteed by the caller.
        unsafe { LOCKUP_DETECTOR_PER_CPU_STATE.get_unchecked(cpu) }
    }
}

// `LockupDetectorState` is `#[repr(C)]` and cache-line aligned so that each CPU's state occupies
// its own cache lines; pin the resulting size and alignment.
zr::static_assert!(size_of::<LockupDetectorState>() == 2 * MAX_CACHE_LINE);
zr::static_assert!(align_of::<LockupDetectorState>() == MAX_CACHE_LINE);

/// The lockup detector state of every CPU, indexed by CPU number.
static LOCKUP_DETECTOR_PER_CPU_STATE: [LockupDetectorState; SMP_MAX_CPUS] =
    [const { LockupDetectorState::new() }; SMP_MAX_CPUS];
