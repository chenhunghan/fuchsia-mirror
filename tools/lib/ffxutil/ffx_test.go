// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffxutil

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/clock"
)

func TestFFXInstance(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), os.ModePerm); err != nil {
		t.Fatal("failed to write mock ffx tool")
	}
	fakeClock := clock.NewFakeClock()
	ctx := clock.NewContext(context.Background(), fakeClock)
	sshPriv := filepath.Join(tmpDir, "privKey")
	sshPub := filepath.Join(tmpDir, "pubKey")
	sshKeys := SSHInfo{SshPriv: sshPriv, SshPub: sshPub}
	ffx, _ := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "target", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXLegacy)

	var buf []byte
	stdout := bytes.NewBuffer(buf)
	ffx.SetStdoutStderr(stdout, stdout)

	assertRunsExpectedCmd := func(runErr error, stdout *bytes.Buffer, expectedCmd string) {
		if runErr != nil {
			t.Errorf("failed to run cmd: %s", runErr)
		}
		stdoutStr := stdout.String()
		if !strings.HasSuffix(strings.TrimSpace(stdoutStr), expectedCmd) {
			t.Errorf("got %q, want %q", stdoutStr, expectedCmd)
		}
	}
	assertRunsExpectedCmd(ffx.List(ctx), stdout, "target list")

	assertRunsExpectedCmd(ffx.TargetWait(ctx), stdout, "--target target --config log.level=debug target wait")

	assertRunsExpectedCmd(ffx.TargetWait(ctx, "-t", "90"), stdout, "--target target --config log.level=debug target wait -t 90")

	// Create a new instance that uses the same ffx config but runs against a different target.
	ffx2 := FFXWithTarget(ffx, "target2")
	var buf2 []byte
	stdout2 := bytes.NewBuffer(buf2)
	ffx2.SetStdoutStderr(stdout2, stdout2)
	assertRunsExpectedCmd(ffx2.TargetWait(ctx), stdout2, "--target target2 --config log.level=debug target wait")

	// Test expects a run_summary.json to be written in the test output directory.
	outDir := filepath.Join(tmpDir, "out")
	testOutputDir := filepath.Join(outDir, "test-outputs")
	if err := os.MkdirAll(testOutputDir, os.ModePerm); err != nil {
		t.Errorf("failed to create test outputs dir: %s", err)
	}
	runSummaryBytes := []byte("{\"schema_id\": \"https://fuchsia.dev/schema/ffx_test/run_summary-8d1dd964.json\"}")
	if err := os.WriteFile(filepath.Join(testOutputDir, runSummaryFilename), runSummaryBytes, os.ModePerm); err != nil {
		t.Errorf("failed to write run_summary.json: %s", err)
	}
	_, err := ffx.TestRun(ctx, build.TestList{}, outDir)
	assertRunsExpectedCmd(
		err,
		stdout,
		fmt.Sprintf(
			"--target target test run --continue-on-timeout --test-file %s --output-directory %s --show-full-moniker-in-logs",
			filepath.Join(outDir, "test-list.json"), testOutputDir,
		),
	)

	// Snapshot expects a file to be written to tmpDir/snapshotZipName which it will move to tmpDir/new_snapshot.zip.
	if err := os.WriteFile(filepath.Join(tmpDir, snapshotZipName), []byte("snapshot"), os.ModePerm); err != nil {
		t.Errorf("failed to write snapshot")
	}
	assertRunsExpectedCmd(ffx.Snapshot(ctx, tmpDir, "new_snapshot.zip"), stdout, "--target target target snapshot --dir "+tmpDir)
	if _, err := os.Stat(filepath.Join(tmpDir, snapshotZipName)); err == nil {
		t.Errorf("expected snapshot to be renamed")
	}
	if _, err := os.Stat(filepath.Join(tmpDir, "new_snapshot.zip")); err != nil {
		t.Errorf("failed to rename snapshot to new_snapshot.zip: %s", err)
	}

	assertRunsExpectedCmd(ffx.GetConfig(ctx), stdout, "config get")

	assertRunsExpectedCmd(ffx.Run(ctx, "random", "cmd", "with", "args"), stdout, "random cmd with args")

	assertRunsExpectedCmd(ffx.RunWithTarget(ctx, "random", "cmd", "with", "args"), stdout, "--target target random cmd with args")

	assertRunsExpectedCmd(ffx.StartFFXMonitor(ctx, "8080", "log.json", "aggregations.json"), stdout, "monitor start --nodename target --port 8080 --no-usb --log-file log.json --aggregations-file aggregations.json")

	assertRunsExpectedCmd(ffx.StartFFXMonitor(ctx, "8081", "", ""), stdout, "monitor start --nodename target --port 8081 --no-usb")

	if err := ffx.Stop(); err != nil {
		t.Errorf("ffx.Stop() = %s", err)
	}

	fPrivKey := ffx.GetSshPrivateKey()
	if sshPriv != fPrivKey {
		t.Errorf("got wrong private key: %s (expected %s)", fPrivKey, sshPriv)
	}
	fPubKey := ffx.GetSshAuthorizedKeys()
	if sshPub != fPubKey {
		t.Errorf("got wrong private key: %s (expected %s)", fPubKey, sshPub)
	}
}

