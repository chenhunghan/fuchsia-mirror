// Copyright 2021 The Fuchsia Authors. All rights 1eserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::{Error, Result};
use addr::{TargetAddr, TargetIpAddr};
use manual_targets::watcher::{ManualTargetEvent, ManualTargetState};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};
use usb_fastboot_discovery::FastbootEvent;

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum FastbootConnectionState {
    Usb,
    Tcp(Vec<TargetIpAddr>),
    Udp(Vec<TargetIpAddr>),
}

impl Display for FastbootConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = match self {
            Self::Usb => format!("Usb"),
            Self::Tcp(addr) => format!("Tcp({:?})", addr),
            Self::Udp(addr) => format!("Udp({:?})", addr),
        };
        write!(f, "{}", res)
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct FastbootTargetState {
    pub serial_number: String,
    pub connection_state: FastbootConnectionState,
}

impl Display for FastbootTargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.serial_number, self.connection_state)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum TargetState {
    Unknown,
    Product { addrs: Vec<TargetAddr>, serial: Option<String> },
    Fastboot(FastbootTargetState),
    Zedboot,
}

impl Display for TargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = match self {
            TargetState::Unknown => "Unknown".to_string(),
            TargetState::Product { addrs: addr, serial } => {
                format!(
                    "Product(addrs: [{}] serial: {:?})",
                    addr.iter().map(|a| format!("{}", a)).collect::<Vec<_>>().join(", "),
                    serial.as_ref().map_or("", |s| s.as_str())
                )
            }
            TargetState::Fastboot(state) => format!("Fastboot({})", state),
            TargetState::Zedboot => "Zedboot".to_string(),
        };
        write!(f, "{}", res)
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct TargetHandle {
    pub node_name: Option<String>,
    pub state: TargetState,
    pub manual: bool,
}

impl Display for TargetHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.node_name.as_ref().map_or("", |n| n.as_str());
        write!(
            f,
            "node: {:?} in state: {}{}",
            name,
            self.state,
            if self.manual { "(manual)" } else { "" }
        )
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum TargetEvent {
    /// Indicates a Target has been discovered.
    Added(TargetHandle),
    /// Indicates a Target has been lost.
    Removed(TargetHandle),
}

impl TargetEvent {
    /// Returns the inner [TargetHandle] reference with the lifetime of this object.
    pub fn target_handle(&self) -> &TargetHandle {
        match self {
            Self::Added(h) | Self::Removed(h) => h,
        }
    }

    pub(crate) fn from_usb_event(
        event: usb_driver_api::DeviceEvent,
        node_name: Option<String>,
    ) -> TargetEvent {
        match event {
            usb_driver_api::DeviceEvent::Added { cid, serial } => {
                TargetEvent::Added(TargetHandle {
                    node_name,
                    state: TargetState::Product { addrs: vec![TargetAddr::UsbCtx(cid)], serial },
                    manual: false,
                })
            }
            usb_driver_api::DeviceEvent::Removed { cid } => TargetEvent::Removed(TargetHandle {
                node_name,
                state: TargetState::Product { addrs: vec![TargetAddr::UsbCtx(cid)], serial: None },
                manual: false,
            }),
        }
    }
}

impl TryFrom<mdns_discovery::MdnsEventType> for TargetEvent {
    type Error = Error;

    fn try_from(e: mdns_discovery::MdnsEventType) -> Result<Self> {
        match e {
            mdns_discovery::MdnsEventType::TargetFound(info)
            | mdns_discovery::MdnsEventType::TargetRediscovered(info) => {
                Ok(TargetEvent::Added(TargetHandle::try_from(info)?))
            }
            mdns_discovery::MdnsEventType::TargetExpired(info) => {
                Ok(TargetEvent::Removed(TargetHandle::try_from(info)?))
            }
            mdns_discovery::MdnsEventType::SocketBound(_) => Err(Error::SocketBoundUnsupported),
        }
    }
}

impl From<emulator_instance::EmulatorTargetAction> for TargetEvent {
    fn from(e: emulator_instance::EmulatorTargetAction) -> Self {
        match e {
            emulator_instance::EmulatorTargetAction::Add(info) => TargetEvent::Added(info.into()),
            emulator_instance::EmulatorTargetAction::Remove(info) => {
                TargetEvent::Removed(info.into())
            }
        }
    }
}

