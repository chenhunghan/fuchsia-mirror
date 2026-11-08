// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"bytes"
	"cmp"
	"context"
	"fmt"
	"os/exec"
	"strings"
	"time"

	analysispb "go.chromium.org/luci/analysis/proto/v1"
	"go.chromium.org/luci/resultdb/pbutil"
	"google.golang.org/protobuf/encoding/protojson"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
	"go.fuchsia.dev/fuchsia/tools/testing/resultdb"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const (
	// maxStabilityRequestBytes guards against piping pathologically large
	// requests into `rdb rpc`.
	maxStabilityRequestBytes = 1 << 20 // 1 MiB

	defaultRPCTimeout = 15 * time.Second

	exonerationDryRunTagKey      = "luci_exoneration_v2_dry_run"
	exonerationFailureRateTagKey = "luci_exoneration_v2_failure_rate"
	exonerationFlakeRateTagKey   = "luci_exoneration_v2_flake_rate"

	suiteExonerationExplanation = "Exonerated by LUCI Analysis stability check (all failing test cases were exonerated)."
)

// ExonerationConfig configures LUCI Analysis QueryStability evaluation.
type ExonerationConfig struct {
	DryRun             bool                       `json:"dry_run,omitempty"`
	MaxExoneratedTests int                        `json:"max_exonerated_tests,omitempty"`
	Project            string                     `json:"project"`
	LUCIAnalysisHost   string                     `json:"luci_analysis_host"`
	Variant            map[string]string          `json:"variant,omitempty"`
	GitilesCommit      *analysispb.GitilesCommit  `json:"gitiles_commit,omitempty"`
	Changelists        []*analysispb.GerritChange `json:"changelists,omitempty"`

	// RunRdbRPC allows unit tests to stub out the `rdb rpc` subprocess.
	RunRdbRPC func(ctx context.Context, host string, reqBytes []byte) ([]byte, error) `json:"-"`
}

// LoadExonerationConfig reads an ExonerationConfig JSON file.
func LoadExonerationConfig(path string) (*ExonerationConfig, error) {
	var cfg ExonerationConfig
	if err := jsonutil.ReadFromFile(path, &cfg); err != nil {
		return nil, fmt.Errorf("reading exoneration config: %w", err)
	}
	if cfg.LUCIAnalysisHost == "" {
		return nil, fmt.Errorf("luci_analysis_host is required")
	}
	if cfg.MaxExoneratedTests < 0 {
		return nil, fmt.Errorf("max_exonerated_tests must be non-negative, got %d", cfg.MaxExoneratedTests)
	}
	return &cfg, nil
}

// ApplyStabilityExoneration queries LUCI Analysis QueryStability for failing
// tests in summary and updates summary in place (attaching dry-run tags when
// cfg.DryRun is true, or marking unstable failures EXONERATED when false).
func ApplyStabilityExoneration(ctx context.Context, cfg *ExonerationConfig, summary *runtests.TestSummary, checkTests []runtests.TestDetails) error {
	if cfg == nil || summary == nil || len(summary.Tests) == 0 {
		return nil
	}
	// TODO(b/575894983): Revisit including tefmocheck checks in stability
	// exoneration once the tefmocheck ResultDB migration finishes.
	if hasBlockingCheckFailure(checkTests) {
		return nil
	}

	failedTargets, testIDs := collectFailedTargetsAndTestIDs(summary.Tests)
	if len(testIDs) == 0 {
		return nil
	}
	if !cfg.DryRun && cfg.MaxExoneratedTests > 0 && len(failedTargets) > cfg.MaxExoneratedTests {
		return nil
	}

	rpcCtx, cancelRPC := context.WithTimeout(ctx, defaultRPCTimeout)
	defer cancelRPC()
	unstable, err := queryStability(rpcCtx, cfg, testIDs)
	if err != nil {
		return err
	}

	applyStabilityVerdicts(cfg.DryRun, summary.Tests, failedTargets, unstable)
	return nil
}

