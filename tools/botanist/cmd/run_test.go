// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"cmp"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/targets"
	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	testrunnerconstants "go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"
	"golang.org/x/sync/errgroup"
)

type MockTarget struct {
	mu         sync.Mutex
	ffx        *targets.FFXInstance
	serialNum  string
	sharedData string
	// emptyNodename makes Nodename() return only a nodename recorded via
	// SetNodename, emulating targets without a statically configured nodename.
	emptyNodename    bool
	nodename         string
	serialSocketPath string
	sshClientFunc    func(attempt int) (*sshutil.Client, error)
	sshClientCalls   int
	testConfigFunc   func(expectsSSH bool) (any, error)
	addPkgRepoErr    error
	addFFXPkgRepoErr error
}

func (m *MockTarget) TestConfig(expectsSSH bool) (any, error) {
	if m.testConfigFunc != nil {
		return m.testConfigFunc(expectsSSH)
	}
	return nil, nil
}
func (m *MockTarget) AddFFXPackageRepository(ctx context.Context, pkgRepoName string, pkgSrvPort int, useForward bool) (func(), error) {
	if m.addFFXPkgRepoErr != nil {
		return nil, m.addFFXPkgRepoErr
	}
	return func() {}, nil
}
func (m *MockTarget) AddPackageRepository(client *sshutil.Client, repoURL, blobURL string) error {
	return m.addPkgRepoErr
}
func (m *MockTarget) CaptureSerialLog(filename string) error { return nil }
func (m *MockTarget) CaptureSyslog(client *sshutil.Client, filename string, pkgSrv *botanist.PackageServer) error {
	return nil
}
func (m *MockTarget) StopSyslog() {}
func (m *MockTarget) IPv6() (*net.IPAddr, error) {
	// Sleep to allow StartFFXMonitor to run before runAgainstTarget fails/returns
	time.Sleep(500 * time.Millisecond)
	addr, _ := net.ResolveIPAddr("ip6", "::1")
	return addr, nil
}
func (m *MockTarget) IPv4() (net.IP, error) {
	return net.ParseIP("127.0.0.1"), nil
}
func (m *MockTarget) Nodename() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.emptyNodename {
		return m.nodename
	}
	return cmp.Or(m.nodename, "mock-target")
}
func (m *MockTarget) SetNodename(nodename string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.nodename = nodename
}
func (m *MockTarget) SerialNumber() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.serialNum
}
func (m *MockTarget) SetSerialNumber(serial string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.serialNum = serial
}
func (m *MockTarget) Serial() io.ReadWriteCloser { return nil }
func (m *MockTarget) SerialSocketPath() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.serialSocketPath
}
func (m *MockTarget) SetConnectionTimeout(time.Duration) {}
func (m *MockTarget) ResolveIP() error                   { return nil }
func (m *MockTarget) SSHClient() (*sshutil.Client, error) {
	m.mu.Lock()
	m.sshClientCalls++
	attempt := m.sshClientCalls
	fn := m.sshClientFunc
	m.mu.Unlock()
	if fn != nil {
		return fn(attempt)
	}
	return nil, nil
}
func (m *MockTarget) SSHControlMasterPath() string { return "" }
func (m *MockTarget) SetupSSHControlMaster(ctx context.Context, sshKey, addr string) (func(), error) {
	return func() {}, nil
}
func (m *MockTarget) SSHControlMasterRunning() bool { return false }
func (m *MockTarget) SSHKey() string                { return "ssh-key" }
func (m *MockTarget) Start(ctx context.Context, args []string, pbPath string, isBootTest bool) error {
	return nil
}
func (m *MockTarget) StartSerialServer() error   { return nil }
func (m *MockTarget) Stop() error                { return nil }
func (m *MockTarget) Wait(context.Context) error { return nil }
func (m *MockTarget) SetFFX(ffx *targets.FFXInstance, env []string) {
	m.ffx = ffx
}
func (m *MockTarget) GetFFX() *targets.FFXInstance { return m.ffx }
func (m *MockTarget) UseFFXExperiment(exp botanist.Experiment) bool {
	return m.ffx.Experiments.Contains(exp)
}
func (m *MockTarget) UseProductBundles() bool { return false }
func (m *MockTarget) FFXEnv() []string        { return nil }
func (m *MockTarget) GetSharedData() string   { return m.sharedData }

type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (n int, err error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buf.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buf.String()
}

