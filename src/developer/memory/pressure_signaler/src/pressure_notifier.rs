// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::pressure_observer::Level;
use fidl::endpoints::{ClientEnd, Proxy as _};
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_memory_debug as fdebug;
use fidl_fuchsia_memorypressure as fmp;
use fuchsia_async as fasync;
use futures::StreamExt as _;
use log::{debug, error, info, warn};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const DEFAULT_CRITICAL_CRASH_REPORT_INTERVAL_MINUTES: i64 = 30;

#[derive(Debug)]
struct WatcherState {
    proxy: fmp::WatcherProxy,
    pending_callback: Cell<bool>,
}

#[derive(Debug)]
struct PressureNotifierInner {
    watchers: Vec<Rc<WatcherState>>,
    /// Start off with `Level::Normal` before the right kernel level has been discovered, so that
    /// `PressureNotifier` notifies clients with a valid level until the level has been initialized.
    ///
    /// We can end up in this uninitialized state if a watcher registers before `PressureObserver`
    /// has discovered the initial system memory pressure level. Since watcher registration is
    /// supposed to return the current level, advertise the current level as `Normal`. This is fine
    /// because once the level is initialized, `post_level_change` will send another signal if the
    /// level is not `Normal`.
    current_level: Level,
    observed_normal_level: bool,
    previous_critical_crash_report_time: Option<zx::MonotonicInstant>,
    critical_crash_report_interval: zx::MonotonicDuration,
}

/// Dispatches memory pressure level changes to registered `fuchsia.memorypressure.Watcher` clients
/// and files crash reports on critical pressure / imminent OOM transitions.
#[derive(Debug)]
pub struct PressureNotifier {
    inner: RefCell<PressureNotifierInner>,
    send_critical_pressure_crash_reports: bool,
    crash_reporter: ffeedback::CrashReporterProxy,
    scope: fasync::Scope,
}

impl PressureNotifier {
    /// Creates a new [`PressureNotifier`].
    pub fn new(
        send_critical_pressure_crash_reports: bool,
        crash_reporter: ffeedback::CrashReporterProxy,
    ) -> Rc<Self> {
        Rc::new(Self {
            inner: RefCell::new(PressureNotifierInner {
                watchers: Vec::new(),
                current_level: Level::Normal,
                observed_normal_level: true,
                previous_critical_crash_report_time: None,
                critical_crash_report_interval: zx::MonotonicDuration::from_minutes(
                    DEFAULT_CRITICAL_CRASH_REPORT_INTERVAL_MINUTES,
                ),
            }),
            send_critical_pressure_crash_reports,
            crash_reporter,
            scope: fasync::Scope::new(),
        })
    }

    /// Serves a client request stream for `fuchsia.memorypressure.Provider`.
    pub fn handle_provider_stream(self: &Rc<Self>, mut stream: fmp::ProviderRequestStream) {
        let notifier_weak = Rc::downgrade(self);
        self.scope.spawn_local(async move {
            while let Some(request_result) = stream.next().await {
                match request_result {
                    Ok(fmp::ProviderRequest::RegisterWatcher { watcher, control_handle: _ }) => {
                        let Some(notifier) = notifier_weak.upgrade() else {
                            return;
                        };
                        notifier.register_watcher(watcher);
                    }
                    Err(error) => {
                        warn!("Error in Provider request stream: {error}");
                        break;
                    }
                }
            }
        });
    }

    /// Serves a client request stream for `fuchsia.memory.debug.MemoryPressure`.
    pub fn handle_debug_stream(self: &Rc<Self>, mut stream: fdebug::MemoryPressureRequestStream) {
        let notifier_weak = Rc::downgrade(self);
        self.scope.spawn_local(async move {
            while let Some(request_result) = stream.next().await {
                match request_result {
                    Ok(fdebug::MemoryPressureRequest::Signal { level, control_handle: _ }) => {
                        let Some(notifier) = notifier_weak.upgrade() else {
                            return;
                        };
                        notifier.debug_notify(level);
                    }
                    Err(error) => {
                        warn!("Error in MemoryPressure debug request stream: {error}");
                        break;
                    }
                }
            }
        });
    }

