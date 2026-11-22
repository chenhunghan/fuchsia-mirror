// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testrunner

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"

	"go.fuchsia.dev/fuchsia/tools/botanist"
	botanistconstants "go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/targets"
	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	ffxutilconstants "go.fuchsia.dev/fuchsia/tools/lib/ffxutil/constants"
	"go.fuchsia.dev/fuchsia/tools/lib/iomisc"
	"go.fuchsia.dev/fuchsia/tools/lib/retry"
	"go.fuchsia.dev/fuchsia/tools/lib/subprocess"
	sshutilconstants "go.fuchsia.dev/fuchsia/tools/net/sshutil/constants"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"
)

type fakeSSHClient struct {
	reconnectErrs  []error
	reconnectCalls int
	runErrs        []error
	runCalls       int
	lastCmd        []string
	shuttingDown   chan struct{}
}

func (c *fakeSSHClient) Run(_ context.Context, command []string, _, _ io.Writer) error {
	c.runCalls++
	c.lastCmd = command
	if c.runErrs == nil {
		return nil
	}
	err, remainingErrs := c.runErrs[0], c.runErrs[1:]
	c.runErrs = remainingErrs
	return err
}

func (c *fakeSSHClient) Close() {
	if c.shuttingDown != nil {
		close(c.shuttingDown)
	}
}

func (c *fakeSSHClient) DisconnectionListener() <-chan struct{} {
	if c.shuttingDown == nil {
		c.shuttingDown = make(chan struct{})
	}
	return c.shuttingDown
}

func (c *fakeSSHClient) ReconnectWithBackoff(_ context.Context, _ retry.Backoff) error {
	c.reconnectCalls++
	if c.reconnectErrs == nil {
		return nil
	}
	err, remainingErrs := c.reconnectErrs[0], c.reconnectErrs[1:]
	c.reconnectErrs = remainingErrs
	return err
}

type fakeSerialClient struct {
	runCalls int
}

func (c *fakeSerialClient) runDiagnostics(_ context.Context) error {
	c.runCalls++
	return nil
}

type fakeCmdRunner struct {
	runErrs  []error
	runCalls int
	lastCmd  []string
	onRun    func(context.Context)
}

func (r *fakeCmdRunner) Run(ctx context.Context, command []string, _ subprocess.RunOptions) error {
	r.runCalls++
	r.lastCmd = command
	if r.onRun != nil {
		r.onRun(ctx)
	}
	if len(r.runErrs) == 0 {
		return ctx.Err()
	}
	err, remainingErrs := r.runErrs[0], r.runErrs[1:]
	r.runErrs = remainingErrs
	if err == nil {
		return ctx.Err()
	}
	return err
}

