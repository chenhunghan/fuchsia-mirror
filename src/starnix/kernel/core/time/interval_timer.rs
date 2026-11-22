// Copyright 2023 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::power::OnWakeOps;
use crate::signals::{
    SignalDetail, SignalEvent, SignalEventNotify, SignalEventValue, SignalInfo, send_signal,
};
use crate::task::{CurrentTask, Kernel, ThreadGroup};
use crate::time::utc::{estimate_boot_deadline_from_utc, utc_now};
use crate::time::{
    GenericDuration, HrTimer, HrTimerHandle, TargetTime, Timeline, TimerId, TimerWakeup,
};
use crate::vfs::timer::TimerOps;
use assert_matches::assert_matches;
use fuchsia_runtime::UtcInstant;
use futures::channel::mpsc;
use futures::stream::AbortHandle;
use futures::{FutureExt, StreamExt, select};
use starnix_logging::{log_debug, log_error, log_trace, log_warn, track_stub};
use starnix_sync::{IntervalTimerState, LockDepMutex};
use starnix_types::time::{duration_from_timespec, timespec_from_duration};
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::SIGALRM;
use starnix_uapi::{SI_TIMER, itimerspec};
use std::fmt::Debug;
use std::pin::pin;
use std::sync::{Arc, Weak};

#[derive(Default)]
pub struct TimerRemaining {
    /// Remaining time until the next expiration.
    pub remainder: zx::SyntheticDuration,
    /// Interval for periodic timer.
    pub interval: zx::SyntheticDuration,
}

impl From<TimerRemaining> for itimerspec {
    fn from(value: TimerRemaining) -> Self {
        Self {
            it_interval: timespec_from_duration(value.interval),
            it_value: timespec_from_duration(value.remainder),
        }
    }
}

#[derive(Debug)]
pub struct IntervalTimer {
    pub timer_id: TimerId,

    /// Whether this timer implements `ITIMER_REAL` (`setitimer` / `alarm`), which dispatches
    /// `SI_KERNEL` `SIGALRM` signals rather than POSIX `SI_TIMER` signals.
    is_itimer_real: bool,

    /// HrTimer to trigger wakeup
    hr_timer: Option<HrTimerHandle>,

    timeline: Timeline,

    pub signal_event: SignalEvent,

    state: LockDepMutex<IntervalTimerMutableState, IntervalTimerState>,
}
pub type IntervalTimerHandle = Arc<IntervalTimer>;

/// Emulates waiting on the UTC timeline for the interval timer.
///
/// Combines two functionalities offered by the Fuchsia runtime's timer
/// and the wake alarm:
/// * Fuchsia's runtime timer can wake process after a wait,
/// * Wake alarm can wake the system after a period expires.
///
/// The UtcWaiter combines the two to ensure that once [UtcWaiter.wait()]
/// returns, the correct amount of UTC (wall clock) time has expired.
#[derive(Debug)]
struct UtcWaiter {
    // Used in `on_wake` below.
    send: mpsc::UnboundedSender<()>,
    // Call to obtain the current UtcInstant. Injected in tests.
    utc_now_fn: fn() -> UtcInstant,
}

impl OnWakeOps for UtcWaiter {
    // This fn is called when a wake alarm expires. [UtcWaiter] must be
    // submitted to HrTimerManager for that to happen.
    fn on_wake(&self, _: &CurrentTask, _: &zx::NullableHandle) {
        self.on_wake_internal()
    }
}

