// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Probes the guest network of an emulator to check the isolation provided by
//! botanist's emulator network proxy (//tools/botanist/targets/emu_net_proxy.go).
//!
//! The test is built twice:
//!
//! * With the `internet` feature, it runs in `aemu_internet_env`. It checks that
//!   the proxy answers DNS only for names in the emulator internet allowlist,
//!   rejects disallowed TLS and HTTP requests, and that the guest has no other
//!   way out (direct connections to the internet or to the host are blocked).
//! * Without it, it runs in the default `aemu_env`, and checks that the guest
//!   has no IPv4 network access at all.
//!
//! None of the checks depend on reaching the internet, so the test does not
//! depend on external services.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// The emulator internet allowlist enforced by the proxy.
const ALLOWLIST: &str =
    include_str!("../../../../tools/botanist/targets/emu_net_allowlist/allowlist.txt");

/// The DNS server that the proxy hands out over DHCP (SLiRP's 10.0.2.3).
const DNS_SERVER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 3), 53);
/// The address that the proxy returns for every allowed DNS name.
const PROXY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 100);
/// SLiRP's gateway address, which maps to the host's loopback interface when
/// SLiRP is not restricted.
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

const DNS_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns the exact (non-wildcard) host names in the allowlist.
fn allowed_hosts() -> Vec<&'static str> {
    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("*."))
        .collect()
}

#[derive(Debug)]
struct DnsReply {
    rcode: u8,
    answers: Vec<Ipv4Addr>,
}

fn build_dns_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::new();
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // RD=1
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&[0; 6]); // ANCOUNT, NSCOUNT, ARCOUNT
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    q
}

fn parse_dns_reply(id: u16, query_len: usize, b: &[u8]) -> Option<DnsReply> {
    if b.len() < query_len || u16::from_be_bytes([b[0], b[1]]) != id || b[2] & 0x80 == 0 {
        return None;
    }
    let rcode = b[3] & 0x0f;
    let ancount = u16::from_be_bytes([b[6], b[7]]) as usize;
    // Answers follow the echoed question. The proxy encodes each answer as a
    // name pointer (2), type (2), class (2), TTL (4), length (2) and data.
    let mut answers = Vec::new();
    let mut idx = query_len;
    for _ in 0..ancount {
        if idx + 12 > b.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([b[idx + 2], b[idx + 3]]);
        let rdlen = u16::from_be_bytes([b[idx + 10], b[idx + 11]]) as usize;
        idx += 12;
        if idx + rdlen > b.len() {
            return None;
        }
        if rtype == 1 && rdlen == 4 {
            answers.push(Ipv4Addr::new(b[idx], b[idx + 1], b[idx + 2], b[idx + 3]));
        }
        idx += rdlen;
    }
    Some(DnsReply { rcode, answers })
}

/// Sends a DNS query for `name` to `server` and waits for a reply.
fn dns_query(server: SocketAddrV4, name: &str, qtype: u16) -> std::io::Result<DnsReply> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(DNS_TIMEOUT))?;
    let id: u16 = 0x5eed ^ (name.len() as u16);
    let query = build_dns_query(id, name, qtype);
    socket.send_to(&query, server)?;
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + DNS_TIMEOUT;
    while Instant::now() < deadline {
        let (n, from) = socket.recv_from(&mut buf)?;
        if from != SocketAddr::V4(server) {
            continue;
        }
        if let Some(reply) = parse_dns_reply(id, query.len(), &buf[..n]) {
            return Ok(reply);
        }
    }
    Err(ErrorKind::TimedOut.into())
}

/// Connects to `addr`, giving up after `CONNECT_TIMEOUT`.
///
/// `TcpStream::connect_timeout` can't be used: it puts the socket in
/// non-blocking mode with the `FIONBIO` ioctl, which fails with `ENOTTY` on
/// Fuchsia. Instead, connect on another thread and stop waiting for it after
/// the timeout. Connections that are silently dropped would otherwise block
/// until the network stack gives up; in that case the connecting thread is
/// leaked until then, which is acceptable in this short-lived test process.
fn connect(addr: SocketAddrV4) -> std::io::Result<TcpStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(TcpStream::connect(addr));
    });
    rx.recv_timeout(CONNECT_TIMEOUT).unwrap_or_else(|_| Err(ErrorKind::TimedOut.into()))
}