// hasBlockingCheckFailure reports whether RunChecks emitted a shard-level
// failure mode without a companion PASS flake entry.
func hasBlockingCheckFailure(checkTests []runtests.TestDetails) bool {
	if len(checkTests) == 0 {
		return false
	}
	passed := make(map[string]bool)
	failed := make(map[string]bool)
	for _, ct := range checkTests {
		if ct.Status == runtests.TestSuccess {
			passed[ct.Name] = true
		} else if runtests.IsFailure(ct.Status) && ct.Status != runtests.TestExonerated {
			failed[ct.Name] = true
		}
	}
	for name := range failed {
		if !passed[name] {
			return true
		}
	}
	return false
}

// isExonerableStatus excludes INFRA_FAIL (bot/harness failures surfaced as
// purple builds) and already-exonerated results.
func isExonerableStatus(status runtests.TestStatus) bool {
	return runtests.IsFailure(status) && status != runtests.TestExonerated && status != runtests.TestInfraFailure
}

// shouldExcludeTarget reports entries that disqualify all runs of t.Name from
// exoneration (synthetic tefmocheck entries, recovered retries/multiplied
// passes, infra failures, and suite setup/teardown crashes).
// See TODO(b/575894983) in ApplyStabilityExoneration regarding tefmocheck entries.
func shouldExcludeTarget(t *runtests.TestDetails) bool {
	return t.IsTestingFailureMode ||
		t.Status == runtests.TestSuccess ||
		t.Status == runtests.TestInfraFailure ||
		(t.SetupSucceeded != nil && !*t.SetupSucceeded) ||
		(t.TeardownSucceeded != nil && !*t.TeardownSucceeded)
}

// testIsInScope excludes non-exonerable statuses and targets disqualified by
// shouldExcludeTarget.
func testIsInScope(t *runtests.TestDetails) bool {
	return t.Status != runtests.TestSkipped &&
		t.Status != runtests.TestExonerated &&
		!shouldExcludeTarget(t)
}

func caseIsInScope(tc *runtests.TestCaseResult) bool {
	return tc.SuiteName != "tefmocheck" && isExonerableStatus(tc.Status)
}

func encodeStructuredTestID(moduleName, scheme, fineName, caseName string) string {
	return pbutil.EncodeTestID(pbutil.BaseTestIdentifier{
		ModuleName:   moduleName,
		ModuleScheme: scheme,
		FineName:     fineName,
		CaseName:     pbutil.EncodeCaseName(caseName),
	})
}

func structuredDetailTestID(t *runtests.TestDetails) string {
	scheme := resultdb.DetermineScheme(t)
	fineName, caseName := resultdb.StructuredDetailIdentifier(scheme)
	return encodeStructuredTestID(t.Name, scheme, fineName, caseName)
}

func structuredCaseTestID(scheme string, t *runtests.TestDetails, tc *runtests.TestCaseResult) string {
	fineName, caseName := resultdb.StructuredCaseIdentifier(scheme, t, tc)
	return encodeStructuredTestID(t.Name, scheme, fineName, caseName)
}

