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
	"strings"
	"sync"
	"testing"
	"time"

	"golang.org/x/net/dns/dnsmessage"
)

// testAllowedDomains is the domain allowlist used by the tests.
var testAllowedDomains = []string{
	"google.com",
	"googleapis.com",
	"gstatic.com",
	"googleusercontent.com",
	"gvt1.com",
	"gvt2.com",
	"android.com",
}

func TestDomainAllowlist(t *testing.T) {
	allowed := []string{
		"google.com",
		"accounts.google.com",
		"mail.google.com",
		"calendar.google.com",
		"signaler-pa.clients6.google.com.",
		"oauth2.googleapis.com",
		"gmail.googleapis.com",
		"connectivitycheck.gstatic.com",
		"lh3.googleusercontent.com",
		"edgedl.me.gvt1.com",
		"redirector.gvt2.com",
		"connectivitycheck.android.com",
		"ACCOUNTS.GOOGLE.COM:443",
	}
	for _, host := range allowed {
		if !isDomainAllowedInList(host, testAllowedDomains) {
			t.Errorf("expected domain %q to be allowed, but it was blocked", host)
		}
	}

	blocked := []string{
		"",
		"example.com",
		"evilgoogle.com",
		"google.com.evil.com",
		"localhost",
		"127.0.0.1",
		"10.0.2.2",
		"169.254.169.254",
		"[::1]",
	}
	for _, host := range blocked {
		if isDomainAllowedInList(host, testAllowedDomains) {
			t.Errorf("expected domain %q to be blocked, but it was allowed", host)
		}
	}
}

func TestEmptyDomainAllowlistBlocksEverything(t *testing.T) {
	for _, host := range []string{"google.com", "accounts.google.com", "example.com"} {
		if isDomainAllowedInList(host, nil) {
			t.Errorf("expected domain %q to be blocked by an empty allowlist", host)
		}
	}
}

func TestIsPublicUnicastIP(t *testing.T) {
	publicIPs := []string{
		"8.8.8.8",
		"8.8.4.4",
		"142.250.190.46",
		"2607:f8b0:4005:809::200e",
	}
	for _, s := range publicIPs {
		if !isPublicUnicastIP(net.ParseIP(s)) {
			t.Errorf("expected IP %s to be recognized as public unicast", s)
		}
	}

	nonPublicIPs := []string{
		"0.0.0.0",
		"127.0.0.1",
		"10.0.2.2",
		"10.0.2.15",
		"172.16.0.1",
		"192.168.1.1",
		"169.254.169.254",
		"100.64.0.1",
		"198.18.0.1",
		"192.0.0.1",
		"192.0.2.1",
		"198.51.100.1",
		"203.0.113.1",
		"240.0.0.1",
		"255.255.255.255",
		"224.0.0.251",
		"::",
		"::1",
		"::ffff:127.0.0.1",
		"::ffff:10.0.2.2",
		"fe80::1",
		"fc00::1",
		"fec0::1",
		"64:ff9b::7f00:1",
		"2001:db8::1",
		"2001::7f00:1",
		"2002:7f00:1::1",
	}
	for _, s := range nonPublicIPs {
		if isPublicUnicastIP(net.ParseIP(s)) {
			t.Errorf("expected IP %s to be rejected as non-public", s)
		}
	}
}

