// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package build

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
)

// BuildAPIClient is a convenience interface for accessing the build API module
// files from the build system, using the //build/api/client script.
type BuildAPIClient struct {
	buildDir string
	toolPath string
}

// NewBuildAPIClient returns a BuildAPIClient associated with a given build
// directory. Note that `gn gen` must be run once to generate the
// $buildDir/build_api_client_path file which will be used to locate
// the script from the checkout directory.
func NewBuildAPIClient(buildDir string) (*BuildAPIClient, error) {
	relativePath, err := os.ReadFile(filepath.Join(buildDir, "build_api_client_path"))
	if err != nil {
		return nil, err
	}
	toolPath := filepath.Join(buildDir, string(relativePath))
	if _, err := os.Stat(toolPath); err != nil {
		return nil, err
	}
	c := &BuildAPIClient{buildDir, toolPath}
	return c, nil
}

// GetRaw returns the content of a build API module file as a raw string.
func (c BuildAPIClient) GetRaw(name string) ([]byte, error) {
	cmd := exec.Command(c.toolPath, "--build-dir", c.buildDir, "print", name)
	cmd.Stderr = os.Stderr
	output, err := cmd.Output()
	if err != nil {
		log.Fatal(err)
	}
	return output, err
}

// GetJson reads build API module file as JSON.
func (c BuildAPIClient) GetJSON(name string, v any) error {
	content, err := c.GetRaw(name)
	if err != nil {
		return err
	}
	return json.Unmarshal(content, v)
}

func (c BuildAPIClient) GetModulePaths() ([]string, error) {
	var paths []string
	cmd := exec.Command(c.toolPath, "--build-dir", c.buildDir, "print_all", "--pretty")
	cmd.Stderr = os.Stderr
	output, err := cmd.Output()
	if err != nil {
		return paths, err
	}
	var buildAPIModules map[string]struct {
		File string `json:"file"`
	}
	if err := json.Unmarshal(output, &buildAPIModules); err != nil {
		return paths, err
	}
	for _, module := range buildAPIModules {
		paths = append(paths, module.File)
	}
	return paths, nil
}

func (c BuildAPIClient) ExportDebugSymbols(ctx context.Context, outputDir string, withBreakpad bool) error {
	args := []string{
		"--build-dir",
		c.buildDir,
		"export_last_build_debug_symbols",
		fmt.Sprintf("--output-dir=%s", outputDir),
	}
	if withBreakpad {
		args = append(args, "--with-breakpad-symbols")
	}
	cmd := exec.CommandContext(ctx, c.toolPath, args...)
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("export_last_build_debug_symbols failed: %w", err)
	}
	return nil
}

// AffectedTestsResult contains the result of calling the affected_tests tool.
type AffectedTestsResult struct {
	// Targets is the list of affected test target labels (e.g. "//src/foo:bar_test").
	Targets []string
	// BuildNotAffected is true if the changed files affected no targets in the
	// build graph.
	BuildNotAffected bool
}

// affectedTestsOutput mirrors the JSON emitted by
// `build/api/client affected_tests --format=json`.
type affectedTestsOutput struct {
	TestTargets []struct {
		Label string `json:"label"`
		Env   string `json:"env"`
	} `json:"test_targets"`
	BuildNotAffected bool `json:"build_not_affected"`
}

// AffectedTests runs the build/api/client affected_tests command with the given
// files list and returns the affected test targets and whether the build graph
// was affected.
func (c BuildAPIClient) AffectedTests(ctx context.Context, filesListPath string) (*AffectedTestsResult, error) {
	cmd := exec.CommandContext(ctx, c.toolPath,
		"--build-dir", c.buildDir,
		"affected_tests",
		"--files-list="+filesListPath,
		"--format=json",
	)
	cmd.Stderr = os.Stderr
	output, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("affected_tests failed: %w", err)
	}

	var out affectedTestsOutput
	if err := json.Unmarshal(output, &out); err != nil {
		return nil, fmt.Errorf("failed to unmarshal affected_tests output: %w", err)
	}

	targets := make([]string, 0, len(out.TestTargets))
	for _, t := range out.TestTargets {
		targets = append(targets, t.Label)
	}
	return &AffectedTestsResult{
		Targets:          targets,
		BuildNotAffected: out.BuildNotAffected,
	}, nil
}
