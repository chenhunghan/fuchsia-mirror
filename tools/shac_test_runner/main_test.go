// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"path/filepath"
	"testing"
)

func TestResolveRoot(t *testing.T) {
	tmp := t.TempDir()
	tests := []struct {
		name      string
		rootFlag  string
		runfiles  string
		testSrcs  string
		want      string
		wantError bool
	}{
		{
			name:     "flag_takes_precedence",
			rootFlag: filepath.Join(tmp, "root"),
			runfiles: filepath.Join(tmp, "runfiles"),
			want:     filepath.Join(tmp, "root"),
		},
		{
			name:     "RUNFILES_DIR",
			runfiles: filepath.Join(tmp, "runfiles"),
			testSrcs: filepath.Join(tmp, "srcdir"),
			want:     filepath.Join(tmp, "runfiles", "_main"),
		},
		{
			name:     "TEST_SRCDIR",
			testSrcs: filepath.Join(tmp, "srcdir"),
			want:     filepath.Join(tmp, "srcdir", "_main"),
		},
		{
			name:      "nothing_set",
			wantError: true,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Setenv("RUNFILES_DIR", tc.runfiles)
			t.Setenv("TEST_SRCDIR", tc.testSrcs)
			got, err := resolveRoot(tc.rootFlag)
			if tc.wantError {
				if err == nil {
					t.Errorf("resolveRoot(%q) = %q, want error", tc.rootFlag, got)
				}
				return
			}
			if err != nil {
				t.Fatalf("resolveRoot(%q) failed: %s", tc.rootFlag, err)
			}
			if got != tc.want {
				t.Errorf("resolveRoot(%q) = %q, want %q", tc.rootFlag, got, tc.want)
			}
		})
	}
}
