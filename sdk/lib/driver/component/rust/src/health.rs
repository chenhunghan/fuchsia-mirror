// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Support for running a driver's [`crate::Driver::health_check`] on behalf of the lifecycle
//! server (the per-level deadlines and the root dispatcher liveness check that precedes every
//! check), the [`HealthAlertReporter`] and [`HealthAlertLatch`] through which a driver proactively
//! reports faults, and the opt-in [`HealthCheckInspect`] helper that mirrors results into Inspect.

use std::sync::Arc;

use fdf::{AsAsyncDispatcherRef, DriverDispatcherRef};
use fidl_next::Server;
use fidl_next_fuchsia_driver_framework::{
    Driver as DriverProtocol, HealthAlert, HealthCheckError, HealthCheckRequest,
    HealthCheckResponse, HealthCondition, HealthDiagnosticItem, HealthDiagnosticValue,
    HealthStatus,
};
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use log::{debug, warn};

use crate::Driver;
use crate::server::Transport;

/// The result of a single health check.
pub type HealthCheckResult = Result<HealthCheckResponse, HealthCheckError>;

// Per-level health check deadlines. These mirror the deadlines driver-host enforces for each
// `HealthCheckRequest`; the server adds `HEALTH_CHECK_TIMEOUT_MARGIN` so the host's own TIMEOUT is
// reported first, and the margin only bounds how long a hung check (or wedged root dispatcher) can
// keep its future alive on the driver side.
const PING_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(250);
const FAST_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);
const DEEP_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(120);
const HEALTH_CHECK_TIMEOUT_MARGIN: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

/// Returns how long the server lets a check at `level` run before answering `TIMEOUT` itself.
fn health_check_timeout(level: HealthCheckRequest) -> zx::MonotonicDuration {
    let deadline = match level {
        HealthCheckRequest::Ping => PING_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Fast => FAST_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Deep => DEEP_HEALTH_CHECK_DEADLINE,
    };
    deadline + HEALTH_CHECK_TIMEOUT_MARGIN
}

/// Confirms that `root_dispatcher` is still processing tasks.
///
/// The lifecycle server runs on a dedicated fuchsia-async thread, not the root dispatcher, so
/// unlike the C++ server (whose hook runs on the root dispatcher and therefore proves its liveness
/// implicitly) it must ping the dispatcher explicitly. If the dispatcher is wedged this never
/// completes, and the caller's deadline turns that into a `TIMEOUT`.
async fn ping_root_dispatcher(
    root_dispatcher: DriverDispatcherRef<'static>,
) -> Result<(), HealthCheckError> {
    let (tx, rx) = oneshot::channel();
    root_dispatcher
        .post_task_sync(move |status| {
            if status.is_ok() {
                let _ = tx.send(());
            }
        })
        .map_err(|_| HealthCheckError::InvalidState)?;
    // The sender is dropped without sending if the dispatcher is shutting down.
    rx.await.map_err(|_| HealthCheckError::InvalidState)
}

/// Runs one health check to completion: confirms `root_dispatcher` is live, then invokes
/// [`Driver::health_check`], bounded by the per-level deadline.
pub(crate) async fn run_health_check<D: Driver>(
    driver: &D,
    root_dispatcher: DriverDispatcherRef<'static>,
    request: HealthCheckRequest,
) -> HealthCheckResult {
    let timeout = health_check_timeout(request);
    let check = async move {
        ping_root_dispatcher(root_dispatcher).await?;
        driver.health_check(request).await
    };
    fuchsia_async::TimeoutExt::on_timeout(check, timeout, || Err(HealthCheckError::Timeout)).await
}

/// The maximum number of health alerts that can be queued in a standalone test receiver created by
/// [`HealthAlertReporter::new_for_testing`].
const HEALTH_ALERT_QUEUE_CAPACITY: usize = 64;

enum HealthAlertSink {
    /// Production sink: writes `OnHealthAlert` events synchronously into the `Driver` channel.
    Server(Server<DriverProtocol, Transport>),
    /// Standalone unit-test queue created by [`HealthAlertReporter::new_for_testing`].
    TestQueue { tx: mpsc::Sender<HealthAlert>, dropped: u64 },
    /// Disconnected sink (`Default::default` or after the driver has been asked to stop).
    Closed,
}

/// Lets a driver proactively report faults to the driver framework as `OnHealthAlert` events.
///
/// Obtained from [`crate::DriverContext::health_alert_reporter`] while the driver is starting.
/// Unlike the rest of the driver's lifecycle, alerts are driver-initiated and may come from any
/// task or thread, so the reporter is cheap to clone and drivers typically keep one in their own
/// state.
#[derive(Clone)]
pub struct HealthAlertReporter {
    sink: Arc<Mutex<HealthAlertSink>>,
}

impl HealthAlertReporter {
    /// Creates a reporter backed by the lifecycle server's `Driver` FIDL channel.
    pub(crate) fn new(server: Server<DriverProtocol, Transport>) -> Self {
        Self { sink: Arc::new(Mutex::new(HealthAlertSink::Server(server))) }
    }

    /// Disconnects the reporter so subsequent alerts are dropped.
    pub(crate) fn close(&self) {
        *self.sink.lock() = HealthAlertSink::Closed;
    }

    /// Creates a reporter and alert receiver pair for unit tests that construct a driver directly
    /// without a lifecycle server.
    pub fn new_for_testing() -> (Self, mpsc::Receiver<HealthAlert>) {
        // A futures mpsc channel holds `buffer + number of senders` messages. All clones share a
        // single sender, so this bounds the queue at exactly `HEALTH_ALERT_QUEUE_CAPACITY`.
        let (tx, rx) = mpsc::channel(HEALTH_ALERT_QUEUE_CAPACITY - 1);
        (Self { sink: Arc::new(Mutex::new(HealthAlertSink::TestQueue { tx, dropped: 0 })) }, rx)
    }

