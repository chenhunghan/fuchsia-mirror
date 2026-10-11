// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetEvent;
use crate::error::Error;
use crate::events::{FastbootConnectionState, FastbootTargetState, TargetHandle, TargetState};
use addr::TargetIpAddr;
use fastboot::command::{ClientVariable, Command};
use fastboot_file_discovery::{
    FastbootEvent, FastbootEventHandler, FastbootFileWatcher, FastbootMode, get_fastboot_devices,
};
use ffx_config::EnvironmentContext;
use ffx_fastboot_transport_interface::{tcp, udp};
use futures::channel::mpsc::UnboundedSender;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_FASTBOOT_DISCOVERY_TIMEOUT_MS: u64 = 500;

async fn get_serial_number(
    context: &EnvironmentContext,
    mode: FastbootMode,
    addr: SocketAddr,
) -> Option<String> {
    let timeout = Duration::from_millis(
        context
            .get(ffx_config::keys::DISCOVERY_FASTBOOT_TIMEOUT_MS)
            .unwrap_or(DEFAULT_FASTBOOT_DISCOVERY_TIMEOUT_MS),
    );
    let chrono_timeout = chrono::Duration::from_std(timeout).unwrap();
    let ctx = fastboot::FastbootContext::new();
    let command = Command::GetVar(ClientVariable::SerialNumber);

    let res = match mode {
        FastbootMode::TCP => {
            let mut interface = match tcp::open_once(&addr, timeout).await {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("Failed to open TCP Fastboot interface for {}: {}", addr, e);
                    return None;
                }
            };
            match fastboot::send_with_timeout(ctx, command.clone(), &mut interface, chrono_timeout)
                .await
            {
                Ok(res) => Some(res),
                Err(e) => {
                    log::warn!("Fastboot TCP command failed for {}: {}", addr, e);
                    None
                }
            }
        }
        FastbootMode::UDP => {
            let mut interface = match udp::open(addr).await {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("Failed to open UDP Fastboot interface for {}: {}", addr, e);
                    return None;
                }
            };
            match fastboot::send_with_timeout(ctx, command.clone(), &mut interface, chrono_timeout)
                .await
            {
                Ok(res) => Some(res),
                Err(e) => {
                    log::warn!("Fastboot UDP command failed for {}: {}", addr, e);
                    None
                }
            }
        }
    }?;

    match res {
        fastboot::reply::Reply::Okay(serial) => Some(serial.trim_end_matches('\0').to_string()),
        _ => None,
    }
}

#[derive(Clone)]
struct FastbootFileHandler {
    context: EnvironmentContext,
    sender: UnboundedSender<TargetEvent>,
    discovered_serials: Arc<Mutex<HashMap<SocketAddr, String>>>,
}

impl FastbootEventHandler for FastbootFileHandler {
    async fn handle_event(&mut self, event: FastbootEvent) {
        match event {
            FastbootEvent::Discovered(device) => {
                let serial_number =
                    get_serial_number(&self.context, device.mode(), device.socket_addr())
                        .await
                        .unwrap_or_else(String::new);
                if let Ok(mut serials) = self.discovered_serials.lock() {
                    serials.insert(device.socket_addr(), serial_number.clone());
                } else {
                    log::error!("Failed to lock discovered_serials map");
                }
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    FastbootMode::UDP => FastbootConnectionState::Udp(vec![address]),
                    FastbootMode::TCP => FastbootConnectionState::Tcp(vec![address]),
                };
                let handle = TargetHandle {
                    node_name: None,
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number,
                        connection_state,
                    }),
                    manual: false,
                };
                let _ = self.sender.unbounded_send(TargetEvent::Added(handle));
            }
            FastbootEvent::Lost(device) => {
                let serial_number = if let Ok(mut serials) = self.discovered_serials.lock() {
                    serials.remove(&device.socket_addr()).unwrap_or_else(String::new)
                } else {
                    log::error!("Failed to lock discovered_serials map");
                    String::new()
                };
                let address: TargetIpAddr = device.socket_addr().into();
                let connection_state = match device.mode() {
                    FastbootMode::UDP => FastbootConnectionState::Udp(vec![address]),
                    FastbootMode::TCP => FastbootConnectionState::Tcp(vec![address]),
                };
                let handle = TargetHandle {
                    node_name: None,
                    state: TargetState::Fastboot(FastbootTargetState {
                        serial_number,
                        connection_state,
                    }),
                    manual: false,
                };
                let _ = self.sender.unbounded_send(TargetEvent::Removed(handle));
            }
        }
    }
}

