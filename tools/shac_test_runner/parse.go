// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

// shacTestCaseResult mirrors the schema of each element of the JSON list
// written by `shac test --json-output`.
type shacTestCaseResult struct {
	Name string `json:"name"`
	File string `json:"file"`
	// shac writes durations as integer nanoseconds, which is time.Duration's
	// underlying representation.
	Duration  time.Duration `json:"duration_nanos"`
	Status    string        `json:"status"`
	Error     string        `json:"error,omitempty"`
	Backtrace string        `json:"backtrace,omitempty"`
	Prints    []string      `json:"prints,omitempty"`
}

// suiteName is the suite name reported for every test case. Each
// host_shac_test() target runs a single test file and ResultDB test IDs
// already start with the target's label, so including the file name in the
// suite name would only repeat it.
const suiteName = "shac"

// parseResults converts the contents of a `shac test --json-output` file into
// test case results. All the results must come from the same test file.
func parseResults(data []byte) ([]runtests.TestCaseResult, error) {
	var results []shacTestCaseResult
	if err := json.Unmarshal(data, &results); err != nil {
		return nil, fmt.Errorf("failed to parse shac test results: %w", err)
	}
	// Non-nil so that a run with no results is serialized as an empty list
	// rather than null.
	cases := []runtests.TestCaseResult{}
	for _, r := range results {
		// Test function names are only unique within a file, so results from
		// multiple files could have colliding test IDs.
		if r.File != results[0].File {
			return nil, fmt.Errorf("got results from multiple test files (%q, %q), want exactly one", results[0].File, r.File)
		}
		status := runtests.TestSuccess
		var failureReason *runtests.FailureReason
		if r.Status != "PASS" {
			status = runtests.TestFailure
			if r.Error != "" {
				failureReason = &runtests.FailureReason{
					Errors: []*runtests.FailureReasonError{
						{
							Message: r.Error,
							Trace:   strings.TrimSpace(r.Backtrace),
						},
					},
				}
			}
		}
		cases = append(cases, runtests.TestCaseResult{
			DisplayName:   r.Name,
			SuiteName:     suiteName,
			CaseName:      r.Name,
			Status:        status,
			Duration:      r.Duration,
			Format:        "shac",
			FailureReason: failureReason,
		})
	}
	return cases, nil
}
