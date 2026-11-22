// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::debian_guest::DebianGuest;
use crate::{gtest, helpers, results_parser};

use anyhow::{self, Context, bail};
use fidl::endpoints;
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_test::{self as ftest, CaseListenerProxy, Result_ as TestResult, Status};
use gtest_runner_lib::parser::TestSuiteOutput;
use helpers::TestType;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;

const HOST_TMP_DIR: &str = "/tmp";
const LIST_TESTS_ARG: &str = "list_tests";

/// Shallow helper struct, simply for encapsulating the various ways that tests report.
struct TestRunnerReport {
    stdout: zx::Socket,
    stderr: zx::Socket,
    top_level_report_proxy: CaseListenerProxy,
    individual_report_proxies: HashMap<String, CaseListenerProxy>,
}

pub(crate) async fn get_cases_list_for_syscall_gtests_on_guest(
    test_start_info: &mut frunner::ComponentStartInfo,
    debian_guest: &DebianGuest,
) -> Result<Vec<ftest::Case>, anyhow::Error> {
    let guest_binary_location =
        helpers::prepare_guest_syscall_test(test_start_info, debian_guest).await?;
    let list_tests_arg = helpers::format_arg(TestType::Gtest, LIST_TESTS_ARG);
    let (exec_command, guest_output_filename) =
        format_exec_command(&guest_binary_location, &list_tests_arg);

    let return_code = debian_guest.execute(&exec_command, &[], None, None, None).await?;
    if return_code != 0 {
        bail!(
            "Failed to list tests on guest (command: '{}', exit code {})",
            exec_command,
            return_code
        );
    }

    let read_content = fetch_guest_output_file(debian_guest, &guest_output_filename)
        .await
        .context("reading tests from guest output file")?;
    gtest::parse_gtest_cases(read_content)
}

/// Runs a set of tests within a Debian guest and reports the results. This function handles setup,
/// pushing test dependencies, executing the tests, and retrieving and parsing the output.
pub(crate) async fn run_syscall_gtests_on_guest(
    tests: &Vec<ftest::Invocation>,
    test_start_info: &mut frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    debian_guest: &DebianGuest,
) -> Result<(), anyhow::Error> {
    let guest_binary_location =
        helpers::prepare_guest_syscall_test(test_start_info, debian_guest).await?;
    let test_runner_report =
        initialize_test_runner_reporting(tests, run_listener_proxy, test_start_info)?;
    let test_filter_arg = gtest::create_tests_filter_arg(tests, TestType::Gtest);
    let (exec_command, guest_output_filename) =
        format_exec_command(&guest_binary_location, &test_filter_arg);

    // Execute the tests and retrieve the results. The command's overall return code is ignored, as
    // the gTest results are parsed from the stdout in get_test_results.
    let _ = debian_guest
        .execute(
            &exec_command,
            &[],
            None,
            Some(test_runner_report.stdout),
            Some(test_runner_report.stderr),
        )
        .await?;
    let test_results = get_test_results(debian_guest, &guest_output_filename).await;

    // Report results back to the test runner. We mark the overall test suite as "Passed,"
    // but we will parse and report the actual status of individual tests afterwards.
    test_runner_report
        .top_level_report_proxy
        .finished(&TestResult { status: Some(Status::Passed), ..Default::default() })?;
    gtest::report_test_results(test_runner_report.individual_report_proxies, test_results)?;

    Ok(())
}

/// Formats a guest gTest command with the given argument and a unique JSON output file, returning
/// the guest command string and output filename.
fn format_exec_command(guest_binary_location: &Path, gtest_arg: &str) -> (String, String) {
    let guest_output_filename = helpers::unique_test_result_filename();
    let guest_output_path = helpers::get_guest_syscall_test_output_path(&guest_output_filename);
    let output_arg = helpers::format_arg(
        TestType::Gtest,
        &format!("output=json:{}", guest_output_path.display()),
    );
    let exec_command = format!("{} {} {}", guest_binary_location.display(), gtest_arg, output_arg);

    (exec_command, guest_output_filename)
}

async fn fetch_guest_output_file(
    debian_guest: &DebianGuest,
    guest_output_filename: &str,
) -> Result<String, anyhow::Error> {
    let host_test_output_path = Path::new(HOST_TMP_DIR).join(guest_output_filename);
    let guest_test_output_path = helpers::get_guest_syscall_test_output_path(guest_output_filename);
    let host_test_output_file =
        OpenOptions::new().write(true).create_new(true).open(&host_test_output_path)?;
    let file_channel: zx::Channel = fdio::transfer_fd(host_test_output_file)?.into();
    let file_client_end = endpoints::ClientEnd::from(file_channel);

    // Since `transfer_fd` consumes the backing file handle, read the transferred file by path and
    // clean up the temporary host copy regardless of whether transfer or read succeeded.
    let result =
        debian_guest.get_file(&guest_test_output_path, file_client_end).await.and_then(|()| {
            std::fs::read_to_string(&host_test_output_path)
                .with_context(|| format!("Failed to read {}", host_test_output_path.display()))
        });
    let _ = std::fs::remove_file(&host_test_output_path);
    result
}