    fn register_watcher(self: &Rc<Self>, watcher_client_end: ClientEnd<fmp::WatcherMarker>) {
        let proxy = watcher_client_end.into_proxy();
        let fidl_level = self.inner.borrow().current_level.for_watcher();
        let watcher = Rc::new(WatcherState { proxy, pending_callback: Cell::new(true) });

        let watcher_for_closed = Rc::clone(&watcher);
        let notifier_weak = Rc::downgrade(self);
        self.scope.spawn_local(async move {
            let _ = watcher_for_closed.proxy.on_closed().await;
            if let Some(notifier) = notifier_weak.upgrade() {
                notifier.release_watcher(&watcher_for_closed);
            }
        });

        self.inner.borrow_mut().watchers.push(Rc::clone(&watcher));
        self.notify_watcher(watcher, fidl_level);
    }

    /// Processes a kernel memory pressure `level_to_send` transition, filing crash reports if
    /// configured and notifying registered watchers.
    pub fn post_level_change(self: &Rc<Self>, level_to_send: Level) {
        self.inner.borrow_mut().current_level = level_to_send;

        let fidl_level = match level_to_send {
            Level::ImminentOom => {
                // We condition sending a crash report for imminent OOM the same way as for
                // critical memory pressure.
                if self.send_critical_pressure_crash_reports {
                    self.file_crash_report("fuchsia-imminent-oom");
                }
                // Nothing else to do. This is a diagnostic-only level that is not signaled to
                // watchers.
                return;
            }
            Level::Critical => {
                if self.send_critical_pressure_crash_reports
                    && self.can_generate_new_critical_crash_reports()
                {
                    let mut inner = self.inner.borrow_mut();
                    inner.previous_critical_crash_report_time = Some(zx::MonotonicInstant::get());
                    inner.observed_normal_level = false;
                    drop(inner);
                    // File crash report before notifying watchers, so that we can capture the
                    // state *before* watchers can respond to memory pressure, thereby changing the
                    // state that caused the memory pressure in the first place.
                    self.file_crash_report("fuchsia-critical-memory-pressure");
                }
                fmp::Level::Critical
            }
            Level::Warning => fmp::Level::Warning,
            Level::Normal => {
                self.inner.borrow_mut().observed_normal_level = true;
                fmp::Level::Normal
            }
        };

        // TODO(rashaeqbal): Throttle notifications to prevent thrashing.
        let watchers = self.inner.borrow().watchers.clone();
        for watcher in watchers {
            // Notify the watcher only if we received a response for the previous level change, i.e.
            // there is no pending callback.
            if !watcher.pending_callback.replace(true) {
                self.notify_watcher(watcher, fidl_level);
            }
        }
    }

    /// Notifies watchers with a simulated memory pressure `level`. For diagnostic use by
    /// `fuchsia.memory.debug.MemoryPressure`.
    pub fn debug_notify(&self, level: fmp::Level) {
        info!("Simulating memory pressure level {}", Level::from(level));

        let watchers = self.inner.borrow().watchers.clone();
        for watcher in watchers {
            let proxy = watcher.proxy.clone();
            self.scope.spawn_local(async move {
                if let Err(error) = proxy.on_level_changed(level).await {
                    if error.is_closed() {
                        debug!(
                            "Failed to simulate pressure level signal (channel closed): {error}"
                        );
                    } else {
                        error!("Failed to simulate pressure level signal: {error}");
                    }
                }
            });
        }
    }