func TestReadTLSClientHelloSNI(t *testing.T) {
	// A ClientHello from crypto/tls, with the extensions that real clients
	// send.
	clientConn, serverConn := net.Pipe()
	go func() {
		_ = tls.Client(clientConn, &tls.Config{ServerName: "accounts.google.com"}).Handshake()
	}()
	_ = serverConn.SetReadDeadline(time.Now().Add(10 * time.Second))
	host, raw, err := readTLSClientHelloSNI(serverConn)
	_ = serverConn.Close()
	if err != nil || host != "accounts.google.com" {
		t.Errorf("crypto/tls ClientHello: got (%q, %v), want accounts.google.com", host, err)
	}
	if len(raw) == 0 || raw[0] != 0x16 {
		t.Errorf("crypto/tls ClientHello: expected the raw handshake record to be returned, got %x", raw)
	}

	// A ClientHello that is split across two TLS records.
	record := buildTestTLSClientHello("oauth2.googleapis.com")
	hs := record[5:]
	var fragmented []byte
	for _, part := range [][]byte{hs[:10], hs[10:]} {
		fragmented = append(fragmented, 0x16, 0x03, 0x01, byte(len(part)>>8), byte(len(part)))
		fragmented = append(fragmented, part...)
	}
	host, raw, err = readTLSClientHelloSNI(bytes.NewReader(fragmented))
	if err != nil || host != "oauth2.googleapis.com" {
		t.Errorf("fragmented ClientHello: got (%q, %v), want oauth2.googleapis.com", host, err)
	}
	if !bytes.Equal(raw, fragmented) {
		t.Errorf("fragmented ClientHello: expected all records to be returned for replay")
	}

	// Data that the guest sends after the ClientHello is returned too, so it
	// is replayed upstream rather than lost.
	trailing := append(append([]byte{}, record...), "early"...)
	if _, raw, err := readTLSClientHelloSNI(bytes.NewReader(trailing)); err != nil || !bytes.HasPrefix(trailing, raw) || len(raw) < len(record) {
		t.Errorf("ClientHello with trailing data: got (%x, %v)", raw, err)
	}

	// A connection that closes before a complete ClientHello is incomplete,
	// not a parse failure.
	if _, _, err := readTLSClientHelloSNI(bytes.NewReader(record[:len(record)-1])); !errors.Is(err, errClientHelloIncomplete) {
		t.Errorf("truncated ClientHello: got %v, want errClientHelloIncomplete", err)
	}
	if _, _, err := readTLSClientHelloSNI(bytes.NewReader([]byte("GET / HTTP/1.1\r\n\r\n"))); err == nil || errors.Is(err, errClientHelloIncomplete) {
		t.Errorf("HTTP request: got %v, want a parse error", err)
	}
}

func TestBuildDNSResponseIsWellFormed(t *testing.T) {
	// The response to an allowed A query parses as a DNS message with the
	// query's ID and RD bit and a single A answer that points at the proxy.
	query := buildTestDNSQuery(0x4242, "accounts.google.com", 1)
	resp := buildDNSResponse(query, func(name string) bool { return name == "accounts.google.com" })
	var parser dnsmessage.Parser
	hdr, err := parser.Start(resp)
	if err != nil {
		t.Fatalf("failed to parse DNS response: %v", err)
	}
	if hdr.ID != 0x4242 || !hdr.Response || !hdr.RecursionDesired || hdr.RCode != dnsmessage.RCodeSuccess {
		t.Errorf("unexpected DNS response header %+v", hdr)
	}
	if err := parser.SkipAllQuestions(); err != nil {
		t.Fatal(err)
	}
	answer, err := parser.AnswerHeader()
	if err != nil {
		t.Fatal(err)
	}
	a, err := parser.AResource()
	if err != nil || answer.Name.String() != "accounts.google.com." || a.A != slirpProxyIP {
		t.Errorf("unexpected DNS answer %v %v (%v)", answer, a, err)
	}

	// Queries that aren't standard QUERYs are dropped.
	inverse := append([]byte{}, query...)
	inverse[2] |= 1 << 3 // OPCODE = 1 (IQUERY)
	if resp := buildDNSResponse(inverse, func(string) bool { return true }); resp != nil {
		t.Errorf("expected IQUERY to be dropped, got %x", resp)
	}
}

