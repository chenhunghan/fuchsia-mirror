// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Lockup detector diagnostics and console commands.

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, Ordering, compiler_fence};
use zx_types::{zx_duration_boot_t, zx_duration_boot_ticks_t, zx_instant_boot_ticks_t};

use crate::arch_rs::{CpuAlignMarker, MAX_CACHE_LINE, curr_cpu_num};
use crate::kernel::event_limiter::EventLimiter;
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::platform_rs::timer::{timer_get_ticks_to_time_ratio, zx_sec};

/// Per-CPU lockup detector state used for detecting the "heartbeat" condition.
#[repr(C)]
struct HeartbeatState {
    /// A flag used to indicate that this CPU is participating in the heartbeat
    /// mechanism. IOW - it is periodically recording that it is still running in
    /// the `last_heartbeat` field, and after doing so it is checking on its peer
    /// CPUs.
    ///
    /// TODO(johngro): Should we merge this field with `last_heartbeat`? Seems
    /// like we should be able to use a sentinel value, such as 0, or
    /// `ZX_TIME_INFINITE` to indicate that the mechanism is disabled.
    active: AtomicBool,

    /// The last time at which this CPU checked in.
    last_heartbeat: AtomicI64,

    /// The largest gap between `last_heartbeat` and now ever observed by a checker.
    /// Note that writes to the field are "protected" by the exclusive role of
    /// "checker".
    max_gap: AtomicI64,

    /// Limiter for the rate at which heartbeat failures are reported. "Protected"
    /// by the exclusive role of "checker".
    alert_limiter: EventLimiter<{ zx_sec(1) }>,
}

/// Per-CPU lockup detector state used for detecting the "critical section"
/// condition.
#[repr(C)]
struct CriticalSectionState {
    /// Critical sections may be nested, so `lockup_timed_begin` and
    /// `lockup_timed_end` (called as code enters and exits critical sections) must
    /// keep track of the depth. This variable is only ever accessed by the code
    /// entering and exiting the CS, and always on the same CPU, so there is no
    /// need for it to be atomic. However, because an interrupt may fire as a
    /// thread enters a critical section and the interrupt handler itself may
    /// enter a critical section, compiler fences must be used when accessing to
    /// ensure that compiler reordering does not lead to problem.
    ///
    /// Accessed only by this CPU. Wrapped in `UnsafeCell` as it is mutated through the shared
    /// `LOCKUP_DETECTOR_PER_CPU_STATE` static (see [`lockup_begin`] and [`lockup_end`]).
    depth: UnsafeCell<u32>,

    /// The name of the active critical section, if any. May be nullptr.
    ///
    /// Accessed by both this CPU and observers.
    name: AtomicPtr<c_char>,

    /// The worst case CS time ever observed by the critical section thread as it
    /// exits the critical section. This variable is written only by the CPU
    /// exiting the critical section, but is read by other CPUs during the
    /// heartbeat sanity checks. While the thread exiting the critical section
    /// reports the worst case time via this variable, only the threads
    /// performing heartbeat sanity checks will ever report issues (via an OOPS)
    /// as a result of a new worst case value.
    ///
    /// Accessed by both this CPU and observers.
    worst_case_ticks: AtomicI64,

    /// The time (tick count) at which the CPU entered the critical section.
    ///
    /// This field is used to establish Release-Acquire ordering of changes made
    /// by critical section threads and observed by observers.
    ///
    /// Accessed by both this CPU and observers.
    begin_ticks: AtomicI64,

    /// State variable used to de-dupe the critical section lockup events for the
    /// purposes of updating kcounters.
    ///
    /// Accessed only by observers. Wrapped in `UnsafeCell` as observers mutate it through the
    /// shared `LOCKUP_DETECTOR_PER_CPU_STATE` static.
    last_counted_begin_ticks: UnsafeCell<zx_instant_boot_ticks_t>,

    /// The largest worst case value ever _reported_ by a heartbeat checker.
    /// This variable is only ever used by the current checker, and the
    /// acquire/release semantics of the `current_checker_id` variable should
    /// ensure that it is coherent on architectures with weak memory ordering.
    ///
    /// Accessed only by observers. Wrapped in `UnsafeCell` as observers mutate it through the
    /// shared `LOCKUP_DETECTOR_PER_CPU_STATE` static.
    reported_worst_case_ticks: UnsafeCell<zx_duration_boot_ticks_t>,