func TestSubprocessTester(t *testing.T) {
	tmpDir := t.TempDir()
	passingTest := filepath.Join("host_x64", "passing")
	passingProfile := filepath.Join("llvm-profile", passingTest, "default.profraw")
	failingTest := filepath.Join("host_x64", "failing")
	failingProfile := filepath.Join("llvm-profile", failingTest, "default.profraw")
	for _, profile := range []string{passingProfile, failingProfile} {
		abs := filepath.Join(tmpDir, profile)
		os.MkdirAll(filepath.Dir(abs), 0o700)
		f, err := os.Create(abs)
		if err != nil {
			t.Fatalf("failed to create profile: %s", err)
		}
		f.Close()
	}
	// Override tempdir creation function to ensure deterministic
	// command output.
	newTempDir = func(dir, pattern string) (string, error) {
		return "/newtmp", nil
	}

	cases := []struct {
		name           string
		test           build.Test
		runErrs        []error
		expectedStatus runtests.TestStatus
		useSandboxing  bool
		env            map[string]string
		wantCmd        []string
		wantDataSinks  runtests.DataSinkMap
	}{
		{
			name:           "no path",
			test:           build.Test{},
			expectedStatus: runtests.TestFailure,
		},
		{
			name:           "test passes with profile",
			test:           build.Test{Path: passingTest},
			expectedStatus: runtests.TestSuccess,
			wantCmd:        []string{"./" + passingTest},
			wantDataSinks: runtests.DataSinkMap{
				"llvm-profile": []runtests.DataSink{
					{
						Name: filepath.Base(passingProfile),
						File: passingProfile,
					},
				},
			},
		},
		{
			name:          "test passes with profile and host test sandboxing",
			test:          build.Test{Path: passingTest},
			useSandboxing: true,
			env: map[string]string{
				llvmProfileEnvKey: "fake/llvm/profile/path/p.profraw",
			},
			expectedStatus: runtests.TestSuccess,
			wantCmd: []string{
				"./fake_nsjail",
				"--disable_clone_newcgroup",
				"--disable_clone_newnet",
				"--quiet",
				"--bindmount_ro",
				"/bin:/bin",
				"--bindmount_ro",
				"/dev/kvm:/dev/kvm",
				"--bindmount_ro",
				"/dev/net/tun:/dev/net/tun",
				"--bindmount",
				"/dev/null:/dev/null",
				"--bindmount_ro",
				"/dev/urandom:/dev/urandom",
				"--bindmount_ro",
				"/dev/zero:/dev/zero",
				"--bindmount_ro",
				"/etc/alternatives/awk:/etc/alternatives/awk",
				"--bindmount_ro",
				"/etc/host.conf:/etc/host.conf",
				"--bindmount_ro",
				"/etc/hosts:/etc/hosts",
				"--bindmount_ro",
				"/etc/nsswitch.conf:/etc/nsswitch.conf",
				"--bindmount_ro",
				"/etc/passwd:/etc/passwd",
				"--bindmount_ro",
				"/etc/resolv.conf:/etc/resolv.conf",
				"--bindmount_ro",
				"/etc/ssl/certs:/etc/ssl/certs",
				"--bindmount_ro",
				"/usr/share/ca-certificates:/usr/share/ca-certificates",
				"--bindmount_ro",
				"/lib:/lib",
				"--bindmount_ro",
				"/lib64:/lib64",
				"--bindmount",
				"/newtmp:/tmp",
				"--bindmount",
				fmt.Sprintf("%s:%s", tmpDir, tmpDir),
				"--bindmount",
				fmt.Sprintf("%s/host_x64/passing:%s/host_x64/passing", tmpDir, tmpDir),
				"--bindmount_ro",
				"/usr/bin:/usr/bin",
				"--bindmount_ro",
				"/usr/lib:/usr/lib",
				"--bindmount_ro",
				"/usr/share/misc/magic.mgc:/usr/share/misc/magic.mgc",
				"--bindmount",
				"/dev/shm:/dev/shm",
				"--bindmount_ro",
				"/usr/share/tcltk:/usr/share/tcltk",
				"--bindmount_ro",
				"/usr/share/vulkan:/usr/share/vulkan",
				"--symlink",
				"/proc/self/fd:/dev/fd",
				"--rlimit_as",
				"soft",
				"--rlimit_fsize",
				"soft",
				"--rlimit_nofile",
				"soft",
				"--rlimit_nproc",
				"soft",
				"--env",
				"ANDROID_TMP=/tmp",
				"--env",
				"FFX_CONFIGS=",
				"--env",
				fmt.Sprintf("FUCHSIA_TEST_OUTDIR=%s/host_x64/passing", tmpDir),
				"--env",
				"HOME=/tmp",
				"--env",
				fmt.Sprintf("LLVM_PROFILE_FILE=%s/llvm-profile/host_x64/passing/%%m.profraw", tmpDir),
				"--env",
				"TEMP=/tmp",
				"--env",
				"TEMPDIR=/tmp",
				"--env",
				fmt.Sprintf("TEST_OUTPUT_SUMMARY_PATH=%s/test_output_summary/host_x64/passing/test_output_summary_path.json", tmpDir),
				"--env",
				"TMP=/tmp",
				"--env",
				"TMPDIR=/tmp",
				"--env",
				"XDG_CACHE_HOME=/tmp",
				"--env",
				"XDG_CONFIG_HOME=/tmp",
				"--env",
				"XDG_DATA_HOME=/tmp",
				"--env",
				"XDG_HOME=/tmp",
				"--env",
				"XDG_STATE_HOME=/tmp",
				"--",
				"./" + passingTest,
			},
			wantDataSinks: runtests.DataSinkMap{
				"llvm-profile": []runtests.DataSink{
					{
						Name: filepath.Base(passingProfile),
						File: passingProfile,
					},
				},
			},
		},
		{
			name:           "test passes without profile",
			test:           build.Test{Path: "uninstrumented_test"},
			expectedStatus: runtests.TestSuccess,
			wantCmd:        []string{"./uninstrumented_test"},
			wantDataSinks:  nil,
		},
		{
			name:           "test fails",
			test:           build.Test{Path: failingTest},
			runErrs:        []error{fmt.Errorf("test failed")},
			expectedStatus: runtests.TestFailure,
			wantCmd:        []string{"./" + failingTest},
			wantDataSinks: runtests.DataSinkMap{
				"llvm-profile": []runtests.DataSink{
					{
						Name: filepath.Base(failingProfile),
						File: failingProfile,
					},
				},
			},
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			runner := &fakeCmdRunner{
				runErrs: c.runErrs,
			}
			tester := SubprocessTester{
				localOutputDir: tmpDir,
				testRuns:       make(map[string]subprocessTestRun),
			}
			if c.useSandboxing {
				tester.sProps = &sandboxingProps{
					nsjailPath: "./fake_nsjail",
				}
				// Override the environment to make sure we have a deterministic
				// command.
				prevEnv := os.Environ()
				prev := make(map[string]string)
				for _, entry := range prevEnv {
					parts := strings.SplitN(entry, "=", 2)
					os.Unsetenv(parts[0])
					prev[parts[0]] = parts[1]
				}
				for k, v := range c.env {
					os.Setenv(k, v)
				}
				// Restore the environment after the test case.
				t.Cleanup(func() {
					for k := range c.env {
						os.Unsetenv(k)
					}
					for k, v := range prev {
						os.Setenv(k, v)
					}
				})
			}
			newRunner = func(dir string, env []string) cmdRunner {
				return runner
			}
			outDir := filepath.Join(tmpDir, c.test.Path)
			ctx := context.Background()
			test := testsharder.Test{Test: c.test}
			if err := tester.SetupTest(ctx, test); err != nil {
				t.Errorf("tester.SetupTest got error: %s, want nil", err)
			}
			testResult, err := tester.Test(ctx, test, io.Discard, io.Discard, outDir)
			testResult, err = tester.ProcessResult(ctx, test, outDir, testResult, err)
			if err != nil {
				t.Errorf("tester.Test got error: %s, want nil", err)
			}
			if c.expectedStatus != testResult.Status {
				t.Errorf("tester.Test got result: %s, want: %s", testResult.Status, c.expectedStatus)
			}
			if _, statErr := os.Stat(outDir); statErr != nil {
				t.Error("tester.Test did not create a readable outDir:", statErr)
			}
			var opts []cmp.Option
			// We sort the cmd when using sandboxing because the mounts are
			// ordered alphabetically.
			if c.useSandboxing {
				opts = append(opts, cmpopts.SortSlices(func(s, t string) bool {
					return s < t
				}))
			}
			if diff := cmp.Diff(c.wantCmd, runner.lastCmd, opts...); diff != "" {
				t.Errorf("Unexpected command run (-want +got):\n%s", diff)
			}

			sinks := testResult.DataSinks
			if diff := cmp.Diff(c.wantDataSinks, sinks); diff != "" {
				t.Errorf("Diff in data sinks (-want +got):\n%s", diff)
			}
		})
	}

	t.Run("power cycles target on exit code 40 in ProcessResult", func(t *testing.T) {
		exit40Err := exec.Command("sh", "-c", "exit 40").Run()
		t.Setenv(botanistconstants.NodenameEnvKey, "device-nodename")

		for _, tc := range []struct {
			name              string
			dmcPath           string
			powerCycleTimeout time.Duration
			hangPowerCycle    bool
			wantPowerCycle    bool
			wantErr           bool
		}{
			{
				name:           "succeeds with default power cycle timeout",
				dmcPath:        "/path/to/dmc",
				wantPowerCycle: true,
			},
			{
				name:              "fails when power cycle times out",
				dmcPath:           "/path/to/dmc",
				powerCycleTimeout: time.Nanosecond,
				hangPowerCycle:    true,
				wantPowerCycle:    true,
				wantErr:           true,
			},
			{
				name:           "skips power cycle when DMC_PATH is unset",
				dmcPath:        "",
				wantPowerCycle: false,
			},
		} {
			t.Run(tc.name, func(t *testing.T) {
				t.Setenv(constants.DMCPathEnvKey, tc.dmcPath)
				runner := &fakeCmdRunner{
					runErrs: []error{exit40Err},
				}
				oldNewRunner := newRunner
				t.Cleanup(func() {
					newRunner = oldNewRunner
				})
				newRunner = func(dir string, env []string) cmdRunner {
					return runner
				}

				tester := SubprocessTester{
					localOutputDir:    tmpDir,
					testRuns:          make(map[string]subprocessTestRun),
					powerCycleTimeout: tc.powerCycleTimeout,
				}
				outDir := filepath.Join(tmpDir, failingTest)
				test := testsharder.Test{
					Test: build.Test{Path: failingTest},
				}
				testResult, err := tester.Test(context.Background(), test, io.Discard, io.Discard, outDir)
				if err != nil {
					t.Fatalf("tester.Test got unexpected error: %s", err)
				}
				if runner.runCalls != 1 {
					t.Fatalf("tester.Test ran %d commands, want 1 (power cycle must run in ProcessResult)", runner.runCalls)
				}
				if tc.hangPowerCycle {
					runner.onRun = func(ctx context.Context) {
						<-ctx.Done()
					}
				}
				testResult, err = tester.ProcessResult(context.Background(), test, outDir, testResult, err)
				if tc.wantErr {
					if !errors.Is(err, context.DeadlineExceeded) {
						t.Fatalf("tester.ProcessResult got error %v, want %v", err, context.DeadlineExceeded)
					}
					return
				}
				if err != nil {
					t.Fatalf("tester.ProcessResult got unexpected error: %s", err)
				}
				if testResult.Status != runtests.TestFailure {
					t.Errorf("tester.ProcessResult got status %s, want %s", testResult.Status, runtests.TestFailure)
				}
				if !tc.wantPowerCycle {
					if runner.runCalls != 1 {
						t.Fatalf("after ProcessResult got %d commands run, want 1", runner.runCalls)
					}
					return
				}
				if runner.runCalls != 2 {
					t.Fatalf("after ProcessResult got %d commands run, want 2", runner.runCalls)
				}
				wantCmd := []string{"/path/to/dmc", "set-power-state", "--nodename", "device-nodename", "--state", "cycle"}
				if diff := cmp.Diff(wantCmd, runner.lastCmd); diff != "" {
					t.Errorf("Unexpected power cycle command (-want +got):\n%s", diff)
				}
			})
		}
	})

	t.Run("mounts usb_socket_path from testbed config when sandboxing", func(t *testing.T) {
		tbConfigPath := filepath.Join(tmpDir, "testbed_config.json")
		usbSocketPath := filepath.Join(tmpDir, "ffx_usb_driver_socketpath")
		serialSocketPath := filepath.Join(tmpDir, "serial_socket")
		sshKeyPath := filepath.Join(tmpDir, "ssh_key")
		tbJSON := fmt.Sprintf(`[{"serial_socket":%q,"ssh_key":%q,"usb_socket_path":%q}]`, serialSocketPath, sshKeyPath, usbSocketPath)
		if err := os.WriteFile(tbConfigPath, []byte(tbJSON), 0o600); err != nil {
			t.Fatal(err)
		}
		t.Setenv(botanistconstants.TestbedConfigEnvKey, tbConfigPath)

		runner := &fakeCmdRunner{}
		oldNewRunner := newRunner
		t.Cleanup(func() { newRunner = oldNewRunner })
		newRunner = func(dir string, env []string) cmdRunner { return runner }

		tester := SubprocessTester{
			localOutputDir: tmpDir,
			testRuns:       make(map[string]subprocessTestRun),
			sProps:         &sandboxingProps{nsjailPath: "./fake_nsjail"},
		}
		test := testsharder.Test{Test: build.Test{Path: "uninstrumented_test"}}
		outDir := filepath.Join(tmpDir, "usb_mount_out")
		if err := tester.SetupTest(context.Background(), test); err != nil {
			t.Fatalf("tester.SetupTest failed: %v", err)
		}
		if _, err := tester.Test(context.Background(), test, io.Discard, io.Discard, outDir); err != nil {
			t.Fatalf("tester.Test failed: %v", err)
		}

		wantUSBMount := fmt.Sprintf("%s:%s", usbSocketPath, usbSocketPath)
		foundWritableUSBMount := false
		for i := 0; i+1 < len(runner.lastCmd); i++ {
			if runner.lastCmd[i] == "--bindmount" && runner.lastCmd[i+1] == wantUSBMount {
				foundWritableUSBMount = true
				break
			}
		}
		if !foundWritableUSBMount {
			t.Errorf("expected writable --bindmount %s in nsjail command, got %v", wantUSBMount, runner.lastCmd)
		}
	})
}

type fakeDataSinkCopier struct {
	reconnectCalls int
	remoteDirs     map[string]struct{}
}

func (c *fakeDataSinkCopier) GetAllDataSinks(remoteDir string) ([]runtests.DataSink, error) {
	c.remoteDirs[remoteDir] = struct{}{}
	return []runtests.DataSink{{Name: "sink", File: filepath.Join("sink_type", "sink")}}, nil
}