// collectFailedTargetsAndTestIDs returns the in-scope failing test targets and
// their deduplicated ResultDB test IDs. Targets with any run matching
// shouldExcludeTarget are excluded across all attempts.
func collectFailedTargetsAndTestIDs(tests []runtests.TestDetails) (map[string]bool, []string) {
	excludedTargets := make(map[string]bool)
	idsByTarget := make(map[string][]string)

	addValidID := func(target, id string) {
		if id != "" && pbutil.ValidateTestID(id, pbutil.DefaultTestIDLimitCallback) == nil {
			idsByTarget[target] = append(idsByTarget[target], id)
		}
	}

	for i := range tests {
		t := &tests[i]
		if shouldExcludeTarget(t) {
			excludedTargets[t.Name] = true
			continue
		}
		if excludedTargets[t.Name] || !testIsInScope(t) {
			continue
		}
		// TODO(b/527958872): Drop legacy flat IDs (t.Name and LegacyTestCaseID)
		// once structured test IDs are fully rolled out in ResultDB. Sending
		// both IDs caps QueryStability's 100-variant per-RPC limit at 50
		// failing test cases per shard; dropping legacy IDs raises it to 100.
		if len(t.Cases) == 0 {
			addValidID(t.Name, structuredDetailTestID(t))
			addValidID(t.Name, t.Name)
			continue
		}
		scheme := resultdb.DetermineScheme(t)
		for j := range t.Cases {
			tc := &t.Cases[j]
			if caseIsInScope(tc) {
				addValidID(t.Name, structuredCaseTestID(scheme, t, tc))
				addValidID(t.Name, resultdb.LegacyTestCaseID(t.Name, tc.SuiteName, tc.CaseName))
			}
		}
	}

	failedTargets := make(map[string]bool)
	var testIDs []string
	seenIDs := make(map[string]bool)
	for name, ids := range idsByTarget {
		if excludedTargets[name] {
			continue
		}
		failedTargets[name] = true
		for _, id := range ids {
			if !seenIDs[id] {
				seenIDs[id] = true
				testIDs = append(testIDs, id)
			}
		}
	}
	return failedTargets, testIDs
}

func defaultRunRdbRPC(ctx context.Context, host string, reqBytes []byte) ([]byte, error) {
	if host == "" {
		return nil, fmt.Errorf("luci_analysis_host is empty")
	}
	cmd := exec.CommandContext(ctx, "rdb", "rpc", "-host", host, "luci.analysis.v1.TestVariants", "QueryStability")
	cmd.Stdin = bytes.NewReader(reqBytes)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("running rdb rpc: %w (stderr: %s)", err, strings.TrimSpace(stderr.String()))
	}
	return stdout.Bytes(), nil
}

func queryStability(ctx context.Context, cfg *ExonerationConfig, testIDs []string) (map[string]*analysispb.TestVariantStabilityAnalysis, error) {
	sources := &analysispb.Sources{
		Changelists: cfg.Changelists,
	}
	if cfg.GitilesCommit != nil {
		sources.BaseSources = &analysispb.Sources_GitilesCommit{
			GitilesCommit: cfg.GitilesCommit,
		}
	}

	var variant *analysispb.Variant
	if len(cfg.Variant) > 0 {
		variant = &analysispb.Variant{Def: cfg.Variant}
	}

	positions := make([]*analysispb.QueryTestVariantStabilityRequest_TestVariantPosition, len(testIDs))
	for i, id := range testIDs {
		positions[i] = &analysispb.QueryTestVariantStabilityRequest_TestVariantPosition{
			TestId:  id,
			Variant: variant,
			Sources: sources,
		}
	}

	reqBytes, err := protojson.Marshal(&analysispb.QueryTestVariantStabilityRequest{
		Project:      cfg.Project,
		TestVariants: positions,
	})
	if err != nil {
		return nil, fmt.Errorf("marshaling QueryStability request: %w", err)
	}
	if len(reqBytes) > maxStabilityRequestBytes {
		return nil, fmt.Errorf("QueryStability request size %d bytes exceeds %d byte limit", len(reqBytes), maxStabilityRequestBytes)
	}

	runRPC := cfg.RunRdbRPC
	if runRPC == nil {
		runRPC = defaultRunRdbRPC
	}
	respBytes, err := runRPC(ctx, cfg.LUCIAnalysisHost, reqBytes)
	if err != nil {
		return nil, err
	}

	var parsed analysispb.QueryTestVariantStabilityResponse
	if err := (protojson.UnmarshalOptions{DiscardUnknown: true}).Unmarshal(respBytes, &parsed); err != nil {
		return nil, fmt.Errorf("unmarshaling QueryStability response: %w", err)
	}

	unstable := make(map[string]*analysispb.TestVariantStabilityAnalysis)
	for _, tv := range parsed.GetTestVariants() {
		if tv.GetFailureRate().GetIsMet() || tv.GetFlakeRate().GetIsMet() {
			unstable[tv.GetTestId()] = tv
		}
	}
	return unstable, nil
}

