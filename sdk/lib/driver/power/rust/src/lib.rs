// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::future::Future;
use core::pin::pin;
use fdf_component::{
    Driver, DriverContext, DriverError, HealthAlertLatch, HealthCheckError, HealthCheckRequest,
    HealthCheckResponse, HealthCondition, HealthDiagnosticItem, HealthDiagnosticValue,
    HealthStatus,
};
use fidl_fuchsia_hardware_power as fhw_power;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_power_broker as fpower_broker;
use fidl_fuchsia_power_system as fpower;
use fuchsia_async as fasync;
use fuchsia_component::client::{Connect, SVC_DIR};
use fuchsia_component::directory::Directory;
use futures::TryStreamExt;
use futures::future::{Either, select};
use log::{error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use zx::{MonotonicDuration, Status};

use fidl_next as _;
use fidl_next_fuchsia_hardware_powerdomain as fpowerdomain;

/// Default duration after which an in-flight `suspend` or `resume` transition is reported as
/// [`HealthStatus::Degraded`] (`POWER_TRANSITION_STALLED`) without cancelling the transition.
///
/// Ten seconds aligns with Power Broker's and System Activity Governor's stalled power transition
/// warnings. Because the transition watchdog and wrapper health-check timeouts execute on the
/// driver's single-threaded `LocalExecutor`, they observe asynchronous stalls (such as awaiting a
/// hung FIDL call or async lock); synchronous thread blocks are caught by the Driver Framework's
/// own `HealthCheck` deadline instead. Drivers with legitimately longer transitions can override
/// [`SuspendableDriver::power_transition_stall_timeout`].
pub const POWER_TRANSITION_STALL_TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(10);

/// `HealthCondition.reason_code` reported when a `suspend` or `resume` transition exceeds
/// [`SuspendableDriver::power_transition_stall_timeout`].
pub const POWER_TRANSITION_STALLED_REASON_CODE: &str = "POWER_TRANSITION_STALLED";

// Per-level timeouts applied to the wrapped driver's `health_check` *only* while a power
// transition is actively stalled, kept safely below the Driver Host / Driver Framework
// `HealthCheck` deadlines (Ping 250ms, Fast 2s) so that if the wrapped driver's `health_check`
// blocks on a lock held by the stalled transition, `Suspendable::health_check` still returns
// `POWER_TRANSITION_STALLED` before the Driver Framework's `HealthCheck` deadline expires.
// `Deep` is not timed out by the wrapper so stateful deep probes are never cancelled mid-restore
// before the framework's 2-minute deadline.
const STALLED_PING_DRIVER_HEALTH_CHECK_TIMEOUT: MonotonicDuration =
    MonotonicDuration::from_millis(100);
const STALLED_FAST_DRIVER_HEALTH_CHECK_TIMEOUT: MonotonicDuration =
    MonotonicDuration::from_millis(1500);

/// Implement this trait if you'd like to get notifications when the system is about to go into
/// suspend and come out of resume.
pub trait SuspendableDriver: Driver {
    /// Called prior the system entering suspend. The system is not guaranteed to enter suspend,
    /// but `resume` will be called regardless before a subsequent suspension attempt occurs.
    fn suspend(&self) -> impl Future<Output = ()> + Send;

    /// Called after `suspend` to indicate the system is no longer in suspend. The system may not
    /// have actually entered suspension in between `suspend` and `resume` invocations. NOTE: there
    /// is no initial call to `resume`; drivers are expected to start in a non-suspended state and
    /// `resume` will only be called after `suspend`.
    fn resume(&self) -> impl Future<Output = ()> + Send;

    /// Returns whether or not suspend is enabled. If false is returned, suspend and resume methods
    /// will never be called.
    fn suspend_enabled(&self) -> bool;

    /// Returns the duration after which an in-flight asynchronous `suspend` or `resume` transition
    /// is reported as [`HealthStatus::Degraded`] (`POWER_TRANSITION_STALLED`) through the driver's
    /// shared [`HealthAlertLatch`], or `None` (or a non-positive duration) to opt out of
    /// power-transition stall reporting.
    fn power_transition_stall_timeout(&self) -> Option<MonotonicDuration> {
        Some(POWER_TRANSITION_STALL_TIMEOUT)
    }
}

#[derive(Clone)]
struct TransitionWatchdog {
    health_latch: HealthAlertLatch,
    stalled: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

struct ActiveStallGuard<'a> {
    watchdog: &'a TransitionWatchdog,
    completed: bool,
}

impl Drop for ActiveStallGuard<'_> {
    fn drop(&mut self) {
        self.watchdog.stalled.store(false, Ordering::Release);
        if !self.completed && !self.watchdog.stopping.load(Ordering::Acquire) {
            self.watchdog.health_latch.clear(POWER_TRANSITION_STALLED_REASON_CODE);
        }
    }
}

