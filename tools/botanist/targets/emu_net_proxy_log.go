// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

// emuNetProxyLogName is the name of the file, in the test output directory
// (FUCHSIA_TEST_OUTDIR), that emuNetProxy writes its connection audit log to.
const emuNetProxyLogName = "emu_net_proxy.jsonl"

// Kinds of proxy events.
const (
	proxyEventDNS  = "dns"
	proxyEventTLS  = "tls"
	proxyEventHTTP = "http"
)

// Proxy decisions.
const (
	// The request was allowed and, for TCP, the upstream connection succeeded.
	proxyDecisionAllowed = "allowed"
	// The request was rejected by the allowlist or could not be parsed.
	proxyDecisionBlocked = "blocked"
	// The request passed the allowlist but the upstream dial failed.
	proxyDecisionDialFailed = "dial_failed"
)

// proxyEvent is a single audit record of a guest network request handled by
// emuNetProxy. Only metadata visible to the proxy is recorded: the proxy never
// decrypts TLS, so URLs, headers, and payloads are never logged.
type proxyEvent struct {
	Time time.Time `json:"time"`
	// Kind is one of proxyEventDNS, proxyEventTLS, or proxyEventHTTP.
	Kind string `json:"kind"`
	// Host is the DNS query name, TLS SNI host name, or HTTP Host header.
	Host string `json:"host"`
	// Port is the guest-requested TCP port (unset for DNS).
	Port int `json:"port,omitempty"`
	// Decision is one of proxyDecisionAllowed, proxyDecisionBlocked, or
	// proxyDecisionDialFailed.
	Decision string `json:"decision"`
	// Upstream is the address that the proxy connected to (TCP only).
	Upstream string `json:"upstream,omitempty"`
	// BytesSent and BytesReceived count guest-to-upstream and
	// upstream-to-guest bytes once an allowed TCP connection closes.
	BytesSent     int64 `json:"bytes_sent,omitempty"`
	BytesReceived int64 `json:"bytes_received,omitempty"`
	// DurationMs is the lifetime of an allowed TCP connection.
	DurationMs int64 `json:"duration_ms,omitempty"`
	// Error describes why a request was blocked or failed, if known.
	Error string `json:"error,omitempty"`
}

type proxyEventKey struct {
	kind string
	host string
	port int
}

type proxyEventCounts struct {
	allowed    int
	blocked    int
	dialFailed int
}

// proxyEventLog records proxyEvents as JSON lines and keeps per-destination
// counts so that a summary can be logged when the proxy shuts down.
type proxyEventLog struct {
	mu     sync.Mutex
	file   *os.File
	enc    *json.Encoder
	counts map[proxyEventKey]*proxyEventCounts
	now    func() time.Time
}

// newProxyEventLog returns a proxyEventLog that writes to path. If path is
// empty, events are only counted for the summary.
func newProxyEventLog(path string) (*proxyEventLog, error) {
	l := &proxyEventLog{
		counts: make(map[proxyEventKey]*proxyEventCounts),
		now:    time.Now,
	}
	if path != "" {
		f, err := os.Create(path)
		if err != nil {
			return nil, fmt.Errorf("failed to create emuNetProxy log %q: %w", path, err)
		}
		l.file = f
		l.enc = json.NewEncoder(f)
	}
	return l, nil
}

// record writes ev to the log and updates the summary counts. It fills in
// ev.Time if unset.
func (l *proxyEventLog) record(ev proxyEvent) {
	if l == nil {
		return
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	if ev.Time.IsZero() {
		ev.Time = l.now().UTC()
	}
	key := proxyEventKey{kind: ev.Kind, host: ev.Host, port: ev.Port}
	c, ok := l.counts[key]
	if !ok {
		c = &proxyEventCounts{}
		l.counts[key] = c
	}
	switch ev.Decision {
	case proxyDecisionAllowed:
		c.allowed++
	case proxyDecisionBlocked:
		c.blocked++
	case proxyDecisionDialFailed:
		c.dialFailed++
	}
	if l.enc != nil {
		// Errors are ignored: auditing must never break guest networking.
		_ = l.enc.Encode(ev)
	}
}

// summary returns one line per (kind, host, port), sorted, describing how
// many requests were allowed, blocked, or failed to dial.
func (l *proxyEventLog) summary() []string {
	if l == nil {
		return nil
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	keys := make([]proxyEventKey, 0, len(l.counts))
	for k := range l.counts {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if keys[i].kind != keys[j].kind {
			return keys[i].kind < keys[j].kind
		}
		if keys[i].host != keys[j].host {
			return keys[i].host < keys[j].host
		}
		return keys[i].port < keys[j].port
	})
	lines := make([]string, 0, len(keys))
	for _, k := range keys {
		c := l.counts[k]
		target := k.host
		if k.port != 0 {
			target = fmt.Sprintf("%s:%d", k.host, k.port)
		}
		lines = append(lines, fmt.Sprintf("%s %s allowed=%d blocked=%d dial_failed=%d",
			k.kind, target, c.allowed, c.blocked, c.dialFailed))
	}
	return lines
}

// logSummary writes the summary to the botanist log. It uses Infof rather
// than Debugf so that blocked requests are easy to find when a test fails.
func (l *proxyEventLog) logSummary(ctx context.Context) {
	if l == nil {
		return
	}
	lines := l.summary()
	if len(lines) == 0 {
		logger.Infof(ctx, "emuNetProxy: no guest network requests were made")
		return
	}
	logger.Infof(ctx, "emuNetProxy: summary of guest network requests (%d destinations):", len(lines))
	for _, line := range lines {
		logger.Infof(ctx, "emuNetProxy:   %s", line)
	}
}

// close closes the underlying log file, if any. Events recorded after close
// are only counted.
func (l *proxyEventLog) close() error {
	if l == nil {
		return nil
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	l.enc = nil
	if l.file == nil {
		return nil
	}
	err := l.file.Close()
	l.file = nil
	return err
}