pub struct FastbootWatcher {
    _watcher: FastbootFileWatcher,
    _task: fuchsia_async::Task<()>,
}

impl FastbootWatcher {
    pub fn new(
        context: EnvironmentContext,
        instance_root: PathBuf,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self, Error> {
        let existing = get_fastboot_devices(&instance_root)
            .map_err(|err| Error::FastbootDiscovery { path: instance_root.clone(), err })?;

        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));

        // Spawn a task scoped to the struct's lifetime for pre-existing devices.
        let initial_handler = FastbootFileHandler {
            context: context.clone(),
            sender: sender.clone(),
            discovered_serials: discovered_serials.clone(),
        };
        let task = fuchsia_async::Task::local(async move {
            let futures = existing.into_iter().map(|device| {
                let mut handler = initial_handler.clone();
                async move {
                    let event = FastbootEvent::Discovered(device);
                    handler.handle_event(event).await;
                }
            });
            futures::future::join_all(futures).await;
        });

        // The async FastbootFileHandler processes subsequent file events natively.
        let handler = FastbootFileHandler { context, sender, discovered_serials };
        let watcher = fastboot_file_discovery::recommended_watcher(handler, instance_root.clone())
            .map_err(|e| Error::FastbootWatcher { path: instance_root, err: e.to_string() })?;

        Ok(Self { _watcher: watcher, _task: task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastboot_file_discovery::FastbootEntry;
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    #[fuchsia::test]
    async fn test_fastboot_file_handler_discovered() {
        let test_env = ffx_config::test_init().unwrap();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        // Use an unreachable port on loopback so get_serial_number fails quickly.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = listener.local_addr().unwrap();
        drop(listener);
        let device: FastbootEntry = format!("tcp:{closed_addr}").parse().unwrap();
        let event = FastbootEvent::Discovered(device.clone());
        handler.handle_event(event).await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Added(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, "");
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Tcp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }

        assert_eq!(
            discovered_serials.lock().unwrap().get(&device.socket_addr()),
            Some(&"".to_string())
        );
    }

    #[fuchsia::test]
    async fn test_fastboot_file_handler_discovered_tcp_with_serial() {
        let test_env = ffx_config::test_init().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = fasync::Task::local(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // 1. Read "FB01" handshake and respond with "FB01"
            let mut handshake = [0u8; 4];
            stream.read_exact(&mut handshake).await.unwrap();
            assert_eq!(&handshake, b"FB01");
            stream.write_all(b"FB01").await.unwrap();

            // 2. Read 8-byte big-endian length + command payload ("getvar:serialno")
            let mut len_bytes = [0u8; 8];
            stream.read_exact(&mut len_bytes).await.unwrap();
            let cmd_len = u64::from_be_bytes(len_bytes) as usize;
            let mut cmd_buf = vec![0u8; cmd_len];
            stream.read_exact(&mut cmd_buf).await.unwrap();
            assert_eq!(&cmd_buf, b"getvar:serialno");

            // 3. Respond with 8-byte big-endian length + "OKAYtcp_serial_123"
            let reply = b"OKAYtcp_serial_123";
            stream.write_all(&(reply.len() as u64).to_be_bytes()).await.unwrap();
            stream.write_all(reply).await.unwrap();
        });

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        let device: FastbootEntry = format!("tcp:{addr}").parse().unwrap();
        handler.handle_event(FastbootEvent::Discovered(device.clone())).await;
        server_task.await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Added(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, "tcp_serial_123");
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Tcp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }

