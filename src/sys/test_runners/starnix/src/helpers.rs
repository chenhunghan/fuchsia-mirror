// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::debian_guest::{DataMount, DebianGuest};
use anyhow::{Context as _, Error, anyhow};
use cm_types::NamespacePath;
use fidl::endpoints;
use fidl::endpoints::{Proxy, create_proxy};
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_data as fdata;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_process as fprocess;
use fidl_fuchsia_test::{self as ftest};
use fidl_fuchsia_virtualization::{BlockFormat, BlockMode, BlockSpec};
use frunner::ComponentNamespaceEntry;
use ftest::CaseListenerProxy;
use fuchsia_fs::{directory, file};
use fuchsia_runtime as fruntime;
use fuchsiaperf::FuchsiaPerfBenchmarkResult;
use futures::StreamExt;
use gtest_runner_lib::parser::read_file;
use log::debug;
use namespace::Namespace;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

pub const SYSCALL_TEST_DEPS_ROMFS_PATH: &str = "data/syscall_test_deps.romfs";
pub const MOUNT_SYSCALL_DEPS_SCRIPT_PATH: &str = "/pkg/data/mount_syscall_deps.sh";
const PACKAGE_META_CONTENTS_PATH: &str = "meta/contents";
const GUEST_TEST_ROOT: &str = "/";

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TestType {
    BinderLatency,
    Gbenchmark,
    Gtest,
    GtestXmlOutput,
    Gunit,
    SeLinux,
    SingleTest,
    SyscallLibtest,
    VtsBinary,
}

pub fn get_opt_str_value_from_dict(
    dict: &fdata::Dictionary,
    name: &str,
) -> Result<Option<String>, Error> {
    match runner::get_value(dict, name) {
        Some(fdata::DictionaryValue::Str(value)) => Ok(Some(value.clone())),
        Some(_) => Err(anyhow!("{} must a string", name)),
        _ => Ok(None),
    }
}

pub fn get_str_value_from_dict(dict: &fdata::Dictionary, name: &str) -> Result<String, Error> {
    match get_opt_str_value_from_dict(dict, name)? {
        Some(s) => Ok(s),
        None => Err(anyhow!("{} is not specified", name)),
    }
}

fn take_value<'a>(dict: &mut fdata::Dictionary, key: &str) -> Option<fdata::DictionaryValue> {
    let entries = dict.entries.as_mut()?;
    for i in 0..entries.len() {
        if entries[i].key == key {
            return entries.remove(i).value.map(|b| *b);
        }
    }
    None
}

pub fn take_opt_str_value_from_dict(
    dict: &mut fdata::Dictionary,
    name: &str,
) -> Result<Option<String>, Error> {
    match take_value(dict, name) {
        Some(fdata::DictionaryValue::Str(value)) => Ok(Some(value)),
        Some(_) => Err(anyhow!("{} must a string", name)),
        _ => Ok(None),
    }
}

