// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package orchestrate

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestFfxEnvContainsSsh(t *testing.T) {
	ffx := &Ffx{Dir: "/foo/bar", bin: "foo/bar/ffx", sslCertPath: "/this/or/something"}
	cmd, err := ffx.CmdContext(context.Background(), "config", "get")
	if err != nil {
		t.Error(err)
	}
	pathFound := false
	wd, err := os.Getwd()
	if err != nil {
		t.Error(err)
	}
	for _, v := range cmd.Env {
		if strings.HasPrefix(v, "PATH=") && strings.Contains(v, filepath.Join(wd, "openssh-portable", "bin")) {
			pathFound = true
		}
	}
	if !pathFound {
		t.Errorf("SSH not found in env: %v", cmd.Env)
	}
}

func TestFfxSetDefaultTargetNotSet(t *testing.T) {
	// Gets the environment variables used for ffx command invocations, optionally
	// after `Ffx.SetDefaultTarget()` is called (if `defaultTarget != nil`).
	ffxEnv := func(defaultTarget *string) []string {
		ffx := &Ffx{Dir: "/foo/bar", bin: "foo/bar/ffx", sslCertPath: "/this/or/something"}
		ffx.SetDefaultTarget(defaultTarget)
		cmd, err := ffx.CmdContext(context.Background(), "config", "get")
		if err != nil {
			t.Error(err)
		}
		return cmd.Env
	}

	// Extracts the relevant ffx default target environment variables as a string.
	getDefaultTargetEnvVars := func(env []string) string {
		cmd := exec.Command("bash", "-c", "echo \"<${FUCHSIA_NODENAME+FUCHSIA_NODENAME=}${FUCHSIA_NODENAME},${FUCHSIA_DEVICE_ADDR+FUCHSIA_DEVICE_ADDR=}${FUCHSIA_DEVICE_ADDR}>\"")
		cmd.Env = env
		outputBytes, err := cmd.CombinedOutput()
		if err != nil {
			t.Error(err)
		}
		return strings.TrimSpace(string(outputBytes))
	}

	// Emulate an environment that comes with these env vars defined already.
	os.Setenv("FUCHSIA_NODENAME", "inherited_nodename_value")
	os.Setenv("FUCHSIA_DEVICE_ADDR", "ignored_device_addr_value")

	// Without a default target set, ffx should inherit $FUCHSIA_NODENAME from the
	// current env.
	actual := getDefaultTargetEnvVars(ffxEnv(nil))
	expected := "<FUCHSIA_NODENAME=inherited_nodename_value,>"
	if actual != expected {
		t.Errorf("Output \"%s\" doesn't equal expected \"%s\"", actual, expected)
	}

	// SetDefaultTarget should remove FUCHSIA_DEVICE_ADDR and override
	// FUCHSIA_NODENAME.
	target := "SetDefaultTargetValue"
	actual = getDefaultTargetEnvVars(ffxEnv(&target))
	expected = "<FUCHSIA_NODENAME=SetDefaultTargetValue,>"
	if actual != expected {
		t.Errorf("Output \"%s\" doesn't equal expected \"%s\"", actual, expected)
	}
}

func TestWriteConfigFile_ConnectivityDirect(t *testing.T) {
	tmpDir := t.TempDir()
	configPath := filepath.Join(tmpDir, "config.json")
	opt := Option{
		LogDir: filepath.Join(tmpDir, "log"),
	}
	if err := writeConfigFile(configPath, opt, filepath.Join(tmpDir, "socket")); err != nil {
		t.Fatalf("writeConfigFile failed: %v", err)
	}
	data, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}
	var config struct {
		Connectivity struct {
			Direct bool `json:"direct"`
		} `json:"connectivity"`
	}
	if err := json.Unmarshal(data, &config); err != nil {
		t.Fatalf("Unmarshal failed: %v", err)
	}
	if !config.Connectivity.Direct {
		t.Errorf("Expected connectivity.direct to be true, got %v", config.Connectivity.Direct)
	}
}

func TestFfxConfigOverridesInCmdContext(t *testing.T) {
	ffx := &Ffx{
		Dir:         "/foo/bar",
		bin:         "foo/bar/ffx",
		sslCertPath: "/this/or/something",
		configOverrides: map[string]string{
			"sdk.overrides.qemu_internal": "/path/to/qemu",
			"emu.start.timeout":           "300",
		},
	}
	cmd, err := ffx.CmdContext(context.Background(), "emu", "start", "product/bundle")
	if err != nil {
		t.Fatalf("CmdContext failed: %v", err)
	}
	expectedArgs := []string{
		"foo/bar/ffx",
		"--config", "emu.start.timeout=300",
		"--config", "sdk.overrides.qemu_internal=/path/to/qemu",
		"emu", "start", "product/bundle",
	}
	if len(cmd.Args) != len(expectedArgs) {
		t.Fatalf("Got args %v, want %v", cmd.Args, expectedArgs)
	}
	for i, arg := range cmd.Args {
		if arg != expectedArgs[i] {
			t.Errorf("Arg %d: got %s, want %s", i, arg, expectedArgs[i])
		}
	}
}

func TestFfxFlash(t *testing.T) {
	tests := []struct {
		name       string
		target     string
		pubKeyPath string
		wantArgs   []string
	}{
		{
			name:       "With target and pubkey",
			target:     "id:serial123",
			pubKeyPath: "path/to/pubkey",
			wantArgs:   []string{"--target", "id:serial123", "target", "flash", "--product-bundle", "path/to/product", "--authorized-keys", "path/to/pubkey"},
		},
		{
			name:       "Empty target omits --target",
			target:     "",
			pubKeyPath: "",
			wantArgs:   []string{"target", "flash", "--product-bundle", "path/to/product"},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			tmpDir := t.TempDir()
			argsFile := filepath.Join(tmpDir, "args.txt")
			fakeFfx := filepath.Join(tmpDir, "fake_ffx")
			script := "#!/bin/bash\nprintf '%s\\n' \"$@\" > " + argsFile + "\n"
			if err := os.WriteFile(fakeFfx, []byte(script), 0755); err != nil {
				t.Fatalf("failed to create fake ffx: %v", err)
			}

			ffx := &Ffx{Dir: tmpDir, bin: fakeFfx}
			if err := ffx.Flash(context.Background(), tc.target, "path/to/product", tc.pubKeyPath); err != nil {
				t.Fatalf("Flash failed: %v", err)
			}

			data, err := os.ReadFile(argsFile)
			if err != nil {
				t.Fatalf("failed to read args file: %v", err)
			}
			gotArgs := strings.Split(strings.TrimSuffix(string(data), "\n"), "\n")
			if !reflect.DeepEqual(gotArgs, tc.wantArgs) {
				t.Errorf("got args %q, want %q", gotArgs, tc.wantArgs)
			}
		})
	}
}
