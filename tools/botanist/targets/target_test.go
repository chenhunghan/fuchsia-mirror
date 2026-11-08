// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
)

func TestAddressLogic(t *testing.T) {
	t.Run("percentage signs are escaped", func(t *testing.T) {
		inputs := []string{
			"[fe80::a019:b0ff:fe21:64bd%qemu]:40860",
			"[fe80::a019:b0ff:fe21:64bd%25qemu]:40860",
		}
		expected := "[fe80::a019:b0ff:fe21:64bd%25qemu]:40860"
		for _, input := range inputs {
			actual := escapePercentSign(input)
			if actual != expected {
				t.Errorf("failed to escape percentage sign:\nactual: %s\nexpected: %s", actual, expected)
			}
		}
	})

	t.Run("derivation of the local-scoped local host", func(t *testing.T) {
		inputs := []string{
			"[fe80::a019:b0ff:fe21:64bd%qemu]:40860",
			"[fe80::a019:b0ff:fe21:64bd%25qemu]:40860",
		}
		expected := "[fe80::a019:b0ff:fe21:64bd%25qemu]"
		for _, input := range inputs {
			actual := localScopedLocalHost(input)
			if actual != expected {
				t.Errorf("failed to derive host:\nactual: %s\nexpected: %s", actual, expected)
			}
		}
	})
}

func TestFromJSON(t *testing.T) {
	ctx := context.Background()

	tests := []struct {
		name              string
		obj               string
		expected          reflect.Type
		expectedEmuBinary string
	}{
		{
			name:              "derive aemu target",
			obj:               `{"type": "aemu", "target": "x64"}`,
			expected:          reflect.TypeOf(&Emulator{}),
			expectedEmuBinary: "emulator",
		},
		{
			name:              "derive qemu target",
			obj:               `{"type": "qemu", "target": "arm64"}`,
			expected:          reflect.TypeOf(&Emulator{}),
			expectedEmuBinary: "qemu-system-aarch64",
		},
		// Testing FromJSON for "device" and "gce" is complex given that
		// the constructor functions for those two targets perform a good amount
		// of side effects, for example, creating a "gce" target will try to
		// initialize a gce instance.
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			target, err := FromJSON(ctx, []byte(test.obj), Options{})
			if err != nil {
				t.Errorf("failed to derive target. err=%q", err)
			}
			if reflect.TypeOf(target) != test.expected {
				t.Errorf("expected target type %q, got %q", test.expected, reflect.TypeOf(target))
			}
			if emuTarget, ok := target.(*Emulator); ok {
				if emuTarget.binary != test.expectedEmuBinary {
					t.Errorf("expected binary name %q, got %q", test.expectedEmuBinary, emuTarget.binary)
				}
			}
		})
	}
}

// Test implementation of FuchsiaTarget using TestbedConfig as its implementation
// of TestConfig.
type testTarget struct {
	FuchsiaTarget
	nodename string
	serial   string
	pdu      *targetPDU
	monsoon  *targetMonsoon
	ipv4     net.IP
	ipv6     *net.IPAddr
}

func (t *testTarget) TestConfig(expectsSSH bool) (any, error) {
	return TestbedConfig(t, expectsSSH, &testbedConfigOptions{PDU: t.pdu, Monsoon: t.monsoon})
}
func (t *testTarget) IPv4() (net.IP, error)      { return t.ipv4, nil }
func (t *testTarget) IPv6() (*net.IPAddr, error) { return t.ipv6, nil }
func (t *testTarget) Nodename() string           { return t.nodename }
func (t *testTarget) SerialSocketPath() string   { return t.serial }
func (t *testTarget) SSHKey() string             { return "" }
func (t *testTarget) GetSharedData() string      { return "/tmp/shared_data" }

