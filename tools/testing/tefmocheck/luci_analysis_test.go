// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
	analysispb "go.chromium.org/luci/analysis/proto/v1"
	"google.golang.org/protobuf/encoding/protojson"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/testing/resultdb"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func boolPtr(v bool) *bool { return &v }

func baseTestExonerationConfig(dryRun bool, runRPC func(context.Context, string, []byte) ([]byte, error)) *ExonerationConfig {
	return &ExonerationConfig{
		DryRun:             dryRun,
		MaxExoneratedTests: 5,
		Project:            "fuchsia",
		LUCIAnalysisHost:   "analysis.api.luci.app",
		Variant: map[string]string{
			"builder":          "core.x64-debug",
			"board":            "boards/x64.gni",
			"product":          "products/core.gni",
			"build_type":       "debug",
			"test_environment": "QEMU-x64",
		},
		GitilesCommit: &analysispb.GitilesCommit{
			Host:       "fuchsia.googlesource.com",
			Project:    "fuchsia",
			Ref:        "refs/heads/main",
			CommitHash: "deadbeef",
			Position:   123456,
		},
		Changelists: []*analysispb.GerritChange{
			{
				Host:     "fuchsia-review.googlesource.com",
				Project:  "fuchsia",
				Change:   1922567,
				Patchset: 1,
			},
		},
		RunRdbRPC: runRPC,
	}
}

