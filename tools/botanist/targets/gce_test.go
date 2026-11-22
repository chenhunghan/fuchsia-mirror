// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bufio"
	"context"
	"crypto/x509"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/retry"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	serialconstants "go.fuchsia.dev/fuchsia/tools/lib/serial/constants"
	"golang.org/x/crypto/ssh"
)

func TestGeneratePrivateKey(t *testing.T) {
	path, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: got %s, want <nil> error", err)
	}
	defer os.Remove(path)

	// Load the pkey and ensure that it's in the right format.
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: got %s, want <nil> error", path, err)
	}
	block, _ := pem.Decode(b)
	pkey, err := x509.ParsePKCS1PrivateKey(block.Bytes)
	if err != nil {
		t.Fatalf("x509.ParsePKCS1PrivateKey(bytes) failed: got %s, want <nil> error", err)
	}
	if err := pkey.Validate(); err != nil {
		t.Errorf("pkey.Validate() failed: got %s, want <nil> error", err)
	}
}

func TestGeneratePublicKey(t *testing.T) {
	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: got %s, want <nil> error", err)
	}
	defer os.Remove(pkeyPath)
	path, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey(%s) failed: got %s, want <nil> error", pkeyPath, err)
	}
	defer os.Remove(path)

	// Load the public key and ensure that it's in the right format.
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: got %s, want <nil> error", path, err)
	}
	key, _, _, _, err := ssh.ParseAuthorizedKey(b)
	if err != nil {
		t.Fatalf("ssh.ParseAuthorizedKey(pubkeyBytes) failed: got %s, want <nil> error", err)
	}
	if key.Type() != "ssh-rsa" {
		t.Errorf("key %s has wrong type: got %s, want ssh-rsa", key, key.Type())
	}
}

func TestGetSerialEndpoint(t *testing.T) {
	tests := []struct {
		zone string
		want string
	}{
		{
			zone: "us-east1-b",
			want: "us-east1-ssh-serialport.googleapis.com:9600",
		},
		{
			zone: "us-east4-a",
			want: "us-east4-ssh-serialport.googleapis.com:9600",
		},
		{
			zone: "us-west1-a",
			want: "us-west1-ssh-serialport.googleapis.com:9600",
		},
		{
			zone: "us-central1-a",
			want: "us-central1-ssh-serialport.googleapis.com:9600",
		},
	}
	for _, tc := range tests {
		if got := getSerialEndpoint(tc.zone); got != tc.want {
			t.Errorf("getSerialEndpoint(%q) = %q, want %q", tc.zone, got, tc.want)
		}
	}
}

// fakePipeSerial wraps an in-memory bidirectional pipe to simulate a GCE serial console.
type fakePipeSerial struct {
	r  io.ReadCloser
	w  io.WriteCloser
	in io.Closer
}

func (f *fakePipeSerial) Read(p []byte) (int, error) {
	return f.r.Read(p)
}

func (f *fakePipeSerial) Write(p []byte) (int, error) {
	return f.w.Write(p)
}

func (f *fakePipeSerial) Close() error {
	_ = f.w.Close()
	_ = f.in.Close()
	return f.r.Close()
}

var provisionCmdRe = regexp.MustCompile(`echo "([^"]+)" > /data/ssh/authorized_keys.*echo "(SSH_KEY_PROVISIONED) \$line"`)

func TestGCEProvisionSSHKeyAndSerialLog(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)
	pubkeyPath, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey() failed: %s", err)
	}
	defer os.Remove(pubkeyPath)

	// Target -> host pipe (serial output from VM) and host -> target pipe (serial input to VM).
	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	var corruptFirstAttempt atomic.Bool
	corruptFirstAttempt.Store(true)
	var provisionAttempts atomic.Int32

	// Simulate early boot serial logs (replay-from=0) and shell responses.
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "[00000.000] early boot log from replay-from=0\n$ ")
		dumbTerm := false
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			if !dumbTerm {
				// Echo the typed command line just like UART echo mode.
				_, _ = fmt.Fprintf(targetOutW, "%s\n", line)
			}
			if strings.Contains(line, "export TERM=dumb") && strings.Contains(line, "echo TERM_IS_$TERM") {
				dumbTerm = true
				_, _ = io.WriteString(targetOutW, "TERM_IS_dumb\n$ ")
				continue
			}
			if strings.Contains(line, `echo "DATA_MOUNTED $d"`) {
				_, _ = io.WriteString(targetOutW, "DATA_MOUNTED /data\n$ ")
				continue
			}
			if m := provisionCmdRe.FindStringSubmatch(line); m != nil {
				provisionAttempts.Add(1)
				writtenKey := m[1]
				marker := m[2]
				if corruptFirstAttempt.CompareAndSwap(true, false) {
					// Simulate a dropped character on the first attempt.
					writtenKey = writtenKey[:len(writtenKey)-3]
				}
				_, _ = fmt.Fprintf(targetOutW, "%s %s\n$ ", marker, writtenKey)
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-test-node",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH:    true,
		SSHKey:        pkeyPath,
		AuthorizedKey: pubkeyPath,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", []string{opts.SSHKey}, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    200 * time.Millisecond,
		serialRetryDelay:     50 * time.Millisecond,
	}

	if got := g.Nodename(); got != cfg.InstanceName {
		t.Errorf("g.Nodename() = %q, want %q", got, cfg.InstanceName)
	}

	if err := g.StartSerialServer(); err != nil {
		t.Fatalf("g.StartSerialServer() failed: %s", err)
	}

	serialLogPath := filepath.Join(t.TempDir(), "serial_log.txt")
	captureDone := make(chan error, 1)
	go func() {
		captureDone <- g.CaptureSerialLog(serialLogPath)
	}()

	// Verify early boot serial logs are captured before Start() runs.
	earlyLogDeadline := time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(serialLogPath); err == nil && strings.Contains(string(b), "early boot log from replay-from=0") {
			break
		}
		if time.Now().After(earlyLogDeadline) {
			t.Fatal("timed out waiting for CaptureSerialLog to record early boot log before Start()")
		}
		time.Sleep(10 * time.Millisecond)
	}

	if err := g.Start(ctx, nil, "", false); err != nil {
		t.Fatalf("g.Start() failed: %s", err)
	}
	if got := provisionAttempts.Load(); got != 2 {
		t.Errorf("provisionAttempts = %d, want 2", got)
	}

	if err := g.Stop(); err != nil {
		t.Fatalf("g.Stop() failed: %s", err)
	}
	if err := <-captureDone; err != nil {
		t.Fatalf("g.CaptureSerialLog() failed: %s", err)
	}

	logBytes, err := os.ReadFile(serialLogPath)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: %s", serialLogPath, err)
	}
	logStr := string(logBytes)
	if !strings.Contains(logStr, "early boot log from replay-from=0") {
		t.Errorf("serial_log.txt missing early boot log; got:\n%s", logStr)
	}
	if !strings.Contains(logStr, "TERM_IS_dumb") {
		t.Errorf("serial_log.txt missing TERM_IS_dumb confirmation; got:\n%s", logStr)
	}
	if !strings.Contains(logStr, "DATA_MOUNTED /data") {
		t.Errorf("serial_log.txt missing DATA_MOUNTED /data confirmation; got:\n%s", logStr)
	}
	if !strings.Contains(logStr, "SSH_KEY_PROVISIONED ") {
		t.Errorf("serial_log.txt missing provisioning marker SSH_KEY_PROVISIONED; got:\n%s", logStr)
	}
}

