// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/lib/owners"
)

func TestRun_WritesDirectoryOwnersJSON(t *testing.T) {
	checkout := t.TempDir()
	prebuiltOwners := path.Join("prebuilt", "tools", "OWNERS")
	writeFiles(t, checkout, map[string]string{
		"OWNERS":                                   "global@google.com\n",
		".git/config":                              "[core]\n",
		".jiri_root/bin":                           "skip\n",
		"integration/OWNERS":                       "skip@google.com\n",
		"out/default/OWNERS":                       "skip@google.com\n",
		prebuiltOwners:                             "skip@google.com\n",
		"src/OWNERS":                               "src-owner@google.com\n",
		"src/sys/OWNERS":                           "sys-owner@google.com # inline comment\nfile: /tools/team/OWNERS\n",
		"src/sys/METADATA.textproto":               "trackers: { issue_tracker: { component_id: 1478143 } }\n",
		"tools/team/OWNERS":                        "infra@google.com\n",
		"third_party/OWNERS":                       "tp-umbrella@google.com\n",
		"third_party/crate/BUILD.gn":               "# gn\n",
		"third_party/crate/src/lib.rs":             "// skipped without BUILD file\n",
		"third_party/lib_b/BUILD.bazel":            "# bazel\n",
		"third_party/symlinked_lib/real_build.gn":  "# real gn\n",
		"third_party/nonregular/sub/README.md":     "dir symlink target\n",
		"vendor/acme/.git/HEAD":                    "ref: refs/heads/main\n",
		"vendor/acme/owners/OWNERS":                "acme-team@google.com\n",
		"vendor/acme/pkg/OWNERS":                   "include /owners/OWNERS\n",
		"vendor/acme/third_party/OWNERS":           "atp@google.com\n",
		"vendor/acme/third_party/driver/BUILD.gn":  "# gn\n",
		"vendor/acme/third_party/driver/raw/src.c": "// skipped without BUILD file\n",
		"vendor/third_party/OWNERS":                "vtp@google.com\n",
		"vendor/third_party/codec/BUILD":           "# build\n",
		"vendor/third_party/codec/sub/helper.cc":   "// skipped without BUILD file\n",
	})
	if err := os.Symlink(
		filepath.Join(checkout, "third_party", "symlinked_lib", "real_build.gn"),
		filepath.Join(checkout, "third_party", "symlinked_lib", "BUILD.gn"),
	); err != nil {
		t.Fatalf("Symlink(BUILD.gn) failed: %v", err)
	}
	if err := os.Symlink(
		filepath.Join(checkout, "third_party", "nonregular", "sub"),
		filepath.Join(checkout, "third_party", "nonregular", "BUILD"),
	); err != nil {
		t.Fatalf("Symlink(nonregular BUILD) failed: %v", err)
	}

	outPath := filepath.Join(t.TempDir(), "nested", "directory_owners.json")
	var stderr bytes.Buffer
	if err := run([]string{"-fuchsia-dir", checkout, "-output", outPath}, &stderr); err != nil {
		t.Fatalf("run() unexpected error: %v (stderr=%q)", err, stderr.String())
	}

	raw, err := os.ReadFile(outPath)
	if err != nil {
		t.Fatalf("ReadFile(%q) failed: %v", outPath, err)
	}
	if len(raw) == 0 || raw[len(raw)-1] != '\n' {
		t.Errorf("run() output missing trailing newline")
	}

	var got snapshot
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("json.Unmarshal() failed: %v", err)
	}
	want := snapshot{
		Version: schemaVersion,
		Directories: map[string]directoryMetadata{
			"": {},
			"src": {
				Owners: []owners.Entry{
					{Email: "src-owner@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "src/OWNERS"},
				},
			},
			"src/sys": {
				ComponentID: 1478143,
				Owners: []owners.Entry{
					{Email: "sys-owner@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "src/sys/OWNERS"},
					{Email: "infra@google.com", Tier: 2, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "tools/team/OWNERS"},
					{Email: "src-owner@google.com", Tier: 3, OwnersStep: 2, IncludeDepth: 0, DirDistance: 1, OwnersFile: "src/OWNERS"},
				},
			},
			"tools": {},
			"tools/team": {
				Owners: []owners.Entry{
					{Email: "infra@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "tools/team/OWNERS"},
				},
			},
			"third_party/crate": {
				Owners: []owners.Entry{
					{Email: "tp-umbrella@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "third_party/OWNERS"},
				},
			},
			"third_party/lib_b": {
				Owners: []owners.Entry{
					{Email: "tp-umbrella@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "third_party/OWNERS"},
				},
			},
			"third_party/symlinked_lib": {
				Owners: []owners.Entry{
					{Email: "tp-umbrella@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "third_party/OWNERS"},
				},
			},
			"vendor":      {},
			"vendor/acme": {},
			"vendor/acme/owners": {
				Owners: []owners.Entry{
					{Email: "acme-team@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 0, OwnersFile: "vendor/acme/owners/OWNERS"},
				},
			},
			"vendor/acme/pkg": {
				Owners: []owners.Entry{
					{Email: "acme-team@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 1, DirDistance: 0, OwnersFile: "vendor/acme/owners/OWNERS"},
				},
			},
			"vendor/acme/third_party/driver": {
				Owners: []owners.Entry{
					{Email: "atp@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "vendor/acme/third_party/OWNERS"},
				},
			},
			"vendor/third_party/codec": {
				Owners: []owners.Entry{
					{Email: "vtp@google.com", Tier: 1, OwnersStep: 1, IncludeDepth: 0, DirDistance: 1, OwnersFile: "vendor/third_party/OWNERS"},
				},
			},
		},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("run() snapshot diff (-want +got):\n%s", diff)
	}

	// Empty root entry must omit both component_id and owners in raw JSON.
	var rawDoc struct {
		Version     int                        `json:"version"`
		Directories map[string]json.RawMessage `json:"directories"`
	}
	if err := json.Unmarshal(raw, &rawDoc); err != nil {
		t.Fatalf("json.Unmarshal(rawDoc) failed: %v", err)
	}
	if gotRoot := string(rawDoc.Directories[""]); gotRoot != "{}" {
		t.Errorf("run() raw directories[\"\"] = %s, want {}", gotRoot)
	}
}

func TestRun_RootMetadataInheritedWithoutRootOwners(t *testing.T) {
	checkout := t.TempDir()
	writeFiles(t, checkout, map[string]string{
		"OWNERS":             "global@google.com\n",
		"METADATA.textproto": "trackers: { issue_tracker: { component_id: 100 } }\n",
		"src/README.md":      "inherits root component\n",
	})
	snap, err := collect(checkout)
	if err != nil {
		t.Fatalf("collect(%q) unexpected error: %v", checkout, err)
	}
	want := &snapshot{
		Version: schemaVersion,
		Directories: map[string]directoryMetadata{
			"":    {ComponentID: 100},
			"src": {ComponentID: 100},
		},
	}
	if diff := cmp.Diff(want, snap); diff != "" {
		t.Errorf("collect() snapshot diff (-want +got):\n%s", diff)
	}
}

func TestRun_HelpSucceeds(t *testing.T) {
	var stderr bytes.Buffer
	if err := run([]string{"-help"}, &stderr); err != nil {
		t.Fatalf("run(-help) unexpected error: %v", err)
	}
	if stderr.Len() == 0 {
		t.Errorf("run(-help) wrote empty usage output")
	}
}

func TestRun_FlagErrors(t *testing.T) {
	dir := t.TempDir()
	out := filepath.Join(t.TempDir(), "out.json")
	tests := []struct {
		name string
		args []string
	}{
		{name: "missing_fuchsia_dir", args: []string{"-output", out}},
		{name: "missing_output", args: []string{"-fuchsia-dir", dir}},
		{name: "unexpected_positional_arg", args: []string{"-fuchsia-dir", dir, "-output", out, "extra"}},
		{name: "unknown_flag", args: []string{"-unknown"}},
		{name: "nonexistent_checkout", args: []string{"-fuchsia-dir", filepath.Join(dir, "missing"), "-output", out}},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			var stderr bytes.Buffer
			if err := run(tc.args, &stderr); err == nil {
				t.Errorf("run(%v) = nil error, want error", tc.args)
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