    /// Emits an `OnHealthAlert` event over the `Driver` channel to the driver framework.
    ///
    /// This never blocks: on a running driver the event is written synchronously to the underlying
    /// [`fdf_fidl::DriverChannel`] via [`fidl_next::SendFuture::send_immediately`]; alerts
    /// reported after the driver has been asked to stop are dropped.
    ///
    /// Alerts are not mirrored into Inspect automatically; a driver that records its health there
    /// can pass `SelfFault` alerts to [`HealthCheckInspect::record_alert`] as well.
    pub fn report_health_alert(&self, alert: HealthAlert) {
        let mut sink = self.sink.lock();
        match &mut *sink {
            HealthAlertSink::Server(server) => {
                let server = server.clone();
                drop(sink);
                if let Err(e) = server.on_health_alert(alert).send_immediately() {
                    debug!("Dropping health alert on closed or failed driver channel: {e}");
                }
            }
            HealthAlertSink::TestQueue { tx, dropped } => {
                let Err(e) = tx.try_send(alert) else {
                    return;
                };
                if e.is_full() {
                    *dropped += 1;
                    let total_dropped = *dropped;
                    drop(sink);
                    if total_dropped.is_power_of_two() {
                        warn!("Health alert queue is full; dropped {total_dropped} alerts so far");
                    }
                } else {
                    drop(sink);
                    debug!("Dropping health alert because the receiver was dropped");
                }
            }
            HealthAlertSink::Closed => {
                drop(sink);
                debug!("Dropping health alert because the driver server has shut down");
            }
        }
    }
}

impl Default for HealthAlertReporter {
    /// Returns a disconnected reporter: every alert reported through it is dropped (logged at
    /// debug). Intended only for tests or placeholders; use
    /// [`crate::DriverContext::health_alert_reporter`] in a running driver or
    /// [`HealthAlertReporter::new_for_testing`] to observe alerts in a test.
    fn default() -> Self {
        Self { sink: Arc::new(Mutex::new(HealthAlertSink::Closed)) }
    }
}

/// Maximum number of active conditions stored in a [`HealthAlertLatch`] or merged into a
/// [`HealthCheckResponse`], matching the FIDL `vector<HealthCondition>:16` bound.
pub const MAX_HEALTH_CONDITIONS: usize = 16;

trait HealthAlertMirror: Send + Sync {
    fn record_self_fault(&self, alert: &HealthAlert);
}

impl HealthAlertMirror for HealthCheckInspect {
    fn record_self_fault(&self, alert: &HealthAlert) {
        self.record_alert(alert);
    }
}

struct LatchState {
    conditions: Vec<HealthCondition>,
    inspect: Option<Arc<dyn HealthAlertMirror>>,
}

struct HealthAlertLatchInner {
    reporter: HealthAlertReporter,
    state: Mutex<LatchState>,
}

/// Thread-safe helper that maintains a driver's active self-fault condition set and emits
/// edge-triggered `OnHealthAlert` ([`HealthAlert::SelfFault`]) transitions via
/// [`HealthAlertReporter`].
///
/// Obtained from [`crate::DriverContext::health_alert_latch`]. Cloning a `HealthAlertLatch` shares
/// the underlying condition table so a driver and any SDK wrappers (such as
/// `fdf_power::Suspendable`) can independently raise and clear distinct `reason_code` conditions
/// without overwriting each other's state in `driver_manager`.
#[derive(Clone, Default)]
pub struct HealthAlertLatch {
    inner: Arc<HealthAlertLatchInner>,
}

impl Default for HealthAlertLatchInner {
    fn default() -> Self {
        Self {
            reporter: HealthAlertReporter::default(),
            state: Mutex::new(LatchState { conditions: Vec::new(), inspect: None }),
        }
    }
}

impl HealthAlertLatch {
    /// Creates a new latch backed by `reporter`.
    pub fn new(reporter: HealthAlertReporter) -> Self {
        Self {
            inner: Arc::new(HealthAlertLatchInner {
                reporter,
                state: Mutex::new(LatchState { conditions: Vec::new(), inspect: None }),
            }),
        }
    }

    /// Creates a latch and alert receiver pair for standalone unit tests without a lifecycle
    /// server.
    pub fn new_for_testing() -> (Self, mpsc::Receiver<HealthAlert>) {
        let (reporter, rx) = HealthAlertReporter::new_for_testing();
        (Self::new(reporter), rx)
    }

    /// Registers a [`HealthCheckInspect`] recorder so every emitted [`HealthAlert::SelfFault`]
    /// transition (including alerts emitted by shared wrapper libraries) is mirrored to Inspect in
    /// lockstep with the `Driver` channel.
    pub fn set_inspect(&self, inspect: Arc<HealthCheckInspect>) {
        self.inner.state.lock().inspect = Some(inspect);
    }

