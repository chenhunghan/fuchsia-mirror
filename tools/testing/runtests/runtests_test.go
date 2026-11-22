// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package runtests

import (
	"encoding/json"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
)

func TestTestSummarySerialization(t *testing.T) {
	exitCode := 1
	setupOk := true
	teardownOk := false

	summary := TestSummary{
		Tests: []TestDetails{
			{
				Name:           "fuchsia-pkg://fuchsia.com/my-test#meta/my-test.cm",
				GNLabel:        "//src/sys/test:my-test(//build/toolchain:fuchsia)",
				SourceLabel:    "//src/sys/test:my-test",
				Status:         TestFailure,
				StartTime:      time.Unix(1700000000, 0).UTC(),
				DurationMillis: 1234,
				TestResult: TestResult{
					OutputDir:              "my-test",
					OutputFiles:            []string{"stdout-and-stderr.txt"},
					FailureReason:          FailureReasonFromMessage("Assertion failed in main.cc:42"),
					ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met flake_rate criteria).",
					SetupSucceeded:         &setupOk,
					TeardownSucceeded:      &teardownOk,
					ExitCode:               &exitCode,
					Cases: []TestCaseResult{
						{
							DisplayName:            "TestFoo",
							SuiteName:              "MySuite",
							CaseName:               "TestFoo",
							Status:                 TestFailure,
							Duration:               500 * time.Millisecond,
							Format:                 "gtest",
							FailureReason:          FailureReasonFromMessage("Expected true, got false"),
							ExonerationExplanation: "Exonerated case explanation",
						},
					},
				},
			},
		},
		Outputs: map[string]string{
			"syslog.txt": "path/to/syslog.txt",
		},
	}

	data, err := json.Marshal(summary)
	if err != nil {
		t.Fatalf("json.Marshal failed: %v", err)
	}

	var parsed TestSummary
	if err := json.Unmarshal(data, &parsed); err != nil {
		t.Fatalf("json.Unmarshal failed: %v", err)
	}

	if len(parsed.Tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(parsed.Tests))
	}

	test := parsed.Tests[0]
	if test.Name != summary.Tests[0].Name {
		t.Errorf("test.Name mismatch: got %q, want %q", test.Name, summary.Tests[0].Name)
	}
	if test.ExitCode == nil || *test.ExitCode != exitCode {
		t.Errorf("test.ExitCode mismatch: got %v, want %v", test.ExitCode, exitCode)
	}
	if test.SetupSucceeded == nil || *test.SetupSucceeded != setupOk {
		t.Errorf("test.SetupSucceeded mismatch: got %v, want %v", test.SetupSucceeded, setupOk)
	}
	if test.TeardownSucceeded == nil || *test.TeardownSucceeded != teardownOk {
		t.Errorf("test.TeardownSucceeded mismatch: got %v, want %v", test.TeardownSucceeded, teardownOk)
	}
	if test.FailureReason == nil || len(test.FailureReason.Errors) != 1 || test.FailureReason.Errors[0].Message != "Assertion failed in main.cc:42" {
		t.Errorf("test.FailureReason mismatch: got %+v", test.FailureReason)
	}
	if test.ExonerationExplanation != summary.Tests[0].ExonerationExplanation {
		t.Errorf("test.ExonerationExplanation mismatch: got %q, want %q", test.ExonerationExplanation, summary.Tests[0].ExonerationExplanation)
	}
	if len(test.Cases) != 1 {
		t.Fatalf("expected 1 case, got %d", len(test.Cases))
	}
	if test.Cases[0].FailureReason == nil || len(test.Cases[0].FailureReason.Errors) != 1 || test.Cases[0].FailureReason.Errors[0].Message != "Expected true, got false" {
		t.Errorf("case.FailureReason mismatch: got %+v", test.Cases[0].FailureReason)
	}
	if test.Cases[0].ExonerationExplanation != summary.Tests[0].Cases[0].ExonerationExplanation {
		t.Errorf("case.ExonerationExplanation mismatch: got %q, want %q", test.Cases[0].ExonerationExplanation, summary.Tests[0].Cases[0].ExonerationExplanation)
	}
}

