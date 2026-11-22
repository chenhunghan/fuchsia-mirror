// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"os/user"
	"strings"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/lib/iomisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/retry"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"

	"golang.org/x/crypto/ssh"
)

const (
	gcemClientBinary = "./gcem_client"

	defaultMediatorTimeout       = 5 * time.Minute
	defaultCreateInstanceTimeout = 10 * time.Minute
	defaultInitTargetTimeout     = 18 * time.Minute
	defaultSerialConnectTimeout  = 3 * time.Minute
	defaultSerialDialTimeout     = 30 * time.Second
	defaultSerialBootTimeout     = 2 * time.Minute
	defaultSerialReadTimeout     = 30 * time.Second
	defaultSerialRetryDelay      = 5 * time.Second
	defaultResolveIPTimeout      = 1 * time.Minute
	maxCreateInstanceAttempts    = 3

	serialTermReadyMarker   = "TERM_IS_dumb"
	serialDataMountedMarker = "DATA_MOUNTED /data"
	sshKeyProvisionedMarker = "SSH_KEY_PROVISIONED"
)

// gceSerial is a ReadWriteCloser that talks to a GCE serial port via SSH.
type gceSerial struct {
	mu      sync.Mutex
	writeMu sync.Mutex
	in      io.WriteCloser
	out     io.Reader
	sess    *ssh.Session
	client  *ssh.Client
	closed  bool
}

func newGCESerial(pkeyPath, username, endpoint string) (*gceSerial, error) {
	return newGCESerialWithTimeout(pkeyPath, username, endpoint, defaultSerialDialTimeout)
}

func newGCESerialWithTimeout(pkeyPath, username, endpoint string, timeout time.Duration) (*gceSerial, error) {
	// Load the pkey and use it to dial the GCE serial port.
	data, err := os.ReadFile(pkeyPath)
	if err != nil {
		return nil, err
	}
	signer, err := ssh.ParsePrivateKey(data)
	if err != nil {
		return nil, err
	}
	sshConfig := &ssh.ClientConfig{
		User: username,
		Auth: []ssh.AuthMethod{
			ssh.PublicKeys(signer),
		},
		// TODO(rudymathu): Replace this with google ssh serial port key.
		HostKeyCallback: ssh.InsecureIgnoreHostKey(),
		Timeout:         timeout,
	}
	conn, err := net.DialTimeout("tcp", endpoint, timeout)
	if err != nil {
		return nil, err
	}
	if timeout > 0 {
		if err := conn.SetDeadline(time.Now().Add(timeout)); err != nil {
			_ = conn.Close()
			return nil, err
		}
	}
	sshConn, chans, reqs, err := ssh.NewClientConn(conn, endpoint, sshConfig)
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	client := ssh.NewClient(sshConn, chans, reqs)

	// Create an SSH shell and wire up stdio.
	session, err := client.NewSession()
	if err != nil {
		_ = client.Close()
		return nil, err
	}
	out, err := session.StdoutPipe()
	if err != nil {
		_ = session.Close()
		_ = client.Close()
		return nil, err
	}
	in, err := session.StdinPipe()
	if err != nil {
		_ = session.Close()
		_ = client.Close()
		return nil, err
	}
	if err := session.Shell(); err != nil {
		_ = session.Close()
		_ = client.Close()
		return nil, err
	}
	if timeout > 0 {
		// Clear the connection deadline now that the TCP connection, SSH
		// handshake, and serial shell session are established, so subsequent
		// serial reads and writes during the test run do not time out.
		if err := conn.SetDeadline(time.Time{}); err != nil {
			_ = session.Close()
			_ = client.Close()
			return nil, err
		}
	}
	return &gceSerial{
		in:     in,
		out:    out,
		sess:   session,
		client: client,
	}, nil
}

func (s *gceSerial) Read(b []byte) (int, error) {
	s.mu.Lock()
	closed := s.closed
	s.mu.Unlock()
	if closed {
		return 0, os.ErrClosed
	}
	return s.out.Read(b)
}

