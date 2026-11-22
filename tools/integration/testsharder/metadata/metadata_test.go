// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package metadata

import (
	"testing"

	"os"
	"path/filepath"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/build"
)

func TestGetMetadata(t *testing.T) {
	metadataFileContent :=
		`trackers: {
  			issue_tracker: {
    			component_id: 1478143
  			}
		}`
	simpleOwnersFileContent := `carverforbes@google.com`
	ownersFileWithIncludeContent := "include /src/OWNERS\ntestgoogler@google.com"

	t.Run("one owners file and one metadata file", func(t *testing.T) {
		// Create checkout directory
		checkoutDir := t.TempDir()

		// Create and write Metadata.Textproto file
		metadataFilePath := filepath.Join(checkoutDir, "src", "sys", "METADATA.textproto")
		writeFile(t, metadataFilePath, metadataFileContent)

		// Create and write OWNERS file
		ownersFilePath := filepath.Join(checkoutDir, "src", "sys", "OWNERS")
		ownersDir := filepath.Dir(ownersFilePath)
		if err := os.MkdirAll(ownersDir, os.ModePerm); err != nil {
			t.Fatalf("MkdirAll(%s) failed: %s", ownersDir, err)
		}
		writeFile(t, ownersFilePath, simpleOwnersFileContent)

		expected := map[string]TestMetadata{
			"fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm": {
				Owners:      []string{"carverforbes@google.com"},
				ComponentID: 1478143,
			},
		}

		metadataMap, _ := GetMetadata(getFakeTestSpecs(), checkoutDir)
		if diff := cmp.Diff(expected, metadataMap); diff != "" {
			t.Errorf("GetMetadata() failed: (-want +got): \n%s", diff)
		}
	})

	t.Run("owners file has include line", func(t *testing.T) {
		// Create checkout directory
		checkoutDir := t.TempDir()

		// Create and write OWNERS files
		simpleOwnersFilePath := filepath.Join(checkoutDir, "src", "OWNERS")
		ownersFileWithIncludePath := filepath.Join(checkoutDir, "src", "sys", "OWNERS")
		ownersDir := filepath.Dir(ownersFileWithIncludePath)
		if err := os.MkdirAll(ownersDir, os.ModePerm); err != nil {
			t.Fatalf("MkdirAll(%s) failed: %s", ownersDir, err)
		}
		writeFile(t, ownersFileWithIncludePath, ownersFileWithIncludeContent)
		writeFile(t, simpleOwnersFilePath, simpleOwnersFileContent)

		expected := map[string]TestMetadata{
			"fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm": {
				Owners: []string{"testgoogler@google.com"},
			},
		}

		metadataMap, err := GetMetadata(getFakeTestSpecs(), checkoutDir)
		if err != nil {
			t.Fatalf("GetMetadata() unexpected error: %v", err)
		}
		if diff := cmp.Diff(expected, metadataMap); diff != "" {
			t.Errorf("GetMetadata() failed: (-want +got): \n%s", diff)
		}
	})

	t.Run("nested includes preserve proximity order and deduplicate", func(t *testing.T) {
		checkoutDir := t.TempDir()

		umbrellaOwnersPath := filepath.Join(checkoutDir, "src", "OWNERS")
		subsystemOwnersPath := filepath.Join(checkoutDir, "src", "sub", "OWNERS")
		leafOwnersPath := filepath.Join(checkoutDir, "src", "sys", "OWNERS")

		writeFile(t, umbrellaOwnersPath, "include /src/sys/OWNERS\numbrella1@google.com\nleaf@google.com\numbrella2@google.com")
		writeFile(t, subsystemOwnersPath, "include /src/OWNERS\nsubsystem@google.com")
		writeFile(t, leafOwnersPath, "include /src/sub/OWNERS\nleaf@google.com\nleaf2@google.com")

		expected := map[string]TestMetadata{
			"fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm": {
				Owners: []string{
					"leaf@google.com",
					"leaf2@google.com",
				},
			},
		}

		metadataMap, err := GetMetadata(getFakeTestSpecs(), checkoutDir)
		if err != nil {
			t.Fatalf("GetMetadata() unexpected error: %v", err)
		}
		if diff := cmp.Diff(expected, metadataMap); diff != "" {
			t.Errorf("GetMetadata() failed: (-want +got): \n%s", diff)
		}
	})
}