func TestTestbedConfig(t *testing.T) {
	tests := []struct {
		name       string
		target     testTarget
		expectsSSH bool
		want       FuchsiaTestbedConfig
	}{
		{
			name:       "valid",
			target:     testTarget{nodename: "node", serial: "serial", ipv4: net.IPv4zero, ipv6: &net.IPAddr{IP: net.IPv6zero}},
			expectsSSH: true,
			want:       FuchsiaTestbedConfig{Type: "FuchsiaDevice", Nodename: "node", SerialSocket: "serial", IPv4: net.IPv4zero.String(), IPv6: net.IPv6zero.String(), PDU: nil, SharedData: "/tmp/shared_data"},
		},
		{
			name:       "valid without SSH",
			target:     testTarget{nodename: "node", serial: "serial", ipv4: net.IPv4zero, ipv6: &net.IPAddr{IP: net.IPv6zero}},
			expectsSSH: false,
			want:       FuchsiaTestbedConfig{Type: "FuchsiaDevice", Nodename: "node", SerialSocket: "serial", PDU: nil},
		},
		{
			name:       "valid no ip addresses",
			target:     testTarget{nodename: "node", serial: "serial"},
			expectsSSH: true,
			want:       FuchsiaTestbedConfig{Type: "FuchsiaDevice", Nodename: "node", SerialSocket: "serial", PDU: nil, SharedData: "/tmp/shared_data"},
		},
		{
			name: "valid with pdu",
			target: testTarget{nodename: "node", serial: "serial", pdu: &targetPDU{
				IP:   "192.168.1.1",
				MAC:  "12:34:56:78:9a:bc",
				Port: 1,
			}},
			expectsSSH: true,
			want: FuchsiaTestbedConfig{Type: "FuchsiaDevice", Nodename: "node", SerialSocket: "serial", PDU: &targetPDU{
				IP:   "192.168.1.1",
				MAC:  "12:34:56:78:9a:bc",
				Port: 1,
			}, SharedData: "/tmp/shared_data"},
		},
		{
			name: "valid with monsoon",
			target: testTarget{nodename: "node", serial: "serial", monsoon: &targetMonsoon{
				Sernum: "12345",
			}},
			expectsSSH: true,
			want: FuchsiaTestbedConfig{Type: "FuchsiaDevice", Nodename: "node", SerialSocket: "serial", Monsoon: &targetMonsoon{
				Sernum: "12345",
			}, SharedData: "/tmp/shared_data"},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, err := test.target.TestConfig(test.expectsSSH)
			if err != nil {
				t.Errorf("unexpected error from target.TestConfig(false): %s", err)
			}
			if diff := cmp.Diff(test.want, got); diff != "" {
				t.Errorf("TestConfig(false) mismatch (-want +got):\n%s", diff)
			}
		})
	}
}

func TestParseFFXForwardTarget(t *testing.T) {
	tests := []struct {
		name    string
		line    string
		want    string
		wantErr bool
	}{
		{
			name: "valid forwarding output",
			line: `{"forwarding":[{"host":{"Tcp":"127.0.0.1:8083"},"target":{"Tcp":"127.0.0.1:8083"}}]}` + "\n",
			want: "127.0.0.1:8083",
		},
		{
			name:    "ffx json error object",
			line:    `{"type":"user","message":"Timeout attempting to reach target \"sorrel\"","code":1}` + "\n",
			wantErr: true,
		},
		{
			name:    "empty forwarding slice",
			line:    `{"forwarding":[]}` + "\n",
			wantErr: true,
		},
		{
			name:    "missing Tcp target",
			line:    `{"forwarding":[{"target":{}}]}` + "\n",
			wantErr: true,
		},
		{
			name:    "invalid json",
			line:    `not json`,
			wantErr: true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, err := parseFFXForwardTarget(test.line)
			if (err != nil) != test.wantErr {
				t.Fatalf("parseFFXForwardTarget(%q) error = %v, wantErr %v", test.line, err, test.wantErr)
			}
			if got != test.want {
				t.Errorf("parseFFXForwardTarget(%q) = %q, want %q", test.line, got, test.want)
			}
		})
	}
}

func TestResolveIPWrapsError(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	target := &genericFuchsiaTarget{
		targetCtx: ctx,
		nodename:  "fuchsia-test-node",
	}
	err := target.ResolveIP()
	if err == nil {
		t.Fatal("expected error from ResolveIP with canceled context, got nil")
	}
	wantPrefix := constants.FailedToResolveIPErrorMsg + " for fuchsia-test-node:"
	if !strings.HasPrefix(err.Error(), wantPrefix) {
		t.Errorf("ResolveIP() error = %q, want prefix %q", err.Error(), wantPrefix)
	}
	if errors.Unwrap(err) == nil {
		t.Errorf("ResolveIP() error is not wrapped: %v", err)
	}
}

