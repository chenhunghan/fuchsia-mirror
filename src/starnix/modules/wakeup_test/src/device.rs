// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input::{
    create_media_buttons_proxy, create_touchscreen_proxy, schedule_wakeup_power_button,
    simulate_swipe, simulate_tap,
};
use crate::ioctl::{
    CommandCode, MAX_POWER_BUTTON_EVENTS, MAX_WAKEUP_EVENTS, WakeupMethod, WakeupTestType,
    WakeupTimerInfo,
};
use crate::tracing;
use anyhow::{Result, anyhow};
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_math as fmath;
use fidl_fuchsia_ui_test_input as futinput;
use fuchsia_async::{BootInstant as AsyncBootInstant, MonotonicInstant, TimeoutExt, Timer};
use fuchsia_component::client::connect_to_protocol;
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::stream::FuturesUnordered;
use starnix_core::device::DeviceOps;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::perf::TraceEventQueueList;
use starnix_core::task::{CurrentTask, Kernel, Waiter};
use starnix_core::vfs::{CloseFreeSafe, FileObject, FileOps, NamespaceNode};
use starnix_core::{fileops_impl_dataless, fileops_impl_noop_sync, fileops_impl_seekless};
use starnix_logging::{log_error, log_info};
use starnix_sync::{LockDepMutex, WakeupTestStateLock};
use starnix_syscalls::{SUCCESS, SyscallResult};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{device_id, errno, error};
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, OnceLock, Weak};

const MIN_SCREEN_DIMENSION: u32 = 2;
const DEFAULT_SCREEN_MAX_X: i64 = 1000;
const DEFAULT_SCREEN_MAX_Y: i64 = 1000;
const DEFAULT_TAP_HOLD_DURATION_MS: i64 = 15;
const DEFAULT_SWIPE_DURATION_MS: i64 = 80;
const SWIPE_MS_PER_MOVE_EVENT: i64 = 8;
const MIN_SWIPE_MOVE_EVENTS: u32 = 2;
const MAX_SWIPE_MOVE_EVENTS: u32 = 60;
const SWIPE_NEAR_FRACTION_NUM: i64 = 1;
const SWIPE_FAR_FRACTION_NUM: i64 = 4;
const SWIPE_FRACTION_DENOM: i64 = 5;
const ALTERNATE_TAP_OFFSET_X: i64 = 2;
const SETUP_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);
const POWER_BUTTON_KEEPALIVE_DURATION: zx::BootDuration = zx::BootDuration::from_seconds(5);
const INPUT_TRACE_CATEGORY: &str = "input";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PowerScheduleAnchor {
    target_boot: zx::BootInstant,
    offset_ns: i64,
}

#[derive(Debug, Clone)]
struct CachedTouchScreen {
    max_x: i64,
    max_y: i64,
    proxy: futinput::TouchScreenProxy,
}

struct TouchScheduleRequest {
    plan: ValidatedTimerPlan,
    touch_action: TouchAction,
    matched_power_anchor: Option<(PowerScheduleAnchor, i64)>,
    registry_client: RegistryClient,
    setup_deadline: MonotonicInstant,
    setup_notifier: SetupNotifier,
    caller_cancelled_rx: oneshot::Receiver<()>,
}

enum TouchWorkerCommand {
    Schedule(TouchScheduleRequest),
    #[cfg(test)]
    HasOpenCachedTouchScreen(oneshot::Sender<bool>),
}

