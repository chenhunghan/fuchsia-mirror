// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::query::TargetInfoQuery;
use discovery::{DiscoverySources, TargetState};
use safe_string::TermSafe;

pub trait AsDiagnosticMessage {
    fn as_diagnostic_message(&self) -> String;
}

// WARN: This assumes the discovery sources enum has been condensed into a single value. Since it's
// a bitflags struct it could be more than one. This is intended to be used with an iterator rather
// than have the string handling happen in here.
impl AsDiagnosticMessage for u8 {
    fn as_diagnostic_message(&self) -> String {
        match *self {
            v if v == DiscoverySources::EMULATOR.bits() => "",
            v if v == DiscoverySources::MDNS.bits() => {
                "For mDNS debugging, see: https://fuchsia.dev/fuchsia-src/development/tools/ffx/workflows/network-connectivity/device-discovery#multicast-dns-resolution"
            }
            v if v == DiscoverySources::MANUAL.bits() => "",
            v if v == DiscoverySources::FASTBOOT_FILE.bits() => "",
            v if v == DiscoverySources::USB_VSOCK.bits() => "",
            v if v == DiscoverySources::USB_FASTBOOT.bits() => "",
            v if v == DiscoverySources::GCE.bits() => "",
            v if v == DiscoverySources::UART.bits() => "",
            b => panic!(
                "Un-handled bit type: {b}. This may be a failure from the discovery library of ffx. Please report this to {}",
                errors::BUG_REPORT_URL
            ),
        }
        .to_owned()
    }
}

/// A human-readable representation of a target query.
pub struct ReadableQuery {
    /// The kind of query in a readable form.
    pub kind: &'static str,
    /// The actual value behind the query.
    pub value: String,
}

/// Formats the target state into a human-readable string.
pub fn format_target_state(state: &TargetState) -> String {
    match state {
        TargetState::Product { addrs, serial } => {
            format!(
                "in product state (addrs: [{}]{})",
                addrs.iter().map(|a| a.optional_port_str()).collect::<Vec<_>>().join(", "),
                serial
                    .as_deref()
                    .map(|s| format!(", serial: \"{}\"", TermSafe::from_str_escaped(s)))
                    .unwrap_or_default()
            )
        }
        TargetState::Fastboot(state) => format!(
            "in fastboot ({}: {})",
            TermSafe::from_str_escaped(&state.serial_number),
            state.connection_state
        ),
        TargetState::Unknown => "in an unknown state".to_owned(),
        TargetState::Zedboot => "in zedboot".to_owned(),
    }
}

/// Formats the query into a human-readable struct.
pub fn format_query(query: &TargetInfoQuery) -> ReadableQuery {
    let (kind, value) = match query {
        TargetInfoQuery::NodenameOrId(v) => {
            ("nodename or id (serial number)", TermSafe::from_str_escaped(v).to_string())
        }
        TargetInfoQuery::First => {
            ("not set. We will search for any device on the network", "".to_string())
        }
        TargetInfoQuery::Addr(a) => ("address", a.to_string()),
        TargetInfoQuery::Id(s) => ("id (serial number)", TermSafe::from_str_escaped(s).to_string()),
        TargetInfoQuery::Usb(u) => ("usb", u.to_string()),
        TargetInfoQuery::VSock(v) => ("vsock", v.to_string()),
        TargetInfoQuery::Uart(u) => ("uart", TermSafe::from_str_escaped(u).to_string()),
    };
    ReadableQuery { kind, value }
}

fn format_mdns_target_addr_info(a: &discovery::TargetAddrInfo) -> String {
    let (ip, scope_id, port) = match a {
        discovery::TargetAddrInfo::Ip(ip) => (ip.ip, ip.scope_id, 0),
        discovery::TargetAddrInfo::IpPort(ip_port) => (ip_port.ip, ip_port.scope_id, ip_port.port),
    };
    let target_addr = addr::TargetAddr::new(ip, scope_id, port);
    if port != 0 {
        match target_addr.ip() {
            Some(std::net::IpAddr::V6(_)) => format!("[{target_addr}]:{port}"),
            _ => format!("{target_addr}:{port}"),
        }
    } else {
        format!("{target_addr}")
    }
}