    /// The alert limiter used to rate limit warnings printed for ongoing
    /// critical section times (eg; CPUs which enter critical sections, but don't
    /// exit them for so long that a heartbeat checker notices them).
    ///
    /// Accessed only by observers.
    ongoing_call_alert_limiter: EventLimiter<{ zx_sec(1) }>,

    /// The alert limiter used to rate limit warnings printed when the heartbeat
    /// monitor notices new, unreported, worst case values.
    ///
    /// Accessed only by observers.
    worst_case_alert_limiter: EventLimiter<{ zx_sec(1) }>,
}

/// Per-CPU state for lockup detector (`struct LockupDetectorState`).
#[repr(C)]
struct LockupDetectorState {
    _align: CpuAlignMarker,

    /// The ID of CPU who is currently performing a check of this CPUs conditions,
    /// or `INVALID_CPU` if no CPU currently is. Used to prevent multiple CPUs from
    /// recognizing and reporting the same condition on the same CPU concurrently.
    ///
    /// TODO(johngro): This really should just be a spinlock which is only ever
    /// try-locked during the check. If spinlocks had a trylock operation which
    /// was compatible with clang static analysis, we could make use of it here
    /// along with static annotations to catch mistakes.
    current_checker_id: AtomicU32,

    /// Per-CPU lockup detector state used for detecting the "heartbeat" condition.
    heartbeat: HeartbeatState,

    /// Per-CPU lockup detector state used for detecting the "critical section" condition.
    critical_section: CriticalSectionState,
}

zr::static_assert!(size_of::<LockupDetectorState>() == 2 * MAX_CACHE_LINE);
zr::static_assert!(align_of::<LockupDetectorState>() == MAX_CACHE_LINE);
zr::static_assert!(offset_of!(LockupDetectorState, current_checker_id) == 0);
zr::static_assert!(offset_of!(LockupDetectorState, heartbeat) == 8);
zr::static_assert!(offset_of!(LockupDetectorState, heartbeat.active) == 8);
zr::static_assert!(offset_of!(LockupDetectorState, heartbeat.last_heartbeat) == 16);
zr::static_assert!(offset_of!(LockupDetectorState, heartbeat.max_gap) == 24);
zr::static_assert!(offset_of!(LockupDetectorState, heartbeat.alert_limiter) == 32);
zr::static_assert!(offset_of!(LockupDetectorState, critical_section) == 40);
zr::static_assert!(offset_of!(LockupDetectorState, critical_section.depth) == 40);
zr::static_assert!(offset_of!(LockupDetectorState, critical_section.name) == 48);
zr::static_assert!(offset_of!(LockupDetectorState, critical_section.worst_case_ticks) == 56);
zr::static_assert!(offset_of!(LockupDetectorState, critical_section.begin_ticks) == 64);
zr::static_assert!(
    offset_of!(LockupDetectorState, critical_section.last_counted_begin_ticks) == 72
);
zr::static_assert!(
    offset_of!(LockupDetectorState, critical_section.reported_worst_case_ticks) == 80
);
zr::static_assert!(
    offset_of!(LockupDetectorState, critical_section.ongoing_call_alert_limiter) == 88
);
zr::static_assert!(
    offset_of!(LockupDetectorState, critical_section.worst_case_alert_limiter) == 96
);

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Per-CPU lockup detector state array defined in `lockup_detector.cc`.
    ///
    /// Although this static is immutable, every field of `LockupDetectorState` that is mutated
    /// (by Rust or C++) is atomic or wrapped in `UnsafeCell`, as mutating any other byte of an
    /// immutable static is undefined behavior.
    #[link_name = "gLockupDetectorPerCpuState"]
    static LOCKUP_DETECTOR_PER_CPU_STATE: [LockupDetectorState; SMP_MAX_CPUS];

    /// Returns the critical section lockup threshold in ticks.
    ///
    /// # Safety
    ///
    /// Always safe to call; performs a relaxed atomic load of the critical section threshold.
    fn cpp_lockup_get_cs_threshold_ticks() -> zx_duration_boot_ticks_t;
    /// Returns the heartbeat period in nanoseconds.
    ///
    /// # Safety
    ///
    /// Always safe to call; performs a relaxed atomic load of the heartbeat period.
    fn cpp_lockup_get_hb_period() -> zx_duration_boot_t;
    /// Returns the heartbeat age threshold in nanoseconds.
    ///
    /// # Safety
    ///
    /// Always safe to call; performs a relaxed atomic load of the heartbeat threshold.
    fn cpp_lockup_get_hb_threshold() -> zx_duration_boot_t;
    /// Enters a named timed critical section on the current CPU.
    ///
    /// # Safety
    ///
    /// `name` must be a valid `'static` null-terminated C string, and preemption or interrupts
    /// must be disabled on the current CPU.
    fn cpp_lockup_timed_begin(name: *const c_char);
    /// Exits a timed critical section on the current CPU.
    ///
    /// # Safety
    ///
    /// Must balance a preceding `cpp_lockup_timed_begin` call on the same CPU with preemption or
    /// interrupts disabled.
    fn cpp_lockup_timed_end();
    /// Returns the number of times a "critical section threshold exceeded" oops was triggered.
    fn cpp_lockup_get_critical_section_oops_count() -> i64;
    /// Returns the number of times a "no heartbeat" oops was triggered.
    fn cpp_lockup_get_no_heartbeat_oops_count() -> i64;
    fn cpp_lockup_percpu_shutdown();
    fn cpp_can_dump_registers_and_backtrace() -> bool;
    fn cpp_dump_registers_and_backtrace_stdout(cpu: u32) -> zx_types::zx_status_t;
}