impl UtcWaiter {
    /// Creates a new UtcWaiter.
    ///
    /// Await on `UtcWaiter::wait` to pass the time.
    ///
    /// # Returns
    /// A pair of:
    /// * `Self`: the waiter itself. Call `UtcWaiter::wait` on it.
    /// * `UnboundedSender<_>`: a channel used for async notification from the
    /// wake alarm subsystem.  Feed it into `UtcWaiter::wait`.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        Self::new_internal(utc_now)
    }

    fn on_wake_internal(&self) {
        self.send
            .unbounded_send(())
            // This is a common occurrence if the timer has been destroyed
            // before we get to `unbounded_send` for it.
            .inspect_err(|err| log_warn!("UtcWaiter::on_wake: {err:?}"))
            .unwrap_or(());
    }

    fn new_internal(clock_fn: fn() -> UtcInstant) -> (Self, mpsc::UnboundedReceiver<()>) {
        let (send, recv) = mpsc::unbounded();
        // Returning recv instead of adding to Self avoids the need to take a Mutex.
        (Self { send, utc_now_fn: clock_fn }, recv)
    }

    /// Awaits until `deadline` expires. Get `utc_signal` from a call to `UtcWaiter::new()`.
    pub async fn wait(&self, deadline: UtcInstant, mut utc_signal: mpsc::UnboundedReceiver<()>) {
        loop {
            let mut utc_wait_fut = utc_signal.next().fuse();
            let (deadline_boot, _) = estimate_boot_deadline_from_utc(deadline);
            let mut boot_wait_fut = pin!(fuchsia_async::Timer::new(deadline_boot));
            log_debug!(
                "UtcWaiter::wait: waiting for: deadline_utc={:?}, deadline_boot={:?}",
                deadline,
                deadline_boot
            );

            // The UTC deadline can move. Initially, the UTC and the boot deadline
            // are in sync. But if after starting the wait the UTC timeline changes,
            // the actual UTC deadline may move to be sooner or later than the boot
            // deadline, meaning that we can wake either earlier or later than
            // the user requested.
            //
            // If we woke up correctly, we return. If we woke up too early, we
            // recompute and continue waiting.
            select! {
                // While nominally the waits on the boot and UTC timelines will trigger at about
                // the same wall clock instant absent timeline changes and wakes, the boot timer
                // has a much finer resolution than the UTC timer. For example, some devices
                // support at most 1 second resolution for UTC wakes, vs effectively millisecond
                // resolution on boot timers or even finer.
                //
                // So if we have an opportunity to be more accurate by waking on boot timer, we
                // should probably take it.
                _ = boot_wait_fut => {
                    log_debug!("UtcWaiter::wait: woken by boot deadline.");
                },
                // The UTC timer has an additional property that it is able to wake the device
                // from sleep. As a tradeoff, it usually has orders of magnitude coarser resolution
                // than the boot timer. So, we also wait on UTC to get the wake functionality.
                _ = utc_wait_fut => {
                    log_debug!("UtcWaiter::wait: woken by UTC deadline.");
                },
            }
            let utc_now = (self.utc_now_fn)();
            if deadline <= utc_now {
                log_debug!(
                    "UtcWaiter::wait: UTC deadline reached: now={:?}, deadline={:?}",
                    utc_now,
                    deadline
                );
                break;
            } else {
                log_debug!(
                    "UtcWaiter::wait: UTC deadline NOT reached: now={:?}, deadline={:?}",
                    utc_now,
                    deadline
                );
            }
        }
    }
}

#[derive(Debug)]
struct IntervalTimerMutableState {
    /// Handle to abort the running timer task.
    abort_handle: Option<AbortHandle>,
    /// True if the timer is currently armed.
    armed: bool,
    /// Time of the next expiration on the requested timeline.
    target_time: TargetTime,
    /// Interval for periodic timer.
    interval: zx::SyntheticDuration,
    /// Monotonically increasing generation incremented whenever timer settings change.
    generation: u64,
    /// Number of timer expirations accumulated for the currently queued signal.
    ///
    /// 0 indicates no signal is queued (or settings were reset since queueing).
    /// 1 indicates a queued signal with 0 overruns.
    /// Values > 1 indicate a queued signal with `expiry_count - 1` overruns.
    expiry_count: u32,
    /// Overrun count latched when the most recent timer signal was dequeued.
    overrun_last: i32,
}

impl IntervalTimerMutableState {
    fn disarm(&mut self) {
        self.armed = false;
        if let Some(abort_handle) = &self.abort_handle {
            abort_handle.abort();
        }
        self.abort_handle = None;
    }

    fn on_setting_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.expiry_count = 0;
        self.overrun_last = 0;
    }
}

impl IntervalTimer {
    pub fn new(
        timer_id: TimerId,
        timeline: Timeline,
        wakeup_type: TimerWakeup,
        signal_event: SignalEvent,
    ) -> Result<IntervalTimerHandle, Errno> {
        // TODO(b/470129973): We may also need to add hr_timer for regular wakeups on the real time
        // timeline, to track UTC timeline changes.
        let hr_timer = match wakeup_type {
            TimerWakeup::Regular => None,
            TimerWakeup::Alarm => Some(HrTimer::new()),
        };
        Ok(Arc::new(Self {
            timer_id,
            is_itimer_real: false,
            hr_timer,
            timeline,
            signal_event,
            state: IntervalTimerMutableState {
                abort_handle: None,
                armed: false,
                target_time: timeline.zero_time(),
                interval: zx::SyntheticDuration::default(),
                generation: 0,
                expiry_count: 0,
                overrun_last: 0,
            }
            .into(),
        }))
    }