func TestGCEProvisionSSHKeyAcceptsDelayedAttemptOutput(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)
	pubkeyPath, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey() failed: %s", err)
	}
	defer os.Remove(pubkeyPath)

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	var provisionAttempts atomic.Int32
	var firstAttemptKey string
	var firstAttemptMarker string
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "[00000.665] Console.shell: enabled\n$ ")
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			_, _ = fmt.Fprintf(targetOutW, "%s\n", line)
			if strings.Contains(line, "export TERM=dumb") && strings.Contains(line, "echo TERM_IS_$TERM") {
				_, _ = io.WriteString(targetOutW, "TERM_IS_dumb\n$ ")
				continue
			}
			if strings.Contains(line, `echo "DATA_MOUNTED $d"`) {
				_, _ = io.WriteString(targetOutW, "DATA_MOUNTED /data\n$ ")
				continue
			}
			if m := provisionCmdRe.FindStringSubmatch(line); m != nil {
				attempt := provisionAttempts.Add(1)
				if attempt == 1 {
					// Record Attempt 1's key and marker, but withhold the output
					// until Attempt 2 arrives so Attempt 1 deterministically
					// hits serialReadTimeout without relying on wall-clock sleeps.
					firstAttemptKey = m[1]
					firstAttemptMarker = m[2]
					continue
				}
				if attempt == 2 {
					_, _ = fmt.Fprintf(targetOutW, "%s %s\n$ ", firstAttemptMarker, firstAttemptKey)
				}
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-delayed-serial",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH:    true,
		SSHKey:        pkeyPath,
		AuthorizedKey: pubkeyPath,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", []string{opts.SSHKey}, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    100 * time.Millisecond,
		serialRetryDelay:     20 * time.Millisecond,
	}
	defer func() {
		_ = g.Stop()
	}()

	if err := g.Start(ctx, nil, "", false); err != nil {
		t.Fatalf("g.Start() failed when Attempt 1 output arrived after readTimeout: %s", err)
	}
	if got := provisionAttempts.Load(); got != 2 {
		t.Errorf("provisionAttempts = %d, want 2", got)
	}
}

func TestGCEProvisionSSHKeyFailsClearlyWhenTermNotSet(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)
	pubkeyPath, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey() failed: %s", err)
	}
	defer os.Remove(pubkeyPath)

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	// Echo commands verbatim (as in UART echo mode) without ever executing
	// export TERM=dumb or printing TERM_IS_dumb, verifying that echoed
	// "TERM=dumb" does not falsely satisfy the readiness check.
	var dataWaitSeen atomic.Bool
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "$ ")
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			_, _ = fmt.Fprintf(targetOutW, "%s\n$ ", line)
			if strings.Contains(line, "DATA_MOUNTED") {
				dataWaitSeen.Store(true)
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-term-fail",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH:    true,
		SSHKey:        pkeyPath,
		AuthorizedKey: pubkeyPath,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", []string{opts.SSHKey}, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    80 * time.Millisecond,
		serialRetryDelay:     10 * time.Millisecond,
	}
	defer func() {
		_ = g.Stop()
	}()

	err = g.Start(ctx, nil, "", false)
	if err == nil {
		t.Fatal("g.Start() succeeded, want error when TERM=dumb is only echoed and never executed")
	}
	if !strings.Contains(err.Error(), "failed to set TERM=dumb over serial") {
		t.Errorf("g.Start() error = %q, want substring %q", err.Error(), "failed to set TERM=dumb over serial")
	}
	if dataWaitSeen.Load() {
		t.Errorf("sent /data wait command even though TERM=dumb readiness check failed")
	}
}