func TestFFXWithTargetStrictConfigIsolation(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), os.ModePerm); err != nil {
		t.Fatal("failed to write mock ffx tool")
	}
	ctx := context.Background()
	sshPriv := filepath.Join(tmpDir, "privKey")
	sshPub := filepath.Join(tmpDir, "pubKey")
	sshKeys := SSHInfo{SshPriv: sshPriv, SshPub: sshPub}
	ffx, err := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "target1", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXStrict)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}
	if err := ffx.ConfigSet(ctx, "parent.key", "parent.val"); err != nil {
		t.Fatalf("ConfigSet failed: %s", err)
	}

	ffxCopy := FFXWithTarget(ffx, "target2")
	if err := ffxCopy.ConfigSet(ctx, "ssh.controlmaster.mode", "explicit"); err != nil {
		t.Fatalf("ConfigSet on copy failed: %s", err)
	}
	if err := ffxCopy.ConfigSet(ctx, "ssh.controlmaster.path", "/tmp/cm.sock"); err != nil {
		t.Fatalf("ConfigSet on copy failed: %s", err)
	}
	if err := ffx.ConfigSet(ctx, "after.copy.key", "after.copy.val"); err != nil {
		t.Fatalf("ConfigSet on original failed: %s", err)
	}

	origArgs := strings.Join(ffx.Command("monitor", "start").Args, " ")
	copyArgs := strings.Join(ffxCopy.Command("test", "run").Args, " ")

	if !strings.Contains(origArgs, "-c parent.key=parent.val") {
		t.Errorf("expected original ffx args to contain -c parent.key=parent.val, got %q", origArgs)
	}
	if !strings.Contains(origArgs, "-c after.copy.key=after.copy.val") {
		t.Errorf("expected original ffx args to contain -c after.copy.key=after.copy.val, got %q", origArgs)
	}
	if strings.Contains(origArgs, "ssh.controlmaster.mode=explicit") {
		t.Errorf("expected original ffx args not to contain ssh.controlmaster.mode=explicit, got %q", origArgs)
	}
	if strings.Contains(origArgs, "ssh.controlmaster.path=/tmp/cm.sock") {
		t.Errorf("expected original ffx args not to contain ssh.controlmaster.path=/tmp/cm.sock, got %q", origArgs)
	}

	if !strings.Contains(copyArgs, "-c parent.key=parent.val") {
		t.Errorf("expected copied ffx args to inherit -c parent.key=parent.val, got %q", copyArgs)
	}
	if !strings.Contains(copyArgs, "-c ssh.controlmaster.mode=explicit") {
		t.Errorf("expected copied ffx args to contain -c ssh.controlmaster.mode=explicit, got %q", copyArgs)
	}
	if !strings.Contains(copyArgs, "-c ssh.controlmaster.path=/tmp/cm.sock") {
		t.Errorf("expected copied ffx args to contain -c ssh.controlmaster.path=/tmp/cm.sock, got %q", copyArgs)
	}
	if strings.Contains(copyArgs, "after.copy.key=after.copy.val") {
		t.Errorf("expected copied ffx args not to contain after.copy.key=after.copy.val, got %q", copyArgs)
	}
}

func TestLegacyFfxCmdBuilderClone(t *testing.T) {
	tmpDir := t.TempDir()
	isolateDir := filepath.Join(tmpDir, "isolate")
	configDir := filepath.Join(tmpDir, "config")
	if err := os.MkdirAll(configDir, 0o755); err != nil {
		t.Fatalf("os.MkdirAll(%q) failed: %v", configDir, err)
	}

	orig := newLegacyFfxCmdBuilder(isolateDir, configDir)
	clonedBuilder := orig.clone()
	cloned, ok := clonedBuilder.(*legacyFfxCmdBuilder)
	if !ok {
		t.Fatalf("clone() returned %T, want *legacyFfxCmdBuilder", clonedBuilder)
	}
	if cloned == orig {
		t.Fatal("clone() returned the same *legacyFfxCmdBuilder pointer, want a distinct copy")
	}
	if cloned.isStrict() {
		t.Error("cloned.isStrict() = true, want false")
	}
	wantEnv := []string{fmt.Sprintf("%s=%s", FFXIsolateDirEnvKey, isolateDir)}
	if gotEnv := cloned.env(); !slices.Equal(gotEnv, wantEnv) {
		t.Errorf("cloned.env() = %v, want %v", gotEnv, wantEnv)
	}
	wantCmd := []string{"/bin/ffx", "--isolate-dir", isolateDir, "-c", "k=v", "target", "list"}
	if gotCmd := cloned.commandWithConfigs("/bin/ffx", []string{"target", "list"}, map[string]any{"k": "v"}); !slices.Equal(gotCmd, wantCmd) {
		t.Errorf("cloned.commandWithConfigs() = %v, want %v", gotCmd, wantCmd)
	}
	if err := cloned.setConfigMap(map[string]any{"user.key": "uval"}, map[string]any{"global.key": "gval"}); err != nil {
		t.Fatalf("cloned.setConfigMap() failed: %v", err)
	}
	for _, name := range []string{ffxEnvFilename, "user_config.json", "global_config.json"} {
		path := filepath.Join(configDir, name)
		if _, err := os.Stat(path); err != nil {
			t.Errorf("expected cloned.setConfigMap() to write %q: %v", path, err)
		}
	}

	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), 0o755); err != nil {
		t.Fatalf("os.WriteFile(%q) failed: %v", ffxPath, err)
	}
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(t.Context(), ffxPath, tmpDir, nil, "target1", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXLegacy)
	if err != nil {
		t.Fatalf("NewFFXInstance() failed: %v", err)
	}
	ffxCopy := FFXWithTarget(ffx, "target2")
	if ffxCopy.cmdBuilder == ffx.cmdBuilder {
		t.Error("FFXWithTarget() shared legacyFfxCmdBuilder pointer, want distinct clone")
	}
	if ffxCopy.isStrict() {
		t.Error("ffxCopy.isStrict() = true, want false")
	}
	if !slices.Equal(ffxCopy.Command("target", "list").Args, ffx.Command("target", "list").Args) {
		t.Errorf("ffxCopy.Command() args = %v, want %v", ffxCopy.Command("target", "list").Args, ffx.Command("target", "list").Args)
	}
}

