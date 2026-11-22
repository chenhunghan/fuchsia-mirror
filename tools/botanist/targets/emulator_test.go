// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"syscall"
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

// newTestEmulator creates an emulator of emuType backed by mock emulator and
// ffx binaries, where ffxScript is the contents of the mock ffx binary.
func newTestEmulator(t *testing.T, emuType, ffxScript string) *Emulator {
	t.Helper()
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte(ffxScript), 0o755); err != nil {
		t.Fatalf("os.WriteFile(%q) failed: %s", ffxPath, err)
	}
	emu, err := NewEmulator(t.Context(), EmulatorConfig{
		Path:   tmpDir,
		Target: TargetX64,
	}, Options{}, emuType)
	if err != nil {
		t.Fatalf("NewEmulator() failed: %s", err)
	}
	if err := os.WriteFile(filepath.Join(tmpDir, emu.binary), []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
		t.Fatalf("os.WriteFile(emu binary) failed: %s", err)
	}
	sshKeys := ffxutil.SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := ffxutil.NewFFXInstance(t.Context(), ffxPath, tmpDir, nil, DefaultEmulatorNodename, &sshKeys, filepath.Join(tmpDir, "out"), ffxutil.UseFFXLegacy)
	if err != nil {
		t.Fatalf("NewFFXInstance() failed: %s", err)
	}
	emu.SetFFX(&FFXInstance{FFXInstance: ffx}, nil)
	return emu
}

func TestEmulatorStartDryRun(t *testing.T) {
	for _, emuType := range []string{"qemu", "crosvm"} {
		t.Run(emuType+"_dry_run_flag_does_not_start_vm", func(t *testing.T) {
			emu := newTestEmulator(t, emuType, "#!/bin/bash\nexit 0\n")
			if err := emu.Start(t.Context(), []string{"-dry-run"}, "pb_path", false); err != nil {
				t.Fatalf("Start with -dry-run returned unexpected error: %s", err)
			}
			if emu.process != nil {
				t.Errorf("expected emu.process to be nil on -dry-run, but process was started with PID %d", emu.process.Pid)
			}
		})

		t.Run(emuType+"_dry_run_failure_fails_fast_without_starting_vm", func(t *testing.T) {
			script := "#!/bin/bash\nif [[ \"$*\" == *\"--dry-run\"* ]]; then\n  exit 1\nfi\nexit 0\n"
			emu := newTestEmulator(t, emuType, script)
			if err := emu.Start(t.Context(), nil, "pb_path", true); err == nil {
				t.Errorf("expected Start to fail when ffx emu start --dry-run fails, got nil")
			}
			if emu.process != nil {
				t.Errorf("expected emu.process to be nil when --dry-run fails, but process was started with PID %d", emu.process.Pid)
			}
		})
	}
}
func TestNewEmulatorSerialAssignsMasterAndSlave(t *testing.T) {
	tmpDir := t.TempDir()
	wantMaster, err := os.CreateTemp(tmpDir, "master")
	if err != nil {
		t.Fatalf("failed to create fake master: %s", err)
	}
	defer wantMaster.Close()
	wantSlave, err := os.CreateTemp(tmpDir, "slave")
	if err != nil {
		t.Fatalf("failed to create fake slave: %s", err)
	}
	defer wantSlave.Close()

	origOpenPTY := openPTY
	t.Cleanup(func() { openPTY = origOpenPTY })
	openPTY = func() (*os.File, *os.File, error) {
		return wantMaster, wantSlave, nil
	}

	emu, err := NewEmulator(context.Background(), EmulatorConfig{
		Target: TargetX64,
		Serial: true,
	}, Options{}, "aemu")
	if err != nil {
		t.Fatalf("NewEmulator failed: %s", err)
	}
	if emu.Serial() != wantMaster {
		t.Errorf("emu.Serial() = %v, want master (%v)", emu.Serial(), wantMaster)
	}
	if emu.pts != wantSlave {
		t.Errorf("emu.pts = %v, want slave (%v)", emu.pts, wantSlave)
	}
}

