// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

// Adversarial tests for emuNetProxy: they check that invalid or malicious
// guest requests are blocked even when emulator internet access is enabled.

import (
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"testing"
	"time"
)

// newAdversarialTestProxy starts a proxy that allows testAllowedDomains, and
// whose upstream connections are recorded instead of dialed.
func newAdversarialTestProxy(t *testing.T) (*emuNetProxy, *dialRecorder) {
	t.Helper()
	rec := &dialRecorder{}
	proxy, err := newEmuNetProxyWithConfig(context.Background(), testAllowedDomains, "/bin/botanist", "", func(p *emuNetProxy) {
		p.dialer = rec.dial
		p.handshakeTimeout = 200 * time.Millisecond
	})
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}
	t.Cleanup(func() { _ = proxy.Close() })
	return proxy, rec
}

// dialRecorder is a fake upstream dialer that records every dial and returns
// a connection that reads the first write from the proxy and then closes.
type dialRecorder struct {
	mu    sync.Mutex
	dials []string
	// received is what each dialed upstream connection received.
	received []string
}

func (r *dialRecorder) dial(ctx context.Context, host string, port int) (net.Conn, error) {
	r.mu.Lock()
	r.dials = append(r.dials, fmt.Sprintf("%s:%d", host, port))
	r.mu.Unlock()
	c1, c2 := net.Pipe()
	go func() {
		// Record everything the proxy forwards for a short while, then
		// close the connection.
		_ = c2.SetReadDeadline(time.Now().Add(200 * time.Millisecond))
		data, _ := io.ReadAll(c2)
		r.mu.Lock()
		r.received = append(r.received, string(data))
		r.mu.Unlock()
		_ = c2.Close()
	}()
	return c1, nil
}

// lastReceived returns what the most recently closed upstream connection
// received.
func (r *dialRecorder) lastReceived() string {
	r.mu.Lock()
	defer r.mu.Unlock()
	if len(r.received) == 0 {
		return ""
	}
	return r.received[len(r.received)-1]
}

func (r *dialRecorder) get() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]string(nil), r.dials...)
}

// sendAndRead writes data to addr, half-closes, and returns everything the
// proxy sends back before closing the connection.
func sendAndRead(t *testing.T, addr string, data []byte) string {
	t.Helper()
	conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial %s: %v", addr, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
	if len(data) > 0 {
		_, _ = conn.Write(data)
	}
	if tc, ok := conn.(*net.TCPConn); ok {
		_ = tc.CloseWrite()
	}
	resp, _ := io.ReadAll(conn)
	return string(resp)
}