func TestStrictFfxCmdBuilderCloneDefensiveCopy(t *testing.T) {
	orig := newStrictFfxCmdBuilder("/tmp/ffx.log")
	if err := orig.setConfigMap(
		map[string]any{
			"scalar":    "v1",
			"str_slice": []string{"a", "b"},
			"nil_slice": []string(nil),
		},
		map[string]any{
			"str_map": map[string]string{"k": "v"},
			"nil_map": map[string]any(nil),
			"nested": map[string]any{
				"items": []any{"x", map[string]any{"inner": "orig"}},
			},
		},
	); err != nil {
		t.Fatalf("setConfigMap() failed: %v", err)
	}

	clonedBuilder := orig.clone()
	cloned, ok := clonedBuilder.(*strictFfxCmdBuilder)
	if !ok {
		t.Fatalf("clone() returned %T, want *strictFfxCmdBuilder", clonedBuilder)
	}
	if cloned == orig {
		t.Fatal("clone() returned the same *strictFfxCmdBuilder pointer, want a distinct copy")
	}
	if cloned.outputFile != orig.outputFile {
		t.Errorf("cloned.outputFile = %q, want %q", cloned.outputFile, orig.outputFile)
	}

	// Mutate slices and maps stored in orig and verify cloned is unaffected.
	origCfg := orig.getConfigMap()
	origCfg["str_slice"].([]string)[0] = "mutated"
	origCfg["str_map"].(map[string]string)["k"] = "mutated"
	origItems := origCfg["nested"].(map[string]any)["items"].([]any)
	origItems[0] = "mutated"
	origItems[1].(map[string]any)["inner"] = "mutated"

	clonedCfg := cloned.getConfigMap()
	if got := clonedCfg["scalar"]; got != "v1" {
		t.Errorf("cloned scalar = %v, want %q", got, "v1")
	}
	if got := clonedCfg["str_slice"].([]string); !slices.Equal(got, []string{"a", "b"}) {
		t.Errorf("cloned str_slice = %v, want [a b]", got)
	}
	if got := clonedCfg["nil_slice"].([]string); got != nil {
		t.Errorf("cloned nil_slice = %#v, want nil", got)
	}
	if got := clonedCfg["str_map"].(map[string]string); !maps.Equal(got, map[string]string{"k": "v"}) {
		t.Errorf("cloned str_map = %v, want map[k:v]", got)
	}
	if got := clonedCfg["nil_map"].(map[string]any); got != nil {
		t.Errorf("cloned nil_map = %#v, want nil", got)
	}
	clonedItems := clonedCfg["nested"].(map[string]any)["items"].([]any)
	if clonedItems[0] != "x" {
		t.Errorf("cloned nested items[0] = %v, want %q", clonedItems[0], "x")
	}
	if gotInner := clonedItems[1].(map[string]any)["inner"]; gotInner != "orig" {
		t.Errorf("cloned nested inner = %v, want %q", gotInner, "orig")
	}
}