impl TransitionWatchdog {
    async fn watch<T: SuspendableDriver>(
        &self,
        driver: &T,
        operation: &'static str,
        transition_fut: impl Future<Output = ()>,
    ) {
        let Some(timeout) =
            driver.power_transition_stall_timeout().filter(|&d| d > MonotonicDuration::ZERO)
        else {
            transition_fut.await;
            return;
        };

        let started_boot = zx::BootInstant::get();
        let mut transition_fut = pin!(transition_fut);
        let timer = pin!(fasync::Timer::new(fasync::MonotonicInstant::after(timeout)));

        match select(transition_fut.as_mut(), timer).await {
            Either::Left(((), _timer)) => {}
            Either::Right(((), remaining_transition)) => {
                // Why: The lifecycle server closes the health-alert reporter before calling
                // `Suspendable::stop`, so outbound alert messages are already suppressed; checking
                // `stopping` avoids mutating `HealthAlertLatch` or its `HealthCheckInspect` mirror
                // while the driver is tearing down.
                if self.stopping.load(Ordering::Acquire) {
                    remaining_transition.await;
                    return;
                }

                let timeout_ms = timeout.into_millis();
                self.stalled.store(true, Ordering::Release);
                let mut guard = ActiveStallGuard { watchdog: self, completed: false };

                self.health_latch.raise(HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some(POWER_TRANSITION_STALLED_REASON_CODE.to_string()),
                    message: Some(format!(
                        "Driver {operation} transition stalled for >= {timeout_ms} ms"
                    )),
                    first_observed: Some(started_boot.into_nanos()),
                    last_observed: Some(zx::BootInstant::get().into_nanos()),
                    occurrence_count: Some(1),
                    diagnostics: Some(vec![
                        HealthDiagnosticItem {
                            key: "stalled_power_operation".to_string(),
                            value: HealthDiagnosticValue::StringValue(operation.to_string()),
                        },
                        HealthDiagnosticItem {
                            key: "stall_timeout_ms".to_string(),
                            value: HealthDiagnosticValue::IntValue(timeout_ms),
                        },
                    ]),
                });

                remaining_transition.await;
                guard.completed = true;
                drop(guard);

                // Why: The lifecycle server has already closed the health-alert reporter before
                // `Suspendable::stop`, so checking `stopping` avoids recording a stale clear on
                // `HealthAlertLatch` or its `HealthCheckInspect` mirror during teardown.
                if self.stopping.load(Ordering::Acquire) {
                    return;
                }

                self.health_latch.clear(POWER_TRANSITION_STALLED_REASON_CODE);
            }
        }
    }
}

/// Wrapper trait to indicate the driver supports power operations.
pub struct Suspendable<T: Driver> {
    #[expect(unused)]
    scope: Option<fasync::Scope>,
    driver: Arc<T>,
    health_latch: HealthAlertLatch,
    stalled: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl<T: Driver> core::ops::Deref for Suspendable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

async fn run_suspend_blocker<T: SuspendableDriver>(
    driver: Weak<T>,
    watchdog: TransitionWatchdog,
    mut service: fpower::SuspendBlockerRequestStream,
) {
    use fpower::SuspendBlockerRequest::*;
    while let Some(req) = service.try_next().await.unwrap() {
        match req {
            BeforeSuspend { responder, .. } => {
                if let Some(driver) = driver.upgrade() {
                    watchdog.watch(driver.as_ref(), "suspend", driver.suspend()).await;
                } else {
                    return;
                }
                let _ = responder.send();
            }
            AfterResume { responder, .. } => {
                if let Some(driver) = driver.upgrade() {
                    watchdog.watch(driver.as_ref(), "resume", driver.resume()).await;
                } else {
                    return;
                }
                let _ = responder.send();
            }
            // Ignore unknown requests.
            _ => {
                warn!("Received unknown sag listener request");
            }
        }
    }
}

async fn run_element_runner<T: SuspendableDriver>(
    driver: Weak<T>,
    watchdog: TransitionWatchdog,
    mut service: fpower_broker::ElementRunnerRequestStream,
) {
    let mut first_activation_occurred = false;
    while let Some(req) = service.try_next().await.unwrap_or_default() {
        if let fpower_broker::ElementRunnerRequest::SetLevel { level, responder } = req {
            let Some(driver) = driver.upgrade() else { return };
            if level != fhw_power::FrameworkElementLevels::Off.into_primitive() as u8 {
                // Hide the initial resume because drivers should start in a resumed state and it is
                // easier for users if we guarantee that a call to resume always follows a call to
                // suspend.
                if first_activation_occurred {
                    watchdog.watch(driver.as_ref(), "resume", driver.resume()).await;
                }
            } else {
                watchdog.watch(driver.as_ref(), "suspend", driver.suspend()).await;
            }
            let _ = responder.send();
            first_activation_occurred = true;
        }
    }
}

impl<T: SuspendableDriver + Send + Sync> Driver for Suspendable<T> {
    const NAME: &str = T::NAME;

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let health_latch = context.health_alert_latch();
        let mut runner = context
            .start_args
            .power_element_args
            .as_mut()
            .and_then(|args| args.runner_server.take());