func (s *gceSerial) Write(b []byte) (int, error) {
	s.mu.Lock()
	closed := s.closed
	s.mu.Unlock()
	if closed {
		return 0, os.ErrClosed
	}
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	// Chunk out writes to 128 bytes or less. SSH connections to GCE do not
	// seem to properly handle longer messages.
	maxChunkSize := 128
	numChunks := len(b) / maxChunkSize
	if len(b)%maxChunkSize != 0 {
		numChunks++
	}
	bytesWritten := 0
	for i := 0; i < numChunks; i++ {
		start := i * maxChunkSize
		end := min(start+maxChunkSize, len(b))
		n, err := s.in.Write(b[start:end])
		bytesWritten += n
		if err != nil {
			return bytesWritten, err
		}
		time.Sleep(100 * time.Millisecond)
	}
	return bytesWritten, nil
}

func (s *gceSerial) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	s.closed = true
	multierr := ""
	if err := s.in.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH session input pipe: %s, ", err)
	}
	if err := s.sess.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH session: %s, ", err)
	}
	if err := s.client.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH client: %s", err)
	}
	if multierr != "" {
		return errors.New(multierr)
	}
	return nil
}

// GCEConfig represents the on disk config used by botanist to launch a GCE
// instance.
type GCEConfig struct {
	// MediatorURL is the url of the GCE Mediator.
	MediatorURL string `json:"mediator_url"`
	// BuildID is the swarming task ID of the associated build.
	BuildID string `json:"build_id"`
	// CloudProject is the cloud project to create the GCE Instance in.
	CloudProject string `json:"cloud_project"`
	// SwarmingServer is the URL to the swarming server that fed us this
	// task.
	SwarmingServer string `json:"swarming_server"`
	// MachineShape is the shape of the instance we want to create.
	MachineShape string `json:"machine_shape"`
	// InstanceName is the name of the instance.
	InstanceName string `json:"instance_name"`
	// Zone is the cloud zone in which the instance lives.
	Zone string `json:"zone"`
}

// GCE represents a GCE VM running Fuchsia.
type GCE struct {
	*genericFuchsiaTarget
	config                GCEConfig
	currentUser           string
	ipv4                  net.IP
	loggerCtx             context.Context
	opts                  Options
	serial                io.ReadWriteCloser
	createInstanceTimeout time.Duration
	initTargetTimeout     time.Duration
	serialConnectTimeout  time.Duration
	resolveIPTimeout      time.Duration
	// serialConnectBackoff overrides the default backoff used when retrying
	// serial connection, GCE instance creation, and IPv4 resolution.
	serialConnectBackoff retry.Backoff
	serialBootTimeout    time.Duration
	serialReadTimeout    time.Duration
	serialRetryDelay     time.Duration
	createdMachineShape  string
	execCommand          func(ctx context.Context, name string, args ...string) *exec.Cmd
	createInstanceFn     func() error
	addSSHKeyFn          func() error
	connectToSerialFn    func() error
	resolveIPv4Fn        func() error
}

var _ FuchsiaTarget = (*GCE)(nil)

// createInstanceRes is returned by the gcem_client's create-instance
// subcommand. Its schema is determined by the CreateInstanceRes proto
// message in http://google3/turquoise/infra/gce_mediator/proto/mediator.proto.
type createInstanceRes struct {
	InstanceName string `json:"instanceName"`
	Zone         string `json:"zone"`
	MachineShape string `json:"machineShape"`
}

// NewGCE creates, starts, and connects to the serial console of a GCE VM.
func NewGCE(ctx context.Context, config GCEConfig, opts Options) (*GCE, error) {
	u, err := user.Current()
	if err != nil {
		return nil, err
	}
	g := &GCE{
		config:      config,
		currentUser: u.Username,
		loggerCtx:   ctx,
		opts:        opts,
	}

	if err := g.initTarget(ctx); err != nil {
		return nil, err
	}

	base, err := newGenericFuchsia(ctx, g.Nodename(), "", []string{g.opts.SSHKey}, g.serial)
	if err != nil {
		return nil, err
	}
	g.genericFuchsiaTarget = base
	return g, nil
}

func defaultExponentialBackoff() retry.Backoff {
	return retry.NewExponentialBackoff(15*time.Second, 30*time.Second, 2)
}

