// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bytes"
	"context"
	"encoding/binary"
	"net"
	"strings"
	"testing"
)

// The fuzz targets below parse untrusted guest input. When run as regular
// tests, only the seed corpus is exercised. Run them with `go test -fuzz` to
// search for crashes.

func FuzzReadTLSClientHelloSNI(f *testing.F) {
	for _, host := range []string{"accounts.google.com", "", "evil.example.com", "127.0.0.1"} {
		f.Add(buildTestTLSClientHello(host))
	}
	f.Add([]byte{})
	f.Add([]byte{0x16, 0x03, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00})
	f.Fuzz(func(t *testing.T, b []byte) {
		host, raw, err := readTLSClientHelloSNI(bytes.NewReader(b))
		// The bytes that are replayed upstream must be exactly what the guest
		// sent.
		if !bytes.HasPrefix(b, raw) {
			t.Errorf("replayed bytes %x are not a prefix of the input %x", raw, b)
		}
		if err != nil {
			return
		}
		// Hosts that the proxy would dial (see handleTLSConn) must be plain
		// DNS host names, never IP addresses or names with ports, wildcards,
		// or other characters.
		if isValidHostName(host) && isDomainAllowedInList(host, testAllowedDomains) {
			if net.ParseIP(host) != nil || strings.ContainsAny(host, ":/@*\\ \x00") {
				t.Errorf("proxy would dial invalid SNI host name %q", host)
			}
		}
	})
}

func FuzzBuildDNSResponse(f *testing.F) {
	f.Add(buildTestDNSQuery(1, "accounts.google.com", 1))
	f.Add(buildTestDNSQuery(2, "example.com", 1))
	f.Add(buildTestDNSQuery(3, "accounts.google.com", 28))
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, query []byte) {
		resp := buildDNSResponse(query, func(name string) bool {
			return isDomainAllowedInList(name, testAllowedDomains)
		})
		if resp == nil {
			return
		}
		if len(resp) < 12 {
			t.Fatalf("DNS response too short: %x", resp)
		}
		if resp[0] != query[0] || resp[1] != query[1] {
			t.Errorf("DNS response ID %x does not match query ID %x", resp[0:2], query[0:2])
		}
		// Any answer must point at the proxy, never at another address.
		if binary.BigEndian.Uint16(resp[6:8]) > 0 {
			if got := resp[len(resp)-4:]; string(got) != string(slirpProxyIP[:]) {
				t.Errorf("DNS answer %v is not the proxy address %v", got, slirpProxyIP)
			}
		}
	})
}

func FuzzHandleEthernetFrame(f *testing.F) {
	proxy := &emuNetProxy{allowedDomains: testAllowedDomains, ctx: context.Background()}
	guestMAC := [6]byte{0x52, 0x54, 0x00, 0x12, 0x34, 0x56}
	f.Add(buildDHCPDiscoverFrame(guestMAC, 1))
	f.Add(buildUDPEthernetFrame(slirpGatewayMAC, guestMAC, slirpGuestIP, slirpDNSIP, 5353, 53, buildTestDNSQuery(1, "accounts.google.com", 1)))
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, frame []byte) {
		_ = proxy.handleEthernetFrame(frame)
	})
}
