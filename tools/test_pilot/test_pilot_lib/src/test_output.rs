// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::{Deserialize, Serialize};
use std::io::BufReader;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};
use thiserror::Error;

/// Distinguished exit status used to indicate the test was run as requested but that the
/// test itself failed.
const FAIL_EXIT_STATUS: i32 = 86;

/// The body of the test output summary, matching `runtests.TestResult` in
/// `//tools/testing/runtests/runtests.go`.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct TestResult {
    /// Paths to the test's output files relative to `output_dir`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,

    /// The common directory that all `output_files` are located in.
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output_dir: String,

    /// Individual test case results.
    #[serde(default)]
    pub cases: Vec<TestCaseResult>,

    /// Optional structured error message or explanation of the failure.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<FailureReason>,

    /// Indicates whether test setup completed successfully.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_succeeded: Option<bool>,

    /// Indicates whether test teardown completed successfully.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_succeeded: Option<bool>,

    /// The process exit code of the test binary.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl TestResult {
    /// Merges `TestResult` read from a file into self. Returns Ok(true) if the file was merged
    /// successfully and `self` changed as a result, Ok(false) if the file did not exist or
    /// no change was made.
    pub fn maybe_merge_file(&mut self, path: &PathBuf) -> Result<bool, TestOutputError> {
        if let Ok(file) = fs::File::open(path) {
            let mut reader = BufReader::new(file);
            let summary_from_file: TestResult = serde_json::from_reader(&mut reader)
                .map_err(|e| TestOutputError::SummaryRead { path: path.clone(), source: e })?;
            Ok(self.merge(summary_from_file))
        } else {
            Ok(false)
        }
    }

    /// Writes this summary to a file. Returns an error if any case has `TestStatus::NotSpecified`.
    pub fn write(&self, path: &PathBuf) -> Result<(), TestOutputError> {
        for case in &self.cases {
            if case.status == TestStatus::NotSpecified {
                return Err(TestOutputError::UnspecifiedCaseStatus {
                    path: path.clone(),
                    case_name: case.case_name.clone(),
                });
            }
        }

        fs::write(path, serde_json::to_string_pretty(self).unwrap())
            .map_err(|e| TestOutputError::SummaryWrite { path: path.clone(), source: e })?;

        Ok(())
    }

    /// Merges `other` into self. This is used for merging test and postprocessor summaries
    /// into the aggregate summary. Returns `true` if `self` changed as a result of the merge.
    pub fn merge(&mut self, other: TestResult) -> bool {
        let mut changed = false;

        for file in other.output_files {
            if !self.output_files.contains(&file) {
                self.output_files.push(file);
                changed = true;
            }
        }

        if !other.output_dir.is_empty() && other.output_dir != self.output_dir {
            self.output_dir = other.output_dir;
            changed = true;
        }

        for other_case in other.cases {
            if let Some(existing_case) = self.cases.iter_mut().find(|c| {
                c.case_name == other_case.case_name
                    && (c.suite_name == other_case.suite_name
                        || c.suite_name.is_empty()
                        || other_case.suite_name.is_empty())
            }) {
                changed |= existing_case.merge(other_case);
            } else {
                self.cases.push(other_case);
                changed = true;
            }
        }

        if other.failure_reason.is_some() && other.failure_reason != self.failure_reason {
            self.failure_reason = other.failure_reason;
            changed = true;
        }

        if other.setup_succeeded.is_some() && other.setup_succeeded != self.setup_succeeded {
            self.setup_succeeded = other.setup_succeeded;
            changed = true;
        }
        if other.teardown_succeeded.is_some() && other.teardown_succeeded != self.teardown_succeeded
        {
            self.teardown_succeeded = other.teardown_succeeded;
            changed = true;
        }
        if other.exit_code.is_some() && other.exit_code != self.exit_code {
            self.exit_code = other.exit_code;
            changed = true;
        }

        changed
    }
}

/// Test output regarding a single test case, matching `runtests.TestCaseResult` in
/// `//tools/testing/runtests/runtests.go`.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct TestCaseResult {
    #[serde(default)]
    pub display_name: String,

    #[serde(default)]
    pub suite_name: String,

    pub case_name: String,

    #[serde(default)]
    #[serde(skip_serializing_if = "TestStatus::is_not_specified")]
    pub status: TestStatus,

    #[serde(
        default,
        rename = "duration_nanos",
        serialize_with = "serialize_duration_nanos",
        deserialize_with = "deserialize_duration_nanos"
    )]
    pub duration: Duration,

    #[serde(default)]
    pub format: String,

    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<FailureReason>,

    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,

    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output_dir: String,

    #[serde(default)]
    pub tags: Vec<TestTag>,
}