func (c *fakeDataSinkCopier) GetReferences(remoteDir string) (map[string]runtests.DataSinkMap, error) {
	c.remoteDirs[remoteDir] = struct{}{}
	return map[string]runtests.DataSinkMap{}, nil
}

func (*fakeDataSinkCopier) Copy(_ []runtests.DataSinkMap, _ string) (runtests.DataSinkMap, error) {
	return runtests.DataSinkMap{}, nil
}

func (*fakeDataSinkCopier) RemoveAll(_ string) error {
	return nil
}

func (c *fakeDataSinkCopier) Reconnect() error {
	c.reconnectCalls++
	return nil
}

func (*fakeDataSinkCopier) Close() error {
	return nil
}

func TestFFXTester(t *testing.T) {
	cases := []struct {
		name                  string
		expectedStatus        runtests.TestStatus
		expectedFailureReason *runtests.FailureReason
		timeout               time.Duration
		connErr               bool
		experiments           []string
		output                string
	}{
		{
			name:           "run v2 tests with ffx",
			expectedStatus: runtests.TestSuccess,
			experiments:    []string{"use_ffx_test", "use_ffx_test_parallel"},
		},
		{
			name:           "ffx test fails",
			expectedStatus: runtests.TestFailure,
			experiments:    []string{"use_ffx_test"},
		},
		{
			name:                  "ffx test times out",
			expectedStatus:        runtests.TestAborted,
			expectedFailureReason: runtests.FailureReasonFromMessage("test timed out after 5s"),
			timeout:               5 * time.Second,
			experiments:           []string{"use_ffx_test"},
		},
		{
			name:           "ffx test skipped",
			expectedStatus: runtests.TestSkipped,
			experiments:    []string{"use_ffx_test"},
		},
		{
			name:           "ffx test returns ssh connection failure",
			expectedStatus: runtests.TestFailure,
			connErr:        true,
			experiments:    []string{"use_ffx_test"},
			output:         sshutilconstants.ProcessTerminatedMsg + "\n" + ffxutilconstants.ClientChannelClosedMsg,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			var outcome string
			switch c.expectedStatus {
			case runtests.TestSuccess:
				outcome = ffxutil.TestPassed
			case runtests.TestFailure:
				outcome = ffxutil.TestFailed
			case runtests.TestAborted:
				outcome = ffxutil.TestTimedOut
			case runtests.TestSkipped:
				outcome = ffxutil.TestNotStarted
			}
			ffx := &ffxutil.MockFFXInstance{
				TestOutcome: outcome,
				Output:      c.output,
			}
			localOutputDir := t.TempDir()
			experiments := botanist.GetExperiments(c.experiments)
			tester, err := NewFFXTester(context.Background(), FFXTesterOptions{Ffx: ffx, OutputDir: localOutputDir, Experiments: experiments})
			if err != nil {
				t.Fatalf("NewFFXTester got unexpected error: %s", err)
			}

			defer func() {
				if err := tester.Close(); err != nil {
					t.Errorf("Close returned error: %s", err)
				}
			}()

			test := testsharder.Test{
				Test:         build.Test{PackageURL: "fuchsia-pkg://foo#meta/bar.cm"},
				Runs:         1,
				RunAlgorithm: testsharder.StopOnSuccess,
				Timeout:      c.timeout,
			}
			ctx := context.Background()
			outDir := t.TempDir()
			if err := tester.SetupTest(ctx, test); err != nil {
				t.Errorf("tester.SetupTest got err: %s, want nil", err)
			}
			testResult, err := tester.Test(ctx, test, io.Discard, io.Discard, outDir)
			testResult, err = tester.ProcessResult(ctx, test, outDir, testResult, err)
			if !ffx.ContainsCmd("resolve", strings.Split(test.PackageURL, "#")[0]) {
				t.Errorf("failed to call `ffx target package resolve`, called: %s", ffx.CmdsCalled)
			}
			if c.connErr && !isConnectionError(err) {
				t.Errorf("tester.Test got err: %s, want conn err", err)
			} else if !c.connErr && err != nil {
				t.Errorf("tester.Test got unexpected error: %s", err)
			}
			if testResult.Status != c.expectedStatus {
				t.Errorf("tester.Test got result: %s, want result: %s", testResult.Status, c.expectedStatus)
			}
			if diff := cmp.Diff(c.expectedFailureReason, testResult.FailureReason); diff != "" {
				t.Errorf("Unexpected suite failure reason (-want +got):\n%s", diff)
			}

			testArgs := []string{}
			if experiments.Contains(botanist.UseFFXTestParallel) {
				testArgs = append(testArgs, "--experimental-parallel-execution", "8")
			}
			if !ffx.ContainsCmd("test", testArgs...) {
				t.Errorf("failed to call `ffx test`, called: %s", ffx.CmdsCalled)
			}
			numRuns := strings.Count(strings.Join(ffx.CmdsCalled, " "), "test:")
			if numRuns != 1 {
				t.Errorf("called `ffx test` %d times, expected 1", numRuns)
			}
			if c.expectedStatus == runtests.TestSkipped {
				if len(testResult.Cases) != 0 {
					t.Errorf("expected 0 test cases, got %d", len(testResult.Cases))
				}
			} else if len(testResult.Cases) != 1 {
				t.Errorf("expected 1 test case, got %d", len(testResult.Cases))
			} else {
				if testResult.Cases[0].Status != c.expectedStatus {
					t.Errorf("test case has status: %s, want: %s", testResult.Cases[0].Status, c.expectedStatus)
				}
				if diff := cmp.Diff(c.expectedFailureReason, testResult.Cases[0].FailureReason); diff != "" {
					t.Errorf("Unexpected case failure reason (-want +got):\n%s", diff)
				}
			}
			p := filepath.Join(t.TempDir(), "testrunner-cmd-test")
			if err = tester.RunSnapshot(ctx, p); err != nil {
				t.Errorf("failed to run snapshot: %s", err)
			}
			if !ffx.ContainsCmd("snapshot") {
				t.Errorf("failed to call `ffx target snapshot`, called: %s", ffx.CmdsCalled)
			}

			// Write the early-boot profiles so EnsureSinks() can find them.
			if _, err := ffx.WriteRunResult(build.TestList{}, filepath.Join(localOutputDir, "early-boot-profiles")); err != nil {
				t.Errorf("failed to write early-boot profiles: %s", err)
			}
			// Call EnsureSinks() for v2 tests to set the copier.remoteDir to the data output dir for v2 tests.
			// v1 tests will already have set the appropriate remoteDir value within Test().
			outputs := &TestOutputs{OutDir: t.TempDir()}
			if err = tester.EnsureSinks(ctx, []runtests.DataSinkMap{testResult.DataSinks}, outputs); err != nil {
				t.Errorf("failed to collect sinks: %s", err)
			}
			foundEarlyBootSinks := false
			for _, test := range outputs.Summary.Tests {
				if test.Name == "early_boot_sinks" {
					foundEarlyBootSinks = true
					if len(test.DataSinks["llvm-profile"]) != 1 {
						t.Errorf("got %d early boot sinks, want 1", len(test.DataSinks["llvm-profile"]))
					}
					break
				}
			}
			if !foundEarlyBootSinks {
				t.Errorf("failed to find early boot sinks")
			}
		})
	}

	t.Run("profile merging failed", func(t *testing.T) {
		ctx := context.Background()
		localOutputDir := t.TempDir()
		tester, err := NewFFXTester(ctx, FFXTesterOptions{Ffx: &ffxutil.MockFFXInstance{}, OutputDir: localOutputDir, LLVMProfdata: "llvm-profdata"})
		if err != nil {
			t.Fatalf("NewFFXTester got unexpected error: %s", err)
		}
		oldMergeProfiles := mergeProfiles
		defer func() {
			mergeProfiles = oldMergeProfiles
		}()
		mergeProfiles = func(_ context.Context, _ string, _ []string, _, _, _ string, _ int, _ []string, _ string, _ []string) error {
			return fmt.Errorf("failed to merge")
		}
		sinkDirA := t.TempDir()
		sinkDirB := t.TempDir()
		sinkFile := filepath.Join("llvm-profile", "sinkfile")
		for _, dir := range []string{sinkDirA, sinkDirB} {
			profile := filepath.Join(dir, sinkFile)
			if err := os.MkdirAll(filepath.Dir(profile), os.ModePerm); err != nil {
				t.Fatalf("failed to create dir of %s: %s", profile, err)
			}
			if err := os.WriteFile(filepath.Join(dir, sinkFile), []byte("data"), os.ModePerm); err != nil {
				t.Fatalf("failed to write profile: %s", err)
			}
		}
		dest, err := tester.moveProfileToOutputDir(ctx, sinkDirA, sinkFile, "test1")
		expectedDest := sinkFile
		if err != nil {
			t.Errorf("failed to move profile %s to %s: %s", sinkFile, expectedDest, err)
		} else if dest != expectedDest {
			t.Errorf("got dest %s, want %s", dest, expectedDest)
		}
		if _, err := os.Stat(filepath.Join(localOutputDir, "v2", expectedDest)); err != nil {
			t.Errorf("failed to move proifle to v2 dir: %s", err)
		}
		dest, err = tester.moveProfileToOutputDir(ctx, sinkDirB, sinkFile, "test2")
		expectedDest = filepath.Join("llvm-profile", "test2", "sinkfile")
		if err != nil {
			t.Errorf("failed to move profile %s to %s: %s", sinkFile, expectedDest, err)
		} else if dest != expectedDest {
			t.Errorf("got dest %s, want %s", dest, expectedDest)
		}
		if _, err := os.Stat(filepath.Join(localOutputDir, "v2", expectedDest)); err != nil {
			t.Errorf("failed to move proifle to v2 dir: %s", err)
		}
	})
}