func TestAdversarialDNS(t *testing.T) {
	proxy, _ := newAdversarialTestProxy(t)

	blocked := []string{
		"example.com",
		"google.com",
		"evil.accounts.google.com",
		"accounts.google.com.evil.com",
		"evilaccounts.google.com",
		"gvt1.com",
		"gvt1.com.evil.com",
		"xn--ggle-55da.com",
		"localhost",
		"127.0.0.1",
		"10.0.2.2",
		"metadata.google.internal",
		strings.Repeat("a", 63) + ".example.com",
	}
	for _, name := range blocked {
		resp := buildDNSResponse(buildTestDNSQuery(0x1111, name, 1), proxy.isDNSQueryAllowed)
		if resp == nil {
			t.Errorf("expected an NXDOMAIN response for %q, got none", name)
			continue
		}
		if rcode := resp[3] & 0x0f; rcode != 3 {
			t.Errorf("expected NXDOMAIN (3) for %q, got rcode=%d", name, rcode)
		}
		if anCount := binary.BigEndian.Uint16(resp[6:8]); anCount != 0 {
			t.Errorf("expected no answers for %q, got %d", name, anCount)
		}
	}

	// Allowed names always resolve to the proxy itself, never to a real
	// address, and AAAA queries get no answers so that the guest cannot use
	// IPv6 to bypass the proxy.
	for _, name := range []string{"accounts.google.com", "ACCOUNTS.Google.COM", "r1---sn-a5mekn7s.gvt1.com"} {
		resp := buildDNSResponse(buildTestDNSQuery(0x2222, name, 1), proxy.isDNSQueryAllowed)
		if resp == nil || resp[3]&0x0f != 0 || binary.BigEndian.Uint16(resp[6:8]) != 1 {
			t.Errorf("expected one A answer for %q, got %x", name, resp)
			continue
		}
		if got := resp[len(resp)-4:]; string(got) != string(slirpProxyIP[:]) {
			t.Errorf("expected A record %v for %q, got %v", slirpProxyIP, name, got)
		}
		aaaa := buildDNSResponse(buildTestDNSQuery(0x3333, name, 28), proxy.isDNSQueryAllowed)
		if aaaa == nil || binary.BigEndian.Uint16(aaaa[6:8]) != 0 {
			t.Errorf("expected no AAAA answers for %q, got %x", name, aaaa)
		}
	}

	// Malformed queries get no response at all.
	valid := buildTestDNSQuery(0x4444, "accounts.google.com", 1)
	compressed := append(append([]byte{}, valid[:12]...), 0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01)
	response := append([]byte{}, valid...)
	response[2] |= 0x80 // QR=1
	noQuestions := append([]byte{}, valid...)
	binary.BigEndian.PutUint16(noQuestions[4:6], 0)
	labelOverflow := append(append([]byte{}, valid[:12]...), 0x3f, 'a', 'b')
	for name, query := range map[string][]byte{
		"empty":                {},
		"short header":         valid[:11],
		"truncated question":   valid[:len(valid)-3],
		"compression pointer":  compressed,
		"response packet":      response,
		"no questions":         noQuestions,
		"label past end":       labelOverflow,
		"unterminated name":    append(append([]byte{}, valid[:12]...), 3, 'f', 'o', 'o'),
		"missing type and cls": append(append([]byte{}, valid[:12]...), 3, 'f', 'o', 'o', 0),
	} {
		if resp := buildDNSResponse(query, proxy.isDNSQueryAllowed); resp != nil {
			t.Errorf("%s: expected no DNS response, got %x", name, resp)
		}
	}
}

func TestAdversarialTLS(t *testing.T) {
	proxy, rec := newAdversarialTestProxy(t)
	addr := proxy.tls443Ln.Addr().String()

	validHello := buildTestTLSClientHello("accounts.google.com")
	oversized := append([]byte{}, validHello...)
	binary.BigEndian.PutUint16(oversized[3:5], 16385)
	notHandshake := append([]byte{}, validHello...)
	notHandshake[0] = 0x17 // Application data.
	notClientHello := append([]byte{}, validHello...)
	notClientHello[5] = 0x02 // ServerHello.

	for name, data := range map[string][]byte{
		"disallowed SNI":              buildTestTLSClientHello("evil.example.com"),
		"parent of allowed host":      buildTestTLSClientHello("google.com"),
		"subdomain of allowed host":   buildTestTLSClientHello("evil.accounts.google.com"),
		"allowed host as prefix":      buildTestTLSClientHello("accounts.google.com.evil.com"),
		"bare wildcard domain":        buildTestTLSClientHello("gvt1.com"),
		"wildcard SNI":                buildTestTLSClientHello("*.gvt1.com"),
		"SNI with port":               buildTestTLSClientHello("accounts.google.com:443"),
		"SNI with userinfo":           buildTestTLSClientHello("evil.com@accounts.google.com"),
		"SNI with path":               buildTestTLSClientHello("accounts.google.com/evil"),
		"SNI with NUL":                buildTestTLSClientHello("accounts.google.com\x00.evil.com"),
		"loopback IP SNI":             buildTestTLSClientHello("127.0.0.1"),
		"gateway IP SNI":              buildTestTLSClientHello("10.0.2.2"),
		"metadata IP SNI":             buildTestTLSClientHello("169.254.169.254"),
		"IPv6 loopback SNI":           buildTestTLSClientHello("::1"),
		"decimal IP SNI":              buildTestTLSClientHello("2130706433"),
		"empty SNI":                   buildTestTLSClientHello(""),
		"no data":                     nil,
		"plain HTTP on TLS port":      []byte("GET / HTTP/1.1\r\nHost: accounts.google.com\r\n\r\n"),
		"not a handshake record":      notHandshake,
		"not a ClientHello":           notClientHello,
		"oversized record":            oversized,
		"truncated record":            validHello[:len(validHello)-10],
		"truncated record header":     validHello[:3],
		"zero-length record":          {0x16, 0x03, 0x01, 0x00, 0x00},
		"garbage after record header": append([]byte{0x16, 0x03, 0x01, 0x00, 0x10}, make([]byte, 16)...),
	} {
		if resp := sendAndRead(t, addr, data); resp != "" {
			t.Errorf("%s: expected the proxy to close the connection without a response, got %q", name, resp)
		}
	}
	if dials := rec.get(); len(dials) != 0 {
		t.Errorf("expected no upstream dials for invalid TLS connections, got %v", dials)
	}

	// The 5228 (FCM) proxy applies the same checks.
	if resp := sendAndRead(t, proxy.tls5228Ln.Addr().String(), buildTestTLSClientHello("evil.example.com")); resp != "" {
		t.Errorf("expected blocked SNI on port 5228 to be closed, got %q", resp)
	}
	if dials := rec.get(); len(dials) != 0 {
		t.Errorf("expected no upstream dials for blocked SNI on port 5228, got %v", dials)
	}

	// A valid, allowed ClientHello is dialed to the SNI host on the guest's
	// port, so the guest cannot choose another upstream host or port.
	_ = sendAndRead(t, addr, validHello)
	if dials := rec.get(); len(dials) != 1 || dials[0] != "accounts.google.com:443" {
		t.Errorf("expected exactly one upstream dial to accounts.google.com:443, got %v", dials)
	}
}