func TestTestResultOmitEmpty(t *testing.T) {
	result := TestResult{
		OutputDir:   "test_dir",
		OutputFiles: []string{"output.txt"},
	}

	data, err := json.Marshal(result)
	if err != nil {
		t.Fatalf("json.Marshal failed: %v", err)
	}

	jsonStr := string(data)
	for _, field := range []string{"setup_succeeded", "teardown_succeeded", "exit_code", "failure_reason", "exoneration_explanation"} {
		if jsonContainsField(jsonStr, field) {
			t.Errorf("expected field %q to be omitted when unset, got json: %s", field, jsonStr)
		}
	}
}

func TestFailureModeSummarySerialization(t *testing.T) {
	// Nil FailureMode should be omitted from TestSummary JSON.
	emptySummary := TestSummary{Tests: []TestDetails{}}
	emptyBytes, err := json.Marshal(emptySummary)
	if err != nil {
		t.Fatalf("json.Marshal(emptySummary) failed: %v", err)
	}
	var gotEmpty map[string]any
	if err := json.Unmarshal(emptyBytes, &gotEmpty); err != nil {
		t.Fatalf("json.Unmarshal(emptyBytes) failed: %v", err)
	}
	wantEmpty := map[string]any{
		"tests": []any{},
	}
	if diff := cmp.Diff(wantEmpty, gotEmpty); diff != "" {
		t.Errorf("json.Marshal(emptySummary) mismatch (-want +got):\n%s", diff)
	}

	// Domain is included when set, while empty optional fields (FailureMessage,
	// OutputFiles, uncategorized Category) are omitted.
	infraSummary := TestSummary{
		Tests: []TestDetails{},
		FailureMode: &FailureModeSummary{
			Name:     "task_state/BOT_DIED",
			Category: CategoryInfra,
			Result:   FailureModeFail,
			Infra:    &InfraDetails{Domain: InfraDomainTools},
			Failures: []CheckFailure{
				{
					Name:     "task_state/BOT_DIED",
					Result:   FailureModeFail,
					Category: CategoryInfra,
					Infra:    &InfraDetails{Domain: InfraDomainTools},
				},
				{
					Name:   "generic_check",
					Result: FailureModeFlaky,
				},
			},
		},
	}
	infraBytes, err := json.Marshal(infraSummary)
	if err != nil {
		t.Fatalf("json.Marshal(infraSummary) failed: %v", err)
	}
	var gotInfra map[string]any
	if err := json.Unmarshal(infraBytes, &gotInfra); err != nil {
		t.Fatalf("json.Unmarshal(infraBytes) failed: %v", err)
	}
	wantInfra := map[string]any{
		"tests": []any{},
		"failure_mode": map[string]any{
			"name":     "task_state/BOT_DIED",
			"category": string(CategoryInfra),
			"result":   string(FailureModeFail),
			"infra": map[string]any{
				"domain": string(InfraDomainTools),
			},
			"failures": []any{
				map[string]any{
					"name":     "task_state/BOT_DIED",
					"result":   string(FailureModeFail),
					"category": string(CategoryInfra),
					"infra": map[string]any{
						"domain": string(InfraDomainTools),
					},
				},
				map[string]any{
					"name":   "generic_check",
					"result": string(FailureModeFlaky),
				},
			},
		},
	}
	if diff := cmp.Diff(wantInfra, gotInfra); diff != "" {
		t.Errorf("json.Marshal(infraSummary) mismatch (-want +got):\n%s", diff)
	}

	var roundTrip TestSummary
	if err := json.Unmarshal(infraBytes, &roundTrip); err != nil {
		t.Fatalf("json.Unmarshal(infraBytes, &roundTrip) failed: %v", err)
	}
	if diff := cmp.Diff(infraSummary, roundTrip); diff != "" {
		t.Errorf("round-trip TestSummary mismatch (-want +got):\n%s", diff)
	}

	// FailureModePass serialization.
	passSummary := TestSummary{
		Tests: []TestDetails{},
		FailureMode: &FailureModeSummary{
			Result:   FailureModePass,
			Failures: []CheckFailure{},
		},
	}
	passBytes, err := json.Marshal(passSummary)
	if err != nil {
		t.Fatalf("json.Marshal(passSummary) failed: %v", err)
	}
	var gotPass map[string]any
	if err := json.Unmarshal(passBytes, &gotPass); err != nil {
		t.Fatalf("json.Unmarshal(passBytes) failed: %v", err)
	}
	wantPass := map[string]any{
		"tests": []any{},
		"failure_mode": map[string]any{
			"result":   string(FailureModePass),
			"failures": []any{},
		},
	}
	if diff := cmp.Diff(wantPass, gotPass); diff != "" {
		t.Errorf("json.Marshal(passSummary) mismatch (-want +got):\n%s", diff)
	}
}