func TestCaptureSerialLogDrainsOnStop(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	defer hostInR.Close()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	target, err := newGenericFuchsia(ctx, "fuchsia-test-node", "", nil, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	if err := target.StartSerialServer(); err != nil {
		t.Fatalf("StartSerialServer() failed: %s", err)
	}

	serialLogPath := filepath.Join(t.TempDir(), "serial_log.txt")
	captureDone := make(chan error, 1)
	go func() {
		captureDone <- target.CaptureSerialLog(serialLogPath)
	}()

	if _, err := io.WriteString(targetOutW, "first line\n"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(serialLogPath); err == nil && strings.Contains(string(b), "first line") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("timed out waiting for first line in serial_log.txt")
		}
		time.Sleep(10 * time.Millisecond)
	}

	// Write a line without sleeping for the 50ms poll interval and immediately stop.
	if _, err := io.WriteString(targetOutW, "final line without newline"); err != nil {
		t.Fatal(err)
	}
	target.Stop()

	if err := <-captureDone; err != nil {
		t.Fatalf("CaptureSerialLog() returned unexpected error on Stop(): %s", err)
	}
	got, err := os.ReadFile(serialLogPath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(got), "final line without newline") {
		t.Errorf("serial_log.txt missing final line; got:\n%s", string(got))
	}
}

func TestCaptureSerialLogReturnsErrorOnUnexpectedDisconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	socketPath := CreateSocketPath("serial-test")
	l, err := net.ListenUnix("unix", &net.UnixAddr{Name: socketPath, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	l.SetUnlinkOnClose(true)
	defer l.Close()

	go func() {
		conn, err := l.Accept()
		if err != nil {
			return
		}
		_, _ = io.WriteString(conn, "line before crash\n")
		conn.Close()
	}()

	target, err := newGenericFuchsia(ctx, "fuchsia-test-node", socketPath, nil, nil)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}

	serialLogPath := filepath.Join(t.TempDir(), "serial_log.txt")
	err = target.CaptureSerialLog(serialLogPath)
	if err == nil {
		t.Fatal("expected error from CaptureSerialLog on unexpected disconnect, got nil")
	}
	if !strings.Contains(err.Error(), constants.SerialReadErrorMsg) {
		t.Errorf("CaptureSerialLog() error = %q, want substring %q", err.Error(), constants.SerialReadErrorMsg)
	}
}