func TestGCEProvisionSSHKeyFailsClearlyWhenDataNotMounted(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)
	pubkeyPath, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey() failed: %s", err)
	}
	defer os.Remove(pubkeyPath)

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	// Confirm TERM=dumb, but simulate /data never mounting (echoing the
	// command without printing DATA_MOUNTED /data).
	var provisionSeen atomic.Bool
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "$ ")
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			_, _ = fmt.Fprintf(targetOutW, "%s\n", line)
			if strings.Contains(line, "export TERM=dumb") && strings.Contains(line, "echo TERM_IS_$TERM") {
				_, _ = io.WriteString(targetOutW, "TERM_IS_dumb\n$ ")
				continue
			}
			if provisionCmdRe.MatchString(line) {
				provisionSeen.Store(true)
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-data-fail",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH:    true,
		SSHKey:        pkeyPath,
		AuthorizedKey: pubkeyPath,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", []string{opts.SSHKey}, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    80 * time.Millisecond,
		serialRetryDelay:     10 * time.Millisecond,
	}
	defer func() {
		_ = g.Stop()
	}()

	err = g.Start(ctx, nil, "", false)
	if err == nil {
		t.Fatal("g.Start() succeeded, want error when /data never mounts")
	}
	if !strings.Contains(err.Error(), "failed waiting for /data to be mounted over serial") {
		t.Errorf("g.Start() error = %q, want substring %q", err.Error(), "failed waiting for /data to be mounted over serial")
	}
	if provisionSeen.Load() {
		t.Errorf("sent SSH key provisioning command even though /data was not mounted")
	}
}

func TestGCEStartBringupWaitsForSerialReady(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	// Simulate Gigaboot loading zircon_a for longer than serialReadTimeout
	// before the serial shell cursor ("$ ") appears. Keep targetOutW open
	// until hostInR closes on g.Stop() so serial.Server does not exit early.
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "zircon_boot: ABR: loading kernel from zircon_a...\n")
		time.Sleep(250 * time.Millisecond)
		_, _ = io.WriteString(targetOutW, "[00001.234] Console.shell: enabled\n$ ")
		_, _ = io.Copy(io.Discard, hostInR)
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-bringup-node",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH: false,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", nil, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    150 * time.Millisecond,
		serialRetryDelay:     30 * time.Millisecond,
	}
	defer func() {
		_ = g.Stop()
	}()

	if err := g.Start(ctx, nil, "", false); err != nil {
		t.Fatalf("g.Start() failed for bringup target: %s", err)
	}

	// Verify that a subsequent serial socket connection (as opened by
	// testrunner's NewFuchsiaSerialTester) immediately sees "$ " from the
	// serial server's AuxiliaryOutput buffer without waiting again.
	testerSocket, err := serial.NewSocketWithIOTimeout(ctx, g.SerialSocketPath(), 200*time.Millisecond, true)
	if err != nil {
		t.Fatalf("serial.NewSocketWithIOTimeout() after Start() failed: %s", err)
	}
	_ = testerSocket.Close()
}

func TestGCEStartBringupBoundsBootTimeout(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	go func() {
		_, _ = io.Copy(io.Discard, hostInR)
	}()

	// Trickle boot logs faster than serialReadTimeout without ever printing "$ ",
	// verifying that serialBootTimeout still bounds the overall wait instead of
	// stalling on NewSocketWithIOTimeout's internal 45s timeout.
	go func() {
		defer targetOutW.Close()
		ticker := time.NewTicker(15 * time.Millisecond)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				if _, err := io.WriteString(targetOutW, "still booting without cursor...\n"); err != nil {
					return
				}
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-bringup-stalled",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH: false,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", nil, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    250 * time.Millisecond,
		serialReadTimeout:    200 * time.Millisecond,
		serialRetryDelay:     30 * time.Millisecond,
	}
	defer func() {
		_ = g.Stop()
	}()

	start := time.Now()
	err = g.Start(ctx, nil, "", false)
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("g.Start() succeeded, want timeout error when serial cursor never appears")
	}
	if !strings.Contains(err.Error(), serialconstants.FailedToFindCursorMsg) {
		t.Errorf("g.Start() error = %q, want substring %q", err.Error(), serialconstants.FailedToFindCursorMsg)
	}
	if elapsed > 5*time.Second {
		t.Errorf("g.Start() took %s, want bounded by serialBootTimeout (<< 45s)", elapsed)
	}
}

func TestGCEInitTargetRetriesCreateInstanceOnSerialTimeout(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		serialConnectTimeout: 30 * time.Millisecond,
		serialConnectBackoff: retry.NewConstantBackoff(10 * time.Millisecond),
	}
	g.createInstanceFn = func() error {
		createCalls++
		if createCalls == 1 {
			g.config.InstanceName = "fuchsia-gce-dead-c"
			g.config.Zone = "us-central1-c"
			return nil
		}
		g.config.InstanceName = "fuchsia-gce-live-a"
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		if g.config.Zone == "us-central1-c" {
			return fmt.Errorf("ssh: handshake failed: ssh: unable to authenticate")
		}
		return nil
	}
	g.resolveIPv4Fn = func() error {
		g.ipv4 = net.IPv4(10, 128, 0, 2)
		return nil
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed: %s", err)
	}
	if createCalls != 2 {
		t.Errorf("createCalls = %d, want 2", createCalls)
	}
	if connectCalls < 2 {
		t.Errorf("connectCalls = %d, want >= 2", connectCalls)
	}
	if g.config.InstanceName != "fuchsia-gce-live-a" || g.config.Zone != "us-central1-a" {
		t.Errorf("g.config = (%q, %q), want (\"fuchsia-gce-live-a\", \"us-central1-a\")", g.config.InstanceName, g.config.Zone)
	}
	if got := g.Nodename(); got != "fuchsia-4201-0a80-0002" {
		t.Errorf("g.Nodename() = %q, want %q", got, "fuchsia-4201-0a80-0002")
	}
}