#[inline]
fn ticks_to_duration(ticks: zx_duration_boot_ticks_t) -> zx_duration_boot_t {
    timer_get_ticks_to_time_ratio().scale::<0>(ticks)
}

/// Accessors for `CriticalSectionLockupChecker`.
pub struct CriticalSectionLockupChecker;

impl CriticalSectionLockupChecker {
    /// Returns the critical section lockup threshold in ticks.
    #[inline]
    pub fn threshold_ticks() -> zx_duration_boot_ticks_t {
        // SAFETY: Safe FFI call into `CriticalSectionLockupChecker::threshold_ticks()`.
        unsafe { cpp_lockup_get_cs_threshold_ticks() }
    }
}

/// Accessors for `HeartbeatLockupChecker`.
pub struct HeartbeatLockupChecker;

impl HeartbeatLockupChecker {
    /// Returns the period at which CPUs emit heartbeats.
    #[inline]
    pub fn period() -> zx_duration_boot_t {
        // SAFETY: Safe FFI call into `HeartbeatLockupChecker::period()`.
        unsafe { cpp_lockup_get_hb_period() }
    }

    /// Returns the heartbeat age threshold.
    #[inline]
    pub fn threshold() -> zx_duration_boot_t {
        // SAFETY: Safe FFI call into `HeartbeatLockupChecker::threshold()`.
        unsafe { cpp_lockup_get_hb_threshold() }
    }
}

/// Must match `LOCKUP_CRITICAL_SECTION_ENALBED` in `<lib/lockup_detector.h>`, which, when 0,
/// compiles the C++ `lockup_begin()` and `lockup_end()` calls out of `LOCKUP_BEGIN` and
/// `LOCKUP_END`.
const LOCKUP_CRITICAL_SECTION_ENABLED: bool = true;

/// Returns the critical section state of the calling CPU.
///
/// Only a raw pointer is returned, as other CPUs concurrently access the state.
#[inline(always)]
fn current_critical_section_state() -> *const CriticalSectionState {
    let cpu = curr_cpu_num() as usize;
    // As for the C++ `gLockupDetectorPerCpuState[arch_curr_cpu_num()]`, the index is not bounds
    // checked outside of debug builds, as these accesses are on the monitored spinlock hot path.
    debug_assert!(cpu < SMP_MAX_CPUS);
    // SAFETY: `LOCKUP_DETECTOR_PER_CPU_STATE` is a valid static array of `SMP_MAX_CPUS` elements,
    // and the number of the calling CPU is always less than `SMP_MAX_CPUS`, so `cpu` indexes an
    // element of it. Only raw pointers to the element and its field are formed.
    unsafe {
        let state = (&raw const LOCKUP_DETECTOR_PER_CPU_STATE).cast::<LockupDetectorState>();
        &raw const (*state.add(cpu)).critical_section
    }
}