/// Returns true if a TCP connection to `addr` succeeds.
fn can_connect(addr: SocketAddrV4) -> bool {
    connect(addr).is_ok()
}

/// Returns the addresses in `addrs` that a TCP connection succeeds to.
///
/// The addresses are probed concurrently: blocked connections are usually
/// dropped rather than reset, so each probe of a blocked address takes the
/// full `CONNECT_TIMEOUT`.
fn reachable(addrs: Vec<SocketAddrV4>) -> Vec<SocketAddrV4> {
    let probes: Vec<_> = addrs
        .into_iter()
        .map(|addr| (addr, std::thread::spawn(move || can_connect(addr))))
        .collect();
    probes
        .into_iter()
        .filter_map(|(addr, probe)| probe.join().expect("probe thread panicked").then_some(addr))
        .collect()
}

/// Destinations that must never be reachable from the guest, with or without
/// emulator internet access.
fn forbidden_destinations() -> Vec<SocketAddrV4> {
    vec![
        // The internet, directly rather than through the proxy.
        SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443),
        SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53),
        SocketAddrV4::new(Ipv4Addr::new(142, 250, 0, 1), 443),
        // Services on the host's loopback interface through SLiRP's gateway.
        SocketAddrV4::new(GATEWAY_IP, 22),
        SocketAddrV4::new(GATEWAY_IP, 80),
        SocketAddrV4::new(GATEWAY_IP, 443),
        SocketAddrV4::new(GATEWAY_IP, 8080),
        // The cloud metadata server.
        SocketAddrV4::new(Ipv4Addr::new(169, 254, 169, 254), 80),
        // Ports on the proxy address that are not forwarded, such as SMTP.
        SocketAddrV4::new(PROXY_IP, 25),
        SocketAddrV4::new(PROXY_IP, 587),
        SocketAddrV4::new(PROXY_IP, 8080),
    ]
}

#[cfg(feature = "internet")]
mod internet {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::OnceLock;

    const READ_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long to wait for the guest to get an IPv4 address over DHCP.
    const NETWORK_READY_TIMEOUT: Duration = Duration::from_secs(180);
    /// Maximum connection attempts to a proxy port on `ConnectionRefused`.
    const PROXY_CONNECT_ATTEMPTS: usize = 8;