func TestStartFFXMonitor(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	// Create a mock ffx script that echoes arguments
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), 0o755); err != nil {
		t.Fatal("failed to write mock ffx tool")
	}

	// Create test SSH keys
	sshPriv := filepath.Join(tmpDir, "ssh_priv")
	sshPub := filepath.Join(tmpDir, "ssh_pub")
	if err := os.WriteFile(sshPriv, []byte("PRIVATE KEY"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(sshPub, []byte("PUBLIC KEY"), 0o600); err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	ffxInstance, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, "target", &ffxutil.SSHInfo{SshPriv: sshPriv, SshPub: sshPub}, tmpDir, ffxutil.UseFFXLegacy)
	if err != nil {
		t.Fatalf("failed to create ffx instance: %s", err)
	}

	// Capture stdout
	rOut, wOut, _ := os.Pipe()
	ffxInstance.SetStdoutStderr(wOut, wOut)

	targetFFX := &targets.FFXInstance{FFXInstance: ffxInstance}
	target := &MockTarget{ffx: targetFFX}

	cmd := &RunCommand{
		expectsSSH:        true,
		productBundles:    "test-bundles",
		productBundleName: "test-bundle-name",
	}
	eg, ctx := errgroup.WithContext(ctx)
	cancel := func() {}

	// Set test out dir env var because StartFFXMonitor uses it
	os.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpDir)
	defer os.Unsetenv(testrunnerconstants.TestOutDirEnvKey)

	experiments := make(botanist.Experiments)

	const (
		monitorName         = "ffx_monitor"
		aggregationFilename = "aggregation.json"
	)

	// Create an initial summary.json
	summaryPath := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, runtests.TestSummaryFilename)
	initialSummary := runtests.TestSummary{Tests: []runtests.TestDetails{}}
	summaryBytes, _ := json.Marshal(initialSummary)
	if err := os.WriteFile(summaryPath, summaryBytes, 0o600); err != nil {
		t.Fatalf("failed to write initial summary.json: %v", err)
	}

	// Run dispatchTests
	cmd.dispatchTests(ctx, cancel, eg, nil, []targets.FuchsiaTarget{target}, target, nil, "invalid-tests-path", experiments)

	// Wait for goroutines to complete
	_ = eg.Wait()

	wOut.Close()
	outBytes, _ := io.ReadAll(rOut)
	outStr := string(outBytes)

	// Check if monitor start command was issued with correct args
	if !strings.Contains(outStr, "monitor start") {
		t.Errorf("expected monitor start command, got: %s", outStr)
	}
	if !strings.Contains(outStr, "--log-file") {
		t.Errorf("expected --log-file arg, got: %s", outStr)
	}
	expectedLogFile := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, monitorName, "device.status.json")
	if !strings.Contains(outStr, expectedLogFile) {
		t.Errorf("expected log file %s, got output: %s", expectedLogFile, outStr)
	}
	if !strings.Contains(outStr, "--aggregations-file") {
		t.Errorf("expected --aggregations-file arg, got: %s", outStr)
	}
	expectedAggregationsFile := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, monitorName, aggregationFilename)
	if !strings.Contains(outStr, expectedAggregationsFile) {
		t.Errorf("expected aggregations file %s, got output: %s", expectedAggregationsFile, outStr)
	}

	// Verify summary.json was updated
	data, err := os.ReadFile(summaryPath)
	if err != nil {
		t.Fatalf("failed to read summary.json: %v", err)
	}
	var summary runtests.TestSummary
	if err := json.Unmarshal(data, &summary); err != nil {
		t.Fatalf("failed to unmarshal summary.json: %v", err)
	}
	found := false
	for _, test := range summary.Tests {
		if test.Name == monitorName {
			found = true
			if test.OutputDir != monitorName {
				t.Errorf("expected OutputDir %s, got %s", monitorName, test.OutputDir)
			}
			if len(test.OutputFiles) != 1 || test.OutputFiles[0] != aggregationFilename {
				t.Errorf("expected OutputFiles [%s], got %v", aggregationFilename, test.OutputFiles)
			}
			break
		}
	}
	if !found {
		t.Error("ffx_monitor entry not found in summary.json")
	}
}

func newFFXTarget(t *testing.T, tmpDir, script string, mode ffxutil.FFXInvokeMode) *MockTarget {
	t.Helper()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte(script), 0o755); err != nil {
		t.Fatalf("failed to write mock ffx tool: %v", err)
	}
	sshPriv := filepath.Join(tmpDir, "ssh_priv")
	sshPub := filepath.Join(tmpDir, "ssh_pub")
	if err := os.WriteFile(sshPriv, []byte("PRIVATE KEY"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(sshPub, []byte("PUBLIC KEY"), 0o600); err != nil {
		t.Fatal(err)
	}
	ffxInstance, err := ffxutil.NewFFXInstance(context.Background(), ffxPath, tmpDir, nil, "target", &ffxutil.SSHInfo{SshPriv: sshPriv, SshPub: sshPub}, tmpDir, mode)
	if err != nil {
		t.Fatalf("failed to create ffx instance: %s", err)
	}
	ffxInstance.SetStdoutStderr(io.Discard, io.Discard)
	return &MockTarget{ffx: &targets.FFXInstance{FFXInstance: ffxInstance}, sharedData: ffxInstance.SharedData()}
}

func TestStartFFXMonitorShutdownWait(t *testing.T) {
	tests := []struct {
		name               string
		timeout            time.Duration
		script             string
		markerFile         string
		wantTimeoutWarning bool
		cleanup            func(t *testing.T, tmpDir string)
	}{
		{
			name:       "waits for monitor exit before returning",
			markerFile: "flushed.txt",
			script: `#!/bin/bash
if [[ "$*" == *"monitor start"* ]]; then
  trap 'sleep 0.2; printf flushed > flushed.txt; exit 0' TERM
  while true; do sleep 5 & wait $!; done
fi
`,
		},
		{
			name:               "timeout does not hang when monitor ignores cancellation",
			timeout:            300 * time.Millisecond,
			wantTimeoutWarning: true,
			script: `#!/bin/bash
if [[ "$*" == *"monitor stop"* ]]; then
  exit 0
fi
if [[ "$*" == *"monitor start"* ]]; then
  echo $$ > monitor.pid
  trap '' TERM
  while true; do sleep 5 & wait $!; done
fi
`,
			cleanup: func(t *testing.T, tmpDir string) {
				t.Helper()
				pidBytes, err := os.ReadFile(filepath.Join(tmpDir, "monitor.pid"))
				if err != nil {
					return
				}
				if pid, err := strconv.Atoi(strings.TrimSpace(string(pidBytes))); err == nil && pid > 1 {
					// Kill the process group to ensure the sleep and script terminate immediately.
					_ = syscall.Kill(-pid, syscall.SIGKILL)
				}
			},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			tmpDir := t.TempDir()
			t.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpDir)

			target := newFFXTarget(t, tmpDir, tc.script, ffxutil.UseFFXLegacy)

			if tc.cleanup != nil {
				t.Cleanup(func() {
					tc.cleanup(t, tmpDir)
				})
			}

			var logBuf syncBuffer
			logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

			cmd := &RunCommand{
				expectsSSH:            true,
				productBundles:        "test-bundles",
				productBundleName:     "test-bundle-name",
				ffxMonitorExitTimeout: tc.timeout,
			}
			eg, ctx := errgroup.WithContext(logCtx)
			cmd.dispatchTests(ctx, func() {}, eg, nil, []targets.FuchsiaTarget{target}, target, nil, "invalid-tests-path", make(botanist.Experiments))
			_ = eg.Wait()

			if tc.markerFile != "" {
				got, err := os.ReadFile(filepath.Join(tmpDir, tc.markerFile))
				if err != nil || string(got) != "flushed" {
					t.Fatalf("expected monitor flush marker before dispatchTests returned, got %q (err=%v)", string(got), err)
				}
			}

			hasTimeout := strings.Contains(logBuf.String(), "timed out waiting for ffx monitor to exit")
			if tc.wantTimeoutWarning != hasTimeout {
				t.Errorf("got timeout warning = %v, want %v (logs: %s)", hasTimeout, tc.wantTimeoutWarning, logBuf.String())
			}
		})
	}
}