/// Indicates that the calling CPU is entering a critical section named `name`, where it might
/// appear to be locked up (`LOCKUP_BEGIN`).
///
/// This is the Rust equivalent of the C++ `lockup_begin()` in
/// `<lib/lockup_detector/inline_impl.h>`, and must stay in sync with it.
///
/// # Safety
///
/// The caller must have preemption or interrupts disabled on the calling CPU, and must balance
/// this call with a [`lockup_end`] on the same CPU (including before returning from an interrupt
/// handler if called from interrupt context). Also, any interrupt handler on the same CPU that
/// calls [`lockup_begin`] must balance its own [`lockup_begin`] / [`lockup_end`] calls before
/// returning.
#[inline]
pub unsafe fn lockup_begin(name: &'static CStr) {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }
    let cs_state = current_critical_section_state();
    // SAFETY: `cs_state` points to the calling CPU's live `CriticalSectionState`, whose `depth` is
    // only accessed by that CPU, which the caller keeps this thread on. An interrupt handler on
    // this CPU may enter and leave a critical section of its own, but restores `depth` before
    // returning.
    let depth = unsafe { (*cs_state).depth.get() };

    // We must maintain the invariant that if a call to `lockup_begin` increments the depth, the
    // matching call to `lockup_end` decrements it. The most reliable way to accomplish that is to
    // always increment and always decrement.
    //
    // SAFETY: As above, `depth` is valid and only accessed by this CPU. Raw pointer `read` and
    // `write` avoid materializing a `&mut u32` reference across the read-modify-write when
    // interrupts are enabled.
    let new_depth = unsafe {
        let new_depth = depth.read().wrapping_add(1);
        depth.write(new_depth);
        new_depth
    };
    if new_depth != 1 {
        // Matches the C++ `likely(outermost)`: nested critical sections are rare.
        core::hint::cold_path();
        return;
    }

    // This is the outermost critical section. However, we may be racing with an interrupt handler
    // that may call into `lockup_begin`. Use a compiler fence to ensure that the compiler cannot
    // reorder upcoming stores to precede the depth=1 store above. If the compiler were to make
    // such a reordering, the `lockup_begin` call made by the interrupt handler may incorrectly
    // believe its critical section is the outermost critical section because it has not seen our
    // depth=1 store.
    //
    // Note, there is a small gap here where the `CriticalSectionLockupChecker` may fail to notice
    // a lockup (though the `HeartbeatLockupChecker` may still detect it). Consider the following:
    //
    //   1. We have stored depth=1, but not yet stored `begin_ticks`.
    //   2. An interrupt fires and the handler calls `lockup_begin`.
    //   3. The handler sees that depth is 1 so it does nothing.
    //   4. The CPU enters an infinite loop.
    //   5. `CriticalSectionLockupChecker` sees that `begin_ticks` has not been set so it assumes
    //      the CPU is not in a critical section.
    //
    // If interrupts are enabled at the time of the infinite loop in step 4, the system may never
    // detect the problem. If instead of an infinite loop we just spend a lot of time in the
    // interrupt handler, we may never know it because `begin_ticks` would only get set by the
    // outer critical section *after* returning from the handler.
    //
    // One way to close the gap would be to use `begin_ticks` rather than depth to determine if
    // we're already in a critical section. Instead of storing `begin_ticks` when depth=1, we would
    // store `begin_ticks` only if it's currently unset (i.e. use an atomic compare and exchange
    // with an expected value of 0). However, this would increase the cost of critical section
    // instrumentation. Because the gap is small and we have heartbeats we have chosen to live
    // with it rather than pay the price of atomic compare and exchange.
    compiler_fence(Ordering::SeqCst);

    // `Relaxed` rather than `Release` to minimize the performance impact. As a result,
    // `HeartbeatLockupChecker` and `CriticalSectionLockupChecker` may see stale name values
    // because there is nothing for them to synchronize-with.
    //
    // SAFETY: `cs_state` points to the calling CPU's live `CriticalSectionState`, and `name` is
    // atomic.
    unsafe { (*cs_state).name.store(name.as_ptr().cast_mut(), Ordering::Relaxed) };
}

