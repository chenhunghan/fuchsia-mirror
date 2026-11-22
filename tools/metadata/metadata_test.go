// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package metadata

import (
	"os"
	"path/filepath"
	"testing"
)

func TestFindComponentID(t *testing.T) {
	tests := []struct {
		name            string
		files           map[string]string
		relDir          string
		wantComponentID int64
	}{
		{
			name: "direct_metadata_file",
			files: map[string]string{
				"src/sys/METADATA.textproto": "trackers: { issue_tracker: { component_id: 1478143 } }",
			},
			relDir:          "src/sys",
			wantComponentID: 1478143,
		},
		{
			name: "inherits_from_ancestor_and_discards_unknown_fields",
			files: map[string]string{
				"src/METADATA.textproto":     "unknown_future_field: 1\ntrackers: { issue_tracker: {} issue_tracker: { component_id: 42 } }",
				"src/sys/METADATA.textproto": "name: \"sys\"",
			},
			relDir:          "src/sys/child",
			wantComponentID: 42,
		},
		{
			name: "inherits_root_metadata_fallback_and_ignores_invalid_child",
			files: map[string]string{
				"METADATA.textproto":         "trackers: { issue_tracker: { component_id: 99 } }",
				"src/sys/METADATA.textproto": "not valid textproto {{{",
			},
			relDir:          "src/sys",
			wantComponentID: 99,
		},
		{
			name: "root_directory_resolves_root_metadata",
			files: map[string]string{
				"METADATA.textproto": "trackers: { issue_tracker: { component_id: 7 } }",
			},
			relDir:          "",
			wantComponentID: 7,
		},
		{
			name: "returns_zero_when_no_positive_component_id_in_ancestors_or_root",
			files: map[string]string{
				"METADATA.textproto":         "name: \"root\"",
				"src/sys/METADATA.textproto": "not valid textproto {{{",
			},
			relDir:          "src/sys/child",
			wantComponentID: 0,
		},
		{
			name: "vendor_subrepo_stops_at_subrepo_boundary",
			files: map[string]string{
				"METADATA.textproto":        "trackers: { issue_tracker: { component_id: 99 } }",
				"vendor/METADATA.textproto": "trackers: { issue_tracker: { component_id: 88 } }",
				"vendor/acme/.git/HEAD":     "ref: refs/heads/main\n",
			},
			relDir:          "vendor/acme/foo",
			wantComponentID: 0,
		},
		{
			name: "vendor_subrepo_inherits_within_subrepo_root",
			files: map[string]string{
				"METADATA.textproto":             "trackers: { issue_tracker: { component_id: 99 } }",
				"vendor/acme/.git/HEAD":          "ref: refs/heads/main\n",
				"vendor/acme/METADATA.textproto": "trackers: { issue_tracker: { component_id: 55 } }",
			},
			relDir:          "vendor/acme/foo",
			wantComponentID: 55,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			checkoutDir := t.TempDir()
			writeFiles(t, checkoutDir, tc.files)

			r, err := NewResolver(checkoutDir)
			if err != nil {
				t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
			}
			got, err := r.FindComponentID(tc.relDir)
			if err != nil {
				t.Fatalf("FindComponentID(%q) unexpected error: %v", tc.relDir, err)
			}
			if got != tc.wantComponentID {
				t.Errorf("FindComponentID(%q) = %d, want %d", tc.relDir, got, tc.wantComponentID)
			}
			gotOneShot, err := FindComponentID(checkoutDir, tc.relDir)
			if err != nil {
				t.Fatalf("metadata.FindComponentID(%q, %q) unexpected error: %v", checkoutDir, tc.relDir, err)
			}
			if gotOneShot != tc.wantComponentID {
				t.Errorf("metadata.FindComponentID(%q, %q) = %d, want %d", checkoutDir, tc.relDir, gotOneShot, tc.wantComponentID)
			}
		})
	}
}

