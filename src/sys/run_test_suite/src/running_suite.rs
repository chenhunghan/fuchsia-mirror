// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cancel::{Cancelled, NamedFutureExt, OrCancel};
use crate::outcome::{
    ExtendedOutcome, Lifecycle, Outcome, RunTestSuiteError, UnexpectedEventError,
};
use crate::output::{self, ArtifactType, CaseId, SuiteReporter, Timestamp};
use crate::stream_util::StreamUtil;
use crate::trace::duration;
use crate::{artifacts, diagnostics};
use diagnostics_data::Severity;
use flex_fuchsia_test_manager::{
    self as ftest_manager, LaunchError, SuiteArtifactGeneratedEventDetails,
    SuiteStoppedEventDetails, TestCaseArtifactGeneratedEventDetails, TestCaseFinishedEventDetails,
    TestCaseFoundEventDetails, TestCaseStartedEventDetails, TestCaseStoppedEventDetails,
};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::future::Either;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use log::{error, info, warn};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Struct used by |run_suite_and_collect_logs| to track the state of test cases and suites.
struct CollectedEntityState<R> {
    reporter: R,
    name: String,
    lifecycle: Lifecycle,
    artifact_tasks:
        Vec<fasync::Task<Result<Option<diagnostics::LogCollectionOutcome>, anyhow::Error>>>,
}