    /// Inserts or updates an active self-fault condition keyed by `condition.reason_code`.
    ///
    /// - On the first raise of an active `reason_code`, `first_observed` and `last_observed`
    ///   default to the current boot instant if unset, and `occurrence_count` defaults to `1` if
    ///   unset or `0`.
    /// - On subsequent raises of an already-active `reason_code`, `first_observed` is preserved
    ///   from the initial raise of the episode, `last_observed` is updated (to
    ///   `condition.last_observed` if set, else now), and `occurrence_count` is incremented unless
    ///   a larger count is supplied.
    /// - An omitted `status` is normalized to [`HealthStatus::Unhealthy`]. Passing
    ///   [`HealthStatus::Ok`] clears `reason_code`.
    /// - An omitted or empty `reason_code` is normalized to `"UNKNOWN"`.
    /// - When [`MAX_HEALTH_CONDITIONS`] conditions are already active, an incoming
    ///   [`HealthStatus::Unhealthy`] condition evicts an active [`HealthStatus::Degraded`] one;
    ///   otherwise the new condition is dropped. Both cases log a warning naming the
    ///   `reason_code`.
    ///
    /// Emits a [`HealthAlert::SelfFault`] containing all active conditions when `reason_code` is
    /// newly added or when its `status` changes. Repeated raises at the same `status` update the
    /// in-memory entry without emitting redundant alerts.
    pub fn raise(&self, mut condition: HealthCondition) {
        let reason_code = match condition.reason_code.take() {
            Some(code) if !code.is_empty() => code,
            _ => "UNKNOWN".to_string(),
        };
        let status = condition.status.unwrap_or(HealthStatus::Unhealthy);
        if status == HealthStatus::Ok {
            self.clear(&reason_code);
            return;
        }

        let now = zx::BootInstant::get().into_nanos();
        let mut state = self.inner.state.lock();

        if let Some(existing) =
            state.conditions.iter_mut().find(|c| c.reason_code.as_deref() == Some(&reason_code))
        {
            let prev_status = existing.status.unwrap_or(HealthStatus::Unhealthy);
            let first_observed =
                existing.first_observed.or(condition.first_observed).unwrap_or(now);
            let last_observed = condition.last_observed.unwrap_or(now);
            let prev_count = existing.occurrence_count.unwrap_or(1);
            let next_count = match condition.occurrence_count {
                Some(c) if c > prev_count => c,
                _ => prev_count.saturating_add(1),
            };

            existing.status = Some(status);
            existing.first_observed = Some(first_observed);
            existing.last_observed = Some(last_observed);
            existing.occurrence_count = Some(next_count);
            if condition.message.is_some() {
                existing.message = condition.message;
            }
            if condition.diagnostics.is_some() {
                existing.diagnostics = condition.diagnostics;
            }

            if prev_status != status {
                self.emit_locked(&state);
            }
            return;
        }

        if state.conditions.len() >= MAX_HEALTH_CONDITIONS {
            if status != HealthStatus::Degraded
                && let Some(degraded_idx) =
                    state.conditions.iter().position(|c| c.status == Some(HealthStatus::Degraded))
            {
                let evicted = state.conditions.remove(degraded_idx);
                warn!(
                    "HealthAlertLatch: evicting DEGRADED condition {} to make room for UNHEALTHY \
                     condition {reason_code} (MAX_HEALTH_CONDITIONS)",
                    evicted.reason_code.as_deref().unwrap_or("UNKNOWN")
                );
            } else {
                warn!(
                    "HealthAlertLatch capacity ({MAX_HEALTH_CONDITIONS}) reached; dropping condition {reason_code}"
                );
                return;
            }
        }

        condition.reason_code = Some(reason_code);
        condition.status = Some(status);
        condition.first_observed = Some(condition.first_observed.unwrap_or(now));
        condition.last_observed = Some(condition.last_observed.unwrap_or(now));
        condition.occurrence_count =
            Some(condition.occurrence_count.filter(|&c| c > 0).unwrap_or(1));

        state.conditions.push(condition);
        self.emit_locked(&state);
    }

    /// Raises a [`HealthStatus::Degraded`] self-fault condition with the given `reason_code` and
    /// `message`.
    ///
    /// Convenience wrapper around [`Self::raise`] that fills in `first_observed`, `last_observed`,
    /// and `occurrence_count` automatically.
    pub fn raise_degraded(&self, reason_code: impl Into<String>, message: impl Into<String>) {
        self.raise(HealthCondition {
            status: Some(HealthStatus::Degraded),
            reason_code: Some(reason_code.into()),
            message: Some(message.into()),
            ..Default::default()
        });
    }

    /// Raises a [`HealthStatus::Unhealthy`] self-fault condition with the given `reason_code` and
    /// `message`.
    ///
    /// Convenience wrapper around [`Self::raise`] that fills in `first_observed`, `last_observed`,
    /// and `occurrence_count` automatically.
    pub fn raise_unhealthy(&self, reason_code: impl Into<String>, message: impl Into<String>) {
        self.raise(HealthCondition {
            status: Some(HealthStatus::Unhealthy),
            reason_code: Some(reason_code.into()),
            message: Some(message.into()),
            ..Default::default()
        });
    }

    /// Removes the condition with `reason_code` if active. An empty `reason_code` refers to the
    /// `"UNKNOWN"` condition, matching how [`Self::raise`] normalizes it.
    ///
    /// If a condition was removed, emits a [`HealthAlert::SelfFault`] with the remaining active
    /// conditions (or an empty response to report [`HealthStatus::Ok`] recovery when none remain).
    pub fn clear(&self, reason_code: &str) {
        let reason_code = if reason_code.is_empty() { "UNKNOWN" } else { reason_code };
        let mut state = self.inner.state.lock();
        let Some(idx) =
            state.conditions.iter().position(|c| c.reason_code.as_deref() == Some(reason_code))
        else {
            return;
        };
        state.conditions.remove(idx);
        self.emit_locked(&state);
    }

    /// Clears all active conditions. If at least one condition was active, emits a single recovery
    /// [`HealthAlert::SelfFault`] with an empty response ([`HealthStatus::Ok`]).
    pub fn clear_all(&self) {
        let mut state = self.inner.state.lock();
        if state.conditions.is_empty() {
            return;
        }
        state.conditions.clear();
        self.emit_locked(&state);
    }

    /// Returns a snapshot of all currently active conditions, suitable for returning from
    /// [`crate::Driver::health_check`] (`Ping`).
    pub fn snapshot(&self) -> Vec<HealthCondition> {
        self.inner.state.lock().conditions.clone()
    }

    /// Builds a [`HealthCheckResponse`] containing all currently active conditions (or `None` when
    /// no conditions are active, reporting [`HealthStatus::Ok`]), suitable for returning directly
    /// from [`crate::Driver::health_check`] on [`HealthCheckRequest::Ping`].
    pub fn to_response(&self) -> HealthCheckResponse {
        let state = self.inner.state.lock();
        HealthCheckResponse {
            diagnostics: None,
            conditions: (!state.conditions.is_empty()).then(|| state.conditions.clone()),
        }
    }

    /// Appends any active conditions whose `reason_code` is not already present in
    /// `response.conditions` into `response` (up to [`MAX_HEALTH_CONDITIONS`]). Once `response`
    /// is full, an active [`HealthStatus::Unhealthy`] condition replaces a
    /// [`HealthStatus::Degraded`] entry (preferring one this call merged over one the caller
    /// measured live) rather than being dropped, so merging never downgrades the overall status
    /// the response reports.
    pub fn merge_into(&self, response: &mut HealthCheckResponse) {
        let state = self.inner.state.lock();
        if state.conditions.is_empty() {
            return;
        }
        let dst = response.conditions.get_or_insert_with(Vec::new);
        let merged_begin = dst.len();
        for cond in &state.conditions {
            if dst.iter().any(|existing| existing.reason_code == cond.reason_code) {
                continue;
            }
            if dst.len() < MAX_HEALTH_CONDITIONS {
                dst.push(cond.clone());
                continue;
            }
            if cond.status.unwrap_or(HealthStatus::Unhealthy) != HealthStatus::Unhealthy {
                continue;
            }
            let is_degraded = |c: &HealthCondition| c.status == Some(HealthStatus::Degraded);
            let victim = dst[merged_begin..]
                .iter()
                .position(is_degraded)
                .map(|i| i + merged_begin)
                .or_else(|| dst[..merged_begin].iter().position(is_degraded));
            if let Some(idx) = victim {
                dst[idx] = cond.clone();
            }
        }
    }