func (g *GCE) initTarget(ctx context.Context) error {
	connectFn := g.connectToSerialFn
	if connectFn == nil {
		connectFn = g.connectToSerial
	}
	resolveIPFn := g.resolveIPv4Fn
	if resolveIPFn == nil {
		resolveIPFn = g.resolveIPv4
	}
	createInstanceTimeout := g.createInstanceTimeout
	if createInstanceTimeout <= 0 {
		createInstanceTimeout = defaultCreateInstanceTimeout
	}
	initTargetTimeout := g.initTargetTimeout
	if initTargetTimeout <= 0 {
		initTargetTimeout = defaultInitTargetTimeout
	}
	connectTimeout := g.serialConnectTimeout
	if connectTimeout <= 0 {
		connectTimeout = defaultSerialConnectTimeout
	}
	resolveIPTimeout := g.resolveIPTimeout
	if resolveIPTimeout <= 0 {
		resolveIPTimeout = defaultResolveIPTimeout
	}
	// retry.WithMaxDuration only checks elapsed time between attempts rather than
	// interrupting an in-flight call or backoff sleep, so callers pair newBackoff
	// with context.WithTimeout.
	newBackoff := func(timeout time.Duration) retry.Backoff {
		baseBackoff := g.serialConnectBackoff
		if baseBackoff == nil {
			baseBackoff = defaultExponentialBackoff()
		}
		return retry.WithMaxDuration(baseBackoff, timeout)
	}

	connectSerial := func(serialCtx context.Context) error {
		if err := serialCtx.Err(); err != nil {
			return err
		}
		logger.Infof(ctx, "setting up the serial connection to the GCE instance")
		connectCtx, cancelConnect := context.WithTimeout(serialCtx, connectTimeout)
		defer cancelConnect()
		connectSerialErrs := make(chan error)
		defer close(connectSerialErrs)
		go logErrors(ctx, "connectToSerial()", connectSerialErrs)
		if err := retry.Retry(connectCtx, newBackoff(connectTimeout), connectFn, connectSerialErrs); err != nil {
			return wrapContextErr(serialCtx, wrapContextErr(connectCtx, err))
		}
		logger.Infof(ctx, "successfully connected to serial")
		return nil
	}

	resolveIP := func(parentCtx context.Context) error {
		if err := parentCtx.Err(); err != nil {
			return fmt.Errorf("failed to resolve IPv4 for GCE instance %s: %w", g.config.InstanceName, err)
		}
		logger.Infof(ctx, "resolving the IPv4 address of GCE instance %s", g.config.InstanceName)
		resolveCtx, cancelResolve := context.WithTimeout(parentCtx, resolveIPTimeout)
		defer cancelResolve()
		resolveIPErrs := make(chan error)
		defer close(resolveIPErrs)
		go logErrors(ctx, "resolveIPv4()", resolveIPErrs)
		baseBackoff := g.serialConnectBackoff
		if baseBackoff == nil {
			baseBackoff = retry.NewExponentialBackoff(time.Second, 10*time.Second, 2)
		}
		if err := retry.Retry(resolveCtx, retry.WithMaxDuration(baseBackoff, resolveIPTimeout), resolveIPFn, resolveIPErrs); err != nil {
			return fmt.Errorf("failed to resolve IPv4 for GCE instance %s: %w", g.config.InstanceName, wrapContextErr(parentCtx, wrapContextErr(resolveCtx, err)))
		}
		logger.Infof(ctx, "resolved IPv4 address %s for GCE instance %s (nodename %s)", g.ipv4, g.config.InstanceName, g.Nodename())
		return nil
	}

	// LINT.IfChange(gce_init_failed)
	if g.config.InstanceName == "" && g.config.Zone == "" {
		// Note: gce_mediator's RPC interface (and gcem_client) does not expose a
		// DeleteInstance method, so botanist cannot directly delete an orphaned VM
		// if serial connection fails. Instead, gce_mediator associates every
		// created instance in Datastore with the Swarming task ID and automatically
		// garbage-collects all instances belonging to the task once it completes.
		initCtx, cancelInit := context.WithTimeout(ctx, initTargetTimeout)
		defer cancelInit()

		var lastErr error
		for attempt := 1; attempt <= maxCreateInstanceAttempts; attempt++ {
			if initCtx.Err() != nil {
				if ctx.Err() != nil {
					if lastErr != nil {
						return fmt.Errorf("failed to connect to GCE instance: %w", wrapContextErr(ctx, lastErr))
					}
					return fmt.Errorf("failed to create GCE instance: %w", ctx.Err())
				}
				if lastErr != nil {
					return fmt.Errorf("failed to create and connect to GCE instance: timed out initializing GCE instance after %s: %w", initTargetTimeout, wrapContextErr(initCtx, lastErr))
				}
				return fmt.Errorf("failed to create GCE instance: timed out initializing GCE instance after %s: %w", initTargetTimeout, initCtx.Err())
			}
			g.config.InstanceName = ""
			g.config.Zone = ""
			g.createdMachineShape = ""
			g.ipv4 = nil
			logger.Infof(ctx, "creating the GCE instance (attempt %d/%d)", attempt, maxCreateInstanceAttempts)
			err := func() error {
				createCtx, cancelCreate := context.WithTimeout(initCtx, createInstanceTimeout)
				defer cancelCreate()
				createFn := g.createInstanceFn
				if createFn == nil {
					createFn = func() error {
						return g.createInstance(createCtx)
					}
				}
				createErrs := make(chan error)
				defer close(createErrs)
				go logErrors(ctx, "createInstance()", createErrs)
				return wrapContextErr(createCtx, retry.Retry(createCtx, newBackoff(createInstanceTimeout), createFn, createErrs))
			}()
			if err != nil {
				if ctx.Err() != nil {
					return fmt.Errorf("failed to create GCE instance: %w", wrapContextErr(ctx, err))
				}
				if initCtx.Err() != nil {
					return fmt.Errorf("failed to create GCE instance: timed out initializing GCE instance after %s: %w", initTargetTimeout, wrapContextErr(initCtx, err))
				}
				// Return immediately rather than repeating the createInstanceTimeout
				// createInstance backoff loop maxCreateInstanceAttempts times;
				// the outer loop is strictly for recreating a VM whose serial
				// console failed to become reachable after creation succeeded.
				return fmt.Errorf("failed to create GCE instance: %w", err)
			}
			if g.createdMachineShape != "" {
				logger.Infof(ctx, "successfully created GCE instance: Name: %s, Zone: %s, Shape: %s", g.config.InstanceName, g.config.Zone, g.createdMachineShape)
			} else {
				logger.Infof(ctx, "successfully created GCE instance: Name: %s, Zone: %s", g.config.InstanceName, g.config.Zone)
			}
			if err := connectSerial(initCtx); err != nil {
				lastErr = err
				if ctx.Err() != nil {
					return fmt.Errorf("failed to connect to GCE instance: %w", wrapContextErr(ctx, err))
				}
				if initCtx.Err() != nil {
					return fmt.Errorf("failed to create and connect to GCE instance: timed out initializing GCE instance after %s: %w", initTargetTimeout, wrapContextErr(initCtx, lastErr))
				}
				if attempt == maxCreateInstanceAttempts {
					break
				}
				logger.Warningf(ctx, "failed to connect to serial for GCE instance %s in %s (%s); recreating instance", g.config.InstanceName, g.config.Zone, err)
				continue
			}
			return resolveIP(initCtx)
		}
		return fmt.Errorf("failed to create and connect to GCE instance after %d attempts: %w", maxCreateInstanceAttempts, lastErr)
	}

	// The instance has already been created, so add the SSH key to it.
	logger.Infof(ctx, "adding the SSH public key to GCE instance %s", g.config.InstanceName)
	if err := func() error {
		addKeyCtx, cancelAddKey := context.WithTimeout(ctx, defaultMediatorTimeout)
		defer cancelAddKey()
		addKeyFn := g.addSSHKeyFn
		if addKeyFn == nil {
			addKeyFn = func() error {
				return g.addSSHKey(addKeyCtx)
			}
		}
		return wrapContextErr(addKeyCtx, retry.Retry(addKeyCtx, newBackoff(defaultMediatorTimeout), addKeyFn, nil))
	}(); err != nil {
		return fmt.Errorf("failed to connect to GCE instance: %w", wrapContextErr(ctx, err))
	}
	logger.Infof(ctx, "successfully added SSH key")
	if err := connectSerial(ctx); err != nil {
		return fmt.Errorf("failed to connect to GCE instance: %w", wrapContextErr(ctx, err))
	}
	return resolveIP(ctx)
	// LINT.ThenChange(//tools/testing/tefmocheck/check-configs.yaml:gce_init_failed)
}