func TestGCEInitTargetExhaustsMaxCreateInstanceAttemptsOnSerialTimeout(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int
	errSerial := errors.New("ssh: handshake failed: ssh: unable to authenticate")

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		serialConnectTimeout: 25 * time.Millisecond,
		serialConnectBackoff: retry.NewConstantBackoff(10 * time.Millisecond),
	}
	g.createInstanceFn = func() error {
		createCalls++
		g.config.InstanceName = fmt.Sprintf("fuchsia-gce-dead-%d", createCalls)
		g.config.Zone = "us-central1-c"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		return errSerial
	}

	err := g.initTarget(ctx)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want error after exhausting max attempts")
	}
	if !errors.Is(err, errSerial) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, errSerial)
	}
	wantSubstr := fmt.Sprintf("after %d attempts", maxCreateInstanceAttempts)
	if !strings.Contains(err.Error(), wantSubstr) {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), wantSubstr)
	}
	if createCalls != maxCreateInstanceAttempts {
		t.Errorf("createCalls = %d, want %d", createCalls, maxCreateInstanceAttempts)
	}
	if connectCalls < maxCreateInstanceAttempts {
		t.Errorf("connectCalls = %d, want >= %d", connectCalls, maxCreateInstanceAttempts)
	}
}

func TestGCEInitTargetBoundsCreateInstanceTimeoutWithoutOuterLoop(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int
	errMediator := errors.New("gcem_client: mediator RPC failed")

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:             ctx,
		createInstanceTimeout: 25 * time.Millisecond,
		serialConnectBackoff:  retry.NewConstantBackoff(10 * time.Millisecond),
		createInstanceFn: func() error {
			createCalls++
			return errMediator
		},
		connectToSerialFn: func() error {
			connectCalls++
			return nil
		},
	}

	err := g.initTarget(ctx)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want error when createInstance always fails")
	}
	if !errors.Is(err, errMediator) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, errMediator)
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if got := strings.Count(err.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("g.initTarget() error = %q, got %d occurrences of %q, want 1", err.Error(), got, context.DeadlineExceeded.Error())
	}
	if !strings.Contains(err.Error(), "failed to create GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to create GCE instance")
	}
	if createCalls < 2 {
		t.Errorf("createCalls = %d, want >= 2 retries within createInstanceTimeout", createCalls)
	}
	if connectCalls != 0 {
		t.Errorf("connectCalls = %d, want 0", connectCalls)
	}
}

func TestGCEInitTargetRetriesCreateInstanceWhileImagePendingAndSucceeds(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:             ctx,
		createInstanceTimeout: 100 * time.Millisecond,
		serialConnectBackoff:  retry.NewConstantBackoff(5 * time.Millisecond),
	}
	// Simulate createInstance failing more than maxCreateInstanceAttempts (3)
	// times while the GCE image is still PENDING, then succeeding within a
	// single createInstanceTimeout window.
	g.createInstanceFn = func() error {
		createCalls++
		if createCalls <= maxCreateInstanceAttempts+1 {
			return errors.New("source image fuchsia-123 is not ready yet (status: PENDING)")
		}
		g.config.InstanceName = "fuchsia-gce-live-a"
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		return nil
	}
	g.resolveIPv4Fn = func() error {
		g.ipv4 = net.IPv4(10, 128, 5, 155)
		return nil
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed: %s", err)
	}
	if createCalls != maxCreateInstanceAttempts+2 {
		t.Errorf("createCalls = %d, want %d", createCalls, maxCreateInstanceAttempts+2)
	}
	if connectCalls != 1 {
		t.Errorf("connectCalls = %d, want 1", connectCalls)
	}
}

func TestGCEInitTargetStopsOnContextCanceledDuringSerialConnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	var createCalls int
	var connectCalls int

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		serialConnectTimeout: 1 * time.Second,
		serialConnectBackoff: retry.NewConstantBackoff(10 * time.Millisecond),
	}
	g.createInstanceFn = func() error {
		createCalls++
		g.config.InstanceName = "fuchsia-gce-canceled"
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		cancel()
		return context.Canceled
	}

	err := g.initTarget(ctx)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("g.initTarget() error = %v, want %v", err, context.Canceled)
	}
	if !strings.Contains(err.Error(), "failed to connect to GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to connect to GCE instance")
	}
	if createCalls != 1 {
		t.Errorf("createCalls = %d, want 1 (should not recreate after ctx cancel)", createCalls)
	}
	if connectCalls != 1 {
		t.Errorf("connectCalls = %d, want 1", connectCalls)
	}
}

func TestGCEInitTargetPreexistingInstanceBoundsSerialTimeout(t *testing.T) {
	ctx := context.Background()
	var addKeyCalls int
	var connectCalls int

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			InstanceName: "fuchsia-gce-existing",
			Zone:         "us-central1-a",
		},
		loggerCtx:            ctx,
		serialConnectTimeout: 25 * time.Millisecond,
		serialConnectBackoff: retry.NewConstantBackoff(10 * time.Millisecond),
		addSSHKeyFn: func() error {
			addKeyCalls++
			return nil
		},
		connectToSerialFn: func() error {
			connectCalls++
			return fmt.Errorf("serial unavailable")
		},
	}

	err := g.initTarget(ctx)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want timeout error")
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if got := strings.Count(err.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("g.initTarget() error = %q, got %d occurrences of %q, want 1", err.Error(), got, context.DeadlineExceeded.Error())
	}
	if !strings.Contains(err.Error(), "failed to connect to GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to connect to GCE instance")
	}
	if addKeyCalls != 1 {
		t.Errorf("addKeyCalls = %d, want 1", addKeyCalls)
	}
	if connectCalls < 2 {
		t.Errorf("connectCalls = %d, want >= 2", connectCalls)
	}
}

func TestNewGCESerialHandshakeTimeout(t *testing.T) {
	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)

	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("net.Listen() failed: %s", err)
	}
	defer ln.Close()

	accepted := make(chan net.Conn, 1)
	go func() {
		conn, err := ln.Accept()
		if err == nil {
			accepted <- conn
		}
	}()
	defer func() {
		select {
		case c := <-accepted:
			_ = c.Close()
		default:
		}
	}()

	_, err = newGCESerialWithTimeout(pkeyPath, "test-user", ln.Addr().String(), 50*time.Millisecond)
	if err == nil {
		t.Fatal("newGCESerialWithTimeout() succeeded against stalled SSH server, want timeout error")
	}
}

