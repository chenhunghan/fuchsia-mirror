// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestLoadConfigs(t *testing.T) {
	tests := []struct {
		name        string
		jsonStr     string
		expectedLen int
		expectErr   bool
	}{
		// Valid configs.
		{"ValidConfig", `[{"nodename":"upper-drank-wick-creek"},{"nodename":"siren-swoop-wick-hasty"}]`, 2, false},
		// Invalid configs.
		{"InvalidConfig", `{{"nodename":"upper-drank-wick-creek"},{"nodename":"siren-swoop-wick-hasty"}}`, 0, true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			configs, err := LoadDeviceConfigs(mkTempFile(t, test.jsonStr))
			if test.expectErr && err == nil {
				t.Error("expected errors; no errors found")
			}
			if !test.expectErr && err != nil {
				t.Errorf("expected no errors; found error %s", err)
			}
			if len(configs) != test.expectedLen {
				t.Errorf("expected %d nodes; found %d", test.expectedLen, len(configs))
			}
		})
	}
}

// mkTempFile returns a new temporary file with the specified content that will
// be cleaned up automatically.
func mkTempFile(t *testing.T, content string) string {
	name := filepath.Join(t.TempDir(), "foo")
	if err := os.WriteFile(name, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return name
}

func TestCheckDMCHealthCheckOutput(t *testing.T) {
	for _, tc := range []struct {
		name    string
		output  string
		wantErr bool
	}{
		{
			name:    "healthy single device",
			output:  `[{"nodename":"fuchsia-node-1","state":"healthy","status_message":"","dms_state":"AVAILABLE"}]`,
			wantErr: false,
		},
		{
			name:    "empty stdout rejected",
			output:  "  \n\t ",
			wantErr: true,
		},
		{
			name:    "empty JSON array rejected",
			output:  `[]`,
			wantErr: true,
		},
		{
			name:    "malformed JSON rejected",
			output:  `not-json`,
			wantErr: true,
		},
		{
			name:    "unhealthy state rejected",
			output:  `[{"nodename":"fuchsia-node-1","state":"unhealthy","status_message":"failed to place device in fastboot","dms_state":"REPAIRING"}]`,
			wantErr: true,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := checkDMCHealthCheckOutput([]byte(tc.output))
			if tc.wantErr && err == nil {
				t.Fatalf("checkDMCHealthCheckOutput(%q) = nil, want error", tc.output)
			}
			if !tc.wantErr && err != nil {
				t.Fatalf("checkDMCHealthCheckOutput(%q) = %v, want nil", tc.output, err)
			}
		})
	}
}