func TestGetBootTimeout(t *testing.T) {
	tests := []struct {
		name                   string
		bootTimeout            time.Duration
		testTimeoutScaleFactor int
		want                   time.Duration
	}{
		{
			name:                   "default unscaled",
			bootTimeout:            0,
			testTimeoutScaleFactor: 1,
			want:                   0,
		},
		{
			name:                   "default scaled by 2",
			bootTimeout:            0,
			testTimeoutScaleFactor: 2,
			want:                   2 * targets.DefaultConnectionTimeout,
		},
		{
			name:                   "custom unscaled",
			bootTimeout:            60 * time.Second,
			testTimeoutScaleFactor: 1,
			want:                   60 * time.Second,
		},
		{
			name:                   "custom scaled by 3",
			bootTimeout:            60 * time.Second,
			testTimeoutScaleFactor: 3,
			want:                   180 * time.Second,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			cmd := &RunCommand{
				bootupTimeout:          tc.bootTimeout,
				testTimeoutScaleFactor: tc.testTimeoutScaleFactor,
			}
			if got := cmd.getBootTimeout(); got != tc.want {
				t.Errorf("getBootTimeout() = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestConnectToSSHRetryOnTimeout(t *testing.T) {
	timeoutErr := sshutil.ConnectionError{
		Err: fmt.Errorf("timed out trying to connect to ssh after 2m0s: %w", context.DeadlineExceeded),
	}

	t.Run("recovers via serial reboot on timeout", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		socketPath := targets.CreateSocketPath("serial")
		t.Cleanup(func() {
			os.Remove(socketPath)
		})
		listener, err := net.Listen("unix", socketPath)
		if err != nil {
			t.Fatalf("net.Listen(unix, %q) failed: %v", socketPath, err)
		}
		t.Cleanup(func() {
			listener.Close()
		})

		serialCmdCh := make(chan string, 1)
		go func() {
			for {
				conn, err := listener.Accept()
				if err != nil {
					return
				}
				_, _ = conn.Write([]byte("$ "))
				var received strings.Builder
				buf := make([]byte, 256)
				for {
					n, readErr := conn.Read(buf)
					if n > 0 {
						received.Write(buf[:n])
						if strings.Contains(received.String(), "dm reboot") {
							serialCmdCh <- received.String()
							break
						}
					}
					if readErr != nil {
						serialCmdCh <- received.String()
						break
					}
				}
				conn.Close()
			}
		}()

		target := &MockTarget{
			serialSocketPath: socketPath,
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				if attempt == 1 {
					return nil, timeoutErr
				}
				return nil, nil
			},
		}

		cmd := &RunCommand{}
		if _, err := cmd.connectToSSH(context.Background(), target); err != nil {
			t.Fatalf("expected connectToSSH to succeed after serial reboot, got: %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected 2 SSHClient attempts, got %d", sshCalls)
		}

		select {
		case gotRebootCmd := <-serialCmdCh:
			if !strings.Contains(gotRebootCmd, "dm reboot") {
				t.Errorf("expected serial reboot command 'dm reboot', got %q", gotRebootCmd)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("timed out waiting for serial reboot command")
		}
	})

	t.Run("falls back to dmc power cycle when serial reboot fails", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcArgsFile := filepath.Join(tmpDir, "dmc_args.txt")
		dmcPath := filepath.Join(tmpDir, "dmc")
		dmcScript := fmt.Sprintf("#!/bin/sh\necho \"$@\" > %q\n", dmcArgsFile)
		if err := os.WriteFile(dmcPath, []byte(dmcScript), 0o755); err != nil {
			t.Fatalf("failed to write mock dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			serialSocketPath: "/tmp/nonexistent-%s.sock",
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				if attempt == 1 {
					return nil, timeoutErr
				}
				return nil, nil
			},
		}

		var logBuf bytes.Buffer
		logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.WarningLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

		cmd := &RunCommand{}
		if _, err := cmd.connectToSSH(logCtx, target); err != nil {
			t.Fatalf("expected connectToSSH to succeed after dmc recovery, got: %v", err)
		}
		if strings.Contains(logBuf.String(), "failed to open serial socket connection") || strings.Contains(logBuf.String(), "failed to find cursor") {
			t.Errorf("expected serial socket failure not to be logged when dmc recovery succeeds, got: %s", logBuf.String())
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected 2 SSHClient attempts, got %d", sshCalls)
		}

		dmcArgsBytes, err := os.ReadFile(dmcArgsFile)
		if err != nil {
			t.Fatalf("expected dmc to be invoked, failed to read %s: %v", dmcArgsFile, err)
		}
		wantDMCArgs := "set-power-state --nodename mock-target --state cycle"
		if got := strings.TrimSpace(string(dmcArgsBytes)); got != wantDMCArgs {
			t.Errorf("dmc args = %q, want %q", got, wantDMCArgs)
		}
	})

	t.Run("aborts when all recovery mechanisms fail", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcPath := filepath.Join(tmpDir, "dmc")
		if err := os.WriteFile(dmcPath, []byte("#!/bin/sh\nexit 1\n"), 0o755); err != nil {
			t.Fatalf("failed to write failing dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			serialSocketPath: "/tmp/nonexistent-%s.sock",
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		var logBuf bytes.Buffer
		logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.WarningLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(logCtx, target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("expected context.DeadlineExceeded when recovery fails, got: %v", err)
		}
		if strings.Contains(logBuf.String(), "%!s(MISSING)") {
			t.Errorf("expected warning log to format %% safely without %%!s(MISSING), got: %s", logBuf.String())
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Fatalf("expected 1 SSHClient attempt when recovery fails, got %d", sshCalls)
		}
	})

	t.Run("aborts when no recovery mechanism is available", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("expected context.DeadlineExceeded when no recovery mechanism is available, got: %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Fatalf("expected 1 SSHClient attempt when no recovery mechanism is available, got %d", sshCalls)
		}
	})

	t.Run("aborts after second timeout without further retries", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcPath := filepath.Join(tmpDir, "dmc")
		if err := os.WriteFile(dmcPath, []byte("#!/bin/sh\nexit 0\n"), 0o755); err != nil {
			t.Fatalf("failed to write mock dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Errorf("expected context.DeadlineExceeded error after retry failure, got %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected exactly 2 SSHClient attempts, got %d", sshCalls)
		}
	})

	t.Run("does not retry on non-timeout error", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		nonTimeoutErr := errors.New("SSHClient() failed; no ssh keys provided")
		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, nonTimeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, nonTimeoutErr) {
			t.Errorf("expected error %v, got %v", nonTimeoutErr, err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Errorf("expected 1 SSHClient attempt for non-timeout error, got %d", sshCalls)
		}
	})

	t.Run("does not retry when parent context is canceled", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				cancel()
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, _ = cmd.connectToSSH(ctx, target)

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Errorf("expected 1 SSHClient attempt when parent context is canceled, got %d", sshCalls)
		}
	})
}