func TestApplyStabilityExoneration(t *testing.T) {
	suiteDetail := runtests.TestDetails{
		Name: "fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm",
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{SuiteName: "SuiteA", CaseName: "Case1", Format: "GoogleTest"},
			},
		},
	}
	structuredCase1ID := structuredCaseTestID(resultdb.DetermineScheme(&suiteDetail), &suiteDetail, &suiteDetail.Cases[0])

	tests := []struct {
		name               string
		dryRun             bool
		maxExoneratedTests int
		inputTests         []runtests.TestDetails
		checkTests         []runtests.TestDetails
		respVariants       []*analysispb.TestVariantStabilityAnalysis
		wantRPCCalled      bool
		wantQueriedIDs     []string
		wantTests          []runtests.TestDetails
	}{
		{
			name:   "active mode exonerates suite when all failing cases match structured ID",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						SetupSucceeded:    boolPtr(true),
						TeardownSucceeded: boolPtr(true),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteA", CaseName: "Case1", Format: "GoogleTest", Status: runtests.TestFailure},
							{SuiteName: "SuiteA", CaseName: "Case2", Format: "GoogleTest", Status: runtests.TestSuccess},
						},
					},
				},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{
					TestId:    structuredCase1ID,
					FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true},
				},
			},
			wantRPCCalled: true,
			wantQueriedIDs: []string{
				structuredCase1ID,
				"fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm/SuiteA:Case1",
			},
			wantTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm",
					Status: runtests.TestExonerated,
					TestResult: runtests.TestResult{
						ExonerationExplanation: suiteExonerationExplanation,
						SetupSucceeded:         boolPtr(true),
						TeardownSucceeded:      boolPtr(true),
						Cases: []runtests.TestCaseResult{
							{
								SuiteName:              "SuiteA",
								CaseName:               "Case1",
								Format:                 "GoogleTest",
								Status:                 runtests.TestExonerated,
								ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met flake_rate criteria).",
								Tags:                   []build.TestTag{{Key: exonerationFlakeRateTagKey, Value: "true"}},
							},
							{SuiteName: "SuiteA", CaseName: "Case2", Format: "GoogleTest", Status: runtests.TestSuccess},
						},
					},
				},
			},
		},
		{
			name:   "active mode keeps suite FAIL when only one of two failing cases is unstable",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteB", CaseName: "FlakyCase", Format: "GoogleTest", Status: runtests.TestFailure},
							{SuiteName: "SuiteB", CaseName: "RealRegression", Format: "GoogleTest", Status: runtests.TestFailure},
						},
					},
				},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{
					TestId:      "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm/SuiteB:FlakyCase",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: true},
					FlakeRate:   &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true},
				},
				{
					TestId:      "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm/SuiteB:RealRegression",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: false},
					FlakeRate:   &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: false},
				},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{
								SuiteName:              "SuiteB",
								CaseName:               "FlakyCase",
								Format:                 "GoogleTest",
								Status:                 runtests.TestExonerated,
								ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met failure_rate and flake_rate criteria).",
								Tags: []build.TestTag{
									{Key: exonerationFailureRateTagKey, Value: "true"},
									{Key: exonerationFlakeRateTagKey, Value: "true"},
								},
							},
							{SuiteName: "SuiteB", CaseName: "RealRegression", Format: "GoogleTest", Status: runtests.TestFailure},
						},
					},
				},
			},
		},
		{
			name:   "dry-run mode preserves FAIL status and tags both exonerated (true) and non-exonerated (false) tests",
			dryRun: true,
			inputTests: []runtests.TestDetails{
				{
					Name:   "host_x64/caseless_unstable",
					Status: runtests.TestFailure,
				},
				{
					Name:   "host_x64/caseless_stable",
					Status: runtests.TestFailure,
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteA", CaseName: "Case1", Format: "GoogleTest", Status: runtests.TestFailure},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteB", CaseName: "FlakyCase", Format: "GoogleTest", Status: runtests.TestFailure},
							{SuiteName: "SuiteB", CaseName: "StableCase", Format: "GoogleTest", Status: runtests.TestFailure},
							{SuiteName: "SuiteB", CaseName: "PassingCase", Format: "GoogleTest", Status: runtests.TestSuccess},
						},
					},
				},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{
					TestId:      "host_x64/caseless_unstable",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: true},
				},
				{
					TestId:      "host_x64/caseless_stable",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: false},
					FlakeRate:   &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: false},
				},
				{
					TestId:    structuredCase1ID,
					FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true},
				},
				{
					TestId:      "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm/SuiteB:FlakyCase",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: true},
				},
				{
					TestId:      "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm/SuiteB:StableCase",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: false},
				},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{
					Name:   "host_x64/caseless_unstable",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "true"},
						{Key: exonerationFailureRateTagKey, Value: "true"},
					},
				},
				{
					Name:   "host_x64/caseless_stable",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "false"},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/suite.cm",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "true"},
					},
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{
								SuiteName: "SuiteA",
								CaseName:  "Case1",
								Format:    "GoogleTest",
								Status:    runtests.TestFailure,
								Tags: []build.TestTag{
									{Key: exonerationDryRunTagKey, Value: "true"},
									{Key: exonerationFlakeRateTagKey, Value: "true"},
								},
							},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/partial.cm",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "false"},
					},
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{
								SuiteName: "SuiteB",
								CaseName:  "FlakyCase",
								Format:    "GoogleTest",
								Status:    runtests.TestFailure,
								Tags: []build.TestTag{
									{Key: exonerationDryRunTagKey, Value: "true"},
									{Key: exonerationFailureRateTagKey, Value: "true"},
								},
							},
							{
								SuiteName: "SuiteB",
								CaseName:  "StableCase",
								Format:    "GoogleTest",
								Status:    runtests.TestFailure,
								Tags: []build.TestTag{
									{Key: exonerationDryRunTagKey, Value: "false"},
								},
							},
							{SuiteName: "SuiteB", CaseName: "PassingCase", Format: "GoogleTest", Status: runtests.TestSuccess},
						},
					},
				},
			},
		},
		{
			name:               "active mode skips RPC and exoneration when failed targets exceed MaxExoneratedTests",
			dryRun:             false,
			maxExoneratedTests: 1,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/flake_1", Status: runtests.TestFailure},
				{Name: "host_x64/flake_2", Status: runtests.TestFailure},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{Name: "host_x64/flake_1", Status: runtests.TestFailure},
				{Name: "host_x64/flake_2", Status: runtests.TestFailure},
			},
		},
		{
			name:               "active mode exonerates unstable targets within MaxExoneratedTests even when another failure is stable",
			dryRun:             false,
			maxExoneratedTests: 2,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/flake_1", Status: runtests.TestFailure},
				{Name: "host_x64/real_regression", Status: runtests.TestFailure},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{TestId: "host_x64/flake_1", FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true}},
				{TestId: "host_x64/real_regression", FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: false}},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{
					Name:   "host_x64/flake_1",
					Status: runtests.TestExonerated,
					TestResult: runtests.TestResult{
						ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met flake_rate criteria).",
					},
					Tags: []build.TestTag{{Key: exonerationFlakeRateTagKey, Value: "true"}},
				},
				{Name: "host_x64/real_regression", Status: runtests.TestFailure},
			},
		},
		{
			name:               "dry-run mode still tags all evaluated failures when matched targets exceed MaxExoneratedTests",
			dryRun:             true,
			maxExoneratedTests: 1,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/flake_1", Status: runtests.TestFailure},
				{Name: "host_x64/flake_2", Status: runtests.TestFailure},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{TestId: "host_x64/flake_1", FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true}},
				{TestId: "host_x64/flake_2", FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true}},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{
					Name:   "host_x64/flake_1",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "true"},
						{Key: exonerationFlakeRateTagKey, Value: "true"},
					},
				},
				{
					Name:   "host_x64/flake_2",
					Status: runtests.TestFailure,
					Tags: []build.TestTag{
						{Key: exonerationDryRunTagKey, Value: "true"},
						{Key: exonerationFlakeRateTagKey, Value: "true"},
					},
				},
			},
		},
		{
			name:   "multiplied test with passes followed by failure is never exonerated or queried",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestSuccess,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteM", CaseName: "CaseM", Status: runtests.TestSuccess},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestSuccess,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{
								SuiteName: "SuiteM",
								CaseName:  "CaseM",
								Status:    runtests.TestSuccess,
							},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteM", CaseName: "CaseM", Status: runtests.TestFailure},
						},
					},
				},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestSuccess,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteM", CaseName: "CaseM", Status: runtests.TestSuccess},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestSuccess,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{
								SuiteName: "SuiteM",
								CaseName:  "CaseM",
								Status:    runtests.TestSuccess,
							},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/multiplied.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						Cases: []runtests.TestCaseResult{
							{SuiteName: "SuiteM", CaseName: "CaseM", Status: runtests.TestFailure},
						},
					},
				},
			},
		},
		{
			name:   "in-task flake (FAIL followed by PASS) and INFRA_FAIL are skipped without RPC",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/in_task_flake", Status: runtests.TestFailure},
				{Name: "host_x64/in_task_flake", Status: runtests.TestSuccess},
				{Name: "host_x64/infra_broken", Status: runtests.TestInfraFailure},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{Name: "host_x64/in_task_flake", Status: runtests.TestFailure},
				{Name: "host_x64/in_task_flake", Status: runtests.TestSuccess},
				{Name: "host_x64/infra_broken", Status: runtests.TestInfraFailure},
			},
		},
		{
			name:   "failed setup or teardown skips RPC and leaves suite and caseless tests FAIL",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(false),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
				{
					Name:   "host_x64/caseless_setup_fail",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						SetupSucceeded: boolPtr(false),
					},
				},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(false),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
				{
					Name:   "host_x64/caseless_setup_fail",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						SetupSucceeded: boolPtr(false),
					},
				},
			},
		},
		{
			name:   "multi-attempt target is excluded when any attempt has INFRA_FAIL or failed setup/teardown",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/fail_then_infra", Status: runtests.TestFailure},
				{Name: "host_x64/fail_then_infra", Status: runtests.TestInfraFailure},
				{Name: "host_x64/infra_then_fail", Status: runtests.TestInfraFailure},
				{Name: "host_x64/infra_then_fail", Status: runtests.TestFailure},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/retry_teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(true),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/retry_teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(false),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{Name: "host_x64/fail_then_infra", Status: runtests.TestFailure},
				{Name: "host_x64/fail_then_infra", Status: runtests.TestInfraFailure},
				{Name: "host_x64/infra_then_fail", Status: runtests.TestInfraFailure},
				{Name: "host_x64/infra_then_fail", Status: runtests.TestFailure},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/retry_teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(true),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
				{
					Name:   "fuchsia-pkg://fuchsia.com/pkg#meta/retry_teardown_fail.cm",
					Status: runtests.TestFailure,
					TestResult: runtests.TestResult{
						TeardownSucceeded: boolPtr(false),
						Cases: []runtests.TestCaseResult{
							{SuiteName: "S", CaseName: "C", Status: runtests.TestFailure},
						},
					},
				},
			},
		},
		{
			name:   "SKIP, EXONERATED, and IsTestingFailureMode entries do not exclude failing attempts",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{Name: "testing_failure_mode/synthetic_check", IsTestingFailureMode: true, Status: runtests.TestSuccess},
				{Name: "testing_failure_mode/synthetic_fail", IsTestingFailureMode: true, Status: runtests.TestFailure},
				{Name: "host_x64/skip_then_fail", Status: runtests.TestSkipped},
				{Name: "host_x64/skip_then_fail", Status: runtests.TestFailure},
				{Name: "host_x64/exonerated_then_fail", Status: runtests.TestExonerated},
				{Name: "host_x64/exonerated_then_fail", Status: runtests.TestFailure},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{TestId: "host_x64/skip_then_fail", FlakeRate: &analysispb.TestVariantStabilityAnalysis_FlakeRate{IsMet: true}},
				{TestId: "host_x64/exonerated_then_fail", FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: true}},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{Name: "testing_failure_mode/synthetic_check", IsTestingFailureMode: true, Status: runtests.TestSuccess},
				{Name: "testing_failure_mode/synthetic_fail", IsTestingFailureMode: true, Status: runtests.TestFailure},
				{Name: "host_x64/skip_then_fail", Status: runtests.TestSkipped},
				{
					Name:   "host_x64/skip_then_fail",
					Status: runtests.TestExonerated,
					TestResult: runtests.TestResult{
						ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met flake_rate criteria).",
					},
					Tags: []build.TestTag{{Key: exonerationFlakeRateTagKey, Value: "true"}},
				},
				{Name: "host_x64/exonerated_then_fail", Status: runtests.TestExonerated},
				{
					Name:   "host_x64/exonerated_then_fail",
					Status: runtests.TestExonerated,
					TestResult: runtests.TestResult{
						ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met failure_rate criteria).",
					},
					Tags: []build.TestTag{{Key: exonerationFailureRateTagKey, Value: "true"}},
				},
			},
		},
		{
			name:   "blocking checkTests skips exoneration while flake checkTests allows it",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/unstable", Status: runtests.TestFailure},
			},
			checkTests: []runtests.TestDetails{
				{Name: "testing_failure_mode/more_than_5_tests_failed", IsTestingFailureMode: true, Status: runtests.TestFailure},
			},
			wantRPCCalled: false,
			wantTests: []runtests.TestDetails{
				{Name: "host_x64/unstable", Status: runtests.TestFailure},
			},
		},
		{
			name:   "flake checkTests (FAIL + companion PASS) allows exoneration",
			dryRun: false,
			inputTests: []runtests.TestDetails{
				{Name: "host_x64/unstable", Status: runtests.TestFailure},
			},
			checkTests: []runtests.TestDetails{
				{Name: "testing_failure_mode/flaky_log_check", IsTestingFailureMode: true, Status: runtests.TestFailure},
				{Name: "testing_failure_mode/flaky_log_check", IsTestingFailureMode: true, Status: runtests.TestSuccess},
			},
			respVariants: []*analysispb.TestVariantStabilityAnalysis{
				{
					TestId:      "host_x64/unstable",
					FailureRate: &analysispb.TestVariantStabilityAnalysis_FailureRate{IsMet: true},
				},
			},
			wantRPCCalled: true,
			wantTests: []runtests.TestDetails{
				{
					Name:   "host_x64/unstable",
					Status: runtests.TestExonerated,
					TestResult: runtests.TestResult{
						ExonerationExplanation: "Exonerated by LUCI Analysis stability check (met failure_rate criteria).",
					},
					Tags: []build.TestTag{{Key: exonerationFailureRateTagKey, Value: "true"}},
				},
			},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			rpcCalled := false
			var gotQueriedIDs []string
			runRPC := func(_ context.Context, _ string, reqBytes []byte) ([]byte, error) {
				rpcCalled = true
				var req analysispb.QueryTestVariantStabilityRequest
				if err := protojson.Unmarshal(reqBytes, &req); err != nil {
					t.Fatalf("protojson.Unmarshal request: %v", err)
				}
				for _, tv := range req.GetTestVariants() {
					gotQueriedIDs = append(gotQueriedIDs, tv.GetTestId())
				}
				return protojson.Marshal(&analysispb.QueryTestVariantStabilityResponse{
					TestVariants: tc.respVariants,
				})
			}

			summary := &runtests.TestSummary{Tests: tc.inputTests}
			cfg := baseTestExonerationConfig(tc.dryRun, runRPC)
			if tc.maxExoneratedTests > 0 {
				cfg.MaxExoneratedTests = tc.maxExoneratedTests
			}
			if err := ApplyStabilityExoneration(context.Background(), cfg, summary, tc.checkTests); err != nil {
				t.Fatalf("ApplyStabilityExoneration returned unexpected error: %v", err)
			}
			if rpcCalled != tc.wantRPCCalled {
				t.Fatalf("rpcCalled = %v, want %v", rpcCalled, tc.wantRPCCalled)
			}
			if tc.wantQueriedIDs != nil {
				sortStrings := cmpopts.SortSlices(func(a, b string) bool { return a < b })
				if diff := cmp.Diff(tc.wantQueriedIDs, gotQueriedIDs, sortStrings); diff != "" {
					t.Errorf("queried test IDs mismatch (-want +got):\n%s", diff)
				}
			}
			if diff := cmp.Diff(tc.wantTests, summary.Tests); diff != "" {
				t.Errorf("summary.Tests mismatch (-want +got):\n%s", diff)
			}
		})
	}
}