func logErrors(ctx context.Context, functionName string, errs <-chan error) {
	for {
		err, more := <-errs
		if err != nil {
			logger.Errorf(ctx, "%s failed: %s, retrying", functionName, err)
		}
		if !more {
			return
		}
	}
}

// serialTimeouts returns the effective serial console boot timeout, per-read IO
// timeout, and retry delay, falling back to connectionTimeout or the package
// defaults when unset on g.
func (g *GCE) serialTimeouts() (bootTimeout, readTimeout, retryDelay time.Duration) {
	bootTimeout = g.serialBootTimeout
	if bootTimeout <= 0 {
		if g.connectionTimeout > 0 {
			bootTimeout = g.connectionTimeout
		} else {
			bootTimeout = defaultSerialBootTimeout
		}
	}
	readTimeout = g.serialReadTimeout
	if readTimeout <= 0 {
		readTimeout = defaultSerialReadTimeout
	}
	retryDelay = g.serialRetryDelay
	if retryDelay <= 0 {
		retryDelay = defaultSerialRetryDelay
	}
	return bootTimeout, readTimeout, retryDelay
}

// waitForSerialReady starts the local serial server (if not already started)
// and waits up to bootTimeout for the GCE serial console to emit a shell
// cursor ("$ "), returning the connected SerialSocket.
func (g *GCE) waitForSerialReady(ctx context.Context, bootTimeout, readTimeout, retryDelay time.Duration) (*serial.SerialSocket, error) {
	if g.serial == nil {
		return nil, fmt.Errorf("serial is not connected")
	}
	if err := g.StartSerialServer(); err != nil {
		return nil, err
	}

	logger.Infof(g.loggerCtx, "waiting for GCE serial console to be ready")
	bootCtx, cancel := context.WithTimeout(ctx, bootTimeout)
	defer cancel()
	var socket *serial.SerialSocket
	bootBackoff := retry.WithMaxDuration(retry.NewConstantBackoff(retryDelay), bootTimeout)
	if err := retry.Retry(bootCtx, bootBackoff, func() error {
		var err error
		socket, err = serial.NewSocketWithIOTimeout(bootCtx, g.SerialSocketPath(), readTimeout, true)
		return err
	}, nil); err != nil {
		return nil, err
	}
	return socket, nil
}