func TestAdversarialTLSSlowClient(t *testing.T) {
	proxy, rec := newAdversarialTestProxy(t)
	conn, err := net.DialTimeout("tcp", proxy.tls443Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial TLS proxy: %v", err)
	}
	defer conn.Close()
	// Send only part of the record header and then stall. The proxy must
	// close the connection after its handshake timeout.
	_, _ = conn.Write([]byte{0x16, 0x03})
	_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	start := time.Now()
	n, err := conn.Read(make([]byte, 1))
	if n != 0 || err == nil {
		t.Fatalf("expected the proxy to close a stalled connection, got n=%d err=%v", n, err)
	}
	if ne, ok := err.(net.Error); ok && ne.Timeout() {
		t.Fatalf("proxy did not close a stalled connection within 5s")
	}
	if elapsed := time.Since(start); elapsed > 4*time.Second {
		t.Errorf("proxy took %v to close a stalled connection", elapsed)
	}
	if dials := rec.get(); len(dials) != 0 {
		t.Errorf("expected no upstream dials for a stalled connection, got %v", dials)
	}
}

func TestAdversarialHTTP(t *testing.T) {
	proxy, rec := newAdversarialTestProxy(t)
	addr := proxy.http80Ln.Addr().String()

	for _, tc := range []struct {
		name    string
		request string
		want    string // Expected response prefix; "" means closed without a response.
	}{
		{"disallowed Host", "GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n", "HTTP/1.1 403"},
		{"parent of allowed host", "GET / HTTP/1.1\r\nHost: google.com\r\n\r\n", "HTTP/1.1 403"},
		{"subdomain of allowed host", "GET / HTTP/1.1\r\nHost: evil.accounts.google.com\r\n\r\n", "HTTP/1.1 403"},
		{"userinfo in Host", "GET / HTTP/1.1\r\nHost: accounts.google.com@evil.com\r\n\r\n", "HTTP/1.1 403"},
		{"loopback IP Host", "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", "HTTP/1.1 403"},
		{"gateway IP Host", "GET / HTTP/1.1\r\nHost: 10.0.2.2:8080\r\n\r\n", "HTTP/1.1 403"},
		{"metadata IP Host", "GET / HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n", "HTTP/1.1 403"},
		{"IPv6 loopback Host", "GET / HTTP/1.1\r\nHost: [::1]\r\n\r\n", "HTTP/1.1 403"},
		{"decimal IP Host", "GET / HTTP/1.1\r\nHost: 2130706433\r\n\r\n", "HTTP/1.1 403"},
		{"wildcard Host", "GET / HTTP/1.1\r\nHost: *.gvt1.com\r\n\r\n", "HTTP/1.1 403"},
		{"empty Host", "GET / HTTP/1.1\r\nHost: \r\n\r\n", "HTTP/1.1 403"},
		{"missing Host", "GET / HTTP/1.0\r\n\r\n", "HTTP/1.1 403"},
		{"absolute-form URL to another host", "GET http://evil.example.com/ HTTP/1.1\r\nHost: accounts.google.com\r\n\r\n", "HTTP/1.1 403"},
		{"CONNECT to another host", "CONNECT evil.example.com:443 HTTP/1.1\r\nHost: accounts.google.com\r\n\r\n", "HTTP/1.1 405"},
		{"CONNECT to an allowed host", "CONNECT accounts.google.com:443 HTTP/1.1\r\nHost: accounts.google.com\r\n\r\n", "HTTP/1.1 405"},
		{"duplicate Host headers", "GET / HTTP/1.1\r\nHost: accounts.google.com\r\nHost: evil.example.com\r\n\r\n", ""},
		{"malformed request line", "NOT HTTP\r\n\r\n", ""},
		{"TLS on HTTP port", string(buildTestTLSClientHello("accounts.google.com")), ""},
		{"no data", "", ""},
	} {
		resp := sendAndRead(t, addr, []byte(tc.request))
		if tc.want == "" {
			if resp != "" {
				t.Errorf("%s: expected the proxy to close the connection without a response, got %q", tc.name, resp)
			}
		} else if !strings.HasPrefix(resp, tc.want) {
			t.Errorf("%s: expected response starting with %q, got %q", tc.name, tc.want, resp)
		}
	}
	if dials := rec.get(); len(dials) != 0 {
		t.Errorf("expected no upstream dials for invalid HTTP requests, got %v", dials)
	}

	// An allowed Host with a port is still dialed on port 80, so the guest
	// cannot choose another upstream port.
	_ = sendAndRead(t, addr, []byte("GET / HTTP/1.1\r\nHost: connectivitycheck.gstatic.com:8080\r\nConnection: close\r\n\r\n"))
	if dials := rec.get(); len(dials) != 1 || dials[0] != "connectivitycheck.gstatic.com:80" {
		t.Errorf("expected exactly one upstream dial to connectivitycheck.gstatic.com:80, got %v", dials)
	}

	// Requests to disallowed hosts that are pipelined after an allowed
	// request on the same connection are never forwarded, and the upstream
	// is asked to close the connection after the allowed request.
	_ = sendAndRead(t, addr, []byte("GET / HTTP/1.1\r\nHost: connectivitycheck.gstatic.com\r\nConnection: keep-alive\r\n\r\n"+
		"GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n"+
		"CONNECT evil.example.com:443 HTTP/1.1\r\nHost: evil.example.com\r\n\r\n"))
	if dials := rec.get(); len(dials) != 2 || dials[1] != "connectivitycheck.gstatic.com:80" {
		t.Errorf("expected a second upstream dial to connectivitycheck.gstatic.com:80 only, got %v", dials)
	}
	got := rec.lastReceived()
	if strings.Contains(got, "evil.example.com") || strings.Count(got, "HTTP/1.1\r\n") != 1 || !strings.Contains(got, "Connection: close\r\n") || strings.Contains(got, "keep-alive") {
		t.Errorf("expected only the allowed request, with Connection: close, to be forwarded; upstream received %q", got)
	}
}