    fn notify_watcher(self: &Rc<Self>, watcher: Rc<WatcherState>, mut level_to_send: fmp::Level) {
        let notifier_weak = Rc::downgrade(self);
        self.scope.spawn_local(async move {
            loop {
                match watcher.proxy.on_level_changed(level_to_send).await {
                    Ok(()) => {
                        let Some(notifier) = notifier_weak.upgrade() else {
                            return;
                        };
                        let current_level = notifier.inner.borrow().current_level.for_watcher();
                        // The watcher might have missed a level change if it occurred before this
                        // callback completed. If the level has changed, notify the watcher.
                        if level_to_send == current_level {
                            watcher.pending_callback.set(false);
                            return;
                        }
                        level_to_send = current_level;
                    }
                    Err(error) => {
                        if error.is_closed() {
                            debug!("Failed to signal pressure change (channel closed): {error}");
                        } else {
                            error!("Failed to signal pressure change: {error}");
                        }
                        if let Some(notifier) = notifier_weak.upgrade() {
                            notifier.release_watcher(&watcher);
                        }
                        return;
                    }
                }
            }
        });
    }

    fn release_watcher(&self, watcher: &Rc<WatcherState>) {
        self.inner.borrow_mut().watchers.retain(|w| !Rc::ptr_eq(w, watcher));
    }

    fn can_generate_new_critical_crash_reports(&self) -> bool {
        // Generate a new Critical crash report only if any of these two conditions hold:
        // 1. `observed_normal_level` is set to true, which indicates that a Normal level was
        //    observed after the last Critical crash report.
        // 2. At least `critical_crash_report_interval` time has elapsed since the last Critical
        //    crash report.
        //
        // This is done for two reasons:
        // 1) It helps limit the number of Critical crash reports we generate.
        // 2) If the memory pressure changes to Critical again after going via Normal, we're
        //    presumably observing a different memory usage pattern / use case, so it makes sense to
        //    generate a new crash report. Instead if we're only observing Critical -> Warning ->
        //    Critical transitions, we might be seeing the same memory usage pattern repeat.
        let inner = self.inner.borrow();
        inner.observed_normal_level
            || inner.previous_critical_crash_report_time.is_none_or(|previous_time| {
                zx::MonotonicInstant::get() >= previous_time + inner.critical_crash_report_interval
            })
    }