// runSerialCommandWithRetry sends cmd over *socket and waits up to readTimeout
// for expected to appear in serial output, retrying up to maxAttempts times on
// error or timeout. On a failed attempt, *socket is closed so the next attempt
// opens a fresh connection; on success, *socket remains open for reuse.
func (g *GCE) runSerialCommandWithRetry(
	ctx context.Context,
	socket **serial.SerialSocket,
	cmd []string,
	expected string,
	readTimeout, retryDelay time.Duration,
	maxAttempts uint64,
	timeoutWarning string,
) error {
	retryBackoff := retry.WithMaxAttempts(retry.NewConstantBackoff(retryDelay), maxAttempts)
	return retry.Retry(ctx, retryBackoff, func() error {
		if *socket == nil {
			s, err := serial.NewSocketWithIOTimeout(ctx, g.SerialSocketPath(), readTimeout, false)
			if err != nil {
				return err
			}
			*socket = s
		}
		if err := serial.RunCommands(ctx, *socket, []serial.Command{{Cmd: cmd}}); err != nil {
			(*socket).Close()
			*socket = nil
			return err
		}
		truncCtx, cancel := context.WithTimeout(ctx, readTimeout)
		_, readErr := iomisc.ReadUntilMatchString(truncCtx, *socket, expected)
		cancel()
		if readErr != nil {
			if errors.Is(readErr, truncCtx.Err()) || errors.Is(readErr, os.ErrDeadlineExceeded) {
				logger.Warningf(ctx, "%s", timeoutWarning)
			} else {
				logger.Errorf(ctx, "unexpected error checking serial output for %q: %s", expected, readErr)
			}
			(*socket).Close()
			*socket = nil
		}
		return readErr
	}, nil)
}

