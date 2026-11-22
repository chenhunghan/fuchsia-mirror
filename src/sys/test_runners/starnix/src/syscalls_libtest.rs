// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::debian_guest::DebianGuest;
use crate::helpers::{self, clone_start_info};
use anyhow::{Context, Error, anyhow, bail};
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_test as ftest;
use fuchsia_async as fasync;
use futures::{AsyncReadExt, TryFutureExt};
use std::collections::HashSet;
use std::path::Path;

fn build_libtest_cases(all_tests: Vec<String>, disabled_tests: Vec<String>) -> Vec<ftest::Case> {
    let disabled_set: HashSet<String> = disabled_tests.into_iter().collect();
    all_tests
        .into_iter()
        .map(|name| {
            let enabled = !disabled_set.contains(&name);
            ftest::Case { name: Some(name), enabled: Some(enabled), ..Default::default() }
        })
        .collect()
}

/// Enumerates libtest test cases on a Debian guest by dry-running the binary with `--list` and
/// `--list --ignored`.
pub(crate) async fn get_cases_list_for_libtests_on_guest(
    start_info: &mut frunner::ComponentStartInfo,
    debian_guest: &DebianGuest,
) -> Result<Vec<ftest::Case>, Error> {
    let guest_binary = helpers::prepare_guest_syscall_test(start_info, debian_guest).await?;
    let all_tests = run_guest_list_command(debian_guest, &guest_binary, false).await?;
    let disabled_tests = run_guest_list_command(debian_guest, &guest_binary, true).await?;
    Ok(build_libtest_cases(all_tests, disabled_tests))
}

/// Enumerates libtest test cases in Starnix by dry-running the binary with `--list` and
/// `--list --ignored`.
pub(crate) async fn get_cases_list_for_libtests_on_starnix(
    mut start_info: frunner::ComponentStartInfo,
    component_runner: &frunner::ComponentRunnerProxy,
) -> Result<Vec<ftest::Case>, Error> {
    let all_tests = run_starnix_list_command(&mut start_info, component_runner, false).await?;
    let disabled_tests = run_starnix_list_command(&mut start_info, component_runner, true).await?;
    Ok(build_libtest_cases(all_tests, disabled_tests))
}

async fn run_guest_list_command(
    debian_guest: &DebianGuest,
    guest_binary: &Path,
    ignored_only: bool,
) -> Result<Vec<String>, Error> {
    let command = if ignored_only {
        format!("{} --list --ignored", guest_binary.display())
    } else {
        format!("{} --list", guest_binary.display())
    };

    let (guest_stdout, host_stdout) = zx::Socket::create_stream();
    let mut async_stdout = fasync::Socket::from_socket(host_stdout);
    let mut output_bytes = Vec::new();

    // Keep the non-zero exit check inside the join so a failed command aborts reading stdout
    // instead of hanging if the guest never closes its stdout socket.
    let ((), _) = futures::try_join!(
        async {
            let return_code =
                debian_guest.execute(&command, &[], None, Some(guest_stdout), None).await?;
            if return_code != 0 {
                bail!(
                    "Failed to list tests on guest (command: '{}', exit code {})",
                    command,
                    return_code
                );
            }
            Ok(())
        },
        async_stdout.read_to_end(&mut output_bytes).map_err(Error::from),
    )?;

    Ok(parse_libtest_list_output(&output_bytes))
}

async fn run_starnix_list_command(
    start_info: &mut frunner::ComponentStartInfo,
    component_runner: &frunner::ComponentRunnerProxy,
    ignored_only: bool,
) -> Result<Vec<String>, Error> {
    let mut cloned_info = clone_start_info(start_info)?;
    let mut args = vec!["--list".to_string()];
    if ignored_only {
        args.push("--ignored".to_string());
    }

    let (numbered_handles, std_handles) = helpers::create_numbered_handles();
    cloned_info.numbered_handles = Some(numbered_handles);
    helpers::replace_program_args(args, cloned_info.program.as_mut().context("No program")?);

    let stdout_sink = std_handles.out.context("Missing stdout handle")?;
    let mut async_stdout = fasync::Socket::from_socket(stdout_sink);

    let controller = helpers::start_test_component(cloned_info, component_runner)?;

    let mut output_bytes = Vec::new();
    let (status, read_res) = futures::join!(
        helpers::read_result(controller.take_event_stream()),
        async_stdout.read_to_end(&mut output_bytes),
    );
    read_res?;

    if status.status != Some(ftest::Status::Passed) {
        bail!("Failed to list tests: {:?}", status.status);
    }

    Ok(parse_libtest_list_output(&output_bytes))
}