        let (svc, svc_server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        context
            .incoming
            .open(
                SVC_DIR,
                fio::Flags::PROTOCOL_DIRECTORY | fio::PERM_READABLE,
                svc_server.into_channel(),
            )
            .map_err(|error| {
                error!(error:?; "Error opening svc directory");
                Status::INTERNAL
            })?;

        let driver = Arc::new(T::start(context).await?);
        let stalled = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));

        let scope = if driver.suspend_enabled() {
            let scope = fasync::Scope::new_with_name("suspend");
            let watchdog = TransitionWatchdog {
                health_latch: health_latch.clone(),
                stalled: stalled.clone(),
                stopping: stopping.clone(),
            };
            let weak_driver = Arc::downgrade(&driver);

            if let Some(runner) = runner.take() {
                let runner = fidl::endpoints::ServerEnd::<fpower_broker::ElementRunnerMarker>::new(
                    runner.into_untyped(),
                );
                scope.spawn(async move {
                    run_element_runner(weak_driver, watchdog, runner.into_stream()).await
                });
            } else {
                let sag =
                    fpower::ActivityGovernorProxy::connect_at_dir_root(&svc).map_err(|error| {
                        error!(error:?; "Error connecting to sag");
                        Status::INTERNAL
                    })?;

                let (client, server) = fidl::endpoints::create_endpoints();

                let _ = sag
                    .register_suspend_blocker(
                        fpower::ActivityGovernorRegisterSuspendBlockerRequest {
                            suspend_blocker: Some(client),
                            name: Some(Self::NAME.into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| {
                        error!(error:?; "Error connecting to sag");
                        Status::INTERNAL
                    })?
                    .map_err(|error| {
                        error!(error:?; "Error connecting to sag");
                        Status::INTERNAL
                    })?;

                scope.spawn(async move {
                    run_suspend_blocker(weak_driver, watchdog, server.into_stream()).await
                });
            }
            Some(scope)
        } else {
            None
        };

        Ok(Self { driver, scope, health_latch, stalled, stopping })
    }

    async fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.driver.stop().await;
    }

    /// Forwards `request` to the wrapped driver and merges any active conditions from the shared
    /// [`HealthAlertLatch`] (including `POWER_TRANSITION_STALLED` while a transition is stalled).
    ///
    /// While a transition is actively stalled, `Ping` (100ms) and `Fast` (1.5s) apply a wrapper
    /// timeout to `self.driver.health_check(request)` below the Driver Host / Driver Framework
    /// deadline so that if the wrapped driver's `health_check` blocks behind the stalled
    /// transition, the active stall condition is still returned before the Driver Framework's
    /// `HealthCheck` deadline expires. `Deep` is never cancelled early by the wrapper, and `Err`
    /// responses from the wrapped driver's `health_check` are preserved.
    async fn health_check(
        &self,
        request: HealthCheckRequest,
    ) -> Result<HealthCheckResponse, HealthCheckError> {
        if !self.stalled.load(Ordering::Acquire) {
            let mut resp = self.driver.health_check(request).await?;
            self.health_latch.merge_into(&mut resp);
            return Ok(resp);
        }

        let driver_check_timeout = if request == HealthCheckRequest::Ping {
            Some(STALLED_PING_DRIVER_HEALTH_CHECK_TIMEOUT)
        } else if request == HealthCheckRequest::Fast {
            Some(STALLED_FAST_DRIVER_HEALTH_CHECK_TIMEOUT)
        } else {
            None
        };

        let Some(driver_check_timeout) = driver_check_timeout else {
            let mut resp = self.driver.health_check(request).await?;
            self.health_latch.merge_into(&mut resp);
            return Ok(resp);
        };

        let driver_health_check_fut = pin!(self.driver.health_check(request));
        let timer = pin!(fasync::Timer::new(fasync::MonotonicInstant::after(driver_check_timeout)));
        match select(driver_health_check_fut, timer).await {
            Either::Left((Ok(mut resp), _timer)) => {
                self.health_latch.merge_into(&mut resp);
                Ok(resp)
            }
            Either::Left((Err(err), _timer)) => Err(err),
            Either::Right(((), _dropped_driver_health_check)) => {
                if !self.stalled.load(Ordering::Acquire) {
                    // Why: The power-transition stall cleared while the wrapped driver's
                    // `health_check` stayed blocked, so `POWER_TRANSITION_STALLED` no longer
                    // explains the missed wrapper timeout even if unrelated conditions remain
                    // latched. Returning `Ok` would mask a check that never completed.
                    return Err(HealthCheckError::Timeout);
                }
                let mut resp = HealthCheckResponse {
                    diagnostics: Some(vec![HealthDiagnosticItem {
                        key: "inner_health_check_timed_out".to_string(),
                        value: HealthDiagnosticValue::BoolValue(true),
                    }]),
                    conditions: None,
                };
                self.health_latch.merge_into(&mut resp);
                let has_stall_condition = resp.conditions.as_ref().is_some_and(|conds| {
                    conds.iter().any(|c| {
                        c.reason_code.as_deref() == Some(POWER_TRANSITION_STALLED_REASON_CODE)
                    })
                });
                if !has_stall_condition {
                    return Err(HealthCheckError::Timeout);
                }
                Ok(resp)
            }
        }
    }
}