impl TestCaseResult {
    /// Merges `other` into self. This is used for merging test and postprocessor case summaries
    /// into the aggregate summary. Returns `true` if `self` changed as a result of the merge.
    pub fn merge(&mut self, other: TestCaseResult) -> bool {
        let mut changed = false;

        if !other.display_name.is_empty() && other.display_name != self.display_name {
            self.display_name = other.display_name;
            changed = true;
        }

        if !other.suite_name.is_empty() && other.suite_name != self.suite_name {
            self.suite_name = other.suite_name;
            changed = true;
        }

        if other.status != TestStatus::NotSpecified && other.status != self.status {
            self.status = other.status;
            self.failure_reason = other.failure_reason;
            changed = true;
        } else if other.failure_reason.is_some() && other.failure_reason != self.failure_reason {
            self.failure_reason = other.failure_reason;
            changed = true;
        }

        if !other.duration.is_zero() && other.duration != self.duration {
            self.duration = other.duration;
            changed = true;
        }

        if !other.format.is_empty() && other.format != self.format {
            self.format = other.format;
            changed = true;
        }

        for file in other.output_files {
            if !self.output_files.contains(&file) {
                self.output_files.push(file);
                changed = true;
            }
        }

        if !other.output_dir.is_empty() && other.output_dir != self.output_dir {
            self.output_dir = other.output_dir;
            changed = true;
        }

        for other_tag in other.tags {
            if let Some(existing_tag) = self.tags.iter_mut().find(|t| t.key == other_tag.key) {
                if existing_tag.value != other_tag.value {
                    existing_tag.value = other_tag.value;
                    changed = true;
                }
            } else {
                self.tags.push(other_tag);
                changed = true;
            }
        }

        changed
    }
}

/// Possible test statuses, matching `runtests.TestStatus` in
/// `//tools/testing/runtests/runtests.go`.
#[derive(Serialize, Deserialize, Debug, Default, Copy, Clone, PartialEq, Eq)]
pub enum TestStatus {
    /// Default status value used when a postprocessor summary updates an existing case
    /// without specifying a status. This value must never appear in a final summary.
    #[default]
    #[serde(rename = "NOT_SPECIFIED", alias = "")]
    NotSpecified,

    /// Represents a passed test.
    #[serde(rename = "PASS")]
    Pass,

    /// Represents a failed test.
    #[serde(rename = "FAIL")]
    Fail,

    /// Represents an aborted test (e.g. timeout or canceled).
    #[serde(rename = "ABORT")]
    Abort,

    /// Represents a skipped test.
    #[serde(rename = "SKIP")]
    Skip,

    /// Represents a test that failed due to an infrastructure-related issue.
    #[serde(rename = "INFRA_FAIL")]
    InfraFail,

    /// Represents an exonerated test.
    #[serde(rename = "EXONERATED")]
    Exonerated,
}

impl TestStatus {
    /// Returns true if the status is `NotSpecified`.
    pub fn is_not_specified(&self) -> bool {
        *self == TestStatus::NotSpecified
    }

    /// Determines whether this status represents a failure state.
    pub fn is_failure(&self) -> bool {
        matches!(self, TestStatus::Fail | TestStatus::Abort | TestStatus::InfraFail)
    }
}

/// Provides structured information about why a test or test case failed, matching
/// `runtests.FailureReason` in `//tools/testing/runtests/runtests.go`.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct FailureReason {
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FailureReasonError>,

    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub truncated_errors_count: i32,
}

impl FailureReason {
    /// Constructs a `FailureReason` from a string message if non-empty after trimming,
    /// containing a single error message.
    pub fn from_message(message: impl Into<String>) -> Option<Self> {
        let message = message.into();
        let trimmed = message.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(Self {
                errors: vec![FailureReasonError {
                    message: trimmed.to_string(),
                    trace: String::new(),
                }],
                truncated_errors_count: 0,
            })
        }
    }
}

/// Represents a problem that caused a test to fail, matching `runtests.FailureReasonError` in
/// `//tools/testing/runtests/runtests.go`.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct FailureReasonError {
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,

    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub trace: String,
}

