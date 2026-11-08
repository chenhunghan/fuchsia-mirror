// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Verifies that `MacAddr`, `Bssid` and `Ssid` are wired up to the
//! process-global redactor.
//!
//! This lives in its own test binary because installing a global redactor
//! affects every value formatted in the process for the rest of its lifetime.

use ieee80211::{Bssid, MacAddr, Ssid};

#[test]
fn display_and_debug_follow_global_redactor() {
    let mac = MacAddr::from([0x00, 0x12, 0x48, 0x9a, 0xbc, 0xdf]);
    let bssid = Bssid::from([0x00, 0x12, 0x48, 0x9a, 0xbc, 0xdf]);
    let ssid = Ssid::try_from("foo").unwrap();

    // Before any redactor is installed the raw values are printed.
    assert_eq!(format!("{mac}"), "00:12:48:9a:bc:df");
    assert_eq!(format!("{mac:?}"), "MacAddr(00:12:48:9a:bc:df)");
    assert_eq!(format!("{bssid}"), "00:12:48:9a:bc:df");
    assert_eq!(format!("{bssid:?}"), "Bssid(00:12:48:9a:bc:df)");
    assert_eq!(format!("{ssid}"), "<ssid-666f6f>");
    assert_eq!(format!("{ssid:?}"), "Ssid(<ssid-666f6f>)");

    // After redaction is enabled, `{}` and `{:?}` are both redacted, rendered
    // in the same shape plus the shared `*` marker.
    //
    // Note: this test installs the masking redactor (used synchronously at
    // component startup before the per-boot salt arrives), which preserves the
    // 3-octet OUI and zeroes the NIC octets (`00:00:00`) and SSID digest bytes
    // rather than hashing them. Salted hashing is tested in
    // `//src/diagnostics/lib/redactor`; this test only verifies that `MacAddr`,
    // `Bssid`, and `Ssid` dispatch through the global redactor.
    redactor::set_masking_redactor();
    assert_eq!(format!("{mac}"), "00:12:48:00:00:00*");
    assert_eq!(format!("{mac:?}"), "MacAddr(00:12:48:00:00:00*)");
    assert_eq!(format!("{bssid}"), "00:12:48:00:00:00*");
    assert_eq!(format!("{bssid:?}"), "Bssid(00:12:48:00:00:00*)");
    assert_eq!(format!("{ssid}"), "<ssid-0000000000000000>*");
    assert_eq!(format!("{ssid:?}"), "Ssid(<ssid-0000000000000000>*)");

    // `to_string_not_redactable` is explicitly exempt.
    assert_eq!(ssid.to_string_not_redactable(), "foo");
}