func TestFFXPBArtifacts(t *testing.T) {
	for _, testcase := range []struct {
		name      string
		output    string
		errOutput string
		exitCode  int
		wantPaths []string
		wantError error
	}{
		{
			name:      "OK paths",
			output:    `{"ok": {"paths": [ "pb1.txt", "pb2.txt"]}}`,
			errOutput: "",
			exitCode:  0,
			wantPaths: []string{"pb1.txt", "pb2.txt"},
			wantError: nil,
		},
		{
			name:      "pb not found paths",
			output:    `{"user_error": {"message": "path not found"}}`,
			errOutput: "path not found",
			exitCode:  1,
			wantPaths: []string{},
			wantError: fmt.Errorf("user error: %s", "path not found"),
		},
		{
			name:      "pb not found paths",
			output:    `{"unexpected_error": {"message": "somthing went wrong"}}`,
			errOutput: "exception processing metadata",
			exitCode:  1,
			wantPaths: []string{},
			wantError: fmt.Errorf("unexpected error: somthing went wrong"),
		},
	} {
		t.Run(testcase.name, func(t *testing.T) {
			var testErr error = nil
			if testcase.exitCode != 0 {
				testErr = fmt.Errorf("Exit Code %d: %s", testcase.exitCode, testcase.errOutput)
			}
			paths, err := processPBArtifactsResult(testcase.output, testErr)

			if err != nil {
				if testcase.wantError != nil {
					if err.Error() != testcase.wantError.Error() {
						t.Errorf("Got error %q wanted error: %q", err, testcase.wantError)
					}
				} else if err != nil {
					t.Errorf("Test error: %s", err)
				}
			}

			if len(paths) != len(testcase.wantPaths) {
				t.Errorf("Length mismatch Got  %v want %v", paths, testcase.wantPaths)
			}
			for i := range paths {
				if paths[i] != testcase.wantPaths[i] {
					t.Errorf("mismatch index %d. Got  %v want %v", i, paths, testcase.wantPaths)
				}
			}
		})
	}
}

func TestEmuStartConsoleDryRunError(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := "#!/bin/bash\nif [[ \"$*\" == *\"--dry-run\"* ]]; then\n  echo '{\"user_error\":{\"message\":\"There are no virtual devices configured for this product bundle\"}}' >&2\n  exit 1\nfi\necho \"$@\"\n"
	if err := os.WriteFile(ffxPath, []byte(script), os.ModePerm); err != nil {
		t.Fatalf("failed to write mock ffx tool: %s", err)
	}
	ctx := context.Background()
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "target", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXLegacy)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}
	cmd, err := ffx.EmuStartConsole(ctx, tmpDir, "fuchsia-emulator", EmuTools{}, EmuStartArgs{
		Engine:        "qemu",
		ProductBundle: filepath.Join(tmpDir, "pb"),
	})
	if err == nil {
		t.Errorf("expected EmuStartConsole to fail when --dry-run fails, got nil error and cmd %v", cmd)
	}
}

func TestLegacyFfxCmdBuilderDeterministicConfigs(t *testing.T) {
	builder := newLegacyFfxCmdBuilder("/tmp/isolate", "/tmp/config")
	got := builder.commandWithConfigs("/bin/ffx", []string{"target", "show"}, map[string]any{
		"z.key": "zval",
		"a.key": "aval",
		"m.key": "mval",
	})
	want := []string{
		"/bin/ffx", "--isolate-dir", "/tmp/isolate",
		"-c", "a.key=aval",
		"-c", "m.key=mval",
		"-c", "z.key=zval",
		"target", "show",
	}
	if !slices.Equal(got, want) {
		t.Errorf("commandWithConfigs() = %v, want %v", got, want)
	}
}

func TestStrictFfxCmdBuilderDeterministicConfigs(t *testing.T) {
	builder := newStrictFfxCmdBuilder("/tmp/ffx.log")
	if err := builder.setConfigMap(
		map[string]any{"user.key": "uval", "shared.key": "from_user"},
		map[string]any{"z.key": "zval", "a.key": "aval", "shared.key": "from_global"},
	); err != nil {
		t.Fatalf("setConfigMap failed: %s", err)
	}
	got := builder.commandWithConfigs("/bin/ffx", []string{"target", "show"}, map[string]any{
		"m.key":      "mval",
		"shared.key": "from_call",
	})
	want := []string{
		"/bin/ffx", "-o", "/tmp/ffx.log", "--strict", "--machine", "json",
		"-c", "a.key=aval",
		"-c", "m.key=mval",
		"-c", "shared.key=from_call",
		"-c", "user.key=uval",
		"-c", "z.key=zval",
		"target", "show",
	}
	if !slices.Equal(got, want) {
		t.Errorf("commandWithConfigs() = %v, want %v", got, want)
	}
}