func TestExecuteSkipSetupPropagatesTestrunnerError(t *testing.T) {
	var logBuf bytes.Buffer
	logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

	cmd := &RunCommand{
		skipSetup: true,
		timeout:   10 * time.Minute,
	}
	err := cmd.execute(logCtx, []string{filepath.Join(t.TempDir(), "nonexistent-tests.json")})
	if err == nil {
		t.Fatal("expected execute to fail for nonexistent tests.json")
	}
	if strings.Contains(err.Error(), "testrunner with flags:") {
		t.Errorf("expected error not to contain raw testrunnerOptions dump, got: %v", err)
	}
	if !strings.Contains(logBuf.String(), "testrunner with flags:") {
		t.Errorf("expected debug log to contain testrunner flags, got: %s", logBuf.String())
	}
}

func TestDispatchTestsWrapsPackageRepoSetupError(t *testing.T) {
	t.Run("legacy AddPackageRepository failure", func(t *testing.T) {
		repoErr := errors.New("connection reset by peer")
		target := newFFXTarget(t, t.TempDir(), "#!/bin/sh\nexit 0\n", ffxutil.UseFFXLegacy)
		target.addPkgRepoErr = repoErr
		cmd := &RunCommand{
			expectsSSH:        true,
			productBundles:    "test-bundles",
			productBundleName: "test-bundle-name",
		}
		pkgSrv := &botanist.PackageServer{
			RepoURL: "http://localhost:8083/repository",
			BlobURL: "http://localhost:8083/repository/blobs",
		}

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		eg, egCtx := errgroup.WithContext(ctx)

		cmd.dispatchTests(egCtx, cancel, eg, nil, []targets.FuchsiaTarget{target}, target, pkgSrv, "tests.json", make(botanist.Experiments))
		err := eg.Wait()
		if err == nil {
			t.Fatal("expected dispatchTests to fail when AddPackageRepository returns an error")
		}
		wantPrefix := constants.PackageRepoSetupErrorMsg + ": "
		if !strings.HasPrefix(err.Error(), wantPrefix) {
			t.Errorf("dispatchTests error = %q, want prefix %q", err.Error(), wantPrefix)
		}
		if !errors.Is(err, repoErr) {
			t.Errorf("expected error to wrap %v, got %v", repoErr, err)
		}
	})

	t.Run("ffx setupFFXPackageServer failure", func(t *testing.T) {
		t.Setenv(constants.PkgSrvPortKey, "invalid-port")
		target := newFFXTarget(t, t.TempDir(), "#!/bin/sh\nexit 0\n", ffxutil.UseFFXLegacy)
		cmd := &RunCommand{
			expectsSSH:        true,
			localRepo:         t.TempDir(),
			productBundles:    "test-bundles",
			productBundleName: "test-bundle-name",
		}
		experiments := botanist.GetExperiments([]string{string(botanist.UseFFXRepository)})

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		eg, egCtx := errgroup.WithContext(ctx)

		cmd.dispatchTests(egCtx, cancel, eg, nil, []targets.FuchsiaTarget{target}, target, nil, "tests.json", experiments)
		err := eg.Wait()
		if err == nil {
			t.Fatal("expected dispatchTests to fail when setupFFXPackageServer returns an error")
		}
		wantPrefix := constants.PackageRepoSetupErrorMsg + ": "
		if !strings.HasPrefix(err.Error(), wantPrefix) {
			t.Errorf("dispatchTests error = %q, want prefix %q", err.Error(), wantPrefix)
		}
	})

	t.Run("ffx AddFFXPackageRepository failure", func(t *testing.T) {
		ffxScript := `#!/bin/sh
case "$*" in
  *"repository server list"*)
    echo '{"ok":{"data":[{"name":"fuchsia-package-server"}]}}'
    ;;
esac
`
		repoErr := errors.New("failed to register ffx repository")
		target := newFFXTarget(t, t.TempDir(), ffxScript, ffxutil.UseFFXLegacy)
		target.addFFXPkgRepoErr = repoErr
		cmd := &RunCommand{
			expectsSSH:        true,
			localRepo:         t.TempDir(),
			productBundles:    "test-bundles",
			productBundleName: "test-bundle-name",
		}
		experiments := botanist.GetExperiments([]string{string(botanist.UseFFXRepository)})

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		eg, egCtx := errgroup.WithContext(ctx)

		cmd.dispatchTests(egCtx, cancel, eg, nil, []targets.FuchsiaTarget{target}, target, nil, "tests.json", experiments)
		err := eg.Wait()
		if err == nil {
			t.Fatal("expected dispatchTests to fail when AddFFXPackageRepository returns an error")
		}
		wantPrefix := constants.PackageRepoSetupErrorMsg + ": "
		if !strings.HasPrefix(err.Error(), wantPrefix) {
			t.Errorf("dispatchTests error = %q, want prefix %q", err.Error(), wantPrefix)
		}
		if !errors.Is(err, repoErr) {
			t.Errorf("expected error to wrap %v, got %v", repoErr, err)
		}
	})
}
func TestLogBotanistFailure(t *testing.T) {
	tests := []struct {
		name       string
		executeErr error
		wantLog    string
		wantNoLog  string
		wantEmpty  bool
	}{
		{
			name:       "nil error produces no log output",
			executeErr: nil,
			wantEmpty:  true,
		},
		{
			name:       "wrapped PhaseError logs phase tag at prefix",
			executeErr: fmt.Errorf("start target error: %w", botanist.NewPhaseError(botanist.PhaseTargetProvisioning, errors.New("flash failed"))),
			wantLog:    "botanist ERROR: botanist failed: [phase=target_provisioning] start target error: flash failed",
		},
		{
			name:       "unclassified generic error omits phase tag",
			executeErr: errors.New("unexpected internal error"),
			wantLog:    "botanist ERROR: botanist failed: unexpected internal error",
			wantNoLog:  "[phase=",
		},
		{
			name:       "zero-value PhaseError with invalid phase omits phase tag",
			executeErr: fmt.Errorf("wrapped: %w", &botanist.PhaseError{}),
			wantLog:    "botanist ERROR: botanist failed: wrapped: <nil>",
			wantNoLog:  "[phase=",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			var logBuf bytes.Buffer
			logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, "botanist "))

			logBotanistFailure(logCtx, tc.executeErr)

			logOutput := logBuf.String()
			if tc.wantEmpty {
				if logOutput != "" {
					t.Errorf("expected no log output for nil error, got:\n%s", logOutput)
				}
				return
			}
			if tc.wantLog != "" && !strings.Contains(logOutput, tc.wantLog) {
				t.Errorf("log output missing %q; got:\n%s", tc.wantLog, logOutput)
			}
			if tc.wantNoLog != "" && strings.Contains(logOutput, tc.wantNoLog) {
				t.Errorf("log output unexpectedly contained %q; got:\n%s", tc.wantNoLog, logOutput)
			}
		})
	}
}