impl TryFrom<mdns_discovery::MdnsTargetInfo> for TargetHandle {
    type Error = Error;

    fn try_from(info: mdns_discovery::MdnsTargetInfo) -> Result<Self> {
        let mut addrs: Vec<TargetIpAddr> = info
            .addresses
            .into_iter()
            .map(|a| match a {
                mdns_discovery::TargetAddrInfo::Ip(ip) => TargetIpAddr::new(ip.ip, ip.scope_id, 0),
                mdns_discovery::TargetAddrInfo::IpPort(ip_port) => {
                    TargetIpAddr::new(ip_port.ip, ip_port.scope_id, ip_port.port)
                }
            })
            .collect();
        // Sorting them this way puts IPv6 above IPv4
        addrs.sort_by(|a, b| b.cmp(a));

        if addrs.is_empty() {
            return Err(Error::TargetHasNoAddresses);
        }

        // Let the target state first dictate what state the device is in. If there is an RCS
        // connection, this supercedes anything else and should set the device into a `PRODUCT`
        // state. Other states appear to be a bit more iffy, so just use the fields in `TargetInfo`
        // to settle on the state afterward.
        //
        // It appears it's possible to be in product mode and also have a fastboot interface set at
        // the same time.
        let state = match info.fastboot_interface {
            None => TargetState::Product {
                addrs: addrs.into_iter().map(Into::into).collect(),
                serial: info.serial_number,
            },
            Some(iface) => {
                let serial_number = info.serial_number.unwrap_or_default();
                let connection_state = match iface {
                    mdns_discovery::FastbootInterface::Udp => FastbootConnectionState::Udp(addrs),
                    mdns_discovery::FastbootInterface::Tcp => FastbootConnectionState::Tcp(addrs),
                };
                TargetState::Fastboot(FastbootTargetState { serial_number, connection_state })
            }
        };

        Ok(TargetHandle { node_name: info.nodename, state, manual: false })
    }
}

impl From<emulator_instance::EmulatorTargetInfo> for TargetHandle {
    fn from(info: emulator_instance::EmulatorTargetInfo) -> Self {
        let addrs = info
            .addresses
            .into_iter()
            .map(|addr| match addr {
                emulator_instance::EmulatorAddr::LoopbackPort(port) => {
                    TargetAddr::Net(std::net::SocketAddr::new(
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                        port,
                    ))
                }
                emulator_instance::EmulatorAddr::Vsock { cid } => TargetAddr::VSockCtx(cid),
            })
            .collect();
        TargetHandle {
            node_name: Some(info.nodename),
            state: TargetState::Product { addrs, serial: info.serial_number },
            manual: false,
        }
    }
}

impl From<FastbootEvent> for TargetEvent {
    fn from(fastboot_event: FastbootEvent) -> Self {
        match fastboot_event {
            FastbootEvent::Discovered(serial) => {
                let handle = TargetHandle {
                    node_name: Some("".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: serial,
                        connection_state: FastbootConnectionState::Usb,
                    }),
                    manual: false,
                };
                TargetEvent::Added(handle)
            }
            FastbootEvent::Lost(serial) => {
                let handle = TargetHandle {
                    node_name: Some("".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: serial,
                        connection_state: FastbootConnectionState::Usb,
                    }),
                    manual: false,
                };
                TargetEvent::Removed(handle)
            }
        }
    }
}

impl From<ManualTargetEvent> for TargetEvent {
    fn from(manual_target_event: ManualTargetEvent) -> Self {
        match manual_target_event {
            ManualTargetEvent::Discovered(manual_target, manual_state) => {
                let state = match manual_state {
                    ManualTargetState::Disconnected => TargetState::Unknown,
                    ManualTargetState::Product => TargetState::Product {
                        addrs: vec![manual_target.addr().into()],
                        serial: None,
                    },
                    ManualTargetState::Fastboot => TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state: FastbootConnectionState::Tcp(vec![
                            manual_target.addr().into(),
                        ]),
                    }),
                };

                let handle = TargetHandle {
                    node_name: Some(manual_target.addr().to_string()),
                    state,
                    manual: true,
                };
                TargetEvent::Added(handle)
            }
            ManualTargetEvent::Lost(manual_target) => {
                let handle = TargetHandle {
                    node_name: Some(manual_target.addr().to_string()),
                    state: TargetState::Unknown,
                    manual: true,
                };
                TargetEvent::Removed(handle)
            }
        }
    }
}