func TestAdversarialUpstreamResolution(t *testing.T) {
	proxy, err := newEmuNetProxy(context.Background(), testAllowedDomains, "/bin/botanist", "")
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}
	defer proxy.Close()

	var mu sync.Mutex
	var dialed []string
	proxy.dialContext = func(ctx context.Context, network, address string) (net.Conn, error) {
		mu.Lock()
		dialed = append(dialed, address)
		mu.Unlock()
		c1, c2 := net.Pipe()
		_ = c2.Close()
		return c1, nil
	}
	resolveTo := func(ips ...string) {
		proxy.lookupIPAddr = func(ctx context.Context, host string) ([]net.IPAddr, error) {
			var addrs []net.IPAddr
			for _, ip := range ips {
				addrs = append(addrs, net.IPAddr{IP: net.ParseIP(ip)})
			}
			return addrs, nil
		}
	}

	// An allowed name that resolves only to loopback, private, link-local,
	// or other non-public addresses (for example, through DNS rebinding) is
	// never dialed.
	for _, ip := range []string{
		"127.0.0.1", "10.0.2.2", "192.168.1.1", "172.16.0.1", "169.254.169.254",
		"100.64.0.1", "0.0.0.0", "::1", "fe80::1", "fc00::1", "::",
	} {
		resolveTo(ip)
		if conn, err := proxy.dialAllowedHost(context.Background(), "accounts.google.com", 443); err == nil {
			_ = conn.Close()
			t.Errorf("expected dialing accounts.google.com resolved to %s to fail", ip)
		}
	}
	mu.Lock()
	if len(dialed) != 0 {
		t.Errorf("expected no dials to non-public addresses, got %v", dialed)
	}
	mu.Unlock()

	// When a name resolves to both private and public addresses, only the
	// public address is dialed.
	resolveTo("127.0.0.1", "10.0.0.1", "142.250.190.46")
	conn, err := proxy.dialAllowedHost(context.Background(), "accounts.google.com", 443)
	if err != nil {
		t.Fatalf("expected dialing a public address to succeed: %v", err)
	}
	_ = conn.Close()
	mu.Lock()
	if len(dialed) != 1 || dialed[0] != "142.250.190.46:443" {
		t.Errorf("expected only the public address to be dialed, got %v", dialed)
	}
	mu.Unlock()

	// Names that are not allowed are never resolved.
	proxy.lookupIPAddr = func(ctx context.Context, host string) ([]net.IPAddr, error) {
		t.Errorf("unexpected lookup of %q", host)
		return nil, fmt.Errorf("unexpected lookup")
	}
	for _, host := range []string{"evil.example.com", "google.com", "127.0.0.1", "accounts.google.com:443", "*.gvt1.com"} {
		if conn, err := proxy.dialAllowedHost(context.Background(), host, 443); err == nil {
			_ = conn.Close()
			t.Errorf("expected dialing %q to fail", host)
		}
	}
}