func TestAddFFXPackageRepositoryReRegistersOnReconnect(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()
	registerLog := filepath.Join(tmpDir, "register.log")
	reconnectTrigger := filepath.Join(tmpDir, "reconnect")
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := `#!/bin/bash
for arg in "$@"; do
  if [[ "$arg" == "forward" ]]; then
    echo '{"forwarding":[{"host":{"Tcp":"127.0.0.1:10050"},"target":{"Tcp":"127.0.0.1:64251"},"direction":"TargetToHost"}]}'
    while [[ ! -f "` + reconnectTrigger + `" ]]; do
      sleep 0.01
    done
    echo '{"forwarding":[{"host":{"Tcp":"127.0.0.1:10050"},"target":{"Tcp":"127.0.0.1:56700"},"direction":"TargetToHost"}]}'
    sleep 60
    exit 0
  fi
  if [[ "$arg" == "register" ]]; then
    echo "$@" >> "` + registerLog + `"
    echo '{"ok":{}}'
    exit 0
  fi
done
echo '{"ok":{}}'
`
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
	ffxInstance, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, "id:test-serial", &ffxutil.SSHInfo{SshPriv: sshPriv, SshPub: sshPub}, tmpDir, ffxutil.UseFFXStrict)
	if err != nil {
		t.Fatalf("failed to create ffx instance: %s", err)
	}
	ffxInstance.SetStdoutStderr(io.Discard, io.Discard)

	target := &genericFuchsiaTarget{
		targetCtx: ctx,
		nodename:  "fuchsia-test-node",
		ffx:       &FFXInstance{FFXInstance: ffxInstance},
	}

	cleanup, err := target.AddFFXPackageRepository(ctx, "fuchsia-package-server", 10050, true)
	if err != nil {
		t.Fatalf("AddFFXPackageRepository() failed: %v", err)
	}
	defer cleanup()

	gotBytes, err := os.ReadFile(registerLog)
	if err != nil {
		t.Fatalf("failed to read register log: %v", err)
	}
	if !strings.Contains(string(gotBytes), "--address-override 127.0.0.1:64251") {
		t.Fatalf("initial registration missing 127.0.0.1:64251; got:\n%s", string(gotBytes))
	}

	// Simulate target reboot causing ffx forward to reconnect on a new target port.
	if err := os.WriteFile(reconnectTrigger, []byte("1"), 0o600); err != nil {
		t.Fatal(err)
	}

	deadline := time.Now().Add(5 * time.Second)
	for {
		b, err := os.ReadFile(registerLog)
		if err == nil && strings.Contains(string(b), "--address-override 127.0.0.1:56700") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for re-registration with 127.0.0.1:56700; got:\n%s", string(b))
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestIPAddr(t *testing.T) {
	ctx := context.Background()
	emu, err := NewEmulator(ctx, EmulatorConfig{Target: "x64"}, Options{}, "aemu")
	if err != nil {
		t.Fatalf("NewEmulator failed: %s", err)
	}
	emu.addrResolved = true
	// With emulator internet access, mDNS also reports the guest's SLiRP
	// address, which isn't reachable from the host.
	emu.ipv4 = net.ParseIP("10.0.2.15")
	emu.ipv6 = &net.IPAddr{IP: net.ParseIP("fe80::1"), Zone: "qemu"}

	if ipv4, err := emu.IPv4(); err != nil || ipv4 != nil {
		t.Errorf("emu.IPv4() = (%v, %v), want (nil, nil)", ipv4, err)
	}
	gotEmuAddr, err := IPAddr(emu)
	if err != nil {
		t.Fatalf("IPAddr(emu) failed: %s", err)
	}
	wantEmuAddr := net.IPAddr{IP: net.ParseIP("fe80::1"), Zone: "qemu"}
	if diff := cmp.Diff(wantEmuAddr, gotEmuAddr); diff != "" {
		t.Errorf("IPAddr(emu) mismatch (-want +got):\n%s", diff)
	}

	nonEmu := &testTarget{
		ipv4: net.ParseIP("192.168.1.2"),
		ipv6: &net.IPAddr{IP: net.ParseIP("fe80::2"), Zone: "eth0"},
	}
	gotNonEmuAddr, err := IPAddr(nonEmu)
	if err != nil {
		t.Fatalf("IPAddr(nonEmu) failed: %s", err)
	}
	wantNonEmuAddr := net.IPAddr{IP: net.ParseIP("192.168.1.2")}
	if diff := cmp.Diff(wantNonEmuAddr, gotNonEmuAddr); diff != "" {
		t.Errorf("IPAddr(nonEmu) mismatch (-want +got):\n%s", diff)
	}
}

func TestResolveIPWaitsForIPv6(t *testing.T) {
	answers := []struct {
		ipv4 net.IP
		ipv6 net.IPAddr
	}{
		// The first answer only has the A record.
		{ipv4: net.ParseIP("10.0.2.15")},
		{ipv4: net.ParseIP("10.0.2.15"), ipv6: net.IPAddr{IP: net.ParseIP("fe80::1"), Zone: "qemu"}},
	}
	calls := 0
	origResolveIP := resolveIP
	defer func() { resolveIP = origResolveIP }()
	resolveIP = func(ctx context.Context, nodename string) (net.IP, net.IPAddr, error) {
		a := answers[min(calls, len(answers)-1)]
		calls++
		return a.ipv4, a.ipv6, nil
	}

	target := &genericFuchsiaTarget{targetCtx: context.Background(), nodename: "fuchsia-test", connectionTimeout: time.Minute}
	if err := target.ResolveIP(); err != nil {
		t.Fatalf("ResolveIP() failed: %s", err)
	}
	if calls != 2 {
		t.Errorf("expected ResolveIP() to ask again after an IPv4-only answer; got %d calls", calls)
	}
	if target.ipv6 == nil || !target.ipv6.IP.Equal(net.ParseIP("fe80::1")) {
		t.Errorf("target.ipv6 = %v, want fe80::1%%qemu", target.ipv6)
	}

	// If no IPv6 address is ever resolved, ResolveIP() fails instead of
	// returning an empty IPv6 address.
	calls = 0
	answers = answers[:1]
	target = &genericFuchsiaTarget{targetCtx: context.Background(), nodename: "fuchsia-test", connectionTimeout: 3 * mDNSQuestionInterval}
	if err := target.ResolveIP(); err == nil {
		t.Errorf("expected ResolveIP() to fail without an IPv6 address")
	}
	if target.addrResolved {
		t.Errorf("expected the address to stay unresolved")
	}
}