    /// Returns the overall status derived from the active condition set
    /// (`Unhealthy > Degraded > Ok`).
    pub fn status(&self) -> HealthStatus {
        let state = self.inner.state.lock();
        overall_status(&state.conditions)
    }

    fn emit_locked(&self, state: &LatchState) {
        let response = HealthCheckResponse {
            diagnostics: None,
            conditions: (!state.conditions.is_empty()).then(|| state.conditions.clone()),
        };
        let alert = HealthAlert::SelfFault(response);
        if let Some(inspect) = &state.inspect {
            inspect.record_self_fault(&alert);
        }
        self.inner.reporter.report_health_alert(alert);
    }
}

/// Derives the overall [`HealthStatus`] (`Unhealthy > Degraded > Ok`) from `conditions`.
///
/// Any condition with [`HealthStatus::Unhealthy`] (or an omitted `status`) makes the overall
/// status [`HealthStatus::Unhealthy`]; otherwise any [`HealthStatus::Degraded`] condition makes it
/// [`HealthStatus::Degraded`]. Returns [`HealthStatus::Ok`] when `conditions` is empty.
pub fn overall_status(conditions: &[HealthCondition]) -> HealthStatus {
    let mut overall = HealthStatus::Ok;
    for condition in conditions {
        match condition.status.unwrap_or(HealthStatus::Unhealthy) {
            HealthStatus::Ok => {}
            HealthStatus::Degraded => overall = HealthStatus::Degraded,
            HealthStatus::Unhealthy => return HealthStatus::Unhealthy,
        }
    }
    overall
}

/// Constructs a string [`HealthDiagnosticItem`].
pub fn diag_str(key: impl Into<String>, val: impl Into<String>) -> HealthDiagnosticItem {
    HealthDiagnosticItem { key: key.into(), value: HealthDiagnosticValue::StringValue(val.into()) }
}

/// Constructs an unsigned 64-bit integer [`HealthDiagnosticItem`].
pub fn diag_uint(key: impl Into<String>, val: u64) -> HealthDiagnosticItem {
    HealthDiagnosticItem { key: key.into(), value: HealthDiagnosticValue::UintValue(val) }
}

/// Constructs a signed 64-bit integer [`HealthDiagnosticItem`].
pub fn diag_int(key: impl Into<String>, val: i64) -> HealthDiagnosticItem {
    HealthDiagnosticItem { key: key.into(), value: HealthDiagnosticValue::IntValue(val) }
}

/// Constructs a boolean [`HealthDiagnosticItem`].
pub fn diag_bool(key: impl Into<String>, val: bool) -> HealthDiagnosticItem {
    HealthDiagnosticItem { key: key.into(), value: HealthDiagnosticValue::BoolValue(val) }
}

/// Constructs a boot-timeline timestamp [`HealthDiagnosticItem`] (in nanoseconds).
pub fn diag_timestamp(key: impl Into<String>, val: i64) -> HealthDiagnosticItem {
    HealthDiagnosticItem { key: key.into(), value: HealthDiagnosticValue::Timestamp(val) }
}

/// Name of the Inspect node that holds the most recent health check result or self-reported
/// fault. Matches the C++ driver SDK (`root/driver_health_check`).
pub const HEALTH_INSPECT_NODE_NAME: &str = "driver_health_check";

/// Mirrors a driver's most recent health check result into its own Inspect hierarchy.
///
/// The Rust SDK does not record health check results into a driver's Inspect tree automatically.
/// A driver that wants its latest health check visible in its own component Inspect hierarchy (at
/// `root/driver_health_check`, matching the C++ driver SDK schema) creates one of these with the
/// node it wants the record under and calls [`Self::record`] from its
/// [`crate::Driver::health_check`]:
///
/// ```ignore
/// async fn health_check(&self, request: HealthCheckRequest) -> HealthCheckResult {
///     let result = self.check_hardware(request).await;
///     self.health_inspect.record(request, &result);
///     result
/// }
/// ```
///
/// Nothing here depends on how the driver publishes Inspect: it works the same with
/// [`crate::DriverContext::publish_inspect`], with
/// [`crate::DriverContext::inspect_content_publisher`], or with an inspector the driver serves
/// itself. Drivers that build their Inspect tree on demand can instead call
/// [`record_health_check_to_inspect`] while populating it.
pub struct HealthCheckInspect {
    parent: fuchsia_inspect::Node,
    /// The current [`HEALTH_INSPECT_NODE_NAME`] child and metadata about what produced it.
    current: Mutex<Option<CurrentRecord>>,
}

struct CurrentRecord {
    _node: fuchsia_inspect::Node,
    level: Option<HealthCheckRequest>,
    is_informative: bool,
}

impl HealthCheckInspect {
    /// Creates a recorder that writes its [`HEALTH_INSPECT_NODE_NAME`] child under `parent`.
    ///
    /// Only a weak reference to `parent` is kept: the driver stays the owner of its Inspect
    /// tree, and records are silently dropped once `parent` has been removed from it.
    pub fn new(parent: &fuchsia_inspect::Node) -> Self {
        Self { parent: parent.clone_weak(), current: Mutex::new(None) }
    }

    /// Records the result of a health check at `level`.
    ///
    /// To avoid erasing diagnostic state with a routine liveness sweep or a skipped check,
    /// non-informative errors ([`HealthCheckError::NotSupported`],
    /// [`HealthCheckError::Unavailable`], [`HealthCheckError::InvalidState`]) and a healthy
    /// [`HealthCheckRequest::Ping`] do not overwrite an existing informative record from a deeper
    /// check.
    pub fn record(&self, level: HealthCheckRequest, result: &HealthCheckResult) {
        self.replace(Some(level), result.as_ref().map_err(|e| *e));
    }