func TestEmuNetProxyDHCPAndDNS(t *testing.T) {
	startCtx, cancelStart := context.WithCancel(context.Background())
	proxy, err := newEmuNetProxy(startCtx, testAllowedDomains, "/bin/botanist")
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}
	defer proxy.Close()
	// Simulate StartTargets' errgroup context completing once Start() returns;
	// the proxy must remain active until proxy.Close() is called.
	cancelStart()

	conn, err := net.DialTimeout("tcp", proxy.netdevLn.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to connect to netdev socket: %v", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))

	readFrame := func() []byte {
		t.Helper()
		var lenBuf [4]byte
		if _, err := io.ReadFull(conn, lenBuf[:]); err != nil {
			t.Fatalf("failed to read frame length: %v", err)
		}
		n := binary.BigEndian.Uint32(lenBuf[:])
		buf := make([]byte, n)
		if _, err := io.ReadFull(conn, buf); err != nil {
			t.Fatalf("failed to read frame payload: %v", err)
		}
		return buf
	}
	writeFrame := func(frame []byte) {
		t.Helper()
		var lenBuf [4]byte
		binary.BigEndian.PutUint32(lenBuf[:], uint32(len(frame)))
		if _, err := conn.Write(lenBuf[:]); err != nil {
			t.Fatalf("failed to write frame length: %v", err)
		}
		if _, err := conn.Write(frame); err != nil {
			t.Fatalf("failed to write frame payload: %v", err)
		}
	}

	// 1. Read the 16 placeholder DHCPDISCOVER frames sent at startup.
	for i := 0; i < slirpBootpSlots; i++ {
		frame := readFrame()
		if frame[6] != 0x02 || frame[11] != byte(i) {
			t.Fatalf("unexpected placeholder MAC in frame %d: %x", i, frame[6:12])
		}
	}

	// 2. Send a guest DHCPDISCOVER and verify DHCPOFFER includes Router (Option 3) = 10.0.2.2.
	guestMAC := [6]byte{0x52, 0x54, 0x00, 0x12, 0x34, 0x56}
	writeFrame(buildDHCPDiscoverFrame(guestMAC, 0xdeadbeef))
	offerFrame := readFrame()
	if ipv4Checksum(offerFrame[14:34]) != 0 {
		t.Errorf("invalid IPv4 header checksum on DHCPOFFER")
	}
	bootpOffer := offerFrame[42:]
	if !bytes.Equal(bootpOffer[16:20], slirpGuestIP[:]) {
		t.Errorf("expected yiaddr %v, got %v", slirpGuestIP, bootpOffer[16:20])
	}
	if !bytes.Contains(bootpOffer[240:], []byte{3, 4, 10, 0, 2, 2}) {
		t.Errorf("expected DHCPOFFER to include Option 3 (Router = 10.0.2.2)")
	}

	// 3. Send a DNS A query for accounts.google.com to 8.8.8.8:53 -> expect 10.0.2.100.
	dnsServerIP := [4]byte{8, 8, 8, 8}
	allowedDNSQuery := buildTestDNSQuery(0x1234, "accounts.google.com", 1)
	writeFrame(buildUDPEthernetFrame(slirpGatewayMAC, guestMAC, slirpGuestIP, dnsServerIP, 54321, 53, allowedDNSQuery))
	dnsReplyFrame := readFrame()
	dnsReply := dnsReplyFrame[42:]
	if rcode := dnsReply[3] & 0x0f; rcode != 0 {
		t.Fatalf("expected NOERROR (0) for accounts.google.com, got rcode=%d", rcode)
	}
	if anCount := binary.BigEndian.Uint16(dnsReply[6:8]); anCount != 1 {
		t.Fatalf("expected ANCOUNT=1 for accounts.google.com, got %d", anCount)
	}
	if !bytes.Equal(dnsReply[len(dnsReply)-4:], slirpProxyIP[:]) {
		t.Errorf("expected A record %v, got %v", slirpProxyIP, dnsReply[len(dnsReply)-4:])
	}

	// 4. Send a DNS A query for example.com to 8.8.8.8:53 -> expect NXDOMAIN (3).
	blockedDNSQuery := buildTestDNSQuery(0x5678, "example.com", 1)
	writeFrame(buildUDPEthernetFrame(slirpGatewayMAC, guestMAC, slirpGuestIP, dnsServerIP, 54322, 53, blockedDNSQuery))
	nxReplyFrame := readFrame()
	nxReply := nxReplyFrame[42:]
	if rcode := nxReply[3] & 0x0f; rcode != 3 {
		t.Errorf("expected NXDOMAIN (3) for example.com, got rcode=%d", rcode)
	}
}