// Creates pair of ReadWriteClosers that mimics the relationship between serial
// and socket i/o. Implemented with in-memory pipes, the input of one can
// synchronously by read as the output of the other.
func serialAndSocket() (*joinedPipeEnds, *joinedPipeEnds) {
	rSerial, wSocket := io.Pipe()
	rSocket, wSerial := io.Pipe()
	serial := &joinedPipeEnds{rSerial, wSerial}
	socket := &joinedPipeEnds{rSocket, wSocket}
	return serial, socket
}

type fakeSerialServer struct {
	received       []byte
	shutdownString string
	socketPath     string
	listeningChan  chan bool
}

func (s *fakeSerialServer) Serve() error {
	listener, err := net.Listen("unix", s.socketPath)
	if err != nil {
		s.listeningChan <- false
		return fmt.Errorf("Listen(%s) failed: %w", s.socketPath, err)
	}
	defer listener.Close()
	s.listeningChan <- true
	conn, err := listener.Accept()
	if err != nil {
		return fmt.Errorf("Accept() failed: %w", err)
	}
	defer conn.Close()
	// Signal we're ready to accept input.
	if _, err := conn.Write([]byte(serialConsoleCursor)); err != nil {
		return fmt.Errorf("conn.Write() failed: %w", err)
	}
	reader := iomisc.NewMatchingReader(conn, []byte(s.shutdownString))
	for {
		buf := make([]byte, 1024)
		bytesRead, err := reader.Read(buf)
		s.received = append(s.received, buf[:bytesRead]...)
		if err != nil {
			if err == io.EOF {
				return nil
			}
			return fmt.Errorf("conn.Read() failed: %w", err)
		}
	}
}

// fakeContext conforms to context.Context but lets us control the return
// value of Err().
type fakeContext struct {
	sync.Mutex
	err error
}

func (ctx *fakeContext) Deadline() (time.Time, bool) {
	return time.Time{}, false
}

func (ctx *fakeContext) Done() <-chan struct{} {
	return make(chan struct{})
}

func (ctx *fakeContext) Err() error {
	ctx.Lock()
	defer ctx.Unlock()
	return ctx.err
}

func (ctx *fakeContext) SetErr(err error) {
	ctx.Lock()
	ctx.err = err
	ctx.Unlock()
}

func (ctx *fakeContext) Value(key any) any {
	return nil
}

func TestSerialTester(t *testing.T) {
	fooTest := testsharder.Test{
		Test: build.Test{
			Name: "myfoo",
			Path: "foo",
		},
	}

	pkgTest := testsharder.Test{
		Test: build.Test{
			Name:       "fuchsia-boot:///myfoo#meta/bar.cm",
			PackageURL: "fuchsia-boot:///myfoo#meta/bar.cm",
		},
	}

	fooExpectedCmd := "\r\nruntests foo\r\n"
	pkgExpectedCmd := "\r\nrun-test-suite --filter-ansi fuchsia-boot:///myfoo#meta/bar.cm\r\n"

	fooStarted := runtests.StartedSignature + fooTest.Name
	pkgStarted := "Running test '" + pkgTest.PackageURL + "'"

	cases := []struct {
		name           string
		test           testsharder.Test
		expectedCmd    string
		expectedStatus runtests.TestStatus
		wantErr        bool
		wantRetry      bool
		startedStr     string
		returnStr      string
	}{
		{
			name:           "test passes",
			test:           fooTest,
			expectedCmd:    fooExpectedCmd,
			expectedStatus: runtests.TestSuccess,
			startedStr:     fooStarted,
			returnStr:      runtests.SuccessSignature + fooTest.Name,
		},
		{
			name:           "packaged test passes",
			test:           pkgTest,
			expectedCmd:    pkgExpectedCmd,
			expectedStatus: runtests.TestSuccess,
			startedStr:     pkgStarted,
			returnStr:      pkgTest.PackageURL + " completed with result: PASSED",
		},
		{
			name:           "test fails",
			test:           fooTest,
			expectedCmd:    fooExpectedCmd,
			expectedStatus: runtests.TestFailure,
			startedStr:     fooStarted,
			returnStr:      runtests.FailureSignature + fooTest.Name,
		},
		{
			name:           "packaged test fails",
			test:           pkgTest,
			expectedCmd:    pkgExpectedCmd,
			expectedStatus: runtests.TestFailure,
			startedStr:     pkgStarted,
			returnStr:      pkgTest.PackageURL + " completed with result: FAILED",
		},
		{
			name:           "packaged test skipped",
			test:           pkgTest,
			expectedCmd:    pkgExpectedCmd,
			expectedStatus: runtests.TestSkipped,
			startedStr:     pkgStarted,
			returnStr:      pkgTest.PackageURL + " completed with result: SKIPPED",
		},
		{
			name:           "packaged test canceled",
			test:           pkgTest,
			expectedCmd:    pkgExpectedCmd,
			expectedStatus: runtests.TestAborted,
			startedStr:     pkgStarted,
			returnStr:      pkgTest.PackageURL + " completed with result: CANCELLED",
		},
		{
			name:           "test does not start on first try",
			test:           fooTest,
			expectedCmd:    fooExpectedCmd,
			expectedStatus: runtests.TestSuccess,
			wantRetry:      true,
			startedStr:     fooStarted,
			returnStr:      runtests.SuccessSignature + fooTest.Name,
		},
		{
			name:           "packaged test does not start on first try",
			test:           pkgTest,
			expectedCmd:    pkgExpectedCmd,
			expectedStatus: runtests.TestSuccess,
			wantRetry:      true,
			startedStr:     pkgStarted,
			returnStr:      pkgTest.PackageURL + " completed with result: PASSED",
		},
		{
			name:        "test returns fatal err",
			test:        fooTest,
			expectedCmd: fooExpectedCmd,
			wantErr:     true,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctx := context.Background()
			serial, socket := serialAndSocket()
			defer socket.Close()
			defer serial.Close()

			tester := FuchsiaSerialTester{socket: socket}
			expectedCmd := tc.expectedCmd

			oldNewTestStartedContext := newTestStartedContext
			defer func() {
				newTestStartedContext = oldNewTestStartedContext
			}()
			var fakeTestStartedContext fakeContext
			fakeTestStartedCancel := func() {}
			newTestStartedContext = func(ctx context.Context) (context.Context, context.CancelFunc) {
				return &fakeTestStartedContext, fakeTestStartedCancel
			}

			type testResult struct {
				result *runtests.TestDetails
				err    error
			}
			results := make(chan testResult)
			var stdout bytes.Buffer
			go func() {
				if err := tester.SetupTest(ctx, tc.test); err != nil {
					t.Errorf("tester.SetupTest got error: %s, want nil", err)
				}
				result, err := tester.Test(ctx, tc.test, &stdout, io.Discard, "unused-out-dir")
				results <- testResult{result, err}
			}()

			expectedTries := 1
			if tc.wantRetry {
				// Ensure tester times out waiting for the test to start.
				fakeTestStartedContext.SetErr(context.DeadlineExceeded)
				expectedTries = 2
			}

			// The write to the socket will block until we read from serial.
			buff := make([]byte, len(expectedCmd))
			for i := 0; i < expectedTries; i++ {
				if _, err := io.ReadFull(serial, buff); err != nil {
					t.Errorf("error reading from serial: %s", err)
				} else if string(buff) != expectedCmd {
					t.Errorf("unexpected command: %s", buff)
				}
			}

			if tc.wantRetry {
				fakeTestStartedContext.SetErr(nil)

				// At this point, the tester is either waiting for the startedSignature
				// (we set the startedContext error to nil before it started waiting for
				// it) or it's waiting to write to the socket again for its next retry.
				// In the latter case, we should read the remaining bytes from serial to
				// unblock the tester's write to the socket.
				// Put in a goroutine to not block in the case that there's nothing to read.
				// This will return when the serial is closed at the end of the test.
				go func() {
					if b, err := io.ReadAll(serial); err != nil {
						fmt.Println(err)
						results <- testResult{nil, fmt.Errorf("error reading from serial: %w", err)}
					} else if string(b) != expectedCmd || string(b) != "" {
						results <- testResult{nil, fmt.Errorf("unexpected serial input: %s", string(buff))}
					}
				}()
			}

			// At this point, the tester will be blocked reading from the socket.
			started := tc.startedStr
			testReturn := tc.returnStr

			if tc.wantErr {
				// Close the serial connection so that the tester returns a fatal error.
				serial.Close()
			} else {
				if _, err := io.WriteString(serial, started); err != nil {
					t.Errorf("failed to write %s to serial", started)
				}
				if _, err := io.WriteString(serial, testReturn); err != nil {
					t.Errorf("failed to write %s to serial", testReturn)
				}
			}

			select {
			case r := <-results:
				if gotErr := r.err != nil; gotErr != tc.wantErr {
					t.Errorf("test got err: %s, want err: %t", r.err, tc.wantErr)
				}
				if tc.wantErr && (!errors.Is(r.err, io.EOF) || !isConnectionError(r.err)) {
					t.Errorf("test got err: %v (%T), want connectionError wrapping io.EOF", r.err, r.err)
				}
				if !tc.wantErr && r.result == nil {
					t.Error("got nil test result")
				}
				if !tc.wantErr && tc.expectedStatus != r.result.Status {
					t.Errorf("test got result: %s, want: %s", r.result.Status, tc.expectedStatus)
				}
			}
			stdoutBytes := stdout.Bytes()
			if !tc.wantErr && !bytes.Contains(stdoutBytes, []byte(started+testReturn)) {
				t.Errorf("Expected stdout to contain %q, got %q", started+testReturn, string(stdoutBytes))
			}
		})
	}
}