func exonerationExplanation(tv *analysispb.TestVariantStabilityAnalysis) string {
	failureMet := tv.GetFailureRate().GetIsMet()
	flakeMet := tv.GetFlakeRate().GetIsMet()
	switch {
	case failureMet && flakeMet:
		return "Exonerated by LUCI Analysis stability check (met failure_rate and flake_rate criteria)."
	case failureMet:
		return "Exonerated by LUCI Analysis stability check (met failure_rate criteria)."
	case flakeMet:
		return "Exonerated by LUCI Analysis stability check (met flake_rate criteria)."
	default:
		return ""
	}
}

func applyVerdict(dryRun bool, status *runtests.TestStatus, explanation *string, tags *[]build.TestTag, tv *analysispb.TestVariantStabilityAnalysis) {
	if dryRun {
		*tags = insertTestTag(*tags, exonerationDryRunTagKey, "true")
	} else {
		*status = runtests.TestExonerated
		*explanation = exonerationExplanation(tv)
	}
	if tv.GetFailureRate().GetIsMet() {
		*tags = insertTestTag(*tags, exonerationFailureRateTagKey, "true")
	}
	if tv.GetFlakeRate().GetIsMet() {
		*tags = insertTestTag(*tags, exonerationFlakeRateTagKey, "true")
	}
}

func applyStabilityVerdicts(dryRun bool, tests []runtests.TestDetails, failedTargets map[string]bool, unstable map[string]*analysispb.TestVariantStabilityAnalysis) {
	for i := range tests {
		t := &tests[i]
		if !failedTargets[t.Name] || !testIsInScope(t) {
			continue
		}

		if len(t.Cases) == 0 {
			if tv := cmp.Or(unstable[structuredDetailTestID(t)], unstable[t.Name]); tv != nil {
				applyVerdict(dryRun, &t.Status, &t.ExonerationExplanation, &t.Tags, tv)
			} else if dryRun {
				t.Tags = insertTestTag(t.Tags, exonerationDryRunTagKey, "false")
			}
			continue
		}

		scheme := resultdb.DetermineScheme(t)
		matchedCases := 0
		remainingFailedCases := 0
		for j := range t.Cases {
			tc := &t.Cases[j]
			if !runtests.IsFailure(tc.Status) || tc.Status == runtests.TestExonerated {
				continue
			}
			if caseIsInScope(tc) {
				if tv := cmp.Or(unstable[structuredCaseTestID(scheme, t, tc)], unstable[resultdb.LegacyTestCaseID(t.Name, tc.SuiteName, tc.CaseName)]); tv != nil {
					matchedCases++
					applyVerdict(dryRun, &tc.Status, &tc.ExonerationExplanation, &tc.Tags, tv)
					continue
				}
				if dryRun {
					tc.Tags = insertTestTag(tc.Tags, exonerationDryRunTagKey, "false")
				}
			}
			remainingFailedCases++
		}

		if matchedCases > 0 && remainingFailedCases == 0 {
			if dryRun {
				t.Tags = insertTestTag(t.Tags, exonerationDryRunTagKey, "true")
			} else {
				t.Status = runtests.TestExonerated
				t.ExonerationExplanation = suiteExonerationExplanation
			}
		} else if dryRun {
			t.Tags = insertTestTag(t.Tags, exonerationDryRunTagKey, "false")
		}
	}
}

func insertTestTag(tags []build.TestTag, key, value string) []build.TestTag {
	for i := range tags {
		if tags[i].Key == key {
			tags[i].Value = value
			return tags
		}
	}
	return append(tags, build.TestTag{Key: key, Value: value})
}