/// Transfers a test results file from a guest and parses its contents. In the case of transfer or
/// parsing errors, it logs a warning and returns an empty vector. The test reporting mechanism
/// should handle interpreting the empty vector appropriately.
async fn get_test_results(
    debian_guest: &DebianGuest,
    guest_output_filename: &str,
) -> Vec<TestSuiteOutput> {
    match fetch_guest_output_file(debian_guest, guest_output_filename).await {
        Ok(gtest_output_buffer) => {
            let test_results =
                results_parser::parse_results(TestType::Gtest, gtest_output_buffer.trim())
                    .with_context(|| format!("Failed to parse {}", guest_output_filename));

            // If tests crash then we may fail to parse the output file. Handle that edge case
            // gracefully so callers can report unfinished cases as failed.
            match test_results {
                Ok(results) => results.testsuites,
                Err(e) => {
                    log::error!("Tests crashed whilst running: {}", e);
                    vec![]
                }
            }
        }
        Err(e) => {
            // Transfer errors could be an issue with the Machina guest, but are most likely
            // indicative of the test result file not being present. That is most likely indicative
            // of the tests crashing while running. In any case, we'll return an empty list. The
            // parsing contract will handle reporting these as failures.
            log::warn!(
                "Failed to transfer results file, which likely indicates the tests crashed while running: {}",
                e
            );
            vec![]
        }
    }
}

/// Initializes the necessary plumbing for capturing stdout and stderr,
/// and for reporting test results to the framework's run listener(s).
fn initialize_test_runner_reporting(
    tests: &Vec<ftest::Invocation>,
    run_listener_proxy: &ftest::RunListenerProxy,
    test_start_info: &frunner::ComponentStartInfo,
) -> Result<TestRunnerReport, anyhow::Error> {
    let (test_stdout, stdout_client) = zx::Socket::create_stream();
    let (test_stderr, stderr_client) = zx::Socket::create_stream();
    let std_handles = ftest::StdHandles {
        out: Some(stdout_client),
        err: Some(stderr_client),
        ..Default::default()
    };
    let (top_level_report_proxy, overall_test_listener) =
        endpoints::create_proxy::<ftest::CaseListenerMarker>();
    run_listener_proxy.on_test_case_started(
        &ftest::Invocation {
            name: Some(test_start_info.resolved_url.clone().unwrap_or_default()),
            tag: None,
            ..Default::default()
        },
        std_handles,
        overall_test_listener,
    )?;

    let test_report_proxies = helpers::start_tests(tests, run_listener_proxy)?;
    Ok(TestRunnerReport {
        stdout: test_stdout,
        stderr: test_stderr,
        top_level_report_proxy,
        individual_report_proxies: test_report_proxies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debian_guest::test_helpers::{MockDebianGuest, send_command_exit};
    use fidl_fuchsia_virtualization_guest_interaction::InteractiveGuestRequest;
    use std::io::Write;

    #[fuchsia::test]
    async fn test_get_cases_list_for_syscall_gtests_on_guest() {
        const BINARY_PATH: &str = "data/tests/linux_foo_test_bin";
        let pkg = helpers::TestPackageDir::with_guest_binary(BINARY_PATH);
        let mut start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_foo_test.cm",
            BINARY_PATH,
            &[],
        );

        let mock = MockDebianGuest::with_request_handler(|request| match request {
            InteractiveGuestRequest::PutFile { remote_path, responder, .. } => {
                assert!(remote_path.starts_with("/linux_foo_test_bin_"));
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand { command, command_listener, .. } => {
                assert!(command.contains("--gtest_list_tests"));
                assert!(command.contains("--gtest_output=json:/test_result-"));
                send_command_exit(command_listener, 0);
            }
            InteractiveGuestRequest::GetFile { remote_path, local_file, responder } => {
                assert!(remote_path.starts_with("/test_result-"));
                let mut host_file: std::fs::File =
                    fdio::create_fd(local_file.into_channel().into())
                        .expect("fd from client")
                        .into();
                let list_json = r#"{
                    "tests": 2,
                    "name": "AllTests",
                    "testsuites": [
                        {
                            "name": "SysctlTest",
                            "tests": 2,
                            "testsuite": [
                                { "name": "ReadsKernelVersion", "file": "sysctl.cc", "line": 10 },
                                { "name": "DISABLED_FlakyCase", "file": "sysctl.cc", "line": 20 }
                            ]
                        }
                    ]
                }"#;
                host_file.write_all(list_json.as_bytes()).expect("write list json");
                drop(host_file);
                responder.send(zx::sys::ZX_OK).expect("send GetFile response");
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let cases = get_cases_list_for_syscall_gtests_on_guest(&mut start_info, &mock.guest)
            .await
            .expect("enumerate guest gtest cases");

        assert_eq!(mock.put_file_count(), 1);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name.as_deref(), Some("SysctlTest.ReadsKernelVersion"));
        assert_eq!(cases[0].enabled, Some(true));
        assert_eq!(cases[1].name.as_deref(), Some("SysctlTest.DISABLED_FlakyCase"));
        assert_eq!(cases[1].enabled, Some(false));

        mock.finish().await;
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_syscall_gtests_on_guest_nonzero_exit() {
        const BINARY_PATH: &str = "data/tests/linux_foo_test_bin";
        let pkg = helpers::TestPackageDir::with_guest_binary(BINARY_PATH);
        let mut start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_foo_test.cm",
            BINARY_PATH,
            &[],
        );

        let mock = MockDebianGuest::with_request_handler(|request| match request {
            InteractiveGuestRequest::PutFile { responder, .. } => {
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand { command, command_listener, .. } => {
                assert!(command.contains("--gtest_list_tests"));
                send_command_exit(command_listener, 1);
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let err = get_cases_list_for_syscall_gtests_on_guest(&mut start_info, &mock.guest)
            .await
            .expect_err("expected error on non-zero list exit code");
        let err_msg = err.to_string();
        assert!(err_msg.contains("--gtest_list_tests"), "unexpected error: {err_msg}");
        assert!(err_msg.contains("exit code 1"), "unexpected error: {err_msg}");
        assert_eq!(mock.put_file_count(), 1);

        mock.finish().await;
    }
}