type matchNotifyWriter struct {
	mu      sync.Mutex
	buf     bytes.Buffer
	target  string
	once    sync.Once
	matched chan struct{}
}

func (w *matchNotifyWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	n, err := w.buf.Write(p)
	if strings.Contains(w.buf.String(), w.target) {
		w.once.Do(func() {
			close(w.matched)
		})
	}
	return n, err
}

func handshakeMockSerialConn(ctx context.Context, conn net.Conn, testerReady <-chan struct{}) error {
	if _, err := iomisc.ReadUntilMatchString(ctx, conn, "\r\n"); err != nil {
		return fmt.Errorf("reading initial prompt trigger: %w", err)
	}
	if _, err := io.WriteString(conn, "$ "); err != nil {
		return fmt.Errorf("writing console prompt: %w", err)
	}
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-testerReady:
			return nil
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			if _, err := io.WriteString(conn, "[driver_manager.cm] INFO: Bootup completed\n"); err != nil {
				return fmt.Errorf("writing bootup completed: %w", err)
			}
		}
	}
}

func TestSerialTesterMidTestEOFAndReconnect(t *testing.T) {
	cases := []struct {
		name        string
		test        testsharder.Test
		expectedCmd string
		startedStr  string
		passStr     string
	}{
		{
			name: "runtests",
			test: testsharder.Test{
				Test: build.Test{
					Name: "myfoo",
					Path: "foo",
				},
			},
			expectedCmd: "runtests foo\r\n",
			startedStr:  runtests.StartedSignature + "myfoo",
			passStr:     runtests.SuccessSignature + "myfoo",
		},
		{
			name: "run-test-suite",
			test: testsharder.Test{
				Test: build.Test{
					Name:       "fuchsia-boot:///myfoo#meta/bar.cm",
					PackageURL: "fuchsia-boot:///myfoo#meta/bar.cm",
				},
			},
			expectedCmd: "run-test-suite --filter-ansi fuchsia-boot:///myfoo#meta/bar.cm\r\n",
			startedStr:  "Running test 'fuchsia-boot:///myfoo#meta/bar.cm'",
			passStr:     "fuchsia-boot:///myfoo#meta/bar.cm completed with result: PASSED",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
			defer cancel()

			// Use a short directory name instead of t.TempDir() to stay well below
			// Unix domain socket sun_path limits (103 chars on macOS, 107 on Linux).
			sockDir, err := os.MkdirTemp("", "mux-")
			if err != nil {
				t.Fatalf("os.MkdirTemp() failed: %v", err)
			}
			t.Cleanup(func() { os.RemoveAll(sockDir) })
			sockPath := filepath.Join(sockDir, "s")
			listener, err := net.Listen("unix", sockPath)
			if err != nil {
				t.Fatalf("net.Listen(%q) failed: %v", sockPath, err)
			}
			defer listener.Close()

			const partialLine = "partial test output before _mux disconnect\n"
			stdout1 := &matchNotifyWriter{
				target:  partialLine,
				matched: make(chan struct{}),
			}

			testerReady := make(chan struct{})
			handshakeDone := make(chan error, 1)
			test2Done := make(chan struct{})
			serverErrCh := make(chan error, 1)

			serveConn1 := func() error {
				conn1, err := listener.Accept()
				if err != nil {
					err = fmt.Errorf("Accept(conn1) failed: %w", err)
					handshakeDone <- err
					return err
				}
				// Close mock _mux socket mid-test without emitting pass signature.
				defer conn1.Close()
				if err := handshakeMockSerialConn(ctx, conn1, testerReady); err != nil {
					handshakeDone <- err
					return err
				}
				handshakeDone <- nil

				if _, err := iomisc.ReadUntilMatchString(ctx, conn1, tc.expectedCmd); err != nil {
					return fmt.Errorf("conn1 reading test command: %w", err)
				}
				if _, err := io.WriteString(conn1, tc.startedStr+"\n"+partialLine); err != nil {
					return fmt.Errorf("conn1 writing partial output: %w", err)
				}
				select {
				case <-stdout1.matched:
					return nil
				case <-ctx.Done():
					return fmt.Errorf("timed out waiting for partial output to reach stdout1: %w", ctx.Err())
				}
			}
			serveConn2 := func() error {
				conn2, err := listener.Accept()
				if err != nil {
					return nil
				}
				defer conn2.Close()

				if _, err := io.WriteString(conn2, "$ "); err != nil {
					return fmt.Errorf("conn2 writing cursor: %w", err)
				}
				if _, err := iomisc.ReadUntilMatchString(ctx, conn2, tc.expectedCmd); err != nil {
					return fmt.Errorf("conn2 reading test command: %w", err)
				}
				if _, err := io.WriteString(conn2, tc.startedStr+"\n"+tc.passStr+"\n"); err != nil {
					return fmt.Errorf("conn2 writing test pass output: %w", err)
				}
				select {
				case <-test2Done:
				case <-ctx.Done():
				}
				return nil
			}
			go func() {
				if err := serveConn1(); err != nil {
					serverErrCh <- err
					return
				}
				serverErrCh <- serveConn2()
			}()

			tester, err := NewFuchsiaSerialTester(ctx, sockPath)
			if err != nil {
				close(testerReady)
				t.Fatalf("NewFuchsiaSerialTester() failed: %v", err)
			}
			defer tester.Close()
			close(testerReady)
			if err := <-handshakeDone; err != nil {
				t.Fatalf("mock _mux handshake failed: %v", err)
			}

			// 1. Mid-test _mux EOF must return a non-nil connectionError wrapping io.EOF
			// (and nil TestDetails), rather than err == nil with a misleading test FAIL result.
			res1, err1 := tester.Test(ctx, tc.test, stdout1, io.Discard, "unused-out-dir")
			if !errors.Is(err1, io.EOF) || !isConnectionError(err1) {
				t.Errorf("Test() after mid-test _mux EOF returned err=%v (%T), res=%+v; want connectionError wrapping io.EOF", err1, err1, res1)
			}
			if res1 != nil {
				t.Errorf("Test() after mid-test _mux EOF returned non-nil TestDetails (status=%q); want nil", res1.Status)
			}

			// 2. Reconnect() must re-dial the Unix socket path so a subsequent Test() succeeds
			// instead of failing with write: broken pipe.
			if err := tester.Reconnect(ctx); err != nil {
				t.Errorf("Reconnect() returned error: %v", err)
			}

			res2, err2 := tester.Test(ctx, tc.test, io.Discard, io.Discard, "unused-out-dir")
			close(test2Done)
			if err2 != nil {
				t.Errorf("Test() after Reconnect() returned error: %v; want nil", err2)
			}
			if res2 == nil {
				t.Errorf("Test() after Reconnect() returned nil TestDetails; want status=%q", runtests.TestSuccess)
			} else if res2.Status != runtests.TestSuccess {
				t.Errorf("Test() after Reconnect() status = %q, want %q", res2.Status, runtests.TestSuccess)
			}

			listener.Close()
			if err := <-serverErrCh; err != nil {
				t.Errorf("mock _mux server error: %v", err)
			}
		})
	}
}

type onEOFSocket struct {
	socketConn
	onEOF func()
}

func (s *onEOFSocket) Read(p []byte) (int, error) {
	n, err := s.socketConn.Read(p)
	if errors.Is(err, io.EOF) && s.onEOF != nil {
		s.onEOF()
	}
	return n, err
}

