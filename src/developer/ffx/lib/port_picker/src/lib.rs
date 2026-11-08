// Copyright 2020-2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, TcpListener, ToSocketAddrs};

pub type Port = u16;

/// Asks the OS for a free port
fn ask_free_tcp_port() -> Option<Port> {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0);
    test_bind_tcp(ipv6).or_else(|| test_bind_tcp(ipv4))
}

/// Check if a port is free on TCP
pub fn is_free_tcp_port(port: u16) -> Option<Port> {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0);
    // Ignore the IPv6 free state if IPv6 is not supported.
    if (test_bind_tcp(ipv6).is_some()
        || test_bind_tcp(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)).is_none())
        && test_bind_tcp(ipv4).is_some()
    {
        return Some(port);
    }
    None
}

/// Picks an available tcp port
pub fn pick_unused_port() -> Option<Port> {
    for _ in 0..10 {
        if let Some(port) = ask_free_tcp_port().and_then(is_free_tcp_port) {
            return Some(port);
        }
    }
    None
}

// Try to bind to a socket using TCP
fn test_bind_tcp<A: ToSocketAddrs>(addr: A) -> Option<Port> {
    Some(TcpListener::bind(addr).ok()?.local_addr().ok()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_pick_unused_port_avoids_occupied_ipv4_localhost() {
        let listeners: Vec<_> = (0..128)
            .map(|_| TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap())
            .collect();
        let occupied: HashSet<Port> =
            listeners.iter().map(|l| l.local_addr().unwrap().port()).collect();
        for _ in 0..1000 {
            let port = pick_unused_port().expect("should find an unused port");
            assert!(!occupied.contains(&port));
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                .expect("picked port should be bindable on 127.0.0.1");
        }
    }

    #[test]
    fn test_pick_unused_port_avoids_occupied_ipv6_localhost() {
        if TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)).is_err() {
            return;
        }
        let listeners: Vec<_> = (0..128)
            .map(|_| TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)).unwrap())
            .collect();
        let occupied: HashSet<Port> =
            listeners.iter().map(|l| l.local_addr().unwrap().port()).collect();
        for _ in 0..1000 {
            let port = pick_unused_port().expect("should find an unused port");
            assert!(!occupied.contains(&port));
        }
    }
}