func TestDispatchTestsCreatesTestbedConfigAfterTargetSetup(t *testing.T) {
	tmpDir := t.TempDir()
	t.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpDir)
	target := newFFXTarget(t, tmpDir, "#!/bin/sh\nexit 0\n", ffxutil.UseFFXLegacy)
	target.GetFFX().SetTarget("")

	var ffxTargetDuringTestConfig string
	var sshCallsDuringTestConfig int
	target.testConfigFunc = func(expectsSSH bool) (any, error) {
		ffxTargetDuringTestConfig = target.GetFFX().GetTarget()
		target.mu.Lock()
		sshCallsDuringTestConfig = target.sshClientCalls
		target.mu.Unlock()
		return targets.TestbedConfig(target, expectsSSH, nil)
	}

	cmd := &RunCommand{
		ffxPath:           filepath.Join(tmpDir, "ffx"),
		expectsSSH:        true,
		productBundles:    "test-bundles",
		productBundleName: "test-bundle-name",
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	eg, egCtx := errgroup.WithContext(ctx)

	cmd.dispatchTests(
		egCtx,
		cancel,
		eg,
		[]targets.Base{target},
		[]targets.FuchsiaTarget{target},
		target,
		nil,
		filepath.Join(tmpDir, "nonexistent-tests.json"),
		make(botanist.Experiments),
	)
	_ = eg.Wait()

	if sshCallsDuringTestConfig == 0 {
		t.Error("expected SSH connection setup to run before TestConfig")
	}
	if ffxTargetDuringTestConfig != "127.0.0.1" {
		t.Errorf("FFX target during TestConfig = %q, want %q", ffxTargetDuringTestConfig, "127.0.0.1")
	}
}

