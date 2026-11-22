// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The lockup detector: per-CPU heartbeats and timed critical sections that let CPUs notice when
//! one of their peers has stopped making progress.
//!
//! Documentation for this library can be found in kernel/lib/lockup_detector/README.md.

#[path = "diagnostics.rs"]
mod diagnostics;
#[path = "inline_impl.rs"]
mod inline_impl;
#[path = "state.rs"]
mod state;
#[cfg(ktest)]
#[path = "tests.rs"]
mod tests;

pub use diagnostics::{can_dump_registers_and_backtrace, dump_registers_and_backtrace};
pub use inline_impl::{lockup_begin, lockup_begin_raw_on_cpu, lockup_end};

#[cfg(any(ktest, console_enabled))]
use core::ffi::CStr;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use affine::{Ratio, Round};
use boot_options::BootOptions;
use debug::{dprintf, kernel_oops};
use kprint::kprintln;
use lazy_init::{LazyInit, NoCheck};
use libc::stdio::{fprintf, stdout};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use zx_types::{
    ZX_EXCP_POLICY_CODE_SPIN_DETECTION, zx_duration_boot_t, zx_duration_boot_ticks_t,
    zx_instant_boot_t, zx_instant_boot_ticks_t,
};

use diagnostics::{FailureSeverity, dump_common_diagnostics};
#[cfg(any(ktest, console_enabled))]
use inline_impl::{call_if_outer_and_leave, enter};
use state::LockupDetectorState;

use crate::arch_rs::curr_cpu_num as arch_curr_cpu_num;
use crate::counters::{Counter, define_kcounter};
use crate::crashlog::STDOUT_PANIC_BUFFER;
use crate::crypto::global_prng;
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::mp::is_cpu_online;
use crate::kernel::percpu::PerCpu;
use crate::kernel::relaxed_atomic::RelaxedAtomicI64;
use crate::kernel::scheduler;
use crate::kernel::thread::{
    THREAD_SIGNAL_POLICY_EXCEPTION, Thread, current_get, preempt_set_pending,
    signal_policy_exception,
};
use crate::kernel::timer::Timer;
use crate::kernel::types::{INVALID_CPU, cpu_num_t};
use crate::pdev_watchdog::{hw_watchdog_pet, hw_watchdog_suppress_petting};
use crate::platform_rs::power::{
    PanicStartHaltOtherCpus, PlatformHaltAction, ZirconCrashReason, platform_halt,
    platform_panic_start_with_option,
};
use crate::platform_rs::timer::{
    DurationBoot, current_boot_time, timer_current_boot_ticks, timer_get_ticks_to_time_ratio,
    zx_msec, zx_usec,
};

/// Whether critical section instrumentation is compiled in.
pub(super) const LOCKUP_CRITICAL_SECTION_ENABLED: bool = true;

// Counter for the number of lockups detected.
define_kcounter!(COUNTER_LOCKUP_CS_COUNT, "lockup_detector.critical_section.count", Sum);

// Counters for number of lockups exceeding a given duration.
define_kcounter!(
    COUNTER_LOCKUP_CS_EXCEEDING_10MS,
    "lockup_detector.critical_section.exceeding_ms.10",
    Sum
);
define_kcounter!(
    COUNTER_LOCKUP_CS_EXCEEDING_1000MS,
    "lockup_detector.critical_section.exceeding_ms.1000",
    Sum
);
define_kcounter!(
    COUNTER_LOCKUP_CS_EXCEEDING_100000MS,
    "lockup_detector.critical_section.exceeding_ms.100000",
    Sum
);

// Counts the number of times the lockup detector has emitted a "no heartbeat" oops.
define_kcounter!(COUNTER_LOCKUP_NO_HEARTBEAT_OOPS, "lockup_detector.no_heartbeat_oops", Sum);

#[inline]
fn ticks_to_duration(ticks: zx_duration_boot_ticks_t) -> zx_duration_boot_t {
    timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(ticks)
}

#[inline]
fn duration_to_ticks(duration: zx_duration_boot_t) -> zx_duration_boot_ticks_t {
    timer_get_ticks_to_time_ratio().inverse().scale::<{ Round::DOWN }>(duration)
}

/// The exclusive role of reporting a fatal condition and rebooting the system.  It is acquired
/// like a try-lock and never released.
struct FatalConditionReporterRole {
    reporter_id: AtomicU32,
}

impl FatalConditionReporterRole {
    const fn new() -> Self {
        Self { reporter_id: AtomicU32::new(INVALID_CPU) }
    }

