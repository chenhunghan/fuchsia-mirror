// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package artifacts

import (
	"testing"
)

func TestParseBlobFetchMode(t *testing.T) {
	for _, tc := range []struct {
		input       string
		expected    BlobFetchMode
		expectError bool
	}{
		{input: "unspecified", expected: Unspecified},
		{input: "UNSPECIFIED", expected: Unspecified},
		{input: "prefetch", expected: PrefetchBlobs},
		{input: "PREFETCH", expected: PrefetchBlobs},
		{input: "lazy", expected: LazilyFetchBlobs},
		{input: "LAZY", expected: LazilyFetchBlobs},
		{input: "invalid", expectError: true},
		{input: "", expectError: true},
	} {
		t.Run(tc.input, func(t *testing.T) {
			mode, err := ParseBlobFetchMode(tc.input)
			if tc.expectError {
				if err == nil {
					t.Fatalf("expected error for %q, got nil", tc.input)
				}
			} else {
				if err != nil {
					t.Fatalf("unexpected error for %q: %v", tc.input, err)
				}
				if mode != tc.expected {
					t.Fatalf("expected mode %v, got %v", tc.expected, mode)
				}
			}
		})
	}
}

func TestBlobFetchModeFlag(t *testing.T) {
	var mode BlobFetchMode
	if mode != Unspecified {
		t.Fatalf("expected default mode Unspecified, got %v", mode)
	}
	if mode.String() != "unspecified" {
		t.Fatalf("expected 'unspecified', got %q", mode.String())
	}

	if err := mode.Set("lazy"); err != nil {
		t.Fatalf("failed to set mode: %v", err)
	}
	if mode != LazilyFetchBlobs {
		t.Fatalf("expected LazilyFetchBlobs, got %v", mode)
	}
	if mode.String() != "lazy" {
		t.Fatalf("expected 'lazy', got %q", mode.String())
	}

	if err := mode.Set("prefetch"); err != nil {
		t.Fatalf("failed to set mode: %v", err)
	}
	if mode != PrefetchBlobs {
		t.Fatalf("expected PrefetchBlobs, got %v", mode)
	}
	if mode.String() != "prefetch" {
		t.Fatalf("expected 'prefetch', got %q", mode.String())
	}

	if err := mode.Set("unspecified"); err != nil {
		t.Fatalf("failed to set mode: %v", err)
	}
	if mode != Unspecified {
		t.Fatalf("expected Unspecified, got %v", mode)
	}
	if mode.String() != "unspecified" {
		t.Fatalf("expected 'unspecified', got %q", mode.String())
	}

	if err := mode.Set("unknown"); err == nil {
		t.Fatalf("expected error setting invalid mode, got nil")
	}
}
