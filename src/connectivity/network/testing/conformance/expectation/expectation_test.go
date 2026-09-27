// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package expectation

import (
	"os"
	"testing"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/expectation/outcome"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/parseoutput"
)

func TestGetExpectation(t *testing.T) {
	const (
		SuiteName string = "IP"
		// The choice to use IP-5.6 is arbitrary.
		MajorNumber int = 5
		MinorNumber int = 6
	)
	result, ok := GetExpectation(parseoutput.CaseIdentifier{SuiteName: SuiteName, MajorNumber: MajorNumber, MinorNumber: MinorNumber})
	if !ok {
		t.Fatalf("expectation missing for %s %d.%d", SuiteName, MajorNumber, MinorNumber)
	}
	if result != outcome.Fail {
		t.Errorf("wrong expectation for %s %d.%d: got = %s, want = %s", SuiteName, MajorNumber, MinorNumber, result, outcome.Fail)
	}
}

func TestAnvlDefaultExpectationPassEnvVar(t *testing.T) {
	const (
		SuiteName string = "IP"
		// Case 99.99 doesn't exist so there is no expectation for it.
		MajorNumber int = 99
		MinorNumber int = 99
	)
	if result, ok := GetExpectation(parseoutput.CaseIdentifier{SuiteName: SuiteName, MajorNumber: MajorNumber, MinorNumber: MinorNumber}); ok {
		t.Fatalf("expectation should be missing for %s %d.%d: got = %s", SuiteName, MajorNumber, MinorNumber, result)
	}

	os.Setenv("ANVL_DEFAULT_EXPECTATION_PASS", "true")
	defer os.Unsetenv("ANVL_DEFAULT_EXPECTATION_PASS")

	result, ok := GetExpectation(parseoutput.CaseIdentifier{SuiteName: SuiteName, MajorNumber: MajorNumber, MinorNumber: MinorNumber})
	if !ok {
		t.Fatalf("expectation missing after defaulting to pass for %s %d.%d", SuiteName, MajorNumber, MinorNumber)
	}
	if result != outcome.Pass {
		t.Errorf("wrong expectation for %s %d.%d: got = %s, want = %s", SuiteName, MajorNumber, MinorNumber, result, outcome.Pass)
	}
}