    pub fn new_itimer_real() -> IntervalTimerHandle {
        let signal_event =
            SignalEvent::new(SignalEventValue(0), SIGALRM, SignalEventNotify::Signal);
        let mut timer = Self::new(0, Timeline::Monotonic, TimerWakeup::Regular, signal_event)
            .expect("Failed to create itimer_real");
        Arc::get_mut(&mut timer).expect("unique IntervalTimerHandle").is_itimer_real = true;
        timer
    }

    fn signal_info(
        self: &IntervalTimerHandle,
        generation: u64,
        expirations: u32,
    ) -> Option<SignalInfo> {
        let signo = self.signal_event.signo?;
        if self.is_itimer_real {
            return Some(SignalInfo::kernel(signo));
        }
        let signal_detail =
            SignalDetail::Timer { timer: self.clone(), generation, expirations, overrun: None };
        Some(SignalInfo::with_detail(signo, SI_TIMER, signal_detail))
    }

    async fn start_timer_loop(
        self: &IntervalTimerHandle,
        kernel: &Kernel,
        timer_thread_group: Weak<ThreadGroup>,
        generation: u64,
    ) {
        loop {
            let overtime = loop {
                // We may have to issue multiple sleeps if the target time in the timer is
                // updated while we are sleeping or if our estimation of the target time
                // relative to the monotonic clock is off. Drop the guard before blocking so
                // that the target time can be updated.
                let target_time = {
                    let guard = self.state.lock();
                    if !guard.armed || guard.generation != generation {
                        return;
                    }
                    guard.target_time
                };
                let now = self.timeline.now();
                if now >= target_time {
                    break now
                        .delta(&target_time)
                        .expect("timer timeline and target time are comparable");
                }
                let (utc_waiter, utc_signal) = UtcWaiter::new();
                let utc_waiter = Arc::new(utc_waiter);
                if let Some(hr_timer) = &self.hr_timer {
                    assert_matches!(
                        target_time,
                        TargetTime::BootInstant(_) | TargetTime::RealTime(_),
                        "monotonic times can't be alarm deadlines",
                    );
                    let weak_utc_waiter = Arc::downgrade(&utc_waiter);
                    if let Err(e) = hr_timer.start(
                        kernel.kthreads.system_task(),
                        Some(weak_utc_waiter),
                        target_time,
                    ) {
                        log_error!("Failed to start the HrTimer to trigger wakeup: {e}");
                    }
                }

                match target_time {
                    TargetTime::Monotonic(t) => fuchsia_async::Timer::new(t).await,
                    TargetTime::BootInstant(t) => fuchsia_async::Timer::new(t).await,
                    TargetTime::RealTime(t) => utc_waiter.wait(t, utc_signal).await,
                }
            };
            let expirations = {
                let guard = self.state.lock();
                if !guard.armed || guard.generation != generation {
                    return;
                }
                if guard.interval == zx::SyntheticDuration::ZERO {
                    1
                } else {
                    u32::try_from(
                        (overtime.into_nanos() / guard.interval.into_nanos()).saturating_add(1),
                    )
                    .unwrap_or(u32::MAX)
                }
            };

            // Check on notify enum to determine the signal target.
            if let Some(timer_thread_group) = timer_thread_group.upgrade() {
                match self.signal_event.notify {
                    SignalEventNotify::Signal => {
                        if let Some(signal_info) = self.signal_info(generation, expirations) {
                            log_trace!(
                                signal = signal_info.signal.number(),
                                pid = timer_thread_group.leader.id;
                                "sending signal for timer"
                            );
                            timer_thread_group.write().send_signal(signal_info);
                        }
                    }
                    SignalEventNotify::None => {}
                    SignalEventNotify::Thread { .. } => {
                        track_stub!(TODO("https://fxbug.dev/322875029"), "SIGEV_THREAD timer");
                    }
                    SignalEventNotify::ThreadId(tid) => {
                        // Drop the `ThreadGroupReadGuard` before calling `send_signal`, because
                        // `SIGKILL` delivery acquires `ThreadGroupMutableStateLock` for write.
                        let target = timer_thread_group.read().get_task(tid);
                        if let Some(target) = target {
                            if let Some(signal_info) = self.signal_info(generation, expirations) {
                                log_trace!(
                                    signal = signal_info.signal.number(),
                                    tid;
                                    "sending signal for timer"
                                );
                                send_signal(&target, signal_info).unwrap_or_else(|e| {
                                    log_warn!("Failed to queue timer signal: {}", e)
                                });
                            }
                        }
                    }
                }
            }

            // If the `interval` is zero, the timer expires just once, at the time
            // specified by `target_time`.
            let mut guard = self.state.lock();
            if !guard.armed || guard.generation != generation {
                return;
            }
            if guard.interval != zx::SyntheticDuration::default() {
                guard.target_time = self.timeline.now() + GenericDuration::from(guard.interval);
            } else {
                guard.disarm();
                return;
            }
        }
    }