/// Indicates that the calling CPU has left the critical section entered by the matching
/// [`lockup_begin`] (`LOCKUP_END`).
///
/// This is the Rust equivalent of the C++ `lockup_end()` in
/// `<lib/lockup_detector/inline_impl.h>`, and must stay in sync with it.
///
/// # Safety
///
/// The caller must have preemption or interrupts disabled on the calling CPU, and must balance a
/// preceding [`lockup_begin`] call on the same CPU (including before returning from an interrupt
/// handler if the matching [`lockup_begin`] was called in that interrupt handler). Also, any
/// interrupt handler on the same CPU that calls [`lockup_begin`] must balance its own
/// [`lockup_begin`] / [`lockup_end`] calls before returning.
#[inline]
pub unsafe fn lockup_end() {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }
    let cs_state = current_critical_section_state();
    // SAFETY: As in `lockup_begin`.
    let depth = unsafe { (*cs_state).depth.get() };
    // SAFETY: As in `lockup_begin`, `depth` is valid and only accessed by this CPU.
    if unsafe { depth.read() } == 1 {
        // This is the outermost critical section. However, we may be racing with an interrupt
        // handler that may call into `lockup_begin`. Use a compiler fence to ensure that the store
        // below cannot be compiler reordered to precede the depth operations above.
        compiler_fence(Ordering::SeqCst);
        // See the comment in `lockup_begin` at the point where the name is stored.
        //
        // SAFETY: `cs_state` points to the calling CPU's live `CriticalSectionState`, and `name`
        // is atomic.
        unsafe { (*cs_state).name.store(ptr::null_mut(), Ordering::Relaxed) };
    } else {
        // Matches the C++ `likely(outermost)`: nested critical sections are rare.
        core::hint::cold_path();
    }
    // SAFETY: As above. Raw pointer `read` and `write` avoid materializing a `&mut u32` reference
    // across the read-modify-write when interrupts are enabled.
    unsafe {
        debug_assert!(depth.read() > 0);
        depth.write(depth.read().wrapping_sub(1));
    }
}

/// Enters a timed critical section (`LOCKUP_TIMED_BEGIN`).
///
/// # Safety
///
/// `name` must be a valid `'static` null-terminated C string, and the caller must have preemption
/// or interrupts disabled on the current CPU.
#[inline]
pub unsafe fn lockup_timed_begin(name: &'static CStr) {
    // SAFETY: Caller guarantees preemption/interrupts are disabled and `name` has static lifetime.
    unsafe { cpp_lockup_timed_begin(name.as_ptr()) };
}

/// Exits a timed critical section (`LOCKUP_TIMED_END`).
///
/// # Safety
///
/// Must match a preceding [`lockup_timed_begin`] call on the same CPU with preemption or
/// interrupts disabled.
#[inline]
pub unsafe fn lockup_timed_end() {
    // SAFETY: Caller guarantees this balances `lockup_timed_begin` on the current CPU.
    unsafe { cpp_lockup_timed_end() };
}

/// Returns the number of times a "critical section threshold exceeded" oops was triggered.
#[inline]
pub fn lockup_get_critical_section_oops_count() -> i64 {
    // SAFETY: Sums a kernel counter across CPUs with no preconditions.
    unsafe { cpp_lockup_get_critical_section_oops_count() }
}

/// Returns the number of times a "no heartbeat" oops was triggered.
#[inline]
pub fn lockup_get_no_heartbeat_oops_count() -> i64 {
    // SAFETY: Sums a kernel counter across CPUs with no preconditions.
    unsafe { cpp_lockup_get_no_heartbeat_oops_count() }
}

/// Shuts down the lockup detector for the calling CPU.
#[inline]
pub fn lockup_percpu_shutdown() {
    // SAFETY: Shuts down the lockup detector on the calling CPU.
    unsafe { cpp_lockup_percpu_shutdown() };
}

/// Returns `true` if the target architecture supports dumping registers and backtraces for a CPU.
#[inline]
pub fn can_dump_registers_and_backtrace() -> bool {
    // SAFETY: Pure query function with no preconditions.
    unsafe { cpp_can_dump_registers_and_backtrace() }
}

