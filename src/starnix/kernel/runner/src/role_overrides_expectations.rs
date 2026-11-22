// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Role-resolution expectations evaluation logic for Starnix container
//! `task_role_overrides` configurations, evaluated through [`SchedulerManager`].

use anyhow::{Context, Error, bail, ensure};
use fidl_fuchsia_component_decl as fdecl;
use serde::Deserialize;
use starnix_core::task::{DEFAULT_SYSTEM_MODE, RoleOverrides, SchedulerManager, SchedulerState};
use starnix_kernel_runner::ContainerProgram;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

const META_DIR_PATH: &str = "/pkg/meta";
const SELF_CM_FILENAME: &str = "role_overrides_expectations_test.cm";
const EXPECTATIONS_JSON5_PATH: &str = "/pkg/data/role_overrides_expectations.json5";
const DEFAULT_REALTIME_PRIORITY: i32 = 1;
const NON_REALTIME_PRIORITY: i32 = 0;

fn default_cgroup() -> String {
    "/".to_string()
}

fn default_policy() -> String {
    "SCHED_NORMAL".to_string()
}

fn default_system_mode() -> String {
    DEFAULT_SYSTEM_MODE.to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleOverrideExpectation {
    process: String,
    thread: String,
    #[serde(default = "default_cgroup")]
    cgroup: String,
    #[serde(default)]
    is_leader: bool,
    #[serde(default = "default_policy")]
    policy: String,
    #[serde(default)]
    nice: i32,
    #[serde(default)]
    realtime_priority: Option<i32>,
    #[serde(default)]
    uclamp_min: u32,
    #[serde(default = "default_system_mode")]
    system_mode: String,
    expected_role: String,
}

fn select_container_manifest_path(
    meta_dir: &Path,
    entries: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf, Error> {
    let candidates: Vec<PathBuf> = entries
        .into_iter()
        .filter(|path| {
            path.extension() == Some(OsStr::new("cm"))
                && path.file_name() != Some(OsStr::new(SELF_CM_FILENAME))
        })
        .collect();
    match <[PathBuf; 1]>::try_from(candidates) {
        Ok([manifest_path]) => Ok(manifest_path),
        Err(candidates) => bail!(
            "expected exactly 1 container .cm manifest in {} (excluding {SELF_CM_FILENAME}), found {}: {:?}",
            meta_dir.display(),
            candidates.len(),
            candidates,
        ),
    }
}

fn find_container_manifest_path(meta_dir: &Path) -> Result<PathBuf, Error> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(meta_dir).with_context(|| format!("reading {}", meta_dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry in {}", meta_dir.display()))?;
        entries.push(entry.path());
    }
    select_container_manifest_path(meta_dir, entries)
}

fn load_role_overrides_from_bytes(
    manifest_bytes: &[u8],
    manifest_label: &str,
) -> Result<RoleOverrides, Error> {
    let component: fdecl::Component = fidl::unpersist(manifest_bytes)
        .with_context(|| format!("unpersisting compiled manifest {manifest_label}"))?;
    let program_dictionary =
        component
            .program
            .as_ref()
            .and_then(|program| program.info.as_ref())
            .with_context(|| format!("manifest {manifest_label} has no `program` dictionary"))?;
    let program: ContainerProgram = runner::serde::deserialize_program(program_dictionary)
        .with_context(|| format!("deserializing `ContainerProgram` from {manifest_label}"))?;
    program
        .build_role_overrides()
        .with_context(|| format!("compiling `task_role_overrides` from {manifest_label}"))
}

fn load_role_overrides(manifest_path: &Path) -> Result<RoleOverrides, Error> {
    let manifest_label = manifest_path.display().to_string();
    let manifest_bytes = fs::read(manifest_path)
        .with_context(|| format!("reading compiled manifest {manifest_label}"))?;
    load_role_overrides_from_bytes(&manifest_bytes, &manifest_label)
}

fn default_realtime_priority(policy: &str) -> i32 {
    match policy {
        "SCHED_FIFO" | "SCHED_RR" => DEFAULT_REALTIME_PRIORITY,
        _ => NON_REALTIME_PRIORITY,
    }
}

fn evaluate_expectations(
    role_overrides: &RoleOverrides,
    raw_expectations: &str,
    expectations_source_label: &str,
    manifest_label: &str,
) -> Result<(), Error> {
    let cases: Vec<RoleOverrideExpectation> = serde_json5::from_str(raw_expectations)
        .with_context(|| format!("parsing expectations file {expectations_source_label}"))?;
    ensure!(
        !cases.is_empty(),
        "expectations file {expectations_source_label} must contain at least one case"
    );

    let mut failures = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let realtime_priority =
            case.realtime_priority.unwrap_or_else(|| default_realtime_priority(&case.policy));
        let state =
            SchedulerState::for_tests(&case.policy, case.nice, realtime_priority, case.uclamp_min)
                .with_context(|| format!("invalid scheduler state in expectation case #{index}"))?;

        let (actual_role, _is_mode_dependent) = SchedulerManager::resolve_role_name_for_tests(
            role_overrides,
            &case.process,
            &case.thread,
            &case.cgroup,
            case.is_leader,
            &state,
            &case.system_mode,
        )
        .with_context(|| format!("invalid task state in expectation case #{index}"))?;

        if actual_role != case.expected_role {
            failures.push(format!(
                "  case #{index} (process={:?}, thread={:?}, cgroup={:?}, is_leader={}, policy={}, nice={}, rt_prio={}, uclamp_min={}, system_mode={:?}):\n    expected_role: {:?}\n    actual_role:   {:?}",
                case.process,
                case.thread,
                case.cgroup,
                case.is_leader,
                case.policy,
                case.nice,
                realtime_priority,
                case.uclamp_min,
                case.system_mode,
                case.expected_role,
                actual_role,
            ));
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} of {} role override expectation(s) failed for {manifest_label}:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }

    Ok(())
}

/// Discovers the packaged container `.cm` manifest under `/pkg/meta` and evaluates
/// `/pkg/data/role_overrides_expectations.json5` against its `task_role_overrides`.
pub fn run_from_pkg() -> Result<(), Error> {
    let container_manifest_path = find_container_manifest_path(Path::new(META_DIR_PATH))?;
    let manifest_label = container_manifest_path.display().to_string();
    let role_overrides = load_role_overrides(&container_manifest_path)?;
    let raw_expectations = fs::read_to_string(EXPECTATIONS_JSON5_PATH)
        .with_context(|| format!("reading expectations file {EXPECTATIONS_JSON5_PATH}"))?;
    evaluate_expectations(
        &role_overrides,
        &raw_expectations,
        EXPECTATIONS_JSON5_PATH,
        &manifest_label,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        evaluate_expectations, load_role_overrides_from_bytes, select_container_manifest_path,
    };
    use fidl_fuchsia_component_decl as fdecl;
    use starnix_core::task::RoleOverrides;
    use std::path::{Path, PathBuf};

    fn sample_overrides() -> RoleOverrides {
        let mut builder = RoleOverrides::new();
        builder.add("custom.render").process("^app$").thread("^RenderThread$");
        builder.build().expect("valid regexes")
    }

    #[fuchsia::test]
    fn evaluate_expectations_passes_valid_cases_and_default_rt_priorities() {
        let overrides = sample_overrides();
        let expectations = r#"[
            // JSON5 comments and unquoted keys are supported.
            {
                process: "app",
                thread: "RenderThread",
                expected_role: "custom.render",
            },
            {
                process: "other",
                thread: "idle",
                policy: "SCHED_IDLE",
                expected_role: "fuchsia.starnix.fair.0",
            },
            {
                process: "other",
                thread: "fifo_default",
                policy: "SCHED_FIFO",
                expected_role: "fuchsia.starnix.realtime",
            },
            {
                process: "other",
                thread: "rr_default",
                policy: "SCHED_RR",
                expected_role: "fuchsia.starnix.realtime",
            },
        ]"#;
        evaluate_expectations(&overrides, expectations, "expectations.json5", "container.cm")
            .expect("valid expectation cases should pass");
    }

    #[fuchsia::test]
    fn evaluate_expectations_rejects_empty_case_list() {
        let overrides = sample_overrides();
        let err = evaluate_expectations(&overrides, "[]", "expectations.json5", "container.cm")
            .expect_err("empty case list must fail");
        assert!(format!("{err:#}").contains("must contain at least one case"), "{err:#}");
    }

    #[fuchsia::test]
    fn evaluate_expectations_rejects_unknown_json_fields() {
        let overrides = sample_overrides();
        let expectations = r#"[{
            "process": "app",
            "thread": "RenderThread",
            "expected_role": "custom.render",
            "unexpected_field": 42
        }]"#;
        let err =
            evaluate_expectations(&overrides, expectations, "expectations.json5", "container.cm")
                .expect_err("unknown field must fail");
        assert!(format!("{err:#}").contains("unknown field `unexpected_field`"), "{err:#}");
    }

    #[fuchsia::test]
    fn evaluate_expectations_reports_role_mismatches() {
        let overrides = sample_overrides();
        let expectations = r#"[
            {
                "process": "app",
                "thread": "RenderThread",
                "expected_role": "custom.render"
            },
            {
                "process": "app",
                "thread": "worker",
                "expected_role": "wrong.role"
            }
        ]"#;
        let err =
            evaluate_expectations(&overrides, expectations, "expectations.json5", "container.cm")
                .expect_err("role mismatch must fail");
        let message = format!("{err:#}");
        assert!(message.contains("1 of 2 role override expectation(s) failed"), "{message}");
        assert!(message.contains("case #1"), "{message}");
        assert!(message.contains("expected_role: \"wrong.role\""), "{message}");
        assert!(message.contains("actual_role:   \"fuchsia.starnix.fair.16\""), "{message}");
    }

    #[fuchsia::test]
    fn evaluate_expectations_rejects_invalid_scheduler_state() {
        let overrides = sample_overrides();
        let expectations = r#"[{
            "process": "app",
            "thread": "worker",
            "nice": 20,
            "expected_role": "fuchsia.starnix.fair.6"
        }]"#;
        let err =
            evaluate_expectations(&overrides, expectations, "expectations.json5", "container.cm")
                .expect_err("invalid nice in case must fail");
        assert!(
            format!("{err:#}").contains("invalid scheduler state in expectation case #0"),
            "{err:#}"
        );
    }

    #[fuchsia::test]
    fn evaluate_expectations_rejects_leader_with_mismatched_names() {
        let overrides = sample_overrides();
        let expectations = r#"[{
            "process": "app",
            "thread": "RenderThread",
            "is_leader": true,
            "expected_role": "custom.render"
        }]"#;
        let err =
            evaluate_expectations(&overrides, expectations, "expectations.json5", "container.cm")
                .expect_err("is_leader=true with process != thread must fail");
        let message = format!("{err:#}");
        assert!(message.contains("invalid task state in expectation case #0"), "{message}");
        assert!(message.contains("is_leader=true requires matching process_name"), "{message}");
    }

    #[fuchsia::test]
    fn select_container_manifest_path_validates_candidates() {
        let meta = Path::new("/pkg/meta");

        // Zero container .cm files (only test manifest and metadata files).
        let err = select_container_manifest_path(
            meta,
            vec![
                PathBuf::from("/pkg/meta/role_overrides_expectations_test.cm"),
                PathBuf::from("/pkg/meta/package"),
                PathBuf::from("/pkg/meta/contents"),
            ],
        )
        .expect_err("missing container .cm must fail");
        assert!(format!("{err:#}").contains("found 0"), "{err:#}");

        // Multiple container .cm files.
        let err = select_container_manifest_path(
            meta,
            vec![
                PathBuf::from("/pkg/meta/role_overrides_expectations_test.cm"),
                PathBuf::from("/pkg/meta/first_container.cm"),
                PathBuf::from("/pkg/meta/second_container.cm"),
            ],
        )
        .expect_err("ambiguous container .cm files must fail");
        assert!(format!("{err:#}").contains("found 2"), "{err:#}");

        // Exactly one container .cm file alongside test manifest and non-.cm files.
        let selected = select_container_manifest_path(
            meta,
            vec![
                PathBuf::from("/pkg/meta/package"),
                PathBuf::from("/pkg/meta/role_overrides_expectations_test.cm"),
                PathBuf::from("/pkg/meta/my_container.cm"),
            ],
        )
        .expect("single container .cm must succeed");
        assert_eq!(selected, PathBuf::from("/pkg/meta/my_container.cm"));
    }

    #[fuchsia::test]
    fn load_role_overrides_from_bytes_rejects_missing_program() {
        let bytes = fidl::persist(&fdecl::Component::default()).expect("persist empty component");
        let err = load_role_overrides_from_bytes(&bytes, "empty.cm")
            .expect_err("manifest without program dictionary must fail");
        assert!(format!("{err:#}").contains("has no `program` dictionary"), "{err:#}");
    }
}