/// Extension trait for [`DriverContext`] to simplify connecting to power resources.
pub trait PowerExt {
    /// Connects to a power domain resource with the given instance name.
    fn connect_to_powerdomain(
        &self,
        instance: &str,
    ) -> Result<fidl_next::Client<fpowerdomain::Domain>, DriverError>;
}

impl PowerExt for DriverContext {
    fn connect_to_powerdomain(
        &self,
        instance: &str,
    ) -> Result<fidl_next::Client<fpowerdomain::Domain>, DriverError> {
        let service = self
            .incoming
            .service::<fdf_component::ServiceInstance<fpowerdomain::Service>>()
            .instance(instance)
            .connect_next()?;
        let (client, server) = fidl_next::fuchsia::create_channel();
        service.domain(server)?;
        Ok(client.spawn())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_component::HealthAlert;
    use fdf_component::testing::harness::TestHarness;
    use fidl_next_fuchsia_driver_framework as fdf;
    use fuchsia_component::server::ServiceFs;
    use futures::StreamExt;
    use futures::channel::oneshot;

    struct TestDriver {
        health_latch: HealthAlertLatch,
        suspend_called: Arc<AtomicBool>,
        resume_called: Arc<AtomicBool>,
        suspend_enabled: bool,
        stop_called: Arc<AtomicBool>,
        health_check_called: AtomicBool,
        stall_timeout: fuchsia_sync::Mutex<Option<MonotonicDuration>>,
        suspend_blocker: fuchsia_sync::Mutex<Option<oneshot::Receiver<()>>>,
        resume_blocker: fuchsia_sync::Mutex<Option<oneshot::Receiver<()>>>,
        on_ping_entered: fuchsia_sync::Mutex<Option<oneshot::Sender<()>>>,
        ping_blocker: fuchsia_sync::Mutex<Option<oneshot::Receiver<()>>>,
        on_stop_entered: fuchsia_sync::Mutex<Option<oneshot::Sender<()>>>,
        stop_blocker: fuchsia_sync::Mutex<Option<oneshot::Receiver<()>>>,
        deep_delay: fuchsia_sync::Mutex<Option<MonotonicDuration>>,
    }

    impl Driver for TestDriver {
        const NAME: &str = "test_driver";

        async fn start(context: DriverContext) -> Result<Self, DriverError> {
            Ok(Self {
                health_latch: context.health_alert_latch(),
                suspend_called: Arc::new(AtomicBool::new(false)),
                resume_called: Arc::new(AtomicBool::new(false)),
                suspend_enabled: true,
                stop_called: Arc::new(AtomicBool::new(false)),
                health_check_called: AtomicBool::new(false),
                stall_timeout: fuchsia_sync::Mutex::new(Some(MonotonicDuration::from_millis(20))),
                suspend_blocker: fuchsia_sync::Mutex::new(None),
                resume_blocker: fuchsia_sync::Mutex::new(None),
                on_ping_entered: fuchsia_sync::Mutex::new(None),
                ping_blocker: fuchsia_sync::Mutex::new(None),
                on_stop_entered: fuchsia_sync::Mutex::new(None),
                stop_blocker: fuchsia_sync::Mutex::new(None),
                deep_delay: fuchsia_sync::Mutex::new(None),
            })
        }

        async fn stop(&self) {
            self.stop_called.store(true, Ordering::SeqCst);
            if let Some(entered_tx) = self.on_stop_entered.lock().take() {
                let _ = entered_tx.send(());
            }
            let rx = self.stop_blocker.lock().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
        }

        async fn health_check(
            &self,
            request: HealthCheckRequest,
        ) -> Result<HealthCheckResponse, HealthCheckError> {
            self.health_check_called.store(true, Ordering::SeqCst);
            match request {
                HealthCheckRequest::Ping => {
                    if let Some(entered_tx) = self.on_ping_entered.lock().take() {
                        let _ = entered_tx.send(());
                    }
                    let rx = self.ping_blocker.lock().take();
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                    let mut resp = HealthCheckResponse::default();
                    self.health_latch.merge_into(&mut resp);
                    Ok(resp)
                }
                HealthCheckRequest::Fast => Ok(HealthCheckResponse {
                    conditions: Some(vec![HealthCondition {
                        status: Some(HealthStatus::Degraded),
                        reason_code: Some("TEST_DEGRADED".to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                HealthCheckRequest::Deep => {
                    let delay = (*self.deep_delay.lock()).ok_or(HealthCheckError::NotSupported)?;
                    fasync::Timer::new(fasync::MonotonicInstant::after(delay)).await;
                    Ok(HealthCheckResponse {
                        conditions: Some(vec![HealthCondition {
                            status: Some(HealthStatus::Degraded),
                            reason_code: Some("DEEP_PROBE_NOTE".to_string()),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    })
                }
            }
        }
    }

    impl SuspendableDriver for TestDriver {
        async fn suspend(&self) {
            self.suspend_called.store(true, Ordering::SeqCst);
            let rx = self.suspend_blocker.lock().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
        }

        async fn resume(&self) {
            self.resume_called.store(true, Ordering::SeqCst);
            let rx = self.resume_blocker.lock().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
        }

        fn suspend_enabled(&self) -> bool {
            self.suspend_enabled
        }

        fn power_transition_stall_timeout(&self) -> Option<MonotonicDuration> {
            *self.stall_timeout.lock()
        }
    }

    #[fuchsia::test]
    async fn test_suspend_resume_with_runner_emits_no_alert_when_fast() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();

        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );

        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let (test_driver_stop_called, test_driver_resume_called, test_driver_suspend_called) = {
            let suspendable = driver_under_test.get_driver().expect("Failed to get driver");
            (
                suspendable.driver.stop_called.clone(),
                suspendable.driver.resume_called.clone(),
                suspendable.driver.suspend_called.clone(),
            )
        };

        let runner_proxy = runner_client.into_proxy();

        // Level 1 should NOT trigger resume as it's the initial activation
        runner_proxy.set_level(1).await.expect("Failed to set level");
        assert!(!test_driver_resume_called.load(Ordering::SeqCst));

        // Level 0 should trigger suspend
        runner_proxy.set_level(0).await.expect("Failed to set level");
        assert!(test_driver_suspend_called.load(Ordering::SeqCst));
        test_driver_suspend_called.store(false, Ordering::SeqCst);

        // Level 1 again should trigger resume
        runner_proxy.set_level(1).await.expect("Failed to set level");
        assert!(test_driver_resume_called.load(Ordering::SeqCst));

        // Fast transitions that finish before the threshold must not queue any health alert.
        assert!(driver_under_test.try_next_health_alert().is_none());

        driver_under_test.stop_driver().await;
        assert!(test_driver_stop_called.load(Ordering::SeqCst));
    }

    #[fuchsia::test]
    async fn test_health_check_forwards_to_inner_driver() {
        let (_runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let suspendable = driver_under_test.get_driver().expect("Failed to get driver");

        assert!(!suspendable.health_check_called.load(Ordering::SeqCst));

        let result = <Suspendable<TestDriver> as Driver>::health_check(
            suspendable,
            HealthCheckRequest::Fast,
        )
        .await;
        assert!(suspendable.health_check_called.load(Ordering::SeqCst));
        let response = result.expect("FAST is answered by the wrapped driver");
        let conditions = response.conditions.expect("conditions");
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].status, Some(HealthStatus::Degraded));
        assert_eq!(conditions[0].reason_code.as_deref(), Some("TEST_DEGRADED"));

        let result = <Suspendable<TestDriver> as Driver>::health_check(
            suspendable,
            HealthCheckRequest::Deep,
        )
        .await;
        assert_eq!(result, Err(HealthCheckError::NotSupported));

        driver_under_test.stop_driver().await;
    }

    fn spawn_fake_sag() -> (
        ServiceFs<fuchsia_component::server::ServiceObj<'static, ()>>,
        fasync::Scope,
        futures::channel::mpsc::UnboundedReceiver<fpower::SuspendBlockerProxy>,
    ) {
        let (blocker_tx, blocker_rx) =
            futures::channel::mpsc::unbounded::<fpower::SuspendBlockerProxy>();
        let sag_scope = fasync::Scope::new();
        let sag_handle = sag_scope.to_handle();
        let mut incoming_fs = ServiceFs::new();
        incoming_fs.dir("svc").add_fidl_service(
            move |mut stream: fpower::ActivityGovernorRequestStream| {
                let blocker_tx = blocker_tx.clone();
                sag_handle.spawn(async move {
                    while let Some(Ok(req)) = stream.next().await {
                        if let fpower::ActivityGovernorRequest::RegisterSuspendBlocker {
                            payload,
                            responder,
                        } = req
                        {
                            let client = payload.suspend_blocker.expect("suspend_blocker client");
                            let (_lease_c, lease_s) = zx::EventPair::create();
                            let _ = blocker_tx.unbounded_send(client.into_proxy());
                            let _ = responder.send(Ok(lease_s));
                        }
                    }
                });
            },
        );
        (incoming_fs, sag_scope, blocker_rx)
    }

    #[fuchsia::test]
    async fn test_stalled_suspend_emits_degraded_alert_and_recovers() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");

        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        *driver_under_test.get_driver().unwrap().driver.suspend_blocker.lock() = Some(unblock_rx);

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("Failed to set level");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("Failed to set level");
        });

        let stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");
        match stall_alert {
            HealthAlert::SelfFault(resp) => {
                let conditions = resp.conditions.expect("conditions");
                assert_eq!(conditions.len(), 1);
                assert_eq!(conditions[0].status, Some(HealthStatus::Degraded));
                assert_eq!(
                    conditions[0].reason_code.as_deref(),
                    Some(POWER_TRANSITION_STALLED_REASON_CODE)
                );
                let first = conditions[0].first_observed.expect("first_observed");
                let last = conditions[0].last_observed.expect("last_observed");
                assert!(first <= last);
                let diagnostics = conditions[0].diagnostics.as_ref().expect("condition diag");
                assert!(diagnostics.iter().any(|d| {
                    d.key == "stalled_power_operation"
                        && d.value == HealthDiagnosticValue::StringValue("suspend".to_string())
                }));
            }
            other => panic!("Unexpected alert variant: {other:?}"),
        }

        // Unblock the transition; verify it completes without cancellation and clears the stall.
        unblock_tx.send(()).expect("unblock transition");
        set_level_task.await;

        let recovery_alert = driver_under_test.next_health_alert().await.expect("recovery alert");
        match recovery_alert {
            HealthAlert::SelfFault(resp) => {
                assert!(resp.conditions.unwrap_or_default().is_empty());
            }
            other => panic!("Unexpected recovery alert variant: {other:?}"),
        }

        let ping_after = driver_under_test
            .health_check(HealthCheckRequest::Ping)
            .await
            .expect("ping health check");
        assert!(ping_after.conditions.unwrap_or_default().is_empty());

        driver_under_test.stop_driver().await;
    }

    /// Verifies that while a power transition is stalled (`POWER_TRANSITION_STALLED` latched):
    /// 1. `Ping` health checks include the latched `POWER_TRANSITION_STALLED` condition.
    /// 2. `Fast` health checks merge `POWER_TRANSITION_STALLED` alongside conditions returned
    ///    directly by the wrapped driver (`TEST_DEGRADED`) without overwriting either.
    /// 3. `Deep` health checks that fail in the wrapped driver (`Err(HealthCheckError::NotSupported)`)
    ///    propagate the error unchanged rather than masking it with an `Ok` stall response.
    #[fuchsia::test]
    async fn test_health_checks_merge_stall_and_preserve_errors_during_stall() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");

        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        *driver_under_test.get_driver().unwrap().driver.suspend_blocker.lock() = Some(unblock_rx);

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("Failed to set level");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("Failed to set level");
        });

        let _stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");

        let ping_result = driver_under_test
            .health_check(HealthCheckRequest::Ping)
            .await
            .expect("ping health check");
        assert_eq!(ping_result.conditions.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            ping_result.conditions.as_ref().unwrap()[0].reason_code.as_deref(),
            Some(POWER_TRANSITION_STALLED_REASON_CODE)
        );

        let fast_result = driver_under_test
            .health_check(HealthCheckRequest::Fast)
            .await
            .expect("fast health check");
        let fast_conds = fast_result.conditions.expect("fast conditions");
        assert_eq!(fast_conds.len(), 2);
        assert!(
            fast_conds.iter().any(|c| {
                c.reason_code.as_deref() == Some(POWER_TRANSITION_STALLED_REASON_CODE)
            })
        );
        assert!(fast_conds.iter().any(|c| c.reason_code.as_deref() == Some("TEST_DEGRADED")));

        assert_eq!(
            driver_under_test.health_check(HealthCheckRequest::Deep).await,
            Err(HealthCheckError::NotSupported)
        );

        unblock_tx.send(()).expect("unblock transition");
        set_level_task.await;
        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_inner_driver_fault_preserved_across_stall_and_recovery() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");

        // 1. Inner driver raises BUS_TIMEOUT on its shared HealthAlertLatch before suspend stalls.
        driver_under_test.get_driver().unwrap().driver.health_latch.raise(HealthCondition {
            status: Some(HealthStatus::Degraded),
            reason_code: Some("BUS_TIMEOUT".to_string()),
            ..Default::default()
        });
        let initial_alert = driver_under_test.next_health_alert().await.expect("bus timeout alert");
        match initial_alert {
            HealthAlert::SelfFault(resp) => {
                assert_eq!(resp.conditions.as_ref().map(Vec::len), Some(1));
            }
            other => panic!("Unexpected alert: {other:?}"),
        }

        // 2. Suspend stalls -> alert carries BOTH BUS_TIMEOUT and POWER_TRANSITION_STALLED.
        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        *driver_under_test.get_driver().unwrap().driver.suspend_blocker.lock() = Some(unblock_rx);

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("set level 1");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("set level 0");
        });

