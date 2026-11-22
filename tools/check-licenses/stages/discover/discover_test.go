// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package discover

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestCrawler_Run(t *testing.T) {
	tempDir := t.TempDir()

	// Resolve the tempDir to absolute path since our Crawler forces absolute
	absTempDir, err := filepath.Abs(tempDir)
	if err != nil {
		t.Fatal(err)
	}

	// Create a nested file
	nestedDir := filepath.Join(absTempDir, "subdir")
	if err := os.Mkdir(nestedDir, 0755); err != nil {
		t.Fatal(err)
	}
	testFilePath := filepath.Join(nestedDir, "test.txt")
	if err := os.WriteFile(testFilePath, []byte("test content"), 0644); err != nil {
		t.Fatal(err)
	}

	crawler := NewCrawler(absTempDir, Config{
		SkipPaths:    map[string]bool{"subdir_skipped": true},
		SkipAnywhere: map[string]bool{"skip_me": true},
	})

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	pathChan, err := crawler.Run(ctx, []string{absTempDir})
	if err != nil {
		t.Fatalf("Failed to run crawler: %v", err)
	}

	foundPaths := make(map[string]bool)
	for p := range pathChan {
		foundPaths[p.Path] = true
	}

	// We expect to find the root dir, the subdir, and the test.txt file
	expectedPaths := []string{
		absTempDir,
		nestedDir,
		testFilePath,
	}

	for _, expected := range expectedPaths {
		if !foundPaths[expected] {
			t.Errorf("Expected crawler to emit path %q, but it was not found. Found: %v", expected, foundPaths)
		}
	}
}

func TestCrawler_Symlinks(t *testing.T) {
	tempDir := t.TempDir()
	absTempDir, err := filepath.Abs(tempDir)
	if err != nil {
		t.Fatal(err)
	}

	// Real file and real directory targets.
	realFile := filepath.Join(absTempDir, "real_license.txt")
	if err := os.WriteFile(realFile, []byte("BSD"), 0644); err != nil {
		t.Fatal(err)
	}
	realDir := filepath.Join(absTempDir, "real_repo")
	if err := os.Mkdir(realDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(realDir, "BUILD.bazel"), []byte("# build"), 0644); err != nil {
		t.Fatal(err)
	}

	// 1. Symlink to a regular file (must be emitted as IsDir=false).
	fileSymlink := filepath.Join(absTempDir, "LICENSE")
	if err := os.Symlink("real_license.txt", fileSymlink); err != nil {
		t.Fatal(err)
	}

	// 2. Symlink to a directory (must be skipped).
	dirSymlink := filepath.Join(absTempDir, "deduped_repo")
	if err := os.Symlink("real_repo", dirSymlink); err != nil {
		t.Fatal(err)
	}

	// 3. Broken symlink (must be skipped).
	brokenSymlink := filepath.Join(absTempDir, "broken_link")
	if err := os.Symlink("does_not_exist", brokenSymlink); err != nil {
		t.Fatal(err)
	}

	crawler := NewCrawler(absTempDir, Config{})
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	pathChan, err := crawler.Run(ctx, []string{absTempDir})
	if err != nil {
		t.Fatalf("Failed to run crawler: %v", err)
	}

	found := make(map[string]bool)
	for p := range pathChan {
		found[p.Path] = p.IsDir
	}

	if isDir, ok := found[fileSymlink]; !ok || isDir {
		t.Errorf("Expected file symlink %q to be emitted with IsDir=false, got ok=%v isDir=%v", fileSymlink, ok, isDir)
	}
	if _, ok := found[dirSymlink]; ok {
		t.Errorf("Expected directory symlink %q to be skipped, but it was emitted", dirSymlink)
	}
	if _, ok := found[brokenSymlink]; ok {
		t.Errorf("Expected broken symlink %q to be skipped, but it was emitted", brokenSymlink)
	}
}