    /// Returns whether a queued timer signal generated under `generation` is still valid.
    pub(crate) fn is_signal_valid(&self, generation: u64) -> bool {
        self.state.lock().generation == generation
    }

    /// Records `expirations` for a timer signal generated under `generation`, returning `false`
    /// if the timer's settings have changed since generation.
    pub(crate) fn on_signal_queued(&self, generation: u64, expirations: u32) -> bool {
        let mut guard = self.state.lock();
        if guard.generation != generation {
            return false;
        }
        guard.expiry_count = guard.expiry_count.saturating_add(expirations);
        true
    }

    /// Latches and returns the overrun count when a queued timer signal of `generation` is
    /// dequeued, resetting the pending expiry counter. Returns `None` if `generation` is no
    /// longer current, indicating the signal is stale and must not be delivered.
    pub(crate) fn on_signal_delivered(&self, generation: u64) -> Option<i32> {
        let mut guard = self.state.lock();
        if guard.generation != generation {
            return None;
        }
        let overrun = i32::try_from(guard.expiry_count.saturating_sub(1)).unwrap_or(i32::MAX);
        guard.overrun_last = overrun;
        guard.expiry_count = 0;
        Some(overrun)
    }

    /// Resets the pending expiry counter if a queued timer signal of `generation` is discarded
    /// without delivery while `generation` is still current.
    pub(crate) fn on_signal_discarded(&self, generation: u64) {
        let mut guard = self.state.lock();
        if guard.generation == generation {
            guard.expiry_count = 0;
        }
    }

    pub fn arm(
        self: &IntervalTimerHandle,
        current_task: &CurrentTask,
        new_value: itimerspec,
        is_absolute: bool,
    ) -> Result<(), Errno> {
        let mut guard = self.state.lock();

        let target_time = if is_absolute {
            self.timeline.target_from_timespec(new_value.it_value)?
        } else {
            self.timeline.now()
                + GenericDuration::from(duration_from_timespec::<zx::SyntheticTimeline>(
                    new_value.it_value,
                )?)
        };
        let interval = duration_from_timespec(new_value.it_interval)?;

        // Stop the current running task and reset overrun state for the new setting.
        guard.disarm();
        guard.on_setting_changed();

        guard.interval = interval;
        if let Some(hr_timer) = &self.hr_timer {
            // It is important for power management that the hrtimer is marked as interval, as
            // interval timers may prohibit container suspension.  Note that marking `is_interval`
            // changes the hrtimer ID, which is only allowed if the hrtimer is not running.
            *hr_timer.is_interval.lock() = guard.interval != zx::SyntheticDuration::default();
        }

        if target_time.is_zero() {
            return Ok(());
        }

        guard.armed = true;
        guard.target_time = target_time;
        let generation = guard.generation;

        let kernel_ref = current_task.kernel().clone();
        let self_ref = self.clone();
        let thread_group = current_task.thread_group().weak_self.clone();
        current_task.kernel().kthreads.spawn_future(
            move || async move {
                let _ = {
                    // 1. Lock the state to update `abort_handle` when the timer is still armed.
                    // 2. MutexGuard needs to be dropped before calling await on the future task.
                    // Unfortunately, std::mem::drop is not working correctly on this:
                    // (https://github.com/rust-lang/rust/issues/57478).
                    let mut guard = self_ref.state.lock();
                    if !guard.armed || guard.generation != generation {
                        return;
                    }

                    let (abortable_future, abort_handle) = futures::future::abortable(
                        self_ref.start_timer_loop(&kernel_ref, thread_group, generation),
                    );
                    guard.abort_handle = Some(abort_handle);
                    abortable_future
                }
                .await;
            },
            "interval_timer_loop",
        );

        Ok(())
    }

