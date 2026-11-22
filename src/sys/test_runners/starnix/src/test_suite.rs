// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::binder_latency::run_binder_latency;
use crate::debian_guest::DebianGuest;
use crate::gbenchmark::*;
use crate::gtest::*;
use crate::helpers::*;
use crate::selinux::*;
use crate::syscalls_gtest::*;
use crate::syscalls_libtest::*;
use crate::vts_binary::*;
use anyhow::{Context as _, Error, anyhow, bail};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_test::{self as ftest};
use frunner::{ComponentRunnerMarker, ComponentRunnerProxy, ComponentStartInfo};
use futures::TryStreamExt;
use log::debug;
use namespace::Namespace;
use rust_measure_tape_for_case::Measurable as _;
use std::sync::Arc;
use test_runners_lib::elf::SuiteServerError;
use zx::sys::ZX_CHANNEL_MAX_MSG_BYTES;

const CML_TARGET_KERNEL_FIELD: &str = "test_target_kernel";

enum SuiteTarget {
    Starnix(ComponentRunnerProxy, TestType),
    LinuxGtest(Arc<DebianGuest>),
    LinuxLibtest(Arc<DebianGuest>),
}

/// Resolves the target kernel and test type from `start_info`'s program dictionary, removing both
/// runner-only tags so the remaining program entries can be forwarded to Starnix.
async fn resolve_suite_target(
    start_info: &mut ComponentStartInfo,
    debian_guest: Arc<DebianGuest>,
) -> Result<SuiteTarget, Error> {
    let program = start_info.program.as_mut().context("Missing program.")?;
    let raw_test_type = take_opt_str_value_from_dict(program, "test_type")?;
    let target_kernel = take_opt_str_value_from_dict(program, CML_TARGET_KERNEL_FIELD)?;

    match (target_kernel.as_deref(), raw_test_type.as_deref()) {
        (Some("linux"), Some("syscall_gtest")) => Ok(SuiteTarget::LinuxGtest(debian_guest)),
        (Some("linux"), Some("syscall_libtest")) => Ok(SuiteTarget::LinuxLibtest(debian_guest)),
        (Some("linux"), other) => {
            bail!(
                "{} = \"linux\" is not supported for test_type {:?}",
                CML_TARGET_KERNEL_FIELD,
                other
            )
        }
        (None, raw_type) => {
            let test_type = match raw_type {
                Some("binder_latency") => TestType::BinderLatency,
                Some("gbenchmark") => TestType::Gbenchmark,
                Some("gtest" | "syscall_gtest") => TestType::Gtest,
                Some("gtest_xml_output") => TestType::GtestXmlOutput,
                Some("gunit") => TestType::Gunit,
                Some("vts_binary" | "ltp") => TestType::VtsBinary,
                Some("selinux") => TestType::SeLinux,
                Some("syscall_libtest") => TestType::SyscallLibtest,
                Some(value) => bail!("Unrecognized test_type: {}", value),
                // If test_type is not specified, run the component as a single test case.
                None => TestType::SingleTest,
            };
            // The kernel start info is largely the same as that of the test component. The main
            // difference is that the `container_start_info` does not contain the outgoing directory
            // of the test component.
            let container_start_info = clone_start_info(start_info)?;
            let component_runner = component_runner_from_start_info(container_start_info).await?;
            Ok(SuiteTarget::Starnix(component_runner, test_type))
        }
        (Some(unexpected_target), _) => {
            bail!(
                "Unexpected and unknown {} specified: {}",
                CML_TARGET_KERNEL_FIELD,
                unexpected_target
            )
        }
    }
}

async fn component_runner_from_start_info(
    start_info: ComponentStartInfo,
) -> Result<ComponentRunnerProxy, Error> {
    debug!("use component runner from namespace");
    let ns = start_info.ns.ok_or_else(|| anyhow!("start info does not have namespace"))?;
    let ns = Namespace::try_from(ns)?;
    let svc = ns
        .get(&"/svc".parse().unwrap())
        .ok_or_else(|| anyhow!("test component namespace does not have /svc"))?;
    return Ok(
        fuchsia_component::client::connect_to_protocol_at_dir_root::<ComponentRunnerMarker>(svc)?,
    );
}