func jsonContainsField(jsonStr, field string) bool {
	var m map[string]interface{}
	if err := json.Unmarshal([]byte(jsonStr), &m); err != nil {
		return false
	}
	_, ok := m[field]
	return ok
}

func TestIsFailure(t *testing.T) {
	testCases := []struct {
		status TestStatus
		want   bool
	}{
		{TestSuccess, false},
		{TestSkipped, false},
		{TestPrecluded, false},
		{TestFailure, true},
		{TestAborted, true},
		{TestInfraFailure, true},
		{TestExonerated, true},
	}
	for _, tc := range testCases {
		if got := IsFailure(tc.status); got != tc.want {
			t.Errorf("IsFailure(%q) = %v, want %v", tc.status, got, tc.want)
		}
	}
}

func TestTestPrecludedSerialization(t *testing.T) {
	summary := TestSummary{
		Tests: []TestDetails{
			{
				Name:   "fuchsia-pkg://fuchsia.com/my-test#meta/my-test.cm",
				Status: TestPrecluded,
				TestResult: TestResult{
					Cases: []TestCaseResult{
						{
							CaseName: "TestPrecludedCase",
							Status:   TestPrecluded,
						},
					},
				},
			},
		},
	}

	data, err := json.Marshal(summary)
	if err != nil {
		t.Fatalf("json.Marshal failed: %v", err)
	}

	var parsed TestSummary
	if err := json.Unmarshal(data, &parsed); err != nil {
		t.Fatalf("json.Unmarshal failed: %v", err)
	}

	if len(parsed.Tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(parsed.Tests))
	}
	if parsed.Tests[0].Status != TestPrecluded {
		t.Errorf("test.Status = %q, want %q", parsed.Tests[0].Status, TestPrecluded)
	}
	if parsed.Tests[0].Passed() {
		t.Errorf("test.Passed() = true, want false for precluded test")
	}
	if len(parsed.Tests[0].Cases) != 1 {
		t.Fatalf("expected 1 test case, got %d", len(parsed.Tests[0].Cases))
	}
	if parsed.Tests[0].Cases[0].Status != TestPrecluded {
		t.Errorf("case.Status = %q, want %q", parsed.Tests[0].Cases[0].Status, TestPrecluded)
	}
}

func TestFormatForRunner(t *testing.T) {
	for _, tc := range []struct {
		runner string
		want   string
	}{
		{"gtest_runner", FormatGoogleTest},
		{"gtest_create_raw_processes_runner", FormatGoogleTest},
		{"gunit_runner", FormatGoogleTest},
		{"zxtest_runner", FormatGoogleTest},
		{"rust_test_runner", FormatRust},
		{"rust_test_create_raw_processes_runner", FormatRust},
		{"rust_test_next_vdso_runner", FormatRust},
		{"go_test_runner", FormatGo},
		{"elf_test_runner", FormatFTF},
		{"elf_test_create_raw_processes_ambient_exec_runner", FormatFTF},
		{"starnix_test_runner", FormatFTF},
		{"starnix_unit_test_runner", FormatFTF},
		{"stress_test_runner", FormatFTF},
		{"fuzz_test_runner", FormatFTF},
		{"fuzz_starnix_test_runner", FormatFTF},
		{"netemul_test_runner", FormatFTF},
		{"rust_runner", FormatFTF},
		{"gtest", FormatFTF},
		{"", FormatFTF},
	} {
		if got := FormatForRunner(tc.runner); got != tc.want {
			t.Errorf("FormatForRunner(%q) = %q, want %q", tc.runner, got, tc.want)
		}
	}
}