        let stall_alert = driver_under_test.next_health_alert().await.expect("merged stall alert");
        match stall_alert {
            HealthAlert::SelfFault(resp) => {
                let conds = resp.conditions.expect("merged conditions");
                assert_eq!(conds.len(), 2);
                assert!(conds.iter().any(|c| c.reason_code.as_deref() == Some("BUS_TIMEOUT")));
                assert!(conds.iter().any(|c| {
                    c.reason_code.as_deref() == Some(POWER_TRANSITION_STALLED_REASON_CODE)
                }));
            }
            other => panic!("Unexpected alert: {other:?}"),
        }

        // 3. Suspend finishes -> partial recovery alert preserves BUS_TIMEOUT (never falsely OK!).
        unblock_tx.send(()).expect("unblock suspend");
        set_level_task.await;

        let partial_recovery =
            driver_under_test.next_health_alert().await.expect("partial recovery alert");
        match partial_recovery {
            HealthAlert::SelfFault(resp) => {
                let conds = resp.conditions.expect("remaining BUS_TIMEOUT condition");
                assert_eq!(conds.len(), 1);
                assert_eq!(conds[0].reason_code.as_deref(), Some("BUS_TIMEOUT"));
            }
            other => panic!("Unexpected alert: {other:?}"),
        }