/// Shared state stored inside [`WakeupTestDevice`].
///
/// Intentionally holds only pure Rust memory (`UnboundedSender`, timestamps) and never stores
/// FIDL proxies (`fidl::AsyncChannel`), because Starnix frees `FileObject` asynchronously on the
/// background `starnix-rcu` thread; any executor-bound `PacketReceiver` inside `WakeupTestDevice`
/// could outlive a test's `LocalExecutor` on teardown. Instead, `CachedTouchScreen` is owned on
/// the stack of `run_touch_worker` inside `kernel.kthreads` (`root_scope`).
#[derive(Debug, Default)]
struct WakeupTestState {
    last_power_schedule: Option<PowerScheduleAnchor>,
    touch_worker_tx: Option<mpsc::UnboundedSender<TouchWorkerCommand>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SwipeDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TouchAction {
    Tap,
    Swipe(SwipeDirection),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum PlanAction {
    PowerButton,
    Touch(TouchAction),
}

impl From<WakeupMethod> for PlanAction {
    fn from(method: WakeupMethod) -> Self {
        match method {
            WakeupMethod::PowerButton => Self::PowerButton,
            WakeupMethod::WakeupByTouch => Self::Touch(TouchAction::Tap),
            WakeupMethod::WakeupBySwipeUp => Self::Touch(TouchAction::Swipe(SwipeDirection::Up)),
            WakeupMethod::WakeupBySwipeDown => {
                Self::Touch(TouchAction::Swipe(SwipeDirection::Down))
            }
            WakeupMethod::WakeupBySwipeLeft => {
                Self::Touch(TouchAction::Swipe(SwipeDirection::Left))
            }
            WakeupMethod::WakeupBySwipeRight => {
                Self::Touch(TouchAction::Swipe(SwipeDirection::Right))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedTimerPlan {
    method: WakeupMethod,
    action: PlanAction,
    test_type: WakeupTestType,
    num_events: u32,
    offset_ns: i64,
    interval_ns: i64,
    last_offset_ns: i64,
    last_target_boot: zx::BootInstant,
    coord_x: u32,
    coord_y: u32,
    dev_option: u32,
    max_x: i64,
    max_y: i64,
    target_boots: Vec<zx::BootInstant>,
}

impl ValidatedTimerPlan {
    fn from_timer_info(timer_info: WakeupTimerInfo, base_boot: zx::BootInstant) -> Result<Self> {
        let method = WakeupMethod::try_from(timer_info.method)?;
        let action = PlanAction::from(method);

        let max_events = match action {
            PlanAction::PowerButton => MAX_POWER_BUTTON_EVENTS,
            PlanAction::Touch(_) => MAX_WAKEUP_EVENTS,
        };
        if timer_info.num_events == 0 || timer_info.num_events > max_events {
            return Err(anyhow!(
                "num_events {} out of allowed range 1..={max_events} for {method:?}",
                timer_info.num_events
            ));
        }
        if timer_info.offset < 0 {
            return Err(anyhow!("offset must be non-negative, got {}", timer_info.offset));
        }
        if timer_info.interval < 0 {
            return Err(anyhow!("interval must be non-negative, got {}", timer_info.interval));
        }

        if (timer_info.screen_x == 0) != (timer_info.screen_y == 0) {
            return Err(anyhow!(
                "screen_x ({}) and screen_y ({}) must either both be 0 or both be >= {MIN_SCREEN_DIMENSION}",
                timer_info.screen_x,
                timer_info.screen_y
            ));
        }

        let max_coord_limit = i32::MAX as u32;
        if timer_info.screen_x > max_coord_limit || timer_info.screen_y > max_coord_limit {
            return Err(anyhow!(
                "screen dimensions ({}, {}) exceed i32::MAX",
                timer_info.screen_x,
                timer_info.screen_y
            ));
        }

        let (max_x, max_y) = if timer_info.screen_x > 0 && timer_info.screen_y > 0 {
            if timer_info.screen_x < MIN_SCREEN_DIMENSION
                || timer_info.screen_y < MIN_SCREEN_DIMENSION
            {
                return Err(anyhow!(
                    "screen dimensions ({}, {}) must be at least {MIN_SCREEN_DIMENSION}",
                    timer_info.screen_x,
                    timer_info.screen_y
                ));
            }
            (i64::from(timer_info.screen_x), i64::from(timer_info.screen_y))
        } else {
            (DEFAULT_SCREEN_MAX_X, DEFAULT_SCREEN_MAX_Y)
        };

        let base_boot_ns = base_boot.into_nanos();
        let mut target_boots = Vec::with_capacity(timer_info.num_events as usize);
        let mut last_offset_ns = timer_info.offset;
        for index in 0..timer_info.num_events {
            let step_ns = i64::from(index)
                .checked_mul(timer_info.interval)
                .ok_or_else(|| anyhow!("timer interval overflow at index {index}"))?;
            let total_offset_ns = timer_info
                .offset
                .checked_add(step_ns)
                .ok_or_else(|| anyhow!("timer offset overflow at index {index}"))?;
            let target_boot_ns = base_boot_ns
                .checked_add(total_offset_ns)
                .ok_or_else(|| anyhow!("boot timestamp overflow at index {index}"))?;
            last_offset_ns = total_offset_ns;
            target_boots.push(zx::BootInstant::from_nanos(target_boot_ns));
        }

        let last_target_boot = target_boots
            .last()
            .copied()
            .ok_or_else(|| anyhow!("target_boots must be non-empty"))?;

        Ok(Self {
            method,
            action,
            test_type: WakeupTestType::from(timer_info.test_type),
            num_events: timer_info.num_events,
            offset_ns: timer_info.offset,
            interval_ns: timer_info.interval,
            last_offset_ns,
            last_target_boot,
            coord_x: timer_info.coord_x,
            coord_y: timer_info.coord_y,
            dev_option: timer_info.dev_option,
            max_x,
            max_y,
            target_boots,
        })
    }
}

/// Checks whether a touch/swipe schedule should anchor to a pending future `PowerButton` wake,
/// returning the matched [`PowerScheduleAnchor`] and non-negative `post_wake_offset_ns`.
fn resolve_power_anchor(
    last_schedule: Option<PowerScheduleAnchor>,
    touch_offset_ns: i64,
    now_boot: zx::BootInstant,
) -> Option<(PowerScheduleAnchor, i64)> {
    let anchor = last_schedule?;
    if touch_offset_ns >= anchor.offset_ns && now_boot < anchor.target_boot {
        Some((anchor, touch_offset_ns - anchor.offset_ns))
    } else {
        None
    }
}

/// Computes post-wake touch/swipe target boot instants anchored to `wake_anchor` (the actual
/// container resume instant after S3 wake, compensating for 1-second hardware RTC quantization).
fn compute_anchored_touch_target_boots(
    wake_anchor: zx::BootInstant,
    post_wake_offset_ns: i64,
    interval_ns: i64,
    num_events: u32,
) -> Vec<zx::BootInstant> {
    let wake_anchor_ns = wake_anchor.into_nanos();
    (0..num_events)
        .map(|index| {
            let step_ns = i64::from(index).saturating_mul(interval_ns);
            let total_offset_ns = post_wake_offset_ns.saturating_add(step_ns);
            zx::BootInstant::from_nanos(wake_anchor_ns.saturating_add(total_offset_ns))
        })
        .collect()
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TouchGesture {
    Tap { x: i64, y: i64, hold_duration: zx::MonotonicDuration },
    Swipe { start: fmath::Vec_, end: fmath::Vec_, duration_ns: i64, move_event_count: u32 },
}

fn compute_touch_gesture(
    plan: &ValidatedTimerPlan,
    action: TouchAction,
    index: u32,
) -> TouchGesture {
    let (anchor_x, anchor_y) = if plan.coord_x > 0 || plan.coord_y > 0 {
        (i64::from(plan.coord_x).clamp(0, plan.max_x), i64::from(plan.coord_y).clamp(0, plan.max_y))
    } else {
        (plan.max_x / 2, plan.max_y / 2)
    };

    match action {
        TouchAction::Tap => {
            let hold_ms = if plan.dev_option > 0 {
                i64::from(plan.dev_option)
            } else {
                DEFAULT_TAP_HOLD_DURATION_MS
            };
            let x = if plan.num_events > 1 && index % 2 == 1 {
                let shifted_x = (anchor_x + ALTERNATE_TAP_OFFSET_X).min(plan.max_x);
                if shifted_x == anchor_x {
                    (anchor_x - ALTERNATE_TAP_OFFSET_X).max(0)
                } else {
                    shifted_x
                }
            } else {
                anchor_x
            };
            TouchGesture::Tap {
                x,
                y: anchor_y,
                hold_duration: zx::MonotonicDuration::from_millis(hold_ms),
            }
        }
        TouchAction::Swipe(direction) => {
            let near_x = plan.max_x * SWIPE_NEAR_FRACTION_NUM / SWIPE_FRACTION_DENOM;
            let far_x = plan.max_x * SWIPE_FAR_FRACTION_NUM / SWIPE_FRACTION_DENOM;
            let near_y = plan.max_y * SWIPE_NEAR_FRACTION_NUM / SWIPE_FRACTION_DENOM;
            let far_y = plan.max_y * SWIPE_FAR_FRACTION_NUM / SWIPE_FRACTION_DENOM;
            let (start_coords, end_coords) = match direction {
                SwipeDirection::Up => ((anchor_x, far_y), (anchor_x, near_y)),
                SwipeDirection::Down => ((anchor_x, near_y), (anchor_x, far_y)),
                SwipeDirection::Left => ((far_x, anchor_y), (near_x, anchor_y)),
                SwipeDirection::Right => ((near_x, anchor_y), (far_x, anchor_y)),
            };

            let default_start = fmath::Vec_ { x: start_coords.0 as i32, y: start_coords.1 as i32 };
            let default_end = fmath::Vec_ { x: end_coords.0 as i32, y: end_coords.1 as i32 };
            let (start, end) = if plan.num_events > 1 && index % 2 == 1 {
                (default_end, default_start)
            } else {
                (default_start, default_end)
            };

            let duration_ms = if plan.dev_option > 0 {
                i64::from(plan.dev_option)
            } else {
                DEFAULT_SWIPE_DURATION_MS
            };
            let duration_ns = zx::MonotonicDuration::from_millis(duration_ms).into_nanos();
            let move_event_count = (duration_ms / SWIPE_MS_PER_MOVE_EVENT)
                .clamp(i64::from(MIN_SWIPE_MOVE_EVENTS), i64::from(MAX_SWIPE_MOVE_EVENTS))
                as u32;

            TouchGesture::Swipe { start, end, duration_ns, move_event_count }
        }
    }
}

/// Outcome of async input setup, recorded by whichever of the setup worker or the waiting `ioctl`
/// caller gets there first.
enum SetupOutcome {
    /// The worker finished setup with this result before the caller gave up waiting.
    Completed(Result<(), Errno>),
    /// The caller gave up waiting (timeout or `EINTR`) before the worker finished setup.
    Cancelled,
    /// The worker dropped its [`SetupNotifier`] without finishing setup.
    Abandoned,
}

/// Single-winner rendezvous between the async input setup worker and the waiting `ioctl` caller.
///
/// Exactly one [`SetupOutcome`] is ever recorded, so the worker keeps (or caches) the proxies it
/// set up only if the `ioctl` returns the worker's `Ok(())`, and never after the `ioctl` has
/// already returned a timeout or `EINTR` error.
#[derive(Default)]
struct SetupRendezvous {
    outcome: OnceLock<SetupOutcome>,
}

impl SetupRendezvous {
    /// Records `outcome` unless an outcome was already recorded.
    /// Returns `true` if `outcome` was recorded.
    fn resolve(&self, outcome: SetupOutcome) -> bool {
        self.outcome.set(outcome).is_ok()
    }

    /// Returns the result recorded by the worker, or `None` if the worker has not recorded one.
    /// An abandoned setup is logged and reported as `Some(Err(EIO))`.
    fn worker_result(&self) -> Option<Result<(), Errno>> {
        match self.outcome.get()? {
            SetupOutcome::Completed(result) => Some(result.clone()),
            SetupOutcome::Cancelled => None,
            SetupOutcome::Abandoned => {
                log_error!(
                    "WakeupTestDevice::WakeupSetTimers input setup abandoned without a result"
                );
                Some(error!(EIO))
            }
        }
    }

    /// Records cancellation after the `ioctl` caller stopped waiting with `wait_err`.
    /// Returns `Err(wait_err)` if cancellation was recorded, or else the result that the worker
    /// recorded first.
    fn cancel(&self, wait_err: Errno) -> Result<(), Errno> {
        if self.resolve(SetupOutcome::Cancelled) {
            return Err(wait_err);
        }
        self.worker_result().unwrap_or(Err(wait_err))
    }

    /// Blocks the `ioctl` caller on `waiter` until the worker records a result, or records
    /// cancellation once `deadline` passes or the wait is interrupted.
    fn wait(
        &self,
        waiter: &Waiter,
        current_task: &CurrentTask,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), Errno> {
        loop {
            if let Some(result) = self.worker_result() {
                return result;
            }
            if let Err(wait_err) = waiter.wait_until(current_task, deadline) {
                return self.cancel(wait_err);
            }
        }
    }
}

/// Ensures the waiting `ioctl` thread is always woken when async input setup finishes or aborts.
struct SetupNotifier {
    rendezvous: Option<Arc<SetupRendezvous>>,
    waiter: Waiter,
}

impl SetupNotifier {
    fn new(rendezvous: Arc<SetupRendezvous>, waiter: Waiter) -> Self {
        Self { rendezvous: Some(rendezvous), waiter }
    }

    /// Records `result` for the `ioctl` caller and notifies `waiter`.
    /// Returns `true` only if `result` was recorded before the `ioctl` caller gave up waiting.
    fn complete(mut self, result: Result<(), Errno>) -> bool {
        self.record_and_notify(SetupOutcome::Completed(result))
    }

    fn record_and_notify(&mut self, outcome: SetupOutcome) -> bool {
        let Some(rendezvous) = self.rendezvous.take() else {
            return false;
        };
        let recorded = rendezvous.resolve(outcome);
        // Notify only after recording `outcome` so the woken caller observes it.
        self.waiter.notify();
        recorded
    }
}

impl Drop for SetupNotifier {
    fn drop(&mut self) {
        let _ = self.record_and_notify(SetupOutcome::Abandoned);
    }
}

/// Runs `setup_fut` while polling `caller_cancelled_rx` first so that if the `ioctl` caller has
/// already exited due to timeout or `EINTR` (dropping its `_caller_alive_tx`), `setup_fut` is
/// dropped immediately without ever polling any further FIDL calls.
async fn run_cancellable_setup<T, F>(
    caller_cancelled_rx: oneshot::Receiver<()>,
    setup_fut: F,
) -> Option<T>
where
    F: Future<Output = T>,
{
    let pinned_setup = pin!(setup_fut);
    match select(caller_cancelled_rx, pinned_setup).await {
        Either::Left(_) => None,
        Either::Right((output, _)) => Some(output),
    }
}

/// Connects to the input Registry and runs `setup` on it before `setup_deadline` via
/// [`run_cancellable_setup`], then reports the result to the `ioctl` caller via `setup_notifier`.
///
/// Returns `Some(value)` only if the waiting `ioctl` caller accepted the `Ok(())` result, so
/// callers keep (or cache) the proxies they set up only when the `ioctl` reports success. Returns
/// `None` if setup failed, timed out, or the `ioctl` caller already gave up waiting.
async fn run_input_setup<T, F, Fut>(
    caller_cancelled_rx: oneshot::Receiver<()>,
    registry_client: &RegistryClient,
    setup_deadline: MonotonicInstant,
    setup_notifier: SetupNotifier,
    device_kind: &str,
    setup: F,
) -> Option<T>
where
    F: FnOnce(futinput::RegistryProxy) -> Fut,
    Fut: Future<Output = Result<T, Errno>>,
{
    let setup_fut = async move {
        let registry = registry_client.connect().map_err(|err| {
            log_error!("Failed to connect to input registry: {err:?}");
            errno!(EIO)
        })?;
        setup(registry).await
    }
    .on_timeout(setup_deadline, || {
        log_error!("WakeupTestDevice::WakeupSetTimers {device_kind} setup timed out");
        error!(ETIMEDOUT)
    });

    match run_cancellable_setup(caller_cancelled_rx, setup_fut).await? {
        Ok(value) => setup_notifier.complete(Ok(())).then_some(value),
        Err(errno) => {
            let _ = setup_notifier.complete(Err(errno));
            None
        }
    }
}

async fn execute_touch_schedule(
    touchscreen_proxy: futinput::TouchScreenProxy,
    plan: ValidatedTimerPlan,
    touch_action: TouchAction,
    matched_power_anchor: Option<(PowerScheduleAnchor, i64)>,
) {
    // If a preceding PowerButton wake is pending in the future, wait for `anchor.target_boot`
    // (which resolves upon actual container resume after S3 wake) and anchor post-wake touch
    // offsets to `zx::BootInstant::get()` so 1-second PMIC RTC quantization does not cause
    // pre-expired touch bursts.
    let target_boots = match matched_power_anchor {
        Some((anchor, post_wake_offset_ns)) => {
            Timer::new(AsyncBootInstant::from_zx(anchor.target_boot)).await;
            let wake_anchor = zx::BootInstant::get();
            log_info!(
                "WakeupTestDevice: anchored {} touch events to wake_anchor {:?} (+{} ns)",
                plan.num_events,
                wake_anchor,
                post_wake_offset_ns
            );
            compute_anchored_touch_target_boots(
                wake_anchor,
                post_wake_offset_ns,
                plan.interval_ns,
                plan.num_events,
            )
        }
        None => plan.target_boots.clone(),
    };

    for (index, target_boot) in target_boots.into_iter().enumerate() {
        let index = index as u32;
        Timer::new(AsyncBootInstant::from_zx(target_boot)).await;

        let method_str = plan.method.as_str();
        fuchsia_trace::instant!(
            INPUT_TRACE_CATEGORY,
            "WakeupTest:OnTouchTimer",
            fuchsia_trace::Scope::Thread,
            "index" => index,
            "method" => method_str
        );
        let gesture = compute_touch_gesture(&plan, touch_action, index);
        log_info!(
            "WakeupTestDevice::OnTouchTimer index: {index}, method: {method_str}, gesture: {gesture:?}"
        );

        let inject_result = match gesture {
            TouchGesture::Tap { x, y, hold_duration } => {
                simulate_tap(&touchscreen_proxy, x, y, hold_duration).await
            }
            TouchGesture::Swipe { start, end, duration_ns, move_event_count } => {
                simulate_swipe(&touchscreen_proxy, start, end, duration_ns, move_event_count).await
            }
        };

        if let Err(err) = inject_result {
            log_error!(
                "WakeupTestDevice: failed to inject {:?} at index {index}: {err:?}",
                plan.method
            );
            break;
        }
    }
}

fn evict_closed_touchscreen(cached_touchscreen: &mut Option<CachedTouchScreen>) {
    if cached_touchscreen.as_ref().is_some_and(|cached| cached.proxy.is_closed()) {
        *cached_touchscreen = None;
    }
}

/// Executor-scoped worker owning [`CachedTouchScreen`] across `WakeupSetTimers` iterations.
///
/// Callers must run at most one touch/swipe schedule at a time: concurrent touch schedules are not
/// serialized and share `cached_touchscreen` and `DEFAULT_TOUCH_CONTACT_ID`.
async fn run_touch_worker(rx: mpsc::UnboundedReceiver<TouchWorkerCommand>) {
    let mut rx = rx.fuse();
    let mut cached_touchscreen: Option<CachedTouchScreen> = None;
    let mut active_injections = FuturesUnordered::new();
    let mut rx_closed = false;

    loop {
        if rx_closed && active_injections.is_empty() {
            break;
        }

        futures::select! {
            command = rx.next() => {
                match command {
                    Some(TouchWorkerCommand::Schedule(mut request)) => {
                        if matches!(request.caller_cancelled_rx.try_recv(), Err(oneshot::Canceled))
                        {
                            continue;
                        }

                        evict_closed_touchscreen(&mut cached_touchscreen);

                        let reusable_proxy = cached_touchscreen
                            .as_ref()
                            .filter(|cached| {
                                cached.max_x == request.plan.max_x
                                    && cached.max_y == request.plan.max_y
                            })
                            .map(|cached| cached.proxy.clone());

                        let touchscreen_proxy = match reusable_proxy {
                            Some(proxy) => {
                                if !request.setup_notifier.complete(Ok(())) {
                                    continue;
                                }
                                proxy
                            }
                            None => {
                                let max_x = request.plan.max_x;
                                let max_y = request.plan.max_y;
                                // Signal completion of virtual touchscreen registration before
                                // returning from ioctl so Input Pipeline and Android EventHub
                                // discover the device while still awake prior to S3 suspend. Only
                                // cache `fresh_proxy` when the waiting `ioctl` accepts the result.
                                let Some(fresh_proxy) = run_input_setup(
                                    request.caller_cancelled_rx,
                                    &request.registry_client,
                                    request.setup_deadline,
                                    request.setup_notifier,
                                    "touchscreen",
                                    move |registry| async move {
                                        create_touchscreen_proxy(&registry, max_x, max_y)
                                            .await
                                            .map_err(|err| {
                                                log_error!(
                                                    "Failed to create touchscreen proxy: {err:?}"
                                                );
                                                errno!(EIO)
                                            })
                                    },
                                )
                                .await
                                else {
                                    continue;
                                };
                                cached_touchscreen = Some(CachedTouchScreen {
                                    max_x,
                                    max_y,
                                    proxy: fresh_proxy.clone(),
                                });
                                fresh_proxy
                            }
                        };

                        active_injections.push(execute_touch_schedule(
                            touchscreen_proxy,
                            request.plan,
                            request.touch_action,
                            request.matched_power_anchor,
                        ));
                    }
                    #[cfg(test)]
                    Some(TouchWorkerCommand::HasOpenCachedTouchScreen(reply_tx)) => {
                        evict_closed_touchscreen(&mut cached_touchscreen);
                        let _ = reply_tx.send(cached_touchscreen.is_some());
                    }
                    None => {
                        rx_closed = true;
                    }
                }
            }
            _ = active_injections.select_next_some() => {
                evict_closed_touchscreen(&mut cached_touchscreen);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RegistryClient {
    #[cfg(test)]
    test_registry_tx:
        Option<mpsc::UnboundedSender<fidl::endpoints::ServerEnd<futinput::RegistryMarker>>>,
}

impl RegistryClient {
    fn connect(&self) -> Result<futinput::RegistryProxy> {
        #[cfg(test)]
        if let Some(sender) = &self.test_registry_tx {
            let (client_end, server_end) =
                fidl::endpoints::create_endpoints::<futinput::RegistryMarker>();
            let _ = sender.unbounded_send(server_end);
            return Ok(client_end.into_proxy());
        }
        connect_to_protocol::<futinput::RegistryMarker>()
            .map_err(|err| anyhow!("Could not get registry proxy: {err}"))
    }
}

#[derive(Debug, Clone)]
pub struct WakeupTestDevice {
    commands: Commands,
}

impl CloseFreeSafe for WakeupTestDevice {}

impl WakeupTestDevice {
    pub fn new(kernel: &Arc<Kernel>) -> Self {
        Self {
            commands: Commands {
                kernel: Arc::downgrade(kernel),
                registry_client: RegistryClient::default(),
                state: Arc::new(LockDepMutex::new(WakeupTestState::default())),
                setup_timeout: SETUP_TIMEOUT,
                power_button_keepalive_duration: POWER_BUTTON_KEEPALIVE_DURATION,
            },
        }
    }

    #[cfg(test)]
    fn new_for_test(
        kernel: &Arc<Kernel>,
        test_registry_tx: mpsc::UnboundedSender<
            fidl::endpoints::ServerEnd<futinput::RegistryMarker>,
        >,
        setup_timeout: zx::MonotonicDuration,
        power_button_keepalive_duration: zx::BootDuration,
    ) -> Self {
        Self {
            commands: Commands {
                kernel: Arc::downgrade(kernel),
                registry_client: RegistryClient { test_registry_tx: Some(test_registry_tx) },
                state: Arc::new(LockDepMutex::new(WakeupTestState::default())),
                setup_timeout,
                power_button_keepalive_duration,
            },
        }
    }

    #[cfg(test)]
    async fn has_open_cached_touchscreen(&self) -> bool {
        let Some(worker_tx) = self
            .commands
            .state
            .lock()
            .touch_worker_tx
            .as_ref()
            .filter(|tx| !tx.is_closed())
            .cloned()
        else {
            return false;
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if worker_tx.unbounded_send(TouchWorkerCommand::HasOpenCachedTouchScreen(reply_tx)).is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }
}

impl DeviceOps for WakeupTestDevice {
    fn open(
        &self,
        current_task: &CurrentTask,
        _id: device_id::DeviceId,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mut opened = self.clone();
        opened.commands.kernel = Arc::downgrade(current_task.kernel());
        Ok(Box::new(opened))
    }
}

#[derive(Debug, Clone)]
struct Commands {
    kernel: Weak<Kernel>,
    registry_client: RegistryClient,
    state: Arc<LockDepMutex<WakeupTestState, WakeupTestStateLock>>,
    setup_timeout: zx::MonotonicDuration,
    power_button_keepalive_duration: zx::BootDuration,
}

impl Commands {
    fn run_wakeup_set_timers(
        &self,
        current_task: &CurrentTask,
        timer_info: WakeupTimerInfo,
    ) -> Result<(), Errno> {
        let base_boot = zx::BootInstant::get();
        let plan = ValidatedTimerPlan::from_timer_info(timer_info, base_boot).map_err(|err| {
            log_error!("WakeupTestDevice::WakeupSetTimers invalid timer info: {err:?}");
            errno!(EINVAL)
        })?;

        let kernel = self.kernel.upgrade().ok_or_else(|| {
            log_error!("WakeupTestDevice::WakeupSetTimers kernel no longer exists");
            errno!(EIO)
        })?;

        log_info!(
            "WakeupTestDevice::WakeupSetTimers test_type: {:?} Setting {} timers for {:?}, interval {} ns, starting {} ns",
            plan.test_type,
            plan.num_events,
            plan.method,
            timer_info.interval,
            timer_info.offset
        );
        tracing::trace_wakeup_test_type(
            self.trace_event_queues(),
            current_task.get_tid(),
            plan.test_type,
        );

        let matched_power_anchor = match plan.action {
            PlanAction::PowerButton => None,
            PlanAction::Touch(_) => resolve_power_anchor(
                self.state.lock().last_power_schedule,
                plan.offset_ns,
                base_boot,
            ),
        };
        let recorded_power_anchor = match plan.action {
            PlanAction::PowerButton => Some(PowerScheduleAnchor {
                target_boot: plan.last_target_boot,
                offset_ns: plan.last_offset_ns,
            }),
            PlanAction::Touch(_) => None,
        };

        let waiter = Waiter::new();
        let rendezvous = Arc::new(SetupRendezvous::default());
        let setup_notifier = SetupNotifier::new(Arc::clone(&rendezvous), waiter.clone());
        // Starnix user threads have no fuchsia_async executor, so read the clock via `zx` rather
        // than `MonotonicInstant::after`, which panics without one.
        let setup_deadline =
            MonotonicInstant::from_zx(zx::MonotonicInstant::after(self.setup_timeout));
        let (_caller_alive_tx, caller_cancelled_rx) = oneshot::channel::<()>();
        let registry_client = self.registry_client.clone();
        let power_button_keepalive_duration = self.power_button_keepalive_duration;

        match plan.action {
            PlanAction::PowerButton => {
                kernel.kthreads.spawn_future(
                    move || async move {
                        // Each MediaButtonsDevice registration in input_helper owns a single
                        // WakeAlarms connection with a fixed alarm ID, so register one proxy per
                        // scheduled PowerButton event over a single Registry connection to avoid
                        // cancelling earlier pending alarms.
                        let target_boots = &plan.target_boots;
                        let Some(media_button_proxies) = run_input_setup(
                            caller_cancelled_rx,
                            &registry_client,
                            setup_deadline,
                            setup_notifier,
                            "media buttons",
                            move |registry| async move {
                                let mut proxies =
                                    Vec::<futinput::MediaButtonsDeviceProxy>::with_capacity(
                                        target_boots.len(),
                                    );
                                for (index, &target_boot) in target_boots.iter().enumerate() {
                                    let proxy = create_media_buttons_proxy(&registry)
                                        .await
                                        .map_err(|err| {
                                            log_error!(
                                                "Failed to create media buttons proxy at index {index}: {err:?}"
                                            );
                                            errno!(EIO)
                                        })?;
                                    let delay = (target_boot - zx::BootInstant::get())
                                        .max(zx::BootDuration::from_nanos(0));
                                    log_info!(
                                        "WakeupTestDevice::set_timer i: {index} delay: {delay:?}"
                                    );
                                    schedule_wakeup_power_button(&proxy, delay).await.map_err(
                                        |err| {
                                            log_error!(
                                                "Failed to schedule power button at index {index}: {err:?}"
                                            );
                                            errno!(EIO)
                                        },
                                    )?;
                                    proxies.push(proxy);
                                }
                                Ok(proxies)
                            },
                        )
                        .await
                        else {
                            return;
                        };

                        // First wait until the scheduled boot target (which across S3 suspend wakes
                        // at `max(last_target_boot, t_rtc_resume)`), then retain the virtual media
                        // button proxies for an additional keepalive window measured from actual
                        // resume so 1-second hardware RTC rounding never drops them early.
                        Timer::new(AsyncBootInstant::from_zx(plan.last_target_boot)).await;
                        let keepalive_deadline =
                            AsyncBootInstant::from_zx(zx::BootInstant::from_nanos(
                                zx::BootInstant::get()
                                    .into_nanos()
                                    .saturating_add(power_button_keepalive_duration.into_nanos()),
                            ));
                        Timer::new(keepalive_deadline).await;
                        log_info!("dropped {} media_button proxies.", media_button_proxies.len());
                    },
                    "wakeup_test",
                );
            }
            PlanAction::Touch(touch_action) => {
                let (worker_tx, new_worker_rx) = {
                    let mut guard = self.state.lock();
                    if let Some(existing_tx) =
                        guard.touch_worker_tx.as_ref().filter(|tx| !tx.is_closed())
                    {
                        (existing_tx.clone(), None)
                    } else {
                        let (tx, rx) = mpsc::unbounded::<TouchWorkerCommand>();
                        guard.touch_worker_tx = Some(tx.clone());
                        (tx, Some(rx))
                    }
                };
                if let Some(rx) = new_worker_rx {
                    kernel.kthreads.spawn_future(
                        move || async move {
                            run_touch_worker(rx).await;
                        },
                        "wakeup_test_touch_worker",
                    );
                }
                let _ =
                    worker_tx.unbounded_send(TouchWorkerCommand::Schedule(TouchScheduleRequest {
                        plan,
                        touch_action,
                        matched_power_anchor,
                        registry_client,
                        setup_deadline,
                        setup_notifier,
                        caller_cancelled_rx,
                    }));
            }
        }

        // On timeout or `EINTR`, `rendezvous` records cancellation atomically so a racing worker
        // completion cannot keep proxies alive after the `ioctl` returns an error, and dropping
        // `_caller_alive_tx` on return stops the worker from polling any further setup.
        let result = rendezvous.wait(&waiter, current_task, setup_deadline.into_zx());
        if result.is_ok() {
            if let Some(anchor) = recorded_power_anchor {
                self.state.lock().last_power_schedule = Some(anchor);
            }
        }
        result
    }

    /// Returns the trace event queue if enabled to emit container trace events.
    fn trace_event_queues(&self) -> Option<Arc<TraceEventQueueList>> {
        let kernel = self.kernel.upgrade()?;
        let queues = TraceEventQueueList::from(&kernel);
        if queues.is_enabled() {
            Some(queues)
        } else {
            log_error!("Trace is not enabled");
            None
        }
    }
}

impl FileOps for WakeupTestDevice {
    fileops_impl_seekless!();
    fileops_impl_dataless!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: starnix_syscalls::SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let cmd_num = CommandCode::from(request);
        log_info!("WakeupTestDevice::ioctl cmd_num {cmd_num:?}, arg: {arg:?}");

        match cmd_num {
            CommandCode::WakeupSetTimers => {
                let timer_ref = UserRef::<WakeupTimerInfo>::new(arg.into());
                let timer_info = current_task.read_object(timer_ref)?;
                log_info!("WakeupTestDevice::WakeupSetTimers {timer_info:?}");
                log_info!("WakeupTestDevice::WakeupSetTimers version 0x{:x}", timer_info.version);

                self.commands.run_wakeup_set_timers(current_task, timer_info)?;
                Ok(SUCCESS)
            }
            CommandCode::WakeupTest => error!(ENOSYS),
            CommandCode::WakeupHowManyTimers => error!(ENOSYS),
            CommandCode::WakeupCancelTimers => error!(ENOSYS),
            CommandCode::Unknown => error!(ENOTTY),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SCREEN_MAX_X, DEFAULT_SCREEN_MAX_Y, PowerScheduleAnchor, SetupNotifier,
        SetupRendezvous, SwipeDirection, TouchAction, TouchGesture, ValidatedTimerPlan,
        WakeupTestDevice, compute_anchored_touch_target_boots, compute_touch_gesture,
        resolve_power_anchor,
    };
    use crate::ioctl::{MAX_POWER_BUTTON_EVENTS, MAX_WAKEUP_EVENTS, WakeupMethod, WakeupTimerInfo};
    use fidl_fuchsia_input::ConsumerControlButton;
    use fidl_fuchsia_math as fmath;
    use fidl_fuchsia_ui_test_input as futinput;
    use futures::channel::{mpsc, oneshot};
    use futures::future::{Either, select};
    use futures::{Stream, StreamExt};
    use linux_uapi::_IOC_NRSHIFT;
    use starnix_core::task::Waiter;
    use starnix_core::testing::{
        UserMemoryWriter, anon_test_file, map_memory, spawn_kernel_and_run,
        spawn_kernel_and_run_sync,
    };
    use starnix_syscalls::{SUCCESS, SyscallArg};
    use starnix_uapi::errno;
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::user_address::UserAddress;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SET_TIMERS_IOCTL: u32 = 1u32 << _IOC_NRSHIFT;
    // Generous setup timeout for non-timeout tests so contended software-emulated environments
    // (such as QEMU-riscv64) do not hit ETIMEDOUT prematurely.
    const TEST_SETUP_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(15);
    const HUNG_REGISTRY_SETUP_TIMEOUT: zx::MonotonicDuration =
        zx::MonotonicDuration::from_millis(100);
    // Multi-second headroom prevents startup, scheduling, and touch setup latency from outrunning
    // test deadlines under contended software-emulated environments such as QEMU-riscv64.
    const POWER_BUTTON_TEST_OFFSET_NS: i64 = 5_000_000_000;
    const POWER_BUTTON_TEST_INTERVAL_NS: i64 = 2_000_000_000;
    const PLANTED_ANCHOR_LEAD_TIME: zx::BootDuration = zx::BootDuration::from_seconds(5);

    type TestRegistrySender =
        mpsc::UnboundedSender<fidl::endpoints::ServerEnd<futinput::RegistryMarker>>;
    type TestRegistryStream =
        Pin<Box<dyn Stream<Item = Result<futinput::RegistryRequest, fidl::Error>>>>;

    fn create_test_registry() -> (TestRegistrySender, TestRegistryStream) {
        let (sender, receiver) =
            mpsc::unbounded::<fidl::endpoints::ServerEnd<futinput::RegistryMarker>>();
        let stream =
            Box::pin(receiver.map(|server_end| server_end.into_stream()).flatten_unordered(None));
        (sender, stream)
    }

    fn base_timer_info(method: WakeupMethod) -> WakeupTimerInfo {
        WakeupTimerInfo {
            version: 1,
            test_type: 0,
            interval: 100_000_000,
            offset: 500_000_000,
            method: method as i32,
            num_events: 1,
            coord_x: 0,
            coord_y: 0,
            dev_option: 0,
            screen_x: 0,
            screen_y: 0,
        }
    }

    fn encode_timer_info(info: &WakeupTimerInfo) -> [u8; 64] {
        let mut buffer = [0u8; 64];
        buffer[0..8].copy_from_slice(&info.version.to_ne_bytes());
        buffer[8..12].copy_from_slice(&info.test_type.to_ne_bytes());
        buffer[16..24].copy_from_slice(&info.interval.to_ne_bytes());
        buffer[24..32].copy_from_slice(&info.offset.to_ne_bytes());
        buffer[32..36].copy_from_slice(&info.method.to_ne_bytes());
        buffer[36..40].copy_from_slice(&info.num_events.to_ne_bytes());
        buffer[40..44].copy_from_slice(&info.coord_x.to_ne_bytes());
        buffer[44..48].copy_from_slice(&info.coord_y.to_ne_bytes());
        buffer[48..52].copy_from_slice(&info.dev_option.to_ne_bytes());
        buffer[52..56].copy_from_slice(&info.screen_x.to_ne_bytes());
        buffer[56..60].copy_from_slice(&info.screen_y.to_ne_bytes());
        buffer
    }

    #[fuchsia::test]
    fn validated_timer_plan_rejects_invalid_inputs() {
        let base_boot = zx::BootInstant::from_nanos(1_000_000_000);

        let mut invalid_method = base_timer_info(WakeupMethod::PowerButton);
        invalid_method.method = 99;
        assert!(ValidatedTimerPlan::from_timer_info(invalid_method, base_boot).is_err());

        let mut negative_method = base_timer_info(WakeupMethod::PowerButton);
        negative_method.method = -1;
        assert!(ValidatedTimerPlan::from_timer_info(negative_method, base_boot).is_err());

        let mut zero_events = base_timer_info(WakeupMethod::WakeupByTouch);
        zero_events.num_events = 0;
        assert!(ValidatedTimerPlan::from_timer_info(zero_events, base_boot).is_err());

        let mut too_many_events = base_timer_info(WakeupMethod::WakeupByTouch);
        too_many_events.num_events = MAX_WAKEUP_EVENTS + 1;
        assert!(ValidatedTimerPlan::from_timer_info(too_many_events, base_boot).is_err());

        let mut too_many_power_events = base_timer_info(WakeupMethod::PowerButton);
        too_many_power_events.num_events = MAX_POWER_BUTTON_EVENTS + 1;
        assert!(ValidatedTimerPlan::from_timer_info(too_many_power_events, base_boot).is_err());

        let mut touch_above_power_cap = base_timer_info(WakeupMethod::WakeupByTouch);
        touch_above_power_cap.num_events = MAX_POWER_BUTTON_EVENTS + 1;
        assert!(ValidatedTimerPlan::from_timer_info(touch_above_power_cap, base_boot).is_ok());

        let mut negative_offset = base_timer_info(WakeupMethod::WakeupByTouch);
        negative_offset.offset = -1;
        assert!(ValidatedTimerPlan::from_timer_info(negative_offset, base_boot).is_err());

        let mut negative_interval = base_timer_info(WakeupMethod::WakeupByTouch);
        negative_interval.interval = -1;
        assert!(ValidatedTimerPlan::from_timer_info(negative_interval, base_boot).is_err());

        let mut overflow_interval = base_timer_info(WakeupMethod::WakeupByTouch);
        overflow_interval.num_events = 3;
        overflow_interval.interval = i64::MAX / 2 + 10;
        assert!(ValidatedTimerPlan::from_timer_info(overflow_interval, base_boot).is_err());

        let mut overflow_offset = base_timer_info(WakeupMethod::WakeupByTouch);
        overflow_offset.offset = i64::MAX;
        assert!(ValidatedTimerPlan::from_timer_info(overflow_offset, base_boot).is_err());

        let mut huge_screen = base_timer_info(WakeupMethod::WakeupByTouch);
        huge_screen.screen_x = u32::MAX;
        huge_screen.screen_y = 1000;
        assert!(ValidatedTimerPlan::from_timer_info(huge_screen, base_boot).is_err());

        // Screen dimensions of 1 or mismatched partial non-zero dimensions must fail.
        let mut tiny_screen_x = base_timer_info(WakeupMethod::WakeupBySwipeLeft);
        tiny_screen_x.screen_x = 1;
        tiny_screen_x.screen_y = 1000;
        assert!(ValidatedTimerPlan::from_timer_info(tiny_screen_x, base_boot).is_err());

        let mut tiny_screen_y = base_timer_info(WakeupMethod::WakeupBySwipeUp);
        tiny_screen_y.screen_x = 1000;
        tiny_screen_y.screen_y = 1;
        assert!(ValidatedTimerPlan::from_timer_info(tiny_screen_y, base_boot).is_err());

        let mut partial_screen_x = base_timer_info(WakeupMethod::WakeupByTouch);
        partial_screen_x.screen_x = 1000;
        partial_screen_x.screen_y = 0;
        assert!(ValidatedTimerPlan::from_timer_info(partial_screen_x, base_boot).is_err());

        let mut partial_screen_y = base_timer_info(WakeupMethod::WakeupByTouch);
        partial_screen_y.screen_x = 0;
        partial_screen_y.screen_y = 1000;
        assert!(ValidatedTimerPlan::from_timer_info(partial_screen_y, base_boot).is_err());
    }

    #[fuchsia::test]
    fn validated_timer_plan_computes_boot_targets_and_screen_bounds() {
        let base_boot = zx::BootInstant::from_nanos(10_000_000_000);
        let mut info = base_timer_info(WakeupMethod::WakeupBySwipeUp);
        info.num_events = 3;
        info.offset = 2_000_000_000;
        info.interval = 500_000_000;

        let default_plan =
            ValidatedTimerPlan::from_timer_info(info, base_boot).expect("valid plan");
        assert_eq!(default_plan.max_x, DEFAULT_SCREEN_MAX_X);
        assert_eq!(default_plan.max_y, DEFAULT_SCREEN_MAX_Y);
        assert_eq!(default_plan.last_offset_ns, 3_000_000_000);
        assert_eq!(default_plan.last_target_boot, zx::BootInstant::from_nanos(13_000_000_000));
        assert_eq!(
            default_plan.target_boots,
            vec![
                zx::BootInstant::from_nanos(12_000_000_000),
                zx::BootInstant::from_nanos(12_500_000_000),
                zx::BootInstant::from_nanos(13_000_000_000),
            ]
        );

        info.screen_x = 2;
        info.screen_y = 2;
        let min_screen_plan =
            ValidatedTimerPlan::from_timer_info(info, base_boot).expect("valid min screen plan");
        assert_eq!((min_screen_plan.max_x, min_screen_plan.max_y), (2, 2));

        info.screen_x = 1080;
        info.screen_y = 2400;
        let custom_screen_plan =
            ValidatedTimerPlan::from_timer_info(info, base_boot).expect("valid custom screen plan");
        assert_eq!((custom_screen_plan.max_x, custom_screen_plan.max_y), (1080, 2400));
    }

    #[fuchsia::test]
    fn power_schedule_anchor_delays_late_rtc_resume_probes_without_bursting() {
        let anchor = PowerScheduleAnchor {
            target_boot: zx::BootInstant::from_nanos(112_000_000_000),
            offset_ns: 12_000_000_000,
        };
        let pre_suspend_now = zx::BootInstant::from_nanos(100_010_000_000);

        // Matching post-wake touch schedule (12.300s >= 12.000s while anchor is still in future).
        let (matched, post_wake_offset_ns) =
            resolve_power_anchor(Some(anchor), 12_300_000_000, pre_suspend_now)
                .expect("should match pending future power schedule");
        assert_eq!(matched, anchor);
        assert_eq!(post_wake_offset_ns, 300_000_000);

        // Touch scheduled earlier than power wake (2.0s < 12.0s) must not anchor.
        assert_eq!(resolve_power_anchor(Some(anchor), 2_000_000_000, pre_suspend_now), None);

        // Expired power anchor (now >= anchor.target_boot) must not anchor standalone touch runs.
        assert_eq!(
            resolve_power_anchor(
                Some(anchor),
                12_300_000_000,
                zx::BootInstant::from_nanos(112_000_000_000),
            ),
            None
        );

        // Simulate da9188 1-second RTC waking +1.50s late at 113.500s (well past unanchored
        // 112.300s / 112.550s / 112.800s): probes must space cleanly from actual wake!
        let actual_wake_boot = zx::BootInstant::from_nanos(113_500_000_000);
        let anchored_targets = compute_anchored_touch_target_boots(
            actual_wake_boot,
            post_wake_offset_ns,
            250_000_000,
            3,
        );
        assert_eq!(
            anchored_targets,
            vec![
                zx::BootInstant::from_nanos(113_800_000_000),
                zx::BootInstant::from_nanos(114_050_000_000),
                zx::BootInstant::from_nanos(114_300_000_000),
            ]
        );
    }

    #[fuchsia::test]
    fn compute_touch_gesture_tap_defaults_alternation_and_right_edge_clamping() {
        let base_boot = zx::BootInstant::from_nanos(1_000);
        let mut info = base_timer_info(WakeupMethod::WakeupByTouch);
        info.num_events = 3;
        let plan = ValidatedTimerPlan::from_timer_info(info, base_boot).expect("valid plan");

        assert_eq!(
            compute_touch_gesture(&plan, TouchAction::Tap, 0),
            TouchGesture::Tap {
                x: 500,
                y: 500,
                hold_duration: zx::MonotonicDuration::from_millis(15),
            }
        );
        assert_eq!(
            compute_touch_gesture(&plan, TouchAction::Tap, 1),
            TouchGesture::Tap {
                x: 502,
                y: 500,
                hold_duration: zx::MonotonicDuration::from_millis(15),
            }
        );
        assert_eq!(
            compute_touch_gesture(&plan, TouchAction::Tap, 2),
            TouchGesture::Tap {
                x: 500,
                y: 500,
                hold_duration: zx::MonotonicDuration::from_millis(15),
            }
        );

        // Near right edge (1079 -> 1080) and custom hold duration.
        let mut near_edge = base_timer_info(WakeupMethod::WakeupByTouch);
        near_edge.num_events = 2;
        near_edge.screen_x = 1080;
        near_edge.screen_y = 2400;
        near_edge.coord_x = 1079;
        near_edge.coord_y = 5000;
        near_edge.dev_option = 42;
        let near_edge_plan =
            ValidatedTimerPlan::from_timer_info(near_edge, base_boot).expect("valid plan");

        assert_eq!(
            compute_touch_gesture(&near_edge_plan, TouchAction::Tap, 0),
            TouchGesture::Tap {
                x: 1079,
                y: 2400,
                hold_duration: zx::MonotonicDuration::from_millis(42),
            }
        );
        assert_eq!(
            compute_touch_gesture(&near_edge_plan, TouchAction::Tap, 1),
            TouchGesture::Tap {
                x: 1080,
                y: 2400,
                hold_duration: zx::MonotonicDuration::from_millis(42),
            }
        );

        // Exact right edge (coord_x == 1080 on screen_x == 1080) shifts inward to 1078 on odd index.
        let mut at_right_edge = base_timer_info(WakeupMethod::WakeupByTouch);
        at_right_edge.num_events = 2;
        at_right_edge.screen_x = 1080;
        at_right_edge.screen_y = 2400;
        at_right_edge.coord_x = 1080;
        at_right_edge.coord_y = 1200;
        let right_edge_plan =
            ValidatedTimerPlan::from_timer_info(at_right_edge, base_boot).expect("valid plan");

        assert_eq!(
            compute_touch_gesture(&right_edge_plan, TouchAction::Tap, 0),
            TouchGesture::Tap {
                x: 1080,
                y: 1200,
                hold_duration: zx::MonotonicDuration::from_millis(15),
            }
        );
        assert_eq!(
            compute_touch_gesture(&right_edge_plan, TouchAction::Tap, 1),
            TouchGesture::Tap {
                x: 1078,
                y: 1200,
                hold_duration: zx::MonotonicDuration::from_millis(15),
            }
        );
    }

    #[fuchsia::test]
    fn compute_touch_gesture_swipe_directions_oscillation_and_duration() {
        let base_boot = zx::BootInstant::from_nanos(1_000);

        let mut swipe_up = base_timer_info(WakeupMethod::WakeupBySwipeUp);
        swipe_up.screen_x = 1000;
        swipe_up.screen_y = 2000;
        swipe_up.num_events = 2;
        let up_plan = ValidatedTimerPlan::from_timer_info(swipe_up, base_boot).expect("valid plan");

        assert_eq!(
            compute_touch_gesture(&up_plan, TouchAction::Swipe(SwipeDirection::Up), 0),
            TouchGesture::Swipe {
                start: fmath::Vec_ { x: 500, y: 1600 },
                end: fmath::Vec_ { x: 500, y: 400 },
                duration_ns: 80_000_000,
                move_event_count: 10,
            }
        );
        // Odd index swaps start and end so periodic swipe trains oscillate in-bounds.
        assert_eq!(
            compute_touch_gesture(&up_plan, TouchAction::Swipe(SwipeDirection::Up), 1),
            TouchGesture::Swipe {
                start: fmath::Vec_ { x: 500, y: 400 },
                end: fmath::Vec_ { x: 500, y: 1600 },
                duration_ns: 80_000_000,
                move_event_count: 10,
            }
        );

        let swipe_down = base_timer_info(WakeupMethod::WakeupBySwipeDown);
        let down_plan =
            ValidatedTimerPlan::from_timer_info(swipe_down, base_boot).expect("valid plan");
        assert_eq!(
            compute_touch_gesture(&down_plan, TouchAction::Swipe(SwipeDirection::Down), 0),
            TouchGesture::Swipe {
                start: fmath::Vec_ { x: 500, y: 200 },
                end: fmath::Vec_ { x: 500, y: 800 },
                duration_ns: 80_000_000,
                move_event_count: 10,
            }
        );

        let mut swipe_left = base_timer_info(WakeupMethod::WakeupBySwipeLeft);
        swipe_left.dev_option = 8; // move_event_count = (8 / 8) -> clamped to 2
        let left_plan =
            ValidatedTimerPlan::from_timer_info(swipe_left, base_boot).expect("valid plan");
        assert_eq!(
            compute_touch_gesture(&left_plan, TouchAction::Swipe(SwipeDirection::Left), 0),
            TouchGesture::Swipe {
                start: fmath::Vec_ { x: 800, y: 500 },
                end: fmath::Vec_ { x: 200, y: 500 },
                duration_ns: 8_000_000,
                move_event_count: 2,
            }
        );

        let mut swipe_right = base_timer_info(WakeupMethod::WakeupBySwipeRight);
        swipe_right.coord_x = 0;
        swipe_right.coord_y = 300;
        swipe_right.dev_option = 1000; // move_event_count = (1000 / 8 = 125) -> clamped to 60
        let right_plan =
            ValidatedTimerPlan::from_timer_info(swipe_right, base_boot).expect("valid plan");
        assert_eq!(
            compute_touch_gesture(&right_plan, TouchAction::Swipe(SwipeDirection::Right), 0),
            TouchGesture::Swipe {
                start: fmath::Vec_ { x: 200, y: 300 },
                end: fmath::Vec_ { x: 800, y: 300 },
                duration_ns: 1_000_000_000,
                move_event_count: 60,
            }
        );
    }

    #[fuchsia::test]
    fn setup_rendezvous_records_exactly_one_outcome() {
        // 1. Worker completes before the caller gives up: the caller returns the worker's result.
        let rendezvous = Arc::new(SetupRendezvous::default());
        let notifier = SetupNotifier::new(Arc::clone(&rendezvous), Waiter::new());
        assert!(notifier.complete(Ok(())));
        assert_eq!(rendezvous.cancel(errno!(ETIMEDOUT)), Ok(()));

        // 2. Caller gives up before the worker completes: the worker loses and drops its proxies.
        let rendezvous = Arc::new(SetupRendezvous::default());
        let notifier = SetupNotifier::new(Arc::clone(&rendezvous), Waiter::new());
        assert_eq!(rendezvous.cancel(errno!(EINTR)), Err(errno!(EINTR)));
        assert!(!notifier.complete(Ok(())));
        assert_eq!(rendezvous.worker_result(), None);

        // 3. Worker drops its notifier without completing: the caller returns EIO.
        let rendezvous = Arc::new(SetupRendezvous::default());
        drop(SetupNotifier::new(Arc::clone(&rendezvous), Waiter::new()));
        assert_eq!(rendezvous.cancel(errno!(ETIMEDOUT)), Err(errno!(EIO)));
    }

    #[fuchsia::test]
    async fn setup_rendezvous_wait_returns_completed_result_or_cancels_on_timeout() {
        spawn_kernel_and_run(async |current_task| {
            let waiter = Waiter::new();
            let rendezvous = Arc::new(SetupRendezvous::default());
            let notifier = SetupNotifier::new(Arc::clone(&rendezvous), waiter.clone());
            assert!(notifier.complete(Ok(())));
            assert_eq!(
                rendezvous.wait(&waiter, current_task, zx::MonotonicInstant::INFINITE),
                Ok(())
            );

            let waiter = Waiter::new();
            let rendezvous = Arc::new(SetupRendezvous::default());
            let notifier = SetupNotifier::new(Arc::clone(&rendezvous), waiter.clone());
            assert_eq!(
                rendezvous.wait(&waiter, current_task, zx::MonotonicInstant::ZERO),
                Err(errno!(ETIMEDOUT))
            );
            assert!(!notifier.complete(Ok(())));
        })
        .await;
    }

    #[fuchsia::test]
    async fn ioctl_returns_expected_errnos_for_invalid_requests() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new(kernel);
            let file = anon_test_file(current_task, Box::new(device), OpenFlags::RDWR);

            let how_many_cmd = 2u32 << _IOC_NRSHIFT;
            let cancel_cmd = 3u32 << _IOC_NRSHIFT;
            let wakeup_test_cmd = 4u32 << _IOC_NRSHIFT;
            let unknown_cmd = 9u32 << _IOC_NRSHIFT;

            assert_eq!(
                file.ioctl(current_task, how_many_cmd, SyscallArg::default()),
                Err(errno!(ENOSYS))
            );
            assert_eq!(
                file.ioctl(current_task, cancel_cmd, SyscallArg::default()),
                Err(errno!(ENOSYS))
            );
            assert_eq!(
                file.ioctl(current_task, wakeup_test_cmd, SyscallArg::default()),
                Err(errno!(ENOSYS))
            );
            assert_eq!(
                file.ioctl(current_task, unknown_cmd, SyscallArg::default()),
                Err(errno!(ENOTTY))
            );
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, SyscallArg::from(0u64)),
                Err(errno!(EFAULT))
            );

            let user_addr = map_memory(current_task, UserAddress::default(), 4096);
            let mut invalid_method = base_timer_info(WakeupMethod::PowerButton);
            invalid_method.method = 42;
            UserMemoryWriter::new(current_task, user_addr)
                .write(&encode_timer_info(&invalid_method));
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(EINVAL))
            );

            let mut zero_events = base_timer_info(WakeupMethod::WakeupByTouch);
            zero_events.num_events = 0;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&zero_events));
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(EINVAL))
            );

            let mut tiny_screen = base_timer_info(WakeupMethod::WakeupBySwipeUp);
            tiny_screen.screen_x = 1;
            tiny_screen.screen_y = 1000;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&tiny_screen));
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(EINVAL))
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn ioctl_wakeup_by_touch_registers_before_return_and_delivers_alternating_taps() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let registered = Arc::new(AtomicBool::new(false));
        let registered_for_server = Arc::clone(&registered);
        let (done_tx, done_rx) = oneshot::channel::<Vec<Vec<(u32, i64, i64)>>>();

        let server_fut = async move {
            let request = registry_stream.next().await.expect("register request").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } = request
            else {
                panic!("expected RegisterTouchScreen, got {request:?}");
            };
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1000, max_y: 1000 })
            );
            let mut touch_stream = payload.device.expect("touch server_end").into_stream();
            registered_for_server.store(true, Ordering::SeqCst);
            responder.send().expect("send RegisterTouchScreen reply");

            let mut observed_reports = Vec::new();
            for _ in 0..4 {
                let touch_request =
                    touch_stream.next().await.expect("touch event").expect("fidl ok");
                let futinput::TouchScreenRequest::SimulateTouchEvent { report, responder } =
                    touch_request
                else {
                    panic!("expected SimulateTouchEvent, got {touch_request:?}");
                };
                responder.send().expect("send SimulateTouchEvent reply");
                let contacts = report
                    .contacts
                    .expect("contacts field")
                    .into_iter()
                    .map(|contact| {
                        (
                            contact.contact_id.expect("id"),
                            contact.position_x.expect("x"),
                            contact.position_y.expect("y"),
                        )
                    })
                    .collect::<Vec<_>>();
                observed_reports.push(contacts);
            }

            done_tx.send(observed_reports).expect("send observed reports");
            // Cached proxy remains open while WakeupTestDevice lives and closes when dropped.
            assert!(touch_stream.next().await.is_none());
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::WakeupByTouch);
            info.num_events = 2;
            info.offset = 5_000_000;
            info.interval = 5_000_000;
            info.dev_option = 1;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));

            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            // Virtual touchscreen registration must have completed before ioctl returned.
            assert!(registered.load(Ordering::SeqCst));

            done_rx.await.expect("receive observed reports")
        });

        let (_, observed_reports) = futures::join!(server_fut, kernel_fut);
        assert_eq!(
            observed_reports,
            vec![vec![(1, 500, 500)], vec![], vec![(1, 502, 500)], vec![],]
        );
    }

    #[fuchsia::test]
    async fn ioctl_wakeup_by_swipe_up_delivers_expected_swipe() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let (done_tx, done_rx) = oneshot::channel::<futinput::TouchScreenSimulateSwipeRequest>();

        let server_fut = async move {
            let request = registry_stream.next().await.expect("register request").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } = request
            else {
                panic!("expected RegisterTouchScreen, got {request:?}");
            };
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1080, max_y: 2400 })
            );
            let mut touch_stream = payload.device.expect("touch server_end").into_stream();
            responder.send().expect("send RegisterTouchScreen reply");

            let swipe_request = touch_stream.next().await.expect("swipe event").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateSwipe { payload, responder } = swipe_request
            else {
                panic!("expected SimulateSwipe, got {swipe_request:?}");
            };
            responder.send().expect("send SimulateSwipe reply");
            done_tx.send(payload).expect("send swipe payload");
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::WakeupBySwipeUp);
            info.offset = 5_000_000;
            info.screen_x = 1080;
            info.screen_y = 2400;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));

            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            done_rx.await.expect("receive swipe payload")
        });

        let (_, payload) = futures::join!(server_fut, kernel_fut);
        assert_eq!(payload.start_location, Some(fmath::Vec_ { x: 540, y: 1920 }));
        assert_eq!(payload.end_location, Some(fmath::Vec_ { x: 540, y: 480 }));
        assert_eq!(payload.duration, Some(80_000_000));
        assert_eq!(payload.move_event_count, Some(10));
    }

    #[fuchsia::test]
    async fn ioctl_reuses_cached_touchscreen_across_consecutive_iterations() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let (first_done_tx, first_done_rx) =
            oneshot::channel::<futinput::TouchScreenSimulateSwipeRequest>();
        let (second_done_tx, second_done_rx) =
            oneshot::channel::<futinput::TouchScreenSimulateSwipeRequest>();

        let server_fut = async move {
            // Only ONE RegisterTouchScreen call should occur across both ioctls.
            let request =
                registry_stream.next().await.expect("initial register request").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } = request
            else {
                panic!("expected RegisterTouchScreen, got {request:?}");
            };
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1080, max_y: 2400 })
            );
            let mut touch_stream = payload.device.expect("touch server_end").into_stream();
            responder.send().expect("send RegisterTouchScreen reply");

            let first_swipe = touch_stream.next().await.expect("first swipe").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateSwipe { payload: first_payload, responder } =
                first_swipe
            else {
                panic!("expected first SimulateSwipe, got {first_swipe:?}");
            };
            responder.send().expect("send first SimulateSwipe reply");
            first_done_tx.send(first_payload).expect("send first swipe payload");

            let second_swipe = touch_stream.next().await.expect("second swipe").expect("fidl ok");
            let futinput::TouchScreenRequest::SimulateSwipe { payload: second_payload, responder } =
                second_swipe
            else {
                panic!("expected second SimulateSwipe, got {second_swipe:?}");
            };
            responder.send().expect("send second SimulateSwipe reply");
            second_done_tx.send(second_payload).expect("send second swipe payload");

            assert!(registry_stream.next().await.is_none());
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let first_file =
                anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let second_file = anon_test_file(current_task, Box::new(device), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut swipe_info = base_timer_info(WakeupMethod::WakeupBySwipeLeft);
            swipe_info.offset = 5_000_000;
            swipe_info.screen_x = 1080;
            swipe_info.screen_y = 2400;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&swipe_info));

            assert_eq!(
                first_file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Ok(SUCCESS)
            );
            let first_payload = first_done_rx.await.expect("receive first swipe");

            assert_eq!(
                second_file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Ok(SUCCESS)
            );
            let second_payload = second_done_rx.await.expect("receive second swipe");
            (first_payload, second_payload)
        });

        let (_, (first_payload, second_payload)) = futures::join!(server_fut, kernel_fut);
        let expected_start = Some(fmath::Vec_ { x: 864, y: 1200 });
        let expected_end = Some(fmath::Vec_ { x: 216, y: 1200 });
        assert_eq!(first_payload.start_location, expected_start);
        assert_eq!(first_payload.end_location, expected_end);
        assert_eq!(second_payload.start_location, expected_start);
        assert_eq!(second_payload.end_location, expected_end);
    }

    #[fuchsia::test]
    async fn ioctl_reregisters_touchscreen_on_dimension_change_or_closed_channel() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let (iteration_0_done_tx, iteration_0_done_rx) = oneshot::channel::<()>();
        let (iteration_1_done_tx, iteration_1_done_rx) = oneshot::channel::<()>();
        let (iteration_2_done_tx, iteration_2_done_rx) = oneshot::channel::<()>();
        let (iteration_3_done_tx, iteration_3_done_rx) = oneshot::channel::<()>();
        let (verify_done_tx, verify_done_rx) = oneshot::channel::<()>();

        let server_fut = async move {
            // Iteration 0: default (1000, 1000) registration.
            let register_request_0 =
                registry_stream.next().await.expect("register #0").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                register_request_0
            else {
                panic!("expected RegisterTouchScreen #0, got {register_request_0:?}");
            };
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1000, max_y: 1000 })
            );
            let mut touch_stream_0 = payload.device.expect("touch #0").into_stream();
            responder.send().expect("reply register #0");

            for _ in 0..2 {
                let touch_request =
                    touch_stream_0.next().await.expect("iteration 0 event").expect("fidl ok");
                let futinput::TouchScreenRequest::SimulateTouchEvent { responder, .. } =
                    touch_request
                else {
                    panic!("expected SimulateTouchEvent, got {touch_request:?}");
                };
                responder.send().expect("reply iteration 0 event");
            }
            iteration_0_done_tx.send(()).expect("signal iteration 0 done");

            // Iteration 1: screen dimensions change to (1080, 2400) -> triggers registration #1
            // and replaces cached proxy (closing touch_stream_0).
            let register_request_1 =
                registry_stream.next().await.expect("register #1").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                register_request_1
            else {
                panic!("expected RegisterTouchScreen #1, got {register_request_1:?}");
            };
            assert_eq!(
                payload.display_dimensions,
                Some(futinput::DisplayDimensions { min_x: 0, min_y: 0, max_x: 1080, max_y: 2400 })
            );
            let mut touch_stream_1 = payload.device.expect("touch #1").into_stream();
            responder.send().expect("reply register #1");
            assert!(touch_stream_0.next().await.is_none());

            for _ in 0..2 {
                let touch_request =
                    touch_stream_1.next().await.expect("iteration 1 event").expect("fidl ok");
                let futinput::TouchScreenRequest::SimulateTouchEvent { responder, .. } =
                    touch_request
                else {
                    panic!("expected SimulateTouchEvent, got {touch_request:?}");
                };
                responder.send().expect("reply iteration 1 event");
            }
            // Drop server stream #1 so cached proxy becomes closed before Iteration 2.
            drop(touch_stream_1);
            iteration_1_done_tx.send(()).expect("signal iteration 1 done");

            // Iteration 2: same (1080, 2400) dimensions, but cached proxy is closed -> triggers
            // fresh registration #2. Close stream #2 when the first touch report arrives so
            // simulate_tap encounters peer-closed mid-gesture and evicts cached_touchscreen.
            let register_request_2 =
                registry_stream.next().await.expect("register #2").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                register_request_2
            else {
                panic!("expected RegisterTouchScreen #2, got {register_request_2:?}");
            };
            let mut touch_stream_2 = payload.device.expect("touch #2").into_stream();
            responder.send().expect("reply register #2");
            let aborted_request =
                touch_stream_2.next().await.expect("iteration 2 down event").expect("fidl ok");
            drop(aborted_request);
            drop(touch_stream_2);
            iteration_2_done_tx.send(()).expect("signal iteration 2 done");

            // Iteration 3: subsequent ioctl cleanly re-registers (#3) and delivers events.
            let register_request_3 =
                registry_stream.next().await.expect("register #3").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                register_request_3
            else {
                panic!("expected RegisterTouchScreen #3, got {register_request_3:?}");
            };
            let mut touch_stream_3 = payload.device.expect("touch #3").into_stream();
            responder.send().expect("reply register #3");

            for _ in 0..2 {
                let touch_request =
                    touch_stream_3.next().await.expect("iteration 3 event").expect("fidl ok");
                let futinput::TouchScreenRequest::SimulateTouchEvent { responder, .. } =
                    touch_request
                else {
                    panic!("expected SimulateTouchEvent, got {touch_request:?}");
                };
                responder.send().expect("reply iteration 3 event");
            }
            iteration_3_done_tx.send(()).expect("signal iteration 3 done");
            // Keep touch_stream_3 open until kernel_fut verifies has_open_cached_touchscreen().
            verify_done_rx.await.expect("wait verify done");
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::WakeupByTouch);
            info.offset = 5_000_000;
            info.dev_option = 1;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));
            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            iteration_0_done_rx.await.expect("wait iteration 0");

            info.screen_x = 1080;
            info.screen_y = 2400;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));
            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            iteration_1_done_rx.await.expect("wait iteration 1");

            // Same (1080, 2400) dimensions after server closed stream #1; stream #2 closes mid-tap.
            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            iteration_2_done_rx.await.expect("wait iteration 2");

            // Iteration 3 recovers cleanly with fresh registration #3 (evicting closed proxy #2).
            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            iteration_3_done_rx.await.expect("wait iteration 3");
            assert!(device.has_open_cached_touchscreen().await);
            verify_done_tx.send(()).expect("signal verify done");
        });

        futures::join!(server_fut, kernel_fut);
    }

    #[fuchsia::test]
    async fn ioctl_power_button_registers_distinct_proxy_per_scheduled_event() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let scheduled_before_return = Arc::new(AtomicBool::new(false));
        let scheduled_for_server = Arc::clone(&scheduled_before_return);
        let (done_tx, done_rx) = oneshot::channel::<(Vec<i64>, Vec<zx::BootInstant>, i64)>();
        let keepalive = zx::BootDuration::from_millis(25);

        let server_fut = async move {
            let mut delays = Vec::new();
            let mut active_streams = Vec::new();
            let mut first_register_reply_boot = None;
            let mut second_schedule_recv_boot = None;
            for index in 0..2 {
                let register_request =
                    registry_stream.next().await.expect("register media button").expect("fidl ok");
                let futinput::RegistryRequest::RegisterMediaButtonsDevice { payload, responder } =
                    register_request
                else {
                    panic!("expected RegisterMediaButtonsDevice, got {register_request:?}");
                };
                let mut button_stream = payload.device.expect("button server_end").into_stream();
                if index == 0 {
                    first_register_reply_boot = Some(zx::BootInstant::get());
                }
                responder.send().expect("send RegisterMediaButtonsDevice reply");

                let schedule_request =
                    button_stream.next().await.expect("schedule press").expect("fidl ok");
                if index == 1 {
                    second_schedule_recv_boot = Some(zx::BootInstant::get());
                }
                let futinput::MediaButtonsDeviceRequest::ScheduleSimulateButtonPress {
                    payload,
                    responder,
                } = schedule_request
                else {
                    panic!("expected ScheduleSimulateButtonPress, got {schedule_request:?}");
                };
                assert_eq!(payload.button, Some(ConsumerControlButton::Power));
                delays.push(payload.delay.expect("delay present"));
                if index == 1 {
                    scheduled_for_server.store(true, Ordering::SeqCst);
                }
                responder.send().expect("send ScheduleSimulateButtonPress reply");
                active_streams.push(button_stream);
            }

            let server_span_ns = (second_schedule_recv_boot.expect("second schedule timestamp")
                - first_register_reply_boot.expect("first register reply timestamp"))
            .into_nanos();

            // Both proxies remain open until keepalive expires, then close cleanly.
            let mut close_timestamps = Vec::new();
            for mut stream in active_streams {
                assert!(stream.next().await.is_none());
                close_timestamps.push(zx::BootInstant::get());
            }
            done_tx
                .send((delays, close_timestamps, server_span_ns))
                .expect("send scheduled delays");
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device =
                WakeupTestDevice::new_for_test(kernel, registry_tx, TEST_SETUP_TIMEOUT, keepalive);
            let file = anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::PowerButton);
            info.num_events = 2;
            // Multi-second headroom prevents startup and scheduling latency from outrunning the
            // initial offset or inter-event interval under contended software-emulated environments
            // such as QEMU-riscv64.
            info.offset = POWER_BUTTON_TEST_OFFSET_NS;
            info.interval = POWER_BUTTON_TEST_INTERVAL_NS;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));

            assert_eq!(file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()), Ok(SUCCESS));
            assert!(scheduled_before_return.load(Ordering::SeqCst));
            let recorded_anchor =
                device.commands.state.lock().last_power_schedule.expect("power anchor recorded");

            let (delays, close_timestamps, server_span_ns) =
                done_rx.await.expect("receive scheduled delays");
            (recorded_anchor, delays, close_timestamps, server_span_ns)
        });

        let (_, (recorded_anchor, delays, close_timestamps, server_span_ns)) =
            futures::join!(server_fut, kernel_fut);
        assert_eq!(delays.len(), 2);
        assert!(delays[0] > 0, "first alarm delay must be positive: {delays:?}");
        assert!(
            server_span_ns < POWER_BUTTON_TEST_INTERVAL_NS,
            "power button setup spanned {server_span_ns} ns >= interval {POWER_BUTTON_TEST_INTERVAL_NS} ns"
        );
        let delay_delta_ns = delays[1] - delays[0];
        assert!(
            delay_delta_ns <= POWER_BUTTON_TEST_INTERVAL_NS,
            "delay delta {delay_delta_ns} ns exceeded interval {POWER_BUTTON_TEST_INTERVAL_NS} ns: {delays:?}"
        );
        assert!(
            delay_delta_ns >= POWER_BUTTON_TEST_INTERVAL_NS - server_span_ns,
            "delay delta {delay_delta_ns} ns below interval - server_span_ns ({POWER_BUTTON_TEST_INTERVAL_NS} - {server_span_ns}): {delays:?}"
        );
        assert_eq!(close_timestamps.len(), 2);
        for closed_at in close_timestamps {
            assert!(
                closed_at >= recorded_anchor.target_boot + keepalive,
                "media_button proxy closed early at {closed_at:?} before {:?} + {keepalive:?}",
                recorded_anchor.target_boot
            );
        }
    }

    #[fuchsia::test]
    async fn ioctl_power_button_followed_by_touch_anchors_probes_to_power_wake() {
        let (registry_tx, mut registry_stream) = create_test_registry();
        let (done_tx, done_rx) = oneshot::channel::<Vec<zx::BootInstant>>();

        let server_fut = async move {
            // 1. PowerButton registration + alarm schedule.
            let power_register_request =
                registry_stream.next().await.expect("power register").expect("fidl ok");
            let futinput::RegistryRequest::RegisterMediaButtonsDevice { payload, responder } =
                power_register_request
            else {
                panic!("expected RegisterMediaButtonsDevice, got {power_register_request:?}");
            };
            let mut button_stream = payload.device.expect("button server_end").into_stream();
            responder.send().expect("send RegisterMediaButtonsDevice reply");

            let schedule_request =
                button_stream.next().await.expect("schedule button").expect("fidl ok");
            let futinput::MediaButtonsDeviceRequest::ScheduleSimulateButtonPress {
                responder, ..
            } = schedule_request
            else {
                panic!("expected ScheduleSimulateButtonPress, got {schedule_request:?}");
            };
            responder.send().expect("send ScheduleSimulateButtonPress reply");

            // 2. Subsequent WakeupByTouch registration + 2 alternating Down+Up tap pairs.
            let touch_register_request =
                registry_stream.next().await.expect("touch register").expect("fidl ok");
            let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                touch_register_request
            else {
                panic!("expected RegisterTouchScreen, got {touch_register_request:?}");
            };
            let mut touch_stream = payload.device.expect("touch server_end").into_stream();
            responder.send().expect("send RegisterTouchScreen reply");

            let mut down_timestamps = Vec::new();
            for step in 0..4 {
                let touch_request =
                    touch_stream.next().await.expect("touch event").expect("fidl ok");
                let futinput::TouchScreenRequest::SimulateTouchEvent { report, responder } =
                    touch_request
                else {
                    panic!("expected SimulateTouchEvent, got {touch_request:?}");
                };
                if step % 2 == 0 {
                    assert_eq!(report.contacts.as_ref().map(Vec::len), Some(1));
                    down_timestamps.push(zx::BootInstant::get());
                } else {
                    assert_eq!(report.contacts.as_ref().map(Vec::len), Some(0));
                }
                responder.send().expect("send SimulateTouchEvent reply");
            }

            done_tx.send(down_timestamps).expect("send down timestamps");
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            // Open two separate file handles sharing the same WakeupTestDevice (mirroring two
            // consecutive `wakeup_press` invocations against `/dev/wakeup_test0`).
            let power_file =
                anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let touch_file =
                anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut power_info = base_timer_info(WakeupMethod::PowerButton);
            power_info.offset = 30_000_000; // 30 ms
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&power_info));
            assert_eq!(
                power_file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Ok(SUCCESS)
            );

            let recorded_anchor =
                device.commands.state.lock().last_power_schedule.expect("power anchor recorded");
            assert_eq!(recorded_anchor.offset_ns, 30_000_000);

            // Plant an anchor 5 s in the future with offset_ns = 10 ms so an unanchored
            // touch_info (offset = 25 ms) would fire ~4.975 s early and fail deterministically,
            // independent of power_file.ioctl round-trip latency, while providing multi-second
            // headroom for touch_file.ioctl setup under contended software-emulated environments
            // such as QEMU-riscv64.
            let planted_anchor = PowerScheduleAnchor {
                target_boot: zx::BootInstant::get() + PLANTED_ANCHOR_LEAD_TIME,
                offset_ns: 10_000_000,
            };
            device.commands.state.lock().last_power_schedule = Some(planted_anchor);

            let mut touch_info = base_timer_info(WakeupMethod::WakeupByTouch);
            touch_info.num_events = 2;
            touch_info.offset = 25_000_000; // +15 ms after power wake
            touch_info.interval = 50_000_000; // 50 ms spacing
            touch_info.dev_option = 1;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&touch_info));
            assert_eq!(
                touch_file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Ok(SUCCESS)
            );
            assert!(
                zx::BootInstant::get() < planted_anchor.target_boot,
                "touch ioctl completed after planted power target {:?}; anchoring path was not exercised",
                planted_anchor.target_boot
            );

            let down_timestamps = done_rx.await.expect("receive down timestamps");
            (planted_anchor, down_timestamps)
        });

        let (_, (anchor, down_timestamps)) = futures::join!(server_fut, kernel_fut);
        assert_eq!(down_timestamps.len(), 2);
        assert!(
            down_timestamps[0] >= anchor.target_boot + zx::BootDuration::from_millis(15),
            "first touch {:?} must fire at least 15ms after power target {:?}",
            down_timestamps[0],
            anchor.target_boot
        );
        assert!(
            down_timestamps[1] >= anchor.target_boot + zx::BootDuration::from_millis(65),
            "second touch {:?} must fire at least 65ms after power target {:?}",
            down_timestamps[1],
            anchor.target_boot
        );
        assert!(
            down_timestamps[1] > down_timestamps[0],
            "second touch {:?} must follow first touch {:?}",
            down_timestamps[1],
            down_timestamps[0]
        );
    }

    #[fuchsia::test]
    async fn ioctl_set_timers_does_not_require_caller_executor() {
        // Real Starnix user threads have no fuchsia_async executor, unlike the
        // `spawn_kernel_and_run` callback, so issue the ioctl from a synchronous task.
        let (closed_registry_tx, closed_registry_stream) = create_test_registry();
        drop(closed_registry_stream);

        spawn_kernel_and_run_sync(move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                closed_registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            for method in [WakeupMethod::WakeupByTouch, WakeupMethod::PowerButton] {
                let info = base_timer_info(method);
                UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));
                assert_eq!(
                    file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                    Err(errno!(EIO))
                );
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn ioctl_closed_and_hung_registry_return_errors_without_phantom_events() {
        // 1. Closed Registry channel -> EIO for both Touch and PowerButton.
        let (closed_registry_tx, closed_registry_stream) = create_test_registry();
        drop(closed_registry_stream);

        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                closed_registry_tx,
                TEST_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let touch_info = base_timer_info(WakeupMethod::WakeupByTouch);
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&touch_info));
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(EIO))
            );
            assert!(!device.has_open_cached_touchscreen().await);

            let power_info = base_timer_info(WakeupMethod::PowerButton);
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&power_info));
            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(EIO))
            );
            assert!(device.commands.state.lock().last_power_schedule.is_none());
        })
        .await;

        // 2. Hung Registry channel (Touch) -> ETIMEDOUT and zero phantom touch events / no cached proxy.
        let (hung_registry_tx, mut hung_registry_stream) = create_test_registry();
        let (timed_out_tx, timed_out_rx) = oneshot::channel::<()>();
        let (server_done_tx, server_done_rx) = oneshot::channel::<()>();

        let server_fut = async move {
            match select(hung_registry_stream.next(), timed_out_rx).await {
                Either::Left((request, timed_out_rx)) => {
                    let request = request.expect("register request").expect("fidl ok");
                    let futinput::RegistryRequest::RegisterTouchScreen { payload, responder } =
                        request
                    else {
                        panic!("expected RegisterTouchScreen, got {request:?}");
                    };
                    let mut touch_stream = payload.device.expect("touch server_end").into_stream();

                    // Hold registration open until ioctl has timed out.
                    timed_out_rx.await.expect("wait for ioctl timeout");
                    let _ = responder.send();

                    // Client future must have cancelled/exited without caching the proxy or
                    // injecting events.
                    assert!(touch_stream.next().await.is_none());
                    server_done_tx.send(()).expect("signal server done");
                }
                Either::Right((timed_out, _)) => {
                    // ioctl timed out before RegisterTouchScreen was sent; kernel_fut verifies
                    // that no touchscreen proxy was cached.
                    timed_out.expect("wait for ioctl timeout");
                    server_done_tx.send(()).expect("signal server done");
                }
            }
        };

        let kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                hung_registry_tx,
                HUNG_REGISTRY_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::WakeupByTouch);
            info.offset = 0;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));

            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(ETIMEDOUT))
            );
            timed_out_tx.send(()).expect("notify server of timeout");
            server_done_rx.await.expect("wait for server verification");
            assert!(!device.has_open_cached_touchscreen().await);
        });

        futures::join!(server_fut, kernel_fut);

        // 3. Hung Registry channel (PowerButton) -> ETIMEDOUT, proxy dropped, no power anchor recorded.
        let (hung_power_registry_tx, mut hung_power_registry_stream) = create_test_registry();
        let (power_timed_out_tx, power_timed_out_rx) = oneshot::channel::<()>();
        let (power_server_done_tx, power_server_done_rx) = oneshot::channel::<()>();

        let power_server_fut = async move {
            match select(hung_power_registry_stream.next(), power_timed_out_rx).await {
                Either::Left((request, power_timed_out_rx)) => {
                    let request = request.expect("power register request").expect("fidl ok");
                    let futinput::RegistryRequest::RegisterMediaButtonsDevice {
                        payload,
                        responder,
                    } = request
                    else {
                        panic!("expected RegisterMediaButtonsDevice, got {request:?}");
                    };
                    let mut button_stream =
                        payload.device.expect("button server_end").into_stream();

                    power_timed_out_rx.await.expect("wait for power ioctl timeout");
                    let _ = responder.send();

                    assert!(button_stream.next().await.is_none());
                    power_server_done_tx.send(()).expect("signal power server done");
                }
                Either::Right((power_timed_out, _)) => {
                    // ioctl timed out before RegisterMediaButtonsDevice was sent;
                    // power_kernel_fut verifies that no power anchor was recorded.
                    power_timed_out.expect("wait for power ioctl timeout");
                    power_server_done_tx.send(()).expect("signal power server done");
                }
            }
        };

        let power_kernel_fut = spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let device = WakeupTestDevice::new_for_test(
                kernel,
                hung_power_registry_tx,
                HUNG_REGISTRY_SETUP_TIMEOUT,
                zx::BootDuration::from_millis(5),
            );
            let file = anon_test_file(current_task, Box::new(device.clone()), OpenFlags::RDWR);
            let user_addr = map_memory(current_task, UserAddress::default(), 4096);

            let mut info = base_timer_info(WakeupMethod::PowerButton);
            info.offset = 0;
            UserMemoryWriter::new(current_task, user_addr).write(&encode_timer_info(&info));

            assert_eq!(
                file.ioctl(current_task, SET_TIMERS_IOCTL, user_addr.into()),
                Err(errno!(ETIMEDOUT))
            );
            power_timed_out_tx.send(()).expect("notify power server of timeout");
            power_server_done_rx.await.expect("wait for power server verification");
            assert!(device.commands.state.lock().last_power_schedule.is_none());
        });

        futures::join!(power_server_fut, power_kernel_fut);
    }
}