// Provisions an SSH key over the serial connection.
func (g *GCE) provisionSSHKey(ctx context.Context) error {
	if g.serial == nil {
		return fmt.Errorf("serial is not connected")
	}
	if err := g.StartSerialServer(); err != nil {
		return err
	}
	p, err := os.ReadFile(g.opts.AuthorizedKey)
	if err != nil {
		return err
	}
	var pubkey string
	if parsedKey, _, _, _, err := ssh.ParseAuthorizedKey(p); err == nil {
		pubkey = strings.TrimSpace(string(ssh.MarshalAuthorizedKey(parsedKey)))
	} else {
		pubkey = strings.TrimSpace(string(p))
	}
	pubkey = fmt.Sprintf("%s %s", pubkey, g.currentUser)

	bootTimeout, readTimeout, retryDelay := g.serialTimeouts()
	socket, err := g.waitForSerialReady(ctx, bootTimeout, readTimeout, retryDelay)
	if err != nil {
		return err
	}
	defer func() {
		if socket != nil {
			socket.Close()
		}
	}()

	logger.Infof(g.loggerCtx, "disabling serial echo (TERM=dumb)")
	// Switch zxsh (sh:console) from UartEcho (TERM=uart) to PromptOnly
	// (TERM=dumb) so the shell does not echo subsequent commands
	// character-by-character through the kernel serial lock or overflow
	// console.cm's 255-byte input FIFO. Validate via "$TERM" expansion so
	// echoed command text cannot false-match before export takes effect.
	termCmd := []string{"export", "TERM=dumb", "&&", "echo", "TERM_IS_$TERM"}
	if err := g.runSerialCommandWithRetry(
		ctx,
		&socket,
		termCmd,
		serialTermReadyMarker,
		readTimeout,
		retryDelay,
		3,
		"timed out waiting for TERM=dumb confirmation over serial",
	); err != nil {
		return fmt.Errorf("failed to set TERM=dumb over serial: %w", err)
	}

	logger.Infof(g.loggerCtx, "waiting for /data to be mounted over serial")
	// Wait for console-launcher/fshost to mount /data in a bounded loop
	// (up to 15s per attempt) before writing /data/ssh/authorized_keys, so
	// storage mount failures surface distinctly from SSH key corruption.
	dataWaitCmd := []string{
		"d=/data;", "i=0;",
		"while", "!", "[", "-d", "\"$d\"", "]", "&&", "[", "$i", "-lt", "150", "];", "do", "msleep", "100;", "i=$((i+1));", "done;",
		"[", "-d", "\"$d\"", "]", "&&", "echo", "\"DATA_MOUNTED $d\"",
	}
	if err := g.runSerialCommandWithRetry(
		ctx,
		&socket,
		dataWaitCmd,
		serialDataMountedMarker,
		readTimeout,
		retryDelay,
		3,
		"timed out waiting for /data to be mounted over serial",
	); err != nil {
		return fmt.Errorf("failed waiting for /data to be mounted over serial: %w", err)
	}

	logger.Infof(g.loggerCtx, "provisioning SSH key over serial")
	expected := fmt.Sprintf("%s %s", sshKeyProvisionedMarker, pubkey)
	provisionCmd := []string{
		"mkdir", "-p", "/data/ssh",
		"&&", "echo", fmt.Sprintf("%q", pubkey), ">", "/data/ssh/authorized_keys",
		"&&", "read", "-r", "line", "<", "/data/ssh/authorized_keys",
		"&&", "echo", fmt.Sprintf("\"%s $line\"", sshKeyProvisionedMarker),
	}
	if err := g.runSerialCommandWithRetry(
		ctx,
		&socket,
		provisionCmd,
		expected,
		readTimeout,
		retryDelay,
		5,
		"ssh key is corrupted",
	); err != nil {
		return fmt.Errorf("failed to provision SSH key over serial: %w", err)
	}
	logger.Infof(g.loggerCtx, "successfully provisioned SSH key")
	return nil
}

func getSerialEndpoint(zone string) string {
	zoneParts := strings.Split(zone, "-")
	region := strings.ToLower(strings.Join(zoneParts[:len(zoneParts)-1], "-"))
	return fmt.Sprintf("%s-ssh-serialport.googleapis.com:9600", region)
}

func (g *GCE) connectToSerial() error {
	username := fmt.Sprintf(
		"%s.%s.%s.%s.%s",
		g.config.CloudProject,
		g.config.Zone,
		g.config.InstanceName,
		g.currentUser,
		"replay-from=0",
	)
	serial, err := newGCESerial(g.opts.SSHKey, username, getSerialEndpoint(g.config.Zone))
	g.serial = serial
	return err
}

func (g *GCE) commandContext(ctx context.Context, name string, args ...string) *exec.Cmd {
	if g.execCommand != nil {
		return g.execCommand(ctx, name, args...)
	}
	return exec.CommandContext(ctx, name, args...)
}