/// Handles a single `ftest::SuiteRequestStream`.
///
/// # Parameters
/// - `start_info`: The start info to use when running the test component.
/// - `stream`: The request stream to handle.
pub async fn handle_suite_requests(
    mut start_info: frunner::ComponentStartInfo,
    mut stream: ftest::SuiteRequestStream,
    debian_guest: Arc<DebianGuest>,
) -> Result<(), Error> {
    debug!(start_info:?; "got suite request stream");
    let target = resolve_suite_target(&mut start_info, debian_guest).await?;

    while let Some(event) = stream.try_next().await? {
        let mut test_start_info = clone_start_info(&mut start_info)?;
        debug!("got suite request");
        match event {
            ftest::SuiteRequest::GetTests { iterator, .. } => {
                debug!("enumerating test cases");
                let stream = iterator.into_stream();

                let test_cases = match &target {
                    SuiteTarget::LinuxGtest(debian_guest) => {
                        get_cases_list_for_syscall_gtests_on_guest(
                            &mut test_start_info,
                            debian_guest,
                        )
                        .await?
                    }
                    SuiteTarget::LinuxLibtest(debian_guest) => {
                        get_cases_list_for_libtests_on_guest(&mut test_start_info, debian_guest)
                            .await?
                    }
                    SuiteTarget::Starnix(_, TestType::VtsBinary | TestType::SeLinux) => {
                        get_cases_list_for_vts_binary(test_start_info).await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::SyscallLibtest) => {
                        get_cases_list_for_libtests_on_starnix(test_start_info, component_runner)
                            .await?
                    }
                    SuiteTarget::Starnix(
                        component_runner,
                        test_type @ (TestType::Gtest | TestType::Gunit | TestType::GtestXmlOutput),
                    ) => {
                        get_cases_list_for_gtests(test_start_info, component_runner, *test_type)
                            .await?
                    }
                    SuiteTarget::Starnix(
                        _,
                        TestType::BinderLatency | TestType::Gbenchmark | TestType::SingleTest,
                    ) => {
                        let name = test_start_info
                            .resolved_url
                            .as_ref()
                            .ok_or_else(|| anyhow!("Missing resolved URL"))?;
                        vec![ftest::Case {
                            name: Some(name.clone()),
                            enabled: Some(true),
                            ..Default::default()
                        }]
                    }
                };

                handle_case_iterator(test_cases, stream).await?
            }
            ftest::SuiteRequest::Run { tests, options, listener, .. } => {
                debug!(tests:?; "running tests");
                let run_listener_proxy = listener.into_proxy();

                if tests.is_empty() {
                    debug!("no tests listed, returning");
                    run_listener_proxy.on_finished()?;
                    break;
                }

                // Replace tests with program arguments if they were passed in.
                let mut program =
                    test_start_info.program.clone().ok_or_else(|| anyhow!("Missing program."))?;
                if let Some(test_args) = &options.arguments {
                    replace_program_args(test_args.clone(), &mut program);
                }
                test_start_info.program = Some(program);
                debug!(test_start_info:?; "running tests with info");

                match &target {
                    SuiteTarget::Starnix(component_runner, TestType::Gunit) => {
                        run_gunit_cases(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?;
                    }
                    SuiteTarget::Starnix(
                        component_runner,
                        test_type @ (TestType::Gtest | TestType::GtestXmlOutput),
                    ) => {
                        run_gtest_cases(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                            *test_type,
                        )
                        .await?;
                    }
                    SuiteTarget::Starnix(component_runner, TestType::Gbenchmark) => {
                        run_gbenchmark(
                            tests.get(0).unwrap().clone(),
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::BinderLatency) => {
                        run_binder_latency(
                            tests.get(0).unwrap().clone(),
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::VtsBinary) => {
                        run_vts_binary_cases(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::SeLinux) => {
                        run_selinux_test_suite_cases(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::SingleTest) => {
                        run_test_case(
                            tests.get(0).unwrap().clone(),
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                        )
                        .await?
                    }
                    SuiteTarget::Starnix(component_runner, TestType::SyscallLibtest) => {
                        log::info!("Executing libtest syscall tests on Starnix kernel.");
                        run_libtests_on_starnix(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            component_runner,
                            options,
                        )
                        .await?;
                    }
                    SuiteTarget::LinuxGtest(debian_guest) => {
                        log::info!("Executing gtest syscall tests on Machina Debian guest.");
                        run_syscall_gtests_on_guest(
                            &tests,
                            &mut test_start_info,
                            &run_listener_proxy,
                            debian_guest,
                        )
                        .await?;
                    }
                    SuiteTarget::LinuxLibtest(debian_guest) => {
                        log::info!("Executing libtest syscall tests on Machina Debian guest.");
                        run_libtests_on_debian_guest(
                            tests,
                            test_start_info,
                            &run_listener_proxy,
                            debian_guest,
                            options,
                        )
                        .await?;
                    }
                }

                run_listener_proxy.on_finished()?;
            }
        }
    }

    Ok(())
}

/// Runs a test case associated with a single `ftest::SuiteRequest::Run` request.
///
/// Running the test component is delegated to an instance of the starnix kernel.
///
/// # Parameters
/// - `tests`: The tests that are to be run. Each test executes an independent run of the test
/// component.
/// - `test_url`: The URL of the test component.
/// - `program`: The program data associated with the runner request for the test component.
/// - `run_listener_proxy`: The listener proxy for the test run.
/// - `component_runner`: The runner that will run the test component.
async fn run_test_case(
    test: ftest::Invocation,
    mut start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    component_runner: &frunner::ComponentRunnerProxy,
) -> Result<(), Error> {
    debug!("running generic fallback test suite");
    let (case_listener_proxy, case_listener) = create_proxy::<ftest::CaseListenerMarker>();
    let (numbered_handles, std_handles) = create_numbered_handles();
    start_info.numbered_handles = Some(numbered_handles);

    debug!("notifying client test case started");
    run_listener_proxy.on_test_case_started(&test, std_handles, case_listener)?;

    debug!("starting test component");
    let component_controller = start_test_component(start_info, component_runner)?;

    let result = read_result(component_controller.take_event_stream()).await;
    debug!(result:?; "notifying client test case finished");
    case_listener_proxy.finished(&result)?;

    Ok(())
}

/// Lists all the available test cases and returns them in response to
/// `ftest::CaseIteratorRequest::GetNext`.
async fn handle_case_iterator(
    cases: Vec<ftest::Case>,
    mut stream: ftest::CaseIteratorRequestStream,
) -> Result<(), Error> {
    let mut remaining_cases = &cases[..];

    while let Some(event) = stream.try_next().await? {
        match event {
            ftest::CaseIteratorRequest::GetNext { responder } => {
                // Paginate cases
                // Page overhead of message header + vector
                let mut bytes_used: usize = 32;
                let mut case_count = 0;
                for case in remaining_cases {
                    bytes_used += case.measure().num_bytes;
                    if bytes_used > ZX_CHANNEL_MAX_MSG_BYTES as usize {
                        break;
                    }
                    case_count += 1;
                }
                responder
                    .send(&remaining_cases[..case_count])
                    .map_err(SuiteServerError::Response)?;
                remaining_cases = &remaining_cases[case_count..];
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debian_guest::test_helpers::{MockDebianGuest, send_command_exit};
    use fidl::endpoints::{ClientEnd, create_proxy_and_stream, create_request_stream};
    use fidl_fuchsia_virtualization_guest_interaction::InteractiveGuestRequest;
    use fuchsia_async as fasync;

    /// Returns a `ftest::CaseIteratorProxy` that is served by `super::handle_case_iterator`.
    ///
    /// # Parameters
    /// - `test_name`: The name of the test case that is provided to `handle_case_iterator`.
    fn set_up_iterator(test_name: &str) -> ftest::CaseIteratorProxy {
        let cases = vec![ftest::Case { name: Some(test_name.to_string()), ..Default::default() }];
        let (iterator_client_end, iterator_stream) =
            create_request_stream::<ftest::CaseIteratorMarker>();
        fasync::Task::local(async move {
            let _ = handle_case_iterator(cases, iterator_stream).await;
        })
        .detach();

        iterator_client_end.into_proxy()
    }

    /// Spawns a `ComponentRunnerRequestStream` server that immediately closes all incoming
    /// component controllers with the epitaph specified in `component_controller_epitaph`.
    ///
    /// This function can be used to mock the starnix kernel in a way that simulates a component
    /// exiting with or without error.
    ///
    /// # Parameters
    /// - `component_controller_epitaph`: The epitaph used to close the component controller.
    ///
    /// # Returns
    /// A `ComponentRunnerProxy` that serves each run request by closing the component with the
    /// provided epitaph.
    fn spawn_runner(
        component_controller_epitaph: Result<(), zx::Status>,
    ) -> frunner::ComponentRunnerProxy {
        let (proxy, mut request_stream) =
            fidl::endpoints::create_proxy_and_stream::<frunner::ComponentRunnerMarker>();
        fasync::Task::local(async move {
            while let Some(event) =
                request_stream.try_next().await.expect("Error in test runner request stream")
            {
                match event {
                    frunner::ComponentRunnerRequest::Start {
                        start_info: _start_info,
                        controller,
                        ..
                    } => {
                        controller
                            .close_with_epitaph(component_controller_epitaph)
                            .expect("Could not close with epitaph");
                    }
                    frunner::ComponentRunnerRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!(ordinal:%; "Unknown ComponentRunner request");
                    }
                }
            }
        })
        .detach();
        proxy
    }

    #[derive(Debug, PartialEq)]
    struct RunListenerResults {
        cases: Vec<(Option<String>, Option<ftest::Status>)>,
        on_finished: bool,
    }

    /// Drains the run listener stream until closure and returns each started case's
    /// `(invocation.name, finished_status)` alongside whether `OnFinished` was observed.
    async fn listen_to_test_results(
        mut run_listener_stream: ftest::RunListenerRequestStream,
    ) -> RunListenerResults {
        let mut cases = Vec::new();
        let mut on_finished = false;
        while let Some(request) = run_listener_stream.try_next().await.expect("run_listener stream")
        {
            match request {
                ftest::RunListenerRequest::OnTestCaseStarted { invocation, listener, .. } => {
                    assert!(!on_finished, "OnTestCaseStarted received after OnFinished");
                    let status = match listener
                        .into_stream()
                        .try_next()
                        .await
                        .expect("Failed to get case listener stream request")
                    {
                        Some(ftest::CaseListenerRequest::Finished { result, .. }) => result.status,
                        None => None,
                    };
                    cases.push((invocation.name, status));
                }
                ftest::RunListenerRequest::OnFinished { .. } => {
                    assert!(!on_finished, "RunListener::OnFinished sent more than once");
                    on_finished = true;
                }
            }
        }
        RunListenerResults { cases, on_finished }
    }

    /// Spawns a task that calls `super::run_test_cases` with the provided `run_listener` and
    /// `runner_proxy`. The call is made with a mock test case.
    fn spawn_run_test_cases(
        run_listener: ClientEnd<ftest::RunListenerMarker>,
        component_runner: frunner::ComponentRunnerProxy,
    ) {
        fasync::Task::local(async move {
            let _ = run_test_case(
                ftest::Invocation {
                    name: Some("".to_string()),
                    tag: Some("".to_string()),
                    ..Default::default()
                },
                frunner::ComponentStartInfo { ns: Some(vec![]), ..Default::default() },
                &run_listener.into_proxy(),
                &component_runner,
            )
            .await;
        })
        .detach();
    }

    #[fuchsia::test]
    async fn test_number_of_test_cases() {
        let iterator_proxy = set_up_iterator("test");
        let first_result = iterator_proxy.get_next().await.expect("Didn't get first result");
        let second_result = iterator_proxy.get_next().await.expect("Didn't get second result");

        assert_eq!(first_result.len(), 1);
        assert_eq!(second_result.len(), 0);
    }

    #[fuchsia::test]
    async fn test_case_name() {
        let test_name = "test_name";
        let iterator_proxy = set_up_iterator(test_name);
        let result = iterator_proxy.get_next().await.expect("Didn't get first result");
        assert_eq!(result[0].name, Some(test_name.to_string()));
    }

    /// Tests that when starnix closes the component controller with an `OK` status, the test case
    /// passes.
    #[fuchsia::test]
    async fn test_component_controller_epitaph_ok() {
        let component_runner = spawn_runner(Ok(()));
        let (run_listener, run_listener_stream) =
            create_request_stream::<ftest::RunListenerMarker>();
        spawn_run_test_cases(run_listener, component_runner);
        assert_eq!(
            listen_to_test_results(run_listener_stream).await,
            RunListenerResults {
                cases: vec![(Some("".to_string()), Some(ftest::Status::Passed))],
                on_finished: false,
            }
        );
    }

    /// Tests that when starnix closes the component controller with an error status, the test case
    /// fails.
    #[fuchsia::test]
    async fn test_component_controller_epitaph_not_ok() {
        let component_runner = spawn_runner(Err(zx::Status::INTERNAL));
        let (run_listener, run_listener_stream) =
            create_request_stream::<ftest::RunListenerMarker>();
        spawn_run_test_cases(run_listener, component_runner);
        assert_eq!(
            listen_to_test_results(run_listener_stream).await,
            RunListenerResults {
                cases: vec![(Some("".to_string()), Some(ftest::Status::Failed))],
                on_finished: false,
            }
        );
    }

    #[fuchsia::test]
    async fn test_handle_suite_requests_linux_guest_without_container_runner() {
        // Provide only `/pkg` in the namespace (no `/svc` or `#container` ComponentRunner).
        const BINARY_PATH: &str = "data/tests/linux_suite_bin";
        let pkg = TestPackageDir::with_guest_binary(BINARY_PATH);
        let start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_suite.cm",
            BINARY_PATH,
            &[("test_type", "syscall_libtest"), ("test_target_kernel", "linux")],
        );

        let mock = MockDebianGuest::with_request_handler(|request| match request {
            InteractiveGuestRequest::PutFile { remote_path, responder, .. } => {
                assert!(remote_path.starts_with("/linux_suite_bin_"));
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand {
                command, stdout, command_listener, ..
            } => {
                let output = if command.ends_with(" --list --ignored") {
                    "ignored_case: test\n"
                } else if command.ends_with(" --list") {
                    "smoke_case: test\nignored_case: test\n"
                } else if command.ends_with(" smoke_case --exact --nocapture") {
                    "running smoke_case\n"
                } else {
                    panic!("Unexpected guest command: {}", command);
                };
                let stdout_socket = stdout.expect("stdout socket");
                assert_eq!(
                    stdout_socket.write(output.as_bytes()).expect("write command output"),
                    output.len()
                );
                drop(stdout_socket);
                send_command_exit(command_listener, 0);
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let (suite_proxy, suite_stream) = create_proxy_and_stream::<ftest::SuiteMarker>();
        let guest = mock.guest.clone();
        let suite_task = fasync::Task::local(async move {
            handle_suite_requests(start_info, suite_stream, guest)
                .await
                .expect("handle_suite_requests succeeds");
        });

        // 1. Enumerate cases via GetTests (stages binary once and runs `--list` + `--list --ignored`).
        let (iterator_proxy, iterator_server) = create_proxy::<ftest::CaseIteratorMarker>();
        suite_proxy.get_tests(iterator_server).expect("call get_tests");
        let cases = iterator_proxy.get_next().await.expect("get_next cases");
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name.as_deref(), Some("smoke_case"));
        assert_eq!(cases[0].enabled, Some(true));
        assert_eq!(cases[1].name.as_deref(), Some("ignored_case"));
        assert_eq!(cases[1].enabled, Some(false));
        assert!(iterator_proxy.get_next().await.expect("get_next end").is_empty());
        drop(iterator_proxy);
        assert_eq!(mock.put_file_count(), 1);

        // 2. Execute case via Run (reuses staged binary without a second PutFile).
        let (run_listener, run_listener_stream) =
            create_request_stream::<ftest::RunListenerMarker>();
        suite_proxy
            .run(
                &[ftest::Invocation {
                    name: Some("smoke_case".to_string()),
                    tag: None,
                    ..Default::default()
                }],
                &ftest::RunOptions::default(),
                run_listener,
            )
            .expect("call run");
        assert_eq!(
            listen_to_test_results(run_listener_stream).await,
            RunListenerResults {
                cases: vec![(Some("smoke_case".to_string()), Some(ftest::Status::Passed))],
                on_finished: true,
            }
        );
        assert_eq!(mock.put_file_count(), 1);

        drop(suite_proxy);
        suite_task.await;
        mock.finish().await;
    }
}