    /// Returns the domains of the wildcard (`*.domain`) entries in the
    /// allowlist.
    fn wildcard_domains() -> Vec<&'static str> {
        ALLOWLIST.lines().map(str::trim).filter_map(|l| l.strip_prefix("*.")).collect()
    }

    /// Waits until the guest has IPv4 connectivity to the proxy, which hands
    /// out the guest's address over DHCP.
    fn wait_for_network() {
        static READY: OnceLock<()> = OnceLock::new();
        READY.get_or_init(|| {
            let host = *allowed_hosts()
                .first()
                .expect("the emulator internet allowlist has no exact (non-wildcard) host names");
            let deadline = Instant::now() + NETWORK_READY_TIMEOUT;
            loop {
                match dns_query(DNS_SERVER, host, 1) {
                    Ok(_) => return,
                    Err(e) if Instant::now() < deadline => {
                        eprintln!("waiting for guest network: {e}");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    Err(e) => panic!("guest never reached the emulator network proxy: {e}"),
                }
            }
        });
    }

    fn assert_nxdomain(server: SocketAddrV4, name: &str) {
        let reply = dns_query(server, name, 1)
            .unwrap_or_else(|e| panic!("DNS query for {name} via {server} failed: {e}"));
        assert_eq!(reply.rcode, 3, "expected NXDOMAIN for {name} via {server}, got {reply:?}");
        assert!(reply.answers.is_empty(), "expected no answers for {name}, got {reply:?}");
    }

    fn assert_resolves_to_proxy(name: &str) {
        let reply =
            dns_query(DNS_SERVER, name, 1).unwrap_or_else(|e| panic!("DNS query for {name}: {e}"));
        assert_eq!(reply.rcode, 0, "expected NOERROR for {name}, got {reply:?}");
        assert_eq!(reply.answers, vec![PROXY_IP], "expected {name} to resolve to the proxy");
    }

    #[test]
    fn allowed_names_resolve_to_the_proxy() {
        wait_for_network();
        let hosts = allowed_hosts();
        assert!(!hosts.is_empty(), "the allowlist has no exact host names");
        for host in hosts {
            assert_resolves_to_proxy(host);
            assert_resolves_to_proxy(&host.to_uppercase());
        }
        for domain in wildcard_domains() {
            assert_resolves_to_proxy(&format!("probe.{domain}"));
        }
    }

    #[test]
    fn allowed_names_have_no_ipv6_answers() {
        wait_for_network();
        let host = allowed_hosts()[0];
        let reply = dns_query(DNS_SERVER, host, 28).expect("AAAA query failed");
        assert_eq!(reply.rcode, 0, "expected NOERROR for AAAA {host}, got {reply:?}");
        assert!(reply.answers.is_empty(), "expected no AAAA answers for {host}, got {reply:?}");
    }

    #[test]
    fn disallowed_names_are_nxdomain() {
        wait_for_network();
        let host = allowed_hosts()[0];
        let mut names = vec![
            "example.com".to_string(),
            "localhost.localdomain".to_string(),
            "metadata.google.internal".to_string(),
            format!("evil.{host}"),
            format!("{host}.example.com"),
            format!("evil{host}"),
        ];
        // The parent domain of an exact entry is not allowed.
        if let Some((_, parent)) = host.split_once('.') {
            if parent.contains('.') && !allowed_hosts().contains(&parent) {
                names.push(parent.to_string());
            }
        }
        // The domain of a wildcard entry is not allowed by the wildcard.
        for domain in wildcard_domains() {
            if !allowed_hosts().contains(&domain) {
                names.push(domain.to_string());
            }
        }
        for name in names {
            assert_nxdomain(DNS_SERVER, &name);
        }
    }

    #[test]
    fn dns_to_public_resolvers_is_filtered() {
        wait_for_network();
        // Queries addressed to public resolvers are answered by the proxy's
        // filter rather than reaching the internet.
        for server in [Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 1, 1, 1)] {
            assert_nxdomain(SocketAddrV4::new(server, 53), "example.com");
        }
    }

    /// Connects to `port` on `PROXY_IP`, retrying `ConnectionRefused` up to
    /// `PROXY_CONNECT_ATTEMPTS` times.
    ///
    /// When the proxy closes a connection first, SLiRP enters `TCPS_TIME_WAIT`
    /// (10s, `rcv_wnd` = 8192) while Netstack3 passively closes and immediately
    /// returns the guest's ephemeral port to its random allocator. If a new
    /// connection to the same proxy port picks that ephemeral port within ~33ms,
    /// Netstack3's RFC 6528 ISN (`elapsed_micros / 4 + hash(4-tuple)`) lands
    /// inside `[rcv_nxt, rcv_nxt + 8192]`. Legacy BSD SLiRP in AEMU only
    /// recycles `TCPS_TIME_WAIT` on `SYN` when `ti_seq > rcv_nxt + rcv_wnd`; an
    /// in-window `SYN` instead hits `tcp_drop` (deleting the `TCPS_TIME_WAIT`
    /// entry) and replies with `RST|ACK` (`ConnectionRefused`).
    ///
    /// Because each `RST|ACK` deletes that `TCPS_TIME_WAIT` entry in SLiRP,
    /// retrying immediately succeeds, whereas a genuinely closed port refuses
    /// every attempt in milliseconds.
    fn connect_to_proxy(port: u16) -> TcpStream {
        let addr = SocketAddrV4::new(PROXY_IP, port);
        let mut attempt = 1;
        loop {
            match connect(addr) {
                Ok(stream) => return stream,
                Err(e)
                    if e.kind() == ErrorKind::ConnectionRefused
                        && attempt < PROXY_CONNECT_ATTEMPTS =>
                {
                    attempt += 1;
                }
                Err(e) => panic!(
                    "failed to connect to the proxy on port {port} (attempt {attempt}/{PROXY_CONNECT_ATTEMPTS}): {e}"
                ),
            }
        }
    }

    /// Sends `request` to the proxy on `port` and returns its response.
    fn proxy_exchange(port: u16, request: &[u8]) -> Vec<u8> {
        let mut stream = connect_to_proxy(port);
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        // The proxy may close a disallowed connection before reading all of
        // the request.
        let _ = stream.write_all(request);
        let mut resp = Vec::new();
        match stream.read_to_end(&mut resp) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => panic!("reading from the proxy on port {port} failed: {e}"),
        }
        resp
    }

    #[test]
    fn http_proxy_rejects_disallowed_requests() {
        wait_for_network();
        let host = allowed_hosts()[0];
        for (request, want) in [
            (
                "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n".to_string(),
                "HTTP/1.1 403",
            ),
            (
                format!("GET / HTTP/1.1\r\nHost: evil.{host}\r\nConnection: close\r\n\r\n"),
                "HTTP/1.1 403",
            ),
            (
                "GET / HTTP/1.1\r\nHost: 10.0.2.2\r\nConnection: close\r\n\r\n".to_string(),
                "HTTP/1.1 403",
            ),
            (
                "GET / HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n\r\n".to_string(),
                "HTTP/1.1 403",
            ),
            (
                format!(
                    "GET http://example.com/ HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
                ),
                "HTTP/1.1 403",
            ),
            (format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}\r\n\r\n"), "HTTP/1.1 405"),
            (
                "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n".to_string(),
                "HTTP/1.1 405",
            ),
        ] {
            let resp = proxy_exchange(80, request.as_bytes());
            let resp = String::from_utf8_lossy(&resp);
            assert!(resp.starts_with(want), "request {request:?}: expected {want}, got {resp:?}");
        }
    }

    /// Builds a TLS 1.2 ClientHello record with the given SNI host name.
    fn client_hello(sni: &str) -> Vec<u8> {
        let name = sni.as_bytes();
        let mut server_name_list = vec![0x00];
        server_name_list.extend_from_slice(&(name.len() as u16).to_be_bytes());
        server_name_list.extend_from_slice(name);
        let mut sni_ext = (server_name_list.len() as u16).to_be_bytes().to_vec();
        sni_ext.extend_from_slice(&server_name_list);
        let mut extensions = vec![0x00, 0x00];
        extensions.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&sni_ext);

        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0; 32]); // Random.
        body.push(0); // Session ID.
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // Cipher suites.
        body.extend_from_slice(&[0x01, 0x00]); // Compression methods.
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let mut handshake = vec![0x01];
        handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);

        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    #[test]
    fn tls_proxy_closes_disallowed_connections() {
        wait_for_network();
        let host = allowed_hosts()[0];
        for port in [443, 5228] {
            for request in [
                client_hello("example.com"),
                client_hello(&format!("evil.{host}")),
                client_hello(&format!("{host}.example.com")),
                client_hello("127.0.0.1"),
                client_hello("10.0.2.2"),
                client_hello(""),
                b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec(),
            ] {
                let resp = proxy_exchange(port, &request);
                assert!(
                    resp.is_empty(),
                    "expected the proxy on port {port} to close a disallowed connection, got {} bytes",
                    resp.len()
                );
            }
        }
    }

    #[test]
    fn direct_connections_are_blocked() {
        wait_for_network();
        let reached = reachable(forbidden_destinations());
        assert!(reached.is_empty(), "guest connected directly to {reached:?}");
    }
}

#[cfg(not(feature = "internet"))]
mod no_internet {
    use super::*;

    #[test]
    fn guest_has_no_dns() {
        // Without emulator internet access, there is no proxy to answer DNS
        // queries.
        let host = allowed_hosts().first().copied().unwrap_or("example.com");
        if let Ok(reply) = dns_query(DNS_SERVER, host, 1) {
            panic!(
                "unexpected DNS reply without emulator internet access: rcode={} answers={:?}",
                reply.rcode, reply.answers
            );
        }
    }

    #[test]
    fn guest_has_no_ipv4_connectivity() {
        let mut destinations = forbidden_destinations();
        destinations.push(SocketAddrV4::new(PROXY_IP, 443));
        destinations.push(SocketAddrV4::new(PROXY_IP, 80));
        let reached = reachable(destinations);
        assert!(
            reached.is_empty(),
            "guest connected to {reached:?} without emulator internet access"
        );
    }
}