        // 4. Inner driver clears BUS_TIMEOUT -> full OK recovery.
        driver_under_test.get_driver().unwrap().driver.health_latch.clear("BUS_TIMEOUT");
        let full_recovery = driver_under_test.next_health_alert().await.expect("full recovery");
        match full_recovery {
            HealthAlert::SelfFault(resp) => assert!(resp.conditions.is_none()),
            other => panic!("Unexpected alert: {other:?}"),
        }

        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_sag_suspend_blocker_opt_out_suppresses_stall_alert() {
        let (incoming_fs, _sag_scope, mut blocker_rx) = spawn_fake_sag();
        let mut harness =
            TestHarness::<Suspendable<TestDriver>>::new().set_driver_incoming(incoming_fs);
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let suspend_blocker_proxy = blocker_rx.next().await.expect("registered SuspendBlocker");

        // Both `None` and `Some(ZERO)` opt out of stall reporting.
        for opt_out_timeout in [None, Some(MonotonicDuration::ZERO)] {
            *driver_under_test.get_driver().unwrap().driver.stall_timeout.lock() = opt_out_timeout;
            let (opt_out_tx, opt_out_rx) = oneshot::channel::<()>();
            *driver_under_test.get_driver().unwrap().driver.suspend_blocker.lock() =
                Some(opt_out_rx);

            let scope = fasync::Scope::new();
            let proxy_for_suspend = suspend_blocker_proxy.clone();
            let before_suspend_task = scope.compute(async move {
                proxy_for_suspend.before_suspend().await.expect("BeforeSuspend");
            });
            fasync::Timer::new(fasync::MonotonicInstant::after(MonotonicDuration::from_millis(25)))
                .await;
            assert!(driver_under_test.try_next_health_alert().is_none());
            opt_out_tx.send(()).expect("unblock BeforeSuspend");
            before_suspend_task.await;
        }

        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_sag_suspend_blocker_resume_stall_and_recovery() {
        let (incoming_fs, _sag_scope, mut blocker_rx) = spawn_fake_sag();
        let mut harness =
            TestHarness::<Suspendable<TestDriver>>::new().set_driver_incoming(incoming_fs);
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let suspend_blocker_proxy = blocker_rx.next().await.expect("registered SuspendBlocker");

        let (resume_tx, resume_rx) = oneshot::channel::<()>();
        *driver_under_test.get_driver().unwrap().driver.resume_blocker.lock() = Some(resume_rx);

        let scope = fasync::Scope::new();
        let after_resume_task = scope.compute(async move {
            suspend_blocker_proxy.after_resume().await.expect("AfterResume");
        });

        let stall_alert = driver_under_test.next_health_alert().await.expect("resume stall alert");
        match stall_alert {
            HealthAlert::SelfFault(resp) => {
                let conds = resp.conditions.expect("conditions");
                assert_eq!(conds.len(), 1);
                let diag = conds[0].diagnostics.as_ref().expect("diagnostics");
                assert!(diag.iter().any(|d| {
                    d.key == "stalled_power_operation"
                        && d.value == HealthDiagnosticValue::StringValue("resume".to_string())
                }));
            }
            other => panic!("Unexpected alert: {other:?}"),
        }

        resume_tx.send(()).expect("unblock AfterResume");
        after_resume_task.await;

        let recovery = driver_under_test.next_health_alert().await.expect("resume recovery");
        match recovery {
            HealthAlert::SelfFault(resp) => assert!(resp.conditions.is_none()),
            other => panic!("Unexpected alert: {other:?}"),
        }

        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_stalled_ping_wrapper_timeout_returns_stall_condition() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let (_ping_hold_tx, ping_hold_rx) = oneshot::channel::<()>();
        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        {
            let inner = &driver_under_test.get_driver().unwrap().driver;
            *inner.ping_blocker.lock() = Some(ping_hold_rx);
            *inner.suspend_blocker.lock() = Some(unblock_rx);
        }

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("set level 1");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("set level 0");
        });