func TestResolver_SetKnownFiles(t *testing.T) {
	checkoutDir := t.TempDir()
	writeFiles(t, checkoutDir, map[string]string{
		"src/METADATA.textproto":     "trackers: { issue_tracker: { component_id: 10 } }",
		"src/sys/METADATA.textproto": "trackers: { issue_tracker: { component_id: 20 } }",
	})

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}
	gotInitial, err := r.FindComponentID("src/sys")
	if err != nil {
		t.Fatalf("FindComponentID(\"src/sys\") unexpected error: %v", err)
	}
	if gotInitial != 20 {
		t.Errorf("initial FindComponentID(\"src/sys\") = %d, want 20", gotInitial)
	}

	known := map[string]bool{"./src/METADATA.textproto": true}
	r.SetKnownFiles(known)
	known["src/sys/METADATA.textproto"] = true // Caller mutation must not affect r.

	gotFiltered, err := r.FindComponentID("src/sys")
	if err != nil {
		t.Fatalf("FindComponentID(\"src/sys\") after SetKnownFiles unexpected error: %v", err)
	}
	if gotFiltered != 10 {
		t.Errorf("filtered FindComponentID(\"src/sys\") = %d, want 10", gotFiltered)
	}

	r.SetKnownFiles(nil)
	gotRestored, err := r.FindComponentID("src/sys")
	if err != nil {
		t.Fatalf("FindComponentID(\"src/sys\") after SetKnownFiles(nil) unexpected error: %v", err)
	}
	if gotRestored != 20 {
		t.Errorf("restored FindComponentID(\"src/sys\") = %d, want 20", gotRestored)
	}
}

func TestResolver_CachesLookups(t *testing.T) {
	checkoutDir := t.TempDir()
	metaPath := filepath.Join(checkoutDir, "src", "METADATA.textproto")
	writeFiles(t, checkoutDir, map[string]string{
		"src/METADATA.textproto": "trackers: { issue_tracker: { component_id: 42 } }",
	})

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}
	got, err := r.FindComponentID("src/sys/leaf")
	if err != nil {
		t.Fatalf("FindComponentID(\"src/sys/leaf\") unexpected error: %v", err)
	}
	if got != 42 {
		t.Fatalf("FindComponentID(\"src/sys/leaf\") = %d, want 42", got)
	}

	if err := os.Remove(metaPath); err != nil {
		t.Fatalf("Remove(%q) failed: %v", metaPath, err)
	}
	for _, dir := range []string{"src/sys/leaf", "src/sys/sibling"} {
		got, err := r.FindComponentID(dir)
		if err != nil {
			t.Errorf("cached FindComponentID(%q) unexpected error: %v", dir, err)
			continue
		}
		if got != 42 {
			t.Errorf("cached FindComponentID(%q) = %d, want 42", dir, got)
		}
	}

	r.SetKnownFiles(nil)
	gotFromFileCache, err := r.FindComponentID("src")
	if err != nil {
		t.Fatalf("fileCache FindComponentID(\"src\") unexpected error: %v", err)
	}
	if gotFromFileCache != 42 {
		t.Errorf("fileCache FindComponentID(\"src\") = %d, want 42", gotFromFileCache)
	}
}

func TestFindComponentID_RejectsEscapingPath(t *testing.T) {
	checkoutDir := t.TempDir()
	outsideDir := t.TempDir()
	writeFiles(t, outsideDir, map[string]string{
		"METADATA.textproto": "trackers: { issue_tracker: { component_id: 999 } }",
	})
	symlinkDir := filepath.Join(checkoutDir, "src", "symlink_escape")
	if err := os.MkdirAll(symlinkDir, 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) failed: %v", symlinkDir, err)
	}
	if err := os.Symlink(filepath.Join(outsideDir, "METADATA.textproto"), filepath.Join(symlinkDir, "METADATA.textproto")); err != nil {
		t.Fatalf("Symlink() failed: %v", err)
	}

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}
	for _, dir := range []string{"../outside", "src/symlink_escape"} {
		if _, err := r.FindComponentID(dir); err == nil {
			t.Errorf("FindComponentID(%q) = nil error, want escape error", dir)
		}
	}
}

func TestNewResolver_Errors(t *testing.T) {
	dir := t.TempDir()
	filePath := filepath.Join(dir, "not_a_dir")
	if err := os.WriteFile(filePath, []byte("x"), 0o644); err != nil {
		t.Fatalf("WriteFile(%q) failed: %v", filePath, err)
	}
	for _, tc := range []struct {
		name string
		path string
	}{
		{name: "empty_path", path: ""},
		{name: "nonexistent_path", path: filepath.Join(dir, "missing")},
		{name: "regular_file_path", path: filePath},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := NewResolver(tc.path); err == nil {
				t.Errorf("NewResolver(%q) = nil error, want error", tc.path)
			}
		})
	}
}

func writeFiles(t *testing.T, root string, files map[string]string) {
	t.Helper()
	for rel, contents := range files {
		abs := filepath.Join(root, filepath.FromSlash(rel))
		if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
			t.Fatalf("MkdirAll(%q) failed: %v", filepath.Dir(abs), err)
		}
		if err := os.WriteFile(abs, []byte(contents), 0o644); err != nil {
			t.Fatalf("WriteFile(%q) failed: %v", abs, err)
		}
	}
}