impl From<fastboot_file_discovery::FastbootEvent> for TargetEvent {
    fn from(fastboot_event: fastboot_file_discovery::FastbootEvent) -> Self {
        match fastboot_event {
            fastboot_file_discovery::FastbootEvent::Discovered(device) => {
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    fastboot_file_discovery::FastbootMode::UDP => {
                        FastbootConnectionState::Udp(vec![address])
                    }
                    fastboot_file_discovery::FastbootMode::TCP => {
                        FastbootConnectionState::Tcp(vec![address])
                    }
                };

                let handle = TargetHandle {
                    node_name: None,
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state,
                    }),
                    manual: false,
                };
                TargetEvent::Added(handle)
            }
            fastboot_file_discovery::FastbootEvent::Lost(device) => {
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    fastboot_file_discovery::FastbootMode::UDP => {
                        FastbootConnectionState::Udp(vec![address])
                    }
                    fastboot_file_discovery::FastbootMode::TCP => {
                        FastbootConnectionState::Tcp(vec![address])
                    }
                };
                let handle = TargetHandle {
                    node_name: Some("".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state,
                    }),
                    manual: false,
                };
                TargetEvent::Removed(handle)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use addr::TargetAddr;
    use manual_targets::watcher::ManualTarget;
    use net_declare::std_socket_addr;
    use pretty_assertions::assert_eq;
    use std::str::FromStr;

    #[test]
    fn test_from_fastbootevent_for_targetevent() -> Result<()> {
        {
            let f = FastbootEvent::Lost("1234".to_string());
            let t = TargetEvent::from(f);
            assert_eq!(
                t,
                TargetEvent::Removed(TargetHandle {
                    node_name: Some("".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "1234".to_string(),
                        connection_state: FastbootConnectionState::Usb,
                    }),
                    manual: false,
                })
            );
        }

        {
            let f = FastbootEvent::Discovered("1234".to_string());
            let t = TargetEvent::from(f);
            assert_eq!(
                t,
                TargetEvent::Added(TargetHandle {
                    node_name: Some("".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "1234".to_string(),
                        connection_state: FastbootConnectionState::Usb,
                    }),
                    manual: false,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn test_from_usb_event_for_targetevent() -> Result<()> {
        let node_name = Some("test_node".to_string());

        {
            let e =
                usb_driver_api::DeviceEvent::Added { cid: 123, serial: Some("1234".to_string()) };
            let t = TargetEvent::from_usb_event(e, node_name.clone());
            assert_eq!(
                t,
                TargetEvent::Added(TargetHandle {
                    node_name: node_name.clone(),
                    state: TargetState::Product {
                        addrs: vec![TargetAddr::UsbCtx(123)],
                        serial: Some("1234".to_string()),
                    },
                    manual: false,
                })
            );
        }

        {
            let e = usb_driver_api::DeviceEvent::Removed { cid: 123 };
            let t = TargetEvent::from_usb_event(e, node_name.clone());
            assert_eq!(
                t,
                TargetEvent::Removed(TargetHandle {
                    node_name: node_name.clone(),
                    state: TargetState::Product {
                        addrs: vec![TargetAddr::UsbCtx(123)],
                        serial: None,
                    },
                    manual: false,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn test_try_from_mdns_target_info_for_targethandle() -> Result<()> {
        {
            let info: mdns_discovery::MdnsTargetInfo = Default::default();
            assert!(TargetHandle::try_from(info).is_err());
        }
        {
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                ..Default::default()
            };
            assert!(TargetHandle::try_from(info).is_err());
        }
        {
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![],
                ..Default::default()
            };
            assert!(TargetHandle::try_from(info).is_err());
        }
        {
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                ..Default::default()
            };
            assert_eq!(
                TargetHandle::try_from(info)?,
                TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product {
                        addrs: vec![TargetAddr::from(std_socket_addr!("127.0.0.1:8080"))],
                        serial: None
                    },
                    manual: false,
                }
            );
        }
        {
            let addr = TargetIpAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                fastboot_interface: Some(mdns_discovery::FastbootInterface::Udp),
                ..Default::default()
            };
            assert_eq!(
                TargetHandle::try_from(info)?,
                TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state: FastbootConnectionState::Udp(vec![addr])
                    }),
                    manual: false,
                }
            );
        }
        {
            let addr = TargetIpAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                fastboot_interface: Some(mdns_discovery::FastbootInterface::Tcp),
                ..Default::default()
            };
            assert_eq!(
                TargetHandle::try_from(info)?,
                TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state: FastbootConnectionState::Tcp(vec![addr])
                    }),
                    manual: false,
                }
            );
        }
        Ok(())
    }

    #[test]
    fn test_from_mdnseventtype_for_targetevent() -> Result<()> {
        {
            // SocketBound is not supported
            let mdns_event = mdns_discovery::MdnsEventType::SocketBound(Default::default());
            assert!(TargetEvent::try_from(mdns_event).is_err());
        }
        {
            let addr = TargetAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                ..Default::default()
            };
            let mdns_event = mdns_discovery::MdnsEventType::TargetFound(info);
            assert_eq!(
                TargetEvent::try_from(mdns_event)?,
                TargetEvent::Added(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product { addrs: vec![addr], serial: None },
                    manual: false,
                })
            );
        }
        {
            let addr = TargetAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                serial_number: Some("12348890".to_string()),
                ..Default::default()
            };
            let mdns_event = mdns_discovery::MdnsEventType::TargetFound(info);
            assert_eq!(
                TargetEvent::try_from(mdns_event)?,
                TargetEvent::Added(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product {
                        addrs: vec![addr],
                        serial: Some("12348890".to_string())
                    },
                    manual: false,
                })
            );
        }
        {
            let addr = TargetAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                ..Default::default()
            };
            let mdns_event = mdns_discovery::MdnsEventType::TargetRediscovered(info);
            assert_eq!(
                TargetEvent::try_from(mdns_event)?,
                TargetEvent::Added(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product { addrs: vec![addr], serial: None },
                    manual: false,
                })
            );
        }
        {
            let addr = TargetAddr::from(std_socket_addr!("127.0.0.1:8080"));
            let addr_info = mdns_discovery::TargetAddrInfo::IpPort(mdns_discovery::TargetIpPort {
                ip: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                scope_id: 0,
                port: 8080,
            });
            let info = mdns_discovery::MdnsTargetInfo {
                nodename: Some("foo".to_string()),
                addresses: vec![addr_info],
                ..Default::default()
            };
            let mdns_event = mdns_discovery::MdnsEventType::TargetExpired(info);
            assert_eq!(
                TargetEvent::try_from(mdns_event)?,
                TargetEvent::Removed(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product { addrs: vec![addr], serial: None },
                    manual: false,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn test_from_emulatoreventtype_for_targetevent() -> Result<()> {
        let addr = TargetAddr::from_str("127.0.0.1:8080").unwrap();
        {
            let info = emulator_instance::EmulatorTargetInfo {
                nodename: "foo".to_string(),
                addresses: vec![emulator_instance::EmulatorAddr::LoopbackPort(8080)],
                serial_number: None,
                ssh_port: Some(8080),
            };
            let emulator_event = emulator_instance::EmulatorTargetAction::Add(info);
            assert_eq!(
                TargetEvent::from(emulator_event),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product { addrs: vec![addr.clone()], serial: None },
                    manual: false,
                })
            );
        }
        {
            let info = emulator_instance::EmulatorTargetInfo {
                nodename: "foo".to_string(),
                addresses: vec![emulator_instance::EmulatorAddr::LoopbackPort(8080)],
                serial_number: None,
                ssh_port: Some(8080),
            };
            let emulator_event = emulator_instance::EmulatorTargetAction::Remove(info);
            assert_eq!(
                TargetEvent::from(emulator_event),
                TargetEvent::Removed(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product { addrs: vec![addr.clone()], serial: None },
                    manual: false,
                })
            );
        }
        {
            let info = emulator_instance::EmulatorTargetInfo {
                nodename: "foo".to_string(),
                addresses: vec![emulator_instance::EmulatorAddr::LoopbackPort(8080)],
                serial_number: Some("EM-9876".to_string()),
                ssh_port: Some(8080),
            };
            let emulator_event = emulator_instance::EmulatorTargetAction::Add(info);
            assert_eq!(
                TargetEvent::from(emulator_event),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("foo".to_string()),
                    state: TargetState::Product {
                        addrs: vec![addr],
                        serial: Some("EM-9876".to_string()),
                    },
                    manual: false,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn test_from_manual_target_event_for_target_event() -> Result<()> {
        {
            let addr = std_socket_addr!("127.0.0.1:8080");
            let lifetime = None;
            let manual_target_event = ManualTargetEvent::Discovered(
                ManualTarget::new(addr, lifetime),
                ManualTargetState::Product,
            );
            assert_eq!(
                TargetEvent::from(manual_target_event),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("127.0.0.1:8080".to_string()),
                    state: TargetState::Product { addrs: vec![addr.into()], serial: None },
                    manual: true,
                })
            );
        }
        {
            let addr = std_socket_addr!("[::1]:8032");
            let lifetime = None;
            let manual_target_event = ManualTargetEvent::Discovered(
                ManualTarget::new(addr, lifetime),
                ManualTargetState::Product,
            );
            assert_eq!(
                TargetEvent::from(manual_target_event),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("[::1]:8032".to_string()),
                    state: TargetState::Product { addrs: vec![addr.into()], serial: None },
                    manual: true,
                })
            );
        }
        {
            let addr = std_socket_addr!("127.0.0.1:8080");
            let lifetime = None;
            let manual_target_event = ManualTargetEvent::Discovered(
                ManualTarget::new(addr, lifetime),
                ManualTargetState::Fastboot,
            );
            assert_eq!(
                TargetEvent::from(manual_target_event),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("127.0.0.1:8080".to_string()),
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number: "".to_string(),
                        connection_state: FastbootConnectionState::Tcp(vec![addr.into()])
                    }),
                    manual: true,
                })
            );
        }
        {
            let addr = std_socket_addr!("127.0.0.1:8080");
            let lifetime = None;
            let manual_target_event = ManualTargetEvent::Lost(ManualTarget::new(addr, lifetime));
            assert_eq!(
                TargetEvent::from(manual_target_event),
                TargetEvent::Removed(TargetHandle {
                    node_name: Some("127.0.0.1:8080".to_string()),
                    state: TargetState::Unknown,
                    manual: true,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn test_try_from_mdns_target_info_empty_addresses_returns_error() {
        let empty_info = mdns_discovery::MdnsTargetInfo {
            nodename: Some("no-addrs-device".to_string()),
            addresses: vec![],
            ..Default::default()
        };
        let err = TargetHandle::try_from(empty_info).unwrap_err();
        assert!(matches!(err, crate::error::Error::TargetHasNoAddresses));
    }

    #[test]
    fn test_from_emulator_target_info_vsock_and_loopback() {
        let emu_info = emulator_instance::EmulatorTargetInfo {
            nodename: "fuchsia-emulator-hybrid".to_string(),
            addresses: vec![
                emulator_instance::EmulatorAddr::Vsock { cid: 42 },
                emulator_instance::EmulatorAddr::LoopbackPort(8022),
            ],
            serial_number: None,
            ssh_port: Some(8022),
        };
        let handle = TargetHandle::from(emu_info);
        assert_eq!(handle.node_name.as_deref(), Some("fuchsia-emulator-hybrid"));
        match handle.state {
            TargetState::Product { addrs, serial } => {
                assert_eq!(serial, None);
                assert_eq!(addrs.len(), 2);
                assert!(addrs.contains(&TargetAddr::VSockCtx(42)));
                assert!(addrs.contains(&TargetAddr::Net("127.0.0.1:8022".parse().unwrap())));
            }
            _ => panic!("Expected Product state"),
        }
    }
}