func writeTestEd25519Key(t *testing.T, dir string) (string, string) {
	t.Helper()
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	pkcs8, err := x509.MarshalPKCS8PrivateKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	pemBytes := pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: pkcs8})
	privPath := filepath.Join(dir, "id_ed25519")
	pubPath := privPath + ".pub"
	if err := os.WriteFile(privPath, pemBytes, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(pubPath, []byte("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	return privPath, pubPath
}

func newStrictTestDevice(t *testing.T, ffxScript string, config targets.DeviceConfig, experiments botanist.Experiments) *targets.Device {
	t.Helper()
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte(ffxScript), 0o755); err != nil {
		t.Fatal(err)
	}
	sshPriv, sshPub := writeTestEd25519Key(t, tmpDir)
	baseFFX, err := ffxutil.NewFFXInstance(t.Context(), ffxPath, tmpDir, nil, "", &ffxutil.SSHInfo{SshPriv: sshPriv, SshPub: sshPub}, filepath.Join(tmpDir, "out"), ffxutil.UseFFXStrict)
	if err != nil {
		t.Fatal(err)
	}
	baseFFX.SetStdoutStderr(io.Discard, io.Discard)

	config.SSHKeys = []string{sshPriv}
	dev, err := targets.NewDevice(t.Context(), config, targets.Options{})
	if err != nil {
		t.Fatal(err)
	}
	dev.SetFFX(&targets.FFXInstance{FFXInstance: ffxutil.FFXWithTarget(baseFFX, ""), Experiments: experiments}, baseFFX.Env())
	return dev
}

func TestSetupFFXUSBDriver(t *testing.T) {
	t.Run("discovers_usb_address_and_populates_test_config", func(t *testing.T) {
		baseDir := t.TempDir()
		t.Chdir(baseDir)
		// Use a relative output directory to verify USBSocketPath is normalized
		// to an absolute path for subprocesses running from other directories.
		t.Setenv(testrunnerconstants.TestOutDirEnvKey, "rel_test_out")
		wantSocketDir := filepath.Join(baseDir, "rel_test_out")
		attemptsFile := filepath.Join(baseDir, "attempts")
		script := fmt.Sprintf(`#!/bin/bash
attempts=%q
if [[ "$*" == *"check-ssh-keys"* ]]; then
  exit 0
fi
if [[ "$*" == *"usb-driver"* ]]; then
  trap 'exit 0' TERM INT
  while true; do sleep 1 & wait $!; done
fi
if [[ "$*" == *"target list"* ]]; then
  if [[ "$*" != *"id:USB9876"* ]]; then
    echo '{"type":"user","code":1,"message":"missing target query: '"$*"'"}'
    exit 1
  fi
  count=0
  if [[ -f "$attempts" ]]; then
    count=$(cat "$attempts")
  fi
  count=$((count + 1))
  echo "$count" > "$attempts"
  if [[ "$count" -lt 2 ]]; then
    echo '[]'
    exit 0
  fi
  echo '[{"nodename":"<unknown>","rcs_state":"U","serial":"USB9876","addresses":[]},{"nodename":"fuchsia-usb-dut","rcs_state":"N","serial":"USB9876","addresses":[{"type":"Usb","cid":7}]}]'
  exit 0
fi
exit 0
`, attemptsFile)

		experiments := botanist.GetExperiments([]string{string(botanist.ForceFFXUSB)})
		dev := newStrictTestDevice(t, script, targets.DeviceConfig{
			SerialNumber: "USB9876",
			Network: targets.NetworkProperties{
				Nodename: "fuchsia-usb-dut",
				IPv4Addr: "192.168.42.99",
			},
		}, experiments)

		cmd := &RunCommand{
			usbTargetDiscoverBackoff: 10 * time.Millisecond,
			usbTargetDiscoverTimeout: 5 * time.Second,
		}
		cleanup, err := cmd.setupFFXUSBDriver(t.Context(), dev)
		if err != nil {
			t.Fatalf("setupFFXUSBDriver() failed: %v", err)
		}
		t.Cleanup(cleanup)

		gotSocket := dev.USBSocketPath()
		if !filepath.IsAbs(gotSocket) {
			t.Errorf("dev.USBSocketPath() = %q, want absolute path", gotSocket)
		}
		if filepath.Dir(gotSocket) != wantSocketDir {
			t.Errorf("filepath.Dir(dev.USBSocketPath()) = %q, want %q", filepath.Dir(gotSocket), wantSocketDir)
		}
		if got := dev.USBAddress(); got != "usb:cid:7" {
			t.Errorf("dev.USBAddress() = %q, want %q", got, "usb:cid:7")
		}
		cfgAny, err := dev.TestConfig(true)
		if err != nil {
			t.Fatalf("dev.TestConfig(true) failed: %v", err)
		}
		cfg := cfgAny.(targets.FuchsiaTestbedConfig)
		if cfg.IPv4 != "" || cfg.IPv6 != "" {
			t.Errorf("TestConfig IPv4=%q IPv6=%q, want both empty", cfg.IPv4, cfg.IPv6)
		}
		if cfg.DeviceUSB != "usb:cid:7" {
			t.Errorf("TestConfig DeviceUSB = %q, want %q", cfg.DeviceUSB, "usb:cid:7")
		}
		if cfg.USBSocketPath != gotSocket {
			t.Errorf("TestConfig USBSocketPath = %q, want %q", cfg.USBSocketPath, gotSocket)
		}
	})

	t.Run("multiple_usb_targets_fails", func(t *testing.T) {
		t.Setenv(testrunnerconstants.TestOutDirEnvKey, t.TempDir())
		script := `#!/bin/bash
if [[ "$*" == *"check-ssh-keys"* ]]; then
  exit 0
fi
if [[ "$*" == *"usb-driver"* ]]; then
  trap 'exit 0' TERM INT
  while true; do sleep 1 & wait $!; done
fi
if [[ "$*" == *"target list"* ]]; then
  echo '[{"nodename":"dut-a","rcs_state":"N","serial":"USB9876","addresses":[{"type":"Usb","cid":7}]},{"nodename":"dut-b","rcs_state":"N","serial":"USB9876","addresses":[{"type":"Usb","cid":8}]}]'
  exit 0
fi
exit 0
`
		experiments := botanist.GetExperiments([]string{string(botanist.ForceFFXUSB)})
		dev := newStrictTestDevice(t, script, targets.DeviceConfig{
			SerialNumber: "USB9876",
			Network:      targets.NetworkProperties{Nodename: "fuchsia-usb-dut"},
		}, experiments)
		cmd := &RunCommand{
			usbTargetDiscoverBackoff: 10 * time.Millisecond,
			usbTargetDiscoverTimeout: 5 * time.Second,
		}
		cleanup, err := cmd.setupFFXUSBDriver(t.Context(), dev)
		cleanup()
		if err == nil {
			t.Fatal("setupFFXUSBDriver() with multiple USB targets = nil, want error")
		}
	})

	t.Run("missing_serial_fails", func(t *testing.T) {
		t.Setenv(testrunnerconstants.TestOutDirEnvKey, t.TempDir())
		experiments := botanist.GetExperiments([]string{string(botanist.ForceFFXUSB)})
		dev := newStrictTestDevice(t, "#!/bin/bash\nexit 0\n", targets.DeviceConfig{
			Network: targets.NetworkProperties{Nodename: "fuchsia-usb-dut"},
		}, experiments)
		cmd := &RunCommand{}
		cleanup, err := cmd.setupFFXUSBDriver(t.Context(), dev)
		cleanup()
		if err == nil {
			t.Fatal("setupFFXUSBDriver() with missing serial = nil, want error")
		}
	})
}

// mockFFXPrelude begins a mock ffx script that accepts `check-ssh-keys`, which
// runs when an FFXInstance is created, so the rest of the script is free to
// fail by default.
const mockFFXPrelude = "#!/bin/bash\nif [[ \"$*\" == *\"check-ssh-keys\"* ]]; then\n  exit 0\nfi\n"

// checkRegisteredTargets checks the contents of ffx's
// $SHARED_DATA/targets.json, which must not exist if want is nil.
func checkRegisteredTargets(t *testing.T, ffx *ffxutil.FFXInstance, want []ffxutil.RegisteredTarget) {
	t.Helper()
	got, err := ffx.LoadRegisteredTargets()
	if want == nil {
		if !errors.Is(err, os.ErrNotExist) {
			t.Errorf("LoadRegisteredTargets() = %+v, %v; want os.ErrNotExist", got, err)
		}
		return
	}
	if err != nil || !slices.Equal(got, want) {
		t.Errorf("LoadRegisteredTargets() = %+v, %v; want %+v", got, err, want)
	}
}

func TestIdentifyTarget(t *testing.T) {
	const defaultAddr = "192.168.42.10"
	for _, tc := range []struct {
		name          string
		initialSerial string
		emptyNodename bool
		scriptBody    string
		wantSerial    string
		wantNodename  string
		wantLogSubstr string
		// wantNoFFXCall means identifyTarget must not query ffx.
		wantNoFFXCall bool
	}{
		{
			name:         "populates_serial_when_rcs_ready",
			scriptBody:   `echo '[{"nodename":"fuchsia-5254-1234-1234","rcs_state":"Y","serial":"EM-B89E30F7F"}]'`,
			wantSerial:   "EM-B89E30F7F",
			wantNodename: "mock-target",
		},
		{
			name:          "discovers_nodename_and_serial_when_target_nodename_empty",
			emptyNodename: true,
			scriptBody:    `echo '[{"nodename":"fuchsia-gce-1","rcs_state":"Y","serial":"GC-998877"}]'`,
			wantSerial:    "GC-998877",
			wantNodename:  "fuchsia-gce-1",
		},
		{
			name:          "skips_ffx_call_when_serial_and_nodename_already_known",
			initialSerial: "STATIC-SER001",
			scriptBody:    "exit 1",
			wantSerial:    "STATIC-SER001",
			wantNodename:  "mock-target",
			wantNoFFXCall: true,
		},
		{
			name:          "logs_and_ignores_failing_ffx",
			emptyNodename: true,
			scriptBody:    "exit 1",
			wantLogSubstr: "unable to identify target <unknown>",
		},
		{
			name:          "logs_and_ignores_when_rcs_not_ready",
			scriptBody:    `echo '[{"nodename":"192.168.42.10","rcs_state":"N","serial":"<unknown>"}]'`,
			wantNodename:  "mock-target",
			wantLogSubstr: "RCS not ready",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			script := fmt.Sprintf(`if [[ "$*" != *"-c discovery.timeout=15000"* || "$*" != *"target list %s"* ]]; then
  echo "unexpected ffx args: $*" >&2
  exit 1
fi
%s
`, defaultAddr, tc.scriptBody)
			target := newFFXTarget(t, t.TempDir(), mockFFXPrelude+script, ffxutil.UseFFXStrict)
			target.emptyNodename = tc.emptyNodename
			if tc.initialSerial != "" {
				target.SetSerialNumber(tc.initialSerial)
			}
			var logBuf syncBuffer
			logCtx := logger.WithLogger(t.Context(), logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))
			identifyTarget(logCtx, target, defaultAddr)

			if got := target.Nodename(); got != tc.wantNodename {
				t.Errorf("identifyTarget() target.Nodename() = %q, want %q", got, tc.wantNodename)
			}
			if got := target.SerialNumber(); got != tc.wantSerial {
				t.Errorf("identifyTarget() target.SerialNumber() = %q, want %q", got, tc.wantSerial)
			}
			// The mock ffx fails every query in the skip case, which
			// identifyTarget would log.
			if tc.wantNoFFXCall && strings.Contains(logBuf.String(), "unable to identify target") {
				t.Errorf("identifyTarget() queried ffx; log = %q", logBuf.String())
			}
			if tc.wantLogSubstr != "" && !strings.Contains(logBuf.String(), tc.wantLogSubstr) {
				t.Errorf("identifyTarget() log = %q, want substring %q", logBuf.String(), tc.wantLogSubstr)
			}
		})
	}
}