func TestEmulatorSerialDoesNotEchoStdoutToStdin(t *testing.T) {
	tmpDir := t.TempDir()
	stdinCapturePath := filepath.Join(tmpDir, "stdin_capture.txt")
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := fmt.Sprintf(`#!/bin/bash
if [[ "$*" == *"config set"* ]] || [[ "$*" == *"--dry-run"* ]]; then
  exit 0
fi
echo "INFO | Feature 'HostComposition' (62) is overridden to 'enabled'"
read -r line
printf "%%s" "$line" > %q
`, stdinCapturePath)
	if err := os.WriteFile(ffxPath, []byte(script), 0o755); err != nil {
		t.Fatalf("failed to write mock ffx: %s", err)
	}

	ctx := context.Background()
	emu, err := NewEmulator(ctx, EmulatorConfig{
		Path:   tmpDir,
		Target: TargetX64,
		Serial: true,
	}, Options{}, "aemu")
	if err != nil {
		if errors.Is(err, os.ErrNotExist) || errors.Is(err, syscall.ENOENT) || errors.Is(err, syscall.EACCES) || errors.Is(err, syscall.EPERM) || errors.Is(err, syscall.ENXIO) {
			t.Skipf("skipping test: pty unavailable in environment: %v", err)
		}
		t.Fatalf("NewEmulator failed: %s", err)
	}
	defer emu.Serial().Close()
	defer emu.pts.Close()

	if err := os.WriteFile(filepath.Join(tmpDir, emu.binary), []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
		t.Fatalf("failed to write mock binary: %s", err)
	}
	ffx, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, DefaultEmulatorNodename, &ffxutil.SSHInfo{}, filepath.Join(tmpDir, "out"), ffxutil.UseFFXLegacy)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}
	emu.SetFFX(&FFXInstance{FFXInstance: ffx}, nil)

	if err := emu.Start(ctx, nil, "pb_path", false); err != nil {
		t.Fatalf("Start returned unexpected error: %s", err)
	}

	serialOut := make(chan string, 1)
	go func() {
		buf := make([]byte, 256)
		var out strings.Builder
		for {
			n, err := emu.Serial().Read(buf)
			if n > 0 {
				out.Write(buf[:n])
				if strings.Contains(out.String(), "HostComposition") {
					_, _ = io.WriteString(emu.Serial(), "runtests\n")
					serialOut <- out.String()
					return
				}
			}
			if err != nil {
				serialOut <- out.String()
				return
			}
		}
	}()

	gotSerial := <-serialOut
	if !strings.Contains(gotSerial, "HostComposition") {
		t.Fatalf("expected serial output to contain emulator stdout, got %q", gotSerial)
	}

	if err := emu.Wait(ctx); err != nil {
		t.Fatalf("Wait returned unexpected error: %s", err)
	}

	gotStdinBytes, err := os.ReadFile(stdinCapturePath)
	if err != nil {
		t.Fatalf("failed to read captured stdin: %s", err)
	}
	if gotStdin := string(gotStdinBytes); gotStdin != "runtests" {
		t.Errorf("emulator stdin received %q, want %q", gotStdin, "runtests")
	}
}
func TestEmulatorStartClearsSerialNumber(t *testing.T) {
	emu := newTestEmulator(t, "qemu", "#!/bin/bash\nexit 0\n")
	emu.SetSerialNumber("EM-CAFEBABE")

	if err := emu.Start(t.Context(), []string{"-dry-run"}, "pb_path", false); err != nil {
		t.Fatalf("emu.Start(-dry-run) failed: %s", err)
	}
	if got := emu.SerialNumber(); got != "" {
		t.Errorf("SerialNumber() after Start() = %q, want empty", got)
	}
}