fn parse_libtest_list_output(output_bytes: &[u8]) -> Vec<String> {
    let output_str = String::from_utf8_lossy(output_bytes);
    let mut test_names = Vec::new();
    for line in output_str.lines() {
        let trimmed = line.trim();
        if let Some(test_name) = trimmed.strip_suffix(": test") {
            test_names.push(test_name.to_string());
        }
    }
    test_names
}

/// Executes individual libtest tests inside Machina with isolated per-test stdout/stderr sockets.
pub(crate) async fn run_libtests_on_debian_guest(
    tests: Vec<ftest::Invocation>,
    mut test_start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    debian_guest: &DebianGuest,
    options: ftest::RunOptions,
) -> Result<(), Error> {
    let guest_binary =
        helpers::prepare_guest_syscall_test(&mut test_start_info, debian_guest).await?;

    for test in tests {
        let test_name = test.name.clone().ok_or_else(|| anyhow!("Invocation missing test name"))?;
        let (case_listener_proxy, case_listener) =
            fidl::endpoints::create_proxy::<ftest::CaseListenerMarker>();

        let (test_stdout, stdout_client) = zx::Socket::create_stream();
        let (test_stderr, stderr_client) = zx::Socket::create_stream();
        let std_handles = ftest::StdHandles {
            out: Some(stdout_client),
            err: Some(stderr_client),
            ..Default::default()
        };

        run_listener_proxy.on_test_case_started(&test, std_handles, case_listener)?;

        // Execute specific test case with --exact and --nocapture
        let mut extra_args = String::new();
        if options.include_disabled_tests.unwrap_or(false) {
            extra_args.push_str(" --include-ignored");
        }
        if let Some(user_args) = &options.arguments {
            for arg in user_args {
                extra_args.push(' ');
                extra_args.push_str(arg);
            }
        }
        let command =
            format!("{} {} --exact --nocapture{}", guest_binary.display(), test_name, extra_args);
        let exec_result =
            debian_guest.execute(&command, &[], None, Some(test_stdout), Some(test_stderr)).await;

        let status = match exec_result {
            Ok(0) => ftest::Status::Passed,
            Ok(return_code) => {
                log::warn!("Test {} failed on guest with exit code {}", test_name, return_code);
                ftest::Status::Failed
            }
            Err(e) => {
                log::warn!("Test {} failed on guest: {:?}", test_name, e);
                ftest::Status::Failed
            }
        };

        case_listener_proxy
            .finished(&ftest::Result_ { status: Some(status), ..Default::default() })?;
    }

    Ok(())
}

