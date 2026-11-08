// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bufio"
	"bytes"
	"context"
	"crypto/tls"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"strings"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"golang.org/x/net/dns/dnsmessage"
)

var (
	slirpGatewayMAC = [6]byte{0x52, 0x55, 0x0a, 0x00, 0x02, 0x02}
	slirpGatewayIP  = [4]byte{10, 0, 2, 2}
	slirpDNSIP      = [4]byte{10, 0, 2, 3}
	slirpGuestIP    = [4]byte{10, 0, 2, 50}
	slirpProxyIP    = [4]byte{10, 0, 2, 100}
	slirpSubnetMask = [4]byte{255, 255, 255, 0}
	broadcastMAC    = [6]byte{0xff, 0xff, 0xff, 0xff, 0xff, 0xff}
	broadcastIP     = [4]byte{255, 255, 255, 255}
	zeroIP          = [4]byte{0, 0, 0, 0}
)

const (
	// slirpBootpSlots is NB_BOOTP_CLIENTS in libslirp/src/bootp.h (16).
	slirpBootpSlots = 16
	dhcpLeaseSecs   = 86400
)

// RunTCPForward bridges stdin/stdout to the specified TCP address for QEMU
// SLiRP `guestfwd=...-cmd:<botanist> tcp-forward <addr>` invocations.
// Note: libslirp's fork_exec() binds stdin, stdout, and stderr to the guest
// socketpair, so this function must never write log messages to stdout/stderr.
func RunTCPForward(addr string, r io.Reader, w io.Writer) int {
	conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		return 1
	}
	defer conn.Close()

	go func() {
		_, _ = io.Copy(conn, r)
		if tc, ok := conn.(*net.TCPConn); ok {
			_ = tc.CloseWrite()
		}
	}()
	// Return as soon as the proxy closes the connection, without waiting for
	// the guest to close its side: SLiRP only closes the guest's connection
	// when this process exits, so waiting for the guest would leave
	// connections that the proxy rejected open until the guest times out.
	_, _ = io.Copy(w, conn)
	return 0
}

// emuNetProxy provides a domain-restricted network proxy for QEMU/FEMU running
// with `-netdev user,id=user0,restrict=on,ipv6=off`:
//
//  1. Because libslirp's `restrict=on` mode omits DHCP Option 3 (Router) in
//     bootp.c and drops all UDP packets (including DNS to 8.8.8.8/8.8.4.4) in
//     udp.c, emuNetProxy attaches a companion `-netdev socket` (`netproxy0`) to
//     `hubid=0` that pre-allocates libslirp's 16 BOOTP slots, replies to the
//     guest's DHCP requests with a default gateway (`10.0.2.2`), and answers
//     UDP port 53 DNS queries (`A 10.0.2.100` for allowlisted domains,
//     `NXDOMAIN` for all other domains).
//  2. Guest TCP traffic to `10.0.2.100:443`, `10.0.2.100:5228`, and
//     `10.0.2.100:80` is forwarded by SLiRP `guestfwd` rules to host-side TLS
//     SNI and HTTP Host proxies that validate the target domain against the
//     allowlist and verify that resolved IPs are global public unicast
//     addresses (blocking host loopback `127.0.0.0/8`, private networks, and
//     link-local metadata addresses).
type emuNetProxy struct {
	ctx            context.Context
	cancel         context.CancelFunc
	allowedDomains []string
	execPath       string
	netdevLn       net.Listener
	tls443Ln       net.Listener
	tls5228Ln      net.Listener
	http80Ln       net.Listener
	dialer         func(ctx context.Context, host string, port int) (net.Conn, error)

	// lookupIPAddr and dialContext resolve and connect to upstream hosts in
	// dialAllowedHost. They are fields so that tests can fake them.
	lookupIPAddr func(ctx context.Context, host string) ([]net.IPAddr, error)
	dialContext  func(ctx context.Context, network, address string) (net.Conn, error)
	// handshakeTimeout bounds how long a guest connection may take to send
	// its TLS ClientHello or HTTP request headers.
	handshakeTimeout time.Duration
	wg               sync.WaitGroup

	// connsMu guards conns and closed. conns are the guest and upstream
	// connections that are open, which Close closes so that it doesn't wait
	// for long-lived connections to end on their own.
	connsMu   sync.Mutex
	conns     map[net.Conn]struct{}
	closed    bool
	closeOnce sync.Once
}

// newEmuNetProxy starts an emuNetProxy that only allows guest access to
// allowedDomains. If allowedDomains is empty, all guest network requests are
// blocked.
func newEmuNetProxy(parentCtx context.Context, allowedDomains []string, execPath string) (*emuNetProxy, error) {
	return newEmuNetProxyWithConfig(parentCtx, allowedDomains, execPath, nil)
}