func TestGCENodename(t *testing.T) {
	tests := []struct {
		name         string
		instanceName string
		ipv4         net.IP
		want         string
	}{
		{
			name:         "derives fuchsia nodename from GCE IPv4 MAC (arm64 shard example)",
			instanceName: "fuchsia-gce-7bdb5b497c24d410-yth",
			ipv4:         net.IPv4(10, 128, 5, 155),
			want:         "fuchsia-4201-0a80-059b",
		},
		{
			name:         "derives fuchsia nodename from GCE IPv4 MAC (x64 shard example)",
			instanceName: "fuchsia-gce-7bc97a19a29f4610-377",
			ipv4:         net.ParseIP("10.128.18.117"),
			want:         "fuchsia-4201-0a80-1275",
		},
		{
			name:         "falls back to GCE instance name when IPv4 is unresolved",
			instanceName: "fuchsia-gce-fallback-instance",
			ipv4:         nil,
			want:         "fuchsia-gce-fallback-instance",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			base, err := newGenericFuchsia(ctx, tc.instanceName, "", nil, nil)
			if err != nil {
				t.Fatalf("newGenericFuchsia() failed: %s", err)
			}
			g := &GCE{
				genericFuchsiaTarget: base,
				config: GCEConfig{
					InstanceName: tc.instanceName,
				},
				ipv4:      tc.ipv4,
				loggerCtx: ctx,
			}
			defer func() {
				_ = g.Stop()
			}()

			if got := g.Nodename(); got != tc.want {
				t.Errorf("g.Nodename() = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestGCEInitTargetRetriesResolveIPv4AndSetsConsistentNodename(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	var resolveCalls int
	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		resolveIPTimeout:     100 * time.Millisecond,
		serialConnectBackoff: retry.NewConstantBackoff(5 * time.Millisecond),
		connectToSerialFn: func() error {
			return nil
		},
	}
	g.createInstanceFn = func() error {
		g.config.InstanceName = "fuchsia-gce-7bdb5b497c24d410-yth"
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.resolveIPv4Fn = func() error {
		resolveCalls++
		if resolveCalls < 3 {
			return errors.New("lookup fuchsia-gce-7bdb5b497c24d410-yth.us-central1-a.c.fuchsia-infra.internal: no such host")
		}
		g.ipv4 = net.IPv4(10, 128, 5, 155)
		return nil
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed: %s", err)
	}
	if resolveCalls != 3 {
		t.Errorf("resolveCalls = %d, want 3", resolveCalls)
	}
	base, err := newGenericFuchsia(ctx, g.Nodename(), "", nil, nil)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g.genericFuchsiaTarget = base
	defer func() {
		_ = g.Stop()
	}()

	const wantNodename = "fuchsia-4201-0a80-059b"
	if got := g.Nodename(); got != wantNodename {
		t.Errorf("g.Nodename() = %q, want %q", got, wantNodename)
	}
	if g.genericFuchsiaTarget.nodename != wantNodename {
		t.Errorf("g.genericFuchsiaTarget.nodename = %q, want %q", g.genericFuchsiaTarget.nodename, wantNodename)
	}
}

func TestGCEInitTargetBoundsResolveIPv4Timeout(t *testing.T) {
	ctx := context.Background()
	var resolveCalls int
	errDNS := errors.New("no such host")

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			InstanceName: "fuchsia-gce-existing",
			Zone:         "us-central1-a",
		},
		loggerCtx:            ctx,
		resolveIPTimeout:     25 * time.Millisecond,
		serialConnectBackoff: retry.NewConstantBackoff(10 * time.Millisecond),
		addSSHKeyFn: func() error {
			return nil
		},
		connectToSerialFn: func() error {
			return nil
		},
		resolveIPv4Fn: func() error {
			resolveCalls++
			return errDNS
		},
	}

	err := g.initTarget(ctx)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want error when resolveIPv4 always fails")
	}
	if !errors.Is(err, errDNS) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, errDNS)
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if got := strings.Count(err.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("g.initTarget() error = %q, got %d occurrences of %q, want 1", err.Error(), got, context.DeadlineExceeded.Error())
	}
	if !strings.Contains(err.Error(), "failed to resolve IPv4") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to resolve IPv4")
	}
	if resolveCalls < 2 {
		t.Errorf("resolveCalls = %d, want >= 2 retries within resolveIPTimeout", resolveCalls)
	}
}

func TestDefaultExponentialBackoff(t *testing.T) {
	b := defaultExponentialBackoff()
	// First interval is 15s plus retry.ExponentialBackoff's jitter of [0, min(10s, 15s/2)].
	first := b.Next()
	if first < 15*time.Second || first > 22500*time.Millisecond {
		t.Errorf("defaultExponentialBackoff().Next() call 1 = %s, want in [15s, 22.5s]", first)
	}
	// Subsequent intervals double to >= 30s and must be capped at 30s.
	for i := 2; i <= 5; i++ {
		if got := b.Next(); got != 30*time.Second {
			t.Errorf("defaultExponentialBackoff().Next() call %d = %s, want 30s", i, got)
		}
	}
}

func TestHelperGCEMClient(t *testing.T) {
	if os.Getenv("GO_WANT_GCEM_HELPER_PROCESS") != "1" {
		return
	}
	switch os.Getenv("GCEM_HELPER_MODE") {
	case "sleep":
		if s := os.Getenv("GCEM_HELPER_STDERR"); s != "" {
			fmt.Fprintln(os.Stderr, s)
		}
		time.Sleep(30 * time.Second)
		os.Exit(0)
	case "fail":
		fmt.Fprintln(os.Stderr, os.Getenv("GCEM_HELPER_STDERR"))
		os.Exit(1)
	case "succeed":
		fmt.Fprint(os.Stdout, os.Getenv("GCEM_HELPER_STDOUT"))
		os.Exit(0)
	default:
		os.Exit(2)
	}
}