func TestLoadExonerationConfigAndDefaultRdbRPC(t *testing.T) {
	cfgDir := t.TempDir()
	cfgPath := filepath.Join(cfgDir, "exoneration.json")
	cfgJSON := `{
		"dry_run": true,
		"max_exonerated_tests": 5,
		"project": "turquoise",
		"luci_analysis_host": "127.0.0.1:8080",
		"variant": {"builder": "sorrel-debug"},
		"gitiles_commit": {
			"host": "turquoise-internal.googlesource.com",
			"project": "integration",
			"ref": "refs/heads/main",
			"commit_hash": "abc123",
			"position": 42
		},
		"changelists": [
			{
				"host": "turquoise-internal-review.googlesource.com",
				"project": "vendor/google",
				"change": 999,
				"patchset": 3
			}
		]
	}`
	if err := os.WriteFile(cfgPath, []byte(cfgJSON), 0o600); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}

	loaded, err := LoadExonerationConfig(cfgPath)
	if err != nil {
		t.Fatalf("LoadExonerationConfig: %v", err)
	}
	if !loaded.DryRun || loaded.MaxExoneratedTests != 5 || loaded.Project != "turquoise" || loaded.GitilesCommit.GetPosition() != 42 || len(loaded.Changelists) != 1 {
		t.Fatalf("unexpected loaded config: %+v", loaded)
	}

	// Negative max_exonerated_tests or missing luci_analysis_host should be rejected on load.
	negPath := filepath.Join(cfgDir, "neg.json")
	if err := os.WriteFile(negPath, []byte(`{"project":"fuchsia","luci_analysis_host":"analysis.api.luci.app","max_exonerated_tests":-1}`), 0o600); err != nil {
		t.Fatalf("WriteFile neg: %v", err)
	}
	if _, err := LoadExonerationConfig(negPath); err == nil {
		t.Errorf("expected error for negative max_exonerated_tests, got nil")
	}
	noHostPath := filepath.Join(cfgDir, "no_host.json")
	if err := os.WriteFile(noHostPath, []byte(`{"project":"fuchsia"}`), 0o600); err != nil {
		t.Fatalf("WriteFile no_host: %v", err)
	}
	if _, err := LoadExonerationConfig(noHostPath); err == nil {
		t.Errorf("expected error for missing luci_analysis_host, got nil")
	}

	// Exercise defaultRunRdbRPC via a fake rdb script on PATH.
	binDir := t.TempDir()
	argsFile := filepath.Join(binDir, "rdb_args.txt")
	fakeRdb := filepath.Join(binDir, "rdb")
	script := fmt.Sprintf("#!/bin/sh\nprintf '%%s\\n' \"$@\" > %q\ncat >/dev/null\nprintf '{\"testVariants\":[{\"testId\":\"failing_test\",\"failureRate\":{\"isMet\":true}}]}'\n", argsFile)
	if err := os.WriteFile(fakeRdb, []byte(script), 0o755); err != nil {
		t.Fatalf("WriteFile fake rdb: %v", err)
	}
	t.Setenv("PATH", binDir+string(os.PathListSeparator)+os.Getenv("PATH"))

	summary := &runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{Name: "failing_test", Status: runtests.TestFailure},
		},
	}
	loaded.DryRun = false
	if err := ApplyStabilityExoneration(context.Background(), loaded, summary, nil); err != nil {
		t.Fatalf("ApplyStabilityExoneration with fake rdb: %v", err)
	}
	if got := summary.Tests[0].Status; got != runtests.TestExonerated {
		t.Errorf("failing_test Status = %v, want EXONERATED", got)
	}
	if got := summary.Tests[0].ExonerationExplanation; got == "" {
		t.Errorf("failing_test ExonerationExplanation is empty, want non-empty")
	}
	gotArgsBytes, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("ReadFile args: %v", err)
	}
	wantArgs := []string{"rpc", "-host", "127.0.0.1:8080", "luci.analysis.v1.TestVariants", "QueryStability"}
	if diff := cmp.Diff(wantArgs, strings.Split(strings.TrimSpace(string(gotArgsBytes)), "\n")); diff != "" {
		t.Errorf("rdb args mismatch (-want +got):\n%s", diff)
	}

	// Verify fail-open when rdb exits non-zero.
	failScript := "#!/bin/sh\necho 'rpc unavailable' >&2\nexit 1\n"
	if err := os.WriteFile(fakeRdb, []byte(failScript), 0o755); err != nil {
		t.Fatalf("WriteFile failing rdb: %v", err)
	}
	failSummary := &runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{Name: "still_failing", Status: runtests.TestFailure},
		},
	}
	if err := ApplyStabilityExoneration(context.Background(), loaded, failSummary, nil); err == nil {
		t.Errorf("expected error when rdb exits non-zero, got nil")
	}
	if got := failSummary.Tests[0].Status; got != runtests.TestFailure {
		t.Errorf("still_failing Status = %v after RPC failure, want FAIL", got)
	}

	// Verify fail-open error (no panic) if ApplyStabilityExoneration is called directly with an empty host.
	emptyHostCfg := &ExonerationConfig{Project: "fuchsia"}
	if err := ApplyStabilityExoneration(context.Background(), emptyHostCfg, failSummary, nil); err == nil {
		t.Errorf("expected error when LUCIAnalysisHost is empty, got nil")
	}
	if got := failSummary.Tests[0].Status; got != runtests.TestFailure {
		t.Errorf("still_failing Status = %v after empty-host error, want FAIL", got)
	}
}