func TestFFXInvokerErrorOmitsConfigAndOutputPaths(t *testing.T) {
	script := "#!/bin/bash\nif [[ \"$*\" == *\"check-ssh-keys\"* ]]; then\n  exit 0\nfi\nexit 1\n"
	ffx, tmpDir := newStrictTestFFX(t, script, "fuchsia-54b2-030e-eb1c")
	ctx := t.Context()

	waitErr := ffx.TargetWait(ctx, "-t", "90")
	if waitErr == nil {
		t.Fatal("expected TargetWait to fail")
	}
	wantWaitPrefix := "FFX command failed (ffx --config log.level=debug target wait -t 90):"
	if !strings.HasPrefix(waitErr.Error(), wantWaitPrefix) {
		t.Errorf("TargetWait error = %q, want prefix %q", waitErr.Error(), wantWaitPrefix)
	}
	if strings.Contains(waitErr.Error(), tmpDir) || strings.Contains(waitErr.Error(), "fuchsia-54b2-030e-eb1c") {
		t.Errorf("TargetWait error should not contain temp paths or target nodename, got %q", waitErr.Error())
	}

	_, rebootErr := ffx.GetLastRebootReason(ctx)
	if rebootErr == nil {
		t.Fatal("expected GetLastRebootReason to fail")
	}
	wantRebootSubstr := "ffx target show failed: FFX command failed (ffx --machine json target show):"
	if !strings.Contains(rebootErr.Error(), wantRebootSubstr) {
		t.Errorf("GetLastRebootReason error = %q, want substring %q", rebootErr.Error(), wantRebootSubstr)
	}
	if strings.Contains(rebootErr.Error(), tmpDir) || strings.Contains(rebootErr.Error(), "fuchsia-54b2-030e-eb1c") {
		t.Errorf("GetLastRebootReason error should not contain temp paths or target nodename, got %q", rebootErr.Error())
	}
}

func TestExtractFFXMachineError(t *testing.T) {
	for _, tc := range []struct {
		name   string
		output string
		target string
		want   string
	}{
		{
			name:   "fastboot partition not big enough with NUL byte and Caused by",
			output: "{\"progress\":{\"stage\":\"boot_a\"}}\n{\"type\":\"unexpected\",\"code\":1,\"message\":\"Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough\\u0000\\n\\nCaused by:\\n    0: Failed to stage file\\n    1: Failed to upload boot_a\\n    2: failed to flash partition (boot_a): not big enough\\u0000\"}\n",
			target: "id:1A2B3C4D",
			want:   "Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough",
		},
		{
			name:   "target specification not found scrubs dynamic serial",
			output: `{"type":"user","code":1,"message":"Target specification \"id:11271J3DA017M8\" was not found. Use ` + "`ffx target list`" + ` to list known targets, and use a different matcher."}`,
			target: "id:11271J3DA017M8",
			want:   `Target specification "<target>" was not found. Use ` + "`ffx target list`" + ` to list known targets, and use a different matcher.`,
		},
		{
			name:   "fastboot write timeout",
			output: `{"type":"unexpected","code":1,"message":"Fastboot interface error: Fastboot send error: timed out writing to fastboot interface: Custom { kind: TimedOut, error: \"timed out\" }\n\nCaused by:\n    0: timed out"}`,
			target: "id:4B121JEAYW01WG",
			want:   `Fastboot interface error: Fastboot send error: timed out writing to fastboot interface: Custom { kind: TimedOut, error: "timed out" }`,
		},
		{
			name:   "finished envelope with BUG header",
			output: `{"finished":{"success":false,"error_message":"BUG: An internal command error occurred.\nError: Fastboot interface error: Fastboot error: Upload error: Could not write to interface\n    1. Fastboot error"}}`,
			target: "id:4B121JEAYW01WG",
			want:   "Fastboot interface error: Fastboot error: Upload error: Could not write to interface",
		},
		{
			name:   "bare serial stripped of id prefix is scrubbed",
			output: `{"type":"unexpected","code":1,"message":"Fastboot interface error: USB Interface creation error for serial 4B121JEAYW01WG: Resource busy (os error 16)"}`,
			target: "id:4B121JEAYW01WG",
			want:   "Fastboot interface error: USB Interface creation error for serial <target>: Resource busy (os error 16)",
		},
		{
			name:   "config error type is surfaced",
			output: `{"type":"config","code":1,"message":"Configuration error: missing required config value"}`,
			target: "id:4B121JEAYW01WG",
			want:   "Configuration error: missing required config value",
		},
		{
			name:   "non-error progress only returns empty",
			output: `{"type":"progress","message":"uploading boot_a"}`,
			target: "id:1A2B3C4D",
			want:   "",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := extractFFXMachineError(tc.output, tc.target); got != tc.want {
				t.Errorf("extractFFXMachineError() = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestFFXInvokerSurfacesMachineJSONError(t *testing.T) {
	script := "#!/bin/bash\nif [[ \"$*\" == *\"check-ssh-keys\"* ]]; then\n  exit 0\nfi\necho '{\"type\":\"unexpected\",\"code\":1,\"message\":\"Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough\\u0000\\n\\nCaused by:\\n    0: Failed to stage file\"}'\nexit 1\n"
	ffx, _ := newStrictTestFFX(t, script, "id:11271J3DA017M8")
	ctx := t.Context()

	flashErr := ffx.Flash(ctx, "id:11271J3DA017M8", "", "product_bundles.json", false)
	if flashErr == nil {
		t.Fatal("expected Flash to fail")
	}
	wantSubstr := "FFX command failed (ffx -v --machine json target flash --product-bundle product_bundles.json): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if flashErr.Error() != wantSubstr {
		t.Errorf("Flash error = %q, want %q", flashErr.Error(), wantSubstr)
	}

	bootErr := ffx.BootloaderBoot(ctx, "id:11271J3DA017M8", "product_bundles.json", false)
	if bootErr == nil {
		t.Fatal("expected BootloaderBoot to fail")
	}
	wantBoot := "FFX command failed (ffx target bootloader --product-bundle product_bundles.json boot): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if bootErr.Error() != wantBoot {
		t.Errorf("BootloaderBoot error = %q, want %q", bootErr.Error(), wantBoot)
	}

	waitErr := ffx.TargetWait(ctx, "-t", "90")
	if waitErr == nil {
		t.Fatal("expected TargetWait to fail")
	}
	wantWait := "FFX command failed (ffx --config log.level=debug target wait -t 90): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if waitErr.Error() != wantWait {
		t.Errorf("TargetWait error = %q, want %q", waitErr.Error(), wantWait)
	}
}

// newStrictTestFFX returns a strict-mode FFXInstance for target backed by a
// mock ffx binary with the given script, along with its temp directory.
func newStrictTestFFX(t *testing.T, script, target string) (*FFXInstance, string) {
	t.Helper()
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte(script), 0o755); err != nil {
		t.Fatalf("os.WriteFile(%q) failed: %v", ffxPath, err)
	}
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(t.Context(), ffxPath, tmpDir, nil, target, &sshKeys, filepath.Join(tmpDir, "out"), UseFFXStrict)
	if err != nil {
		t.Fatalf("NewFFXInstance() failed: %v", err)
	}
	return ffx, tmpDir
}