    /// Records the details of a [`HealthAlert::SelfFault`] the driver is reporting through its
    /// [`HealthAlertReporter`], replacing the previous record, so Inspect also reflects faults
    /// the driver notices between health checks. Neighbour faults describe other drivers and are
    /// ignored.
    pub fn record_alert(&self, alert: &HealthAlert) {
        if let HealthAlert::SelfFault(details) = alert {
            self.replace(None, Ok(details));
        }
    }

    fn replace(
        &self,
        level: Option<HealthCheckRequest>,
        result: Result<&HealthCheckResponse, HealthCheckError>,
    ) {
        let is_no_info_error = matches!(
            result,
            Err(HealthCheckError::NotSupported
                | HealthCheckError::Unavailable
                | HealthCheckError::InvalidState)
        );
        let is_healthy_ping = level == Some(HealthCheckRequest::Ping)
            && matches!(result, Ok(resp) if derive_health_status(resp) == HealthStatus::Ok);

        let mut current = self.current.lock();
        if let Some(prev) = current.as_ref()
            && prev.is_informative
        {
            if is_no_info_error {
                return;
            }
            if is_healthy_ping && prev.level != Some(HealthCheckRequest::Ping) {
                return;
            }
        }
        // The previous node must be dropped *before* the new one is created: Inspect nodes are
        // removed from the hierarchy on drop, so assigning directly would briefly leave two
        // same-named children under `parent`, which readers may observe.
        *current = None;
        *current = Some(CurrentRecord {
            _node: record_health_check_to_inspect(&self.parent, level, result),
            level,
            is_informative: !is_no_info_error,
        });
    }
}

/// Derives the overall status of `response` from its conditions: any `UNHEALTHY` (or omitted)
/// condition makes the whole response `UNHEALTHY`; otherwise any `DEGRADED` condition makes it
/// `DEGRADED`.
fn derive_health_status(response: &HealthCheckResponse) -> HealthStatus {
    response.conditions.as_deref().map_or(HealthStatus::Ok, overall_status)
}

fn health_status_str(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Ok => "OK",
        HealthStatus::Degraded => "DEGRADED",
        HealthStatus::Unhealthy => "UNHEALTHY",
    }
}

fn health_level_str(level: HealthCheckRequest) -> &'static str {
    match level {
        HealthCheckRequest::Ping => "PING",
        HealthCheckRequest::Fast => "FAST",
        HealthCheckRequest::Deep => "DEEP",
    }
}

fn health_error_str(error: HealthCheckError) -> &'static str {
    match error {
        HealthCheckError::Timeout => "TIMEOUT",
        HealthCheckError::NotSupported => "NOT_SUPPORTED",
        HealthCheckError::InternalError => "INTERNAL_ERROR",
        HealthCheckError::InvalidState => "INVALID_STATE",
        HealthCheckError::Unavailable => "UNAVAILABLE",
        HealthCheckError::CommunicationError => "COMMUNICATION_ERROR",
        HealthCheckError::NotFound => "NOT_FOUND",
        _ => "UNKNOWN",
    }
}

/// Records `diagnostics` as properties of a new `diagnostics` child of `parent`.
fn record_health_diagnostics(parent: &fuchsia_inspect::Node, diagnostics: &[HealthDiagnosticItem]) {
    parent.record_child("diagnostics", |diag_node| {
        for item in diagnostics {
            match &item.value {
                HealthDiagnosticValue::StringValue(s) => diag_node.record_string(&item.key, s),
                HealthDiagnosticValue::IntValue(i) => diag_node.record_int(&item.key, *i),
                HealthDiagnosticValue::UintValue(u) => diag_node.record_uint(&item.key, *u),
                HealthDiagnosticValue::BoolValue(b) => diag_node.record_bool(&item.key, *b),
                HealthDiagnosticValue::Timestamp(t) => diag_node.record_int(&item.key, *t),
                _ => {}
            }
        }
    });
}

