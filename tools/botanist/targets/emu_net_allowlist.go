// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	_ "embed"
	"fmt"
	"strings"
)

// emuNetAllowlistData is the allowlist of host names that emulators with
// opt-in internet access (`allow_internet = true`) may reach through emuNetProxy.
//
// The list lives in its own directory so that changes to it require approval
// from the OWNERS of that directory. See the comments in the file for the
// format.
//
//go:embed emu_net_allowlist/allowlist.txt
var emuNetAllowlistData string

// emuNetAllowlist returns the parsed emulator internet allowlist.
func emuNetAllowlist() ([]string, error) {
	return parseEmuNetAllowlist(emuNetAllowlistData)
}

// parseEmuNetAllowlist parses an allowlist file. Each non-empty line that is
// not a comment (starting with '#') must be either an exact host name, such
// as "accounts.google.com", or a wildcard of the form "*.example.com", which
// matches any subdomain of example.com but not example.com itself. Trailing
// comments are not supported. Entries are lowercased.
func parseEmuNetAllowlist(data string) ([]string, error) {
	var entries []string
	seen := make(map[string]bool)
	for i, line := range strings.Split(data, "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		entry := strings.ToLower(line)
		if err := validateEmuNetAllowlistEntry(entry); err != nil {
			return nil, fmt.Errorf("emulator internet allowlist line %d: %q: %w", i+1, line, err)
		}
		if seen[entry] {
			return nil, fmt.Errorf("emulator internet allowlist line %d: duplicate entry %q", i+1, line)
		}
		seen[entry] = true
		entries = append(entries, entry)
	}
	return entries, nil
}

// validateEmuNetAllowlistEntry checks that entry is a lowercase host name or a
// "*." wildcard followed by a host name with at least two labels.
func validateEmuNetAllowlistEntry(entry string) error {
	return validateHostName(strings.TrimPrefix(entry, "*."))
}