func TestEmuNetProxyTLSAndHTTP(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	fakeDialer := func(ctx context.Context, host string, port int) (net.Conn, error) {
		if host == "unreachable.google.com" {
			return nil, fmt.Errorf("simulated dial failure")
		}
		c1, c2 := net.Pipe()
		go func() {
			defer c2.Close()
			if port == 443 {
				// Read the replayed ClientHello record and send a marker back.
				buf := make([]byte, 512)
				n, _ := c2.Read(buf)
				if n > 0 {
					_, _ = fmt.Fprintf(c2, "TLS-OK:%s:%d", host, port)
				}
			} else if port == 80 {
				buf := make([]byte, 512)
				n, _ := c2.Read(buf)
				if n > 0 {
					_, _ = io.WriteString(c2, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
				}
			}
		}()
		return c1, nil
	}

	proxy, err := newEmuNetProxyWithConfig(ctx, testAllowedDomains, "/bin/botanist", func(p *emuNetProxy) {
		p.dialer = fakeDialer
	})
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}
	defer proxy.Close()

	// 1. Allowed TLS SNI (oauth2.googleapis.com) -> succeeds.
	tlsConn, err := net.DialTimeout("tcp", proxy.tls443Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial TLS proxy: %v", err)
	}
	_, _ = tlsConn.Write(buildTestTLSClientHello("oauth2.googleapis.com"))
	if tc, ok := tlsConn.(*net.TCPConn); ok {
		_ = tc.CloseWrite()
	}
	tlsResp, err := io.ReadAll(tlsConn)
	_ = tlsConn.Close()
	if err != nil || string(tlsResp) != "TLS-OK:oauth2.googleapis.com:443" {
		t.Errorf("unexpected TLS response for allowed domain: %q (err=%v)", string(tlsResp), err)
	}

	// 2. Disallowed TLS SNI (evil.example.com) -> closed with empty response.
	badTLSConn, err := net.DialTimeout("tcp", proxy.tls443Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial TLS proxy: %v", err)
	}
	_, _ = badTLSConn.Write(buildTestTLSClientHello("evil.example.com"))
	badTLSResp, _ := io.ReadAll(badTLSConn)
	_ = badTLSConn.Close()
	if len(badTLSResp) != 0 {
		t.Errorf("expected empty response for blocked TLS SNI, got %q", string(badTLSResp))
	}

	// 3. Allowed HTTP Host (connectivitycheck.gstatic.com) -> 204 No Content.
	httpConn, err := net.DialTimeout("tcp", proxy.http80Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial HTTP proxy: %v", err)
	}
	_, _ = io.WriteString(httpConn, "GET /generate_204 HTTP/1.1\r\nHost: connectivitycheck.gstatic.com\r\nConnection: close\r\n\r\n")
	httpResp, _ := io.ReadAll(httpConn)
	_ = httpConn.Close()
	if !strings.Contains(string(httpResp), "204 No Content") {
		t.Errorf("expected 204 No Content for allowed HTTP host, got %q", string(httpResp))
	}

	// 4. Disallowed HTTP Host (evil.example.com) -> 403 Forbidden.
	badHTTPConn, err := net.DialTimeout("tcp", proxy.http80Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial HTTP proxy: %v", err)
	}
	_, _ = io.WriteString(badHTTPConn, "GET / HTTP/1.1\r\nHost: evil.example.com\r\nConnection: close\r\n\r\n")
	badHTTPResp, _ := io.ReadAll(badHTTPConn)
	_ = badHTTPConn.Close()
	if !strings.Contains(string(badHTTPResp), "403 Forbidden") {
		t.Errorf("expected 403 Forbidden for blocked HTTP host, got %q", string(badHTTPResp))
	}

	// 5. Allowed TLS SNI whose upstream dial fails -> closed with empty response.
	failTLSConn, err := net.DialTimeout("tcp", proxy.tls443Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial TLS proxy: %v", err)
	}
	_, _ = failTLSConn.Write(buildTestTLSClientHello("unreachable.google.com"))
	failTLSResp, _ := io.ReadAll(failTLSConn)
	_ = failTLSConn.Close()
	if len(failTLSResp) != 0 {
		t.Errorf("expected empty response for failed TLS upstream, got %q", string(failTLSResp))
	}

}

func TestRunTCPForward(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("Listen failed: %v", err)
	}
	defer ln.Close()

	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		_, _ = io.Copy(conn, conn)
	}()

	var out bytes.Buffer
	code := RunTCPForward(ln.Addr().String(), strings.NewReader("hello-proxy"), &out)
	if code != 0 {
		t.Fatalf("expected exit code 0, got %d", code)
	}
	if out.String() != "hello-proxy" {
		t.Errorf("expected echoed string %q, got %q", "hello-proxy", out.String())
	}
}