/// Records the result of a health check under a new [`HEALTH_INSPECT_NODE_NAME`] child of
/// `parent` and returns it. Dropping the returned node removes the record again.
///
/// `level` is the requested level of the check, or `None` when the record does not come from a
/// health check request (i.e. a self-reported fault). Errors are recorded as `status = "ERROR"`
/// with an `error` property. The `diagnostics` and `conditions` children are only created when
/// non-empty, matching the C++ driver SDK mirror.
///
/// Drivers that publish Inspect on demand (for example via
/// [`crate::DriverContext::inspect_content_publisher`]) can call this directly when populating
/// their tree; drivers with a long-lived inspector should prefer [`HealthCheckInspect`].
pub fn record_health_check_to_inspect(
    parent: &fuchsia_inspect::Node,
    level: Option<HealthCheckRequest>,
    result: Result<&HealthCheckResponse, HealthCheckError>,
) -> fuchsia_inspect::Node {
    let node = parent.create_child(HEALTH_INSPECT_NODE_NAME);
    if let Some(level) = level {
        node.record_string("level", health_level_str(level));
    }
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            node.record_string("status", "ERROR");
            node.record_string("error", health_error_str(error));
            return node;
        }
    };
    node.record_string("status", health_status_str(derive_health_status(response)));
    if let Some(diagnostics) = response.diagnostics.as_deref().filter(|d| !d.is_empty()) {
        record_health_diagnostics(&node, diagnostics);
    }
    if let Some(conditions) = response.conditions.as_deref().filter(|c| !c.is_empty()) {
        node.record_child("conditions", |conditions_node| {
            for condition in conditions {
                let reason_code = condition.reason_code.as_deref().unwrap_or("UNKNOWN");
                conditions_node.record_child(reason_code, |condition_node| {
                    let status = condition.status.unwrap_or(HealthStatus::Unhealthy);
                    condition_node.record_string("status", health_status_str(status));
                    if let Some(message) = &condition.message {
                        condition_node.record_string("message", message);
                    }
                    if let Some(first_observed) = condition.first_observed {
                        condition_node.record_int("first_observed", first_observed);
                    }
                    if let Some(last_observed) = condition.last_observed {
                        condition_node.record_int("last_observed", last_observed);
                    }
                    if let Some(occurrence_count) = condition.occurrence_count {
                        condition_node.record_uint("occurrence_count", occurrence_count);
                    }
                    if let Some(diagnostics) =
                        condition.diagnostics.as_deref().filter(|d| !d.is_empty())
                    {
                        record_health_diagnostics(condition_node, diagnostics);
                    }
                });
            }
        });
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fidl_next_fuchsia_driver_framework::HealthCondition;
    use futures::StreamExt;

    #[fuchsia::test]
    async fn record_health_check_to_inspect_mirrors_full_response() {
        let inspector = fuchsia_inspect::Inspector::default();
        let response = HealthCheckResponse {
            diagnostics: Some(vec![
                HealthDiagnosticItem {
                    key: "state".to_string(),
                    value: HealthDiagnosticValue::StringValue("active".to_string()),
                },
                HealthDiagnosticItem {
                    key: "underruns".to_string(),
                    value: HealthDiagnosticValue::UintValue(2),
                },
                HealthDiagnosticItem {
                    key: "temp".to_string(),
                    value: HealthDiagnosticValue::IntValue(-3),
                },
                HealthDiagnosticItem {
                    key: "powered".to_string(),
                    value: HealthDiagnosticValue::BoolValue(true),
                },
                HealthDiagnosticItem {
                    key: "last_irq".to_string(),
                    value: HealthDiagnosticValue::Timestamp(99),
                },
            ]),
            conditions: Some(vec![
                HealthCondition {
                    status: Some(HealthStatus::Unhealthy),
                    reason_code: Some("DSI_UNLOCKED".to_string()),
                    message: Some("DSI PHY lost lock".to_string()),
                    first_observed: Some(10),
                    last_observed: Some(20),
                    occurrence_count: Some(3),
                    diagnostics: Some(vec![HealthDiagnosticItem {
                        key: "phy_status".to_string(),
                        value: HealthDiagnosticValue::UintValue(7),
                    }]),
                },
                HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("TE_JITTER".to_string()),
                    ..Default::default()
                },
            ]),
        };
        let _node = record_health_check_to_inspect(
            inspector.root(),
            Some(HealthCheckRequest::Deep),
            Ok(&response),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "DEEP",
                status: "UNHEALTHY",
                diagnostics: {
                    state: "active",
                    underruns: 2u64,
                    temp: -3i64,
                    powered: true,
                    last_irq: 99i64,
                },
                conditions: {
                    DSI_UNLOCKED: {
                        status: "UNHEALTHY",
                        message: "DSI PHY lost lock",
                        first_observed: 10i64,
                        last_observed: 20i64,
                        occurrence_count: 3u64,
                        diagnostics: { phy_status: 7u64 },
                    },
                    TE_JITTER: { status: "DEGRADED" },
                },
            },
        });
    }

    #[fuchsia::test]
    async fn record_health_check_to_inspect_mirrors_error() {
        let inspector = fuchsia_inspect::Inspector::default();
        let node = record_health_check_to_inspect(
            inspector.root(),
            Some(HealthCheckRequest::Ping),
            Err(HealthCheckError::NotSupported),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "PING", status: "ERROR", error: "NOT_SUPPORTED" },
        });

        // Dropping the node removes the record; records without a level (self-reported alerts)
        // omit the level property.
        drop(node);
        let _node =
            record_health_check_to_inspect(inspector.root(), None, Err(HealthCheckError::Timeout));
        assert_data_tree!(inspector, root: {
            driver_health_check: { status: "ERROR", error: "TIMEOUT" },
        });
    }

    #[fuchsia::test]
    async fn derive_status_prefers_unhealthy_over_degraded() {
        let response = HealthCheckResponse {
            conditions: Some(vec![
                HealthCondition { status: Some(HealthStatus::Degraded), ..Default::default() },
                HealthCondition { status: Some(HealthStatus::Unhealthy), ..Default::default() },
            ]),
            ..Default::default()
        };
        assert_eq!(derive_health_status(&response), HealthStatus::Unhealthy);
        assert_eq!(derive_health_status(&HealthCheckResponse::default()), HealthStatus::Ok);
    }

    #[fuchsia::test]
    async fn health_check_inspect_replaces_and_retains_records() {
        let inspector = fuchsia_inspect::Inspector::default();
        let health_inspect = HealthCheckInspect::new(inspector.root());
        assert_data_tree!(inspector, root: {});

        // If the first check is a no-info error, it is still recorded so the node exists.
        health_inspect.record(HealthCheckRequest::Deep, &Err(HealthCheckError::NotSupported));
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "DEEP", status: "ERROR", error: "NOT_SUPPORTED" },
        });

        // A subsequent informative PING replaces the uninformative error.
        health_inspect.record(HealthCheckRequest::Ping, &Ok(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: { level: "PING", status: "OK" },
        });

        // A deeper FAST check replaces PING.
        health_inspect.record(
            HealthCheckRequest::Fast,
            &Ok(HealthCheckResponse {
                conditions: Some(vec![HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("FIFO_WARN".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        );
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "DEGRADED",
                conditions: {
                    FIFO_WARN: { status: "DEGRADED" },
                },
            },
        });

        // No-information errors (NOT_SUPPORTED, UNAVAILABLE, INVALID_STATE) and a healthy PING
        // must not overwrite the informative FAST record.
        health_inspect.record(HealthCheckRequest::Deep, &Err(HealthCheckError::NotSupported));
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::Unavailable));
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::InvalidState));
        health_inspect.record(HealthCheckRequest::Ping, &Ok(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "DEGRADED",
                conditions: {
                    FIFO_WARN: { status: "DEGRADED" },
                },
            },
        });

        // A real failure error does replace the FAST record.
        health_inspect.record(HealthCheckRequest::Fast, &Err(HealthCheckError::CommunicationError));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                level: "FAST",
                status: "ERROR",
                error: "COMMUNICATION_ERROR",
            },
        });

        // Dropping the recorder removes the record.
        drop(health_inspect);
        assert_data_tree!(inspector, root: {});
    }

    #[fuchsia::test]
    async fn self_fault_alert_replaces_inspect_node() {
        let inspector = fuchsia_inspect::Inspector::default();
        let health_inspect = HealthCheckInspect::new(inspector.root());

        health_inspect.record_alert(&HealthAlert::SelfFault(HealthCheckResponse {
            conditions: Some(vec![HealthCondition {
                status: Some(HealthStatus::Unhealthy),
                reason_code: Some("DMA_STALLED".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                status: "UNHEALTHY",
                conditions: { DMA_STALLED: { status: "UNHEALTHY" } },
            },
        });

        // Neighbour faults are not this driver's own state, so they do not touch the record.
        health_inspect
            .record_alert(&HealthAlert::UpstreamNeighborFault(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: {
            driver_health_check: {
                status: "UNHEALTHY",
                conditions: { DMA_STALLED: { status: "UNHEALTHY" } },
            },
        });

        // Recovery is reported with empty conditions and replaces the record with OK.
        health_inspect.record_alert(&HealthAlert::SelfFault(HealthCheckResponse::default()));
        assert_data_tree!(inspector, root: { driver_health_check: { status: "OK" } });
    }

    #[fuchsia::test]
    async fn alerts_are_dropped_when_queue_is_full() {
        let (reporter, alerts) = HealthAlertReporter::new_for_testing();
        // Report through clones to check that they all share one bounded queue.
        for _ in 0..(HEALTH_ALERT_QUEUE_CAPACITY + 5) {
            reporter.clone().report_health_alert(HealthAlert::SelfFault(Default::default()));
        }
        match &*reporter.sink.lock() {
            HealthAlertSink::TestQueue { dropped, .. } => assert_eq!(*dropped, 5),
            _ => panic!("expected TestQueue"),
        }
        // Dropping the last reporter closes the queue, so `collect` drains what was accepted.
        drop(reporter);
        let received: Vec<_> = alerts.collect().await;
        assert_eq!(received.len(), HEALTH_ALERT_QUEUE_CAPACITY);
    }

    #[fuchsia::test]
    async fn latch_deduplicates_repeated_raises_and_tracks_episode_counts() {
        let (latch, mut rx) = HealthAlertLatch::new_for_testing();
        assert_eq!(latch.status(), HealthStatus::Ok);
        assert!(latch.snapshot().is_empty());

        latch.raise(HealthCondition {
            status: Some(HealthStatus::Degraded),
            reason_code: Some("BUS_TIMEOUT".to_string()),
            message: Some("timeout #1".to_string()),
            first_observed: Some(100),
            last_observed: Some(100),
            ..Default::default()
        });

        let Some(HealthAlert::SelfFault(first_alert)) = rx.try_recv().ok() else {
            panic!("expected initial SelfFault alert");
        };
        let conds = first_alert.conditions.expect("conditions");
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].reason_code.as_deref(), Some("BUS_TIMEOUT"));
        assert_eq!(conds[0].status, Some(HealthStatus::Degraded));
        assert_eq!(conds[0].first_observed, Some(100));
        assert_eq!(conds[0].last_observed, Some(100));
        assert_eq!(conds[0].occurrence_count, Some(1));

        // Repeated raises at the same status update in-memory state without emitting new alerts.
        for i in 2..=4 {
            latch.raise(HealthCondition {
                status: Some(HealthStatus::Degraded),
                reason_code: Some("BUS_TIMEOUT".to_string()),
                message: Some(format!("timeout #{i}")),
                first_observed: Some(999),
                last_observed: Some(100 * i),
                ..Default::default()
            });
        }
        assert!(rx.try_recv().is_err(), "duplicate status raises must not emit alerts");

        let snap = latch.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].first_observed, Some(100));
        assert_eq!(snap[0].last_observed, Some(400));
        assert_eq!(snap[0].occurrence_count, Some(4));
        assert_eq!(snap[0].message.as_deref(), Some("timeout #4"));
    }

    #[fuchsia::test]
    async fn latch_merges_shared_clones_and_emits_partial_and_full_recovery() {
        let (driver_latch, mut rx) = HealthAlertLatch::new_for_testing();
        let wrapper_latch = driver_latch.clone();
        assert!(driver_latch.to_response().conditions.is_none());

        // Inner driver raises BUS_TIMEOUT (DEGRADED) via convenience helper.
        driver_latch.raise_degraded("BUS_TIMEOUT", "bus transfer timed out");
        assert!(rx.try_recv().is_ok());
        assert_eq!(driver_latch.to_response().conditions.as_ref().map(Vec::len), Some(1));

        // Wrapper raises POWER_TRANSITION_STALLED (DEGRADED) on its clone; alert carries BOTH.
        wrapper_latch.raise_degraded("POWER_TRANSITION_STALLED", "suspend exceeded 5s");
        let Some(HealthAlert::SelfFault(both)) = rx.try_recv().ok() else {
            panic!("expected merged SelfFault alert");
        };
        assert_eq!(both.conditions.as_ref().map(Vec::len), Some(2));
        assert_eq!(driver_latch.to_response().conditions.as_ref().map(Vec::len), Some(2));

        // Escalate stall to UNHEALTHY -> emits transition alert, overall status becomes Unhealthy.
        wrapper_latch.raise_unhealthy("POWER_TRANSITION_STALLED", "suspend wedged");
        assert_eq!(driver_latch.status(), HealthStatus::Unhealthy);
        assert!(rx.try_recv().is_ok());

        // Wrapper clears stall -> partial recovery alert still preserves BUS_TIMEOUT!
        wrapper_latch.clear("POWER_TRANSITION_STALLED");
        assert_eq!(driver_latch.status(), HealthStatus::Degraded);
        let Some(HealthAlert::SelfFault(partial)) = rx.try_recv().ok() else {
            panic!("expected partial recovery SelfFault alert");
        };
        let remaining = partial.conditions.expect("remaining conditions");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].reason_code.as_deref(), Some("BUS_TIMEOUT"));
        assert_eq!(remaining[0].message.as_deref(), Some("bus transfer timed out"));

        // Clearing an already-inactive code is a no-op.
        wrapper_latch.clear("POWER_TRANSITION_STALLED");
        assert!(rx.try_recv().is_err());

        // Driver clears BUS_TIMEOUT -> full OK recovery (empty conditions).
        driver_latch.clear("BUS_TIMEOUT");
        assert_eq!(driver_latch.status(), HealthStatus::Ok);
        assert!(driver_latch.to_response().conditions.is_none());
        let Some(HealthAlert::SelfFault(recovered)) = rx.try_recv().ok() else {
            panic!("expected full recovery SelfFault alert");
        };
        assert!(recovered.conditions.is_none());
    }

    #[fuchsia::test]
    async fn latch_omitted_status_ok_clear_clear_all_and_capacity_eviction() {
        let (latch, mut rx) = HealthAlertLatch::new_for_testing();

        // Omitted status normalizes to Unhealthy; empty reason_code normalizes to UNKNOWN.
        latch.raise(HealthCondition::default());
        assert_eq!(latch.status(), HealthStatus::Unhealthy);
        assert_eq!(latch.snapshot()[0].reason_code.as_deref(), Some("UNKNOWN"));
        assert!(rx.try_recv().is_ok());

        // clear("") refers to the same UNKNOWN condition (parity with raise and with C++ Clear).
        latch.clear("");
        assert_eq!(latch.status(), HealthStatus::Ok);
        assert!(latch.snapshot().is_empty());
        assert!(rx.try_recv().is_ok());

        // Raising HealthStatus::Ok clears the condition.
        latch.raise(HealthCondition::default());
        assert!(rx.try_recv().is_ok());
        latch.raise(HealthCondition {
            status: Some(HealthStatus::Ok),
            reason_code: Some("UNKNOWN".to_string()),
            ..Default::default()
        });
        assert_eq!(latch.status(), HealthStatus::Ok);
        assert!(rx.try_recv().is_ok());

        // Fill capacity with 16 Degraded conditions, then verify an Unhealthy condition evicts one.
        for i in 0..MAX_HEALTH_CONDITIONS {
            latch.raise(HealthCondition {
                status: Some(HealthStatus::Degraded),
                reason_code: Some(format!("DEG_{i}")),
                ..Default::default()
            });
            let _ = rx.try_recv();
        }
        assert_eq!(latch.snapshot().len(), MAX_HEALTH_CONDITIONS);

        latch.raise(HealthCondition {
            status: Some(HealthStatus::Unhealthy),
            reason_code: Some("CRITICAL_FAULT".to_string()),
            ..Default::default()
        });
        assert_eq!(latch.snapshot().len(), MAX_HEALTH_CONDITIONS);
        assert_eq!(latch.status(), HealthStatus::Unhealthy);
        assert!(
            latch.snapshot().iter().any(|c| c.reason_code.as_deref() == Some("CRITICAL_FAULT"))
        );
        let _ = rx.try_recv();

        // merge_into deduplicates by reason_code (keeping the caller's live entry) and caps at
        // MAX_HEALTH_CONDITIONS.
        let mut resp = HealthCheckResponse {
            diagnostics: None,
            conditions: Some(vec![HealthCondition {
                status: Some(HealthStatus::Unhealthy),
                reason_code: Some("CRITICAL_FAULT".to_string()),
                message: Some("live FAST measurement".to_string()),
                ..Default::default()
            }]),
        };
        latch.merge_into(&mut resp);
        let merged = resp.conditions.as_ref().expect("conditions");
        assert_eq!(merged.len(), MAX_HEALTH_CONDITIONS);
        assert_eq!(
            merged.iter().filter(|c| c.reason_code.as_deref() == Some("CRITICAL_FAULT")).count(),
            1
        );
        assert_eq!(merged[0].message.as_deref(), Some("live FAST measurement"));

        // When the response already holds a live Degraded condition, the 15 latched Degraded
        // entries fill it before the latched Unhealthy entry is reached; merge_into must replace
        // a latched Degraded entry with it rather than drop it or the caller's live entry.
        let mut live_only = HealthCheckResponse {
            diagnostics: None,
            conditions: Some(vec![HealthCondition {
                status: Some(HealthStatus::Degraded),
                reason_code: Some("LIVE_ONLY".to_string()),
                ..Default::default()
            }]),
        };
        latch.merge_into(&mut live_only);
        let merged = live_only.conditions.as_ref().expect("conditions");
        assert_eq!(merged.len(), MAX_HEALTH_CONDITIONS);
        assert!(merged.iter().any(|c| c.reason_code.as_deref() == Some("CRITICAL_FAULT")
            && c.status == Some(HealthStatus::Unhealthy)));
        assert_eq!(merged[0].reason_code.as_deref(), Some("LIVE_ONLY"));

        // clear_all emits a single empty recovery alert and resets episode count.
        latch.clear_all();
        assert_eq!(latch.status(), HealthStatus::Ok);
        let Some(HealthAlert::SelfFault(cleared)) = rx.try_recv().ok() else {
            panic!("expected clear_all recovery alert");
        };
        assert!(cleared.conditions.is_none());
        assert!(rx.try_recv().is_err());

        // A re-raise after clear_all starts a fresh episode.
        latch.raise(HealthCondition {
            status: Some(HealthStatus::Unhealthy),
            reason_code: Some("CRITICAL_FAULT".to_string()),
            ..Default::default()
        });
        assert_eq!(latch.snapshot().len(), 1);
        assert_eq!(latch.snapshot()[0].occurrence_count, Some(1));
        assert!(rx.try_recv().is_ok());
    }

    #[fuchsia::test]
    async fn latch_set_inspect_stays_consistent_across_threads() {
        let inspector = fuchsia_inspect::Inspector::default();
        let health_inspect = Arc::new(HealthCheckInspect::new(inspector.root()));
        let (latch, _rx) = HealthAlertLatch::new_for_testing();
        latch.set_inspect(health_inspect);

        let l1 = latch.clone();
        let t1 = std::thread::spawn(move || {
            for _ in 0..30 {
                l1.raise(HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("THREAD_1".to_string()),
                    ..Default::default()
                });
                l1.clear("THREAD_1");
            }
        });

        let l2 = latch.clone();
        let t2 = std::thread::spawn(move || {
            for _ in 0..30 {
                l2.raise(HealthCondition {
                    status: Some(HealthStatus::Unhealthy),
                    reason_code: Some("THREAD_2".to_string()),
                    ..Default::default()
                });
                l2.clear("THREAD_2");
            }
        });

        t1.join().expect("t1");
        t2.join().expect("t2");

        assert_eq!(latch.status(), HealthStatus::Ok);
        assert!(latch.snapshot().is_empty());
        assert_data_tree!(inspector, root: {
            driver_health_check: { status: "OK" },
        });
    }
}
