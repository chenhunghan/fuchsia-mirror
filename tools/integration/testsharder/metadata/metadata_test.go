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
				Owners: []string{"testgoogler@google.com", "carverforbes@google.com"},
			},
		}

		metadataMap, _ := GetMetadata(getFakeTestSpecs(), checkoutDir)
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
					"subsystem@google.com",
					"umbrella1@google.com",
					"umbrella2@google.com",
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