        assert_eq!(
            discovered_serials.lock().unwrap().get(&device.socket_addr()),
            Some(&"tcp_serial_123".to_string())
        );
    }

    #[fuchsia::test]
    async fn test_fastboot_file_handler_discovered_udp_with_serial() {
        let test_env = ffx_config::test_init().unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();

        let server_task = fasync::Task::local(async move {
            let mut buf = [0u8; 1500];

            // 1. Receive Query packet (id = 0x01)
            let (sz, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(sz, 4);
            assert_eq!(buf[0], 0x01);
            // Reply with Query response: id=0x01, flags=0, seq=0, next_seq=0x0010
            let query_resp = [0x01, 0x00, 0x00, 0x00, 0x00, 0x10];
            server.send_to(&query_resp, peer).await.unwrap();

            // 2. Receive Init packet (id = 0x02, seq = 0x0010)
            let (sz, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(sz, 8);
            assert_eq!(buf[0], 0x02);
            // Reply with Init response: id=0x02, flags=0, seq=0x0010, version=0x0001, max_size=0x0200 (512)
            let init_resp = [0x02, 0x00, 0x00, 0x10, 0x00, 0x01, 0x02, 0x00];
            server.send_to(&init_resp, peer).await.unwrap();

            // 3. Receive Fastboot command packet (id = 0x03, seq = 0x0011, payload = "getvar:serialno")
            let (sz, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(buf[0], 0x03);
            assert_eq!(&buf[4..sz], b"getvar:serialno");
            // Send empty ACK for command write (seq = 0x0011)
            let ack = [0x03, 0x00, 0x00, 0x11];
            server.send_to(&ack, peer).await.unwrap();

            // 4. Receive empty Fastboot read poll packet (id = 0x03, seq = 0x0012)
            let (sz, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(sz, 4);
            assert_eq!(buf[0], 0x03);
            // Send Fastboot reply packet (seq = 0x0012, payload = "OKAYudp_serial_456")
            let mut reply_pkt = vec![0x03, 0x00, 0x00, 0x12];
            reply_pkt.extend_from_slice(b"OKAYudp_serial_456");
            server.send_to(&reply_pkt, peer).await.unwrap();
        });

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        let device: FastbootEntry = format!("udp:{addr}").parse().unwrap();
        handler.handle_event(FastbootEvent::Discovered(device.clone())).await;
        server_task.await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Added(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, "udp_serial_456");
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Udp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }

        assert_eq!(
            discovered_serials.lock().unwrap().get(&device.socket_addr()),
            Some(&"udp_serial_456".to_string())
        );
    }

    #[fuchsia::test]
    async fn test_fastboot_file_handler_lost() {
        let test_env = ffx_config::test_init().unwrap();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let discovered_serials = Arc::new(Mutex::new(HashMap::new()));
        let mut handler = FastbootFileHandler {
            context: test_env.context.clone(),
            sender,
            discovered_serials: discovered_serials.clone(),
        };
        let device: FastbootEntry = "udp:127.0.0.1:5555".parse().unwrap();
        discovered_serials.lock().unwrap().insert(device.socket_addr(), "test_serial".to_string());

        let event = FastbootEvent::Lost(device.clone());
        handler.handle_event(event).await;

        let target_event = receiver.next().await.unwrap();
        match target_event {
            TargetEvent::Removed(handle) => {
                assert_eq!(handle.node_name, None);
                if let TargetState::Fastboot(fb) = handle.state {
                    assert_eq!(fb.serial_number, "test_serial");
                    assert_eq!(
                        fb.connection_state,
                        FastbootConnectionState::Udp(vec![device.socket_addr().into()])
                    );
                } else {
                    panic!("wrong state type");
                }
            }
            _ => panic!("wrong event form"),
        }
    }
}