/// Dumps registers and backtrace for `cpu` to stdout.
#[inline]
pub fn dump_registers_and_backtrace_stdout(cpu: u32) -> Result<(), zx_status::Status> {
    if !crate::kernel::types::is_valid_cpu_num(cpu) || cpu >= crate::arch_rs::max_num_cpus() {
        return Err(zx_status::Status::INVALID_ARGS);
    }
    // SAFETY: `cpu` is a valid CPU index strictly less than both `SMP_MAX_CPUS` and
    // `max_num_cpus()`, and `stdout` is passed internally in C++.
    zx_status::Status::ok(unsafe { cpp_dump_registers_and_backtrace_stdout(cpu) })
}

#[cfg(console_enabled)]
mod console {
    use super::{
        CriticalSectionLockupChecker, HeartbeatLockupChecker, LOCKUP_DETECTOR_PER_CPU_STATE,
        lockup_timed_begin, lockup_timed_end, ticks_to_duration,
    };
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::mp::{cpu_num_to_mask, is_cpu_online};
    use crate::kernel::percpu::PerCpu;
    use crate::kernel::scheduler;
    use crate::kernel::thread::{AutoPreemptDisabler, DEFAULT_PRIORITY, create_with_priority};
    use crate::kernel::types::cpu_num_t;
    use crate::platform_rs::timer::{
        InstantMono, current_boot_time, timer_current_boot_ticks, zx_msec, zx_usec,
    };
    use core::ffi::{CStr, c_char, c_int, c_void};
    use core::sync::atomic::Ordering;
    use kprint::kprintln;
    use zx_status::Status;
    use zx_types::{
        ZX_OK, ZX_TIME_INFINITE, zx_duration_boot_t, zx_duration_boot_ticks_t, zx_instant_boot_t,
        zx_instant_boot_ticks_t,
    };

    fn lockup_status() {
        let ticks: zx_duration_boot_ticks_t = CriticalSectionLockupChecker::threshold_ticks();
        kprintln!(
            "critical section threshold is {} ticks ({} ms)",
            ticks,
            ticks_to_duration(ticks) / zx_msec(1)
        );
        if ticks != 0 {
            for i in 0..(PerCpu::processor_count() as cpu_num_t) {
                if !scheduler::peek_is_active(i) {
                    kprintln!("CPU-{} is not active, skipping", i);
                    continue;
                }

                // SAFETY: `LOCKUP_DETECTOR_PER_CPU_STATE` is a valid static array of `SMP_MAX_CPUS`
                // elements (`i < PerCpu::processor_count() <= SMP_MAX_CPUS`), and only the atomic
                // fields (`begin_ticks`, `name`, `worst_case_ticks`) of `critical_section` are
                // accessed through raw pointer projection.
                let cs_state = unsafe {
                    &raw const LOCKUP_DETECTOR_PER_CPU_STATE[i as usize].critical_section
                };
                // SAFETY: `cs_state` points to a valid `CriticalSectionState` with atomic
                // `begin_ticks`.
                let begin_ticks: zx_instant_boot_ticks_t =
                    unsafe { (*cs_state).begin_ticks.load(Ordering::Acquire) };
                // SAFETY: `cs_state` points to a valid `CriticalSectionState` with atomic `name`.
                let name: *const c_char = unsafe { (*cs_state).name.load(Ordering::Relaxed) };
                let now: zx_instant_boot_ticks_t = timer_current_boot_ticks().0;
                // SAFETY: `cs_state` points to a valid `CriticalSectionState` with atomic
                // `worst_case_ticks`.
                let worst_case_usec: i64 = ticks_to_duration(unsafe {
                    (*cs_state).worst_case_ticks.load(Ordering::Relaxed)
                }) / zx_usec(1);
                if begin_ticks == 0 {
                    kprintln!(
                        "CPU-{} not in critical section (worst case {} uSec)",
                        i,
                        worst_case_usec
                    );
                } else {
                    let duration: zx_duration_boot_t = ticks_to_duration(now - begin_ticks);
                    kprintln!(
                        "CPU-{} in critical section ({:s}) for {} ms (worst case {} uSec)",
                        i,
                        if !name.is_null() {
                            // SAFETY: Non-null `cs_state.name` points to a `'static`
                            // null-terminated C string.
                            unsafe { CStr::from_ptr(name) }
                        } else {
                            c"unknown"
                        },
                        duration / zx_msec(1),
                        worst_case_usec
                    );
                }
            }
        }

        kprintln!(
            "heartbeat period is {} ms, heartbeat threshold is {} ms",
            HeartbeatLockupChecker::period() / zx_msec(1),
            HeartbeatLockupChecker::threshold() / zx_msec(1)
        );

        for cpu in 0..(PerCpu::processor_count() as cpu_num_t) {
            if !is_cpu_online(cpu) || !scheduler::peek_is_active(cpu) {
                continue;
            }

            // SAFETY: `LOCKUP_DETECTOR_PER_CPU_STATE` is a valid static array of `SMP_MAX_CPUS`
            // elements whose `heartbeat` fields are all atomic, and `cpu <
            // PerCpu::processor_count() <= SMP_MAX_CPUS`.
            let hb_state = unsafe { &LOCKUP_DETECTOR_PER_CPU_STATE[cpu as usize].heartbeat };
            if !hb_state.active.load(Ordering::SeqCst) {
                kprintln!("CPU-{} heartbeats disabled", cpu);
                continue;
            }
            let last_heartbeat: zx_instant_boot_t = hb_state.last_heartbeat.load(Ordering::SeqCst);
            let age: zx_duration_boot_t = current_boot_time().0.saturating_sub(last_heartbeat);
            let max_gap: zx_duration_boot_t = hb_state.max_gap.load(Ordering::SeqCst);
            kprintln!(
                "CPU-{} last heartbeat at {} ms, age is {} ms, max gap is {} ms",
                cpu,
                last_heartbeat / zx_msec(1),
                age / zx_msec(1),
                max_gap / zx_msec(1)
            );
        }
    }