    pub fn disarm(&self, current_task: &CurrentTask) -> Result<(), Errno> {
        let mut guard = self.state.lock();
        guard.disarm();
        guard.on_setting_changed();
        if let Some(hr_timer) = &self.hr_timer {
            hr_timer.stop(current_task.kernel())?;
        }
        Ok(())
    }

    pub fn time_remaining(&self) -> TimerRemaining {
        let guard = self.state.lock();
        if !guard.armed {
            return TimerRemaining::default();
        }

        TimerRemaining {
            remainder: std::cmp::max(
                zx::SyntheticDuration::ZERO,
                *guard.target_time.delta(&self.timeline.now()).expect("timelines must match"),
            ),
            interval: guard.interval,
        }
    }

    pub fn overrun_last(&self) -> i32 {
        self.state.lock().overrun_last
    }
}

impl PartialEq for IntervalTimer {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for IntervalTimer {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::utc::UtcClockOverrideGuard;
    use assert_matches::assert_matches;
    use fuchsia_async as fasync;
    use fuchsia_runtime as fxr;
    use std::task::Poll;

    struct TestContext {
        _initial_time_mono: zx::MonotonicInstant,
        initial_time_utc: UtcInstant,
        _utc_clock: fxr::UtcClock,
        _guard: UtcClockOverrideGuard,
    }

    impl TestContext {
        async fn new() -> Self {
            // Make them the same initially.
            let _initial_time_mono = zx::MonotonicInstant::from_nanos(1000);
            let initial_time_utc = UtcInstant::from_nanos(_initial_time_mono.into_nanos());
            fasync::TestExecutor::advance_to(_initial_time_mono.into()).await;

            // Create and start the UTC clock.
            let utc_clock =
                fxr::UtcClock::create(zx::ClockOpts::empty(), Some(initial_time_utc)).unwrap();
            let utc_clock_clone = utc_clock.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
            let initial_time_boot = zx::BootInstant::from_nanos(_initial_time_mono.into_nanos());
            utc_clock
                .update(
                    fxr::UtcClockUpdate::builder()
                        .absolute_value(initial_time_boot, initial_time_utc)
                        .build(),
                )
                .unwrap();

            // Inject the clock into Starnix infra.
            let _guard = UtcClockOverrideGuard::new(utc_clock_clone);

            Self { _initial_time_mono, initial_time_utc, _utc_clock: utc_clock, _guard }
        }
    }

    // If the UTC signal is received and the wait expired, we are done.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_utc_waiter_on_utc_expired() {
        let _context = TestContext::new().await;

        let (waiter, utc_signal) = UtcWaiter::new();
        // Expired deadline, and notification.
        waiter.on_wake_internal();
        let deadline_utc = _context.initial_time_utc - fxr::UtcDuration::from_nanos(10);
        let wait_fut = pin!(waiter.wait(deadline_utc, utc_signal));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(wait_fut).await,
            Poll::Ready(_),
            "UTC deadline should have expired"
        );
    }

    // If the UTC signal is received, but the deadline is not reached, we
    // must still pend.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_utc_waiter_on_utc_still_pending() {
        let context = TestContext::new().await;

        let (waiter, utc_signal) =
            UtcWaiter::new_internal(|| -> fxr::UtcInstant { fxr::UtcInstant::from_nanos(2000) });
        // Notified, but not expired yet.
        waiter.on_wake_internal();
        let deadline_utc = context.initial_time_utc + fxr::UtcDuration::INFINITE;

        let wait_fut = pin!(waiter.wait(deadline_utc, utc_signal));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(wait_fut).await,
            Poll::Pending,
            "UTC deadline should not have expired"
        );
    }

    // If we are woken by the timer, and UTC deadline has passed, we are done.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_utc_waiter_on_boot_expires() {
        let context = TestContext::new().await;

        let (waiter, utc_signal) =
            UtcWaiter::new_internal(|| -> fxr::UtcInstant { fxr::UtcInstant::from_nanos(5000) });
        let deadline_utc = context.initial_time_utc + fxr::UtcDuration::from_nanos(4000);
        let wait_fut = pin!(waiter.wait(deadline_utc, utc_signal));

        fasync::TestExecutor::advance_to(zx::MonotonicInstant::from_nanos(10000).into()).await;
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(wait_fut).await,
            Poll::Ready(_),
            "UTC deadline should have expired, and we got notified via the timer wait"
        );
    }
}