func TestDispatchTests_UseSerialTargetID(t *testing.T) {
	for _, tc := range []struct {
		name           string
		experiments    []string
		emptyNodename  bool
		wantSerial     string
		wantTarget     string
		wantRegistered []ffxutil.RegisteredTarget
		// wantNodenameEnv is the FUCHSIA_NODENAME that testrunner runs with.
		wantNodenameEnv string
	}{
		{
			name:        "with_use_serial_target_id",
			experiments: []string{string(botanist.UseSerialTargetID)},
			wantSerial:  "EM-DISPATCH1",
			wantTarget:  "id:EM-DISPATCH1",
			wantRegistered: []ffxutil.RegisteredTarget{
				{Nodename: "mock-target", ID: "EM-DISPATCH1", Address: "127.0.0.1"},
			},
			wantNodenameEnv: "mock-target",
		},
		{
			// A discovered nodename is registered, but FUCHSIA_NODENAME stays
			// empty so testrunner does not take the target for a physical
			// device with a static nodename.
			name:          "discovered_nodename_not_exported",
			experiments:   []string{string(botanist.UseSerialTargetID)},
			emptyNodename: true,
			wantSerial:    "EM-DISPATCH1",
			wantTarget:    "id:EM-DISPATCH1",
			wantRegistered: []ffxutil.RegisteredTarget{
				{Nodename: "mock-target", ID: "EM-DISPATCH1", Address: "127.0.0.1"},
			},
		},
		{
			// Without the experiment, botanist neither queries the target nor
			// records it in testbed.json or $SHARED_DATA/targets.json.
			name:            "without_experiment",
			wantTarget:      "127.0.0.1",
			wantNodenameEnv: "mock-target",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			script := `if [[ "$*" == *"target list 127.0.0.1"* ]]; then
  echo '[{"nodename":"mock-target","rcs_state":"Y","serial":"EM-DISPATCH1"}]'
fi
`
			tmpDir := t.TempDir()
			target := newFFXTarget(t, tmpDir, mockFFXPrelude+script, ffxutil.UseFFXStrict)
			target.emptyNodename = tc.emptyNodename
			target.testConfigFunc = func(expectsSSH bool) (any, error) {
				return targets.TestbedConfig(target, expectsSSH, nil)
			}
			experiments := botanist.GetExperiments(tc.experiments)
			target.ffx.Experiments = experiments

			t.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpDir)
			t.Setenv(constants.NodenameEnvKey, "nodename-not-set-by-botanist")

			cmd := &RunCommand{
				ffxPath:           filepath.Join(tmpDir, "ffx"),
				expectsSSH:        true,
				productBundles:    "test-bundles",
				productBundleName: "test-bundle-name",
			}
			eg, ctx := errgroup.WithContext(t.Context())
			cmd.dispatchTests(
				ctx,
				func() {},
				eg,
				[]targets.Base{target},
				[]targets.FuchsiaTarget{target},
				target,
				nil,
				filepath.Join(tmpDir, "nonexistent-tests.json"),
				experiments,
			)
			_ = eg.Wait()

			if len(cmd.testrunnerOptions.TestbedConfig) != 1 {
				t.Fatalf("len(TestbedConfig) = %d, want 1", len(cmd.testrunnerOptions.TestbedConfig))
			}
			gotCfg, ok := cmd.testrunnerOptions.TestbedConfig[0].(targets.FuchsiaTestbedConfig)
			if !ok {
				t.Fatalf("TestbedConfig[0] has type %T, want targets.FuchsiaTestbedConfig", cmd.testrunnerOptions.TestbedConfig[0])
			}
			if gotCfg.DeviceSerial != tc.wantSerial {
				t.Errorf("TestbedConfig[0].DeviceSerial = %q, want %q", gotCfg.DeviceSerial, tc.wantSerial)
			}
			if got := target.SerialNumber(); got != tc.wantSerial {
				t.Errorf("target.SerialNumber() = %q, want %q", got, tc.wantSerial)
			}
			if got := target.GetFFX().GetTarget(); got != tc.wantTarget {
				t.Errorf("target.GetFFX().GetTarget() = %q, want %q", got, tc.wantTarget)
			}
			if got := os.Getenv(constants.NodenameEnvKey); got != tc.wantNodenameEnv {
				t.Errorf("%s = %q, want %q", constants.NodenameEnvKey, got, tc.wantNodenameEnv)
			}

			checkRegisteredTargets(t, target.ffx.FFXInstance, tc.wantRegistered)
		})
	}
}

