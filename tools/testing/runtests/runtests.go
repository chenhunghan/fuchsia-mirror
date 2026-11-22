// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package runtests contains specifics related to the runtests command.
package runtests

import (
	"log"
	"slices"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
)

// TODO(olivernewman): Move the contents of this file into a separate library as
// it's no longer specific to runtests - in fact, runtests only implements a
// subset of the types and fields defined here.

const (
	// TestSummaryFilename is the summary file name expected by the fuchsia
	// recipe module.
	TestSummaryFilename = "summary.json"

	// TestOutputFilename is the default output file name for a test.
	TestOutputFilename = "stdout-and-stderr.txt"

	// MultipliedShardPrefix is the prefix added to the names of shards that
	// run multiplied tests.
	MultipliedShardPrefix = "multiplied:"

	// MultipliedRunTagKey is the test tag key attached by testrunner to
	// multiplied test runs.
	MultipliedRunTagKey = "is_multiplied_run"
)

// TestStatus is the exit status of a test.
type TestStatus string

// Possible test statuses, names chosen for consistency with ResultDB:
// https://chromium.googlesource.com/infra/luci/luci-go/+/15458901113063d2a0d95be2e27a245c4cfd5690/resultdb/proto/v1/test_result.proto#142
const (
	// TestSuccess represents a passed test.
	TestSuccess TestStatus = "PASS"

	// TestFailure represents a failed test.
	TestFailure TestStatus = "FAIL"

	// TestAborted represents an aborted test (likely a timeout).
	TestAborted TestStatus = "ABORT"

	// TestSkipped represents a skipped test.
	TestSkipped TestStatus = "SKIP"

	// TestInfraFailure means that the test failed because of infra related issue.
	TestInfraFailure TestStatus = "INFRA_FAIL"

	// TestExonerated represents an exonerated test.
	TestExonerated TestStatus = "EXONERATED"

	// TestPrecluded represents a test whose execution was precluded by a
	// higher-level failure (e.g. fixture setup failure or shard timeout).
	TestPrecluded TestStatus = "PRECLUDED"
)

// IsFailure returns whether a test result corresponds to any failure condition
// (failure, timeout, etc.).
func IsFailure(tr TestStatus) bool {
	return tr != TestSuccess && tr != TestSkipped && tr != TestPrecluded
}

// FailureCategory classifies who owns investigating a shard-level failure mode
// and whether it should fail presubmit vs. surface as an infra failure.
type FailureCategory string

const (
	CategoryInfra         FailureCategory = "infra"
	CategoryInfraOrSystem FailureCategory = "infra_or_system"
	CategorySystemOrTest  FailureCategory = "system_or_test"
)

// FailureModeResult indicates whether a tefmocheck failure mode was fatal to
// the shard, observed as a flake, or passed without any detected failure modes.
type FailureModeResult string

const (
	FailureModePass  FailureModeResult = "PASS"
	FailureModeFail  FailureModeResult = "FAIL"
	FailureModeFlaky FailureModeResult = "FLAKY"
)

// InfraDomain identifies the sub-system or layer responsible for an infrastructure failure mode.
type InfraDomain string

const (
	InfraDomainTools       InfraDomain = "tools"
	InfraDomainEnvironment InfraDomain = "environment"
)

// InfraDetails provides infrastructure-specific metadata when a failure mode's
// effective category is CategoryInfra.
type InfraDetails struct {
	Domain InfraDomain `json:"domain,omitempty"`
}

// CheckFailure describes a single matched tefmocheck check on a shard.
type CheckFailure struct {
	Name           string            `json:"name"`
	Result         FailureModeResult `json:"result"`
	FailureMessage string            `json:"failure_message,omitempty"`
	OutputFiles    []string          `json:"output_files,omitempty"`
	Category       FailureCategory   `json:"category,omitempty"`
	Infra          *InfraDetails     `json:"infra,omitempty"`
}

// FailureModeSummary aggregates shard-level failure modes detected by
// tefmocheck so recipes can classify task failures without parsing synthetic
// test entries out of Tests.
type FailureModeSummary struct {
	Name     string            `json:"name,omitempty"`
	Category FailureCategory   `json:"category,omitempty"`
	Result   FailureModeResult `json:"result"`
	Infra    *InfraDetails     `json:"infra,omitempty"`
	Failures []CheckFailure    `json:"failures"`
}