    fn file_crash_report(&self, signature: &str) {
        let report = ffeedback::CrashReport {
            program_name: Some("system".to_string()),
            program_uptime: Some(zx::MonotonicInstant::get().into_nanos()),
            crash_signature: Some(signature.to_string()),
            is_fatal: Some(false),
            ..Default::default()
        };

        let crash_reporter = self.crash_reporter.clone();
        self.scope.spawn_local(async move {
            match crash_reporter.file_report(report).await {
                Ok(Ok(_)) => {}
                Ok(Err(filing_error)) => {
                    error!("Failed to file a report: {:?}", filing_error);
                }
                Err(fidl_error) => {
                    error!("Failed to file a report: {fidl_error}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;

    struct CrashReporterForTest {
        num_crash_reports: Rc<Cell<usize>>,
        _task: fasync::Task<()>,
    }

    impl CrashReporterForTest {
        fn new() -> (Self, ffeedback::CrashReporterProxy) {
            let (proxy, mut stream) = create_proxy_and_stream::<ffeedback::CrashReporterMarker>();
            let num_crash_reports = Rc::new(Cell::new(0));
            let count_clone = Rc::clone(&num_crash_reports);
            let task = fasync::Task::local(async move {
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        ffeedback::CrashReporterRequest::FileReport { report: _, responder } => {
                            count_clone.set(count_clone.get() + 1);
                            let _ = responder.send(Ok(&ffeedback::FileReportResults::default()));
                        }
                    }
                }
            });
            (Self { num_crash_reports, _task: task }, proxy)
        }

        fn num_crash_reports(&self) -> usize {
            self.num_crash_reports.get()
        }
    }

    struct PressureWatcherForTest {
        changes: Rc<Cell<usize>>,
        last_level: Rc<Cell<Option<fmp::Level>>>,
        stashed_responder: Rc<RefCell<Option<fmp::WatcherOnLevelChangedResponder>>>,
        _task: fasync::Task<()>,
    }

    impl PressureWatcherForTest {
        fn new(provider: &fmp::ProviderProxy, send_responses: bool) -> Self {
            let (client_end, mut stream) =
                fidl::endpoints::create_request_stream::<fmp::WatcherMarker>();
            provider.register_watcher(client_end).expect("Failed to send register_watcher");

            let changes = Rc::new(Cell::new(0));
            let last_level = Rc::new(Cell::new(None));
            let stashed_responder = Rc::new(RefCell::new(None));

            let changes_clone = Rc::clone(&changes);
            let last_level_clone = Rc::clone(&last_level);
            let stashed_clone = Rc::clone(&stashed_responder);

            let task = fasync::Task::local(async move {
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fmp::WatcherRequest::OnLevelChanged { level, responder } => {
                            changes_clone.set(changes_clone.get() + 1);
                            last_level_clone.set(Some(level));
                            if send_responses {
                                let _ = responder.send();
                            } else {
                                *stashed_clone.borrow_mut() = Some(responder);
                            }
                        }
                    }
                }
            });

            Self { changes, last_level, stashed_responder, _task: task }
        }

        fn respond(&self) {
            let responder =
                self.stashed_responder.borrow_mut().take().expect("Expected stashed responder");
            responder.send().expect("Failed to send OnLevelChanged response");
        }

        fn num_changes(&self) -> usize {
            self.changes.get()
        }

        fn last_level(&self) -> fmp::Level {
            self.last_level.get().expect("No level received yet")
        }
    }

    struct TestFixture {
        notifier: Rc<PressureNotifier>,
        crash_reporter: CrashReporterForTest,
        provider_proxy: fmp::ProviderProxy,
        memory_debug_proxy: Option<fdebug::MemoryPressureProxy>,
        executor: fasync::TestExecutor,
    }

    impl TestFixture {
        fn new() -> Self {
            Self::with_crash_reports(true)
        }

        fn with_crash_reports(send_critical_pressure_crash_reports: bool) -> Self {
            let mut executor = fasync::TestExecutor::new();
            let (crash_reporter, crash_reporter_proxy) = CrashReporterForTest::new();
            let notifier =
                PressureNotifier::new(send_critical_pressure_crash_reports, crash_reporter_proxy);
            let (provider_proxy, stream) = create_proxy_and_stream::<fmp::ProviderMarker>();
            notifier.handle_provider_stream(stream);
            let _ = executor.run_until_stalled(&mut std::future::pending::<()>());
            Self { executor, notifier, crash_reporter, provider_proxy, memory_debug_proxy: None }
        }

        fn run_loop_until_idle(&mut self) {
            let _ = self.executor.run_until_stalled(&mut std::future::pending::<()>());
        }

        fn register_watcher(&self, send_responses: bool) -> PressureWatcherForTest {
            PressureWatcherForTest::new(&self.provider_proxy, send_responses)
        }

        fn trigger_level_change(&mut self, level: Level) {
            self.notifier.post_level_change(level);
            self.run_loop_until_idle();
        }

        fn watcher_count(&self) -> usize {
            self.notifier.inner.borrow().watchers.len()
        }

        fn setup_memory_debug_service(&mut self) {
            let (proxy, stream) = create_proxy_and_stream::<fdebug::MemoryPressureMarker>();
            self.notifier.handle_debug_stream(stream);
            self.memory_debug_proxy = Some(proxy);
        }

        fn test_simulated_pressure(&self, level: fmp::Level) {
            let proxy = self
                .memory_debug_proxy
                .as_ref()
                .expect("setup_memory_debug_service must be called first");
            proxy.signal(level).expect("Failed to signal simulated pressure");
        }

        fn set_crash_report_interval(&self, minutes: i64) {
            self.notifier.inner.borrow_mut().critical_crash_report_interval =
                zx::MonotonicDuration::from_minutes(minutes);
        }

        fn can_generate_new_critical_crash_reports(&self) -> bool {
            self.notifier.can_generate_new_critical_crash_reports()
        }

        fn num_crash_reports(&self) -> usize {
            self.crash_reporter.num_crash_reports()
        }
    }

    #[fuchsia::test]
    fn watcher_notifications_and_disconnect() {
        let mut fixture = TestFixture::new();
        {
            let watcher1 = fixture.register_watcher(true);
            let watcher2 = fixture.register_watcher(true);
            fixture.run_loop_until_idle();
            assert_eq!(fixture.watcher_count(), 2);
            assert_eq!(watcher1.num_changes(), 1);
            assert_eq!(watcher2.num_changes(), 1);

            fixture.trigger_level_change(Level::Normal);
            assert_eq!(watcher1.num_changes(), 2);
            assert_eq!(watcher2.num_changes(), 2);
        }

        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 0);
    }

    #[fuchsia::test]
    fn delayed_and_mixed_watcher_responses() {
        let mut fixture = TestFixture::new();
        fixture.trigger_level_change(Level::Normal);

        let delayed_watcher = fixture.register_watcher(false);
        let immediate_watcher = fixture.register_watcher(true);
        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 2);
        assert_eq!(delayed_watcher.num_changes(), 1);
        assert_eq!(immediate_watcher.num_changes(), 1);

        // `delayed_watcher` has not acknowledged the initial callback yet, so only
        // `immediate_watcher` receives the new Warning level right away.
        fixture.trigger_level_change(Level::Warning);
        assert_eq!(delayed_watcher.num_changes(), 1);
        assert_eq!(immediate_watcher.num_changes(), 2);
        assert_eq!(immediate_watcher.last_level(), fmp::Level::Warning);

        // Once `delayed_watcher` responds, it immediately receives the follow-up Warning level.
        delayed_watcher.respond();
        fixture.run_loop_until_idle();
        assert_eq!(delayed_watcher.num_changes(), 2);
        assert_eq!(delayed_watcher.last_level(), fmp::Level::Warning);
        assert_eq!(immediate_watcher.num_changes(), 2);
    }