/// Formats an mDNS event into a human-readable string.
pub fn format_mdns_event(event: &discovery::MdnsEventType) -> String {
    match event {
        discovery::MdnsEventType::TargetFound(info) => {
            format!("device found: {}", format_mdns_target_info(info))
        }
        discovery::MdnsEventType::TargetRediscovered(info) => {
            format!("device rediscovered: {}", format_mdns_target_info(info))
        }
        discovery::MdnsEventType::TargetExpired(info) => {
            format!("device expired: {}", format_mdns_target_info(info))
        }
        discovery::MdnsEventType::SocketBound(event) => {
            event.port.as_ref().map(|p| format!("binding on socket: {p}")).unwrap_or_else(|| {
                format!("mDNS bind event to unspecified socket (this is highly unexpected)")
            })
        }
    }
}

/// Formats an `MdnsTargetInfo` struct into a human-readable string.
pub fn format_mdns_target_info(info: &discovery::MdnsTargetInfo) -> String {
    let mut parts = Vec::new();
    if let Some(nodename) = &info.nodename {
        parts.push(format!("nodename: \"{}\"", TermSafe::from_str_escaped(nodename)));
    }
    if let Some(serial) = &info.serial_number {
        parts.push(format!("serial: \"{}\"", TermSafe::from_str_escaped(serial)));
    }
    if !info.addresses.is_empty() {
        let addrs_str =
            info.addresses.iter().map(format_mdns_target_addr_info).collect::<Vec<_>>().join(", ");
        parts.push(format!("addresses: [{addrs_str}]"));
    }
    if let Some(ssh_address) = &info.ssh_address {
        parts.push(format!("ssh_address: {}", format_mdns_target_addr_info(ssh_address)));
    }
    if let Some(iface) = &info.fastboot_interface {
        parts.push(format!("fastboot: {iface:?}"));
    }
    parts.join(", ")
}

/// Extension trait for `TargetInfoQuery` to provide analytics tags.
/// This exists to avoid a circular dependency between the `discovery` and `ffx_diagnostics_formatting` crates.
pub trait TargetInfoQueryExt {
    fn to_analytics_tag(&self) -> String;
}