// newEmuNetProxyWithConfig is newEmuNetProxy, but calls configure (if
// non-nil) before the proxy starts serving, so that tests can replace the
// proxy's dependencies without racing with its goroutines.
func newEmuNetProxyWithConfig(parentCtx context.Context, allowedDomains []string, execPath string, configure func(*emuNetProxy)) (*emuNetProxy, error) {
	if execPath == "" {
		var err error
		execPath, err = os.Executable()
		if err != nil {
			return nil, fmt.Errorf("failed to get executable path for emuNetProxy: %w", err)
		}
	}

	netdevLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("failed to listen for netdev socket: %w", err)
	}
	tls443Ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		_ = netdevLn.Close()
		return nil, fmt.Errorf("failed to listen for TLS 443 proxy: %w", err)
	}
	tls5228Ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		_ = netdevLn.Close()
		_ = tls443Ln.Close()
		return nil, fmt.Errorf("failed to listen for TLS 5228 proxy: %w", err)
	}
	http80Ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		_ = netdevLn.Close()
		_ = tls443Ln.Close()
		_ = tls5228Ln.Close()
		return nil, fmt.Errorf("failed to listen for HTTP 80 proxy: %w", err)
	}

	// Detach from parentCtx cancellation because StartTargets uses an
	// errgroup.WithContext that is canceled as soon as Start() returns,
	// whereas the network proxy must stay alive until Close() is called
	// in Stop().
	ctx, cancel := context.WithCancel(context.WithoutCancel(parentCtx))
	p := &emuNetProxy{
		ctx:            ctx,
		cancel:         cancel,
		allowedDomains: allowedDomains,
		execPath:       execPath,
		netdevLn:       netdevLn,
		tls443Ln:       tls443Ln,
		tls5228Ln:      tls5228Ln,
		http80Ln:       http80Ln,
	}
	p.dialer = p.dialAllowedHost
	p.lookupIPAddr = net.DefaultResolver.LookupIPAddr
	p.dialContext = (&net.Dialer{}).DialContext
	p.handshakeTimeout = 10 * time.Second
	if configure != nil {
		configure(p)
	}

	p.wg.Add(4)
	go p.serveNetdevSocket()
	go p.serveTLS(p.tls443Ln, 443)
	go p.serveTLS(p.tls5228Ln, 5228)
	go p.serveHTTP()

	return p, nil
}

func (p *emuNetProxy) emuArgs() []string {
	proxyIP := fmt.Sprintf("%d.%d.%d.%d", slirpProxyIP[0], slirpProxyIP[1], slirpProxyIP[2], slirpProxyIP[3])
	return []string{
		"--netdev-socket",
		p.netdevLn.Addr().String(),
		"--guestfwd",
		fmt.Sprintf("tcp:%s:443-cmd:%s tcp-forward %s", proxyIP, p.execPath, p.tls443Ln.Addr().String()),
		"--guestfwd",
		fmt.Sprintf("tcp:%s:5228-cmd:%s tcp-forward %s", proxyIP, p.execPath, p.tls5228Ln.Addr().String()),
		"--guestfwd",
		fmt.Sprintf("tcp:%s:80-cmd:%s tcp-forward %s", proxyIP, p.execPath, p.http80Ln.Addr().String()),
	}
}

// Close stops the proxy, closes all open connections, and waits for its
// goroutines to exit. It is safe to call Close more than once, including
// concurrently.
func (p *emuNetProxy) Close() error {
	p.closeOnce.Do(func() {
		p.cancel()
		_ = p.netdevLn.Close()
		_ = p.tls443Ln.Close()
		_ = p.tls5228Ln.Close()
		_ = p.http80Ln.Close()
		p.connsMu.Lock()
		p.closed = true
		for c := range p.conns {
			_ = c.Close()
		}
		p.connsMu.Unlock()
		p.wg.Wait()
	})
	return nil
}

// trackConn records c as open so that Close closes it. It returns false (and
// closes c) if the proxy is already closed.
func (p *emuNetProxy) trackConn(c net.Conn) bool {
	p.connsMu.Lock()
	defer p.connsMu.Unlock()
	if p.closed {
		_ = c.Close()
		return false
	}
	if p.conns == nil {
		p.conns = make(map[net.Conn]struct{})
	}
	p.conns[c] = struct{}{}
	return true
}