/// Key-value metadata tag for a test case.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct TestTag {
    pub key: String,
    pub value: String,
}

fn is_zero_i32(to_test: &i32) -> bool {
    *to_test == 0
}

fn serialize_duration_nanos<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_u128(duration.as_nanos())
}

fn deserialize_duration_nanos<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let nanos = u128::deserialize(deserializer)?;
    let secs = (nanos / 1_000_000_000) as u64;
    let subsec_nanos = (nanos % 1_000_000_000) as u32;
    Ok(Duration::new(secs, subsec_nanos))
}

const TEST_SUBDIR: &str = "test";
const EXECUTION_LOG_FILE_PATH: &str = "execution_log.json";
const TEST_CONFIG_FILE_PATH: &str = "test_config.json";
const OUTPUT_SUMMARY_FILE_PATH: &str = "output_summary.json";
const INVOCATION_STDOUT_FILE_PATH: &str = "invocation_stdout.txt";
const INVOCATION_STDERR_FILE_PATH: &str = "invocation_stderr.txt";

/// Creates path values based on output directory structure.
pub struct OutputDirectory<'a> {
    path: &'a Path,
}

impl<'a> OutputDirectory<'a> {
    /// Creates a new `OutputDirectory` located at `Path`.
    pub fn new(path: &'a Path) -> Self {
        Self { path }
    }

    /// Returns the full path of the specified file or subdirectory in the top-level output
    /// directory.
    pub fn join(&self, subdir: &str) -> PathBuf {
        self.path.join(subdir)
    }

    /// Returns the full path of the execution log.
    pub fn execution_log(&self) -> PathBuf {
        self.join(EXECUTION_LOG_FILE_PATH)
    }

    /// Returns the full path of the main summary.
    pub fn main_summary(&self) -> PathBuf {
        self.join(OUTPUT_SUMMARY_FILE_PATH)
    }

    /// Returns the full path of the test subdirectory, where the test deposits its summary
    /// and artifacts.
    pub fn test_subdir(&self) -> PathBuf {
        self.join(TEST_SUBDIR)
    }

    /// Returns the full path of the summary generated by the test.
    pub fn test_summary(&self) -> PathBuf {
        self.test_subdir().join(OUTPUT_SUMMARY_FILE_PATH)
    }

    /// Returns the full path of the test configuration.
    pub fn test_config(&self) -> PathBuf {
        self.join(TEST_CONFIG_FILE_PATH)
    }

    /// Returns the full path of the file to which test-pilot writes the test's stdout.
    pub fn test_invocation_stdout(&self) -> PathBuf {
        self.test_subdir().join(INVOCATION_STDOUT_FILE_PATH)
    }

    /// Returns the full path of the file to which test-pilot writes the test's stderr.
    pub fn test_invocation_stderr(&self) -> PathBuf {
        self.test_subdir().join(INVOCATION_STDERR_FILE_PATH)
    }

    /// Returns the full path of the subdirectory for the specified postprocessor.
    pub fn postprocessor_subdir(&self, binary: &PathBuf) -> PathBuf {
        self.join(postprocessor_name(binary))
    }

    /// Returns the full path of the summary for a specified postprocessor.
    pub fn postprocessor_summary(&self, binary: &PathBuf) -> PathBuf {
        self.postprocessor_subdir(binary).join(OUTPUT_SUMMARY_FILE_PATH)
    }

    /// Returns the full path of the file to which test-pilot writes a post-processor's stdout.
    pub fn postprocessor_invocation_stdout(&self, binary: &PathBuf) -> PathBuf {
        self.postprocessor_subdir(binary).join(INVOCATION_STDOUT_FILE_PATH)
    }

    /// Returns the full path of the file to which test-pilot writes a post-processor's stderr.
    pub fn postprocessor_invocation_stderr(&self, binary: &PathBuf) -> PathBuf {
        self.postprocessor_subdir(binary).join(INVOCATION_STDERR_FILE_PATH)
    }

    /// Converts a full path into a path relative to the output directory.
    pub fn make_relative(&self, path: &PathBuf) -> Option<PathBuf> {
        path.strip_prefix(self.path).ok().map(|p| p.to_path_buf())
    }
}

