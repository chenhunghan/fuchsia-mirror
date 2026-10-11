// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
)

func TestNewEmulator(t *testing.T) {
	for _, tc := range []struct {
		name       string
		emuType    string
		wantBinary string
		wantErr    bool
	}{{
		name:       "qemu",
		emuType:    "qemu",
		wantBinary: fmt.Sprintf("%s-%s", qemuSystemPrefix, qemuTargetMapping[TargetX64]),
	}, {
		name:       "aemu",
		emuType:    "aemu",
		wantBinary: aemuBinaryName,
	}, {
		name:       "crosvm",
		emuType:    "crosvm",
		wantBinary: "crosvm",
	}, {
		name:    "unknown",
		emuType: "unknown",
		wantErr: true,
	}} {
		t.Run(tc.name, func(t *testing.T) {
			ctx := context.Background()
			emu, err := NewEmulator(
				ctx,
				EmulatorConfig{
					Target: "x64",
				},
				Options{},
				tc.emuType,
			)
			if err != nil && !tc.wantErr {
				t.Fatalf("Unable to create NewEmulator: %s", err)
			} else if err == nil && tc.wantErr {
				t.Fatalf("expected err but got none")
			}

			if !tc.wantErr && emu.binary != tc.wantBinary {
				t.Errorf("Unexpected emu binary %s, expected %s", emu.binary, tc.wantBinary)
			}
		})
	}
}

func TestEmulatorStartDryRun(t *testing.T) {
	for _, emuType := range []string{"qemu", "crosvm"} {
		t.Run(emuType+"_dry_run_flag_does_not_start_vm", func(t *testing.T) {
			tmpDir := t.TempDir()
			ffxPath := filepath.Join(tmpDir, "ffx")
			if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
				t.Fatalf("failed to write mock ffx: %s", err)
			}
			ctx := context.Background()
			emu, err := NewEmulator(ctx, EmulatorConfig{
				Path:   tmpDir,
				Target: TargetX64,
			}, Options{}, emuType)
			if err != nil {
				t.Fatalf("NewEmulator failed: %s", err)
			}
			if err := os.WriteFile(filepath.Join(tmpDir, emu.binary), []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
				t.Fatalf("failed to write mock binary: %s", err)
			}
			ffx, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, DefaultEmulatorNodename, &ffxutil.SSHInfo{}, filepath.Join(tmpDir, "out"), ffxutil.UseFFXLegacy)
			if err != nil {
				t.Fatalf("NewFFXInstance failed: %s", err)
			}
			emu.SetFFX(&FFXInstance{FFXInstance: ffx}, nil)

			if err := emu.Start(ctx, []string{"-dry-run"}, "pb_path", false); err != nil {
				t.Fatalf("Start with -dry-run returned unexpected error: %s", err)
			}
			if emu.process != nil {
				t.Errorf("expected emu.process to be nil on -dry-run, but process was started with PID %d", emu.process.Pid)
			}
		})

		t.Run(emuType+"_dry_run_failure_fails_fast_without_starting_vm", func(t *testing.T) {
			tmpDir := t.TempDir()
			ffxPath := filepath.Join(tmpDir, "ffx")
			script := "#!/bin/bash\nif [[ \"$*\" == *\"--dry-run\"* ]]; then\n  exit 1\nfi\nexit 0\n"
			if err := os.WriteFile(ffxPath, []byte(script), 0o755); err != nil {
				t.Fatalf("failed to write mock ffx: %s", err)
			}
			ctx := context.Background()
			emu, err := NewEmulator(ctx, EmulatorConfig{
				Path:   tmpDir,
				Target: TargetX64,
			}, Options{}, emuType)
			if err != nil {
				t.Fatalf("NewEmulator failed: %s", err)
			}
			if err := os.WriteFile(filepath.Join(tmpDir, emu.binary), []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
				t.Fatalf("failed to write mock binary: %s", err)
			}
			ffx, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, DefaultEmulatorNodename, &ffxutil.SSHInfo{}, filepath.Join(tmpDir, "out"), ffxutil.UseFFXLegacy)
			if err != nil {
				t.Fatalf("NewFFXInstance failed: %s", err)
			}
			emu.SetFFX(&FFXInstance{FFXInstance: ffx}, nil)

			if err := emu.Start(ctx, nil, "pb_path", true); err == nil {
				t.Errorf("expected Start to fail when ffx emu start --dry-run fails, got nil")
			}
			if emu.process != nil {
				t.Errorf("expected emu.process to be nil when --dry-run fails, but process was started with PID %d", emu.process.Pid)
			}
		})
	}
}