        let _stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");

        // Slow wrapped Ping hits the 100ms wrapper timeout and still returns POWER_TRANSITION_STALLED.
        let ping_resp = driver_under_test
            .health_check(HealthCheckRequest::Ping)
            .await
            .expect("timed-out ping succeeds with active stall condition");
        assert!(ping_resp.diagnostics.as_ref().unwrap().iter().any(|d| {
            d.key == "inner_health_check_timed_out"
                && d.value == HealthDiagnosticValue::BoolValue(true)
        }));
        assert_eq!(
            ping_resp.conditions.as_ref().unwrap()[0].reason_code.as_deref(),
            Some(POWER_TRANSITION_STALLED_REASON_CODE)
        );

        unblock_tx.send(()).expect("unblock transition");
        set_level_task.await;
        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_stalled_deep_check_runs_without_wrapper_timeout() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        *driver_under_test.get_driver().unwrap().driver.deep_delay.lock() =
            Some(MonotonicDuration::from_millis(140));

        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        *driver_under_test.get_driver().unwrap().driver.suspend_blocker.lock() = Some(unblock_rx);

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("set level 1");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("set level 0");
        });

        let _stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");

        // Deep taking 140ms (>100ms Ping timeout) runs to completion without wrapper cancellation.
        let deep_resp = driver_under_test
            .health_check(HealthCheckRequest::Deep)
            .await
            .expect("deep check completes without wrapper timeout");
        let deep_conds = deep_resp.conditions.expect("deep conditions");
        assert_eq!(deep_conds.len(), 2);
        assert!(deep_conds.iter().any(|c| c.reason_code.as_deref() == Some("DEEP_PROBE_NOTE")));
        assert!(
            deep_conds.iter().any(|c| {
                c.reason_code.as_deref() == Some(POWER_TRANSITION_STALLED_REASON_CODE)
            })
        );

        unblock_tx.send(()).expect("unblock transition");
        set_level_task.await;
        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_ping_wrapper_timeout_returns_error_if_stall_clears_with_unrelated_fault() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        driver_under_test.get_driver().unwrap().driver.health_latch.raise(HealthCondition {
            status: Some(HealthStatus::Degraded),
            reason_code: Some("BUS_TIMEOUT".to_string()),
            ..Default::default()
        });
        let _initial_alert = driver_under_test.next_health_alert().await.expect("bus timeout");

        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        let (_ping_hold_tx, ping_hold_rx) = oneshot::channel::<()>();
        {
            let inner = &driver_under_test.get_driver().unwrap().driver;
            *inner.suspend_blocker.lock() = Some(unblock_rx);
            *inner.on_ping_entered.lock() = Some(unblock_tx);
            *inner.ping_blocker.lock() = Some(ping_hold_rx);
        }

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("set level 1");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            runner_proxy.set_level(0).await.expect("set level 0");
        });

        let _stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");

        // Start a blocked Ping while stalled; `on_ping_entered` clears the power stall only once
        // the inner Ping has begun (after `Suspendable::health_check` entered its 100ms stalled
        // timeout path). Because POWER_TRANSITION_STALLED is gone when the wrapper timeout fires,
        // the incomplete Ping must return Err(Timeout) rather than Ok([BUS_TIMEOUT]).
        let ping_res = driver_under_test.health_check(HealthCheckRequest::Ping).await;
        assert_eq!(ping_res, Err(HealthCheckError::Timeout));
        if let Some(tx) =
            driver_under_test.get_driver().unwrap().driver.on_ping_entered.lock().take()
        {
            let _ = tx.send(());
        }
        set_level_task.await;

        let ping_after = driver_under_test
            .health_check(HealthCheckRequest::Ping)
            .await
            .expect("unblocked ping succeeds after stall clears");
        let after_conds = ping_after.conditions.expect("remaining BUS_TIMEOUT condition");
        assert_eq!(after_conds.len(), 1);
        assert_eq!(after_conds[0].reason_code.as_deref(), Some("BUS_TIMEOUT"));

        driver_under_test.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_stop_during_stall_preserves_latched_state_without_clearing() {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<fpower_broker::ElementRunnerMarker>();
        let mut harness = TestHarness::<Suspendable<TestDriver>>::new().set_power_element_args(
            fdf::PowerElementArgs {
                runner_server: Some(fidl_next::ServerEnd::from_untyped(
                    runner_server.into_channel(),
                )),
                ..Default::default()
            },
        );
        let mut driver_under_test = harness.start_driver().await.expect("Failed to start driver");
        let shared_latch = driver_under_test.get_driver().unwrap().health_latch.clone();

        let (unblock_tx, unblock_rx) = oneshot::channel::<()>();
        let (stop_entered_tx, stop_entered_rx) = oneshot::channel::<()>();
        let (stop_unblock_tx, stop_unblock_rx) = oneshot::channel::<()>();
        {
            let inner = &driver_under_test.get_driver().unwrap().driver;
            *inner.suspend_blocker.lock() = Some(unblock_rx);
            *inner.on_stop_entered.lock() = Some(stop_entered_tx);
            *inner.stop_blocker.lock() = Some(stop_unblock_rx);
        }

        let runner_proxy = runner_client.into_proxy();
        runner_proxy.set_level(1).await.expect("set level 1");

        let scope = fasync::Scope::new();
        let set_level_task = scope.compute(async move {
            let _ = runner_proxy.set_level(0).await;
        });

        let _stall_alert = driver_under_test.next_health_alert().await.expect("stall alert");

        // Stop the driver while the stall is active; once `Suspendable::stop` has set `stopping`
        // and entered `TestDriver::stop`, unblock the transition so it completes during teardown
        // without clearing the latch.
        futures::future::join(driver_under_test.stop_driver(), async move {
            let _ = stop_entered_rx.await;
            let _ = unblock_tx.send(());
            set_level_task.await;
            let _ = stop_unblock_tx.send(());
        })
        .await;

        assert!(
            shared_latch.snapshot().iter().any(|c| {
                c.reason_code.as_deref() == Some(POWER_TRANSITION_STALLED_REASON_CODE)
            })
        );
    }
}