func TestLoadRegisteredTargets_MissingFile(t *testing.T) {
	ffx := &FFXInstance{sharedData: t.TempDir()}
	if _, err := ffx.LoadRegisteredTargets(); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("LoadRegisteredTargets() error = %v, want os.ErrNotExist", err)
	}
}

func TestRegisterTargets_NilSliceAndAtomicCleanup(t *testing.T) {
	ffx := &FFXInstance{sharedData: filepath.Join(t.TempDir(), "shared")}
	if err := ffx.RegisterTargets(nil); err != nil {
		t.Fatalf("RegisterTargets(nil) failed: %v", err)
	}
	path, _ := ffx.registeredTargetsPath()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("os.ReadFile(%q) failed: %v", path, err)
	}
	if got := strings.TrimSpace(string(raw)); got != "[]" {
		t.Errorf("RegisterTargets(nil) wrote %q, want %q", got, "[]")
	}
	if entries, err := os.ReadDir(ffx.SharedData()); err != nil || len(entries) != 1 || entries[0].Name() != registeredTargetsFilename {
		t.Errorf("os.ReadDir(%q) = (%v, %v), want only %q", ffx.SharedData(), entries, err, registeredTargetsFilename)
	}
}

func TestRegisterTargets_InvalidEntriesRejected(t *testing.T) {
	for _, tc := range []struct {
		name   string
		target RegisteredTarget
	}{
		{
			name:   "empty_address",
			target: RegisteredTarget{Nodename: "fuchsia-node-1", ID: "SER001"},
		},
		{
			name:   "no_nodename_or_id",
			target: RegisteredTarget{Address: "192.168.42.10"},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			ffx := &FFXInstance{sharedData: filepath.Join(t.TempDir(), "shared")}
			valid := RegisteredTarget{Nodename: "fuchsia-peer", Address: "192.168.42.50"}
			if err := ffx.RegisterTargets([]RegisteredTarget{valid, tc.target}); err == nil {
				t.Errorf("RegisterTargets(%+v) = nil, want non-nil error", tc.target)
			}
			if err := ffx.UpsertRegisteredTarget(tc.target); err == nil {
				t.Errorf("UpsertRegisteredTarget(%+v) = nil, want non-nil error", tc.target)
			}
			path, _ := ffx.registeredTargetsPath()
			if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
				t.Errorf("os.Stat(%q) error = %v, want os.ErrNotExist", path, err)
			}
		})
	}
}