func wrapContextErr(ctx context.Context, err error) error {
	if err == nil {
		return nil
	}
	if ctxErr := ctx.Err(); ctxErr != nil && !errors.Is(err, ctxErr) {
		return fmt.Errorf("%w: %w", ctxErr, err)
	}
	return err
}

func (g *GCE) addSSHKey(ctx context.Context) error {
	invocation := []string{
		gcemClientBinary,
		"add-ssh-key",
		"-host", g.config.MediatorURL,
		"-project", g.config.CloudProject,
		"-instance-name", g.config.InstanceName,
		"-zone", g.config.Zone,
		"-user", g.currentUser,
		"-pubkey", g.opts.AuthorizedKey,
	}
	logger.Infof(g.loggerCtx, "GCE Mediator client command: %s", invocation)
	cmd := g.commandContext(ctx, invocation[0], invocation[1:]...)
	stdout, stderr, flush := botanist.NewStdioWriters(g.loggerCtx, "gce")
	defer flush()
	cmd.Stdout = stdout
	cmd.Stderr = stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("gcem_client add-ssh-key failed: %w", wrapContextErr(ctx, err))
	}
	return nil
}

// isPermanentCreateInstanceError reports whether stderr from gcem_client
// create-instance indicates a terminal failure that cannot succeed on retry.
func isPermanentCreateInstanceError(stderr string) bool {
	for _, substr := range []string{
		"code = InvalidArgument",
		"code = ResourceExhausted",
		"code = PermissionDenied",
		"code = Unauthenticated",
		"is in terminal status FAILED",
		"is in terminal status DELETING",
		"failed to read pubkey file",
		"failed to parse public key",
		"-machine-shape cannot be empty",
	} {
		if strings.Contains(stderr, substr) {
			return true
		}
	}
	return false
}

func (g *GCE) createInstance(ctx context.Context) error {
	taskID := os.Getenv("SWARMING_TASK_ID")
	if taskID == "" {
		return retry.Fatal(errors.New("task did not specify SWARMING_TASK_ID"))
	}

	invocation := []string{
		gcemClientBinary,
		"create-instance",
		"-host", g.config.MediatorURL,
		"-project", g.config.CloudProject,
		"-build-id", g.config.BuildID,
		"-task-id", taskID,
		"-swarming-host", g.config.SwarmingServer,
		"-machine-shape", g.config.MachineShape,
		"-user", g.currentUser,
		"-pubkey", g.opts.AuthorizedKey,
	}

	logger.Infof(g.loggerCtx, "GCE Mediator client command: %s", invocation)
	cmd := g.commandContext(ctx, invocation[0], invocation[1:]...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = io.MultiWriter(os.Stderr, &stderr)

	if err := cmd.Run(); err != nil {
		ctxCanceled := ctx.Err() != nil
		err = wrapContextErr(ctx, err)
		errMsg := strings.TrimSpace(stderr.String())
		if errMsg != "" {
			err = fmt.Errorf("gcem_client create-instance failed: %w: %s", err, errMsg)
		} else {
			err = fmt.Errorf("gcem_client create-instance failed: %w", err)
		}
		if !ctxCanceled && isPermanentCreateInstanceError(errMsg) {
			return retry.Fatal(err)
		}
		return err
	}

	var res createInstanceRes
	if err := json.Unmarshal(stdout.Bytes(), &res); err != nil {
		return fmt.Errorf("failed to decode gcem_client create-instance output: %w", err)
	}
	g.config.InstanceName = res.InstanceName
	g.config.Zone = res.Zone
	g.createdMachineShape = res.MachineShape
	return nil
}

// generatePrivateKey generates a 2048 bit RSA private key, writes it to
// a temporary file, and returns the path to the key.
func generatePrivateKey() (string, error) {
	pkey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return "", err
	}
	f, err := os.CreateTemp("", "gce_pkey")
	if err != nil {
		return "", err
	}
	defer f.Close()
	pemBlock := &pem.Block{
		Type:    "RSA PRIVATE KEY",
		Headers: nil,
		Bytes:   x509.MarshalPKCS1PrivateKey(pkey),
	}
	return f.Name(), pem.Encode(f, pemBlock)
}