func TestStructuredAndLegacyTestIDs(t *testing.T) {
	if got, want := resultdb.LegacyTestCaseID("pkg.cm", "Suite", "Case"), "pkg.cm/Suite:Case"; got != want {
		t.Errorf("LegacyTestCaseID = %q, want %q", got, want)
	}

	caseless := &runtests.TestDetails{Name: "host_x64/my_test"}
	if got, want := structuredDetailTestID(caseless), ":host_x64/my_test!single::#*fixture"; got != want {
		t.Errorf("structuredDetailTestID = %q, want %q", got, want)
	}

	gtestDetail := &runtests.TestDetails{
		Name: "fuchsia-pkg://fuchsia.com/pkg#meta/my_test.cm",
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{SuiteName: "SuiteA", CaseName: "!Reserved:Name", Format: "GoogleTest"},
			},
		},
	}
	wantGTestID := `:fuchsia-pkg\://fuchsia.com/pkg\#meta/my_test.cm!gtest::SuiteA#[\!Reserved\:Name]`
	if got := structuredCaseTestID(resultdb.DetermineScheme(gtestDetail), gtestDetail, &gtestDetail.Cases[0]); got != wantGTestID {
		t.Errorf("structuredCaseTestID(gtest) = %q, want %q", got, wantGTestID)
	}
}