func TestUpsertRegisteredTargets(t *testing.T) {
	for _, tc := range []struct {
		name     string
		existing []RegisteredTarget
		upsert   RegisteredTarget
		want     []RegisteredTarget
	}{
		{
			name:   "insert_into_empty_registry",
			upsert: RegisteredTarget{Nodename: "fuchsia-node-1", ID: "SER001", Address: "192.168.42.10"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "192.168.42.10"},
			},
		},
		{
			name: "update_by_id_preserves_nodename_and_peers",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "192.168.42.10"},
				{Nodename: "fuchsia-node-2", Address: "fe80::1%qemu"},
			},
			upsert: RegisteredTarget{ID: "SER001", Address: "usb:cid:7"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "usb:cid:7"},
				{Nodename: "fuchsia-node-2", Address: "fe80::1%qemu"},
			},
		},
		{
			name: "update_by_nodename_populates_id",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-node-2", Address: "fe80::1%qemu"},
				{Nodename: "fuchsia-node-3", Address: "fe80::2%qemu"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-node-2", ID: "EM-1234ABCD", Address: "fe80::99%qemu"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-node-2", ID: "EM-1234ABCD", Address: "fe80::99%qemu"},
				{Nodename: "fuchsia-node-3", Address: "fe80::2%qemu"},
			},
		},
		{
			name: "update_by_nodename_preserves_id",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "192.168.42.10"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-node-1", Address: "192.168.42.99"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "192.168.42.99"},
			},
		},
		{
			name: "shared_nodename_with_distinct_ids_appends",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-FIRST", Address: "fe80::1%qemu"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-emulator", ID: "EM-SECOND", Address: "fe80::2%qemu"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-FIRST", Address: "fe80::1%qemu"},
				{Nodename: "fuchsia-emulator", ID: "EM-SECOND", Address: "fe80::2%qemu"},
			},
		},
		{
			name: "shared_nodename_update_by_id_preserves_peer_without_id",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-FIRST", Address: "fe80::1%qemu"},
				{Nodename: "fuchsia-emulator", Address: "fe80::2%qemu"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-emulator", ID: "EM-FIRST", Address: "fe80::3%qemu"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-FIRST", Address: "fe80::3%qemu"},
				{Nodename: "fuchsia-emulator", Address: "fe80::2%qemu"},
			},
		},
		{
			name: "ambiguous_shared_nodename_without_id_appends",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-1", Address: "fe80::1%qemu"},
				{Nodename: "fuchsia-emulator", ID: "EM-2", Address: "fe80::2%qemu"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-emulator", Address: "fe80::99%qemu"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-emulator", ID: "EM-1", Address: "fe80::1%qemu"},
				{Nodename: "fuchsia-emulator", ID: "EM-2", Address: "fe80::2%qemu"},
				{Nodename: "fuchsia-emulator", Address: "fe80::99%qemu"},
			},
		},
		{
			name: "renamed_id_does_not_merge_nodename_peer",
			existing: []RegisteredTarget{
				{Nodename: "node-a", ID: "SER1", Address: "192.168.42.10"},
				{Nodename: "node-b", Address: "192.168.42.11"},
			},
			upsert: RegisteredTarget{Nodename: "node-b", ID: "SER1", Address: "192.168.42.12"},
			want: []RegisteredTarget{
				{Nodename: "node-b", ID: "SER1", Address: "192.168.42.12"},
				{Nodename: "node-b", Address: "192.168.42.11"},
			},
		},
		{
			name: "coalesces_split_partial_entries",
			existing: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", Address: "192.168.42.10"},
				{ID: "SER001", Address: "192.168.42.11"},
			},
			upsert: RegisteredTarget{Nodename: "fuchsia-node-1", ID: "SER001", Address: "usb:cid:9"},
			want: []RegisteredTarget{
				{Nodename: "fuchsia-node-1", ID: "SER001", Address: "usb:cid:9"},
			},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := upsertRegisteredTargets(tc.existing, tc.upsert); !slices.Equal(got, tc.want) {
				t.Errorf("upsertRegisteredTargets(%+v, %+v) = %+v, want %+v", tc.existing, tc.upsert, got, tc.want)
			}
		})
	}
}

func TestUpsertRegisteredTarget_RoundTrip(t *testing.T) {
	ffx := &FFXInstance{sharedData: filepath.Join(t.TempDir(), "shared")}
	peer := RegisteredTarget{Nodename: "fuchsia-node-2", Address: "fe80::1%qemu"}
	if err := ffx.UpsertRegisteredTarget(peer); err != nil {
		t.Fatalf("UpsertRegisteredTarget(%+v) failed: %v", peer, err)
	}
	upsert := RegisteredTarget{Nodename: "fuchsia-node-1", ID: "SER001", Address: "usb:cid:7"}
	if err := ffx.UpsertRegisteredTarget(upsert); err != nil {
		t.Fatalf("UpsertRegisteredTarget(%+v) failed: %v", upsert, err)
	}
	got, err := FFXWithTarget(ffx, "id:SER001").LoadRegisteredTargets()
	if err != nil {
		t.Fatalf("LoadRegisteredTargets() failed: %v", err)
	}
	if want := []RegisteredTarget{peer, upsert}; !slices.Equal(got, want) {
		t.Errorf("LoadRegisteredTargets() = %+v, want %+v", got, want)
	}
}

func TestRegisteredTargets_UnconfiguredSharedData(t *testing.T) {
	unconfigured := &FFXInstance{}
	if _, err := unconfigured.registeredTargetsPath(); err == nil {
		t.Errorf("registeredTargetsPath() on unconfigured FFXInstance = nil error, want non-nil")
	}
	if err := unconfigured.RegisterTargets(nil); err == nil {
		t.Errorf("RegisterTargets(nil) on unconfigured FFXInstance = nil, want non-nil error")
	}
	if _, err := unconfigured.LoadRegisteredTargets(); err == nil {
		t.Errorf("LoadRegisteredTargets() on unconfigured FFXInstance = nil, want non-nil error")
	}
	if err := unconfigured.UpsertRegisteredTarget(RegisteredTarget{ID: "SER001", Address: "192.168.42.10"}); err == nil {
		t.Errorf("UpsertRegisteredTarget() on unconfigured FFXInstance = nil, want non-nil error")
	}
}