// TestSummary is a summary of a suite of test runs. It represents the output
// file format of a runtests invocation.
type TestSummary struct {
	// Tests is a list of the details of the test runs.
	Tests []TestDetails `json:"tests"`

	// Outputs gives the suite-wide outputs, mapping canonical name of the
	// output to its path.
	Outputs map[string]string `json:"outputs,omitempty"`
	// ExonerationError records any error encountered while loading the
	// stability exoneration config or querying LUCI Analysis when stability
	// exoneration fails open.
	ExonerationError string `json:"exoneration_error,omitempty"`

	// FailureMode summarizes shard-level failure modes detected by tefmocheck,
	// or is nil when no non-exoneration failure mode matched.
	FailureMode *FailureModeSummary `json:"failure_mode,omitempty"`
}

// DataSink is a data sink exported by the test.
type DataSink struct {
	// Name is the name of the sink.
	Name string `json:"name"`

	// File is the file containing the sink data.
	File string `json:"file"`
}

// DataSinkMap is mapping from a data sink name to a list of the corresponding
// data sink files.
type DataSinkMap map[string][]DataSink

// TestDetails contains the details of a test run.
type TestDetails struct {
	TestResult

	// Name is the name of the test.
	Name string `json:"name"`

	// GNLabel is label of the test target (with toolchain).
	GNLabel string `json:"gn_label"`

	// SourceLabel is the build-system agnostic (either GN or Bazel) label corresponding to the test.
	SourceLabel string `json:"source_label"`

	// Status is the status of the test.
	Status TestStatus `json:"result"`

	// DataSinks gives the data sinks attached to a test.
	DataSinks DataSinkMap `json:"data_sinks,omitempty"`

	// StartTime is the UTC time when the test was started.
	StartTime time.Time `json:"start_time"`

	// EndTime is the UTC time when the test completed. Used to calculate the DurationMillis.
	EndTime time.Time `json:"-"`

	// Duration is how long the test execution took.
	DurationMillis int64 `json:"duration_milliseconds"`

	// IsTestingFailureMode is true iff this test was produced by tefmocheck.
	IsTestingFailureMode bool `json:"is_testing_failure_mode"`

	// Affected indicates whether the test is affected by the change under test.
	// It will only be set for tests running within tryjobs.
	Affected bool `json:"affected"`

	// Tags contain test metadata.
	Tags []build.TestTag `json:"tags"`

	// Test metadata
	Metadata metadata.TestMetadata `json:"metadata,omitempty"`

	// RunIndex is the index of this test run among all the runs of the same test.
	RunIndex int `json:"-"`

	// The combined stdout and stderr from this test.
	Stdio []byte `json:"-"`

	IsMultipliedRun bool `json:"-"`
}

// Passed indicates whether the test completed successfully. This will be false
// if the test timed out or failed.
func (r TestDetails) Passed() bool {
	return r.Status == TestSuccess
}

func (r TestDetails) Duration() time.Duration {
	return r.EndTime.Sub(r.StartTime)
}

// TestResult contains the details of a test run. A test can structure its results
// in this format and write it to TEST_OUTPUT_SUMMARY_PATH.
type TestResult struct {
	// OutputFiles are paths to the test's output files relative to the OutputDir.
	OutputFiles []string `json:"output_files"`

	// OutputDir is the common dir that all the OutputFiles should be located in.
	OutputDir string `json:"output_dir"`

	// Cases is individual test case results.
	Cases []TestCaseResult `json:"cases"`

	// FailureReason is an optional structured error message or explanation of the failure.
	// This will be ignored if the test status is a success. Host tests must exit with
	// a non-zero exit code to be considered a failure.
	FailureReason *FailureReason `json:"failure_reason,omitempty"`

	// ExonerationExplanation is an optional plain-text explanation of why an
	// exonerated test was marked EXONERATED (surfaced via ResultDB's
	// TestExoneration.explanation_html).
	ExonerationExplanation string `json:"exoneration_explanation,omitempty"`

	// SetupSucceeded indicates whether test setup completed successfully.
	SetupSucceeded *bool `json:"setup_succeeded,omitempty"`

	// TeardownSucceeded indicates whether test teardown completed successfully.
	TeardownSucceeded *bool `json:"teardown_succeeded,omitempty"`

	// ExitCode is the process exit code of the test binary.
	ExitCode *int `json:"exit_code,omitempty"`

	// Runner is the test's runner, if known.
	Runner string `json:"runner,omitempty"`
}

// FailureReasonError represents a problem that caused a test to fail, such as a crash
// or expectation failure.
// Mirrors third_party/luci-go/resultdb/proto/v1/FailureReason_Error.
type FailureReasonError struct {
	Message string `json:"message,omitempty"`
	Trace   string `json:"trace,omitempty"`
}