func fakeGCEMCommand(mode, stdout, stderr string) func(ctx context.Context, name string, args ...string) *exec.Cmd {
	return func(ctx context.Context, _ string, _ ...string) *exec.Cmd {
		cmd := exec.CommandContext(ctx, os.Args[0], "-test.run=^TestHelperGCEMClient$", "--")
		cmd.Env = append(os.Environ(),
			"GO_WANT_GCEM_HELPER_PROCESS=1",
			"GCEM_HELPER_MODE="+mode,
			"GCEM_HELPER_STDOUT="+stdout,
			"GCEM_HELPER_STDERR="+stderr,
		)
		return cmd
	}
}

func TestGCEInitTargetEnforcesSharedOuterDeadline(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int
	errSerial := errors.New("ssh: handshake failed")

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:             ctx,
		createInstanceTimeout: 5 * time.Second,
		serialConnectTimeout:  5 * time.Second,
		initTargetTimeout:     40 * time.Millisecond,
		serialConnectBackoff:  retry.NewConstantBackoff(5 * time.Millisecond),
	}
	g.createInstanceFn = func() error {
		createCalls++
		g.config.InstanceName = fmt.Sprintf("fuchsia-gce-outer-%d", createCalls)
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		// Wait past initTargetTimeout so the shared outer deadline expires
		// during attempt 1, preventing attempts 2..maxCreateInstanceAttempts.
		time.Sleep(60 * time.Millisecond)
		return errSerial
	}

	err := g.initTarget(ctx)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want outer deadline timeout error")
	}
	if !errors.Is(err, errSerial) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, errSerial)
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if got := strings.Count(err.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("g.initTarget() error = %q, got %d occurrences of %q, want 1", err.Error(), got, context.DeadlineExceeded.Error())
	}
	if !strings.Contains(err.Error(), "failed to create and connect to GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to create and connect to GCE instance")
	}
	if !strings.Contains(err.Error(), "timed out initializing GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "timed out initializing GCE instance")
	}
	if createCalls != 1 {
		t.Errorf("createCalls = %d, want 1 (outer deadline should stop before recreate)", createCalls)
	}
	if connectCalls != 1 {
		t.Errorf("connectCalls = %d, want 1", connectCalls)
	}
}

func TestIsPermanentCreateInstanceError(t *testing.T) {
	tests := []struct {
		name   string
		stderr string
		want   bool
	}{
		{
			name:   "InvalidArgument",
			stderr: "CreateInstance() call failed: rpc error: code = InvalidArgument desc = machine shape bad is unsupported",
			want:   true,
		},
		{
			name:   "ResourceExhausted",
			stderr: "CreateInstance() call failed: rpc error: code = ResourceExhausted desc = build b1 has exhausted allowed number of instances (48)",
			want:   true,
		},
		{
			name:   "PermissionDenied",
			stderr: "CreateInstance() call failed: rpc error: code = PermissionDenied desc = caller lacks permission",
			want:   true,
		},
		{
			name:   "Unauthenticated",
			stderr: "CreateInstance() call failed: rpc error: code = Unauthenticated desc = missing token",
			want:   true,
		},
		{
			name:   "terminal image FAILED",
			stderr: "CreateInstance() call failed: rpc error: code = FailedPrecondition desc = source image fuchsia-123 in project p is in terminal status FAILED",
			want:   true,
		},
		{
			name:   "terminal image DELETING",
			stderr: "CreateInstance() call failed: rpc error: code = FailedPrecondition desc = source image fuchsia-123 in project p is in terminal status DELETING",
			want:   true,
		},
		{
			name:   "unreadable pubkey",
			stderr: "failed to read pubkey file /missing: no such file or directory",
			want:   true,
		},
		{
			name:   "unparseable pubkey",
			stderr: "failed to parse public key: ssh: no key found",
			want:   true,
		},
		{
			name:   "empty machine shape",
			stderr: "-machine-shape cannot be empty",
			want:   true,
		},
		{
			name:   "transient Unavailable",
			stderr: "CreateInstance() call failed: rpc error: code = Unavailable desc = ZONE_RESOURCE_POOL_EXHAUSTED",
			want:   false,
		},
		{
			name:   "transient DeadlineExceeded",
			stderr: "CreateInstance() call failed: rpc error: code = DeadlineExceeded desc = context deadline exceeded",
			want:   false,
		},
		{
			name:   "transient image PENDING",
			stderr: "CreateInstance() call failed: rpc error: code = FailedPrecondition desc = source image fuchsia-123 in project p is not ready yet (status: PENDING)",
			want:   false,
		},
		{
			name:   "empty stderr",
			stderr: "",
			want:   false,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := isPermanentCreateInstanceError(tc.stderr); got != tc.want {
				t.Errorf("isPermanentCreateInstanceError(%q) = %v, want %v", tc.stderr, got, tc.want)
			}
		})
	}
}