func TestSerialTesterReconnectErrors(t *testing.T) {
	ctx := context.Background()
	test := testsharder.Test{Test: build.Test{Name: "myfoo", Path: "foo"}}

	t.Run("write failure on closed socket", func(t *testing.T) {
		// Write failure in serial.RunCommands (e.g. socket closed between tests)
		// must return a connectionError so testrunner attempts Reconnect().
		serial, socket := serialAndSocket()
		serial.Close()
		socket.Close()
		tester := &FuchsiaSerialTester{socket: socket}
		if res, err := tester.Test(ctx, test, io.Discard, io.Discard, "unused-out-dir"); res != nil || !isConnectionError(err) {
			t.Errorf("Test() on closed socket = (%v, %v), want (nil, connectionError)", res, err)
		}
	})

	t.Run("mid-test ECONNRESET returns connectionError", func(t *testing.T) {
		serial, socket := serialAndSocket()
		defer serial.Close()
		defer socket.Close()

		tester := &FuchsiaSerialTester{socket: socket}
		go func() {
			expectedCmd := "\r\nruntests foo\r\n"
			buf := make([]byte, len(expectedCmd))
			if _, err := io.ReadFull(serial, buf); err != nil {
				return
			}
			if _, err := io.WriteString(serial, runtests.StartedSignature+test.Name+"\n"); err != nil {
				return
			}
			// Second write completes only once Test() reads from socket in TestPassed.
			_, _ = io.WriteString(serial, "running before reset\n")
			_ = serial.w.CloseWithError(syscall.ECONNRESET)
		}()
		res, err := tester.Test(ctx, test, io.Discard, io.Discard, "unused-out-dir")
		if res != nil || !isConnectionError(err) || !errors.Is(err, syscall.ECONNRESET) {
			t.Errorf("Test() after mid-test ECONNRESET = (%v, %v), want (nil, connectionError wrapping ECONNRESET)", res, err)
		}
	})

	t.Run("canceled context before start", func(t *testing.T) {
		// Canceled context must return context.Canceled rather than connectionError
		// or a failed TestDetails, both in Test() (even with nil socket) and Reconnect().
		canceledCtx, cancel := context.WithCancel(ctx)
		cancel()
		serial, socket := serialAndSocket()
		defer serial.Close()
		defer socket.Close()
		tester := &FuchsiaSerialTester{
			socket:           socket,
			serialSocketPath: filepath.Join(t.TempDir(), "nonexistent.sock"),
		}
		if res, err := tester.Test(canceledCtx, test, io.Discard, io.Discard, "unused-out-dir"); res != nil || !errors.Is(err, context.Canceled) || isConnectionError(err) {
			t.Errorf("Test() with canceled ctx before start = (%v, %v), want (nil, context.Canceled) and not connectionError", res, err)
		}
		nilSocketTester := &FuchsiaSerialTester{}
		if res, err := nilSocketTester.Test(canceledCtx, test, io.Discard, io.Discard, "unused-out-dir"); res != nil || !errors.Is(err, context.Canceled) || isConnectionError(err) {
			t.Errorf("Test() with canceled ctx and nil socket = (%v, %v), want (nil, context.Canceled) and not connectionError", res, err)
		}
		if err := tester.Reconnect(canceledCtx); !errors.Is(err, context.Canceled) {
			t.Errorf("Reconnect() with canceled ctx = %v, want context.Canceled", err)
		}
	})

	t.Run("canceled mid-test on EOF", func(t *testing.T) {
		// Mid-test cancellation while socket closes concurrently must return
		// context.Canceled rather than connectionError{io.EOF} or a test failure.
		// Uses fakeContext (whose Done channel never closes) and onEOFSocket so
		// context cancellation is set inside Read right as io.EOF is returned,
		// deterministically exercising handleReadErr's ctx.Err() precedence.
		serial, socket := serialAndSocket()
		defer serial.Close()
		defer socket.Close()
		var midCtx fakeContext
		tester := &FuchsiaSerialTester{
			socket: &onEOFSocket{
				socketConn: socket,
				onEOF:      func() { midCtx.SetErr(context.Canceled) },
			},
		}
		go func() {
			expectedCmd := "\r\nruntests foo\r\n"
			cmdBuf := make([]byte, len(expectedCmd))
			if _, err := io.ReadFull(serial, cmdBuf); err != nil {
				return
			}
			if _, err := io.WriteString(serial, runtests.StartedSignature+test.Name+"\n"); err != nil {
				return
			}
			// Second write completes only once Test() reads from socket in TestPassed.
			_, _ = io.WriteString(serial, "running before cancel\n")
			serial.Close()
		}()
		res, err := tester.Test(&midCtx, test, io.Discard, io.Discard, "unused-out-dir")
		if res != nil || !errors.Is(err, context.Canceled) || isConnectionError(err) {
			t.Errorf("Test() canceled mid-test = (%v, %v), want (nil, context.Canceled) and not connectionError", res, err)
		}
	})

	t.Run("empty serialSocketPath", func(t *testing.T) {
		serial, socket := serialAndSocket()
		defer serial.Close()
		defer socket.Close()
		tester := &FuchsiaSerialTester{socket: socket}
		if err := tester.Reconnect(ctx); err == nil {
			t.Error("Reconnect() with empty serialSocketPath returned nil, want error")
		}
	})

	t.Run("failed reconnect leaves Close and Test safe", func(t *testing.T) {
		serial, socket := serialAndSocket()
		defer serial.Close()
		defer socket.Close()
		tester := &FuchsiaSerialTester{
			socket:           socket,
			serialSocketPath: filepath.Join(t.TempDir(), "nonexistent.sock"),
		}
		if err := tester.Reconnect(ctx); err == nil {
			t.Error("Reconnect() to nonexistent socket returned nil, want error")
		}
		if err := tester.Close(); err != nil {
			t.Errorf("Close() after failed Reconnect() returned error: %v", err)
		}
		res, err := tester.Test(ctx, test, io.Discard, io.Discard, "unused-out-dir")
		if res != nil || !isConnectionError(err) {
			t.Errorf("Test() after failed Reconnect() = (%v, %v), want (nil, connectionError)", res, err)
		}
	})
}

func longKernelLog(numChars int) string {
	var kernelLog strings.Builder
	kernelLog.WriteString("[123.456]")
	for i := 0; i < numChars; i++ {
		kernelLog.WriteString("a")
	}
	return kernelLog.String() + "\n"
}

func longPotentialKernelLog(numChars int) string {
	var log strings.Builder
	log.WriteString("[")
	for i := 0; i < numChars; i++ {
		log.WriteString("1")
	}
	return log.String()
}

type hangForeverReader struct {
	readCalled chan struct{}
	readChan   chan struct{}
}

func (r *hangForeverReader) Read(buf []byte) (int, error) {
	r.readCalled <- struct{}{}
	<-r.readChan
	return 0, nil
}

func TestParseOutKernelReader(t *testing.T) {
	cases := []struct {
		name           string
		output         string
		expectedOutput string
	}{
		{
			name:           "no kernel logs",
			output:         "line1\n[line2]output\nline3[output]\nline4[out",
			expectedOutput: "line1\n[line2]output\nline3[output]\nline4[out",
		}, {
			name:           "with kernel logs",
			output:         "line1\n[123.456]kernel line1\noutput[123.456]kernel line2\noutput continued [bracket] output [123.456] kernel [line3]\noutput continued[123]\n",
			expectedOutput: "line1\noutputoutput continued [bracket] output output continued",
		}, {
			name:           "long kernel log",
			output:         "line1" + longKernelLog(2000) + "output continued\n",
			expectedOutput: "line1output continued\n",
		}, {
			name: "long potential kernel log",
			// This should test the proper reading of logs which need to store both
			// kernel and non-kernel logs for later processing.
			output:         "line1" + longPotentialKernelLog(2000) + "\noutput continued[123",
			expectedOutput: "line1" + longPotentialKernelLog(2000) + "\noutput continued[123",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			r := &parseOutKernelReader{
				ctx:    context.Background(),
				reader: strings.NewReader(c.output),
			}
			b, err := io.ReadAll(r)
			if err != nil {
				t.Fatal(err)
			}
			if string(b) != c.expectedOutput {
				t.Errorf("expected: %s, got: %s", c.expectedOutput, string(b))
			}
		})
	}
	t.Run("canceled context", func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		reader := &hangForeverReader{readCalled: make(chan struct{}), readChan: make(chan struct{})}
		r := &parseOutKernelReader{
			ctx:    ctx,
			reader: reader,
		}
		errs := make(chan error)
		go func() {
			_, err := io.ReadAll(r)
			errs <- err
		}()
		// Wait for Read() to be called before canceling the context.
		<-reader.readCalled
		cancel()
		if err := <-errs; !errors.Is(err, ctx.Err()) {
			t.Errorf("expected: %s, got: %s", ctx.Err(), err)
		}
		close(reader.readChan)
	})
}