pub async fn run_starnix_benchmark(
    test: ftest::Invocation,
    mut start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    component_runner: &frunner::ComponentRunnerProxy,
    test_data_path: &str,
    converter: impl FnOnce(&str, &str) -> Result<Vec<FuchsiaPerfBenchmarkResult>, Error>,
) -> Result<(), Error> {
    let (case_listener_proxy, case_listener) = create_proxy::<ftest::CaseListenerMarker>();
    let (numbered_handles, std_handles) = create_numbered_handles();
    start_info.numbered_handles = Some(numbered_handles);

    debug!("notifying client test case started");
    run_listener_proxy.on_test_case_started(&test, std_handles, case_listener)?;

    debug!("getting test suite label");
    let program = start_info.program.as_mut().context("No program")?;
    let test_suite = take_opt_str_value_from_dict(program, "test_suite_label")?
        .ok_or_else(|| anyhow!("missing test suite label"))?;

    // Save the custom_artifacts DirectoryProxy for result reporting.
    let custom_artifacts =
        get_custom_artifacts_directory(start_info.ns.as_mut().expect("No namespace."))?;

    // Environment variables BENCHMARK_FORMAT and BENCHMARK_OUT should be set
    // so the test writes json results to this directory.
    let output_dir = add_output_dir_to_namespace(&mut start_info)?;

    // Start the test component.
    let component_controller = start_test_component(start_info, component_runner)?;
    let result = read_result(component_controller.take_event_stream()).await;

    // Read the output it produced.
    let test_data = read_file(&output_dir, test_data_path)
        .await
        .with_context(|| format!("reading {test_data_path} from test data"))?
        .trim()
        .to_owned();

    let perfs =
        converter(&test_data, &test_suite).context("converting test output to fuchsiaperf")?;

    // Write JSON to custom artifacts directory where perf test infra expects it.
    let file_proxy = fuchsia_fs::directory::open_file(
        &custom_artifacts,
        "results.fuchsiaperf.json",
        fio::PERM_WRITABLE | fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await?;
    fuchsia_fs::file::write(&file_proxy, serde_json::to_string(&perfs)?).await?;

    case_listener_proxy.finished(&result)?;

    Ok(())
}

/// Replace the arguments in `program` with `test_arguments`, which were provided to the test
/// framework directly.
pub fn replace_program_args(test_arguments: Vec<String>, program: &mut fdata::Dictionary) {
    update_program_args(test_arguments, program, false);
}

/// Insert `new_args` into the arguments in `program`.
pub fn append_program_args(new_args: Vec<String>, program: &mut fdata::Dictionary) {
    update_program_args(new_args, program, true);
}

/// Clones relevant fields of `start_info`. `&mut` is required to clone the `ns` field, but the
/// `start_info` is not modified.
///
/// The `outgoing_dir` of the result will be present, but unusable.
pub fn clone_start_info(
    start_info: &mut frunner::ComponentStartInfo,
) -> Result<frunner::ComponentStartInfo, Error> {
    let ns = Namespace::try_from(start_info.ns.take().unwrap())?;
    // Reset the namespace of the start_info, since it was moved out above.
    start_info.ns = Some(ns.clone().try_into()?);

    let (outgoing_dir, _outgoing_dir_server) = zx::Channel::create();

    Ok(frunner::ComponentStartInfo {
        resolved_url: start_info.resolved_url.clone(),
        program: start_info.program.clone(),
        ns: Some(ns.try_into()?),
        outgoing_dir: Some(outgoing_dir.into()),
        runtime_dir: None,
        numbered_handles: Some(vec![]),
        encoded_config: None,
        break_on_start: None,
        ..Default::default()
    })
}

/// Creates numbered handles for a test component with a respective `StdHandles` that should be
/// passed to the `RunListener`.
pub fn create_numbered_handles() -> (Vec<fprocess::HandleInfo>, ftest::StdHandles) {
    let (test_stdin, _) = zx::Socket::create_stream();
    let (test_stdout, stdout_client) = zx::Socket::create_stream();
    let (test_stderr, stderr_client) = zx::Socket::create_stream();
    let stdin_handle_info = fprocess::HandleInfo {
        handle: test_stdin.into_handle(),
        id: fruntime::HandleInfo::new(fruntime::HandleType::FileDescriptor, 0).as_raw(),
    };
    let stdout_handle_info = fprocess::HandleInfo {
        handle: test_stdout.into_handle(),
        id: fruntime::HandleInfo::new(fruntime::HandleType::FileDescriptor, 1).as_raw(),
    };
    let stderr_handle_info = fprocess::HandleInfo {
        handle: test_stderr.into_handle(),
        id: fruntime::HandleInfo::new(fruntime::HandleType::FileDescriptor, 2).as_raw(),
    };

    let numbered_handles = vec![stdin_handle_info, stdout_handle_info, stderr_handle_info];
    let std_handles = ftest::StdHandles {
        out: Some(stdout_client),
        err: Some(stderr_client),
        ..Default::default()
    };

    (numbered_handles, std_handles)
}

/// Starts the test component and returns its proxy.
pub fn start_test_component(
    start_info: frunner::ComponentStartInfo,
    component_runner: &frunner::ComponentRunnerProxy,
) -> Result<frunner::ComponentControllerProxy, Error> {
    let (component_controller, component_controller_server_end) =
        create_proxy::<frunner::ComponentControllerMarker>();

    debug!(start_info:?; "asking kernel to start component");
    component_runner.start(start_info, component_controller_server_end)?;

    Ok(component_controller)
}

/// Reads the epitaph from the provided `event_stream`.
pub async fn read_component_epitaph(
    mut event_stream: frunner::ComponentControllerEventStream,
) -> zx::sys::zx_status_t {
    match event_stream.next().await {
        Some(Err(fidl::Error::ClientChannelClosed { epitaph, .. })) => {
            match Result::<(), zx::Status>::from(epitaph) {
                Err(s) => s.into_raw(),
                Ok(()) => zx::sys::ZX_OK,
            }
        }
        result => {
            log::error!(
                "Didn't get epitaph from the component controller, instead got: {:?}",
                result
            );
            // Fail the test case here, since the component controller's epitaph couldn't be
            // read.
            zx::Status::INTERNAL.into_raw()
        }
    }
}

/// Reads the result of the test run from `event_stream`.
///
/// The result is determined by reading the epitaph from the provided `event_stream`.
pub async fn read_result(event_stream: frunner::ComponentControllerEventStream) -> ftest::Result_ {
    match read_component_epitaph(event_stream).await {
        zx::sys::ZX_OK => {
            ftest::Result_ { status: Some(ftest::Status::Passed), ..Default::default() }
        }
        _ => ftest::Result_ { status: Some(ftest::Status::Failed), ..Default::default() },
    }
}

pub fn add_output_dir_to_namespace(
    start_info: &mut frunner::ComponentStartInfo,
) -> Result<fio::DirectoryProxy, Error> {
    const TEST_DATA_DIR: &str = "/tmp/test_data";

    let test_data_path = format!("{}/{}", TEST_DATA_DIR, uuid::Uuid::new_v4());
    std::fs::create_dir_all(&test_data_path).expect("cannot create test output directory.");
    let data_dir_proxy = fuchsia_fs::directory::open_in_namespace(
        &test_data_path,
        fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .expect("Cannot open test data directory.");

    let data_dir = data_dir_proxy.into_client_end().expect("Cannot get client end from proxy.");
    start_info.ns.as_mut().ok_or_else(|| anyhow!("Missing namespace."))?.push(
        ComponentNamespaceEntry {
            path: Some("/test_data".to_string()),
            directory: Some(data_dir),
            ..Default::default()
        },
    );

    let test_data_dir =
        fuchsia_fs::directory::open_in_namespace(&test_data_path, fio::PERM_READABLE)
            .expect("Cannot open test data directory.");
    Ok(test_data_dir)
}

/// Replace or append the arguments in `program` with `new_args`.
fn update_program_args(mut new_args: Vec<String>, program: &mut fdata::Dictionary, append: bool) {
    /// The program argument key name.
    const ARGS_KEY: &str = "args";

    if new_args.is_empty() {
        return;
    }

    let mut new_entry = fdata::DictionaryEntry {
        key: ARGS_KEY.to_string(),
        value: Some(Box::new(fdata::DictionaryValue::StrVec(new_args.clone()))),
    };
    if let Some(entries) = &mut program.entries {
        if let Some(index) = entries.iter().position(|entry| entry.key == ARGS_KEY) {
            let entry = entries.remove(index);

            if append {
                if let Some(mut box_value) = entry.value {
                    if let fdata::DictionaryValue::StrVec(args) = &mut *box_value {
                        args.append(&mut new_args);
                        new_entry.value =
                            Some(Box::new(fdata::DictionaryValue::StrVec(args.to_vec())));
                    };
                }
            }
        }
        entries.push(new_entry);
    } else {
        let entries = vec![new_entry];
        program.entries = Some(entries);
    };
}

fn get_custom_artifacts_directory(
    namespace: &mut Vec<frunner::ComponentNamespaceEntry>,
) -> Result<fio::DirectoryProxy, Error> {
    for entry in namespace {
        if entry.path.as_ref().unwrap() == "/custom_artifacts" {
            return Ok(entry.directory.take().unwrap().into_proxy());
        }
    }

    Err(anyhow!("Couldn't find /custom artifacts."))
}

pub async fn read_tests_list(
    start_info: &mut frunner::ComponentStartInfo,
) -> Result<Vec<TestDefinition>, Error> {
    let program = start_info.program.as_ref().unwrap();
    let tests_list_file = get_str_value_from_dict(program, "tests_list")?;
    Ok(read_file_from_component_ns(start_info, &tests_list_file)
        .await
        .with_context(|| format!("Failed to read {}", tests_list_file))?
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(parse_test_definition)
        .collect())
}

pub struct TestDefinition {
    pub name: String,
    pub command: String,
}

pub fn parse_test_definition(test_def: &str) -> TestDefinition {
    let test_def = test_def.trim();
    match test_def.split_once(' ') {
        Some((name, command)) => {
            TestDefinition { name: name.to_string(), command: command.trim().to_string() }
        }
        None => TestDefinition { name: test_def.to_string(), command: test_def.to_string() },
    }
}

pub async fn read_file_from_dir(dir: &fio::DirectoryProxy, path: &str) -> Result<String, Error> {
    let file_proxy = fuchsia_fs::directory::open_file_async(&dir, path, fuchsia_fs::PERM_READABLE)?;
    fuchsia_fs::file::read_to_string(&file_proxy).await.map_err(Into::into)
}

pub async fn read_file_from_component_ns(
    start_info: &mut frunner::ComponentStartInfo,
    path: &str,
) -> Result<String, Error> {
    for entry in
        start_info.ns.as_mut().ok_or_else(|| anyhow!("Component NS is not set"))?.iter_mut()
    {
        if entry.path == Some("/pkg".to_string()) {
            let dir =
                entry.directory.take().ok_or_else(|| anyhow!("NS entry directory is not set"))?;
            let dir_proxy = dir.into_proxy();

            let result = read_file_from_dir(&dir_proxy, path).await;

            // Return the directory back to the `start_info`.
            entry.directory = Some(fidl::endpoints::ClientEnd::new(
                dir_proxy.into_channel().unwrap().into_zx_channel(),
            ));

            return result;
        }
    }

    Err(anyhow!("/pkg is not in the namespace"))
}

// This is a hacky workaround for run dashboard ergonomics, so that we get a top-level view of
// the suite status (including logs from all test cases), while also retaining individual
// test case pass/fail results. To do this, we'll send a placeholder overall test case to the
// test manager, with stdio handles wired into that overall "test" case. The individual tests
// will report into those same handles, by way of the component start_info.numbered_handles.
pub fn start_top_level_report(
    start_info: &mut frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    handle_info: Vec<fprocess::HandleInfo>,
    handles: ftest::StdHandles,
) -> Result<CaseListenerProxy, Error> {
    start_info.numbered_handles = Some(handle_info);
    let (overall_test_listener_proxy, overall_test_listener) =
        create_proxy::<ftest::CaseListenerMarker>();
    run_listener_proxy.on_test_case_started(
        &ftest::Invocation {
            name: Some(start_info.resolved_url.clone().unwrap_or_default()),
            tag: None,
            ..Default::default()
        },
        handles,
        overall_test_listener,
    )?;

    Ok(overall_test_listener_proxy)
}

pub fn format_arg(test_type: TestType, test_arg: &str) -> String {
    match test_type {
        TestType::Gtest | TestType::GtestXmlOutput => format!("--gtest_{}", test_arg),
        TestType::Gunit => format!("--gunit_{}", test_arg),
        _ => panic!("unexpected test type"),
    }
}

pub fn start_tests(
    tests: &Vec<ftest::Invocation>,
    run_listener_proxy: &ftest::RunListenerProxy,
) -> Result<HashMap<String, CaseListenerProxy>, Error> {
    let mut run_listener_proxies = HashMap::new();
    for test in tests {
        let test_name = test.name.clone().expect("No test name.");

        let (case_listener_proxy, case_listener) =
            endpoints::create_proxy::<ftest::CaseListenerMarker>();
        run_listener_proxy.on_test_case_started(
            &test,
            ftest::StdHandles::default(),
            case_listener,
        )?;

        run_listener_proxies.insert(test_name, case_listener_proxy);
    }

    Ok(run_listener_proxies)
}

pub fn unique_test_result_filename() -> String {
    format!("test_result-{}.json", uuid::Uuid::new_v4())
}

/// Opens the syscall test dependencies ROMFS and mount script, returning a `DataMount`.
async fn get_syscall_data_mount(test_pkg_dir: &fio::DirectoryProxy) -> Result<DataMount, Error> {
    let romfs_file =
        directory::open_file(test_pkg_dir, SYSCALL_TEST_DEPS_ROMFS_PATH, fio::PERM_READABLE)
            .await
            .with_context(|| format!("Could not open {}", SYSCALL_TEST_DEPS_ROMFS_PATH))?;
    let client_end = romfs_file
        .into_client_end()
        .map_err(|s| anyhow!("Failed to convert romfs file to client end: {:?}", s))?;
    let block_spec = BlockSpec {
        id: "test_deps".to_string(),
        mode: BlockMode::ReadOnly,
        format: BlockFormat::File(client_end),
    };
    let mount_file = file::open_in_namespace(MOUNT_SYSCALL_DEPS_SCRIPT_PATH, fio::PERM_READABLE)
        .with_context(|| format!("Could not open {}", MOUNT_SYSCALL_DEPS_SCRIPT_PATH))?;
    let mount_script_file = mount_file
        .into_client_end()
        .map_err(|s| anyhow!("Failed to convert mount script file to client end: {:?}", s))?;
    Ok(DataMount::new(block_spec, mount_script_file))
}

/// Gets the absolute guest filepath for test results when running syscall tests on a guest.
pub fn get_guest_syscall_test_output_path(guest_output_filename: &str) -> PathBuf {
    Path::new(GUEST_TEST_ROOT).join(guest_output_filename)
}

/// Prepares `debian_guest` to execute a syscall test binary by ensuring test dependencies are
/// mounted, and the test binary has been pushed to the guest.
pub async fn prepare_guest_syscall_test(
    test_start_info: &mut frunner::ComponentStartInfo,
    debian_guest: &DebianGuest,
) -> Result<PathBuf, Error> {
    let host_binary_location = runner::get_program_binary(test_start_info)?;
    let guest_binary_path =
        get_guest_syscall_test_binary_path(test_start_info, &host_binary_location)?;

    let namespace_entries =
        test_start_info.ns.take().ok_or_else(|| anyhow!("Missing namespace."))?;
    let mut test_component_ns = Namespace::try_from(namespace_entries)?;
    test_start_info.ns = Some(test_component_ns.clone().try_into()?);
    let test_pkg_dir = test_component_ns
        .remove(&NamespacePath::new("/pkg")?)
        .ok_or_else(|| anyhow!("Could not find /pkg in namespace!"))?
        .into_proxy();

    if !debian_guest.is_bootstrapped() {
        let data_mount = get_syscall_data_mount(&test_pkg_dir).await?;
        debian_guest.configure_data_mount(data_mount);
    }

    let content_hash = read_package_file_hash(&test_pkg_dir, &host_binary_location).await;
    // TODO(https://fxbug.dev/585583492): Stage each component in its own isolated guest directory.
    debian_guest
        .push_data_to_guest_once(&guest_binary_path, content_hash.as_deref(), async || {
            open_guest_syscall_test_binary(&test_pkg_dir, &host_binary_location).await
        })
        .await?;

    Ok(guest_binary_path)
}

/// Attempts to read the binary's Merkle hash, returning `None` if it cannot be read or the package
/// metadata does not contain the Merkle content entry.
async fn read_package_file_hash(
    pkg_dir: &fio::DirectoryProxy,
    package_relative_path: &str,
) -> Option<String> {
    // Read `meta/contents` instead of the package-wide `/pkg/meta` root so that rebuilding a single
    // binary in a multi-binary test package only invalidates the single binary's cached entry.
    let meta_contents = match directory::read_file_to_string(pkg_dir, PACKAGE_META_CONTENTS_PATH)
        .await
    {
        Ok(contents) => contents,
        Err(err) => {
            log::warn!(
                err:?,
                path = PACKAGE_META_CONTENTS_PATH;
                "Failed to read package meta/contents; staging guest test binary unconditionally"
            );
            return None;
        }
    };

    let normalized_path = package_relative_path.trim_start_matches("./");
    let content_hash = meta_contents.lines().find_map(|line| {
        let (path, hash) = line.rsplit_once('=')?;
        if path == normalized_path && !hash.is_empty() { Some(hash.to_string()) } else { None }
    });
    if content_hash.is_none() {
        log::warn!(
            binary_path = package_relative_path;
            "Package meta/contents missing entry for test binary; staging unconditionally"
        );
    }
    content_hash
}

/// Gets the absolute guest filepath for the test binary when running syscall tests on a guest.
///
/// Appends a hash of the component's `resolved_url` and package-relative binary path so distinct
/// test components with identical executable basenames do not collide on the shared guest.
// TODO(https://fxbug.dev/585583492): Give each test component an isolated directory on the guest
// (reusing the mounted dependencies) instead of hash-mangling binary filenames in `/`.
fn get_guest_syscall_test_binary_path(
    start_info: &frunner::ComponentStartInfo,
    host_binary_location: &str,
) -> Result<PathBuf, Error> {
    let resolved_url =
        start_info.resolved_url.as_deref().ok_or_else(|| anyhow!("Missing resolved URL"))?;
    let binary_name = Path::new(host_binary_location)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("Binary path format was unexpected."))?;

    let mut hasher = DefaultHasher::new();
    resolved_url.hash(&mut hasher);
    host_binary_location.hash(&mut hasher);
    let identity_hash = hasher.finish();

    Ok(Path::new(GUEST_TEST_ROOT).join(format!("{}_{:016x}", binary_name, identity_hash)))
}

/// Opens the test binary from the package directory to push to a guest for syscall testing.
async fn open_guest_syscall_test_binary(
    pkg_dir: &fio::DirectoryProxy,
    host_binary_location: &str,
) -> Result<endpoints::ClientEnd<fio::FileMarker>, Error> {
    directory::open_file(pkg_dir, host_binary_location, fio::PERM_READABLE)
        .await?
        .into_client_end()
        .map_err(|_| anyhow!("Converting test bin file to client end failed"))
}

#[cfg(test)]
pub(crate) struct TestPackageDir {
    path: PathBuf,
}

#[cfg(test)]
impl TestPackageDir {
    pub(crate) fn new(prefix: &str) -> Self {
        let path = PathBuf::from(format!("/tmp/{}-{}", prefix, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp pkg dir");
        Self { path }
    }

    pub(crate) fn with_guest_binary(binary_rel_path: &str) -> Self {
        let pkg = Self::new("test-guest-pkg");
        pkg.write_file(SYSCALL_TEST_DEPS_ROMFS_PATH, b"mock romfs");
        pkg.write_file(binary_rel_path, b"mock binary");
        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            format!(
                "{binary_rel_path}=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            ),
        );
        pkg
    }

    pub(crate) fn write_file(&self, rel_path: &str, contents: impl AsRef<[u8]>) {
        let full_path = self.path.join(rel_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(full_path, contents).expect("write fixture file");
    }

    pub(crate) fn remove_file(&self, rel_path: &str) {
        std::fs::remove_file(self.path.join(rel_path)).expect("remove fixture file");
    }

    pub(crate) fn open_proxy(&self) -> fio::DirectoryProxy {
        directory::open_in_namespace(
            self.path.to_str().expect("valid UTF-8 temp path"),
            fio::PERM_READABLE,
        )
        .expect("open temp pkg dir")
    }

    pub(crate) fn start_info(
        &self,
        resolved_url: &str,
        binary: &str,
        extra_program_entries: &[(&str, &str)],
    ) -> frunner::ComponentStartInfo {
        let pkg_client = self.open_proxy().into_client_end().expect("pkg dir client end");
        let mut entries = vec![fdata::DictionaryEntry {
            key: "binary".to_string(),
            value: Some(Box::new(fdata::DictionaryValue::Str(binary.to_string()))),
        }];
        for (key, value) in extra_program_entries {
            entries.push(fdata::DictionaryEntry {
                key: (*key).to_string(),
                value: Some(Box::new(fdata::DictionaryValue::Str((*value).to_string()))),
            });
        }
        frunner::ComponentStartInfo {
            resolved_url: Some(resolved_url.to_string()),
            program: Some(fdata::Dictionary { entries: Some(entries), ..Default::default() }),
            ns: Some(vec![ComponentNamespaceEntry {
                path: Some("/pkg".to_string()),
                directory: Some(pkg_client),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }
}

#[cfg(test)]
impl Drop for TestPackageDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debian_guest::test_helpers::MockDebianGuest;

    #[fuchsia::test]
    async fn test_get_syscall_data_mount_missing_romfs_errs() {
        let pkg = TestPackageDir::new("test-get-syscall-data-mount-missing");
        let pkg_dir = pkg.open_proxy();
        assert!(get_syscall_data_mount(&pkg_dir).await.is_err());
    }

    #[fuchsia::test]
    async fn test_get_syscall_data_mount_success() {
        let pkg = TestPackageDir::new("test-get-syscall-data-mount-success");
        pkg.write_file(SYSCALL_TEST_DEPS_ROMFS_PATH, b"mock romfs");
        let pkg_dir = pkg.open_proxy();
        assert!(get_syscall_data_mount(&pkg_dir).await.is_ok());
    }

    #[fuchsia::test]
    async fn test_prepare_guest_syscall_test_stages_and_invalidates_binary() {
        const BINARY_PATH: &str = "data/tests/starnix_linux_foo_bin";
        let pkg = TestPackageDir::new("test-prepare-guest-syscall-test");
        pkg.write_file(BINARY_PATH, b"mock binary v1");
        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            format!(
                "data/tests/other_bin=0000000000000000000000000000000000000000000000000000000000000000\n\
                 {BINARY_PATH}=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            ),
        );

        let first_url = "fuchsia-pkg://fuchsia.com/pkg_a#meta/foo.cm";

        // An unbootstrapped guest requires the dependency ROMFS to configure its data mount.
        let unbootstrapped_guest = DebianGuest::new("unbootstrapped_test_guest".to_string());
        let mut unbootstrapped_start_info = pkg.start_info(first_url, BINARY_PATH, &[]);
        assert!(
            prepare_guest_syscall_test(&mut unbootstrapped_start_info, &unbootstrapped_guest)
                .await
                .is_err()
        );

        // Once bootstrapped (`MockDebianGuest` uses a pre-bootstrapped proxy), preparing tests
        // skips re-opening the dependency ROMFS altogether.
        let mock = MockDebianGuest::with_put_file_handler(|remote_path, _attempt| {
            assert!(remote_path.starts_with("/starnix_linux_foo_bin_"));
            zx::sys::ZX_OK
        });

        let mut first_start_info = pkg.start_info(first_url, BINARY_PATH, &[]);
        let first_path = prepare_guest_syscall_test(&mut first_start_info, &mock.guest)
            .await
            .expect("first chunk prepares guest binary");
        assert!(
            first_path
                .to_str()
                .expect("valid UTF-8 guest path")
                .starts_with("/starnix_linux_foo_bin_")
        );
        assert_eq!(mock.put_file_count(), 1);

        // Changing an unrelated entry in `meta/contents` must keep `BINARY_PATH` cached and
        // short-circuit before opening the binary or pushing over vsock again.
        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            format!(
                "data/tests/other_bin=1111111111111111111111111111111111111111111111111111111111111111\n\
                 {BINARY_PATH}=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            ),
        );
        pkg.remove_file(BINARY_PATH);
        let mut second_start_info = pkg.start_info(first_url, BINARY_PATH, &[]);
        let second_path = prepare_guest_syscall_test(&mut second_start_info, &mock.guest)
            .await
            .expect("second chunk reuses staged binary");
        assert_eq!(second_path, first_path);
        assert_eq!(mock.put_file_count(), 1);
        pkg.write_file(BINARY_PATH, b"mock binary v2");

        // Rebuilding the test binary changes its hash in `meta/contents` (while other earlier
        // entries remain); the runner re-pushes to the same guest path and caches the new hash.
        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            format!(
                "data/tests/other_bin=1111111111111111111111111111111111111111111111111111111111111111\n\
                 {BINARY_PATH}=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
            ),
        );
        let mut rebuilt_start_info = pkg.start_info(first_url, BINARY_PATH, &[]);
        let rebuilt_path = prepare_guest_syscall_test(&mut rebuilt_start_info, &mock.guest)
            .await
            .expect("rebuilt binary is re-pushed");
        assert_eq!(rebuilt_path, first_path);
        assert_eq!(mock.put_file_count(), 2);

        // A distinct component with an identical executable basename gets an isolated guest path
        // and stages its own binary rather than colliding with the first component.
        let mut distinct_start_info =
            pkg.start_info("fuchsia-pkg://fuchsia.com/pkg_b#meta/foo.cm", BINARY_PATH, &[]);
        let distinct_path = prepare_guest_syscall_test(&mut distinct_start_info, &mock.guest)
            .await
            .expect("distinct component stages its own binary");
        assert_ne!(distinct_path, first_path);
        assert_eq!(mock.put_file_count(), 3);

        // If `meta/contents` is readable but has an empty hash or omits `BINARY_PATH`,
        // `read_package_file_hash` must return `None` rather than caching an empty string.
        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            format!(
                "data/tests/other_bin=1111111111111111111111111111111111111111111111111111111111111111\n\
                 {BINARY_PATH}=\n"
            ),
        );
        assert_eq!(read_package_file_hash(&pkg.open_proxy(), BINARY_PATH).await, None);

        pkg.write_file(
            PACKAGE_META_CONTENTS_PATH,
            "data/tests/other_bin=1111111111111111111111111111111111111111111111111111111111111111\n",
        );
        assert_eq!(read_package_file_hash(&pkg.open_proxy(), BINARY_PATH).await, None);

        // Without `meta/contents`, the runner falls back to pushing unconditionally on every call.
        pkg.remove_file(PACKAGE_META_CONTENTS_PATH);
        let mut unhashed_first = pkg.start_info(first_url, BINARY_PATH, &[]);
        prepare_guest_syscall_test(&mut unhashed_first, &mock.guest)
            .await
            .expect("first missing meta/contents push succeeds");
        assert_eq!(mock.put_file_count(), 4);

        let mut unhashed_second = pkg.start_info(first_url, BINARY_PATH, &[]);
        prepare_guest_syscall_test(&mut unhashed_second, &mock.guest)
            .await
            .expect("second missing meta/contents push is not cached");
        assert_eq!(mock.put_file_count(), 5);

        mock.finish().await;
    }
}