func TestGCEInitTargetFailsFastOnPermanentCreateInstanceError(t *testing.T) {
	tests := []struct {
		name          string
		taskID        string
		stderr        string
		wantErrSubstr string
	}{
		{
			name:          "missing SWARMING_TASK_ID",
			taskID:        "",
			wantErrSubstr: "task did not specify SWARMING_TASK_ID",
		},
		{
			name:          "permanent gcem_client error",
			taskID:        "task-123",
			stderr:        "CreateInstance() call failed: rpc error: code = InvalidArgument desc = machine shape bad is unsupported",
			wantErrSubstr: "code = InvalidArgument",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Setenv("SWARMING_TASK_ID", tc.taskID)
			ctx := context.Background()
			var execCalls atomic.Int32

			g := &GCE{
				config: GCEConfig{
					MediatorURL:    "mediator.example.com",
					CloudProject:   "fuchsia-testing-vms",
					BuildID:        "build-123",
					SwarmingServer: "chromium-swarm.appspot.com",
					MachineShape:   "c4a-standard-4",
				},
				currentUser:           "swarming",
				loggerCtx:             ctx,
				createInstanceTimeout: 5 * time.Second,
				serialConnectBackoff:  retry.NewConstantBackoff(1 * time.Millisecond),
				execCommand: func(ctx context.Context, name string, args ...string) *exec.Cmd {
					execCalls.Add(1)
					return fakeGCEMCommand("fail", "", tc.stderr)(ctx, name, args...)
				},
			}

			err := g.initTarget(ctx)
			if err == nil {
				t.Fatal("g.initTarget() succeeded, want permanent error")
			}
			if !strings.Contains(err.Error(), tc.wantErrSubstr) {
				t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), tc.wantErrSubstr)
			}
			if strings.Contains(err.Error(), "EOF") {
				t.Errorf("g.initTarget() error = %q, must surface stderr rather than EOF", err.Error())
			}
			wantCalls := int32(1)
			if tc.taskID == "" {
				wantCalls = 0
			}
			if got := execCalls.Load(); got != wantCalls {
				t.Errorf("execCalls = %d, want %d (must fail fast without retrying)", got, wantCalls)
			}
		})
	}
}

func TestGCECreateInstanceRetriesTransientStderrAndSucceeds(t *testing.T) {
	t.Setenv("SWARMING_TASK_ID", "task-123")
	ctx := context.Background()
	var execCalls atomic.Int32
	const pendingStderr = "CreateInstance() call failed: rpc error: code = FailedPrecondition desc = source image fuchsia-123 in project p is not ready yet (status: PENDING)"

	g := &GCE{
		config: GCEConfig{
			MediatorURL:    "mediator.example.com",
			CloudProject:   "fuchsia-testing-vms",
			BuildID:        "build-123",
			SwarmingServer: "chromium-swarm.appspot.com",
			MachineShape:   "c4a-standard-4",
		},
		currentUser:           "swarming",
		loggerCtx:             ctx,
		createInstanceTimeout: 5 * time.Second,
		serialConnectBackoff:  retry.NewConstantBackoff(5 * time.Millisecond),
		connectToSerialFn: func() error {
			return nil
		},
		resolveIPv4Fn: func() error {
			return nil
		},
	}
	g.execCommand = func(ctx context.Context, name string, args ...string) *exec.Cmd {
		if execCalls.Add(1) == 1 {
			return fakeGCEMCommand("fail", "", pendingStderr)(ctx, name, args...)
		}
		return fakeGCEMCommand("succeed", `{"instanceName":"fuchsia-gce-1","zone":"us-east4-a","machineShape":"c4a-standard-8"}`, "")(ctx, name, args...)
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed: %s", err)
	}
	if got := execCalls.Load(); got != 2 {
		t.Errorf("execCalls = %d, want 2", got)
	}
	if g.config.InstanceName != "fuchsia-gce-1" || g.config.Zone != "us-east4-a" {
		t.Errorf("g.config = (%q, %q), want (\"fuchsia-gce-1\", \"us-east4-a\")", g.config.InstanceName, g.config.Zone)
	}
	if g.createdMachineShape != "c4a-standard-8" {
		t.Errorf("g.createdMachineShape = %q, want %q", g.createdMachineShape, "c4a-standard-8")
	}
	if g.config.MachineShape != "c4a-standard-4" {
		t.Errorf("g.config.MachineShape = %q, want %q (must preserve requested shape)", g.config.MachineShape, "c4a-standard-4")
	}
}

func TestGCECreateInstanceCancelsSubprocessOnContextCancel(t *testing.T) {
	t.Setenv("SWARMING_TASK_ID", "task-123")
	ctx, cancel := context.WithTimeout(context.Background(), 250*time.Millisecond)
	defer cancel()

	g := &GCE{
		config: GCEConfig{
			MediatorURL:    "mediator.example.com",
			CloudProject:   "fuchsia-testing-vms",
			BuildID:        "build-123",
			SwarmingServer: "chromium-swarm.appspot.com",
			MachineShape:   "c4a-standard-4",
		},
		currentUser: "swarming",
		loggerCtx:   ctx,
		execCommand: fakeGCEMCommand("sleep", "", "dialing mediator..."),
	}

	start := time.Now()
	err := g.createInstance(ctx)
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("g.createInstance() succeeded, want error on timed-out context")
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.createInstance() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if !strings.Contains(err.Error(), "context deadline exceeded") {
		t.Errorf("g.createInstance() error = %q, want substring %q", err.Error(), "context deadline exceeded")
	}
	if !strings.Contains(err.Error(), "dialing mediator...") {
		t.Errorf("g.createInstance() error = %q, want substring %q", err.Error(), "dialing mediator...")
	}
	if elapsed > 5*time.Second {
		t.Errorf("g.createInstance() took %s after context timeout, want prompt termination", elapsed)
	}
}