func TestCommandForTest(t *testing.T) {
	cases := []struct {
		name      string
		test      testsharder.Test
		useSerial bool
		timeout   time.Duration
		expected  []string
		wantErr   bool
	}{
		{
			name:      "use serial URL",
			useSerial: true,
			test: testsharder.Test{
				Test: build.Test{
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
			},
			expected: []string{"run-test-suite", "--filter-ansi", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "use serial path",
			useSerial: true,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				},
			},
			expected: []string{"runtests", "/path/to/test"},
		},
		{
			name:      "use serial timeout",
			useSerial: true,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				},
			},
			timeout:  time.Second,
			expected: []string{"runtests", "-i", "1", "/path/to/test"},
		},
		{
			name:      "system path",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				},
			},
			wantErr: true,
		},
		{
			name:      "components v2",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
			},
			expected: []string{"run-test-suite", "--filter-ansi", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "components v2 no parallel",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
			},
			expected: []string{"run-test-suite", "--filter-ansi", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "components v2 parallel",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
					Parallel:   2,
				},
			},
			expected: []string{"run-test-suite", "--filter-ansi", "--parallel", "2", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "components v2 timeout",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
			},
			timeout:  time.Second,
			expected: []string{"run-test-suite", "--filter-ansi", "--timeout", "1", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "components v2 with realm",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
				Realm: "/some/realm",
			},
			expected: []string{"run-test-suite", "--filter-ansi", "--realm", "/some/realm", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:      "components v2 with empty realm",
			useSerial: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				},
				Realm: "",
			},
			expected: []string{"run-test-suite", "--filter-ansi", "fuchsia-pkg://example.com/test.cm"},
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			command, err := commandForTest(&c.test, c.useSerial, c.timeout)
			if err == nil {
				if c.wantErr {
					t.Errorf("commandForTest returned nil error, want non-nil error")
				}
			} else if !c.wantErr {
				t.Errorf("commandForTest returned error: %s, want nil", err)
			}
			if diff := cmp.Diff(c.expected, command); diff != "" {
				t.Errorf("unexpected command (-want +got):\n%s", diff)
			}
		})
	}
}

type joinedPipeEnds struct {
	r *io.PipeReader
	w *io.PipeWriter
}

func (pe *joinedPipeEnds) Read(p []byte) (int, error) {
	return pe.r.Read(p)
}

func (pe *joinedPipeEnds) Write(p []byte) (int, error) {
	return pe.w.Write(p)
}

func (pe *joinedPipeEnds) SetIOTimeout(_ time.Duration) {
}

func (pe *joinedPipeEnds) Close() error {
	if err := pe.r.Close(); err != nil {
		pe.w.Close()
		return err
	}
	return pe.w.Close()
}

func TestBaseTestResultFromTest(t *testing.T) {
	test := testsharder.Test{
		Test: build.Test{
			Name:  "fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm",
			Label: "//src/sys:foo_test(//build/toolchain/fuchsia:x64)",
		},
		Tags: []build.TestTag{
			{
				Key:   "key",
				Value: "value",
			},
		},
		Metadata: metadata.TestMetadata{
			Owners:      []string{"carverforbes@google.com"},
			ComponentID: 1478143,
		},
	}
	expected := runtests.TestDetails{
		Name:    "fuchsia-pkg://fuchsia.com/sparky-sparky-boom-test#meta/sparky-sparky-boom-test.cm",
		GNLabel: "//src/sys:foo_test(//build/toolchain/fuchsia:x64)",
		Status:  runtests.TestFailure,
		Tags: []build.TestTag{
			{
				Key:   "key",
				Value: "value",
			},
		},
		// This is necessary for LUCI analysis to know where (which component) to
		// file the bug.
		Metadata: metadata.TestMetadata{
			Owners:      []string{"carverforbes@google.com"},
			ComponentID: 1478143,
		},
	}
	testResult := *BaseTestResultFromTest(test)
	if diff := cmp.Diff(expected, testResult); diff != "" {
		t.Errorf("BaseTestResultFromTest() failed: (-want +got): \n%s", diff)
	}
}

func TestProcessTestResult(t *testing.T) {
	tmpDir := t.TempDir()

	// Create a mock stderr file.
	stderrContent := "test case failed with some error"
	stderrFile := "stderr.txt"
	caseArtifactDir := filepath.Join(tmpDir, "case_artifacts")
	if err := os.MkdirAll(caseArtifactDir, 0755); err != nil {
		t.Fatalf("failed to create case artifact dir: %s", err)
	}
	if err := os.WriteFile(filepath.Join(caseArtifactDir, stderrFile), []byte(stderrContent), 0644); err != nil {
		t.Fatalf("failed to write stderr file: %s", err)
	}

	runResultData := ffxutil.TestRunResult{
		Suites: []ffxutil.SuiteResult{
			{
				Outcome:     ffxutil.TestFailed,
				Name:        "fuchsia-pkg://foo#meta/bar.cm",
				ArtifactDir: "suite_artifacts",
				Cases: []ffxutil.CaseResult{
					{
						Outcome:     ffxutil.TestFailed,
						Name:        "case1",
						ArtifactDir: "case_artifacts",
						Artifacts: map[string]ffxutil.ArtifactMetadata{
							stderrFile: {
								ArtifactType: ffxutil.StderrType,
							},
						},
					},
					{
						Outcome:     ffxutil.TestPassed,
						Name:        "case_passed",
						ArtifactDir: "case_artifacts",
						Artifacts: map[string]ffxutil.ArtifactMetadata{
							stderrFile: {
								ArtifactType: ffxutil.StderrType,
							},
						},
					},
					{
						Outcome:     ffxutil.TestSkipped,
						Name:        "case_skipped",
						ArtifactDir: "case_artifacts",
						Artifacts: map[string]ffxutil.ArtifactMetadata{
							stderrFile: {
								ArtifactType: ffxutil.StderrType,
							},
						},
					},
				},
			},
		},
	}

	envelope := ffxutil.TestRunResultEnvelope{
		Data:     runResultData,
		SchemaID: "https://fuchsia.dev/schema/ffx_test/run_summary-8d1dd964.json",
	}

	envelopeBytes, err := json.Marshal(envelope)
	if err != nil {
		t.Fatalf("failed to marshal envelope: %s", err)
	}

	if err := os.WriteFile(filepath.Join(tmpDir, "run_summary.json"), envelopeBytes, 0644); err != nil {
		t.Fatalf("failed to write run_summary.json: %s", err)
	}

	runResult, err := ffxutil.GetRunResult(tmpDir)
	if err != nil {
		t.Fatalf("ffxutil.GetRunResult failed: %s", err)
	}

	test := testsharder.Test{
		Test: build.Test{
			PackageURL: "fuchsia-pkg://foo#meta/bar.cm",
		},
	}

	testDetails, err := processTestResult(runResult, test, 5*time.Second, false)
	if err != nil {
		t.Fatalf("processTestResult failed: %s", err)
	}

	if testDetails.Status != runtests.TestFailure {
		t.Errorf("got test status %v, want %v", testDetails.Status, runtests.TestFailure)
	}

	if len(testDetails.Cases) != 3 {
		t.Fatalf("got %d test cases, want 3", len(testDetails.Cases))
	}

	tcFailed := testDetails.Cases[0]
	expectedFailureReason := &runtests.FailureReason{
		Errors: []*runtests.FailureReasonError{
			{Message: stderrContent},
		},
	}

	if diff := cmp.Diff(expectedFailureReason, tcFailed.FailureReason); diff != "" {
		t.Errorf("unexpected FailureReason (-want +got):\n%s", diff)
	}

	tcPassed := testDetails.Cases[1]
	if tcPassed.FailureReason != nil {
		t.Errorf("passed case got FailureReason %v, want nil", tcPassed.FailureReason)
	}

	tcSkipped := testDetails.Cases[2]
	if tcSkipped.FailureReason != nil {
		t.Errorf("skipped case got FailureReason %v, want nil", tcSkipped.FailureReason)
	}
}

type reconnectTestTarget struct {
	fuchsiaTarget
	ffxInstance *targets.FFXInstance
	ipv6Addr    *net.IPAddr
	nodename    string
	serialNum   string
}

func (t *reconnectTestTarget) GetFFX() *targets.FFXInstance { return t.ffxInstance }
func (t *reconnectTestTarget) GetSharedData() string        { return t.ffxInstance.SharedData() }
func (t *reconnectTestTarget) Nodename() string             { return t.nodename }
func (t *reconnectTestTarget) SerialNumber() string         { return t.serialNum }
func (t *reconnectTestTarget) IPv6() (*net.IPAddr, error)   { return t.ipv6Addr, nil }