func TestPlaceInFastbootNonZeroExitIncludesStdout(t *testing.T) {
	origBackoff := fastbootRetryBackoff
	fastbootRetryBackoff = time.Millisecond
	t.Cleanup(func() { fastbootRetryBackoff = origBackoff })

	fakeDMC := filepath.Join(t.TempDir(), "dmc")
	script := "#!/bin/sh\nif [ \"$1\" = \"health-check\" ]; then\n  printf 'diagnostic failure detail'\n  exit 1\nfi\nexit 0\n"
	if err := os.WriteFile(fakeDMC, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DMC_PATH", fakeDMC)

	dev := &Device{
		config: DeviceConfig{
			Network: NetworkProperties{Nodename: "fuchsia-node-1"},
		},
	}
	err := dev.placeInFastboot(context.Background())
	if err == nil {
		t.Fatal("placeInFastboot() = nil, want error")
	}
	if !strings.Contains(err.Error(), "dmc health-check failed:") || !strings.Contains(err.Error(), "diagnostic failure detail") {
		t.Fatalf("placeInFastboot() error = %q, want stdout included in error message", err.Error())
	}
}

func TestPlaceInFastbootNonZeroExitIncludesStderr(t *testing.T) {
	origBackoff := fastbootRetryBackoff
	fastbootRetryBackoff = time.Millisecond
	t.Cleanup(func() { fastbootRetryBackoff = origBackoff })

	fakeDMC := filepath.Join(t.TempDir(), "dmc")
	script := "#!/bin/sh\nif [ \"$1\" = \"health-check\" ]; then\n  printf 'stderr diagnostic failure' >&2\n  exit 1\nfi\nexit 0\n"
	if err := os.WriteFile(fakeDMC, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DMC_PATH", fakeDMC)

	dev := &Device{
		config: DeviceConfig{
			Network: NetworkProperties{Nodename: "fuchsia-node-1"},
		},
	}
	err := dev.placeInFastboot(context.Background())
	if err == nil {
		t.Fatal("placeInFastboot() = nil, want error")
	}
	if !strings.Contains(err.Error(), "dmc health-check failed:") || !strings.Contains(err.Error(), "stderr diagnostic failure") {
		t.Fatalf("placeInFastboot() error = %q, want stderr included in error message", err.Error())
	}
}

func TestDeviceStartClosesSerialWatcherSocket(t *testing.T) {
	testCases := []struct {
		name              string
		connectionTimeout time.Duration
		sendBootSignature bool
		cancelAfterAccept bool
		wantErrContains   string
		wantErrIs         error
	}{
		{
			name:              "boot signature received",
			connectionTimeout: 10 * time.Second,
			sendBootSignature: true,
		},
		{
			name:              "boot timeout",
			connectionTimeout: 100 * time.Millisecond,
			wantErrContains:   "timed out after",
		},
		{
			name:              "parent context cancellation",
			connectionTimeout: 10 * time.Second,
			cancelAfterAccept: true,
			wantErrIs:         context.Canceled,
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			sockPath := filepath.Join(t.TempDir(), "serial.sock")
			listener, err := net.Listen("unix", sockPath)
			if err != nil {
				t.Fatal(err)
			}

			var (
				mu   sync.Mutex
				conn net.Conn
			)
			accepted := make(chan struct{})
			clientClosed := make(chan struct{})
			var wg sync.WaitGroup
			wg.Add(1)
			go func() {
				defer wg.Done()
				c, acceptErr := listener.Accept()
				if acceptErr != nil {
					if !errors.Is(acceptErr, net.ErrClosed) {
						t.Errorf("Accept() failed: %v", acceptErr)
					}
					return
				}
				mu.Lock()
				conn = c
				mu.Unlock()
				defer c.Close()
				close(accepted)
				var buf [1]byte
				_, _ = c.Read(buf[:])
				close(clientClosed)
			}()

			base, err := newGenericFuchsia(context.Background(), "fuchsia-node-test", sockPath, nil, nil)
			if err != nil {
				t.Fatal(err)
			}
			dev := &Device{
				genericFuchsiaTarget: base,
				config: DeviceConfig{
					Network: NetworkProperties{Nodename: "fuchsia-node-test"},
				},
			}
			dev.SetConnectionTimeout(tc.connectionTimeout)
			defer dev.Stop()

			ctx, cancel := context.WithCancel(context.Background())
			defer func() {
				_ = listener.Close()
				mu.Lock()
				if conn != nil {
					_ = conn.Close()
				}
				mu.Unlock()
				wg.Wait()
			}()
			defer cancel()

			startDone := make(chan error, 1)
			go func() {
				startDone <- dev.Start(ctx, nil, "", false)
			}()

			select {
			case <-accepted:
			case <-time.After(2 * time.Second):
				t.Fatal("timed out waiting for serial watcher to connect")
			}

			if tc.sendBootSignature {
				mu.Lock()
				c := conn
				mu.Unlock()
				if _, writeErr := c.Write([]byte(bootedLogSignature + "\n")); writeErr != nil {
					t.Fatalf("Write() failed: %v", writeErr)
				}
			}
			if tc.cancelAfterAccept {
				cancel()
			}

			select {
			case err = <-startDone:
			case <-time.After(2 * time.Second):
				t.Fatal("timed out waiting for Start() to return")
			}
			wantErr := tc.wantErrContains != "" || tc.wantErrIs != nil
			if wantErr {
				if err == nil {
					t.Fatal("Start() = nil, want error")
				}
				if tc.wantErrContains != "" && !strings.Contains(err.Error(), tc.wantErrContains) {
					t.Errorf("Error() = %q, want substring %q", err.Error(), tc.wantErrContains)
				}
				if tc.wantErrIs != nil && !errors.Is(err, tc.wantErrIs) {
					t.Errorf("Start() error = %v, want %v", err, tc.wantErrIs)
				}
			} else if err != nil {
				t.Fatalf("Start() = %v, want nil", err)
			}
			select {
			case <-clientClosed:
			case <-time.After(2 * time.Second):
				t.Errorf("expected serial watcher socket to be closed promptly")
			}
		})
	}
}

func TestDeviceResolveFastbootTransport(t *testing.T) {
	ipv6Addr := &net.IPAddr{IP: net.ParseIP("fe80::1"), Zone: "eth0"}
	for _, tc := range []struct {
		name         string
		serialNumber string
		tcpFastboot  bool
		ipv6         *net.IPAddr
		wantTarget   string
		wantTCPFlash bool
		wantErr      bool
	}{
		{
			name:         "usb_fastboot_uses_id_prefix",
			serialNumber: "SER12345",
			ipv6:         ipv6Addr,
			wantTarget:   "id:SER12345",
		},
		{
			name:         "tcp_fastboot_without_serial_number_uses_ipv6",
			tcpFastboot:  true,
			ipv6:         ipv6Addr,
			wantTarget:   "fe80::1%eth0",
			wantTCPFlash: true,
		},
		{
			name:         "tcp_fastboot_takes_precedence_over_serial_number",
			serialNumber: "SER12345",
			tcpFastboot:  true,
			ipv6:         ipv6Addr,
			wantTarget:   "fe80::1%eth0",
			wantTCPFlash: true,
		},
		{
			name:         "tcp_fastboot_without_ipv6_rejected",
			serialNumber: "SER12345",
			tcpFastboot:  true,
			wantErr:      true,
		},
		{
			name:        "tcp_fastboot_with_empty_ip_rejected",
			tcpFastboot: true,
			ipv6:        &net.IPAddr{},
			wantErr:     true,
		},
		{
			name:    "usb_fastboot_without_serial_number_rejected",
			ipv6:    ipv6Addr,
			wantErr: true,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dev := &Device{
				genericFuchsiaTarget: &genericFuchsiaTarget{
					addrResolved: true,
					ipv6:         tc.ipv6,
				},
				config: DeviceConfig{
					SerialNumber: tc.serialNumber,
					TCPFastboot:  tc.tcpFastboot,
					Network:      NetworkProperties{Nodename: "fuchsia-node-1"},
				},
			}
			gotTarget, gotTCP, err := dev.resolveFastbootTransport()
			if (err != nil) != tc.wantErr {
				t.Fatalf("resolveFastbootTransport() error = %v, wantErr %v", err, tc.wantErr)
			}
			if !tc.wantErr && (gotTarget != tc.wantTarget || gotTCP != tc.wantTCPFlash) {
				t.Errorf("resolveFastbootTransport() = (%q, %v), want (%q, %v)", gotTarget, gotTCP, tc.wantTarget, tc.wantTCPFlash)
			}
		})
	}
}

