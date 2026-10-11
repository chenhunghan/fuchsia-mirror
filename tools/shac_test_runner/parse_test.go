// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func TestParseResults(t *testing.T) {
	data := []byte(`[
  {
    "name": "test_pass",
    "file": "scripts/shac/tests/foo_test.star",
    "duration_nanos": 1500000,
    "status": "PASS",
    "prints": ["hello"]
  },
  {
    "name": "test_fail",
    "file": "scripts/shac/tests/foo_test.star",
    "duration_nanos": 2000000,
    "status": "FAIL",
    "error": "asserts.eq: 1 != 2",
    "backtrace": "Traceback (most recent call last):\n  foo_test.star:10:5: in test_fail\n"
  },
  {
    "name": "test_fail_no_error",
    "file": "scripts/shac/tests/foo_test.star",
    "duration_nanos": 0,
    "status": "FAIL"
  }
]`)
	got, err := parseResults(data)
	if err != nil {
		t.Fatalf("parseResults() failed: %s", err)
	}
	want := []runtests.TestCaseResult{
		{
			DisplayName: "test_pass",
			SuiteName:   "shac",
			CaseName:    "test_pass",
			Status:      runtests.TestSuccess,
			Duration:    1500 * time.Microsecond,
			Format:      "shac",
		},
		{
			DisplayName: "test_fail",
			SuiteName:   "shac",
			CaseName:    "test_fail",
			Status:      runtests.TestFailure,
			Duration:    2 * time.Millisecond,
			Format:      "shac",
			FailureReason: &runtests.FailureReason{
				Errors: []*runtests.FailureReasonError{
					{
						Message: "asserts.eq: 1 != 2",
						Trace:   "Traceback (most recent call last):\n  foo_test.star:10:5: in test_fail",
					},
				},
			},
		},
		{
			DisplayName: "test_fail_no_error",
			SuiteName:   "shac",
			CaseName:    "test_fail_no_error",
			Status:      runtests.TestFailure,
			Format:      "shac",
		},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("parseResults() diff (-want +got):\n%s", diff)
	}
}

func TestParseResults_Empty(t *testing.T) {
	got, err := parseResults([]byte("[]"))
	if err != nil {
		t.Fatalf("parseResults(%q) failed: %s", "[]", err)
	}
	// cmp.Diff distinguishes nil from an empty slice, which matters because
	// a nil slice would be serialized as null rather than [].
	if diff := cmp.Diff([]runtests.TestCaseResult{}, got); diff != "" {
		t.Errorf("parseResults(%q) diff (-want +got):\n%s", "[]", diff)
	}
}

func TestParseResults_Errors(t *testing.T) {
	tests := []struct {
		name string
		data string
	}{
		{
			name: "invalid_json",
			data: "{",
		},
		{
			name: "multiple_files",
			data: `[
  {"name": "test_a", "file": "a_test.star", "status": "PASS"},
  {"name": "test_a", "file": "b_test.star", "status": "PASS"}
]`,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got, err := parseResults([]byte(tc.data)); err == nil {
				t.Errorf("parseResults(%q) = %v, want error", tc.data, got)
			}
		})
	}
}