// Provides structured information about why a test failed. This information is helpful
// for developers debugging failures and is also used by systems like LUCI Analysis
// to cluster similar failures together.
// It typically contains one or more error messages and potentially stack traces.
// Note: The total combined size of all errors within this message (as measured
// by proto.Size()) must not exceed 16,384 bytes.
// Mirrors third_party/luci-go/resultdb/proto/v1/FailureReason.
type FailureReason struct {
	Errors               []*FailureReasonError `json:"errors,omitempty"`
	TruncatedErrorsCount int32                 `json:"truncated_errors_count,omitempty"`
}

// FailureReasonFromMessage does not truncate message length so that full diagnostic output
// is preserved in summary.json for debugging. Downstream consumers and uploaders (such as resultdb/lib.go)
// are responsible for truncating error messages to meet specific RPC payload constraints.
func FailureReasonFromMessage(message string) *FailureReason {
	msg := strings.TrimSpace(message)
	if msg == "" {
		return nil
	}
	return &FailureReason{
		Errors: []*FailureReasonError{
			{Message: msg},
		},
	}
}

// TestCaseResult contains the details of a single test case, nested within a
// top-level TestDetails.
type TestCaseResult struct {
	DisplayName string        `json:"display_name"`
	SuiteName   string        `json:"suite_name"`
	CaseName    string        `json:"case_name"`
	Status      TestStatus    `json:"status"`
	Duration    time.Duration `json:"duration_nanos"`
	// Format is the test runner used to execute the test.
	Format string `json:"format"`
	// FailureReason is structured information about why a test case failed, including repeatable errors.
	FailureReason *FailureReason `json:"failure_reason,omitempty"`
	// ExonerationExplanation is an optional plain-text explanation of why an
	// exonerated test case was marked EXONERATED (surfaced via ResultDB's
	// TestExoneration.explanation_html).
	ExonerationExplanation string   `json:"exoneration_explanation,omitempty"`
	OutputFiles            []string `json:"output_files,omitempty"`
	// The directory where the OutputFiles live if given as relative paths.
	OutputDir string `json:"output_dir,omitempty"`
	// Tags contain test case metadata.
	Tags []build.TestTag `json:"tags"`
}

const (
	FormatFTF        = "FTF"
	FormatGoogleTest = "GoogleTest"
	FormatRust       = "Rust"
	FormatGo         = "Go"
)

var runnerFamilies = []struct {
	prefix string
	format string
}{
	{"gtest_", FormatGoogleTest},
	{"gunit_", FormatGoogleTest},
	{"zxtest_", FormatGoogleTest},
	{"rust_test_", FormatRust},
	{"go_test_", FormatGo},
}

// FormatForRunner returns the test case format for runner. Keep in sync with
// format_for_runner in pilot.rs.
func FormatForRunner(runner string) string {
	for _, family := range runnerFamilies {
		if strings.HasPrefix(runner, family.prefix) {
			return family.format
		}
	}
	return FormatFTF
}

// SplitCaseName splits name into suite and case names for format. Keep in sync
// with split_case_name in pilot.rs.
func SplitCaseName(format, name string) (suiteName, caseName string, ok bool) {
	switch format {
	case FormatGoogleTest:
		suite, testCase, found := strings.Cut(name, ".")
		if !found || suite == "" || testCase == "" {
			return "", name, false
		}
		return suite, testCase, true
	case FormatRust:
		i := strings.LastIndex(name, "::")
		if i < 0 {
			return "", name, true
		}
		if name[i+len("::"):] == "" {
			return "", name, false
		}
		return name[:i], name[i+len("::"):], true
	case FormatGo:
		return "", name, true
	default:
		return "", name, true
	}
}

// CaseNamesForRunner returns the format for runner and the split names.
func CaseNamesForRunner(runner string, names []string) (format string, suiteNames, caseNames []string) {
	format = FormatForRunner(runner)
	suiteNames = make([]string, len(names))
	caseNames = make([]string, len(names))
	for i, name := range names {
		suite, testCase, ok := SplitCaseName(format, name)
		if !ok {
			log.Printf("[Warn] case %q cannot be split as %s for runner %q; reporting all cases as %s", name, format, runner, FormatFTF)
			for j := range names {
				suiteNames[j] = ""
			}
			return FormatFTF, suiteNames, slices.Clone(names)
		}
		suiteNames[i], caseNames[i] = suite, testCase
	}
	return format, suiteNames, caseNames
}