func TestGCEInitTargetCreateInstanceTimeoutHardInterruptsSubprocess(t *testing.T) {
	t.Setenv("SWARMING_TASK_ID", "task-123")
	ctx := context.Background()
	var execCalls atomic.Int32

	g := &GCE{
		config: GCEConfig{
			MediatorURL:    "mediator.example.com",
			CloudProject:   "fuchsia-testing-vms",
			BuildID:        "build-123",
			SwarmingServer: "chromium-swarm.appspot.com",
			MachineShape:   "c4a-standard-4",
		},
		currentUser:           "swarming",
		loggerCtx:             ctx,
		createInstanceTimeout: 50 * time.Millisecond,
		initTargetTimeout:     10 * time.Second,
		serialConnectBackoff:  retry.NewConstantBackoff(5 * time.Second),
		execCommand: func(ctx context.Context, name string, args ...string) *exec.Cmd {
			execCalls.Add(1)
			return fakeGCEMCommand("sleep", "", "")(ctx, name, args...)
		},
	}

	start := time.Now()
	err := g.initTarget(ctx)
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("g.initTarget() succeeded, want createInstanceTimeout error")
	}
	if elapsed > 3*time.Second {
		t.Errorf("g.initTarget() took %s, want hard interrupt at createInstanceTimeout << initTargetTimeout", elapsed)
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.initTarget() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if got := strings.Count(err.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("g.initTarget() error = %q, got %d occurrences of %q, want 1", err.Error(), got, context.DeadlineExceeded.Error())
	}
	if !strings.Contains(err.Error(), "failed to create GCE instance") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "failed to create GCE instance")
	}
	if !strings.Contains(err.Error(), "context deadline exceeded") {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), "context deadline exceeded")
	}
	if strings.Contains(err.Error(), "timed out initializing GCE instance") {
		t.Errorf("g.initTarget() error = %q, must report createInstance timeout rather than outer initTargetTimeout", err.Error())
	}
	if got := execCalls.Load(); got != 1 {
		t.Errorf("execCalls = %d, want 1", got)
	}
}

func TestGCEInitTargetRecreatesSubprocessWithFreshCreateContext(t *testing.T) {
	t.Setenv("SWARMING_TASK_ID", "task-123")
	ctx := context.Background()
	var execCalls atomic.Int32

	g := &GCE{
		config: GCEConfig{
			MediatorURL:    "mediator.example.com",
			CloudProject:   "fuchsia-testing-vms",
			BuildID:        "build-123",
			SwarmingServer: "chromium-swarm.appspot.com",
			MachineShape:   "c4a-standard-4",
		},
		currentUser:           "swarming",
		loggerCtx:             ctx,
		createInstanceTimeout: 5 * time.Second,
		serialConnectTimeout:  20 * time.Millisecond,
		serialConnectBackoff:  retry.NewConstantBackoff(5 * time.Millisecond),
		execCommand: func(ctx context.Context, name string, args ...string) *exec.Cmd {
			attempt := execCalls.Add(1)
			stdout := fmt.Sprintf(`{"instanceName":"fuchsia-gce-%d","zone":"us-east4-a"}`, attempt)
			return fakeGCEMCommand("succeed", stdout, "")(ctx, name, args...)
		},
	}
	g.resolveIPv4Fn = func() error {
		g.ipv4 = net.IPv4(10, 128, 0, 9)
		return nil
	}
	g.connectToSerialFn = func() error {
		if g.config.InstanceName == "fuchsia-gce-1" {
			return errors.New("serial unreachable on attempt 1")
		}
		return nil
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed on recreate attempt: %s", err)
	}
	if got := execCalls.Load(); got != 2 {
		t.Errorf("execCalls = %d, want 2", got)
	}
	if g.config.InstanceName != "fuchsia-gce-2" {
		t.Errorf("g.config.InstanceName = %q, want %q", g.config.InstanceName, "fuchsia-gce-2")
	}
}

func TestGCEAddSSHKeyWrapsContextDeadlineExceeded(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()

	g := &GCE{
		config: GCEConfig{
			MediatorURL:  "mediator.example.com",
			CloudProject: "fuchsia-testing-vms",
			InstanceName: "fuchsia-gce-existing",
			Zone:         "us-central1-a",
		},
		currentUser: "swarming",
		loggerCtx:   ctx,
		execCommand: fakeGCEMCommand("sleep", "", ""),
	}

	err := g.addSSHKey(ctx)
	if err == nil {
		t.Fatal("g.addSSHKey() succeeded, want timeout error")
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("g.addSSHKey() error = %v, want errors.Is(..., %v)", err, context.DeadlineExceeded)
	}
	if !strings.Contains(err.Error(), "gcem_client add-ssh-key failed") {
		t.Errorf("g.addSSHKey() error = %q, want substring %q", err.Error(), "gcem_client add-ssh-key failed")
	}
	if !strings.Contains(err.Error(), "context deadline exceeded") {
		t.Errorf("g.addSSHKey() error = %q, want substring %q", err.Error(), "context deadline exceeded")
	}
}

func TestWrapContextErr(t *testing.T) {
	errSerial := errors.New("ssh: handshake failed")

	if got := wrapContextErr(context.Background(), nil); got != nil {
		t.Errorf("wrapContextErr(liveCtx, nil) = %v, want nil", got)
	}
	if got := wrapContextErr(context.Background(), errSerial); !errors.Is(got, errSerial) || errors.Is(got, context.DeadlineExceeded) {
		t.Errorf("wrapContextErr(liveCtx, errSerial) = %v, want unwrapped %v", got, errSerial)
	}

	expiredCtx, cancel := context.WithDeadline(context.Background(), time.Now().Add(-time.Second))
	defer cancel()

	wrappedOnce := wrapContextErr(expiredCtx, errSerial)
	if !errors.Is(wrappedOnce, errSerial) {
		t.Errorf("wrapContextErr(expiredCtx, errSerial) = %v, want errors.Is(..., %v)", wrappedOnce, errSerial)
	}
	if !errors.Is(wrappedOnce, context.DeadlineExceeded) {
		t.Errorf("wrapContextErr(expiredCtx, errSerial) = %v, want errors.Is(..., %v)", wrappedOnce, context.DeadlineExceeded)
	}
	if got := strings.Count(wrappedOnce.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("wrappedOnce = %q, got %d occurrences of %q, want 1", wrappedOnce.Error(), got, context.DeadlineExceeded.Error())
	}

	wrappedTwice := wrapContextErr(expiredCtx, wrappedOnce)
	if got := strings.Count(wrappedTwice.Error(), context.DeadlineExceeded.Error()); got != 1 {
		t.Errorf("wrapContextErr(expiredCtx, wrappedOnce) = %q, got %d occurrences of %q, want 1 (idempotent)", wrappedTwice.Error(), got, context.DeadlineExceeded.Error())
	}
}