func TestEmuNetProxyHTTPForwardsOnlyOneRequest(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	upstreamData := make(chan string, 1)
	fakeDialer := func(ctx context.Context, host string, port int) (net.Conn, error) {
		c1, c2 := net.Pipe()
		go func() {
			defer c2.Close()
			// Read the forwarded request. The proxy asks the upstream to
			// close the connection after responding.
			br := bufio.NewReader(c2)
			req, err := http.ReadRequest(br)
			if err != nil {
				upstreamData <- "error: " + err.Error()
				return
			}
			// Collect anything else the proxy forwards for a short while,
			// then respond and close.
			_ = c2.SetReadDeadline(time.Now().Add(500 * time.Millisecond))
			rest, _ := io.ReadAll(br)
			upstreamData <- fmt.Sprintf("%s %s close=%v rest=%q", req.Method, req.Host, req.Close, rest)
			_ = c2.SetWriteDeadline(time.Now().Add(5 * time.Second))
			_, _ = io.WriteString(c2, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
		}()
		return c1, nil
	}
	proxy, err := newEmuNetProxyWithConfig(ctx, testAllowedDomains, "/bin/botanist", func(p *emuNetProxy) {
		p.dialer = fakeDialer
	})
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}
	defer proxy.Close()

	conn, err := net.DialTimeout("tcp", proxy.http80Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to connect to HTTP proxy: %v", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	// An allowed request followed by a pipelined request to a disallowed
	// host on the same connection.
	_, _ = io.WriteString(conn, "GET /generate_204 HTTP/1.1\r\nHost: connectivitycheck.gstatic.com\r\nConnection: keep-alive\r\n\r\n"+
		"GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n")
	resp, _ := io.ReadAll(conn)
	if !strings.HasPrefix(string(resp), "HTTP/1.1 204") || strings.Count(string(resp), "HTTP/1.1") != 1 {
		t.Errorf("expected exactly one response, got %q", resp)
	}
	select {
	case got := <-upstreamData:
		if want := `GET connectivitycheck.gstatic.com close=true rest=""`; got != want {
			t.Errorf("upstream received %s, want %s", got, want)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("upstream never received the request")
	}
}

func TestEmuNetProxyCloseClosesOpenConnections(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// The upstream never closes the connection on its own, like a
	// long-lived FCM connection.
	upstreamClosed := make(chan struct{})
	fakeDialer := func(ctx context.Context, host string, port int) (net.Conn, error) {
		c1, c2 := net.Pipe()
		go func() {
			_, _ = io.Copy(io.Discard, c2)
			close(upstreamClosed)
		}()
		return c1, nil
	}
	proxy, err := newEmuNetProxyWithConfig(ctx, testAllowedDomains, "/bin/botanist", func(p *emuNetProxy) {
		p.dialer = fakeDialer
	})
	if err != nil {
		t.Fatalf("newEmuNetProxy failed: %v", err)
	}

	conn, err := net.DialTimeout("tcp", proxy.tls5228Ln.Addr().String(), 5*time.Second)
	if err != nil {
		t.Fatalf("failed to connect to TLS proxy: %v", err)
	}
	defer conn.Close()
	if _, err := conn.Write(buildTestTLSClientHello("oauth2.googleapis.com")); err != nil {
		t.Fatalf("failed to send ClientHello: %v", err)
	}
	// An HTTP request to a disallowed Host with a port is blocked.
	if resp := sendAndReadHTTP(t, proxy.http80Ln.Addr().String(), "GET / HTTP/1.1\r\nHost: evil.example.com:8080\r\n\r\n"); !strings.HasPrefix(resp, "HTTP/1.1 403") {
		t.Errorf("expected 403 for disallowed Host, got %q", resp)
	}
	// Wait until the proxy has dialed the upstream.
	deadline := time.Now().Add(5 * time.Second)
	for {
		proxy.connsMu.Lock()
		n := len(proxy.conns)
		proxy.connsMu.Unlock()
		if n >= 2 || time.Now().After(deadline) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}

	// Close must not wait for the open connection to end on its own, and is
	// safe to call concurrently.
	done := make(chan struct{})
	go func() {
		var wg sync.WaitGroup
		for i := 0; i < 3; i++ {
			wg.Add(1)
			go func() {
				defer wg.Done()
				_ = proxy.Close()
			}()
		}
		wg.Wait()
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatalf("Close did not return while a proxied connection was open")
	}
	select {
	case <-upstreamClosed:
	case <-time.After(5 * time.Second):
		t.Errorf("expected Close to close the upstream connection")
	}
}

func TestDHCPDiscoverFrameIsLongEnoughForSlirp(t *testing.T) {
	frame := buildDHCPDiscoverFrame([6]byte{0x02, 0, 0, 0, 0, 1}, 1)
	// libslirp's bootp_input() drops IP packets shorter than
	// sizeof(struct bootp_t) (576 bytes).
	if ipLen := len(frame) - 14; ipLen < 576 {
		t.Errorf("DHCPDISCOVER IP packet is %d bytes, want at least 576", ipLen)
	}
}

// sendAndReadHTTP sends request to addr and returns the response, after the
// proxy closes the connection.
func sendAndReadHTTP(t *testing.T, addr, request string) string {
	t.Helper()
	conn, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		t.Fatalf("failed to dial %s: %v", addr, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
	_, _ = io.WriteString(conn, request)
	resp, _ := io.ReadAll(conn)
	return string(resp)
}

func TestRunTCPForwardReturnsWhenUpstreamCloses(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("Listen failed: %v", err)
	}
	defer ln.Close()

	// The upstream sends a response and closes without reading the request,
	// like the proxy does for rejected connections.
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		_, _ = io.WriteString(conn, "HTTP/1.1 403 Forbidden\r\n\r\n")
		_ = conn.Close()
	}()

	// The guest side stays open (never reaches EOF).
	guestR, guestW := io.Pipe()
	defer guestW.Close()
	var out bytes.Buffer
	done := make(chan int)
	go func() { done <- RunTCPForward(ln.Addr().String(), guestR, &out) }()
	select {
	case code := <-done:
		if code != 0 {
			t.Errorf("expected exit code 0, got %d", code)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("RunTCPForward did not return after the upstream closed the connection")
	}
	if !strings.HasPrefix(out.String(), "HTTP/1.1 403") {
		t.Errorf("expected the upstream response to be forwarded, got %q", out.String())
	}
}

func buildTestDNSQuery(id uint16, domain string, qType uint16) []byte {
	var b []byte
	hdr := make([]byte, 12)
	binary.BigEndian.PutUint16(hdr[0:2], id)
	binary.BigEndian.PutUint16(hdr[2:4], 0x0100) // RD=1
	binary.BigEndian.PutUint16(hdr[4:6], 1)      // QDCOUNT=1
	b = append(b, hdr...)
	for _, label := range strings.Split(domain, ".") {
		b = append(b, byte(len(label)))
		b = append(b, label...)
	}
	b = append(b, 0)
	var tail [4]byte
	binary.BigEndian.PutUint16(tail[0:2], qType)
	binary.BigEndian.PutUint16(tail[2:4], 1) // IN
	b = append(b, tail[:]...)
	return b
}

func buildTestTLSClientHello(sni string) []byte {
	// SNI extension (type 0x0000)
	sniBytes := []byte(sni)
	sniEntry := make([]byte, 3+len(sniBytes))
	sniEntry[0] = 0x00 // host_name
	binary.BigEndian.PutUint16(sniEntry[1:3], uint16(len(sniBytes)))
	copy(sniEntry[3:], sniBytes)

	sniList := make([]byte, 2+len(sniEntry))
	binary.BigEndian.PutUint16(sniList[0:2], uint16(len(sniEntry)))
	copy(sniList[2:], sniEntry)

	ext := make([]byte, 4+len(sniList))
	binary.BigEndian.PutUint16(ext[0:2], 0x0000)
	binary.BigEndian.PutUint16(ext[2:4], uint16(len(sniList)))
	copy(ext[4:], sniList)

	extsBlock := make([]byte, 2+len(ext))
	binary.BigEndian.PutUint16(extsBlock[0:2], uint16(len(ext)))
	copy(extsBlock[2:], ext)

	var body []byte
	body = append(body, 0x03, 0x03)             // TLS 1.2
	body = append(body, make([]byte, 32)...)    // Random
	body = append(body, 0x00)                   // Session ID len = 0
	body = append(body, 0x00, 0x02, 0x13, 0x01) // Cipher suites (1 suite)
	body = append(body, 0x01, 0x00)             // Compression methods (null)
	body = append(body, extsBlock...)

	hs := make([]byte, 4+len(body))
	hs[0] = 0x01 // ClientHello
	hs[1] = byte(len(body) >> 16)
	hs[2] = byte(len(body) >> 8)
	hs[3] = byte(len(body))
	copy(hs[4:], body)

	rec := make([]byte, 5+len(hs))
	rec[0] = 0x16 // Handshake
	rec[1] = 0x03
	rec[2] = 0x01
	binary.BigEndian.PutUint16(rec[3:5], uint16(len(hs)))
	copy(rec[5:], hs)
	return rec
}