func TestConfigureAndRegisterTarget_MultiTarget(t *testing.T) {
	script := `if [[ "$*" == *"target list 192.168.42.12"* ]]; then
  echo '[{"nodename":"fuchsia-gce-1","rcs_state":"Y","serial":"GC-998877"}]'
  exit 0
fi
exit 1
`
	base := newFFXTarget(t, t.TempDir(), mockFFXPrelude+script, ffxutil.UseFFXStrict)
	experiments := botanist.GetExperiments([]string{string(botanist.UseSerialTargetID)})

	cmd := &RunCommand{}
	for _, tc := range []struct {
		ip           string
		nodename     string
		serial       string
		wantNodename string
		wantSerial   string
		// wantTarget is the expected ffx target, or empty for the IP address.
		wantTarget string
	}{
		// A target with a statically known serial number and nodename.
		{ip: "192.168.42.10", nodename: "fuchsia-dev-1", serial: "SER001", wantNodename: "fuchsia-dev-1", wantSerial: "SER001", wantTarget: "id:SER001"},
		// A target identified at runtime.
		{ip: "192.168.42.12", wantNodename: "fuchsia-gce-1", wantSerial: "GC-998877", wantTarget: "id:GC-998877"},
		// A target that can't be identified keeps using its IP address.
		{ip: "192.168.42.13"},
	} {
		target := &MockTarget{
			ffx:           &targets.FFXInstance{FFXInstance: ffxutil.FFXWithTarget(base.ffx.FFXInstance, "")},
			nodename:      tc.nodename,
			emptyNodename: tc.nodename == "",
			serialNum:     tc.serial,
			sharedData:    base.sharedData,
		}
		addr := net.IPAddr{IP: net.ParseIP(tc.ip)}
		forceUSB, cleanup, err := cmd.configureAndRegisterTarget(t.Context(), target, addr, experiments)
		if err != nil {
			t.Fatalf("configureAndRegisterTarget(%s) failed: %s", tc.ip, err)
		}
		t.Cleanup(cleanup)
		if forceUSB {
			t.Errorf("configureAndRegisterTarget(%s) forceUSB = %v, want false", tc.ip, forceUSB)
		}
		wantTarget := cmp.Or(tc.wantTarget, tc.ip)
		if got := target.GetFFX().GetTarget(); got != wantTarget {
			t.Errorf("configureAndRegisterTarget(%s) GetTarget() = %q, want %q", tc.ip, got, wantTarget)
		}
		if got := target.Nodename(); got != tc.wantNodename {
			t.Errorf("configureAndRegisterTarget(%s) Nodename() = %q, want %q", tc.ip, got, tc.wantNodename)
		}
		if got := target.SerialNumber(); got != tc.wantSerial {
			t.Errorf("configureAndRegisterTarget(%s) SerialNumber() = %q, want %q", tc.ip, got, tc.wantSerial)
		}
	}

	checkRegisteredTargets(t, base.ffx.FFXInstance, []ffxutil.RegisteredTarget{
		{Nodename: "fuchsia-dev-1", ID: "SER001", Address: "192.168.42.10"},
		{Nodename: "fuchsia-gce-1", ID: "GC-998877", Address: "192.168.42.12"},
	})
}
