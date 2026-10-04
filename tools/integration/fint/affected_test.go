// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fint

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/build"
	fintpb "go.fuchsia.dev/fuchsia/tools/integration/fint/proto"
)

type mockBuildAPIClient struct {
	affectedTests    []string
	buildNotAffected bool
	recordedFiles    []string
	err              error
}

func (m *mockBuildAPIClient) ExportDebugSymbols(ctx context.Context, outputDir string, withBreakpad bool) error {
	return nil
}

func (m *mockBuildAPIClient) AffectedTests(ctx context.Context, filesListPath string) (*build.AffectedTestsResult, error) {
	if m.err != nil {
		return nil, m.err
	}
	content, err := os.ReadFile(filesListPath)
	if err == nil && len(content) > 0 {
		m.recordedFiles = strings.Split(strings.TrimSpace(string(content)), "\n")
	}
	return &build.AffectedTestsResult{
		Targets:          m.affectedTests,
		BuildNotAffected: m.buildNotAffected,
	}, nil
}

func TestResolveAffectedTestNames(t *testing.T) {
	testSpecs := []build.TestSpec{
		{
			Test: build.Test{
				Name:  "gn_test_name",
				Label: "//src/foo:foo_test(//build/toolchain/fuchsia:arm64)",
			},
		},
		{
			Test: build.Test{
				Name:        "bazel_test_name",
				Label:       "@@//src/bazel:bar_test",
				SourceLabel: "//src/bazel:bar_test",
			},
		},
		{
			Test: build.Test{
				Name:  "recovery_sim_test",
				Label: "//src/recovery/simulator:recovery_simulator_boot_test(//build/toolchain:x64)",
			},
		},
		{
			Test: build.Test{
				Name:  "unaffected_test",
				Label: "//src/other:other_test(//build/toolchain/fuchsia:arm64)",
			},
		},
	}

	targetLabels := []string{
		"//src/foo:foo_test(//build/toolchain/fuchsia:arm64)",
		"@@//src/bazel:bar_test",
		"//src/recovery/simulator:recovery_simulator_boot_test(//build/toolchain:x64)",
	}

	got := resolveAffectedTestNames(testSpecs, targetLabels)
	want := []string{"bazel_test_name", "gn_test_name"}

	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("resolveAffectedTestNames mismatch (-want +got):\n%s", diff)
	}
}

func TestWriteChangedFilesList(t *testing.T) {
	files := []*fintpb.Context_ChangedFile{
		{Path: "src/foo.cc"},
		{Path: "src/bar.py"},
	}

	artifactDir := t.TempDir()
	path, cleanup, err := writeChangedFilesList(context.Background(), artifactDir, files)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if !strings.HasPrefix(path, artifactDir) {
		t.Errorf("expected path to be inside artifactDir %q, got %q", artifactDir, path)
	}

	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("failed to read written file: %v", err)
	}

	expected := "src/foo.cc\nsrc/bar.py\n"
	if string(content) != expected {
		t.Errorf("writeChangedFilesList content got %q, want %q", string(content), expected)
	}

	cleanup()
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("expected file %q to be deleted after cleanup, got err: %v", path, err)
	}
}

func TestAffectedImpl(t *testing.T) {
	checkoutDir := t.TempDir()
	artifactDir := t.TempDir()
	buildDir := t.TempDir()

	testFile := "src/foo.cc"
	absPath := filepath.Join(checkoutDir, testFile)
	if err := os.MkdirAll(filepath.Dir(absPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(absPath, []byte("content"), 0o600); err != nil {
		t.Fatal(err)
	}

	contextSpec := &fintpb.Context{
		CheckoutDir: checkoutDir,
		BuildDir:    buildDir,
		ArtifactDir: artifactDir,
		ChangedFiles: []*fintpb.Context_ChangedFile{
			{Path: testFile},
		},
	}

	testSpecs := []build.TestSpec{
		{
			Test: build.Test{
				Name:  "gn_test",
				Label: "//src/foo:gn_test(//build/toolchain:arm64)",
			},
		},
		{
			Test: build.Test{
				Name:  "bazel_test",
				Label: "@@//src/bazel:bazel_test",
			},
		},
	}

	modules := fakeBuildModules{
		buildDir:  buildDir,
		testSpecs: testSpecs,
	}

	t.Run("affected tests found", func(t *testing.T) {
		client := &mockBuildAPIClient{
			affectedTests: []string{
				"//src/foo:gn_test(//build/toolchain:arm64)",
				"@@//src/bazel:bazel_test",
			},
		}

		artifacts, err := affectedImpl(context.Background(), client, contextSpec, modules)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}

		expected := []string{"bazel_test", "gn_test"}
		if diff := cmp.Diff(expected, artifacts.AffectedTests); diff != "" {
			t.Errorf("unexpected affected tests (-want +got):\n%s", diff)
		}
		if artifacts.BuildNotAffected {
			t.Errorf("expected BuildNotAffected to be false")
		}
	})

	t.Run("no affected tests", func(t *testing.T) {
		client := &mockBuildAPIClient{
			affectedTests: []string{},
		}

		artifacts, err := affectedImpl(context.Background(), client, contextSpec, modules)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}

		if len(artifacts.AffectedTests) != 0 {
			t.Errorf("expected 0 affected tests, got %v", artifacts.AffectedTests)
		}
		if artifacts.BuildNotAffected {
			t.Errorf("expected BuildNotAffected to be false, got true")
		}
	})

	t.Run("build not affected", func(t *testing.T) {
		client := &mockBuildAPIClient{
			affectedTests:    []string{},
			buildNotAffected: true,
		}

		artifacts, err := affectedImpl(context.Background(), client, contextSpec, modules)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}

		if len(artifacts.AffectedTests) != 0 {
			t.Errorf("expected 0 affected tests, got %v", artifacts.AffectedTests)
		}
		if !artifacts.BuildNotAffected {
			t.Errorf("expected BuildNotAffected to be true, got false")
		}
	})

	t.Run("no test specs returns early", func(t *testing.T) {
		client := &mockBuildAPIClient{
			affectedTests: []string{"//src/foo:gn_test"},
		}
		emptyModules := fakeBuildModules{
			buildDir:  buildDir,
			testSpecs: nil,
		}

		artifacts, err := affectedImpl(context.Background(), client, contextSpec, emptyModules)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}

		if len(artifacts.AffectedTests) != 0 {
			t.Errorf("expected 0 affected tests, got %v", artifacts.AffectedTests)
		}
		if artifacts.BuildNotAffected {
			t.Errorf("expected BuildNotAffected to be false, got true")
		}
		if len(client.recordedFiles) != 0 {
			t.Errorf("expected client.AffectedTests not to be called, but recorded files: %v", client.recordedFiles)
		}
	})

	t.Run("tool error", func(t *testing.T) {
		client := &mockBuildAPIClient{
			err: errors.New("tool failure"),
		}

		_, err := affectedImpl(context.Background(), client, contextSpec, modules)
		if err == nil {
			t.Fatalf("expected error from affectedImpl, got nil")
		}
	})
}
