// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package expectation

import (
	"fmt"
	"os"
	"strings"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/expectation/outcome"
	"go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/parseoutput"
)

type SuiteIdentifier struct {
	SuiteName string
}

// TODO(https://fxbug.dev/42173824): Test expectations are intended to potentially be moved into a
// config file (perhaps JSON) rather than being embedded in Go in this way.
var expectations map[SuiteIdentifier]map[AnvlCaseNumber]outcome.Outcome = func() map[SuiteIdentifier]map[AnvlCaseNumber]outcome.Outcome {
	m := make(map[SuiteIdentifier]map[AnvlCaseNumber]outcome.Outcome)

	addAllExpectations := func(suite string, expects map[AnvlCaseNumber]outcome.Outcome) {
		m[SuiteIdentifier{
			SuiteName: strings.ToUpper(suite),
		}] = expects
	}

	// keep-sorted start
	addAllExpectations("arp", arpExpectations)
	addAllExpectations("dhcp-client", dhcpClientExpectations)
	addAllExpectations("dhcp-server", dhcpServerExpectations)
	addAllExpectations("dhcpv6-client", dhcpv6ClientExpectations)
	addAllExpectations("dhcpv6-client-pd", dhcpv6ClientPDExpectations)
	addAllExpectations("icmp", icmpExpectations)
	addAllExpectations("icmp-router", icmpRouterExpectations)
	addAllExpectations("icmpv6", icmpv6Expectations)
	addAllExpectations("icmpv6-router", icmpv6RouterExpectations)
	addAllExpectations("igmp", igmpExpectations)
	addAllExpectations("igmpv3", igmpv3Expectations)
	addAllExpectations("ip", ipExpectations)
	addAllExpectations("ip-router", ipRouterExpectations)
	addAllExpectations("ipv6", ipv6Expectations)
	addAllExpectations("ipv6-autoconfig", ipv6AutoconfigExpectations)
	addAllExpectations("ipv6-mld", ipv6MldExpectations)
	addAllExpectations("ipv6-mldv2", ipv6Mldv2Expectations)
	addAllExpectations("ipv6-ndp", ipv6ndpExpectations)
	addAllExpectations("ipv6-pmtu", ipv6PmtuExpectations)
	addAllExpectations("ipv6-router", ipv6RouterExpectations)
	addAllExpectations("tcp-advanced", tcpAdvancedExpectations)
	addAllExpectations("tcp-advanced-v6", tcpAdvancedV6Expectations)
	addAllExpectations("tcp-core", tcpCoreExpectations)
	addAllExpectations("tcp-core-v6", tcpcorev6Expectations)
	addAllExpectations("tcp-highperf", tcpHighperfExpectations)
	addAllExpectations("tcp-highperf-v6", tcpHighperfV6Expectations)
	addAllExpectations("udp", udpExpectations)
	addAllExpectations("udp-v6", udpV6Expectations)
	// keep-sorted end

	return m
}()

type AnvlCaseNumber struct {
	MajorNumber int
	MinorNumber int
}

func (n AnvlCaseNumber) String() string {
	return fmt.Sprintf("%d.%d", n.MajorNumber, n.MinorNumber)
}

// Returns whether a is smaller than b
func (a AnvlCaseNumber) Cmp(b AnvlCaseNumber) bool {
	return a.MajorNumber < b.MajorNumber ||
		(a.MajorNumber == b.MajorNumber && a.MinorNumber < b.MinorNumber)
}

var Pass = outcome.Pass
var Fail = outcome.Fail
var Inconclusive = outcome.Inconclusive
var NoResponse = outcome.NoResponse
var Flaky = outcome.Flaky
var Skip = outcome.Skip
var AnvlSkip = outcome.AnvlSkip

func GetExpectation(
	ident parseoutput.CaseIdentifier,
) (outcome.Outcome, bool) {
	suiteIdent := SuiteIdentifier{
		SuiteName: ident.SuiteName,
	}
	perSuiteExpectations, ok := expectations[suiteIdent]
	if ok {
		expectation, ok := perSuiteExpectations[AnvlCaseNumber{
			MajorNumber: ident.MajorNumber,
			MinorNumber: ident.MinorNumber,
		}]
		if ok {
			return expectation, ok
		}
	}
	if os.Getenv("ANVL_DEFAULT_EXPECTATION_PASS") != "" {
		return outcome.Pass, true
	}
	return 0, false
}

func GetPerSuiteExpectations(
	ident SuiteIdentifier,
) (map[AnvlCaseNumber]outcome.Outcome, bool) {
	expectations, ok := expectations[ident]
	return expectations, ok
}
