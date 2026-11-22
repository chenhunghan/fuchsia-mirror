// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package owners

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestResolverFind(t *testing.T) {
	tests := []struct {
		name      string
		files     map[string]string
		relDir    string
		want      []Entry
		wantTier1 []string
	}{
		{
			name: "single_owners_file",
			files: map[string]string{
				"src/sys/OWNERS": "carverforbes@google.com",
			},
			relDir: "src/sys",
			want: []Entry{
				{Email: "carverforbes@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "src/sys/OWNERS"},
			},
			wantTier1: []string{"carverforbes@google.com"},
		},
		{
			name: "nested_includes_preserve_proximity_order_and_deduplicate",
			files: map[string]string{
				"src/OWNERS":     "include /src/sys/OWNERS\numbrella1@google.com\nleaf@google.com\numbrella2@google.com",
				"src/sub/OWNERS": "include /src/OWNERS\nsubsystem@google.com",
				"src/sys/OWNERS": "include /src/sub/OWNERS\nleaf@google.com\nleaf2@google.com",
			},
			relDir: "src/sys",
			want: []Entry{
				{Email: "leaf@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "src/sys/OWNERS"},
				{Email: "leaf2@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "src/sys/OWNERS"},
				{Email: "subsystem@google.com", Tier: 2, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/sub/OWNERS"},
				{Email: "umbrella1@google.com", Tier: 3, OwnersStep: 1, IncludeDepth: 2, DirDistance: 0, OwnersFile: "src/OWNERS"},
				{Email: "umbrella2@google.com", Tier: 3, OwnersStep: 1, IncludeDepth: 2, DirDistance: 0, OwnersFile: "src/OWNERS"},
			},
			wantTier1: []string{"leaf@google.com", "leaf2@google.com"},
		},
		{
			name: "comments_service_accounts_per_file_and_multi_step_tiers",
			files: map[string]string{
				"OWNERS":               "global@google.com\n",
				"src/OWNERS":           "parent@google.com\nalice@google.com # deduplicated across steps\n",
				"src/team/OWNERS":      "team@google.com\nroller@fuchsia-infra.iam.gserviceaccount.com\n",
				"src/sys/OWNERS":       "# comment\n  alice@google.com  # primary owner\n*\nper-file *.cml = perfile@google.com\nper-file BUILD.gn = file: /OWNERS\nfile:/src/team/OWNERS\n",
				"src/sys/child/README": "inherits from src/sys and src\n",
			},
			relDir: "src/sys/child",
			want: []Entry{
				{Email: "alice@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/sys/OWNERS"},
				{Email: "team@google.com", Tier: 2, OwnersStep: 1, IncludeDepth: 1, DirDistance: 1, OwnersFile: "src/team/OWNERS"},
				{Email: "parent@google.com", Tier: 3, OwnersStep: 2, IncludeDepth: 0, DirDistance: 2, OwnersFile: "src/OWNERS"},
			},
			wantTier1: []string{"alice@google.com"},
		},
		{
			name: "include_only_leaf_promotes_first_novel_depth_to_tier_1",
			files: map[string]string{
				"src/OWNERS":      "umbrella@google.com\n",
				"src/team/OWNERS": "a@google.com\nb@google.com\n",
				"src/sys/OWNERS":  "file: ../team/OWNERS\n",
			},
			relDir: "src/sys",
			want: []Entry{
				{Email: "a@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/team/OWNERS"},
				{Email: "b@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/team/OWNERS"},
				{Email: "umbrella@google.com", Tier: 2, OwnersStep: 2, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/OWNERS"},
			},
			wantTier1: []string{"a@google.com", "b@google.com"},
		},
		{
			name: "include_propagates_set_noparent",
			files: map[string]string{
				"src/OWNERS":        "parent@google.com\n",
				"src/frozen/OWNERS": "set noparent\nfrozen@google.com\n",
				"src/api/OWNERS":    "include /src/frozen/OWNERS\n",
			},
			relDir: "src/api",
			want: []Entry{
				{Email: "frozen@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/frozen/OWNERS"},
			},
			wantTier1: []string{"frozen@google.com"},
		},
		{
			name: "file_directive_does_not_propagate_set_noparent_unless_also_included",
			files: map[string]string{
				"src/OWNERS":        "parent@google.com\n",
				"src/shared/OWNERS": "set noparent\nshared@google.com\n",
				"src/leaf/OWNERS":   "file: /src/shared/OWNERS\n",
			},
			relDir: "src/leaf",
			want: []Entry{
				{Email: "shared@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/shared/OWNERS"},
				{Email: "parent@google.com", Tier: 2, OwnersStep: 2, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/OWNERS"},
			},
			wantTier1: []string{"shared@google.com"},
		},
		{
			name: "diamond_file_and_include_still_propagates_noparent",
			files: map[string]string{
				"src/OWNERS":        "parent@google.com\n",
				"src/shared/OWNERS": "set noparent\nshared@google.com\n",
				"src/mid/OWNERS":    "include /src/shared/OWNERS\n",
				"src/leaf/OWNERS":   "file: /src/shared/OWNERS\ninclude /src/mid/OWNERS\n",
			},
			relDir: "src/leaf",
			want: []Entry{
				{Email: "shared@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "src/shared/OWNERS"},
			},
			wantTier1: []string{"shared@google.com"},
		},
		{
			name: "explicit_root_include_allowed_while_root_dir_skips_owners",
			files: map[string]string{
				"OWNERS":       "root@google.com\n",
				"tools/OWNERS": "include /OWNERS\n",
			},
			relDir: "tools",
			want: []Entry{
				{Email: "root@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "OWNERS"},
			},
			wantTier1: []string{"root@google.com"},
		},
		{
			name: "root_directory_returns_no_owners",
			files: map[string]string{
				"OWNERS": "root@google.com\n",
			},
			relDir: "",
		},
		{
			name: "vendor_subrepo_anchors_slash_includes_and_respects_project_prefixes",
			files: map[string]string{
				"OWNERS":                   "root@google.com\n",
				"tools/OWNERS":             "main-tools@google.com\n",
				"sub/OWNERS":               "wrong-main-sub@google.com\n",
				"vendor/OWNERS":            "vendor-umbrella@google.com\n",
				"vendor/acme/.git/HEAD":    "ref: refs/heads/main\n",
				"vendor/acme/tools/OWNERS": "acme-tools@google.com\n",
				"vendor/acme/sub/OWNERS":   "acme-sub@google.com\n",
				"vendor/acme/foo/OWNERS":   "include /tools/OWNERS\ninclude :main:/sub/OWNERS\ninclude fuchsia:tools/OWNERS\ninclude external-repo:main:/OWNERS\n",
			},
			relDir: "vendor/acme/foo",
			want: []Entry{
				{Email: "acme-tools@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "vendor/acme/tools/OWNERS"},
				{Email: "acme-sub@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "vendor/acme/sub/OWNERS"},
				{Email: "main-tools@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "tools/OWNERS"},
				{Email: "vendor-umbrella@google.com", Tier: 2, OwnersStep: 2, IncludeDepth: 0, DirDistance: 2, OwnersFile: "vendor/OWNERS"},
			},
			wantTier1: []string{"acme-tools@google.com", "acme-sub@google.com", "main-tools@google.com"},
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
			got, err := r.Find(tc.relDir)
			if err != nil {
				t.Fatalf("Find(%q) unexpected error: %v", tc.relDir, err)
			}
			if diff := cmp.Diff(tc.want, got); diff != "" {
				t.Errorf("Find(%q) diff (-want +got):\n%s", tc.relDir, diff)
			}
			if diff := cmp.Diff(tc.wantTier1, Tier1Emails(got)); diff != "" {
				t.Errorf("Tier1Emails(Find(%q)) diff (-want +got):\n%s", tc.relDir, diff)
			}
		})
	}
}

func TestResolver_SetKnownFiles(t *testing.T) {
	checkoutDir := t.TempDir()
	writeFiles(t, checkoutDir, map[string]string{
		"src/OWNERS":               "src@google.com\n",
		"src/sys/OWNERS":           "sys@google.com\n",
		"tools/OWNERS":             "main-tools@google.com\n",
		"vendor/acme/tools/OWNERS": "acme-tools@google.com\n",
		"vendor/acme/foo/OWNERS":   "include /tools/OWNERS\n",
	})

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}

	// Prime dirCache and ownersCache before SetKnownFiles. Without a .git marker on
	// disk, vendor/acme/foo resolves /tools/OWNERS against the main checkout root.
	gotInitial, err := r.Find("src/sys")
	if err != nil {
		t.Fatalf("Find(\"src/sys\") unexpected error: %v", err)
	}
	if diff := cmp.Diff([]string{"sys@google.com"}, Tier1Emails(gotInitial)); diff != "" {
		t.Errorf("initial Tier1Emails(Find(\"src/sys\")) diff (-want +got):\n%s", diff)
	}
	gotVendorInitial, err := r.Find("vendor/acme/foo")
	if err != nil {
		t.Fatalf("Find(\"vendor/acme/foo\") unexpected error: %v", err)
	}
	if diff := cmp.Diff([]string{"main-tools@google.com"}, Tier1Emails(gotVendorInitial)); diff != "" {
		t.Errorf("initial Tier1Emails(Find(\"vendor/acme/foo\")) diff (-want +got):\n%s", diff)
	}

	// Restricting knownFiles omits src/sys/OWNERS and registers vendor/acme/.git,
	// verifying both cache invalidation and subrepository .git detection via knownFiles.
	known := map[string]bool{
		"src/OWNERS":             true,
		"vendor/acme/.git":       true,
		"vendor/acme/foo/OWNERS": true,
	}
	r.SetKnownFiles(known)
	known["src/sys/OWNERS"] = true // Caller mutation after SetKnownFiles must not affect r.

	gotFiltered, err := r.Find("src/sys")
	if err != nil {
		t.Fatalf("Find(\"src/sys\") after SetKnownFiles unexpected error: %v", err)
	}
	if diff := cmp.Diff([]string{"src@google.com"}, Tier1Emails(gotFiltered)); diff != "" {
		t.Errorf("filtered Tier1Emails(Find(\"src/sys\")) diff (-want +got):\n%s", diff)
	}
	gotVendorSubrepo, err := r.Find("vendor/acme/foo")
	if err != nil {
		t.Fatalf("Find(\"vendor/acme/foo\") after SetKnownFiles unexpected error: %v", err)
	}
	if diff := cmp.Diff([]string{"acme-tools@google.com"}, Tier1Emails(gotVendorSubrepo)); diff != "" {
		t.Errorf("subrepo Tier1Emails(Find(\"vendor/acme/foo\")) diff (-want +got):\n%s", diff)
	}

	// Passing nil restores direct filesystem probing.
	r.SetKnownFiles(nil)
	gotRestored, err := r.Find("src/sys")
	if err != nil {
		t.Fatalf("Find(\"src/sys\") after SetKnownFiles(nil) unexpected error: %v", err)
	}
	if diff := cmp.Diff([]string{"sys@google.com"}, Tier1Emails(gotRestored)); diff != "" {
		t.Errorf("restored Tier1Emails(Find(\"src/sys\")) diff (-want +got):\n%s", diff)
	}
}

func TestResolver_Memoization(t *testing.T) {
	checkoutDir := t.TempDir()
	writeFiles(t, checkoutDir, map[string]string{
		"src/sys/OWNERS":          "sys@google.com\n",
		"src/sys/override/OWNERS": "leaf@google.com\n",
	})

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}

	gotChild, err := r.Find("src/sys/child")
	if err != nil {
		t.Fatalf("Find(\"src/sys/child\") unexpected error: %v", err)
	}
	wantChild := []Entry{
		{Email: "sys@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/sys/OWNERS"},
	}
	if diff := cmp.Diff(wantChild, gotChild); diff != "" {
		t.Errorf("first Find(\"src/sys/child\") diff (-want +got):\n%s", diff)
	}

	// Mutating the returned slice must not corrupt dirCache for subsequent calls.
	gotChild[0].Email = "mutated@google.com"
	gotChildCached, err := r.Find("src/sys/child")
	if err != nil {
		t.Fatalf("cached Find(\"src/sys/child\") unexpected error: %v", err)
	}
	if diff := cmp.Diff(wantChild, gotChildCached); diff != "" {
		t.Errorf("cached Find(\"src/sys/child\") diff (-want +got):\n%s", diff)
	}

	// Reusing ownersCache for src/sys/OWNERS from another directory must recompute
	// directory-specific Tier, OwnersStep, and DirDistance.
	gotOverride, err := r.Find("src/sys/override/nested")
	if err != nil {
		t.Fatalf("Find(\"src/sys/override/nested\") unexpected error: %v", err)
	}
	wantOverride := []Entry{
		{Email: "leaf@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/sys/override/OWNERS"},
		{Email: "sys@google.com", Tier: 2, OwnersStep: 2, IncludeDepth: 0, DirDistance: 2, OwnersFile: "src/sys/OWNERS"},
	}
	if diff := cmp.Diff(wantOverride, gotOverride); diff != "" {
		t.Errorf("Find(\"src/sys/override/nested\") diff (-want +got):\n%s", diff)
	}
}

func TestResolverFind_RejectsEscapingPaths(t *testing.T) {
	checkoutDir := t.TempDir()
	outsideDir := t.TempDir()
	writeFiles(t, outsideDir, map[string]string{
		"OWNERS": "outside@google.com\n",
	})
	writeFiles(t, checkoutDir, map[string]string{
		"src/escape/OWNERS":           "include ../../../outside/OWNERS\n",
		"vendor/acme/.git":            "gitdir: ../../.git/modules/vendor/acme\n",
		"vendor/acme/escape/OWNERS":   "include ../../src/OWNERS\n",
		"vendor/acme/slash_up/OWNERS": "include /a/../../outside/OWNERS\n",
	})
	symlinkDir := filepath.Join(checkoutDir, "src", "symlink_escape")
	if err := os.MkdirAll(symlinkDir, 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) failed: %v", symlinkDir, err)
	}
	if err := os.Symlink(filepath.Join(outsideDir, "OWNERS"), filepath.Join(symlinkDir, "OWNERS")); err != nil {
		t.Fatalf("Symlink() failed: %v", err)
	}

	r, err := NewResolver(checkoutDir)
	if err != nil {
		t.Fatalf("NewResolver(%q) unexpected error: %v", checkoutDir, err)
	}
	for _, dir := range []string{
		"../outside",
		"src/escape",
		"src/symlink_escape",
		"vendor/acme/escape",
		"vendor/acme/slash_up",
	} {
		if _, err := r.Find(dir); err == nil {
			t.Errorf("Find(%q) = nil error, want escape error", dir)
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
