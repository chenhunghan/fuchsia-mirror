// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fint

import (
	"context"
	"fmt"
	"os"
	"slices"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/build"
	fintpb "go.fuchsia.dev/fuchsia/tools/integration/fint/proto"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

var (
	// The following tests should never be considered affected. These tests use
	// a system image as data, so they appear affected by a broad range of
	// changes, but they're almost never actually sensitive to said changes.
	// https://fxbug.dev/42146209 tracks generating this list automatically.
	neverAffectedTestLabels = []string{
		"//src/recovery/simulator:recovery_simulator_boot_test",
		"//src/recovery/simulator:recovery_simulator_serial_test",
	}
)

// Affected runs the build/api/client affected_tests tool to determine which tests
// are affected by changed files.
func Affected(ctx context.Context, contextSpec *fintpb.Context) (*fintpb.BuildArtifacts, error) {
	modules, err := build.NewModules(contextSpec.BuildDir)
	if err != nil {
		return nil, err
	}
	client, err := build.NewBuildAPIClient(contextSpec.BuildDir)
	if err != nil {
		return nil, err
	}
	artifacts, err := affectedImpl(ctx, client, contextSpec, modules)
	if err != nil && artifacts != nil && artifacts.FailureSummary == "" {
		// Fall back to using the error text as the failure summary if the
		// failure summary is unset. It's better than failing without emitting
		// any information.
		artifacts.FailureSummary = err.Error()
	}
	return artifacts, err
}

// affectedImpl contains the business logic of finding affected tests using the
// build/api/client affected_tests tool.
func affectedImpl(
	ctx context.Context,
	client buildAPIClient,
	contextSpec *fintpb.Context,
	modules buildModules,
) (*fintpb.BuildArtifacts, error) {
	artifacts := &fintpb.BuildArtifacts{}

	if contextSpec.ArtifactDir == "" || len(contextSpec.ChangedFiles) == 0 || len(modules.TestSpecs()) == 0 {
		return artifacts, nil
	}

	filesListPath, cleanup, err := writeChangedFilesList(ctx, contextSpec.ArtifactDir, contextSpec.ChangedFiles)
	if err != nil {
		return artifacts, err
	}
	defer cleanup()

	if client == nil {
		return artifacts, fmt.Errorf("buildAPIClient is nil")
	}

	res, err := client.AffectedTests(ctx, filesListPath)
	if err != nil {
		return artifacts, err
	}

	affectedTests := resolveAffectedTestNames(modules.TestSpecs(), res.Targets)
	artifacts.AffectedTests = affectedTests
	artifacts.BuildNotAffected = res.BuildNotAffected

	logger.Infof(
		ctx,
		"Found %d affected tests (build_not_affected=%t)",
		len(affectedTests),
		artifacts.BuildNotAffected,
	)

	return artifacts, nil
}

// writeChangedFilesList writes changed files to a temporary file in artifactDir, one per line.
// It returns the file path and a cleanup function to delete the temporary file.
func writeChangedFilesList(ctx context.Context, artifactDir string, changedFiles []*fintpb.Context_ChangedFile) (string, func(), error) {
	var paths []string
	for _, f := range changedFiles {
		paths = append(paths, f.Path)
	}

	if artifactDir != "" {
		if err := os.MkdirAll(artifactDir, 0o700); err != nil {
			return "", nil, err
		}
	}

	tmpFile, err := os.CreateTemp(artifactDir, "changed_files_*.txt")
	if err != nil {
		return "", nil, err
	}
	cleanup := func() {
		if err := os.Remove(tmpFile.Name()); err != nil {
			logger.Warningf(ctx, "failed to remove temporary file %s: %s", tmpFile.Name(), err)
		}
	}

	if _, err := tmpFile.WriteString(strings.Join(paths, "\n") + "\n"); err != nil {
		tmpFile.Close()
		cleanup()
		return "", nil, err
	}
	if err := tmpFile.Close(); err != nil {
		cleanup()
		return "", nil, err
	}

	return tmpFile.Name(), cleanup, nil
}

// resolveAffectedTestNames maps the target labels returned by build/api/client affected_tests
// to the actual test names (test.Name) defined in testSpecs.
func resolveAffectedTestNames(testSpecs []build.TestSpec, targetLabels []string) []string {
	testsByLabel := make(map[string][]string)
	testsByNoToolchainLabel := make(map[string][]string)

	for _, spec := range testSpecs {
		test := spec.Test
		labelNoToolchain := strings.Split(test.Label, "(")[0]
		if slices.Contains(neverAffectedTestLabels, labelNoToolchain) {
			continue
		}
		if test.Label != "" {
			testsByLabel[test.Label] = append(testsByLabel[test.Label], test.Name)
			testsByNoToolchainLabel[labelNoToolchain] = append(testsByNoToolchainLabel[labelNoToolchain], test.Name)
		}
		if test.PackageLabel != "" {
			testsByLabel[test.PackageLabel] = append(testsByLabel[test.PackageLabel], test.Name)
			pkgNoToolchain := strings.Split(test.PackageLabel, "(")[0]
			testsByNoToolchainLabel[pkgNoToolchain] = append(testsByNoToolchainLabel[pkgNoToolchain], test.Name)
		}
		if test.SourceLabel != "" {
			testsByLabel[test.SourceLabel] = append(testsByLabel[test.SourceLabel], test.Name)
			srcNoToolchain := strings.Split(test.SourceLabel, "(")[0]
			testsByNoToolchainLabel[srcNoToolchain] = append(testsByNoToolchainLabel[srcNoToolchain], test.Name)
		}
		testsByLabel[test.Name] = append(testsByLabel[test.Name], test.Name)
	}

	var affectedTests []string
	for _, targetLabel := range targetLabels {
		labelNoToolchain := strings.Split(targetLabel, "(")[0]
		if slices.Contains(neverAffectedTestLabels, labelNoToolchain) {
			continue
		}
		if names, ok := testsByLabel[targetLabel]; ok {
			affectedTests = append(affectedTests, names...)
		} else if names, ok := testsByNoToolchainLabel[labelNoToolchain]; ok {
			affectedTests = append(affectedTests, names...)
		} else {
			affectedTests = append(affectedTests, targetLabel)
		}
	}

	return removeDuplicates(affectedTests)
}