// generatePublicKey reads the private key at path pkey and generates a public
// key in Authorized Keys format. Returns the path to the public key file.
func generatePublicKey(pkeyFile string) (string, error) {
	if pkeyFile == "" {
		return "", errors.New("no private key file provided")
	}
	data, err := os.ReadFile(pkeyFile)
	if err != nil {
		return "", err
	}
	block, _ := pem.Decode(data)
	pkey, err := x509.ParsePKCS1PrivateKey(block.Bytes)
	if err != nil {
		return "", err
	}
	pubkey, err := ssh.NewPublicKey(pkey.Public())
	if err != nil {
		return "", err
	}
	f, err := os.CreateTemp("", "gce_pubkey")
	if err != nil {
		return "", err
	}
	defer f.Close()
	_, err = f.Write(ssh.MarshalAuthorizedKey(pubkey))
	return f.Name(), err
}

func (g *GCE) resolveIPv4() error {
	if g.config.InstanceName == "" || g.config.Zone == "" || g.config.CloudProject == "" {
		return fmt.Errorf("cannot resolve IPv4 with incomplete GCE config (instance=%q, zone=%q, project=%q)", g.config.InstanceName, g.config.Zone, g.config.CloudProject)
	}
	fqdn := fmt.Sprintf("%s.%s.c.%s.internal", g.config.InstanceName, g.config.Zone, g.config.CloudProject)
	addr, err := net.ResolveIPAddr("ip4", fqdn)
	if err != nil {
		if g.loggerCtx != nil {
			logger.Infof(g.loggerCtx, "failed to resolve IPv4 of instance %s: %s", g.config.InstanceName, err)
		}
		return err
	}
	g.ipv4 = addr.IP
	return nil
}

func (g *GCE) IPv4() (net.IP, error) {
	if g.ipv4 == nil {
		if err := g.resolveIPv4(); err != nil {
			return nil, err
		}
	}
	return g.ipv4, nil
}

// GCE targets don't have IPv6 addresses.
func (g *GCE) IPv6() (*net.IPAddr, error) {
	return nil, nil
}

// Nodename returns the Fuchsia nodename of the GCE instance.
//
// GCE assigns every VM's primary network interface (nic0) a deterministic MAC
// address with the 42:01 locally administered OUI prefix followed by the 4
// octets of its internal IPv4 address (42:01:ip[0]:ip[1]:ip[2]:ip[3]), which
// device-name-provider formats as "fuchsia-4201-XXXX-XXXX".
//
// g.ipv4 is resolved during target initialization (initTarget), so Nodename
// reads the cached IPv4 address without performing network lookups or mutating
// target state, falling back to the GCE instance name if no IPv4 address is
// set.
func (g *GCE) Nodename() string {
	if ip4 := g.ipv4.To4(); ip4 != nil {
		return fmt.Sprintf("fuchsia-4201-%02x%02x-%02x%02x", ip4[0], ip4[1], ip4[2], ip4[3])
	}
	return g.config.InstanceName
}

func (g *GCE) Serial() io.ReadWriteCloser {
	return g.serial
}

func (g *GCE) SSHKey() string {
	return g.opts.SSHKey
}

func (g *GCE) UseProductBundles() bool {
	return false
}

func (g *GCE) Start(ctx context.Context, _ []string, _ string, _ bool) error {
	// If we're running a non-bringup configuration, we need to provision an SSH key.
	if g.opts.ExpectsSSH {
		return g.provisionSSHKey(ctx)
	}
	bootTimeout, readTimeout, retryDelay := g.serialTimeouts()
	socket, err := g.waitForSerialReady(ctx, bootTimeout, readTimeout, retryDelay)
	if socket != nil {
		_ = socket.Close()
	}
	return err
}

func (g *GCE) Stop() error {
	g.genericFuchsiaTarget.Stop()
	if g.serial == nil {
		return nil
	}
	return g.serial.Close()
}

func (g *GCE) Wait(context.Context) error {
	return ErrUnimplemented
}

func (g *GCE) SSHClient() (*sshutil.Client, error) {
	addr, err := g.IPv4()
	if err != nil {
		return nil, err
	}

	return g.sshClient(&net.IPAddr{IP: addr}, "gce")
}

func (g *GCE) TestConfig(expectsSSH bool) (any, error) {
	return TestbedConfig(g, expectsSSH, nil)
}
