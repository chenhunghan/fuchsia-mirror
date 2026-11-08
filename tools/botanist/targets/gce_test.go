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
	"path/filepath"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/retry"
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

var provisionCmdRe = regexp.MustCompile(`echo "([^"]+)" > /data/ssh/authorized_keys.*echo "(SSH_KEY_PROVISIONED_\d+) \$line"`)

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

	// Simulate early boot serial logs (replay-from=0) and shell responses.
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "[00000.000] early boot log from replay-from=0\n$ ")
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			// Echo the typed command line just like UART echo mode.
			_, _ = fmt.Fprintf(targetOutW, "%s\n", line)
			if m := provisionCmdRe.FindStringSubmatch(line); m != nil {
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
	if !strings.Contains(logStr, "SSH_KEY_PROVISIONED_2") {
		t.Errorf("serial_log.txt missing retry provisioning marker SSH_KEY_PROVISIONED_2; got:\n%s", logStr)
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

func TestGCEInitTargetExhaustsMaxCreateInstanceAttemptsOnCreateFailure(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int
	errMediator := errors.New("gcem_client: mediator RPC failed")

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		serialConnectBackoff: retry.NewConstantBackoff(5 * time.Millisecond),
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
	wantSubstr := fmt.Sprintf("after %d attempts", maxCreateInstanceAttempts)
	if !strings.Contains(err.Error(), wantSubstr) {
		t.Errorf("g.initTarget() error = %q, want substring %q", err.Error(), wantSubstr)
	}
	if createCalls != maxCreateInstanceAttempts {
		t.Errorf("createCalls = %d, want %d", createCalls, maxCreateInstanceAttempts)
	}
	if connectCalls != 0 {
		t.Errorf("connectCalls = %d, want 0", connectCalls)
	}
}

func TestGCEInitTargetRetriesCreateInstanceFailureAndSucceeds(t *testing.T) {
	ctx := context.Background()
	var createCalls int
	var connectCalls int

	g := &GCE{
		config: GCEConfig{
			CloudProject: "fuchsia-infra",
			MachineShape: "c4a-standard-4",
		},
		loggerCtx:            ctx,
		serialConnectBackoff: retry.NewConstantBackoff(5 * time.Millisecond),
	}
	g.createInstanceFn = func() error {
		createCalls++
		if createCalls == 1 {
			return errors.New("transient mediator 503")
		}
		g.config.InstanceName = "fuchsia-gce-live-a"
		g.config.Zone = "us-central1-a"
		return nil
	}
	g.connectToSerialFn = func() error {
		connectCalls++
		return nil
	}

	if err := g.initTarget(ctx); err != nil {
		t.Fatalf("g.initTarget() failed: %s", err)
	}
	if createCalls != 2 {
		t.Errorf("createCalls = %d, want 2", createCalls)
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

	if err := g.initTarget(ctx); err == nil {
		t.Fatal("g.initTarget() succeeded, want timeout error")
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