// untrackConn removes c from the connections that Close closes.
func (p *emuNetProxy) untrackConn(c net.Conn) {
	p.connsMu.Lock()
	defer p.connsMu.Unlock()
	delete(p.conns, c)
}

func (p *emuNetProxy) isDomainAllowed(host string) bool {
	return isDomainAllowedInList(host, p.allowedDomains)
}

func isDomainAllowedInList(host string, allowedDomains []string) bool {
	if h, _, err := net.SplitHostPort(host); err == nil {
		host = h
	}
	host = strings.ToLower(strings.TrimSuffix(strings.TrimSpace(host), "."))
	if host == "" || net.ParseIP(host) != nil {
		return false
	}
	for _, pattern := range allowedDomains {
		d := strings.ToLower(strings.TrimPrefix(strings.TrimSuffix(strings.TrimSpace(pattern), "."), "*."))
		if d == "" {
			continue
		}
		if host == d || strings.HasSuffix(host, "."+d) {
			return true
		}
	}
	return false
}

// nonPublicPrefixes are special-purpose address ranges (from the IANA IPv4 and
// IPv6 special-purpose address registries) that netip.Addr's predicates don't
// cover and that the proxy must never connect to, either because they aren't
// globally reachable or because they embed an IPv4 address that could be
// private (NAT64, 6to4, Teredo).
var nonPublicPrefixes = []netip.Prefix{
	netip.MustParsePrefix("0.0.0.0/8"),       // "This network" (RFC 791)
	netip.MustParsePrefix("100.64.0.0/10"),   // Carrier-grade NAT (RFC 6598)
	netip.MustParsePrefix("192.0.0.0/24"),    // IETF protocol assignments (RFC 6890)
	netip.MustParsePrefix("192.0.2.0/24"),    // Documentation, TEST-NET-1 (RFC 5737)
	netip.MustParsePrefix("198.18.0.0/15"),   // Benchmarking (RFC 2544)
	netip.MustParsePrefix("198.51.100.0/24"), // Documentation, TEST-NET-2 (RFC 5737)
	netip.MustParsePrefix("203.0.113.0/24"),  // Documentation, TEST-NET-3 (RFC 5737)
	netip.MustParsePrefix("240.0.0.0/4"),     // Reserved and limited broadcast (RFC 1112, RFC 919)
	netip.MustParsePrefix("64:ff9b::/96"),    // NAT64 well-known prefix (RFC 6052)
	netip.MustParsePrefix("64:ff9b:1::/48"),  // Local-use NAT64 (RFC 8215)
	netip.MustParsePrefix("100::/64"),        // Discard-only (RFC 6666)
	netip.MustParsePrefix("2001::/23"),       // IETF protocol assignments, including Teredo (RFC 2928)
	netip.MustParsePrefix("2001:db8::/32"),   // Documentation (RFC 3849)
	netip.MustParsePrefix("2002::/16"),       // 6to4 (RFC 3056)
	netip.MustParsePrefix("3fff::/20"),       // Documentation (RFC 9637)
	netip.MustParsePrefix("5f00::/16"),       // Segment Routing SIDs (RFC 9602)
	netip.MustParsePrefix("fec0::/10"),       // Deprecated site-local (RFC 3879)
}

// isPublicUnicastIP returns true iff ip is a global public unicast address:
// not loopback, private (RFC 1918 / RFC 4193), link-local, multicast,
// unspecified, or in nonPublicPrefixes. IPv4-mapped IPv6 addresses are checked
// as IPv4.
func isPublicUnicastIP(ip net.IP) bool {
	addr, ok := netip.AddrFromSlice(ip)
	if !ok {
		return false
	}
	addr = addr.Unmap()
	if !addr.IsGlobalUnicast() || addr.IsPrivate() {
		return false
	}
	for _, prefix := range nonPublicPrefixes {
		if prefix.Contains(addr) {
			return false
		}
	}
	return true
}

func (p *emuNetProxy) dialAllowedHost(ctx context.Context, host string, port int) (net.Conn, error) {
	if !isValidHostName(host) || !p.isDomainAllowed(host) {
		return nil, fmt.Errorf("domain %q is not in allowlist", host)
	}
	dialCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()

	addrs, err := p.lookupIPAddr(dialCtx, host)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve %q: %w", host, err)
	}
	var lastErr error
	for _, addr := range addrs {
		if !isPublicUnicastIP(addr.IP) {
			lastErr = fmt.Errorf("refusing non-public IP %s for %q", addr.IP, host)
			continue
		}
		target := net.JoinHostPort(addr.IP.String(), fmt.Sprintf("%d", port))
		conn, err := p.dialContext(dialCtx, "tcp", target)
		if err == nil {
			return conn, nil
		}
		lastErr = err
	}
	if lastErr == nil {
		lastErr = fmt.Errorf("no public IP addresses found for %q", host)
	}
	return nil, lastErr
}