func TestSplitCaseName(t *testing.T) {
	for _, tc := range []struct {
		format    string
		name      string
		wantSuite string
		wantCase  string
		wantOk    bool
	}{
		{FormatGoogleTest, "Suite.Case", "Suite", "Case", true},
		{FormatGoogleTest, "Inst/Suite.Case/0", "Inst/Suite", "Case/0", true},
		{FormatGoogleTest, "TypedSuite/0.Case", "TypedSuite/0", "Case", true},
		{FormatGoogleTest, "NoDot", "", "NoDot", false},
		{FormatGoogleTest, ".Case", "", ".Case", false},
		{FormatGoogleTest, "Suite.", "", "Suite.", false},
		{FormatRust, "tests::module::test_fn", "tests::module", "test_fn", true},
		{FormatRust, "test_fn", "", "test_fn", true},
		{FormatRust, "tests::", "", "tests::", false},
		{FormatGo, "TestFoo/sub.test", "", "TestFoo/sub.test", true},
		{FormatFTF, "Suite.Case", "", "Suite.Case", true},
	} {
		gotSuite, gotCase, gotOk := SplitCaseName(tc.format, tc.name)
		if gotSuite != tc.wantSuite || gotCase != tc.wantCase || gotOk != tc.wantOk {
			t.Errorf("SplitCaseName(%q, %q) = (%q, %q, %t), want (%q, %q, %t)",
				tc.format, tc.name, gotSuite, gotCase, gotOk, tc.wantSuite, tc.wantCase, tc.wantOk)
		}
	}
}

func TestCaseNamesForRunner(t *testing.T) {
	for _, tc := range []struct {
		desc       string
		runner     string
		names      []string
		wantFormat string
		wantSuites []string
		wantCases  []string
	}{
		{
			desc:       "gtest",
			runner:     "gtest_runner",
			names:      []string{"A.b", "Inst/C.d/1"},
			wantFormat: FormatGoogleTest,
			wantSuites: []string{"A", "Inst/C"},
			wantCases:  []string{"b", "d/1"},
		},
		{
			desc:       "gtest falls back to FTF if any name cannot be split",
			runner:     "gtest_runner",
			names:      []string{"A.b", "unsplittable"},
			wantFormat: FormatFTF,
			wantSuites: []string{"", ""},
			wantCases:  []string{"A.b", "unsplittable"},
		},
		{
			desc:       "rust",
			runner:     "rust_test_next_vdso_runner",
			names:      []string{"tests::a", "b"},
			wantFormat: FormatRust,
			wantSuites: []string{"tests", ""},
			wantCases:  []string{"a", "b"},
		},
		{
			desc:       "unknown runner",
			runner:     "elf_test_runner",
			names:      []string{"A.b"},
			wantFormat: FormatFTF,
			wantSuites: []string{""},
			wantCases:  []string{"A.b"},
		},
	} {
		t.Run(tc.desc, func(t *testing.T) {
			gotFormat, gotSuites, gotCases := CaseNamesForRunner(tc.runner, tc.names)
			if gotFormat != tc.wantFormat {
				t.Errorf("format = %q, want %q", gotFormat, tc.wantFormat)
			}
			if diff := cmp.Diff(tc.wantSuites, gotSuites); diff != "" {
				t.Errorf("suite names mismatch (-want +got):\n%s", diff)
			}
			if diff := cmp.Diff(tc.wantCases, gotCases); diff != "" {
				t.Errorf("case names mismatch (-want +got):\n%s", diff)
			}
		})
	}
}