func TestRegisteredTargets_MalformedJSONPreserved(t *testing.T) {
	ffx := &FFXInstance{sharedData: filepath.Join(t.TempDir(), "shared")}
	if err := os.MkdirAll(ffx.SharedData(), 0o755); err != nil {
		t.Fatalf("os.MkdirAll(%q) failed: %v", ffx.SharedData(), err)
	}
	path, _ := ffx.registeredTargetsPath()
	const malformed = "{not-valid-json"
	if err := os.WriteFile(path, []byte(malformed), 0o600); err != nil {
		t.Fatalf("os.WriteFile(%q) failed: %v", path, err)
	}
	if _, err := ffx.LoadRegisteredTargets(); err == nil {
		t.Errorf("LoadRegisteredTargets() on malformed JSON = nil, want non-nil error")
	}
	if err := ffx.UpsertRegisteredTarget(RegisteredTarget{ID: "SER001", Address: "192.168.42.10"}); err == nil {
		t.Errorf("UpsertRegisteredTarget() on malformed JSON = nil, want non-nil error")
	}
	gotRaw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("os.ReadFile(%q) failed: %v", path, err)
	}
	if string(gotRaw) != malformed {
		t.Errorf("os.ReadFile(%q) = %q, want %q (malformed file preserved)", path, string(gotRaw), malformed)
	}
}

func TestTargetList(t *testing.T) {
	// The script rejects any target specifier, since TargetList must not inject
	// the FFXInstance's target (fe80::99%qemu) ahead of the subcommand.
	script := `#!/bin/bash
if [[ "$*" == *"check-ssh-keys"* ]]; then
  exit 0
fi
if [[ "$*" == *"--target"* || "$*" == *"-t "* || "$*" == *"fe80::99%qemu"* ||
      "$*" != *"-c discovery.timeout=15000"* || "$*" != *"target list fe80::1%qemu"* ]]; then
  echo '{"type":"user","code":1,"message":"unexpected args: '"$*"'"}'
  exit 1
fi
cat <<'EOF'
[
  {"nodename":"fuchsia-5254-1234-1234","rcs_state":"Y","serial":"EM-B89E30F7F","target_type":"core.x64","target_state":"Product","addresses":[{"type":"Ip","ip":"fe80::1%qemu","ssh_port":22},{"type":"Usb","cid":7}],"is_default":true,"is_manual":false},
  {"nodename":"<unknown>","rcs_state":"Y","serial":"<unknown>","target_type":"Unknown","target_state":"Product","addresses":[],"is_default":false,"is_manual":false},
  {"nodename":"<unknown-1>","rcs_state":"N","serial":"GC-12345","target_type":"Unknown","target_state":"Unknown","addresses":[{"type":"VSock","cid":3}],"is_default":false,"is_manual":false}
]
EOF
`
	ffx, _ := newStrictTestFFX(t, script, "")
	got, err := FFXWithTarget(ffx, "fe80::99%qemu").TargetList(
		t.Context(),
		map[string]any{"discovery.timeout": 15000},
		"fe80::1%qemu",
	)
	if err != nil {
		t.Fatalf("TargetList() failed: %v", err)
	}
	type summary struct {
		nodename, serial, rcsState, usbAddress string
	}
	var gotSummaries []summary
	for _, e := range got {
		gotSummaries = append(gotSummaries, summary{e.NormalizedNodename(), e.NormalizedSerial(), e.RCSState, e.USBAddress()})
	}
	want := []summary{
		{"fuchsia-5254-1234-1234", "EM-B89E30F7F", "Y", "usb:cid:7"},
		{"", "", "Y", ""},
		{"", "GC-12345", "N", ""},
	}
	if !slices.Equal(gotSummaries, want) {
		t.Errorf("TargetList() (NormalizedNodename(), NormalizedSerial(), RCSState, USBAddress()) = %+v, want %+v", gotSummaries, want)
	}
}

func TestTargetList_Errors(t *testing.T) {
	for _, tc := range []struct {
		name   string
		script string
	}{
		{
			name:   "empty_output_rejected",
			script: "#!/bin/bash\nexit 0\n",
		},
		{
			name: "malformed_json_rejected",
			script: `#!/bin/bash
if [[ "$*" == *"check-ssh-keys"* ]]; then exit 0; fi
echo 'not-json'
`,
		},
		{
			name: "command_failure_propagated",
			script: `#!/bin/bash
if [[ "$*" == *"check-ssh-keys"* ]]; then exit 0; fi
echo '{"type":"user","code":1,"message":"bad query"}'
exit 1
`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			ffx, _ := newStrictTestFFX(t, tc.script, "")
			if got, err := ffx.TargetList(t.Context(), nil); err == nil {
				t.Errorf("TargetList() = (%+v, nil), want non-nil error", got)
			}
		})
	}
}