func (p *emuNetProxy) serveTLS(ln net.Listener, targetPort int) {
	defer p.wg.Done()
	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		if !p.trackConn(conn) {
			return
		}
		p.wg.Add(1)
		go func(c net.Conn) {
			defer p.wg.Done()
			defer p.untrackConn(c)
			p.handleTLSConn(c, targetPort)
		}(conn)
	}
}

func (p *emuNetProxy) handleTLSConn(clientConn net.Conn, targetPort int) {
	defer clientConn.Close()
	_ = clientConn.SetReadDeadline(time.Now().Add(p.handshakeTimeout))
	sniHost, clientHello, err := readTLSClientHelloSNI(clientConn)
	if errors.Is(err, errClientHelloIncomplete) {
		// The guest closed the connection or timed out before sending a
		// complete ClientHello.
		return
	}
	_ = clientConn.SetReadDeadline(time.Time{})

	if err == nil && !isValidHostName(sniHost) {
		err = fmt.Errorf("invalid SNI host name")
	}
	if err == nil && !p.isDomainAllowed(sniHost) {
		err = fmt.Errorf("not in allowlist")
	}
	if err != nil {
		logger.Debugf(p.ctx, "emuNetProxy: blocked TLS connection to SNI %q (err=%v)", sniHost, err)
		return
	}

	upstream, err := p.dialer(p.ctx, sniHost, targetPort)
	if err != nil {
		logger.Debugf(p.ctx, "emuNetProxy: failed to dial TLS upstream %s:%d: %v", sniHost, targetPort, err)
		return
	}
	defer upstream.Close()
	if !p.trackConn(upstream) {
		return
	}
	defer p.untrackConn(upstream)

	if _, err := upstream.Write(clientHello); err != nil {
		return
	}

	proxyStreams(clientConn, upstream)
}

// isValidHostName reports whether host is a syntactically valid DNS host name
// (case-insensitive) with at least two labels. It rejects IP addresses,
// wildcards, ports, and any other characters that should never appear in a
// TLS SNI or HTTP Host name that the proxy dials.
func isValidHostName(host string) bool {
	return validateHostName(strings.ToLower(strings.TrimSuffix(host, "."))) == nil
}

// validateHostName checks that host is a lowercase DNS host name with at least
// two labels. IP addresses are rejected.
func validateHostName(host string) error {
	if host == "" || len(host) > 253 {
		return fmt.Errorf("invalid host name length")
	}
	if net.ParseIP(host) != nil {
		return fmt.Errorf("IP addresses are not allowed")
	}
	labels := strings.Split(host, ".")
	if len(labels) < 2 {
		return fmt.Errorf("host name must have at least two labels")
	}
	for _, label := range labels {
		if label == "" || len(label) > 63 {
			return fmt.Errorf("invalid label %q", label)
		}
		if label[0] == '-' || label[len(label)-1] == '-' {
			return fmt.Errorf("label %q must not start or end with '-'", label)
		}
		for _, c := range label {
			if !(c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '-') {
				return fmt.Errorf("label %q contains invalid character %q", label, c)
			}
		}
	}
	return nil
}

// maxClientHelloBytes bounds how many bytes of a guest's TLS handshake the
// proxy buffers while looking for the SNI. It is larger than the largest
// handshake message that crypto/tls accepts (64 KiB) plus record headers.
const maxClientHelloBytes = 80 << 10

var (
	// errClientHelloRead stops the crypto/tls handshake in
	// readTLSClientHelloSNI once the ClientHello has been parsed.
	errClientHelloRead = errors.New("ClientHello read")
	// errClientHelloIncomplete is returned by readTLSClientHelloSNI if reading
	// from the guest failed (for example, because the guest closed the
	// connection or the read deadline expired) before a complete ClientHello
	// was read.
	errClientHelloIncomplete = errors.New("incomplete ClientHello")
)