    /// Runs `func` on `cpu`, passing `duration` as an argument.
    fn run_lockup_func(
        cpu: cpu_num_t,
        mut duration: zx_duration_boot_t,
        func: extern "C" fn(*mut c_void) -> c_int,
    ) {
        // SAFETY: `func` is a valid thread entry routine and `&raw mut duration` remains valid on
        // the stack until `t.join` returns below.
        let Ok(t) = (unsafe {
            create_with_priority(
                c"lockup-test".as_ptr(),
                func,
                (&raw mut duration).cast::<c_void>(),
                DEFAULT_PRIORITY,
            )
        }) else {
            kprintln!("failed to create lockup-test thread");
            return;
        };
        // SAFETY: `t` was just created and has not been joined or destroyed.
        unsafe {
            t.set_cpu_affinity(cpu_num_to_mask(cpu));
            t.resume();
            let _ = t.join(InstantMono(ZX_TIME_INFINITE));
        }
    }

    /// Trigger a temporary lockup of `cpu` by holding a spinlock for `duration`.
    fn lockup_trigger_spinlock(cpu: cpu_num_t, duration: zx_duration_boot_t) {
        extern "C" fn spinlock_thread(arg: *mut c_void) -> c_int {
            // SAFETY: `arg` points to `duration` on `run_lockup_func`'s stack, which remains valid
            // until the thread is joined.
            let duration: zx_duration_boot_t = unsafe { *(arg as *const zx_duration_boot_t) };
            // Acquire a spinlock and hold it for `duration`.
            crate::ksync_kernel::declare_singleton_monitored_spinlock!(LockupTestLock);
            ksync::lock!(let _guard = LockupTestLock::get().lock_with(ksync::source_tag!()));
            let deadline: zx_instant_boot_t = current_boot_time().0.saturating_add(duration);
            while current_boot_time().0 < deadline {
                libarch::r#yield();
            }
            0
        }
        run_lockup_func(cpu, duration, spinlock_thread);
    }