    #[fuchsia::test]
    fn watcher_does_not_see_imminent_oom() {
        let mut fixture = TestFixture::new();

        fixture.trigger_level_change(Level::ImminentOom);
        let watcher = fixture.register_watcher(true);
        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 1);
        assert_eq!(watcher.num_changes(), 1);
        assert_eq!(watcher.last_level(), fmp::Level::Critical);

        fixture.trigger_level_change(Level::Warning);
        assert_eq!(watcher.num_changes(), 2);
        assert_eq!(watcher.last_level(), fmp::Level::Warning);

        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(watcher.num_changes(), 2);
        assert_eq!(watcher.last_level(), fmp::Level::Warning);
    }

    #[fuchsia::test]
    fn delayed_watcher_does_not_see_imminent_oom() {
        let mut fixture = TestFixture::new();

        fixture.trigger_level_change(Level::Normal);
        let watcher = fixture.register_watcher(false);
        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 1);
        assert_eq!(watcher.num_changes(), 1);
        assert_eq!(watcher.last_level(), fmp::Level::Normal);

        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(watcher.num_changes(), 1);
        assert_eq!(watcher.last_level(), fmp::Level::Normal);

        watcher.respond();
        fixture.run_loop_until_idle();
        assert_eq!(watcher.num_changes(), 2);
        assert_eq!(watcher.last_level(), fmp::Level::Critical);
    }

    #[fuchsia::test]
    fn no_crash_report_on_non_critical_levels() {
        let mut fixture = TestFixture::new();
        assert_eq!(fixture.num_crash_reports(), 0);
        assert!(fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Warning);
        assert_eq!(fixture.num_crash_reports(), 0);
        assert!(fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Normal);
        assert_eq!(fixture.num_crash_reports(), 0);
        assert!(fixture.can_generate_new_critical_crash_reports());
    }

    #[fuchsia::test]
    fn crash_report_throttling_and_recovery() {
        let mut fixture = TestFixture::new();
        assert_eq!(fixture.num_crash_reports(), 0);
        assert!(fixture.can_generate_new_critical_crash_reports());

        // First Critical transition files a report and starts throttling.
        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 1);
        assert!(!fixture.can_generate_new_critical_crash_reports());

        // Critical -> Warning -> Critical within the interval does not file a new report.
        fixture.trigger_level_change(Level::Warning);
        assert_eq!(fixture.num_crash_reports(), 1);
        assert!(!fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 1);
        assert!(!fixture.can_generate_new_critical_crash_reports());

        // Once the interval has elapsed, a Critical transition files a new report.
        fixture.set_crash_report_interval(0);
        assert!(fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 2);

        // Restoring the 30-minute interval throttles subsequent Critical transitions again.
        fixture.set_crash_report_interval(30);
        assert!(!fixture.can_generate_new_critical_crash_reports());

        // Recovering to Normal resets throttling immediately regardless of the interval.
        fixture.trigger_level_change(Level::Normal);
        assert_eq!(fixture.num_crash_reports(), 2);
        assert!(fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 3);
        assert!(!fixture.can_generate_new_critical_crash_reports());
    }

    #[fuchsia::test]
    fn do_not_send_critical_pressure_crash_report() {
        let mut fixture = TestFixture::with_crash_reports(false);
        assert_eq!(fixture.num_crash_reports(), 0);
        assert!(fixture.can_generate_new_critical_crash_reports());

        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 0);

        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(fixture.num_crash_reports(), 0);
    }

    #[fuchsia::test]
    fn crash_reports_on_oom_and_critical_independence() {
        let mut fixture = TestFixture::new();
        assert_eq!(fixture.num_crash_reports(), 0);

        // ImminentOom always generates crash reports without throttling.
        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(fixture.num_crash_reports(), 1);

        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(fixture.num_crash_reports(), 2);

        // ImminentOom does not prevent Critical from filing a report, nor vice versa.
        fixture.trigger_level_change(Level::Critical);
        assert_eq!(fixture.num_crash_reports(), 3);

        fixture.trigger_level_change(Level::ImminentOom);
        assert_eq!(fixture.num_crash_reports(), 4);
    }

    #[fuchsia::test]
    fn simulate_pressure() {
        let mut fixture = TestFixture::new();
        {
            let watcher1 = fixture.register_watcher(true);
            let watcher2 = fixture.register_watcher(true);
            fixture.run_loop_until_idle();
            assert_eq!(fixture.watcher_count(), 2);
            assert_eq!(watcher1.num_changes(), 1);
            assert_eq!(watcher2.num_changes(), 1);

            fixture.setup_memory_debug_service();

            for (expected_changes, level) in [
                (2, fmp::Level::Critical),
                (3, fmp::Level::Warning),
                (4, fmp::Level::Warning),
                (5, fmp::Level::Normal),
            ] {
                fixture.test_simulated_pressure(level);
                fixture.run_loop_until_idle();
                assert_eq!(watcher1.num_changes(), expected_changes);
                assert_eq!(watcher2.num_changes(), expected_changes);
            }

            fixture.trigger_level_change(Level::Normal);
            assert_eq!(watcher1.num_changes(), 6);
            assert_eq!(watcher2.num_changes(), 6);
        }

        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 0);
    }

    #[fuchsia::test]
    fn watcher_disconnects_with_pending_callback() {
        let mut fixture = TestFixture::new();
        {
            let watcher = fixture.register_watcher(false);
            fixture.run_loop_until_idle();
            assert_eq!(fixture.watcher_count(), 1);
            assert_eq!(watcher.num_changes(), 1);
            // `watcher` drops here without responding to the pending `OnLevelChanged` call.
        }

        fixture.run_loop_until_idle();
        assert_eq!(fixture.watcher_count(), 0);
    }
}