// readTLSClientHelloSNI reads a TLS ClientHello from r using crypto/tls and
// returns its SNI host name and the raw bytes that were read from r, which
// must be forwarded to the upstream server before any other data. The
// ClientHello may span multiple TLS records. Nothing is ever written to the
// guest.
func readTLSClientHelloSNI(r io.Reader) (string, []byte, error) {
	var raw bytes.Buffer
	conn := &clientHelloConn{r: io.TeeReader(io.LimitReader(r, maxClientHelloBytes), &raw)}
	var serverName string
	var parsed bool
	err := tls.Server(conn, &tls.Config{
		GetConfigForClient: func(hello *tls.ClientHelloInfo) (*tls.Config, error) {
			serverName = hello.ServerName
			parsed = true
			return nil, errClientHelloRead
		},
	}).Handshake()
	if !parsed {
		if conn.readErr != nil {
			return "", raw.Bytes(), fmt.Errorf("%w: %w", errClientHelloIncomplete, conn.readErr)
		}
		return "", raw.Bytes(), fmt.Errorf("not a TLS ClientHello: %w", err)
	}
	if serverName == "" {
		return "", raw.Bytes(), errors.New("no SNI extension found")
	}
	return serverName, raw.Bytes(), nil
}

// clientHelloConn is a read-only net.Conn for readTLSClientHelloSNI. Writes
// (the TLS alert that crypto/tls sends when the handshake is aborted) are
// discarded.
type clientHelloConn struct {
	r io.Reader
	// readErr is the first error returned by r.
	readErr error
}

func (c *clientHelloConn) Read(b []byte) (int, error) {
	n, err := c.r.Read(b)
	if err != nil && c.readErr == nil {
		c.readErr = err
	}
	return n, err
}

func (c *clientHelloConn) Write(b []byte) (int, error)      { return len(b), nil }
func (c *clientHelloConn) Close() error                     { return nil }
func (c *clientHelloConn) LocalAddr() net.Addr              { return nil }
func (c *clientHelloConn) RemoteAddr() net.Addr             { return nil }
func (c *clientHelloConn) SetDeadline(time.Time) error      { return nil }
func (c *clientHelloConn) SetReadDeadline(time.Time) error  { return nil }
func (c *clientHelloConn) SetWriteDeadline(time.Time) error { return nil }

func (p *emuNetProxy) serveHTTP() {
	defer p.wg.Done()
	for {
		conn, err := p.http80Ln.Accept()
		if err != nil {
			return
		}
		if !p.trackConn(conn) {
			return
		}
		p.wg.Add(1)
		go func(c net.Conn) {
			defer p.wg.Done()
			defer p.untrackConn(c)
			p.handleHTTPConn(c)
		}(conn)
	}
}