    /// Trigger a temporary lockup of `cpu` by remaining in a critical section for `duration`.
    fn lockup_trigger_critical_section(cpu: cpu_num_t, duration: zx_duration_boot_t) {
        extern "C" fn cs_thread(arg: *mut c_void) -> c_int {
            // SAFETY: `arg` points to `duration` on `run_lockup_func`'s stack, and preemption is
            // disabled by `_preempt_disable` for the duration of the timed critical section.
            unsafe {
                let duration: zx_duration_boot_t = *(arg as *const zx_duration_boot_t);
                let _preempt_disable = AutoPreemptDisabler::new();
                lockup_timed_begin(c"trigger-tool");
                let deadline: zx_instant_boot_t = current_boot_time().0.saturating_add(duration);
                while current_boot_time().0 < deadline {
                    libarch::r#yield();
                }
                lockup_timed_end();
            }
            0
        }
        run_lockup_func(cpu, duration, cs_thread);
    }

    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where
    /// `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn cmd_lockup(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = argv[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!(
                "{:s} status                                 : print lockup detector status",
                cmd_name
            );
            kprintln!(
                concat!(
                    "{:s} test_spinlock <cpu> <num msec>         : ",
                    "hold spinlock on <cpu> for <num msec>",
                ),
                cmd_name
            );
            kprintln!(
                concat!(
                    "{:s} test_critical_section <cpu> <num msec> : ",
                    "hold critical section on <cpu> for <num msec>",
                ),
                cmd_name
            );
            Status::INTERNAL.into_raw()
        };

        if argc < 2 {
            kprintln!("not enough arguments");
            return usage();
        }

        if argv[1].as_str() == "status" {
            lockup_status();
        } else if argv[1].as_str() == "test_spinlock" {
            if argc < 4 {
                return usage();
            }
            let cpu = argv[2].arg_uint as cpu_num_t;
            let ms = argv[3].arg_uint as u32;
            kprintln!("test_spinlock: locking up CPU {} for {} ms", cpu, ms);
            lockup_trigger_spinlock(cpu, zx_msec(ms as i64));
            kprintln!("done");
        } else if argv[1].as_str() == "test_critical_section" {
            if argc < 4 {
                return usage();
            }
            let cpu = argv[2].arg_uint as cpu_num_t;
            let ms = argv[3].arg_uint as u32;
            kprintln!("test_critical_section: locking up CPU {} for {} ms", cpu, ms);
            lockup_trigger_critical_section(cpu, zx_msec(ms as i64));
            kprintln!("done");
        } else {
            kprintln!("unknown command");
            return usage();
        }

        ZX_OK
    }

    static_command!(
        CMD_LOCKUP,
        c"lockup".as_ptr(),
        c"lockup detector commands".as_ptr(),
        cmd_lockup,
        CMD_AVAIL_NORMAL
    );
}

/// Returns the critical section depth and name of the calling CPU, for tests.
///
/// Interrupts must be disabled, so that the values are not changed by an interrupt handler.
#[cfg(ktest)]
pub(crate) fn current_critical_section_depth_and_name() -> (u32, *const c_char) {
    debug_assert!(crate::arch_rs::ints_disabled());
    let cs_state = current_critical_section_state();
    // SAFETY: `cs_state` points to the calling CPU's live `CriticalSectionState`. `depth` is only
    // accessed by this CPU, which has interrupts disabled, and `name` is atomic.
    unsafe { (*(*cs_state).depth.get(), (*cs_state).name.load(Ordering::Relaxed).cast_const()) }
}

/// Tests of the Rust `lockup_begin` and `lockup_end`.
#[cfg(ktest)]
#[unittest::suite(name = "lockup_detector_rs")]
mod tests {
    use super::{current_critical_section_depth_and_name, lockup_begin, lockup_end};
    use crate::arch_rs::InterruptDisableGuard;
    use core::ptr;
    use unittest::expect_true;

    /// Verifies that the depth tracks nesting and only the outermost section is named.
    #[test]
    fn nested_critical_section() {
        // Mirrors the C++ `nested_critical_section` test.
        let _interrupt_guard = InterruptDisableGuard::new();

        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );

        const OUTER: &core::ffi::CStr = c"NestedCriticalSectionTest-outer";
        // SAFETY: Interrupts are disabled, and this is balanced by a `lockup_end` below.
        unsafe { lockup_begin(OUTER) };
        expect_true!(
            current_critical_section_depth_and_name() == (1, OUTER.as_ptr()),
            "critical section depth and name"
        );

        const INNER: &core::ffi::CStr = c"NestedCriticalSectionTest-inner";
        // SAFETY: Interrupts are disabled, and this is balanced by a `lockup_end` below.
        unsafe { lockup_begin(INNER) };
        // No change to the name, because only the outermost critical section is tracked.
        expect_true!(
            current_critical_section_depth_and_name() == (2, OUTER.as_ptr()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled, and this balances the inner `lockup_begin`.
        unsafe { lockup_end() };
        expect_true!(
            current_critical_section_depth_and_name() == (1, OUTER.as_ptr()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled, and this balances the outer `lockup_begin`.
        unsafe { lockup_end() };
        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );
    }
}