/// Returns the name of a postprocessor subdirectory given a path to the postprocessor's binary.
fn postprocessor_name<'b>(binary: &'b PathBuf) -> &'b str {
    binary.file_name().expect("binary path has file name").to_str().expect("file name is ansi")
}

/// Determines whether `exit_status` indicates the test run ran correctly, but the test itself
/// failed.
pub fn is_fail_exit_status(exit_status: std::process::ExitStatus) -> bool {
    exit_status.code() == Some(FAIL_EXIT_STATUS) || exit_status.into_raw() == FAIL_EXIT_STATUS
}

/// Returns a `FailureReason` from an exit status if the exit status indicates an error
/// (neither 0 nor `FAIL_EXIT_STATUS`).
pub fn failure_reason_from_exit_status(
    exit_status: std::process::ExitStatus,
) -> Option<FailureReason> {
    let code = exit_status.code().unwrap_or_else(|| exit_status.into_raw());
    match code {
        0 | FAIL_EXIT_STATUS => None,
        status => {
            FailureReason::from_message(format!("test binary failed with exit status {status}"))
        }
    }
}

/// Error encountered while reading or writing a test output summary.
#[derive(Debug, Error)]
pub enum TestOutputError {
    #[error("Error reading output summary file {path}: {source}")]
    SummaryRead {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("Error writing output summary file {path}: {source}")]
    SummaryWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "Cannot write output summary file {path}: test case '{case_name}' has no status specified"
    )]
    UnspecifiedCaseStatus { path: PathBuf, case_name: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs::File;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use tempfile::tempdir;

    fn case(case_name: &str, status: TestStatus, detail: &str) -> TestCaseResult {
        TestCaseResult {
            display_name: String::from(case_name),
            case_name: String::from(case_name),
            status,
            failure_reason: FailureReason::from_message(detail),
            ..Default::default()
        }
    }

    #[fuchsia::test]
    fn test_merge_case_properties() {
        let mut under_test = TestCaseResult {
            display_name: String::from("case_a"),
            suite_name: String::from("suite_a"),
            case_name: String::from("case_a"),
            status: TestStatus::Fail,
            duration: Duration::from_millis(1),
            format: String::from("FTF"),
            failure_reason: FailureReason::from_message("test outcome a"),
            output_files: vec![String::from("a"), String::from("b")],
            output_dir: String::from("/out/a"),
            tags: vec![
                TestTag { key: String::from("x"), value: String::from("x_value") },
                TestTag { key: String::from("y"), value: String::from("y_value") },
            ],
        };

        // Non-trivial values from argument take precedence. New output_files and tags are inserted.
        // Existing tags with updated values get updated.
        assert!(under_test.merge(TestCaseResult {
            display_name: String::from("case_a_new"),
            suite_name: String::from("suite_a_new"),
            case_name: String::from("case_a"),
            status: TestStatus::Fail,
            duration: Duration::from_millis(3),
            format: String::from("GoogleTest"),
            failure_reason: FailureReason::from_message("test outcome b"),
            output_files: vec![String::from("a"), String::from("c")],
            output_dir: String::from("/out/b"),
            tags: vec![
                TestTag { key: String::from("x"), value: String::from("new_x_value") },
                TestTag { key: String::from("z"), value: String::from("z_value") },
            ],
        }));
        assert_eq!(
            under_test,
            TestCaseResult {
                display_name: String::from("case_a_new"),
                suite_name: String::from("suite_a_new"),
                case_name: String::from("case_a"),
                status: TestStatus::Fail,
                duration: Duration::from_millis(3),
                format: String::from("GoogleTest"),
                failure_reason: FailureReason::from_message("test outcome b"),
                output_files: vec![String::from("a"), String::from("b"), String::from("c")],
                output_dir: String::from("/out/b"),
                tags: vec![
                    TestTag { key: String::from("x"), value: String::from("new_x_value") },
                    TestTag { key: String::from("y"), value: String::from("y_value") },
                    TestTag { key: String::from("z"), value: String::from("z_value") },
                ],
            }
        );

        // Trivial values from argument are not copied, while failure_reason is updated when status
        // is NotSpecified.
        assert!(under_test.merge(TestCaseResult {
            display_name: String::new(),
            suite_name: String::new(),
            case_name: String::from("case_a"),
            status: TestStatus::NotSpecified,
            duration: Duration::ZERO,
            format: String::new(),
            failure_reason: FailureReason::from_message("test outcome c"),
            output_files: vec![String::from("a")],
            output_dir: String::new(),
            tags: vec![],
        }));
        assert_eq!(
            under_test,
            TestCaseResult {
                display_name: String::from("case_a_new"),
                suite_name: String::from("suite_a_new"),
                case_name: String::from("case_a"),
                status: TestStatus::Fail,
                duration: Duration::from_millis(3),
                format: String::from("GoogleTest"),
                failure_reason: FailureReason::from_message("test outcome c"),
                output_files: vec![String::from("a"), String::from("b"), String::from("c")],
                output_dir: String::from("/out/b"),
                tags: vec![
                    TestTag { key: String::from("x"), value: String::from("new_x_value") },
                    TestTag { key: String::from("y"), value: String::from("y_value") },
                    TestTag { key: String::from("z"), value: String::from("z_value") },
                ],
            }
        );

        // Merging with trivial or identical values does not change self and returns false.
        let before = under_test.clone();
        assert!(!under_test.merge(TestCaseResult {
            case_name: String::from("case_a"),
            status: TestStatus::NotSpecified,
            failure_reason: FailureReason::from_message("test outcome c"),
            output_files: vec![String::from("a")],
            ..Default::default()
        }));
        assert_eq!(under_test, before);

        assert!(!under_test.merge(TestCaseResult {
            case_name: String::from("case_a"),
            duration: Duration::from_millis(3),
            output_files: vec![String::from("a")],
            ..Default::default()
        }));
        assert_eq!(under_test, before);
    }

    #[fuchsia::test]
    fn test_merge_cases() {
        let mut under_test = TestResult {
            cases: vec![
                case("case_a", TestStatus::Fail, "case_a_outcome"),
                case("case_b", TestStatus::Fail, "case_b_outcome"),
            ],
            ..Default::default()
        };

        // Non-trivial values from argument take precedence. New cases are appended.
        // Existing cases get updated.
        assert!(under_test.merge(TestResult {
            cases: vec![
                case("case_a", TestStatus::Fail, "case_a_new_outcome"),
                case("case_c", TestStatus::Fail, "case_c_outcome"),
            ],
            ..Default::default()
        }));
        let expected = TestResult {
            cases: vec![
                case("case_a", TestStatus::Fail, "case_a_new_outcome"),
                case("case_b", TestStatus::Fail, "case_b_outcome"),
                case("case_c", TestStatus::Fail, "case_c_outcome"),
            ],
            ..Default::default()
        };
        assert_eq!(under_test, expected);

        // Merging unchanged cases returns false and leaves self unchanged.
        assert!(!under_test.merge(TestResult {
            cases: vec![case("case_a", TestStatus::Fail, "case_a_new_outcome")],
            ..Default::default()
        }));
        assert_eq!(under_test, expected);
    }

    #[fuchsia::test]
    fn test_merge_lifecycle_fields() {
        // When self fields are None and other has Some, other's fields take precedence.
        let mut under_test = TestResult {
            setup_succeeded: None,
            teardown_succeeded: None,
            exit_code: None,
            ..Default::default()
        };
        assert!(under_test.merge(TestResult {
            setup_succeeded: Some(true),
            teardown_succeeded: Some(false),
            exit_code: Some(0),
            ..Default::default()
        }));
        assert_eq!(under_test.setup_succeeded, Some(true));
        assert_eq!(under_test.teardown_succeeded, Some(false));
        assert_eq!(under_test.exit_code, Some(0));

        // When other has None, self fields are preserved.
        assert!(!under_test.merge(TestResult {
            setup_succeeded: None,
            teardown_succeeded: None,
            exit_code: None,
            ..Default::default()
        }));
        assert_eq!(under_test.setup_succeeded, Some(true));
        assert_eq!(under_test.teardown_succeeded, Some(false));
        assert_eq!(under_test.exit_code, Some(0));

        // When other has Some with identical values, self fields are preserved and merge returns false.
        assert!(!under_test.merge(TestResult {
            setup_succeeded: Some(true),
            teardown_succeeded: Some(false),
            exit_code: Some(0),
            ..Default::default()
        }));
        assert_eq!(under_test.setup_succeeded, Some(true));
        assert_eq!(under_test.teardown_succeeded, Some(false));
        assert_eq!(under_test.exit_code, Some(0));

        // When other has Some, it overwrites existing self fields.
        assert!(under_test.merge(TestResult {
            setup_succeeded: Some(false),
            teardown_succeeded: Some(true),
            exit_code: Some(86),
            ..Default::default()
        }));
        assert_eq!(under_test.setup_succeeded, Some(false));
        assert_eq!(under_test.teardown_succeeded, Some(true));
        assert_eq!(under_test.exit_code, Some(86));
    }

    #[fuchsia::test]
    fn test_merge_statuses() {
        let statuses = [
            TestStatus::NotSpecified,
            TestStatus::Skip,
            TestStatus::Pass,
            TestStatus::Abort,
            TestStatus::Fail,
            TestStatus::InfraFail,
            TestStatus::Exonerated,
        ];
        let specified_statuses = [
            TestStatus::Skip,
            TestStatus::Pass,
            TestStatus::Abort,
            TestStatus::Fail,
            TestStatus::InfraFail,
            TestStatus::Exonerated,
        ];

        // When other is unspecified and failure_reason is none, merging leaves self unchanged.
        for status in &statuses {
            let mut under_test = case("c", *status, "self");
            assert!(!under_test.merge(TestCaseResult {
                case_name: String::from("c"),
                status: TestStatus::NotSpecified,
                failure_reason: None,
                ..Default::default()
            }));
            assert_eq!(under_test, case("c", *status, "self"));
        }

        // When other's status is specified:
        for self_status in &statuses {
            for other_status in &specified_statuses {
                // If other has a different status, or same status with a different failure_reason,
                // merging changes self to equal other.
                let mut under_test = case("c", *self_status, "self");
                let other = case("c", *other_status, "other");
                assert!(under_test.merge(other.clone()));
                assert_eq!(under_test, other);

                // If other's status is different, self takes other's status and None failure_reason.
                // If other's status is the same and other's failure_reason is None, self is unchanged.
                let mut under_test = case("c", *self_status, "self");
                let other = TestCaseResult {
                    display_name: String::from("c"),
                    case_name: String::from("c"),
                    status: *other_status,
                    failure_reason: None,
                    ..Default::default()
                };
                if self_status != other_status {
                    assert!(under_test.merge(other.clone()));
                    assert_eq!(under_test, other);
                } else {
                    assert!(!under_test.merge(other));
                    assert_eq!(under_test, case("c", *self_status, "self"));
                }
            }
        }

        // When other's status and failure_reason are identical, merging leaves self unchanged.
        for status in &specified_statuses {
            let mut under_test = case("c", *status, "self");
            let other = case("c", *status, "self");
            assert!(!under_test.merge(other));
            assert_eq!(under_test, case("c", *status, "self"));
        }

        // When other's status is unspecified:
        for status in &statuses {
            // Different failure_reason changes self's failure_reason to other's.
            let mut under_test = case("c", *status, "self");
            assert!(under_test.merge(case("c", TestStatus::NotSpecified, "other")));
            assert_eq!(under_test, case("c", *status, "other"));

            // Same failure_reason leaves self unchanged and returns false.
            let mut under_test = case("c", *status, "self");
            assert!(!under_test.merge(case("c", TestStatus::NotSpecified, "self")));
            assert_eq!(under_test, case("c", *status, "self"));
        }
    }

    #[fuchsia::test]
    fn test_failure_reason_from_exit_status() {
        assert_eq!(failure_reason_from_exit_status(ExitStatus::from_raw(0)), None);
        assert_eq!(failure_reason_from_exit_status(ExitStatus::from_raw(FAIL_EXIT_STATUS)), None);
        assert_eq!(
            failure_reason_from_exit_status(ExitStatus::from_raw(1)),
            FailureReason::from_message("test binary failed with exit status 1")
        );
    }

    #[fuchsia::test]
    fn test_summary_write() {
        let under_test = TestResult {
            output_files: vec![String::from("a"), String::from("b"), String::from("c")],
            output_dir: String::from("/out/dir"),
            cases: vec![
                TestCaseResult {
                    display_name: String::from("case_a"),
                    suite_name: String::new(),
                    case_name: String::from("case_a"),
                    status: TestStatus::Fail,
                    duration: Duration::from_millis(3),
                    format: String::from("FTF"),
                    failure_reason: FailureReason::from_message("case_a_outcome"),
                    output_files: vec![String::from("case_a/log.txt")],
                    output_dir: String::from("/out/dir"),
                    tags: vec![TestTag {
                        key: String::from("test_outcome"),
                        value: String::from("FAILED"),
                    }],
                },
                TestCaseResult {
                    display_name: String::from("case_b"),
                    suite_name: String::new(),
                    case_name: String::from("case_b"),
                    status: TestStatus::Pass,
                    duration: Duration::from_millis(5),
                    format: String::from("FTF"),
                    failure_reason: None,
                    output_files: vec![],
                    output_dir: String::from("/out/dir"),
                    tags: vec![TestTag {
                        key: String::from("test_outcome"),
                        value: String::from("PASSED"),
                    }],
                },
            ],
            failure_reason: FailureReason::from_message("test outcome b"),
            setup_succeeded: Some(true),
            teardown_succeeded: Some(true),
            exit_code: Some(86),
        };

        let temp_dir = tempdir().expect("to create temporary directory");
        let file_path = temp_dir.path().join("test_output_summary.json");

        under_test.write(&file_path).expect("to write summary");

        let file = File::open(&file_path).expect("to open summary");
        let mut reader = BufReader::new(file);
        let file_contents: Value = serde_json5::from_reader(&mut reader).expect("to read summary");
        assert_eq!(
            file_contents,
            json!({
                "output_files": ["a", "b", "c"],
                "output_dir": "/out/dir",
                "cases": [
                    {
                        "display_name": "case_a",
                        "suite_name": "",
                        "case_name": "case_a",
                        "status": "FAIL",
                        "duration_nanos": 3_000_000,
                        "format": "FTF",
                        "failure_reason": {
                            "errors": [{"message": "case_a_outcome"}]
                        },
                        "output_files": ["case_a/log.txt"],
                        "output_dir": "/out/dir",
                        "tags": [{"key": "test_outcome", "value": "FAILED"}]
                    },
                    {
                        "display_name": "case_b",
                        "suite_name": "",
                        "case_name": "case_b",
                        "status": "PASS",
                        "duration_nanos": 5_000_000,
                        "format": "FTF",
                        "output_dir": "/out/dir",
                        "tags": [{"key": "test_outcome", "value": "PASSED"}]
                    }
                ],
                "failure_reason": {
                    "errors": [{"message": "test outcome b"}]
                },
                "setup_succeeded": true,
                "teardown_succeeded": true,
                "exit_code": 86
            })
        );

        temp_dir.close().expect("to close temporary directory");
    }

    #[fuchsia::test]
    fn test_summary_write_rejects_not_specified_status() {
        let under_test = TestResult {
            output_dir: String::from("/out/dir"),
            cases: vec![TestCaseResult {
                case_name: String::from("case_unspecified"),
                status: TestStatus::NotSpecified,
                ..Default::default()
            }],
            ..Default::default()
        };

        let temp_dir = tempdir().expect("to create temporary directory");
        let file_path = temp_dir.path().join("test_output_summary.json");

        let err = under_test
            .write(&file_path)
            .expect_err("writing summary with NotSpecified case status should fail");
        assert!(matches!(
            err,
            TestOutputError::UnspecifiedCaseStatus { case_name, .. }
                if case_name == "case_unspecified"
        ));

        temp_dir.close().expect("to close temporary directory");
    }

    #[fuchsia::test]
    fn test_summary_maybe_merge_file() {
        let temp_dir = tempdir().expect("to create temporary directory");
        let mut under_test = TestResult::default();

        assert_eq!(
            false,
            under_test
                .maybe_merge_file(&temp_dir.path().join("does_not_exist.json"))
                .expect("success"),
        );

        assert_eq!(TestResult::default(), under_test);

        let file_path = temp_dir.path().join("test_output_summary.json");
        fs::write(
            &file_path,
            serde_json::to_string_pretty(&json!({
                "output_files": ["a", "b"],
                "output_dir": "/out",
                "cases": [
                    {
                        "display_name": "case_a",
                        "suite_name": "",
                        "case_name": "case_a",
                        "status": "FAIL",
                        "duration_nanos": 3_000_000,
                        "format": "FTF",
                        "failure_reason": {"errors": [{"message": "case_a_outcome"}]},
                        "tags": []
                    },
                    {
                        "display_name": "case_b",
                        "suite_name": "",
                        "case_name": "case_b",
                        "status": "PASS",
                        "duration_nanos": 1_000_000,
                        "format": "FTF",
                        "tags": []
                    }
                ],
                "failure_reason": {"errors": [{"message": "test outcome b"}]}
            }))
            .unwrap(),
        )
        .expect("json write succeeds");

        assert_eq!(true, under_test.maybe_merge_file(&file_path).expect("success"));

        assert_eq!(
            TestResult {
                output_files: vec![String::from("a"), String::from("b")],
                output_dir: String::from("/out"),
                cases: vec![
                    TestCaseResult {
                        display_name: String::from("case_a"),
                        case_name: String::from("case_a"),
                        status: TestStatus::Fail,
                        duration: Duration::from_millis(3),
                        format: String::from("FTF"),
                        failure_reason: FailureReason::from_message("case_a_outcome"),
                        ..Default::default()
                    },
                    TestCaseResult {
                        display_name: String::from("case_b"),
                        case_name: String::from("case_b"),
                        status: TestStatus::Pass,
                        duration: Duration::from_millis(1),
                        format: String::from("FTF"),
                        ..Default::default()
                    },
                ],
                failure_reason: FailureReason::from_message("test outcome b"),
                ..Default::default()
            },
            under_test
        );

        // Merging a postprocessor file that updates an existing case without specifying status
        // preserves the existing case's status.
        let postproc_file_path = temp_dir.path().join("postproc_summary.json");
        fs::write(
            &postproc_file_path,
            serde_json::to_string_pretty(&json!({
                "cases": [
                    {
                        "case_name": "case_b",
                        "output_files": ["postproc/extra.txt"]
                    }
                ]
            }))
            .unwrap(),
        )
        .expect("json write succeeds");

        assert_eq!(true, under_test.maybe_merge_file(&postproc_file_path).expect("success"));
        assert_eq!(under_test.cases[1].status, TestStatus::Pass);
        assert_eq!(under_test.cases[1].output_files, vec![String::from("postproc/extra.txt")]);

        // Merging a file with no new information returns Ok(false).
        let no_change_file_path = temp_dir.path().join("no_change_summary.json");
        fs::write(
            &no_change_file_path,
            serde_json::to_string_pretty(&json!({
                "output_dir": "/out",
                "failure_reason": {"errors": [{"message": "test outcome b"}]},
            }))
            .unwrap(),
        )
        .expect("json write succeeds");

        assert_eq!(false, under_test.maybe_merge_file(&no_change_file_path).expect("success"));

        temp_dir.close().expect("to close temporary directory");
    }

    #[fuchsia::test]
    fn test_summary_maybe_merge_file_with_lifecycle_fields() {
        let temp_dir = tempdir().expect("to create temporary directory");
        let file_path = temp_dir.path().join("test_output_summary.json");
        fs::write(
            &file_path,
            serde_json::to_string_pretty(&json!({
                "setup_succeeded": false,
                "teardown_succeeded": true,
                "exit_code": 1,
            }))
            .unwrap(),
        )
        .expect("json write succeeds");

        let mut under_test = TestResult::default();
        assert!(under_test.maybe_merge_file(&file_path).expect("success"));
        assert_eq!(under_test.setup_succeeded, Some(false));
        assert_eq!(under_test.teardown_succeeded, Some(true));
        assert_eq!(under_test.exit_code, Some(1));

        // Merging the same file again produces no changes and returns false.
        assert!(!under_test.maybe_merge_file(&file_path).expect("success"));

        temp_dir.close().expect("to close temporary directory");
    }

    #[test]
    fn test_failure_reason_from_message() {
        assert_eq!(FailureReason::from_message(""), None);
        assert_eq!(FailureReason::from_message("   \n"), None);
        assert_eq!(
            FailureReason::from_message("  test failure  "),
            Some(FailureReason {
                errors: vec![FailureReasonError {
                    message: String::from("test failure"),
                    trace: String::new(),
                }],
                truncated_errors_count: 0,
            })
        );
    }

    #[test]
    fn test_is_failure() {
        assert!(!TestStatus::NotSpecified.is_failure());
        assert!(!TestStatus::Skip.is_failure());
        assert!(!TestStatus::Pass.is_failure());
        assert!(!TestStatus::Exonerated.is_failure());
        assert!(TestStatus::Abort.is_failure());
        assert!(TestStatus::Fail.is_failure());
        assert!(TestStatus::InfraFail.is_failure());
    }
}