func TestAdversarialEthernetFrames(t *testing.T) {
	proxy, _ := newAdversarialTestProxy(t)
	guestMAC := [6]byte{0x52, 0x54, 0x00, 0x12, 0x34, 0x56}
	dnsServer := [4]byte{8, 8, 8, 8}
	valid := buildUDPEthernetFrame(slirpGatewayMAC, guestMAC, slirpGuestIP, dnsServer, 5353, 53, buildTestDNSQuery(1, "example.com", 1))

	// A DNS query sent directly to a public resolver is answered by the
	// proxy's filter, so the guest cannot bypass it.
	reply := proxy.handleEthernetFrame(valid)
	if reply == nil {
		t.Fatalf("expected a filtered DNS reply for a query to 8.8.8.8")
	}
	if rcode := reply[42+3] & 0x0f; rcode != 3 {
		t.Errorf("expected NXDOMAIN from the proxy for a query to 8.8.8.8, got rcode=%d", rcode)
	}

	ipv6 := append([]byte{}, valid...)
	binary.BigEndian.PutUint16(ipv6[12:14], 0x86dd)
	tcp := append([]byte{}, valid...)
	tcp[14+9] = 6
	badIPLen := append([]byte{}, valid...)
	binary.BigEndian.PutUint16(badIPLen[14+2:14+4], 0xffff)
	badUDPLen := append([]byte{}, valid...)
	binary.BigEndian.PutUint16(badUDPLen[14+20+4:14+20+6], 0xffff)
	otherPort := buildUDPEthernetFrame(slirpGatewayMAC, guestMAC, slirpGuestIP, dnsServer, 5353, 5353, buildTestDNSQuery(1, "example.com", 1))
	for name, frame := range map[string][]byte{
		"empty":                {},
		"truncated":            valid[:20],
		"IPv6 ethertype":       ipv6,
		"TCP protocol":         tcp,
		"IP length too large":  badIPLen,
		"UDP length too large": badUDPLen,
		"non-DNS UDP port":     otherPort,
	} {
		if reply := proxy.handleEthernetFrame(frame); reply != nil {
			t.Errorf("%s: expected no reply, got %x", name, reply)
		}
	}
}