/// Collects results and artifacts for a single suite.
// TODO(satsukiu): There's two ways to return an error here:
// * Err(RunTestSuiteError)
// * Ok(Outcome::Error(RunTestSuiteError))
// We should consider how to consolidate these.
pub(crate) async fn run_suite_and_collect_logs<F: Future<Output = ()> + Unpin>(
    running_suite: RunningSuite,
    suite_reporter: &SuiteReporter<'_>,
    log_display: diagnostics::LogDisplayConfiguration,
    cancel_fut: F,
) -> Result<ExtendedOutcome, RunTestSuiteError> {
    duration!("collect_suite");

    let RunningSuite {
        mut event_stream,
        timeout,
        timeout_grace,
        max_severity_logs,
        no_cases_equals_success,
        ..
    } = running_suite;

    let log_opts =
        diagnostics::LogCollectionOptions { format: log_display, max_severity: max_severity_logs };

    let mut test_cases: HashMap<u32, CollectedEntityState<_>> = HashMap::new();
    let mut suite_state = CollectedEntityState {
        reporter: suite_reporter,
        name: "".to_string(),
        lifecycle: Lifecycle::Found,
        artifact_tasks: vec![],
    };
    let mut suite_finish_timestamp = Timestamp::Unknown;
    let mut outcome = Outcome::Passed;

    // This future collects results by handling test manager events arriving via `event_stream`.
    // The state of individual cases is tracked in `test_cases`, and the state of the suite is
    // tracked in `suite_state`. An error is returned if collection could not be completed.
    // The result type is `Result<(), RunTestSuiteError>`.
    let collect_results_fut = async {
        while let Some(event_result) = event_stream.next().named("next_event").await {
            match event_result {
                Err(e) => match (e, no_cases_equals_success) {
                    (RunTestSuiteError::Launch(LaunchError::NoMatchingCases), Some(true)) => {
                        suite_state.lifecycle = Lifecycle::Stopped;
                    }
                    (e, _) => {
                        suite_state
                            .reporter
                            .stopped(&output::ReportedOutcome::Error, Timestamp::Unknown)?;
                        return Err(e);
                    }
                },
                Ok(event) => {
                    let timestamp = Timestamp::from_nanos(event.timestamp);
                    match event.details.expect("event cannot be None") {
                        ftest_manager::EventDetails::TestCaseFound(TestCaseFoundEventDetails {
                            test_case_name: Some(test_case_name),
                            test_case_id: Some(test_case_id),
                            ..
                        }) => {
                            if test_cases.contains_key(&test_case_id) {
                                return Err(UnexpectedEventError::InvalidCaseEvent {
                                    last_state: Lifecycle::Found,
                                    next_state: Lifecycle::Found,
                                    test_case_name,
                                    test_case_id,
                                }
                                .into());
                            }
                            test_cases.insert(
                                test_case_id,
                                CollectedEntityState {
                                    reporter: suite_reporter
                                        .new_case(&test_case_name, &CaseId(test_case_id))?,
                                    name: test_case_name,
                                    lifecycle: Lifecycle::Found,
                                    artifact_tasks: vec![],
                                },
                            );
                        }
                        ftest_manager::EventDetails::TestCaseStarted(
                            TestCaseStartedEventDetails {
                                test_case_id: Some(test_case_id), ..
                            },
                        ) => {
                            let entry = test_cases.get_mut(&test_case_id).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Started,
                                    test_case_id,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Found => {
                                    // TODO(https://fxbug.dev/42159975): Record per-case runtime once we have an
                                    // accurate way to measure it.
                                    entry.reporter.started(Timestamp::Unknown)?;
                                    entry.lifecycle = Lifecycle::Started;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Started,
                                        test_case_name: entry.name.clone(),
                                        test_case_id,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::EventDetails::TestCaseArtifactGenerated(
                            TestCaseArtifactGeneratedEventDetails {
                                test_case_id: Some(test_case_id),
                                artifact: Some(artifact),
                                ..
                            },
                        ) => {
                            let entry = test_cases.get_mut(&test_case_id).ok_or(
                                UnexpectedEventError::CaseArtifactButNotFound { test_case_id },
                            )?;
                            if matches!(entry.lifecycle, Lifecycle::Finished) {
                                return Err(UnexpectedEventError::CaseArtifactButFinished {
                                    test_case_id,
                                }
                                .into());
                            }
                            let artifact_fut = artifacts::drain_artifact(
                                &entry.reporter,
                                artifact,
                                log_opts.clone(),
                            )
                            .await?;
                            entry.artifact_tasks.push(fasync::Task::spawn(artifact_fut));
                        }
                        ftest_manager::EventDetails::TestCaseStopped(
                            TestCaseStoppedEventDetails {
                                test_case_id: Some(test_case_id),
                                result: Some(result),
                                ..
                            },
                        ) => {
                            let entry = test_cases.get_mut(&test_case_id).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Stopped,
                                    test_case_id,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Started => {
                                    // TODO(https://fxbug.dev/42159975): Record per-case runtime once we have an
                                    // accurate way to measure it.
                                    entry.reporter.stopped(&result.into(), Timestamp::Unknown)?;
                                    entry.lifecycle = Lifecycle::Stopped;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Stopped,
                                        test_case_name: entry.name.clone(),
                                        test_case_id,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::EventDetails::TestCaseFinished(
                            TestCaseFinishedEventDetails {
                                test_case_id: Some(test_case_id), ..
                            },
                        ) => {
                            let entry = test_cases.get_mut(&test_case_id).ok_or(
                                UnexpectedEventError::CaseEventButNotFound {
                                    next_state: Lifecycle::Finished,
                                    test_case_id,
                                },
                            )?;
                            match &entry.lifecycle {
                                Lifecycle::Stopped => {
                                    // don't mark reporter finished yet, we want to finish draining
                                    // artifacts separately.
                                    entry.lifecycle = Lifecycle::Finished;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidCaseEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Finished,
                                        test_case_name: entry.name.clone(),
                                        test_case_id,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::EventDetails::SuiteArtifactGenerated(
                            SuiteArtifactGeneratedEventDetails { artifact: Some(artifact), .. },
                        ) => {
                            let artifact_fut = artifacts::drain_artifact(
                                suite_reporter,
                                artifact,
                                log_opts.clone(),
                            )
                            .await?;
                            suite_state.artifact_tasks.push(fasync::Task::spawn(artifact_fut));
                        }
                        ftest_manager::EventDetails::SuiteStarted(_) => {
                            match &suite_state.lifecycle {
                                Lifecycle::Found => {
                                    suite_state.reporter.started(timestamp)?;
                                    suite_state.lifecycle = Lifecycle::Started;
                                }
                                other => {
                                    return Err(UnexpectedEventError::InvalidEvent {
                                        last_state: *other,
                                        next_state: Lifecycle::Started,
                                    }
                                    .into());
                                }
                            }
                        }
                        ftest_manager::EventDetails::SuiteStopped(SuiteStoppedEventDetails {
                            result: Some(result),
                            ..
                        }) => match &suite_state.lifecycle {
                            Lifecycle::Started => {
                                suite_state.lifecycle = Lifecycle::Stopped;
                                suite_finish_timestamp = timestamp;
                                outcome = match result {
                                    ftest_manager::SuiteResult::Finished => Outcome::Passed,
                                    ftest_manager::SuiteResult::Failed => Outcome::Failed,
                                    ftest_manager::SuiteResult::DidNotFinish => {
                                        Outcome::Inconclusive
                                    }
                                    ftest_manager::SuiteResult::TimedOut
                                    | ftest_manager::SuiteResult::Stopped => Outcome::Timedout,
                                    ftest_manager::SuiteResult::InternalError => Outcome::error(
                                        UnexpectedEventError::InternalErrorSuiteResult,
                                    ),
                                    r => {
                                        return Err(
                                            UnexpectedEventError::UnrecognizedSuiteResult {
                                                result: r,
                                            }
                                            .into(),
                                        );
                                    }
                                };
                            }
                            other => {
                                return Err(UnexpectedEventError::InvalidEvent {
                                    last_state: *other,
                                    next_state: Lifecycle::Stopped,
                                }
                                .into());
                            }
                        },
                        ftest_manager::EventDetailsUnknown!() => {
                            warn!("Encountered unrecognized suite event");
                        }
                    }
                }
            }
        }
        drop(event_stream); // Explicit drop here to force ownership move.
        Ok(())
    }
    .boxed_local();

    // `kill_timeout_future` never completes if no timeout is set. If a timeout is set,
    // it completes after the timeout interval, in which case it returns `()`.
    let start_time = std::time::Instant::now();
    let kill_timeout_future = match timeout {
        None => futures::future::pending::<()>().boxed(),
        Some(duration) => fasync::Timer::new(start_time + duration + timeout_grace).boxed(),
    };

    // `kill_fut` combines the timeout future with the cancel future that was passed in.
    // If it completes, it returns `Outcome::Cancelled`. or `Outcome::Timedout`.
    let kill_fut = async move {
        match futures::future::select(cancel_fut, kill_timeout_future).await {
            Either::Left(_) => Outcome::Cancelled,
            Either::Right(_) => Outcome::Timedout,
        }
    }
    .shared();

    let early_termination_outcome: Option<Outcome> =
        // `or_cancelled` here wraps the result of `collect_results_fut` in another result.
        // That outer result is an error if collection is cancelled or times out.
        match collect_results_fut.boxed_local().or_cancelled(kill_fut.clone()).await {
            // Collection completed successfully. `early_termination_outcome` will be `None`.
            Ok(Ok(())) => None,
            // Collection completed with an error. Return early from this function with an
            // `ExtendedOutcome` that wraps the error.
            Ok(Err(e)) => return Ok(ExtendedOutcome {
                outcome: Outcome::Error { origin: Arc::new(e) },
                // We regard setup as having succeeded if the `SuiteStarted` event was received.
                // If it was not received, the suite's lifecycle will be `Found`.
                setup_succeeded: Some(suite_state.lifecycle != Lifecycle::Found),
                teardown_succeeded: if suite_state.lifecycle == Lifecycle::Stopped {
                    // The suite stopped normally, but we're bailing for some reason. Teardown
                    // failed, because we're not completing the rest of the teardown steps below.
                    Some(false)
                } else {
                    // `setup_succeeded` is false, so we don't report a definite
                    // `teardown_succeeded`.
                    None
                },
            }),
            // Collection was cancelled or timed out. Note that `Cancelled` here is the
            // error type returned by `or_cancelled` and does not indicate that collection
            // was necessarily cancelled (it may have timed out). `outcome` here will be
            // `Outcome::Cancelled` or `Outcome::Timedout`.
            Err(Cancelled(outcome)) => Some(outcome),
        };

    // Finish collecting artifacts and report errors.
    info!("Awaiting case artifacts");
    let mut unfinished_test_case_names = vec![];

    let mut teardown_succeeded = true;

    // Here we examine all the test cases that were found. Note that `lifecycle` pertains to
    // the specific case, whereas `early_termination_outcome` is the same value across all
    // cases.
    for (_, test_case) in test_cases.into_iter() {
        let CollectedEntityState { reporter, name, lifecycle, artifact_tasks } = test_case;
        match (lifecycle, early_termination_outcome.clone()) {
            (Lifecycle::Started | Lifecycle::Found, Some(early)) => {
                // The suite run was either cancelled or timed out, and the case either started
                // but did not complete or was never started.
                reporter.stopped(&early.into(), Timestamp::Unknown)?;
            }
            (Lifecycle::Found, None) => {
                // The suite run completed normally, but this test case was never started.
                unfinished_test_case_names.push(name.clone());
                reporter.stopped(&Outcome::Inconclusive.into(), Timestamp::Unknown)?;
            }
            (Lifecycle::Started, None) => {
                // The suite run completed normally. This test case was started, but never
                // finished.
                unfinished_test_case_names.push(name.clone());
                reporter.stopped(&Outcome::DidNotFinish.into(), Timestamp::Unknown)?;
            }
            (Lifecycle::Stopped | Lifecycle::Finished, _) => {
                // This test case run stopped and maybe completed. The suite run may or may
                // not have completed.
            }
        }

        // Wait for all artifacts to be collected for this case.
        let finish_artifacts_fut = FuturesUnordered::from_iter(artifact_tasks)
            .map(|result| match result {
                Err(e) => {
                    error!("Failed to collect artifact for {}: {:?}", name, e);
                    teardown_succeeded = false;
                }
                Ok(Some(_log_result)) => warn!("Unexpectedly got log results for a test case"),
                Ok(None) => (),
            })
            .collect::<()>();
        if let Err(Cancelled(_)) = finish_artifacts_fut.or_cancelled(kill_fut.clone()).await {
            warn!("Stopped polling artifacts for {} due to timeout or cancellation", name);
            teardown_succeeded = false;
        }

        reporter.finished()?;
    }
    if !unfinished_test_case_names.is_empty() {
        outcome = Outcome::error(UnexpectedEventError::CasesDidNotFinish {
            cases: unfinished_test_case_names,
        });
    }

    match (suite_state.lifecycle, early_termination_outcome) {
        (Lifecycle::Found | Lifecycle::Started, Some(early)) => {
            // We terminated early and never got `SuiteStopped`. `outcome` was optimistically
            // set to `Passed`, so we set it to `Cancelled` or `Timedout` depending on how we
            // terminated early. Note that `outcome` cannot be `Failed` here, because we only
            // set it to that if we get `SuiteStopped`.
            if outcome == Outcome::Passed {
                outcome = early;
            }
        }
        (Lifecycle::Found | Lifecycle::Started, None) => {
            // There was no cancellation or timeout, but we never got `SuiteStopped`.
            outcome = Outcome::error(UnexpectedEventError::SuiteDidNotReportStop);
        }
        // If the suite successfully reported a result, don't alter it.
        (Lifecycle::Stopped, _) => (),
        // Finished doesn't happen since there's no SuiteFinished event.
        (Lifecycle::Finished, _) => unreachable!(),
    }

    let restricted_logs_present = AtomicBool::new(false);
    let all_artifacts_finished = AtomicBool::new(true);
    let finish_artifacts_fut = FuturesUnordered::from_iter(suite_state.artifact_tasks)
        .then(|result| async {
            match result {
                Err(e) => {
                    error!("Failed to collect artifact for suite: {:?}", e);
                    all_artifacts_finished.store(false, Ordering::Relaxed);
                }
                Ok(Some(log_result)) => match log_result {
                    diagnostics::LogCollectionOutcome::Error { restricted_logs } => {
                        restricted_logs_present.store(true, Ordering::Relaxed);
                        let mut log_artifact = match suite_reporter
                            .new_artifact(&ArtifactType::RestrictedLog)
                        {
                            Ok(artifact) => artifact,
                            Err(e) => {
                                warn!("Error creating artifact to report restricted logs: {:?}", e);
                                all_artifacts_finished.store(false, Ordering::Relaxed);
                                return;
                            }
                        };
                        for log in restricted_logs.iter() {
                            if let Err(e) = writeln!(log_artifact, "{}", log) {
                                warn!("Error recording restricted logs: {:?}", e);
                                all_artifacts_finished.store(false, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                    diagnostics::LogCollectionOutcome::Passed => (),
                },
                Ok(None) => (),
            }
        })
        .collect::<()>();

    if let Err(Cancelled(_)) = finish_artifacts_fut.or_cancelled(kill_fut).await {
        warn!("Stopped polling artifacts due to timeout");
        teardown_succeeded = false;
    }
    if restricted_logs_present.into_inner() && matches!(outcome, Outcome::Passed) {
        outcome = Outcome::Failed;
    }

    teardown_succeeded &= all_artifacts_finished.into_inner();

    suite_reporter.stopped(&outcome.clone().into(), suite_finish_timestamp)?;

    Ok(ExtendedOutcome {
        outcome: outcome,
        // We regard setup as having succeeded if the `SuiteStarted` event was received.
        // If it was not received, the suite's lifecycle will be `Found`.
        setup_succeeded: Some(suite_state.lifecycle != Lifecycle::Found),
        teardown_succeeded: if suite_state.lifecycle == Lifecycle::Stopped {
            Some(teardown_succeeded)
        } else {
            // `setup_succeeded` is false, so we don't report a definite
            // `teardown_succeeded`.
            None
        },
    })
}

type EventStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<ftest_manager::Event, RunTestSuiteError>> + Send>>;

/// A test suite that is known to have started execution. A suite is considered started once
/// any event is produced for the suite.
pub(crate) struct RunningSuite {
    event_stream: EventStream,
    max_severity_logs: Option<Severity>,
    timeout: Option<std::time::Duration>,
    timeout_grace: std::time::Duration,
    no_cases_equals_success: Option<bool>,
}

pub(crate) struct WaitForStartArgs {
    pub(crate) proxy: ftest_manager::SuiteControllerProxy,
    pub(crate) max_severity_logs: Option<Severity>,
    pub(crate) timeout: Option<std::time::Duration>,
    pub(crate) timeout_grace: std::time::Duration,
    pub(crate) max_pipelined: Option<usize>,
    pub(crate) no_cases_equals_success: Option<bool>,
}

impl RunningSuite {
    /// Number of concurrently active WatchEvents requests. Chosen by testing powers of 2 when
    /// running a set of tests using ffx test against an emulator, and taking the value at
    /// which improvement stops.
    const DEFAULT_PIPELINED_REQUESTS: usize = 8;
    pub(crate) async fn wait_for_start(args: WaitForStartArgs) -> Self {
        let WaitForStartArgs {
            proxy,
            max_severity_logs,
            timeout,
            timeout_grace,
            max_pipelined,
            no_cases_equals_success,
        } = args;

        let proxy = Arc::new(proxy);
        // Stream of fidl responses, with multiple concurrently active requests.
        let unprocessed_event_stream = futures::stream::repeat_with(move || {
            proxy.watch_events().inspect(|events_result| match events_result {
                Ok(Ok(events)) => info!("Latest suite event: {:?}", events.last()),
                _ => (),
            })
        })
        .buffered(max_pipelined.unwrap_or(Self::DEFAULT_PIPELINED_REQUESTS));
        // Terminate the stream after we get an error or empty list of events.
        let terminated_event_stream =
            unprocessed_event_stream.take_until_stop_after(|result| match &result {
                Ok(Ok(events)) => events.is_empty(),
                Err(_) | Ok(Err(_)) => true,
            });
        // Flatten the stream of vecs into a stream of single events.
        let mut event_stream = terminated_event_stream
            .map(Self::convert_to_result_vec)
            .map(futures::stream::iter)
            .flatten()
            .peekable();
        // Wait for the first event to be ready, which signals the suite has started.
        std::pin::Pin::new(&mut event_stream).peek().await;

        Self {
            event_stream: event_stream.boxed(),
            timeout,
            timeout_grace,
            max_severity_logs,
            no_cases_equals_success,
        }
    }

    fn convert_to_result_vec(
        vec: Result<Result<Vec<ftest_manager::Event>, ftest_manager::LaunchError>, fidl::Error>,
    ) -> Vec<Result<ftest_manager::Event, RunTestSuiteError>> {
        match vec {
            Ok(Ok(events)) => events.into_iter().map(Ok).collect(),
            Ok(Err(e)) => vec![Err(e.into())],
            Err(e) => vec![Err(e.into())],
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::output::EntityId;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use maplit::hashmap;

    async fn respond_to_watch_events(
        request_stream: &mut ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::Event>,
    ) {
        let request = request_stream
            .next()
            .await
            .expect("did not get next request")
            .expect("error getting next request");
        let responder = match request {
            ftest_manager::SuiteControllerRequest::WatchEvents { responder } => responder,
            r => panic!("Expected WatchEvents request but got {:?}", r),
        };

        responder.send(Ok(events)).expect("send events");
    }

    /// Serves all events to completion.
    async fn serve_all_events(
        mut request_stream: ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::Event>,
    ) {
        const BATCH_SIZE: usize = 5;
        let mut event_iter = events.into_iter();
        while event_iter.len() > 0 {
            respond_to_watch_events(
                &mut request_stream,
                event_iter.by_ref().take(BATCH_SIZE).collect(),
            )
            .await;
        }
        respond_to_watch_events(&mut request_stream, vec![]).await;
    }

    /// Serves all events to completion, then wait for the channel to close.
    async fn serve_all_events_then_hang(
        mut request_stream: ftest_manager::SuiteControllerRequestStream,
        events: Vec<ftest_manager::Event>,
    ) {
        const BATCH_SIZE: usize = 5;
        let mut event_iter = events.into_iter();
        while event_iter.len() > 0 {
            respond_to_watch_events(
                &mut request_stream,
                event_iter.by_ref().take(BATCH_SIZE).collect(),
            )
            .await;
        }
        let _requests = request_stream.collect::<Vec<_>>().await;
    }

    /// Creates an Event which is unpopulated, except for timestamp.
    /// This isn't representative of an actual event from test framework, but is sufficient
    /// to assert events are routed correctly.
    fn create_empty_event(timestamp: i64) -> ftest_manager::Event {
        ftest_manager::Event { timestamp: Some(timestamp), ..Default::default() }
    }

    macro_rules! assert_empty_events_eq {
        ($t1:expr, $t2:expr) => {
            assert_eq!($t1.timestamp, $t2.timestamp, "Got incorrect event.")
        };
    }

    #[fuchsia::test]
    async fn running_events_simple() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_watch_events(&mut suite_request_stream, vec![create_empty_event(0)]).await;
            respond_to_watch_events(&mut suite_request_stream, vec![]).await;
            drop(suite_request_stream);
        });

        let mut running_suite = RunningSuite::wait_for_start(WaitForStartArgs {
            proxy: suite_proxy,
            max_severity_logs: None,
            timeout: None,
            timeout_grace: std::time::Duration::ZERO,
            max_pipelined: None,
            no_cases_equals_success: None,
        })
        .await;
        assert_empty_events_eq!(
            running_suite.event_stream.next().await.unwrap().unwrap(),
            create_empty_event(0)
        );
        assert!(running_suite.event_stream.next().await.is_none());
        // polling again should still give none.
        assert!(running_suite.event_stream.next().await.is_none());
        suite_server_task.await;
    }

    #[fuchsia::test]
    async fn running_events_multiple_events() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_watch_events(
                &mut suite_request_stream,
                vec![create_empty_event(0), create_empty_event(1)],
            )
            .await;
            respond_to_watch_events(
                &mut suite_request_stream,
                vec![create_empty_event(2), create_empty_event(3)],
            )
            .await;
            respond_to_watch_events(&mut suite_request_stream, vec![]).await;
            drop(suite_request_stream);
        });

        let mut running_suite = RunningSuite::wait_for_start(WaitForStartArgs {
            proxy: suite_proxy,
            max_severity_logs: None,
            timeout: None,
            timeout_grace: std::time::Duration::ZERO,
            max_pipelined: None,
            no_cases_equals_success: None,
        })
        .await;

        for num in 0..4 {
            assert_empty_events_eq!(
                running_suite.event_stream.next().await.unwrap().unwrap(),
                create_empty_event(num)
            );
        }
        assert!(running_suite.event_stream.next().await.is_none());
        suite_server_task.await;
    }

    #[fuchsia::test]
    async fn running_events_peer_closed() {
        let (suite_proxy, mut suite_request_stream) =
            create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let suite_server_task = fasync::Task::spawn(async move {
            respond_to_watch_events(&mut suite_request_stream, vec![create_empty_event(1)]).await;
            drop(suite_request_stream);
        });

        let mut running_suite = RunningSuite::wait_for_start(WaitForStartArgs {
            proxy: suite_proxy,
            max_severity_logs: None,
            timeout: None,
            timeout_grace: std::time::Duration::ZERO,
            max_pipelined: None,
            no_cases_equals_success: None,
        })
        .await;
        assert_empty_events_eq!(
            running_suite.event_stream.next().await.unwrap().unwrap(),
            create_empty_event(1)
        );
        assert_matches!(
            running_suite.event_stream.next().await,
            Some(Err(RunTestSuiteError::Fidl(fidl::Error::ClientChannelClosed { .. })))
        );
        suite_server_task.await;
    }

    fn event_from_details(
        timestamp: i64,
        details: ftest_manager::EventDetails,
    ) -> ftest_manager::Event {
        let mut event = create_empty_event(timestamp);
        event.details = Some(details);
        event
    }

    fn case_found_event(timestamp: i64, test_case_id: u32, name: &str) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseFound(ftest_manager::TestCaseFoundEventDetails {
                test_case_name: Some(name.into()),
                test_case_id: Some(test_case_id),
                ..Default::default()
            }),
        )
    }

    fn case_started_event(timestamp: i64, test_case_id: u32) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseStarted(
                ftest_manager::TestCaseStartedEventDetails {
                    test_case_id: Some(test_case_id),
                    ..Default::default()
                },
            ),
        )
    }

    fn case_stopped_event(
        timestamp: i64,
        test_case_id: u32,
        result: ftest_manager::TestCaseResult,
    ) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseStopped(
                ftest_manager::TestCaseStoppedEventDetails {
                    test_case_id: Some(test_case_id),
                    result: Some(result),
                    ..Default::default()
                },
            ),
        )
    }

    fn case_finished_event(timestamp: i64, test_case_id: u32) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseFinished(
                ftest_manager::TestCaseFinishedEventDetails {
                    test_case_id: Some(test_case_id),
                    ..Default::default()
                },
            ),
        )
    }

    fn case_stdout_event(
        timestamp: i64,
        test_case_id: u32,
        stdout: fidl::Socket,
    ) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseArtifactGenerated(
                ftest_manager::TestCaseArtifactGeneratedEventDetails {
                    test_case_id: Some(test_case_id),
                    artifact: Some(ftest_manager::Artifact::Stdout(stdout)),
                    ..Default::default()
                },
            ),
        )
    }

    fn case_stderr_event(
        timestamp: i64,
        test_case_id: u32,
        stderr: fidl::Socket,
    ) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::TestCaseArtifactGenerated(
                ftest_manager::TestCaseArtifactGeneratedEventDetails {
                    test_case_id: Some(test_case_id),
                    artifact: Some(ftest_manager::Artifact::Stderr(stderr)),
                    ..Default::default()
                },
            ),
        )
    }

    fn suite_started_event(timestamp: i64) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::SuiteStarted(ftest_manager::SuiteStartedEventDetails {
                ..Default::default()
            }),
        )
    }

    fn suite_stopped_event(
        timestamp: i64,
        result: ftest_manager::SuiteResult,
    ) -> ftest_manager::Event {
        event_from_details(
            timestamp,
            ftest_manager::EventDetails::SuiteStopped(ftest_manager::SuiteStoppedEventDetails {
                result: Some(result),
                ..Default::default()
            }),
        )
    }

    #[fuchsia::test]
    async fn collect_events_simple() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Finished),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Passed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert!(case.report.artifacts.is_empty());
            assert!(case.report.directories.is_empty());
            let suite = reports.iter().find(|report| report.id == EntityId::Suite).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_with_case_artifacts() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) = fidl::Socket::create_stream();
        let (stderr_write, stderr_read) = fidl::Socket::create_stream();
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Finished),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let stdio_write_fut = async move {
            let mut async_stdout = fasync::Socket::from_socket(stdout_write);
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr = fasync::Socket::from_socket(stderr_write);
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Passed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite = reports.iter().find(|report| report.id == EntityId::Suite).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join3(serve_all_events(stream, all_events), stdio_write_fut, test_fut)
            .await;
    }

    #[fuchsia::test]
    async fn collect_events_case_artifacts_complete_after_suite() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) = fidl::Socket::create_stream();
        let (stderr_write, stderr_read) = fidl::Socket::create_stream();
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Passed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Finished),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let serve_fut = async move {
            // server side will send all events, then write to (and close) sockets.
            serve_all_events(stream, all_events).await;
            let mut async_stdout = fasync::Socket::from_socket(stdout_write);
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr = fasync::Socket::from_socket(stderr_write);
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: Some(1),
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Passed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite = reports.iter().find(|report| report.id == EntityId::Suite).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_fut, test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_with_case_artifacts_sent_after_case_stopped() {
        const STDOUT_CONTENT: &str = "stdout from my_test_case";
        const STDERR_CONTENT: &str = "stderr from my_test_case";

        let (stdout_write, stdout_read) = fidl::Socket::create_stream();
        let (stderr_write, stderr_read) = fidl::Socket::create_stream();
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Passed),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Finished),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let stdio_write_fut = async move {
            let mut async_stdout = fasync::Socket::from_socket(stdout_write);
            async_stdout.write_all(STDOUT_CONTENT.as_bytes()).await.expect("write to socket");
            let mut async_stderr = fasync::Socket::from_socket(stderr_write);
            async_stderr.write_all(STDERR_CONTENT.as_bytes()).await.expect("write to socket");
        };
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Passed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => STDOUT_CONTENT.as_bytes().to_vec(),
                    output::ArtifactType::Stderr => STDERR_CONTENT.as_bytes().to_vec()
                }
            );
            assert!(case.report.directories.is_empty());

            let suite = reports.iter().find(|report| report.id == EntityId::Suite).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Passed));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join3(serve_all_events(stream, all_events), stdio_write_fut, test_fut)
            .await;
    }

    #[fuchsia::test]
    async fn collect_events_timed_out_case_with_hanging_artifacts() {
        // create sockets and leave the server end open to simulate a hang.
        let (_stdout_write, stdout_read) = fidl::Socket::create_stream();
        let (_stderr_write, stderr_read) = fidl::Socket::create_stream();
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stdout_event(300, 0, stdout_read),
            case_stderr_event(300, 0, stderr_read),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: Some(std::time::Duration::from_secs(2)),
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Timedout,
                    setup_succeeded: Some(true),
                    teardown_succeeded: None,
                }
            );
            suite_reporter.finished().expect("Reporter finished");

            let reports = reporter.get_reports();
            let case = reports
                .iter()
                .find(|report| report.id == EntityId::Case { case: CaseId(0) })
                .unwrap();
            assert_eq!(case.report.name, "my_test_case");
            assert_eq!(case.report.outcome, Some(output::ReportedOutcome::Timedout));
            assert!(case.report.is_finished);
            assert_eq!(case.report.artifacts.len(), 2);
            assert_eq!(
                case.report
                    .artifacts
                    .iter()
                    .map(|(artifact_type, artifact)| (*artifact_type, artifact.get_contents()))
                    .collect::<HashMap<_, _>>(),
                hashmap! {
                    output::ArtifactType::Stdout => vec![],
                    output::ArtifactType::Stderr => vec![]
                }
            );
            assert!(case.report.directories.is_empty());

            let suite = reports.iter().find(|report| report.id == EntityId::Suite).unwrap();
            assert_eq!(suite.report.name, "test-url");
            assert_eq!(suite.report.outcome, Some(output::ReportedOutcome::Timedout));
            assert!(suite.report.is_finished);
            assert!(suite.report.artifacts.is_empty());
            assert!(suite.report.directories.is_empty());
        };

        futures::future::join(serve_all_events_then_hang(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_suite_failed() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Failed),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Failed),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Failed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_suite_did_not_finish() {
        let all_events = vec![
            suite_started_event(0),
            suite_stopped_event(500, ftest_manager::SuiteResult::DidNotFinish),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Inconclusive,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_suite_timed_out_clean_teardown() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::TimedOut),
            case_finished_event(400, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::TimedOut),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Timedout,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_suite_internal_error() {
        let all_events = vec![
            suite_started_event(0),
            suite_stopped_event(500, ftest_manager::SuiteResult::InternalError),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::error(UnexpectedEventError::InternalErrorSuiteResult),
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_unfinished_case() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            suite_stopped_event(500, ftest_manager::SuiteResult::Finished),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::error(UnexpectedEventError::CasesDidNotFinish {
                        cases: vec!["my_test_case".to_string()],
                    }),
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_suite_did_not_report_stop() {
        let all_events = vec![
            suite_started_event(0),
            case_found_event(100, 0, "my_test_case"),
            case_started_event(200, 0),
            case_stopped_event(300, 0, ftest_manager::TestCaseResult::Passed),
            case_finished_event(400, 0),
        ];

        let (proxy, stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: None,
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::error(UnexpectedEventError::SuiteDidNotReportStop),
                    setup_succeeded: Some(true),
                    teardown_succeeded: None,
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_all_events(stream, all_events), test_fut).await;
    }

    #[fuchsia::test]
    async fn collect_events_no_matching_cases_success() {
        let (proxy, mut stream) = create_proxy_and_stream::<ftest_manager::SuiteControllerMarker>();
        let serve_fut = async move {
            while let Ok(Some(req)) = stream.try_next().await {
                if let ftest_manager::SuiteControllerRequest::WatchEvents { responder, .. } = req {
                    let _ = responder.send(Err(ftest_manager::LaunchError::NoMatchingCases));
                }
            }
        };

        let test_fut = async move {
            let reporter = output::InMemoryReporter::new();
            let run_reporter = output::RunReporter::new(reporter.clone());
            let suite_reporter = run_reporter.new_suite("test-url").expect("create new suite");

            let suite = RunningSuite::wait_for_start(WaitForStartArgs {
                proxy,
                max_severity_logs: None,
                timeout: None,
                timeout_grace: std::time::Duration::ZERO,
                max_pipelined: None,
                no_cases_equals_success: Some(true),
            })
            .await;
            assert_eq!(
                run_suite_and_collect_logs(
                    suite,
                    &suite_reporter,
                    diagnostics::LogDisplayConfiguration::default(),
                    futures::future::pending()
                )
                .await
                .expect("collect results"),
                ExtendedOutcome {
                    outcome: Outcome::Passed,
                    setup_succeeded: Some(true),
                    teardown_succeeded: Some(true),
                }
            );
            suite_reporter.finished().expect("Reporter finished");
        };

        futures::future::join(serve_fut, test_fut).await;
    }
}