impl TargetInfoQueryExt for TargetInfoQuery {
    fn to_analytics_tag(&self) -> String {
        match self {
            TargetInfoQuery::First => "unspecified".to_owned(),
            _ => format_query(self).kind.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use discovery::FastbootConnectionState;
    use std::net::SocketAddr;

    #[test]
    fn test_format_query() {
        let query = TargetInfoQuery::NodenameOrId("test".to_string());
        let f = format_query(&query);
        assert_eq!(f.kind, "nodename or id (serial number)");
        assert_eq!(f.value, "test");

        let query = TargetInfoQuery::First;
        let f = format_query(&query);
        assert_eq!(f.kind, "not set. We will search for any device on the network");
        assert_eq!(f.value, "");

        let addr = "192.168.1.1:8080".parse::<SocketAddr>().unwrap();
        let query = TargetInfoQuery::Addr(addr);
        let f = format_query(&query);
        assert_eq!(f.kind, "address");
        assert_eq!(f.value, "192.168.1.1:8080");

        let query = TargetInfoQuery::Id("1234".to_string());
        let f = format_query(&query);
        assert_eq!(f.kind, "id (serial number)");
        assert_eq!(f.value, "1234");

        let query = TargetInfoQuery::Usb(1);
        let f = format_query(&query);
        assert_eq!(f.kind, "usb");
        assert_eq!(f.value, "1");

        let query = TargetInfoQuery::VSock(2);
        let f = format_query(&query);
        assert_eq!(f.kind, "vsock");
        assert_eq!(f.value, "2");
    }

    #[test]
    fn test_format_target_state() {
        let state = TargetState::Unknown;
        assert_eq!(format_target_state(&state), "in an unknown state");

        let state = TargetState::Zedboot;
        assert_eq!(format_target_state(&state), "in zedboot");

        let state = TargetState::Fastboot(discovery::FastbootTargetState {
            serial_number: "1234".to_string(),
            connection_state: FastbootConnectionState::Usb,
        });
        assert_eq!(format_target_state(&state), "in fastboot (1234: Usb)");

        let addr = "192.168.1.1:8080".parse::<SocketAddr>().unwrap();
        let state =
            TargetState::Product { addrs: vec![addr.into()], serial: Some("1234".to_string()) };
        assert_eq!(
            format_target_state(&state),
            "in product state (addrs: [192.168.1.1:8080], serial: \"1234\")"
        );

        let state = TargetState::Product { addrs: vec![addr.into()], serial: None };
        assert_eq!(format_target_state(&state), "in product state (addrs: [192.168.1.1:8080])");

        let addr = "192.168.1.1:0".parse::<SocketAddr>().unwrap();
        let state =
            TargetState::Product { addrs: vec![addr.into()], serial: Some("1234".to_string()) };
        assert_eq!(
            format_target_state(&state),
            "in product state (addrs: [192.168.1.1], serial: \"1234\")"
        );
    }

    #[test]
    fn test_format_mdns_event() {
        let info = discovery::MdnsTargetInfo {
            nodename: Some("test-nodename".to_string()),
            ..Default::default()
        };
        let info_str = format_mdns_target_info(&info);

        let event = discovery::MdnsEventType::TargetFound(info.clone());
        assert_eq!(format_mdns_event(&event), format!("device found: {info_str}"));

        let event = discovery::MdnsEventType::TargetRediscovered(info.clone());
        assert_eq!(format_mdns_event(&event), format!("device rediscovered: {info_str}"));

        let event = discovery::MdnsEventType::TargetExpired(info.clone());
        assert_eq!(format_mdns_event(&event), format!("device expired: {info_str}"));

        let event =
            discovery::MdnsEventType::SocketBound(discovery::MdnsBindEvent { port: Some(1234) });
        assert_eq!(format_mdns_event(&event), "binding on socket: 1234");

        let event = discovery::MdnsEventType::SocketBound(discovery::MdnsBindEvent { port: None });
        assert_eq!(
            format_mdns_event(&event),
            "mDNS bind event to unspecified socket (this is highly unexpected)"
        );
    }

    #[test]
    fn test_format_mdns_target_info_addresses() {
        let ip_v6_ll = addr::TargetAddr::new("fe80::1".parse().unwrap(), 2, 0);
        let ip_v6_ll_port = addr::TargetAddr::new("fe80::1".parse().unwrap(), 2, 8022);
        let ip_v6_ssh = addr::TargetAddr::new("fe80::1".parse().unwrap(), 2, 22);

        let info = discovery::MdnsTargetInfo {
            nodename: Some("target-node".to_string()),
            addresses: vec![
                discovery::TargetAddrInfo::Ip(discovery::TargetIp {
                    ip: "192.168.1.10".parse().unwrap(),
                    scope_id: 0,
                }),
                discovery::TargetAddrInfo::Ip(discovery::TargetIp {
                    ip: "2001:db8::1".parse().unwrap(),
                    scope_id: 0,
                }),
                discovery::TargetAddrInfo::Ip(discovery::TargetIp {
                    ip: "fe80::1".parse().unwrap(),
                    scope_id: 2,
                }),
                discovery::TargetAddrInfo::IpPort(discovery::TargetIpPort {
                    ip: "192.168.1.10".parse().unwrap(),
                    scope_id: 0,
                    port: 8022,
                }),
                discovery::TargetAddrInfo::IpPort(discovery::TargetIpPort {
                    ip: "2001:db8::1".parse().unwrap(),
                    scope_id: 0,
                    port: 8080,
                }),
                discovery::TargetAddrInfo::IpPort(discovery::TargetIpPort {
                    ip: "fe80::1".parse().unwrap(),
                    scope_id: 2,
                    port: 8022,
                }),
            ],
            ssh_address: Some(discovery::TargetAddrInfo::IpPort(discovery::TargetIpPort {
                ip: "fe80::1".parse().unwrap(),
                scope_id: 2,
                port: 22,
            })),
            fastboot_interface: Some(discovery::FastbootInterface::Tcp),
            ..Default::default()
        };
        let formatted = format_mdns_target_info(&info);
        assert!(formatted.contains("nodename: \"target-node\""));
        assert!(formatted.contains("192.168.1.10"));
        assert!(formatted.contains("2001:db8::1"));
        assert!(formatted.contains(&format!("{ip_v6_ll}")));
        assert!(formatted.contains("192.168.1.10:8022"));
        assert!(formatted.contains("[2001:db8::1]:8080"));
        assert!(formatted.contains(&format!("[{ip_v6_ll_port}]:8022")));
        assert!(formatted.contains(&format!("ssh_address: [{ip_v6_ssh}]:22")));
        assert!(formatted.contains("fastboot: Tcp"));
    }
    #[test]
    fn test_formatting_escapes_control_characters() {
        let info = discovery::MdnsTargetInfo {
            nodename: Some("node\x1b[31m_evil\n".to_string()),
            serial_number: Some("serial\x1b]0;hack\x07".to_string()),
            ..Default::default()
        };
        let formatted_info = format_mdns_target_info(&info);
        assert!(!formatted_info.contains('\x1b'));
        assert!(!formatted_info.contains('\n'));
        assert!(!formatted_info.contains('\x07'));
        assert_eq!(
            formatted_info,
            "nodename: \"node\\u{1b}[31m_evil\\n\", serial: \"serial\\u{1b}]0;hack\\u{7}\""
        );

        let state_product =
            TargetState::Product { addrs: vec![], serial: Some("evil\x1b[32m_serial".to_string()) };
        let formatted_state = format_target_state(&state_product);
        assert!(!formatted_state.contains('\x1b'));
        assert_eq!(
            formatted_state,
            "in product state (addrs: [], serial: \"evil\\u{1b}[32m_serial\")"
        );

        let state_fastboot = TargetState::Fastboot(discovery::FastbootTargetState {
            serial_number: "fastboot\x1b[33m_serial".to_string(),
            connection_state: FastbootConnectionState::Usb,
        });
        let formatted_fastboot = format_target_state(&state_fastboot);
        assert!(!formatted_fastboot.contains('\x1b'));
        assert_eq!(formatted_fastboot, "in fastboot (fastboot\\u{1b}[33m_serial: Usb)");

        let query = TargetInfoQuery::NodenameOrId("query\x1b[34m_target".to_string());
        let formatted_query = format_query(&query);
        assert!(!formatted_query.value.contains('\x1b'));
        assert_eq!(formatted_query.value, "query\\u{1b}[34m_target");

        let query_id = TargetInfoQuery::Id("query\x1b[35m_id".to_string());
        let formatted_query_id = format_query(&query_id);
        assert!(!formatted_query_id.value.contains('\x1b'));
        assert_eq!(formatted_query_id.value, "query\\u{1b}[35m_id");

        let query_uart = TargetInfoQuery::Uart("query\x1b[36m_uart".to_string());
        let formatted_query_uart = format_query(&query_uart);
        assert!(!formatted_query_uart.value.contains('\x1b'));
        assert_eq!(formatted_query_uart.value, "query\\u{1b}[36m_uart");
    }
}