/// Executes individual libtest tests on Starnix kernel.
pub(crate) async fn run_libtests_on_starnix(
    tests: Vec<ftest::Invocation>,
    mut test_start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    component_runner: &frunner::ComponentRunnerProxy,
    options: ftest::RunOptions,
) -> Result<(), Error> {
    for test in tests {
        let test_name = test.name.clone().ok_or_else(|| anyhow!("Invocation missing test name"))?;
        let (case_listener_proxy, case_listener) =
            fidl::endpoints::create_proxy::<ftest::CaseListenerMarker>();

        let (numbered_handles, std_handles) = helpers::create_numbered_handles();
        let mut start_info = clone_start_info(&mut test_start_info)?;
        start_info.numbered_handles = Some(numbered_handles);

        let mut args = vec![test_name, "--exact".to_string(), "--nocapture".to_string()];
        if options.include_disabled_tests.unwrap_or(false) {
            args.push("--include-ignored".to_string());
        }
        if let Some(user_args) = &options.arguments {
            args.extend(user_args.clone());
        }

        helpers::replace_program_args(args, start_info.program.as_mut().unwrap());

        run_listener_proxy.on_test_case_started(&test, std_handles, case_listener)?;
        let component_controller = helpers::start_test_component(start_info, component_runner)?;
        let result = helpers::read_result(component_controller.take_event_stream()).await;
        case_listener_proxy.finished(&result)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debian_guest::test_helpers::{MockDebianGuest, send_command_exit};
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_data as fdata;
    use fidl_fuchsia_virtualization_guest_interaction::InteractiveGuestRequest;
    use fuchsia_async as fasync;
    use futures::{AsyncWriteExt, TryStreamExt};

    fn spawn_mock_runner_with_filter() -> (frunner::ComponentRunnerProxy, fasync::Task<()>) {
        let (proxy, mut request_stream) =
            create_proxy_and_stream::<frunner::ComponentRunnerMarker>();
        let task = fasync::Task::local(async move {
            while let Some(event) =
                request_stream.try_next().await.expect("Error in test runner request stream")
            {
                match event {
                    frunner::ComponentRunnerRequest::Start { start_info, controller, .. } => {
                        let mut is_ignored = false;
                        if let Some(entries) = start_info.program.and_then(|p| p.entries) {
                            for entry in entries {
                                if entry.key == "args" {
                                    if let Some(fdata::DictionaryValue::StrVec(args)) =
                                        entry.value.map(|b| *b)
                                    {
                                        if args.contains(&"--ignored".to_string()) {
                                            is_ignored = true;
                                        }
                                    }
                                }
                            }
                        }
                        let list_output = if is_ignored {
                            "foo::test_b: test\n"
                        } else {
                            "foo::test_a: test\nfoo::test_b: test\n"
                        };

                        if let Some(handles) = start_info.numbered_handles {
                            for handle_info in handles {
                                if handle_info.id
                                    == fuchsia_runtime::HandleInfo::new(
                                        fuchsia_runtime::HandleType::FileDescriptor,
                                        1,
                                    )
                                    .as_raw()
                                {
                                    let socket = zx::Socket::from(handle_info.handle);
                                    let mut async_socket = fasync::Socket::from_socket(socket);
                                    let _ = async_socket.write_all(list_output.as_bytes()).await;
                                }
                            }
                        }
                        controller
                            .close_with_epitaph(Ok(()))
                            .expect("Could not close with epitaph");
                    }
                    frunner::ComponentRunnerRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!(ordinal:%; "Unknown ComponentRunner request");
                    }
                }
            }
        });
        (proxy, task)
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_libtests() {
        let (runner, _task) = spawn_mock_runner_with_filter();
        let start_info = frunner::ComponentStartInfo {
            program: Some(fdata::Dictionary { entries: Some(vec![]), ..Default::default() }),
            ns: Some(vec![]),
            ..Default::default()
        };

        let cases = get_cases_list_for_libtests_on_starnix(start_info, &runner).await.unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name.as_deref(), Some("foo::test_a"));
        assert_eq!(cases[0].enabled, Some(true));
        assert_eq!(cases[1].name.as_deref(), Some("foo::test_b"));
        assert_eq!(cases[1].enabled, Some(false));
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_libtests_on_guest_on_started_error() {
        const BINARY_PATH: &str = "data/tests/linux_rust_test_bin";
        let pkg = helpers::TestPackageDir::with_guest_binary(BINARY_PATH);
        let mut start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_rust_test.cm",
            BINARY_PATH,
            &[],
        );

        // Keep stdout open so listing must abort reading stdout rather than hanging.
        let mut held_stdout = None;
        let mock = MockDebianGuest::with_request_handler(move |request| match request {
            InteractiveGuestRequest::PutFile { responder, .. } => {
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand {
                command, stdout, command_listener, ..
            } => {
                assert!(command.ends_with(" --list"), "Unexpected guest command: {}", command);
                assert!(held_stdout.replace(stdout.expect("stdout socket")).is_none());
                let (_stream, control_handle) = command_listener.into_stream_and_control_handle();
                control_handle
                    .send_on_started(zx::Status::INTERNAL.into_raw())
                    .expect("send OnStarted error");
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let err = get_cases_list_for_libtests_on_guest(&mut start_info, &mock.guest)
            .await
            .expect_err("expected error on OnStarted failure");
        let err_msg = err.to_string();
        assert!(err_msg.contains("failed to start"), "unexpected error: {err_msg}");
        assert!(err_msg.contains("INTERNAL"), "unexpected error: {err_msg}");

        assert_eq!(mock.put_file_count(), 1);
        mock.finish().await;
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_libtests_on_guest_list_nonzero_exit() {
        const BINARY_PATH: &str = "data/tests/linux_rust_test_bin";
        let pkg = helpers::TestPackageDir::with_guest_binary(BINARY_PATH);
        let mut start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_rust_test.cm",
            BINARY_PATH,
            &[],
        );

        // Keep stdout open so listing must abort reading stdout rather than hanging.
        let mut held_stdout = None;
        let mock = MockDebianGuest::with_request_handler(move |request| match request {
            InteractiveGuestRequest::PutFile { responder, .. } => {
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand {
                command, stdout, command_listener, ..
            } => {
                assert!(command.ends_with(" --list"), "Unexpected guest command: {}", command);
                assert!(held_stdout.replace(stdout.expect("stdout socket")).is_none());
                send_command_exit(command_listener, 1);
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let err = get_cases_list_for_libtests_on_guest(&mut start_info, &mock.guest)
            .await
            .expect_err("expected error on non-zero --list exit code");
        let err_msg = err.to_string();
        assert!(err_msg.contains(" --list'"), "unexpected error: {err_msg}");
        assert!(err_msg.contains("exit code 1"), "unexpected error: {err_msg}");

        assert_eq!(mock.put_file_count(), 1);
        mock.finish().await;
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_libtests_on_guest_ignored_list_nonzero_exit() {
        const BINARY_PATH: &str = "data/tests/linux_rust_test_bin";
        let pkg = helpers::TestPackageDir::with_guest_binary(BINARY_PATH);
        let mut start_info = pkg.start_info(
            "fuchsia-pkg://fuchsia.com/pkg#meta/linux_rust_test.cm",
            BINARY_PATH,
            &[],
        );

        // Keep the failed `--list --ignored` stdout open so listing must abort reading stdout
        // rather than hanging.
        let mut held_stdout = None;
        let mock = MockDebianGuest::with_request_handler(move |request| match request {
            InteractiveGuestRequest::PutFile { responder, .. } => {
                responder.send(zx::sys::ZX_OK).expect("send PutFile response");
            }
            InteractiveGuestRequest::ExecuteCommand {
                command, stdout, command_listener, ..
            } => {
                if command.ends_with(" --list") {
                    let output = b"foo::test_a: test\n";
                    let stdout_socket = stdout.expect("stdout socket");
                    assert_eq!(
                        stdout_socket.write(output).expect("write list output"),
                        output.len()
                    );
                    drop(stdout_socket);
                    send_command_exit(command_listener, 0);
                } else if command.ends_with(" --list --ignored") {
                    assert!(held_stdout.replace(stdout.expect("stdout socket")).is_none());
                    send_command_exit(command_listener, 2);
                } else {
                    panic!("Unexpected guest command: {}", command);
                }
            }
            other => panic!("Unexpected InteractiveGuest request: {:?}", other),
        });

        let err = get_cases_list_for_libtests_on_guest(&mut start_info, &mock.guest)
            .await
            .expect_err("expected error on non-zero --list --ignored exit code");
        let err_msg = err.to_string();
        assert!(err_msg.contains(" --list --ignored'"), "unexpected error: {err_msg}");
        assert!(err_msg.contains("exit code 2"), "unexpected error: {err_msg}");

        assert_eq!(mock.put_file_count(), 1);
        mock.finish().await;
    }

    #[fuchsia::test]
    async fn test_get_cases_list_for_libtests_error_on_disabled_tests() {
        let (proxy, mut request_stream) =
            create_proxy_and_stream::<frunner::ComponentRunnerMarker>();
        let _task = fasync::Task::local(async move {
            if let Some(event) =
                request_stream.try_next().await.expect("Error in test runner request stream")
            {
                match event {
                    frunner::ComponentRunnerRequest::Start { start_info, controller, .. } => {
                        let list_output = "foo::test_a: test\n";
                        if let Some(handles) = start_info.numbered_handles {
                            for handle_info in handles {
                                if handle_info.id
                                    == fuchsia_runtime::HandleInfo::new(
                                        fuchsia_runtime::HandleType::FileDescriptor,
                                        1,
                                    )
                                    .as_raw()
                                {
                                    let socket = zx::Socket::from(handle_info.handle);
                                    let mut async_socket = fasync::Socket::from_socket(socket);
                                    let _ = async_socket.write_all(list_output.as_bytes()).await;
                                }
                            }
                        }
                        // Drop request_stream before closing controller so the runner channel is closed
                        // for the subsequent disabled tests query.
                        drop(request_stream);
                        controller
                            .close_with_epitaph(Ok(()))
                            .expect("Could not close with epitaph");
                    }
                    _ => {}
                }
            }
        });

        let start_info = frunner::ComponentStartInfo {
            program: Some(fdata::Dictionary { entries: Some(vec![]), ..Default::default() }),
            ns: Some(vec![]),
            ..Default::default()
        };

        assert!(get_cases_list_for_libtests_on_starnix(start_info, &proxy).await.is_err());
    }

    #[fuchsia::test]
    async fn test_run_libtests_on_starnix_with_options() {
        let (proxy, mut request_stream) =
            create_proxy_and_stream::<frunner::ComponentRunnerMarker>();
        let _task = fasync::Task::local(async move {
            while let Some(event) =
                request_stream.try_next().await.expect("Error in test runner request stream")
            {
                match event {
                    frunner::ComponentRunnerRequest::Start { start_info, controller, .. } => {
                        let entries = start_info.program.unwrap().entries.unwrap();
                        let args_entry = entries.into_iter().find(|e| e.key == "args").unwrap();
                        let fdata::DictionaryValue::StrVec(args) = *args_entry.value.unwrap()
                        else {
                            panic!("expected StrVec");
                        };
                        assert_eq!(
                            args,
                            vec![
                                "foo::test_a".to_string(),
                                "--exact".to_string(),
                                "--nocapture".to_string(),
                                "--include-ignored".to_string(),
                                "custom_arg".to_string(),
                            ]
                        );
                        controller
                            .close_with_epitaph(Ok(()))
                            .expect("Could not close with epitaph");
                    }
                    _ => {}
                }
            }
        });

        let (run_listener_proxy, mut run_listener_stream) =
            create_proxy_and_stream::<ftest::RunListenerMarker>();
        let _listener_task = fasync::Task::local(async move {
            while let Some(event) = run_listener_stream.try_next().await.unwrap() {
                match event {
                    ftest::RunListenerRequest::OnTestCaseStarted { listener, .. } => {
                        let mut stream = listener.into_stream();
                        while let Some(req) = stream.try_next().await.unwrap() {
                            match req {
                                ftest::CaseListenerRequest::Finished { .. } => {}
                            }
                        }
                    }
                    ftest::RunListenerRequest::OnFinished { .. } => {}
                }
            }
        });

        let start_info = frunner::ComponentStartInfo {
            program: Some(fdata::Dictionary { entries: Some(vec![]), ..Default::default() }),
            ns: Some(vec![]),
            ..Default::default()
        };

        let tests =
            vec![ftest::Invocation { name: Some("foo::test_a".to_string()), ..Default::default() }];
        let options = ftest::RunOptions {
            include_disabled_tests: Some(true),
            arguments: Some(vec!["custom_arg".to_string()]),
            ..Default::default()
        };

        run_libtests_on_starnix(tests, start_info, &run_listener_proxy, &proxy, options)
            .await
            .unwrap();
    }
}