func TestDeviceAccessorsPreferConfig(t *testing.T) {
	dev := &Device{genericFuchsiaTarget: &genericFuchsiaTarget{}}
	dev.SetNodename("fuchsia-discovered")
	dev.SetSerialNumber("SER-DISCOVERED")
	if got := dev.Nodename(); got != "fuchsia-discovered" {
		t.Errorf("Nodename() = %q, want %q", got, "fuchsia-discovered")
	}
	if got := dev.SerialNumber(); got != "SER-DISCOVERED" {
		t.Errorf("SerialNumber() = %q, want %q", got, "SER-DISCOVERED")
	}

	configured := &Device{
		genericFuchsiaTarget: &genericFuchsiaTarget{},
		config: DeviceConfig{
			SerialNumber: "SER-CONFIGURED",
			Network:      NetworkProperties{Nodename: "fuchsia-configured"},
		},
	}
	configured.SetNodename("fuchsia-discovered")
	configured.SetSerialNumber("SER-DISCOVERED")
	if got := configured.Nodename(); got != "fuchsia-configured" {
		t.Errorf("Nodename() = %q, want %q", got, "fuchsia-configured")
	}
	if got := configured.SerialNumber(); got != "SER-CONFIGURED" {
		t.Errorf("SerialNumber() = %q, want %q", got, "SER-CONFIGURED")
	}
}