    fn acquire(&self) -> bool {
        // A fatal condition has been observed and we are on the road to rebooting.
        // Attempt to pet the watchdog one last time, then suppress all future pets.
        // If anything goes wrong from here on out which prevents us from reporting
        // the fatal condition, we want the HW WDT (if present) to reboot us.
        hw_watchdog_pet();
        hw_watchdog_suppress_petting(true);

        // Now that the WDT is armed, attempt to assume the role of the fatal
        // condition reporter.  If we fail, then someone else is already in the
        // process of reporting the fatal condition.  We will just leave them to
        // their task.  If they hang while attempting to write a crashlog and reboot
        // the system, the HW WDT will end up rebooting the system for them (if
        // present).
        let current_cpu = arch_curr_cpu_num();
        self.reporter_id
            .compare_exchange(INVALID_CPU, current_cpu, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// No one should ever actually release the role of the fatal condition
    /// reporter.  Even so, we need to have a call to "release" in place in order
    /// to balance the acquire.  If we ever _do_ actually make it to this function,
    /// it means that the CPU which was assigned the role of fatal condition
    /// reporter failed to reboot the system for some bizarre reason.  Do our best
    /// to panic the system in this case.
    fn release(&self) -> ! {
        panic!("Fatal condition reporter failed to reboot!");
    }
}

static FATAL_CONDITION_REPORTER_ROLE: FatalConditionReporterRole =
    FatalConditionReporterRole::new();

/// Checks that each CPU keeps emitting heartbeats.
struct HeartbeatLockupChecker;

// Note that the following static parameters are non-atomic because they are
// currently setup only once by the primary CPU before any other CPUs have
// started, and because they cannot change after setup. If we ever turn these
// into dynamic properties which can be adjusted while the system is running,
// we need to come back here and make them atomics with proper memory order
// semantics.

/// The period at which CPUs emit heartbeats.  0 means heartbeats are disabled.
static HEARTBEAT_PERIOD: LazyInit<zx_duration_boot_t, NoCheck> = LazyInit::uninit();

/// If a CPU's most recent heartbeat is older than this threshold, it is
/// considered to be locked up and a KERNEL OOPS will be triggered.
static HEARTBEAT_THRESHOLD: LazyInit<zx_duration_boot_t, NoCheck> = LazyInit::uninit();

/// If a CPU's most recent heartbeat is older than this threshold, it is
/// considered to be locked up and crashlog will be generated followed by a
/// reboot.
static HEARTBEAT_FATAL_THRESHOLD: LazyInit<zx_duration_boot_t, NoCheck> = LazyInit::uninit();

impl HeartbeatLockupChecker {
    #[inline]
    fn period() -> zx_duration_boot_t {
        // SAFETY: Initialized once on the boot CPU in `lockup_init()` before any reader runs.
        unsafe { *HEARTBEAT_PERIOD.get_unchecked() }
    }

    #[inline]
    fn threshold() -> zx_duration_boot_t {
        // SAFETY: Initialized once on the boot CPU in `lockup_init()` before any reader runs.
        unsafe { *HEARTBEAT_THRESHOLD.get_unchecked() }
    }

    #[inline]
    fn fatal_threshold() -> zx_duration_boot_t {
        // SAFETY: Initialized once on the boot CPU in `lockup_init()` before any reader runs.
        unsafe { *HEARTBEAT_FATAL_THRESHOLD.get_unchecked() }
    }

    /// # Safety
    ///
    /// Must be called at most once on the boot CPU before secondary CPUs start and before
    /// `period`, `threshold`, or `fatal_threshold` are read.
    unsafe fn init_static_params() {
        // SAFETY: Guaranteed by the caller.
        unsafe {
            HEARTBEAT_PERIOD
                .init(zx_msec(BootOptions::get().lockup_detector_heartbeat_period_ms as i64));
            HEARTBEAT_THRESHOLD.init(zx_msec(
                BootOptions::get().lockup_detector_heartbeat_age_threshold_ms as i64,
            ));
            HEARTBEAT_FATAL_THRESHOLD
                .init(zx_msec(BootOptions::get().lockup_detector_age_fatal_threshold_ms as i64));
        }
    }

    // TODO(johngro): once state.current_checker_id becomes a more formal
    // spinlock, come back here and require its token.
    fn perform_check(state: &LockupDetectorState, cpu: cpu_num_t, now_boot: zx_instant_boot_t) {
        // If the heartbeat mechanism is currently not active for this CPU, just skip
        // all of the checks.
        let hb_state = &state.heartbeat;
        if !hb_state.active.load(Ordering::SeqCst) {
            return;
        }

        // Observe each of the details we need to know to make a determination of
        // whether or not we should report a failure.
        let observed_last_heartbeat = hb_state.last_heartbeat.load(Ordering::SeqCst);
        let observed_age: zx_duration_boot_t = now_boot.saturating_sub(observed_last_heartbeat);
        let cs_state = &state.critical_section;
        // Note, we're loading name with relaxed semantics so there is nothing
        // ensuring that we see the "latest value".  Ideally we'd use
        // Ordering::Acquire when reading name and Ordering::Release when
        // writing.  However, doing so has a measurable performance impact and it's
        // crucial to minimize lockup_detector overhead.  We tolerate stale values
        // because we're only using name to help us find the point where the lockup
        // occurred.
        let observed_name = cs_state.name.load(Ordering::Relaxed);

        // If this is the worst gap we have ever seen, record that fact now.
        if observed_age > hb_state.max_gap.load(Ordering::SeqCst) {
            hb_state.max_gap.store(observed_age, Ordering::SeqCst);
        }

        // A shared closure used to report errors in a consistent fashion, either to
        // just stdout, or to the stdout_panic buffer in the case that this is a fatal
        // condition.
        let report_failure = |severity: FailureSeverity| {
            let output_target = if severity == FailureSeverity::Fatal {
                STDOUT_PANIC_BUFFER.as_mut_ptr()
            } else {
                stdout()
            };

            // Print an OOPS header so that we properly trigger tefmo checks, but only
            // send it to stdout.  If this a fatal failure, we don't want to waste any
            // bytes saying "OOPS" in the crashlog.  It should be pretty clear from the
            // fact that we are filing a crashlog that things went pretty seriously
            // wrong.
            kernel_oops!("");
            // SAFETY: `output_target` is one of the two `'static` `FILE`s, the format string's
            // conversions match the arguments, and `observed_name` is null or a `'static` C string.
            unsafe {
                fprintf(
                    output_target,
                    c"lockup_detector: no heartbeat from CPU-%u in %ld ms, last_heartbeat=%ld \
                      observed now=%ld name=%s.\nReported by [CPU-%u] (message rate limited)\n"
                        .as_ptr(),
                    cpu,
                    observed_age / zx_msec(1),
                    observed_last_heartbeat,
                    now_boot,
                    if observed_name.is_null() {
                        c"unknown".as_ptr()
                    } else {
                        observed_name.cast_const()
                    },
                    arch_curr_cpu_num(),
                );
            }
            // SAFETY: `dump_common_diagnostics`'s requirements, in its order:
            // - Interrupts are disabled: `timer_tick` runs timer callbacks with them disabled.
            // - `cpu < PerCpu::processor_count()`: `do_heartbeat_and_check_peer_cpus`, the only
            //   caller of this check, passes only CPUs below that count.
            // - `output_target` is one of the two `'static` `FILE`s.
            // - This CPU holds no `queue_lock_` and has no active `ChainLockTransaction`. Both keep
            //   interrupts disabled until they end (`SpinLock::acquire`, `KernelTransactionHooks`),
            //   so the interrupted code had neither, and `TimerQueue::tick_internal` drops
            //   `TimerLock` before it calls back.
            // For a fatal report:
            // - `cpu` is not this CPU: the caller skips `current_cpu`, and with interrupts disabled
            //   this callback cannot migrate.
            // - Terminal panic path: `FATAL_CONDITION_REPORTER_ROLE.acquire()` succeeded,
            //   `platform_panic_start_with_option` ran, and `platform_halt` follows.
            // - On `aarch64`, `cpu` is powered up: `do_heartbeat_and_check_peer_cpus` checks only
            //   online, active peers, this check returned early unless `cpu`'s heartbeats were
            //   active, and the kernel's power-down paths (unplug, `platform_suspend_cpu`,
            //   `platform_suspend_system`) stop a CPU's heartbeats before powering it down. The
            //   console's `power cpu-off` does not, and nothing in its contract covers a report
            //   that follows: not shown. The read's EDPRSR check refuses such a CPU only while its
            //   debug power domain stays on. A CPU past the fatal threshold is taken to be wedged,
            //   not partway into one of the other paths.
            // - On `x86_64`, `(cpu as usize) < SMP_MAX_CPUS`, because
            //   `PerCpu::initialize_secondaries_begin` asserts that its count is at most that.
            //   Kernel code runs at CPL 0.
            unsafe { dump_common_diagnostics(cpu, output_target, severity) };
        };

        // If we have a fatal threshold configured, and we have exceeded that
        // threshold, then it is time to file a crashlog and reboot the system.
        if Self::fatal_threshold() > 0
            && observed_age > Self::fatal_threshold()
            && FATAL_CONDITION_REPORTER_ROLE.acquire()
        {
            // Leave the other CPUs running so we can gather diagnostics.
            platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
            report_failure(FailureSeverity::Fatal);
            platform_halt(PlatformHaltAction::Reboot, ZirconCrashReason::SoftwareWatchdog);
            #[allow(unreachable_code)]
            FATAL_CONDITION_REPORTER_ROLE.release();
        }

        if Self::threshold() > 0
            && observed_age > Self::threshold()
            && hb_state.alert_limiter.ready()
        {
            COUNTER_LOCKUP_NO_HEARTBEAT_OOPS.add(1);
            report_failure(FailureSeverity::Oops);
        }
    }
}

/// Checks that no CPU spends too long in a timed critical section.
struct CriticalSectionLockupChecker;

/// Provides histogram-like kcounter functionality.
struct CounterBucket {
    exceeding: zx_duration_boot_t,
    counter: Counter,
}

const COUNTER_BUCKETS: [CounterBucket; 3] = [
    CounterBucket { exceeding: zx_msec(10), counter: COUNTER_LOCKUP_CS_EXCEEDING_10MS },
    CounterBucket { exceeding: zx_msec(1000), counter: COUNTER_LOCKUP_CS_EXCEEDING_1000MS },
    CounterBucket { exceeding: zx_msec(100000), counter: COUNTER_LOCKUP_CS_EXCEEDING_100000MS },
];

/// These thresholds control whether critical section checking is enabled and how long is "too
/// long".
///
/// A non-zero `CS_THRESHOLD_TICKS` enables critical section checking with a non-fatal action (log,
/// but don't reboot).
///
/// A non-zero `CS_FATAL_THRESHOLD_TICKS` enables critical section checking with a fatal action
/// (reboot).
///
/// These values are expressed in units of ticks rather than nanoseconds because it is faster to
/// read the platform timer's tick count than to get current_boot_time().
///
/// These variables are atomic because, although set early during lockup detector initialization,
/// their values may change to facilitate testing (see
/// [`CriticalSectionLockupChecker::set_threshold_ticks`]).  Use relaxed operations because these
/// fields are accessed within the critical section and must be fast.
static CS_THRESHOLD_TICKS: RelaxedAtomicI64 = RelaxedAtomicI64::new(0);
/// See [`CS_THRESHOLD_TICKS`].
static CS_FATAL_THRESHOLD_TICKS: RelaxedAtomicI64 = RelaxedAtomicI64::new(0);

/// The smallest bucket threshold in ticks; initialized once on the boot CPU in `lockup_init()`
/// before secondary CPUs start and never modified afterwards.
static CS_WORST_CASE_THRESHOLD_TICKS: LazyInit<zx_duration_boot_ticks_t, NoCheck> =
    LazyInit::uninit();

impl CriticalSectionLockupChecker {
    #[inline]
    fn threshold_ticks() -> zx_duration_boot_ticks_t {
        CS_THRESHOLD_TICKS.load()
    }

    #[cfg(ktest)]
    #[inline]
    fn set_threshold_ticks(val: zx_duration_boot_ticks_t) {
        CS_THRESHOLD_TICKS.store(val)
    }

    #[inline]
    fn fatal_threshold_ticks() -> zx_duration_boot_ticks_t {
        CS_FATAL_THRESHOLD_TICKS.load()
    }

    #[inline]
    fn worst_case_threshold_ticks() -> zx_duration_boot_ticks_t {
        // SAFETY: Initialized once on the boot CPU in `lockup_init()` before any reader runs.
        unsafe { *CS_WORST_CASE_THRESHOLD_TICKS.get_unchecked() }
    }

    #[inline]
    fn is_enabled() -> bool {
        Self::threshold_ticks() > 0 || Self::fatal_threshold_ticks() > 0
    }

    /// # Safety
    ///
    /// Must be called at most once on the boot CPU before secondary CPUs start and before
    /// `worst_case_threshold_ticks` is read (mutually exclusive with [`Self::init_disabled`]).
    unsafe fn init_static_params() {
        let threshold_duration =
            zx_msec(BootOptions::get().lockup_detector_critical_section_threshold_ms as i64);
        CS_THRESHOLD_TICKS.store(duration_to_ticks(threshold_duration));

        let fatal_threshold_duration =
            zx_msec(BootOptions::get().lockup_detector_critical_section_fatal_threshold_ms as i64);
        CS_FATAL_THRESHOLD_TICKS.store(duration_to_ticks(fatal_threshold_duration));

        // SAFETY: Guaranteed by the caller.
        unsafe {
            CS_WORST_CASE_THRESHOLD_TICKS.init(duration_to_ticks(COUNTER_BUCKETS[0].exceeding));
        }
    }

    /// Initializes `CS_WORST_CASE_THRESHOLD_TICKS` to its disabled sentinel (`MAX`, matching C++)
    /// when critical section checking is disabled in `lockup_init()`, so tests that temporarily
    /// override `CS_THRESHOLD_TICKS` still read an initialized threshold.
    ///
    /// # Safety
    ///
    /// Must be called at most once on the boot CPU before secondary CPUs start and before
    /// `worst_case_threshold_ticks` is read (mutually exclusive with [`Self::init_static_params`]).
    unsafe fn init_disabled() {
        // SAFETY: Guaranteed by the caller.
        unsafe {
            CS_WORST_CASE_THRESHOLD_TICKS.init(zx_duration_boot_ticks_t::MAX);
        }
    }

    #[inline]
    fn record_critical_section_bucket_counters(lockup_ticks: zx_duration_boot_ticks_t) {
        // Fast abort if the time spent in the critical sections is less than the
        // minimum bucket threshold.
        if lockup_ticks < Self::worst_case_threshold_ticks() {
            return;
        }
        core::hint::cold_path();

        let lockup_duration = ticks_to_duration(lockup_ticks);
        for bucket in COUNTER_BUCKETS.iter().rev() {
            if lockup_duration >= bucket.exceeding {
                bucket.counter.add(1);
                break;
            }
        }
    }

    fn perform_check(
        state: &LockupDetectorState,
        cpu: cpu_num_t,
        now_ticks: zx_instant_boot_ticks_t,
    ) {
        let cs_state = &state.critical_section;

        // Observe all of the info we need to make a decision as to whether or not there
        // has been a condition violation.
        let observed_threshold_ticks = Self::threshold_ticks();
        // Use Acquire semantics to ensure that if we observe a previously stored `begin_ticks`
        // value we will also observe stores to other fields that were issued prior to a Release on
        // `begin_ticks`.
        let observed_begin_ticks = cs_state.begin_ticks.load(Ordering::Acquire);
        // See `HeartbeatLockupChecker::perform_check` for why `name` is loaded relaxed.
        let observed_name = cs_state.name.load(Ordering::Relaxed);
        // `worst_case_ticks` is a monotonic statistic that the Acquire above already orders behind
        // the critical section's stores, so a relaxed load suffices.
        let observed_worst_case_ticks = cs_state.worst_case_ticks.load(Ordering::Relaxed);

        // If observed_begin_ticks is non-zero, then the CPU we are checking is currently in a
        // critical section.  Compute how long it has been in the CS and check to see
        // if it exceeds any of our configured thresholds.
        if observed_begin_ticks > 0 {
            let age_ticks: zx_duration_boot_ticks_t =
                now_ticks.saturating_sub(observed_begin_ticks);

            // A shared closure used to report errors in a consistent fashion, either to
            // just stdout, or to the stdout_panic buffer in the case that this is a fatal
            // condition.
            let report_failure = |severity: FailureSeverity| {
                let output_target = if severity == FailureSeverity::Fatal {
                    STDOUT_PANIC_BUFFER.as_mut_ptr()
                } else {
                    stdout()
                };

                // See the comment in HeartbeatLockupChecker::perform_check for an explanation of
                // why this curious empty-string OOPS is here.
                kernel_oops!("");
                // SAFETY: `output_target` is one of the two `'static` `FILE`s, the format string's
                // conversions match the arguments, and `observed_name` is null or a `'static` C
                // string.
                unsafe {
                    fprintf(
                        output_target,
                        c"lockup_detector: CPU-%u in critical section for %ld ms, threshold=%ld \
                          ms start=%ld now=%ld name=%s.\n\
                          Reported by [CPU-%u] (message rate limited)\n"
                            .as_ptr(),
                        cpu,
                        ticks_to_duration(age_ticks) / zx_msec(1),
                        ticks_to_duration(observed_threshold_ticks) / zx_msec(1),
                        ticks_to_duration(observed_begin_ticks),
                        ticks_to_duration(now_ticks),
                        if observed_name.is_null() {
                            c"unknown".as_ptr()
                        } else {
                            observed_name.cast_const()
                        },
                        arch_curr_cpu_num(),
                    );
                }

                // SAFETY: As for the `dump_common_diagnostics` call in
                // `HeartbeatLockupChecker::perform_check`, with this argument for power: on
                // `aarch64`, `cpu` is powered up for a fatal report because `begin_ticks` is set
                // only while a timed critical section is open, `lockup_timed_begin`'s `# Safety`
                // keeps preemption or interrupts disabled on `cpu` until the matching
                // `lockup_timed_end`, and every path that powers a CPU down runs on another
                // thread. The operator who holds a section past the fatal threshold vouches that
                // nothing powers `cpu` down during the report.
                unsafe { dump_common_diagnostics(cpu, output_target, severity) };
            };

            // Check the fatal condition first.
            if Self::fatal_threshold_ticks() > 0
                && age_ticks >= Self::fatal_threshold_ticks()
                && FATAL_CONDITION_REPORTER_ROLE.acquire()
            {
                // Leave the other CPUs running so we can gather diagnostics.
                platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
                report_failure(FailureSeverity::Fatal);
                platform_halt(PlatformHaltAction::Reboot, ZirconCrashReason::SoftwareWatchdog);
                #[allow(unreachable_code)]
                FATAL_CONDITION_REPORTER_ROLE.release();
            }

            // Next, check to see if our "oops" threshold was exceeded.
            if observed_threshold_ticks > 0 && age_ticks >= observed_threshold_ticks {
                // Threshold exceeded.  Record this in the kcounters if this is the first time
                // we have seen this event, and then decide whether or not to print out an
                // oops based on our rate limiter.
                //
                // `last_counted_begin_ticks` is only accessed by the current checker, whose
                // exclusive role `current_checker_id` orders, so relaxed accesses suffice.
                if cs_state.last_counted_begin_ticks.load(Ordering::Relaxed) != observed_begin_ticks
                {
                    COUNTER_LOCKUP_CS_COUNT.add(1);
                    cs_state
                        .last_counted_begin_ticks
                        .store(observed_begin_ticks, Ordering::Relaxed);
                }

                if cs_state.ongoing_call_alert_limiter.ready() {
                    report_failure(FailureSeverity::Oops);
                }
            }
        }

        // Next check to see if we have a new worst case time spent in a critical
        // section to report.
        //
        // `reported_worst_case_ticks` is only accessed by the current checker, whose exclusive
        // role `current_checker_id` orders, so relaxed accesses suffice.
        if observed_worst_case_ticks > Self::worst_case_threshold_ticks()
            && observed_worst_case_ticks
                > cs_state.reported_worst_case_ticks.load(Ordering::Relaxed)
            && cs_state.worst_case_alert_limiter.ready()
        {
            // Remember the last worst case we reported, so we don't report it multiple
            // times.
            cs_state.reported_worst_case_ticks.store(observed_worst_case_ticks, Ordering::Relaxed);

            // Now go ahead and report the new worst case.
            let duration = ticks_to_duration(observed_worst_case_ticks);
            kprintln!(
                "lockup_detector: CPU-{} encountered a new worst case critical section \
                 time of {} usec. Reported by [CPU-{}] (message rate limited)",
                cpu,
                duration / zx_usec(1),
                arch_curr_cpu_num()
            );
        }
    }
}

/// Return an absolute deadline `duration` nanoseconds from now with positive jitter up to
/// `percent`% of `duration`.
fn deadline_with_jitter_after(duration: zx_duration_boot_t, percent: u32) -> Deadline {
    debug_assert!(percent <= 100);
    let max_delta = Ratio::new(percent, 100).scale::<{ Round::DOWN }>(duration);
    let delta = if max_delta > 0 {
        let mut seed = [0u8; 32];
        global_prng::draw(&mut seed);
        let mut rng = SmallRng::from_seed(seed);
        rng.random_range(0..=max_delta)
    } else {
        0
    };
    Deadline::after_boot(DurationBoot(duration.saturating_add(delta)), TimerSlack::none())
}

/// Record that the current CPU is still alive by having it update its last
/// heartbeat.  Then, check all of current CPU's peers to see if they have
/// tripped any of our low level lockup detectors. This currently consists of:
///
/// 1) The heartbeat detector (verifies that CPU timers are working)
/// 2) The critical section detector (verifies that no CPU spends too long in a
///    critical section of code, such as an SMC call).
///
/// # Safety
///
/// `timer` must be the current CPU's `lockup_detector_timer`, which is firing, and `arg` must be
/// the current CPU's `'static` [`LockupDetectorState`], as [`start_heartbeats`] passes them.
unsafe extern "C" fn do_heartbeat_and_check_peer_cpus(
    timer: *mut Timer,
    now_boot: zx_instant_boot_t,
    arg: *mut c_void,
) {
    let now_ticks = timer_current_boot_ticks().0;
    let current_cpu = arch_curr_cpu_num();

    // Record that we are still alive.
    // SAFETY: The caller passes the `'static` `LockupDetectorState` that `start_heartbeats`
    // scheduled this callback with.
    let checker_state = unsafe { &*arg.cast_const().cast::<LockupDetectorState>() };
    checker_state.heartbeat.last_heartbeat.store(now_boot, Ordering::SeqCst);

    // Pet the HW WDT, but only if we have a fatal heartbeat threshold
    // configured. We don't want the heartbeat checkers to be petting the dog if
    // they don't plan to reboot the system if things start to get really bad.
    if HeartbeatLockupChecker::fatal_threshold() > 0 {
        hw_watchdog_pet();
    }

    // Check whether a deadline thread is starving fair threads on this CPU.
    // SAFETY: This is the current CPU's heartbeat timer callback.
    unsafe { check_fair_starvation(current_cpu) };

    // Now, check each of the lockup conditions for each of our peers.
    for cpu in 0..(PerCpu::processor_count() as cpu_num_t) {
        if cpu == current_cpu || !is_cpu_online(cpu) || !scheduler::peek_is_active(cpu) {
            continue;
        }
        let state = LockupDetectorState::get(cpu);

        // Attempt to claim the role of the "checker" for this CPU.  If we fail to
        // do so, then another CPU is checking this CPU already, so we will just
        // skip our checks this time.  Note that this leaves a small gap in
        // detection ability.
        //
        // If the other checker has discovered no trouble and is just about to drop
        // the role of checker, but time has progressed to the point where a failure
        // would now be detected.  In this case, we would have reported the problem
        // had we been able to assume the checker role, but since it had not been
        // released yet, we will miss it.
        //
        // This gap is an acknowledged limitation.  Never stalling in these threads
        // is a more important property to maintain then having perfect gap free
        // coverage.  Presumably, some other core will check again in a short while
        // (or, we will do so ourselves next time around).
        //
        // TODO(johngro): either just replace this with a spin-try-lock, or spend
        // some time reviewing the memory order here.  CST seems like overkill, but
        // then again, checks are currently only performed once per second, so I
        // would rather be correct than fast for the time being.
        if state
            .current_checker_id
            .compare_exchange(INVALID_CPU, current_cpu, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // Now that we are the assigned "checker", perform the checks.  Start with
            // the CriticalSection check.  If there is a fatal condition to be
            // reported, we would rather start with the CriticalSection fatal
            // condition as it can provide more specific details about the lockup
            // than the heartbeat checker can.
            CriticalSectionLockupChecker::perform_check(state, cpu, now_ticks);
            HeartbeatLockupChecker::perform_check(state, cpu, now_boot);

            // Next, release our role as checker for this CPU.
            state.current_checker_id.store(INVALID_CPU, Ordering::SeqCst);
        }
    }

    // If heartbeats are still enabled for this core, schedule the next check.
    if checker_state.heartbeat.active.load(Ordering::SeqCst) {
        // SAFETY: `timer` is the current CPU's `lockup_detector_timer`, which lives in its
        // `'static` `PerCpu` and is only ever set from its own callback or with heartbeats
        // stopped, and `arg` is its `'static` `LockupDetectorState`.
        unsafe {
            core::pin::Pin::new_unchecked(&*timer).set_deadline(
                &Deadline::after_boot(
                    DurationBoot(HeartbeatLockupChecker::period()),
                    TimerSlack::none(),
                ),
                do_heartbeat_and_check_peer_cpus,
                arg,
            );
        }
    }
}

/// Reports a fair starvation episode on the current CPU, if the scheduler has one due. See
/// `kernel.lockup-detector.fair-starvation-threshold-ms`.
///
/// The scheduler tracks how long the CPU has run deadline threads while fair threads were queued
/// (see `Scheduler::poll_fair_starvation`).
///
/// # Safety
///
/// Must be called from the current CPU's heartbeat timer callback: interrupts are disabled, no
/// scheduler queue lock is held, no `ChainLockTransaction` is active, and the current thread is
/// the one the timer interrupted. The current CPU's `PerCpu` and the current thread must be
/// initialized.
unsafe fn check_fair_starvation(cpu: cpu_num_t) {
    // SAFETY: Interrupts are disabled and no queue lock is held, per the caller.
    if let Some(report) = unsafe { scheduler::Scheduler::poll_fair_starvation() } {
        // SAFETY: Forwarded from the caller.
        unsafe { report_fair_starvation(cpu, &report) };
    }
}

/// Reports the fair starvation episode `report` on the current CPU: prints the threads involved,
/// optionally raises a policy exception on the reported thread, and leaves the CPU in its relief
/// cool-down if one is configured.
///
/// This is kept out of line so that the heartbeat callback, which runs on the stack of whatever
/// thread the timer interrupted, does not carry the stack frame of a report that is rarely made.
///
/// # Safety
///
/// Must be called from the current CPU's heartbeat timer callback: interrupts are disabled, no
/// scheduler queue lock is held, no `ChainLockTransaction` is active, and the current thread is
/// the one the timer interrupted. The current CPU's `PerCpu` and the current thread must be
/// initialized.
#[cold]
#[inline(never)]
unsafe fn report_fair_starvation(cpu: cpu_num_t, report: &scheduler::FairStarvationReport) {
    // The scheduler only names a deadline thread that ran for more than half of the episode.
    // When several deadline threads share the CPU, as they do when it is overloaded, no one
    // thread is the cause.
    if report.culprit_tid != 0 {
        kprintln!(
            "lockup_detector: CPU-{}: fair threads starved for {} ms by deadline thread tid {} \
             ({} fair threads queued, running tid {})",
            cpu,
            report.starved_ns / zx_msec(1),
            report.culprit_tid,
            report.fair_queued,
            report.running_tid
        );
    } else {
        kprintln!(
            "lockup_detector: CPU-{}: fair threads starved for {} ms by deadline threads, none \
             of which ran for more than half of that time ({} fair threads queued, running tid \
             {})",
            cpu,
            report.starved_ns / zx_msec(1),
            report.fair_queued,
            report.running_tid
        );
    }
    // SAFETY: Interrupts are disabled, no queue lock is held, and the current CPU's `PerCpu` is
    // initialized, per the caller. `stdout()` is initialized during early boot and is not
    // changed during normal kernel operation; see `libc::stdio::stdout`.
    unsafe { scheduler::Scheduler::dump_fair_starvation() };
    if report.relief_ns != 0 {
        kprintln!(
            "lockup_detector: CPU-{}: fair threads get priority for a share of this CPU for {} ms",
            cpu,
            report.relief_ns / zx_msec(1)
        );
        // Relief is applied when this CPU next selects a thread. Request that now, instead of
        // waiting for the running deadline thread's time slice to end.
        preempt_set_pending();
    }

    if BootOptions::get().lockup_detector_fair_starvation_exception {
        // The current thread is the thread the timer interrupted, which is the running thread in
        // the report: this CPU cannot reschedule while interrupts are disabled. A thread can only
        // raise a policy exception on itself, so the reported thread gets one only if it is the
        // one running. The exception is delivered when the thread next returns to user mode.
        // The thread's fields are read through the raw pointer. No lock is held here, so a
        // reference to the whole `Thread` could alias references that other CPUs form under the
        // thread's lock.
        let current = current_get();
        // SAFETY: `current` is the valid, live running thread on this CPU.
        let (current_tid, current_signals) =
            unsafe { (Thread::tid_ptr(current), Thread::signals_ptr(current)) };
        debug_assert_eq!(current_tid, report.running_tid);
        if report.culprit_tid == 0 {
            kprintln!("lockup_detector: CPU-{}: no policy exception: no thread to report", cpu);
        } else if !report.running_is_user || report.running_tid != report.culprit_tid {
            kprintln!(
                "lockup_detector: CPU-{}: no policy exception: tid {} is not running or is not a \
                 user thread",
                cpu,
                report.culprit_tid
            );
        } else if current_signals & THREAD_SIGNAL_POLICY_EXCEPTION != 0 {
            // The timer interrupted the thread in a syscall that has a policy exception pending
            // for a violation. A thread has one pending policy exception at a time; keep the
            // violation, which this diagnostic must not replace.
            kprintln!(
                "lockup_detector: CPU-{}: no policy exception: tid {} has one pending",
                cpu,
                report.running_tid
            );
        } else {
            kprintln!(
                "lockup_detector: CPU-{}: raising a policy exception on tid {}",
                cpu,
                report.running_tid
            );
            signal_policy_exception(ZX_EXCP_POLICY_CODE_SPIN_DETECTION, cpu);
        }
    }
}

/// Stop the process of having the current CPU recording heartbeats and checking
/// in on other CPUs.
///
/// # Safety
///
/// The calling CPU's `PerCpu` must be initialized (`PerCpu::get_current()`), preemption or
/// interrupts must be disabled so the caller stays pinned to the current CPU throughout the call,
/// and [`lockup_init`] must have completed.
unsafe fn stop_heartbeats() {
    let state = LockupDetectorState::get(arch_curr_cpu_num());
    state.heartbeat.active.store(false, Ordering::SeqCst);
    PerCpu::get_current().lockup_detector_timer.cancel();
}

/// Start the process of recording heartbeats and checking in on other CPUs on
/// the current CPU.
///
/// # Safety
///
/// - [`lockup_init`] must have completed, and the global PRNG and platform timer must be
///   initialized.
/// - The calling CPU's `PerCpu` must be initialized (`PerCpu::get_current()`), and preemption or
///   interrupts must be disabled so the caller stays pinned to the current CPU throughout the
///   call.
/// - The calling CPU's `lockup_detector_timer` must not currently be scheduled (heartbeats were
///   never started on this CPU or were previously stopped with [`stop_heartbeats`]).
unsafe fn start_heartbeats() {
    if HeartbeatLockupChecker::period() <= 0 {
        // SAFETY: Forwarded from the caller.
        unsafe { stop_heartbeats() };
        return;
    }

    // To be safe, make sure we have a recent last heartbeat before activating.
    let state = LockupDetectorState::get(arch_curr_cpu_num());
    let hb_state = &state.heartbeat;
    let now = current_boot_time().0;
    hb_state.last_heartbeat.store(now, Ordering::SeqCst);
    hb_state.active.store(true, Ordering::SeqCst);

    // Use a deadline with some jitter to avoid having all CPUs heartbeat at the same time.
    let deadline = deadline_with_jitter_after(HeartbeatLockupChecker::period(), 10);
    // SAFETY: The caller guarantees this thread cannot migrate CPUs and `lockup_detector_timer`
    // in the current CPU's `'static` `PerCpu` is not already scheduled; `state` is `'static`.
    unsafe {
        core::pin::Pin::new_unchecked(&PerCpu::get_current().lockup_detector_timer).set_deadline(
            &deadline,
            do_heartbeat_and_check_peer_cpus,
            ptr::from_ref(state).cast_mut().cast::<c_void>(),
        );
    }
}

/// Initialize the lockup detector subsystem.
///
/// This should be called once on the boot CPU (`BOOT_CPU_ID`), before we've gone SMP, but after
/// the platform timer has been initialized since it needs to perform ticks to time conversion.
///
/// # Safety
///
/// The caller must ensure that this function is called once on the boot CPU before secondary CPUs
/// start or [`lockup_percpu_init`] is called, and after boot options and the platform timer have
/// been initialized.
pub unsafe fn lockup_init() {
    // Initialize parameters for the heartbeat checks.
    // SAFETY: Caller guarantees single-CPU boot context on `BOOT_CPU_ID` prior to any reader.
    unsafe { HeartbeatLockupChecker::init_static_params() };
    // The fair starvation check reports from the heartbeat, so it is enabled from here, and only
    // if heartbeats are enabled. A heartbeat period that does not fit in a duration disables
    // heartbeats too.
    if HeartbeatLockupChecker::period() > 0
        && let Some(threshold_ms) = scheduler::fair_starvation_init()
    {
        dprintf!(
            INFO,
            "lockup_detector: WARNING: kernel.lockup-detector.fair-starvation-threshold-ms is \
             shorter than the heartbeat period; using {} ms\n",
            threshold_ms
        );
    }

    dprintf!(
        INFO,
        "lockup_detector: heartbeats {}, period {} ms, threshold {} ms, fatal threshold {} ms, \
         diags dump timeout {} ms\n",
        if HeartbeatLockupChecker::period() > 0 { "enabled" } else { "disabled" },
        HeartbeatLockupChecker::period() / zx_msec(1),
        HeartbeatLockupChecker::threshold() / zx_msec(1),
        HeartbeatLockupChecker::fatal_threshold() / zx_msec(1),
        BootOptions::get().lockup_detector_diagnostic_query_timeout_ms
    );

    // Initialize parameters for the critical section checks, but only if the
    // heartbeat mechanism is enabled.  If the heartbeat mechanism is disabled, no
    // checks will ever be performed.
    //
    // TODO(johngro): relax this.  There is no strong reason to not do our
    // periodic checking if any of the check conditions are enabled.
    if LOCKUP_CRITICAL_SECTION_ENABLED {
        if HeartbeatLockupChecker::period() > 0 {
            // SAFETY: Caller guarantees single-CPU boot context on `BOOT_CPU_ID` prior to any
            // reader.
            unsafe { CriticalSectionLockupChecker::init_static_params() };

            if CriticalSectionLockupChecker::is_enabled() {
                dprintf!(
                    INFO,
                    "lockup_detector: critical section threshold {} ms, fatal threshold {} ms\n",
                    ticks_to_duration(CriticalSectionLockupChecker::threshold_ticks()) / zx_msec(1),
                    ticks_to_duration(CriticalSectionLockupChecker::fatal_threshold_ticks())
                        / zx_msec(1)
                );
            } else {
                dprintf!(
                    INFO,
                    "lockup_detector: critical section detection disabled by threshold\n"
                );
            }
        } else {
            // SAFETY: Caller guarantees single-CPU boot context on `BOOT_CPU_ID` prior to any
            // reader.
            unsafe { CriticalSectionLockupChecker::init_disabled() };
            dprintf!(
                INFO,
                "lockup_detector: critical section detection disabled because heartbeats are \
                 disabled\n"
            );
        }
    } else {
        // SAFETY: Caller guarantees single-CPU boot context on `BOOT_CPU_ID` prior to any reader.
        unsafe { CriticalSectionLockupChecker::init_disabled() };
        dprintf!(INFO, "lockup_detector: critical section detection disabled by build\n");
    }
}

/// Initialize the lockup detector for the calling CPU.
///
/// This should be called once on each CPU after the platform timer has been initialized and after
/// the boot CPU has called [`lockup_init`].
///
/// # Safety
///
/// The caller must ensure that [`lockup_init`], the platform timer, the global PRNG, and the
/// calling CPU's `PerCpu` have been initialized, that preemption or interrupts are disabled so the
/// caller stays pinned to the current CPU throughout the call, and that heartbeats are not already
/// active on the calling CPU.
pub unsafe fn lockup_percpu_init() {
    // SAFETY: Forwarded from the caller.
    unsafe { start_heartbeats() };
}

/// Shutdown the lockup detector for the calling CPU.
///
/// This should be called once on each CPU prior to suspending or taking it offline.
///
/// # Safety
///
/// The caller must ensure that [`lockup_init`] and the calling CPU's `PerCpu` have been
/// initialized, and that preemption or interrupts are disabled so the caller stays pinned to the
/// current CPU throughout the call.
pub unsafe fn lockup_percpu_shutdown() {
    // SAFETY: Forwarded from the caller.
    unsafe { stop_heartbeats() };
}

/// Same as [`lockup_begin`] except the critical section is timed.
///
/// # Safety
///
/// - Preemption or interrupts must be disabled on the calling CPU and remain disabled until the
///   matching [`lockup_timed_end`] call on the same CPU.
/// - Any interrupt handler on the same CPU that enters a critical section while preemption is
///   disabled must balance its own `begin`/`end` calls before returning.
#[cfg(any(ktest, console_enabled))]
#[inline]
unsafe fn lockup_timed_begin(name: &'static CStr) {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }

    // SAFETY: `arch_curr_cpu_num()` is always `< SMP_MAX_CPUS`.
    let state = unsafe { LockupDetectorState::get_unchecked(arch_curr_cpu_num()) };
    let outermost = enter(state);
    if outermost {
        let cs_state = &state.critical_section;
        // We're using Ordering::Relaxed instead of Ordering::Release to
        // minimize performance impact.  As a result, HeartbeatLockupChecker may see
        // stale name values because there is nothing for it to synchronize-with.
        // However, if CriticalSectionLockupChecker is enabled, then the begin_ticks
        // store with release semantics will ensure the CriticalSectionLockupChecker
        // sees the latest value.
        cs_state.name.store(name.as_ptr().cast_mut(), Ordering::Relaxed);
        if CriticalSectionLockupChecker::is_enabled() {
            let now = timer_current_boot_ticks().0;
            // Use release semantics to ensure that if an observer sees this store to `begin_ticks`,
            // they will also see the stores that preceded it.
            cs_state.begin_ticks.store(now, Ordering::Release);
        }
    }
}

/// Same as [`lockup_end`] except the critical section is timed.
///
/// # Safety
///
/// Preemption or interrupts must be disabled on the calling CPU, and the call must balance an
/// earlier call to [`lockup_timed_begin`] on the same CPU.
#[cfg(any(ktest, console_enabled))]
#[inline]
unsafe fn lockup_timed_end() {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }

    // SAFETY: `arch_curr_cpu_num()` is always `< SMP_MAX_CPUS`.
    let state = unsafe { LockupDetectorState::get_unchecked(arch_curr_cpu_num()) };
    call_if_outer_and_leave(state, |state| {
        let cs_state = &state.critical_section;
        // Only this CPU writes `begin_ticks` and `worst_case_ticks`, so relaxed loads see its own
        // stores; observers synchronize with the Release store of `begin_ticks` below.
        let begin = cs_state.begin_ticks.load(Ordering::Relaxed);
        if begin > 0 {
            // Is this a new worst for us?
            let now_ticks = timer_current_boot_ticks().0;
            let delta: zx_duration_boot_ticks_t = now_ticks.saturating_sub(begin);

            // Update our counters.
            CriticalSectionLockupChecker::record_critical_section_bucket_counters(delta);

            if delta > cs_state.worst_case_ticks.load(Ordering::Relaxed) {
                core::hint::cold_path();
                cs_state.worst_case_ticks.store(delta, Ordering::Relaxed);
            }
        }

        // See comment in lockup_timed_begin at the point where name is stored.
        cs_state.name.store(ptr::null_mut(), Ordering::Relaxed);

        // We are done with the CS now.  Clear the begin time to indicate that we are not in any
        // critical section.
        //
        // Use release semantics to ensure that if an observer sees this store to `begin_ticks`,
        // they will also see any of our previous stores.
        cs_state.begin_ticks.store(0, Ordering::Release);
    });
}

/// Returns the number of times a "critical section threshold exceeded" oops was triggered.
pub fn lockup_get_critical_section_oops_count() -> i64 {
    COUNTER_LOCKUP_CS_COUNT.sum_across_all_cpus()
}

/// Returns the number of times a "no heartbeat" oops was triggered.
pub fn lockup_get_no_heartbeat_oops_count() -> i64 {
    COUNTER_LOCKUP_NO_HEARTBEAT_OOPS.sum_across_all_cpus()
}

#[cfg(console_enabled)]
mod console {
    use super::{
        CriticalSectionLockupChecker, HeartbeatLockupChecker, LockupDetectorState,
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

                let cs_state = &LockupDetectorState::get(i).critical_section;
                let begin_ticks: zx_instant_boot_ticks_t =
                    cs_state.begin_ticks.load(Ordering::Acquire);
                let name: *const c_char = cs_state.name.load(Ordering::Relaxed);
                let now: zx_instant_boot_ticks_t = timer_current_boot_ticks().0;
                let worst_case_usec: i64 =
                    ticks_to_duration(cs_state.worst_case_ticks.load(Ordering::Relaxed))
                        / zx_usec(1);
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

            let hb_state = &LockupDetectorState::get(cpu).heartbeat;
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
            t.join(InstantMono(ZX_TIME_INFINITE)).expect("failed to join lockup-test thread");
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
            // SAFETY: `arg` points to `duration` on `run_lockup_func`'s stack, which remains valid
            // until the thread is joined.
            let duration: zx_duration_boot_t = unsafe { *(arg as *const zx_duration_boot_t) };
            let _preempt_disable = AutoPreemptDisabler::new();
            // SAFETY: Preemption is disabled by `_preempt_disable` and remains disabled until
            // `lockup_timed_end` below.
            unsafe { lockup_timed_begin(c"trigger-tool") };
            let deadline: zx_instant_boot_t = current_boot_time().0.saturating_add(duration);
            while current_boot_time().0 < deadline {
                libarch::r#yield();
            }
            // SAFETY: Preemption is disabled by `_preempt_disable`; balances `lockup_timed_begin`
            // above.
            unsafe { lockup_timed_end() };
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
pub fn current_critical_section_depth_and_name() -> (u32, *const core::ffi::c_char) {
    assert!(crate::arch_rs::ints_disabled());
    let cs_state = &LockupDetectorState::get(arch_curr_cpu_num()).critical_section;
    (cs_state.depth.load(Ordering::Relaxed), cs_state.name.load(Ordering::Relaxed).cast_const())
}
