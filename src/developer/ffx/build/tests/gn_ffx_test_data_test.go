// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package lib

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestGnFfxTestDataRuntimeLayout(t *testing.T) {
	info, err := os.Stat(DefaultFfxPath)
	if err != nil {
		t.Fatalf("expected %s to exist: %v", DefaultFfxPath, err)
	}
	if !info.Mode().IsRegular() || info.Size() == 0 {
		t.Fatalf("expected %s to be a non-empty regular file, got mode=%v size=%d", DefaultFfxPath, info.Mode(), info.Size())
	}
	if info.Mode()&0o111 == 0 {
		t.Fatalf("expected %s to be executable, got mode=%v", DefaultFfxPath, info.Mode())
	}

	subtoolSearchDir := filepath.Dir(DefaultFfxPath)
	subtools := []string{
		"ffx-component",
		"ffx-emu",
		"ffx-inspect",
		"ffx-log",
		"ffx-package",
		"ffx-product",
		"ffx-product-bundle",
		"ffx-repository",
	}

	for _, tool := range subtools {
		binPath := filepath.Join(subtoolSearchDir, tool)
		binInfo, err := os.Stat(binPath)
		if err != nil {
			t.Errorf("expected subtool binary %s to exist: %v", binPath, err)
			continue
		}
		if !binInfo.Mode().IsRegular() || binInfo.Size() == 0 {
			t.Errorf("expected %s to be a non-empty regular file, got mode=%v size=%d", binPath, binInfo.Mode(), binInfo.Size())
		}
		if binInfo.Mode()&0o111 == 0 {
			t.Errorf("expected %s to be executable, got mode=%v", binPath, binInfo.Mode())
		}

		metaPath := binPath + ".json"
		raw, err := os.ReadFile(metaPath)
		if err != nil {
			t.Errorf("expected subtool metadata %s to exist: %v", metaPath, err)
			continue
		}
		var meta map[string]any
		if err := json.Unmarshal(raw, &meta); err != nil {
			t.Errorf("expected %s to contain valid JSON: %v", metaPath, err)
			continue
		}
		if _, ok := meta["name"]; !ok {
			t.Errorf("expected %s JSON metadata to contain 'name', got %v", metaPath, meta)
		}
	}
}