func TestGetMetadata_IncludeOnlyAndBazelLabels(t *testing.T) {
	checkoutDir := t.TempDir()
	writeFile(t, filepath.Join(checkoutDir, "OWNERS"), "global@google.com\n")
	writeFile(t, filepath.Join(checkoutDir, "METADATA.textproto"), "trackers: { issue_tracker: { component_id: 77 } }\n")
	writeFile(t, filepath.Join(checkoutDir, "src", "team", "OWNERS"), "team1@google.com # inline comment\nteam2@google.com\n")
	writeFile(t, filepath.Join(checkoutDir, "src", "sys", "OWNERS"), "file: /src/team/OWNERS\n")

	specs := []build.TestSpec{
		{Test: build.Test{Name: "file_directive_test", SourceLabel: "//src/sys:foo_test(//build/toolchain/fuchsia:x64)"}},
		{Test: build.Test{Name: "whitespace_padded_label_test", SourceLabel: "  //src/sys:spaced_test (//build/toolchain/fuchsia:x64)  "}},
		{Test: build.Test{Name: "short_gn_label_test", SourceLabel: "//src/sys"}},
		{Test: build.Test{Name: "bazel_main_repo_test", SourceLabel: "@@//src/sys:bar_test"}},
		{Test: build.Test{Name: "root_label_test", SourceLabel: "//:root_test"}},
		{Test: build.Test{Name: "external_repo_test", SourceLabel: "@ext_repo//pkg:ext_test"}},
	}
	got, err := GetMetadata(specs, checkoutDir)
	if err != nil {
		t.Fatalf("GetMetadata() unexpected error: %v", err)
	}
	want := map[string]TestMetadata{
		"file_directive_test":          {Owners: []string{"team1@google.com", "team2@google.com"}, ComponentID: 77},
		"whitespace_padded_label_test": {Owners: []string{"team1@google.com", "team2@google.com"}, ComponentID: 77},
		"short_gn_label_test":          {Owners: []string{"team1@google.com", "team2@google.com"}, ComponentID: 77},
		"bazel_main_repo_test":         {Owners: []string{"team1@google.com", "team2@google.com"}, ComponentID: 77},
		"root_label_test":              {ComponentID: 77},
		"external_repo_test":           {},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("GetMetadata() diff (-want +got):\n%s", diff)
	}
}

func TestGetMetadata_EmptySpecs(t *testing.T) {
	got, err := GetMetadata(nil, "")
	if err != nil {
		t.Fatalf("GetMetadata(nil, \"\") unexpected error: %v", err)
	}
	if diff := cmp.Diff(map[string]TestMetadata{}, got); diff != "" {
		t.Errorf("GetMetadata(nil, \"\") diff (-want +got):\n%s", diff)
	}
}

func TestGetMetadata_Errors(t *testing.T) {
	checkoutDir := t.TempDir()
	tests := []struct {
		name        string
		specs       []build.TestSpec
		checkoutDir string
	}{
		{
			name:  "empty_checkout_dir",
			specs: getFakeTestSpecs(),
		},
		{
			name:        "missing_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "whitespace_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "   "}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "toolchain_only_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "(//build/toolchain/fuchsia:x64)"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "unrooted_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: ":foo_test"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "empty_target_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "//src/sys:"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "missing_package_and_target_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "//"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "bare_at_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "@@"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "triple_at_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "@@@//src/sys:foo_test"}}},
			checkoutDir: checkoutDir,
		},
		{
			name:        "escaping_source_label",
			specs:       []build.TestSpec{{Test: build.Test{Name: "t", SourceLabel: "//../outside:foo_test"}}},
			checkoutDir: checkoutDir,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := GetMetadata(tc.specs, tc.checkoutDir); err == nil {
				t.Errorf("GetMetadata(%v, %q) = nil error, want error", tc.specs, tc.checkoutDir)
			}
		})
	}
}

func getFakeTestSpecs() []build.TestSpec {
	testSpecs := []build.TestSpec{
		{
			Test: build.Test{
				Name:         "fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm",
				Label:        "//src/sys:foo_test(//build/toolchain/fuchsia:x64)",
				SourceLabel:  "//src/sys:foo_test",
				OS:           "fuchsia",
				PackageLabel: "//src/sys:foo_test_package(//build/toolchain/fuchsia:x64)",
				PackageURL:   "fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm",
			},
			Envs: []build.Environment{
				{
					Dimensions: build.DimensionSet{
						"device_type": "AEMU",
					},
				},
			},
		},
	}
	return testSpecs
}

func writeFile(t *testing.T, path string, contents string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), os.ModePerm); err != nil {
		t.Fatalf("MkdirAll(%s) failed: %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(contents), os.ModePerm); err != nil {
		t.Fatalf("WriteFile(%s, %s), failed: %v", path, contents, err)
	}
}