// newReconnectHarness returns an FFXTester for an emulator-like target whose
// address changes to fe80::2%qemu on reconnect, along with the target's
// FFXInstance. The first ffxTesterReconnect call fails so that Reconnect
// re-resolves the address.
func newReconnectHarness(t *testing.T) (*ffxutil.FFXInstance, *reconnectTestTarget, *FFXTester) {
	t.Helper()
	tmpDir := t.TempDir()
	t.Setenv(botanistconstants.NodenameEnvKey, targets.DefaultEmulatorNodename)
	t.Setenv(botanistconstants.DeviceAddrEnvKey, "")
	t.Setenv(botanistconstants.IPv4AddrEnvKey, "")
	t.Setenv(botanistconstants.IPv6AddrEnvKey, "")
	t.Setenv(botanistconstants.TestbedConfigEnvKey, "")

	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\nexit 0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	sshKeys := ffxutil.SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	realFFX, err := ffxutil.NewFFXInstance(t.Context(), ffxPath, tmpDir, nil, "", &sshKeys, filepath.Join(tmpDir, "out"), ffxutil.UseFFXStrict)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}
	realFFX.SetTarget("fe80::1%qemu")

	target := &reconnectTestTarget{
		ffxInstance: &targets.FFXInstance{
			FFXInstance: realFFX,
			Experiments: botanist.GetExperiments([]string{string(botanist.UseSerialTargetID)}),
		},
		ipv6Addr:  &net.IPAddr{IP: net.ParseIP("fe80::2"), Zone: "qemu"},
		nodename:  targets.DefaultEmulatorNodename,
		serialNum: "EM-REBOOT1",
	}
	tester := &FFXTester{
		ffx:    &ffxutil.MockFFXInstance{},
		target: target,
	}

	oldReconnect := ffxTesterReconnect
	t.Cleanup(func() { ffxTesterReconnect = oldReconnect })
	calls := 0
	ffxTesterReconnect = func(_ *FFXTester, _ context.Context) error {
		calls++
		if calls == 1 {
			return fmt.Errorf("initial reconnect failure")
		}
		return nil
	}
	return realFFX, target, tester
}

func TestFFXTesterReconnect_UpdatesRegisteredTargets(t *testing.T) {
	peer := ffxutil.RegisteredTarget{Nodename: "fuchsia-peer-1", ID: "PEER-001", Address: "192.168.42.10"}
	for _, tc := range []struct {
		name      string
		nodename  string
		serialNum string
		existing  []ffxutil.RegisteredTarget
		want      []ffxutil.RegisteredTarget
		// noExperiment disables use_serial_target_id.
		noExperiment bool
		// ffxTarget is the target's ffx target before reconnecting, or empty
		// for its IP address.
		ffxTarget string
		// wantTarget is the target's ffx target after reconnecting.
		wantTarget string
	}{
		{
			// configureAndRegisterTarget targets devices with a serial number
			// by `id:<serial>`.
			name:      "updates_matching_target_and_preserves_peers",
			nodename:  targets.DefaultEmulatorNodename,
			serialNum: "EM-REBOOT1",
			ffxTarget: "id:EM-REBOOT1",
			existing: []ffxutil.RegisteredTarget{
				peer,
				{Nodename: targets.DefaultEmulatorNodename, ID: "EM-REBOOT1", Address: "fe80::1%qemu"},
			},
			want: []ffxutil.RegisteredTarget{
				peer,
				{Nodename: targets.DefaultEmulatorNodename, ID: "EM-REBOOT1", Address: "fe80::2%qemu"},
			},
			// A target addressed by serial number keeps that target, and ffx
			// finds the new address through the registry.
			wantTarget: "id:EM-REBOOT1",
		},
		{
			// Without the experiment, the target keeps its IP address target
			// and the registry is left alone.
			name:         "without_experiment_repoints_target",
			nodename:     targets.DefaultEmulatorNodename,
			serialNum:    "EM-REBOOT1",
			existing:     []ffxutil.RegisteredTarget{peer},
			want:         []ffxutil.RegisteredTarget{peer},
			noExperiment: true,
			wantTarget:   "fe80::2%qemu",
		},
		{
			// Without the experiment, a target addressed as `id:<serial>`
			// (as force_ffx_usb devices are) is still pointed at the new IP
			// address.
			name:         "without_experiment_repoints_id_target",
			nodename:     targets.DefaultEmulatorNodename,
			serialNum:    "EM-REBOOT1",
			existing:     []ffxutil.RegisteredTarget{peer},
			want:         []ffxutil.RegisteredTarget{peer},
			noExperiment: true,
			ffxTarget:    "id:EM-REBOOT1",
			wantTarget:   "fe80::2%qemu",
		},
		{
			name:       "unidentifiable_target_not_registered",
			existing:   []ffxutil.RegisteredTarget{peer},
			want:       []ffxutil.RegisteredTarget{peer},
			wantTarget: "fe80::2%qemu",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			realFFX, target, tester := newReconnectHarness(t)
			target.nodename = tc.nodename
			target.serialNum = tc.serialNum
			if tc.noExperiment {
				target.ffxInstance.Experiments = nil
			}
			if tc.ffxTarget != "" {
				realFFX.SetTarget(tc.ffxTarget)
			}
			if err := realFFX.RegisterTargets(tc.existing); err != nil {
				t.Fatal(err)
			}

			if err := tester.Reconnect(t.Context()); err != nil {
				t.Fatalf("Reconnect() unexpected error: %s", err)
			}
			if got := target.GetFFX().GetTarget(); got != tc.wantTarget {
				t.Errorf("Reconnect() target.GetFFX().GetTarget() = %q, want %q", got, tc.wantTarget)
			}
			got, err := realFFX.LoadRegisteredTargets()
			if err != nil {
				t.Fatalf("LoadRegisteredTargets() failed: %s", err)
			}
			if diff := cmp.Diff(tc.want, got); diff != "" {
				t.Errorf("LoadRegisteredTargets() mismatch (-want +got):\n%s", diff)
			}
		})
	}
}

func TestProcessTestResultRunnerFormat(t *testing.T) {
	for _, tc := range []struct {
		desc       string
		runner     string
		caseName   string
		wantFormat string
		wantSuite  string
		wantCase   string
	}{
		{"gtest", "gtest_runner", "MySuite.MyCase", "GoogleTest", "MySuite", "MyCase"},
		{"gtest variant", "zxtest_runner", "Inst/MySuite.MyCase/0", "GoogleTest", "Inst/MySuite", "MyCase/0"},
		{"rust", "rust_test_runner", "tests::my_test", "Rust", "tests", "my_test"},
		{"go", "go_test_runner", "TestFoo/sub", "Go", "", "TestFoo/sub"},
		{"unknown runner", "elf_test_runner", "MySuite.MyCase", "FTF", "", "MySuite.MyCase"},
		{"no runner", "", "MySuite.MyCase", "FTF", "", "MySuite.MyCase"},
	} {
		t.Run(tc.desc, func(t *testing.T) {
			tmpDir := t.TempDir()
			envelope := ffxutil.TestRunResultEnvelope{
				Data: ffxutil.TestRunResult{
					Suites: []ffxutil.SuiteResult{{
						Outcome: ffxutil.TestPassed,
						Name:    "fuchsia-pkg://foo#meta/bar.cm",
						Runner:  tc.runner,
						Cases:   []ffxutil.CaseResult{{Outcome: ffxutil.TestPassed, Name: tc.caseName}},
					}},
				},
				SchemaID: "https://fuchsia.dev/schema/ffx_test/run_summary-8d1dd964.json",
			}
			envelopeBytes, err := json.Marshal(envelope)
			if err != nil {
				t.Fatalf("failed to marshal envelope: %s", err)
			}
			if err := os.WriteFile(filepath.Join(tmpDir, "run_summary.json"), envelopeBytes, 0o644); err != nil {
				t.Fatalf("failed to write run_summary.json: %s", err)
			}
			runResult, err := ffxutil.GetRunResult(tmpDir)
			if err != nil {
				t.Fatalf("ffxutil.GetRunResult failed: %s", err)
			}
			test := testsharder.Test{Test: build.Test{PackageURL: "fuchsia-pkg://foo#meta/bar.cm"}}

			testDetails, err := processTestResult(runResult, test, 5*time.Second, false)
			if err != nil {
				t.Fatalf("processTestResult failed: %s", err)
			}

			if testDetails.Runner != tc.runner {
				t.Errorf("got runner %q, want %q", testDetails.Runner, tc.runner)
			}
			if len(testDetails.Cases) != 1 {
				t.Fatalf("got %d test cases, want 1", len(testDetails.Cases))
			}
			got := testDetails.Cases[0]
			if got.Format != tc.wantFormat || got.SuiteName != tc.wantSuite || got.CaseName != tc.wantCase || got.DisplayName != tc.caseName {
				t.Errorf("got (format, suite, case, display) = (%q, %q, %q, %q), want (%q, %q, %q, %q)",
					got.Format, got.SuiteName, got.CaseName, got.DisplayName,
					tc.wantFormat, tc.wantSuite, tc.wantCase, tc.caseName)
			}
		})
	}
}