func (p *emuNetProxy) handleHTTPConn(clientConn net.Conn) {
	defer clientConn.Close()
	_ = clientConn.SetReadDeadline(time.Now().Add(p.handshakeTimeout))
	br := bufio.NewReader(clientConn)
	req, err := http.ReadRequest(br)
	if err != nil {
		return
	}
	_ = clientConn.SetReadDeadline(time.Time{})

	host := req.Host
	if h, _, err := net.SplitHostPort(host); err == nil {
		host = h
	}
	if req.Method == http.MethodConnect {
		logger.Debugf(p.ctx, "emuNetProxy: blocked HTTP CONNECT to %q", req.Host)
		_, _ = io.WriteString(clientConn, "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
		return
	}
	if !isValidHostName(host) || !p.isDomainAllowed(host) {
		logger.Debugf(p.ctx, "emuNetProxy: blocked HTTP request to Host %q", req.Host)
		_, _ = io.WriteString(clientConn, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
		return
	}

	upstream, err := p.dialer(p.ctx, host, 80)
	if err != nil {
		logger.Debugf(p.ctx, "emuNetProxy: failed to dial HTTP upstream %s:80: %v", host, err)
		_, _ = io.WriteString(clientConn, "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
		return
	}
	defer upstream.Close()
	if !p.trackConn(upstream) {
		return
	}
	defer p.untrackConn(upstream)

	// Forward only this request, and ask the upstream server to close the
	// connection after its response. Any further requests that the guest
	// sends on the same connection (keep-alive or pipelined) are never
	// forwarded, because they haven't been checked against the allowlist.
	req.Close = true
	req.Header.Del("Connection")
	req.Header.Del("Keep-Alive")
	req.Header.Del("Proxy-Connection")
	if err := req.Write(upstream); err != nil {
		return
	}
	_, _ = io.Copy(clientConn, upstream)
}

// proxyStreams copies data in both directions between a and b until both
// directions are done.
func proxyStreams(a, b net.Conn) {
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		_, _ = io.Copy(b, a)
		if tc, ok := b.(*net.TCPConn); ok {
			_ = tc.CloseWrite()
		}
	}()
	go func() {
		defer wg.Done()
		_, _ = io.Copy(a, b)
		if tc, ok := a.(*net.TCPConn); ok {
			_ = tc.CloseWrite()
		}
	}()
	wg.Wait()
}

func (p *emuNetProxy) serveNetdevSocket() {
	defer p.wg.Done()
	for {
		conn, err := p.netdevLn.Accept()
		if err != nil {
			return
		}
		if !p.trackConn(conn) {
			return
		}
		p.wg.Add(1)
		go func(c net.Conn) {
			defer p.wg.Done()
			defer p.untrackConn(c)
			p.handleNetdevSocketConn(c)
		}(conn)
	}
}

func (p *emuNetProxy) handleNetdevSocketConn(conn net.Conn) {
	defer conn.Close()
	var writeMu sync.Mutex
	writeFrame := func(frame []byte) error {
		writeMu.Lock()
		defer writeMu.Unlock()
		var buf [4]byte
		binary.BigEndian.PutUint32(buf[:], uint32(len(frame)))
		if _, err := conn.Write(buf[:]); err != nil {
			return err
		}
		_, err := conn.Write(frame)
		return err
	}

	// Pre-allocate all 16 BOOTP slots in libslirp's user0 instance with placeholder
	// MACs (02:00:00:00:00:00..0f) so user0 never sends a competing DHCPOFFER
	// that omits the default gateway when restrict=on.
	for i := 0; i < slirpBootpSlots; i++ {
		placeholderMAC := [6]byte{0x02, 0x00, 0x00, 0x00, 0x00, byte(i)}
		frame := buildDHCPDiscoverFrame(placeholderMAC, uint32(i+1))
		if err := writeFrame(frame); err != nil {
			return
		}
	}

	var lenBuf [4]byte
	for {
		if _, err := io.ReadFull(conn, lenBuf[:]); err != nil {
			return
		}
		pktLen := binary.BigEndian.Uint32(lenBuf[:])
		if pktLen == 0 || pktLen > 65536 {
			return
		}
		pkt := make([]byte, pktLen)
		if _, err := io.ReadFull(conn, pkt); err != nil {
			return
		}
		if reply := p.handleEthernetFrame(pkt); reply != nil {
			if err := writeFrame(reply); err != nil {
				return
			}
		}
	}
}

func (p *emuNetProxy) handleEthernetFrame(frame []byte) []byte {
	// Ethernet header is 14 bytes: [6B dst][6B src][2B ethertype].
	if len(frame) < 14+20 {
		return nil
	}
	etherType := binary.BigEndian.Uint16(frame[12:14])
	if etherType != 0x0800 { // IPv4
		return nil
	}
	var dstMAC, srcMAC [6]byte
	copy(dstMAC[:], frame[0:6])
	copy(srcMAC[:], frame[6:12])

	ipPkt := frame[14:]
	if ipPkt[0]>>4 != 4 {
		return nil
	}
	ihl := int(ipPkt[0]&0x0f) * 4
	if ihl < 20 || len(ipPkt) < ihl+8 {
		return nil
	}
	totalLen := int(binary.BigEndian.Uint16(ipPkt[2:4]))
	if totalLen < ihl+8 || totalLen > len(ipPkt) {
		return nil
	}
	ipPkt = ipPkt[:totalLen]
	if ipPkt[9] != 17 { // UDP
		return nil
	}

	var srcIP, dstIP [4]byte
	copy(srcIP[:], ipPkt[12:16])
	copy(dstIP[:], ipPkt[16:20])

	udpPkt := ipPkt[ihl:]
	srcPort := binary.BigEndian.Uint16(udpPkt[0:2])
	dstPort := binary.BigEndian.Uint16(udpPkt[2:4])
	udpLen := int(binary.BigEndian.Uint16(udpPkt[4:6]))
	if udpLen < 8 || udpLen > len(udpPkt) {
		return nil
	}
	udpPayload := udpPkt[8:udpLen]

	if dstPort == 67 && srcPort == 68 {
		return handleDHCPPacket(udpPayload)
	}
	if dstPort == 53 {
		dnsResp := buildDNSResponse(udpPayload, p.isDomainAllowed)
		if dnsResp == nil {
			return nil
		}
		return buildUDPEthernetFrame(srcMAC, dstMAC, dstIP, srcIP, 53, srcPort, dnsResp)
	}
	return nil
}

// dhcpMinMessageLen is the length of a BOOTP/DHCP message with a full 312-byte
// options field (RFC 2131), so that the IP packet is the 576-byte minimum
// that libslirp's bootp_input() requires; libslirp drops shorter packets.
const dhcpMinMessageLen = 236 + 312

func buildDHCPDiscoverFrame(mac [6]byte, xid uint32) []byte {
	var bootp [dhcpMinMessageLen]byte
	bootp[0] = 1 // BOOTREQUEST
	bootp[1] = 1 // Ethernet
	bootp[2] = 6 // 6-byte MAC
	binary.BigEndian.PutUint32(bootp[4:8], xid)
	copy(bootp[28:34], mac[:])
	// Magic cookie 99.130.83.99
	copy(bootp[236:240], []byte{0x63, 0x82, 0x53, 0x63})
	// Option 53 (DHCP Message Type) = 1 (DHCPDISCOVER), Option 255 (End)
	copy(bootp[240:244], []byte{53, 1, 1, 255})
	return buildUDPEthernetFrame(broadcastMAC, mac, zeroIP, broadcastIP, 68, 67, bootp[:])
}

func handleDHCPPacket(bootp []byte) []byte {
	// BOOTP fixed header is 236 bytes + 4-byte magic cookie.
	if len(bootp) < 240 || bootp[0] != 1 || bootp[1] != 1 || bootp[2] != 6 {
		return nil
	}
	// Ignore our own placeholder MACs (02:00:00:00:00:xx).
	if bootp[28] == 0x02 && bootp[29] == 0x00 && bootp[30] == 0x00 && bootp[31] == 0x00 && bootp[32] == 0x00 {
		return nil
	}
	if !bytes.Equal(bootp[236:240], []byte{0x63, 0x82, 0x53, 0x63}) {
		return nil
	}

	var msgType byte
	opts := bootp[240:]
	for len(opts) > 0 {
		code := opts[0]
		if code == 0 {
			opts = opts[1:]
			continue
		}
		if code == 255 {
			break
		}
		if len(opts) < 2 {
			break
		}
		optLen := int(opts[1])
		opts = opts[2:]
		if len(opts) < optLen {
			break
		}
		if code == 53 && optLen >= 1 {
			msgType = opts[0]
		}
		opts = opts[optLen:]
	}

	var replyMsgType byte
	switch msgType {
	case 1: // DHCPDISCOVER -> DHCPOFFER (2)
		replyMsgType = 2
	case 3: // DHCPREQUEST -> DHCPACK (5)
		replyMsgType = 5
	default:
		return nil
	}

	var clientMAC [6]byte
	copy(clientMAC[:], bootp[28:34])

	var resp [300]byte
	resp[0] = 2                     // BOOTREPLY
	resp[1] = 1                     // Ethernet
	resp[2] = 6                     // 6-byte MAC
	copy(resp[4:8], bootp[4:8])     // xid
	copy(resp[10:12], bootp[10:12]) // flags
	copy(resp[16:20], slirpGuestIP[:])
	copy(resp[20:24], slirpGatewayIP[:])
	copy(resp[28:34], clientMAC[:])
	copy(resp[236:240], []byte{0x63, 0x82, 0x53, 0x63})

	q := resp[240:240]
	// Option 53: DHCP Message Type
	q = append(q, 53, 1, replyMsgType)
	// Option 54: Server Identifier (10.0.2.2)
	q = append(q, 54, 4, slirpGatewayIP[0], slirpGatewayIP[1], slirpGatewayIP[2], slirpGatewayIP[3])
	// Option 1: Subnet Mask (255.255.255.0)
	q = append(q, 1, 4, slirpSubnetMask[0], slirpSubnetMask[1], slirpSubnetMask[2], slirpSubnetMask[3])
	// Option 3: Router / Default Gateway (10.0.2.2)
	q = append(q, 3, 4, slirpGatewayIP[0], slirpGatewayIP[1], slirpGatewayIP[2], slirpGatewayIP[3])
	// Option 6: DNS Server (10.0.2.3)
	q = append(q, 6, 4, slirpDNSIP[0], slirpDNSIP[1], slirpDNSIP[2], slirpDNSIP[3])
	// Option 51: IP Address Lease Time
	var leaseBytes [4]byte
	binary.BigEndian.PutUint32(leaseBytes[:], dhcpLeaseSecs)
	q = append(q, 51, 4, leaseBytes[0], leaseBytes[1], leaseBytes[2], leaseBytes[3])
	// Option 255: End
	q = append(q, 255)

	totalBootpLen := 240 + len(q)
	if totalBootpLen < 300 {
		totalBootpLen = 300
	}
	return buildUDPEthernetFrame(clientMAC, slirpGatewayMAC, slirpGatewayIP, broadcastIP, 67, 68, resp[:totalBootpLen])
}

// buildDNSResponse answers the first question in the DNS query: A queries for
// names that isAllowed accepts resolve to slirpProxyIP, other query types for
// allowed names get an empty NOERROR answer, and all other names get NXDOMAIN.
// It returns nil for messages that should be dropped (malformed queries,
// responses, non-QUERY opcodes, and queries without questions).
func buildDNSResponse(query []byte, isAllowed func(string) bool) []byte {
	var parser dnsmessage.Parser
	hdr, err := parser.Start(query)
	if err != nil || hdr.Response || hdr.OpCode != 0 {
		return nil
	}
	question, err := parser.Question()
	if err != nil {
		return nil
	}
	name := strings.TrimSuffix(question.Name.String(), ".")

	respHdr := dnsmessage.Header{
		ID:                 hdr.ID,
		Response:           true,
		RecursionDesired:   hdr.RecursionDesired,
		RecursionAvailable: true,
	}
	allowed := isAllowed(name)
	if !allowed {
		respHdr.RCode = dnsmessage.RCodeNameError
	}
	builder := dnsmessage.NewBuilder(make([]byte, 0, 512), respHdr)
	builder.EnableCompression()
	if err := builder.StartQuestions(); err != nil {
		return nil
	}
	if err := builder.Question(question); err != nil {
		return nil
	}
	if allowed && question.Type == dnsmessage.TypeA && question.Class == dnsmessage.ClassINET {
		if err := builder.StartAnswers(); err != nil {
			return nil
		}
		if err := builder.AResource(
			dnsmessage.ResourceHeader{Name: question.Name, Class: dnsmessage.ClassINET, TTL: 60},
			dnsmessage.AResource{A: slirpProxyIP},
		); err != nil {
			return nil
		}
	}
	resp, err := builder.Finish()
	if err != nil {
		return nil
	}
	return resp
}

func buildUDPEthernetFrame(dstMAC, srcMAC [6]byte, srcIP, dstIP [4]byte, srcPort, dstPort uint16, payload []byte) []byte {
	udpLen := 8 + len(payload)
	ipLen := 20 + udpLen
	frame := make([]byte, 14+ipLen)

	// Ethernet header
	copy(frame[0:6], dstMAC[:])
	copy(frame[6:12], srcMAC[:])
	binary.BigEndian.PutUint16(frame[12:14], 0x0800)

	// IPv4 header
	ipHdr := frame[14:34]
	ipHdr[0] = 0x45 // Version 4, IHL 5
	binary.BigEndian.PutUint16(ipHdr[2:4], uint16(ipLen))
	ipHdr[8] = 64 // TTL
	ipHdr[9] = 17 // UDP
	copy(ipHdr[12:16], srcIP[:])
	copy(ipHdr[16:20], dstIP[:])
	binary.BigEndian.PutUint16(ipHdr[10:12], ipv4Checksum(ipHdr))

	// UDP header + payload
	udpHdr := frame[34:]
	binary.BigEndian.PutUint16(udpHdr[0:2], srcPort)
	binary.BigEndian.PutUint16(udpHdr[2:4], dstPort)
	binary.BigEndian.PutUint16(udpHdr[4:6], uint16(udpLen))
	copy(udpHdr[8:], payload)
	binary.BigEndian.PutUint16(udpHdr[6:8], udpChecksum(srcIP, dstIP, udpHdr))

	return frame
}

func ipv4Checksum(hdr []byte) uint16 {
	var sum uint32
	for i := 0; i+1 < len(hdr); i += 2 {
		sum += uint32(binary.BigEndian.Uint16(hdr[i : i+2]))
	}
	for (sum >> 16) > 0 {
		sum = (sum & 0xffff) + (sum >> 16)
	}
	return ^uint16(sum)
}

func udpChecksum(srcIP, dstIP [4]byte, udpPkt []byte) uint16 {
	var sum uint32
	sum += uint32(binary.BigEndian.Uint16(srcIP[0:2]))
	sum += uint32(binary.BigEndian.Uint16(srcIP[2:4]))
	sum += uint32(binary.BigEndian.Uint16(dstIP[0:2]))
	sum += uint32(binary.BigEndian.Uint16(dstIP[2:4]))
	sum += 17 // IPPROTO_UDP
	sum += uint32(len(udpPkt))

	for i := 0; i+1 < len(udpPkt); i += 2 {
		sum += uint32(binary.BigEndian.Uint16(udpPkt[i : i+2]))
	}
	if len(udpPkt)%2 == 1 {
		sum += uint32(udpPkt[len(udpPkt)-1]) << 8
	}
	for (sum >> 16) > 0 {
		sum = (sum & 0xffff) + (sum >> 16)
	}
	csum := ^uint16(sum)
	if csum == 0 {
		return 0xffff
	}
	return csum
}
