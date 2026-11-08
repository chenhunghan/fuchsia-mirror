// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"fmt"
	"os"
	"path/filepath"
)

func checkExecutable(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("stat %s: %w", path, err)
	}
	if !info.Mode().IsRegular() || info.Size() == 0 {
		return fmt.Errorf("expected %s to be a non-empty regular file (mode=%v size=%d)", path, info.Mode(), info.Size())
	}
	if info.Mode()&0o111 == 0 {
		return fmt.Errorf("expected %s to be executable (mode=%v)", path, info.Mode())
	}
	return nil
}

func checkNonEmpty(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("stat %s: %w", path, err)
	}
	if !info.Mode().IsRegular() || info.Size() == 0 {
		return fmt.Errorf("expected %s to be a non-empty regular file (mode=%v size=%d)", path, info.Mode(), info.Size())
	}
	return nil
}

func main() {
	allSubtools := []string{
		"ffx-component",
		"ffx-emu",
		"ffx-inspect",
		"ffx-log",
		"ffx-package",
		"ffx-product",
		"ffx-product-bundle",
		"ffx-repository",
	}

	checks := []struct {
		prefix   string
		subtools []string
	}{
		// host_test() stages every FuchsiaHostTestDataInfo file in the runtime
		// directory, regardless of whether the subtool comes from
		// gn_ffx_test_data() or ffx_tool().
		{prefix: ".", subtools: allSubtools},
		// Only gn_ffx_test_data() also exposes its files as DefaultInfo
		// runfiles; ffx_tool()'s host_test_data_files() does not. The loop's
		// host-tools/ffx check (from //src/developer/ffx:test_data, a
		// gn_ffx_test_data() target) is the whole runfiles check, so that
		// migrating a subtool to ffx_tool() does not require updating it.
		{prefix: "verify_runfiles_bin.runfiles/_main"},
	}

	for _, check := range checks {
		if err := checkExecutable(filepath.Join(check.prefix, "host-tools", "ffx")); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		for _, tool := range check.subtools {
			if err := checkExecutable(filepath.Join(check.prefix, "host-tools", tool)); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(1)
			}
			if err := checkNonEmpty(filepath.Join(check.prefix, "host-tools", tool+".json")); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(1)
			}
		}
	}
}
