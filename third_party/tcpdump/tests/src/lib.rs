// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use fdio::{SpawnAction, SpawnOptions};
use fidl_fuchsia_posix_socket as fposix_socket;
use fidl_fuchsia_posix_socket_packet as fposix_socket_packet;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_runtime::{HandleInfo, HandleType, duplicate_utc_clock_handle, job_default};
use futures::future;
use futures::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use futures::stream::StreamExt as _;
use libc::{STDERR_FILENO, STDOUT_FILENO};
use net_declare::std_socket_addr;
use netemul::RealmUdpSocket as _;
use netstack_testing_common::realms::{Netstack3, TestSandboxExt as _};
use netstack_testing_macros::netstack_test;
use regex::Regex;
use std::convert::TryInto as _;
use std::ffi::{CStr, CString};
use zx::ProcessInfo;

const BINARY_PATH: &str = "/pkg/bin/tcpdump";

/// Returns true iff the patterns were found, false if the stream ended.
async fn wait_for_pattern<SO: AsyncBufReadExt + Unpin, SE: AsyncReadExt + Unpin>(
    reader: &mut SO,
    other_reader: &mut SE,
    mut patterns: Vec<Regex>,
) {
    while !patterns.is_empty() {
        let mut line = String::new();
        let read_bytes = reader.read_line(&mut line).await.expect("read_line");

        if read_bytes == 0 {
            let mut buf = String::new();
            let _read_bytes: usize =
                other_reader.read_to_string(&mut buf).await.expect("read other reader");
            panic!(
                "failed to match all patterns from reader; patterns = {:?}\nOTHER READER:\n{}",
                patterns, buf
            )
        }

        println!("GOT LINE FROM READER: {}", line);

        // Trim the trailing new line.
        let line = &line[..line.len() - 1];
        let () = patterns.retain(|pattern| !pattern.is_match(&line));
    }
}

fn start_tcpdump(
    args: impl IntoIterator<Item = &'static str>,
    mut spawn_actions: Vec<SpawnAction<'_>>,
) -> (zx::Process, zx::Socket, zx::Socket) {
    let (stdout_reader, stdout_writer) = zx::Socket::create_stream();
    let (stderr_reader, stderr_writer) = zx::Socket::create_stream();

    // The reader-ends should not write.
    let () = stdout_writer.half_close().expect("stdout_reader.half_close");
    let () = stdout_writer.half_close().expect("stderr_reader.half_close");

    let path = CString::new(BINARY_PATH).expect("cstring path");
    let path = path.as_c_str();

    let args: Vec<CString> = std::iter::once(BINARY_PATH)
        .chain(args.into_iter())
        .map(|a| {
            CString::new(a).unwrap_or_else(|e| panic!("failed to parse {} to CString: {}", a, e))
        })
        .collect();
    let args: Vec<&CStr> = args.iter().map(|s| s.as_c_str()).collect();

    // Provide the socket that TCPDump should use for stdout.
    spawn_actions.push(SpawnAction::add_handle(
        HandleInfo::new(
            HandleType::FileDescriptor,
            STDOUT_FILENO.try_into().expect("STDOUT_FILENO.try_into"),
        ),
        stdout_writer.into(),
    ));
    // Provide the socket that TCPDump should use for stderr.
    spawn_actions.push(SpawnAction::add_handle(
        HandleInfo::new(
            HandleType::FileDescriptor,
            STDERR_FILENO.try_into().expect("STDERR_FILENO.try_into"),
        ),
        stderr_writer.into(),
    ));

    let process = fdio::spawn_etc(
        &job_default(),
        SpawnOptions::DEFAULT_LOADER,
        path,
        &args[..],
        None,
        &mut spawn_actions,
    )
    .expect("spawn tcpdump");

    (process, stdout_reader, stderr_reader)
}


async fn start_tcpdump_and_wait_for_patterns<
    Fut: future::Future<Output = ()>,
    F: FnOnce() -> Fut,
>(
    realm: &netemul::TestRealm<'_>,
    args: impl IntoIterator<Item = &'static str>,
    bind_addr: std::net::SocketAddr,
    inject_packet: F,
    patterns: Vec<Regex>,
) {
    let (svc_client_end, svc_server_end) = fidl::endpoints::create_endpoints();
    let (process, stdout_reader, stderr_reader) = start_tcpdump(
        args,
        vec![
            SpawnAction::add_namespace_entry(
                CString::new("/svc").expect("CString /svc").as_c_str(),
                svc_client_end.into_channel().into(),
            ),
            SpawnAction::add_handle(
                HandleInfo::new(HandleType::ClockUtc, 0),
                duplicate_utc_clock_handle(
                    zx::Rights::READ | zx::Rights::WAIT | zx::Rights::TRANSFER,
                )
                .expect("duplicate utc clock handle")
                .into_handle(),
            ),
        ],
    );
    let mut stdout_reader = BufReader::new(fasync::Socket::from_socket(stdout_reader));
    let mut stderr_reader = BufReader::new(fasync::Socket::from_socket(stderr_reader));

    let mut svcfs = ServiceFs::new_local();
    let svcfs = svcfs
        .add_service_connector::<_, fposix_socket::ProviderMarker>(|server_end| {
            realm
                .connect_to_protocol_with_server_end(server_end)
                .expect("connect to regular socket provider")
        })
        .add_service_connector::<_, fposix_socket_packet::ProviderMarker>(|server_end| {
            realm
                .connect_to_protocol_with_server_end(server_end)
                .expect("connect to packet socket provider")
        })
        .serve_connection(svc_server_end)
        .expect("servicefs serve connection")
        .collect::<()>();

    // Wait for TCPDump to start.
    let svcfs = {
        let wait_for_pattern_fut = wait_for_pattern(
            &mut stderr_reader,
            &mut stdout_reader,
            vec![Regex::new(r"listening on any, link-type LINUX_SLL2 \(Linux cooked v2\), snapshot length \d+ bytes").expect("parse tcpdump listening regex")],
        );
        futures::pin_mut!(wait_for_pattern_fut);
        match future::select(wait_for_pattern_fut, svcfs).await {
            future::Either::Left(((), svcfs)) => svcfs,
            future::Either::Right(((), _wait_for_pattern_fut)) => {
                panic!("service directory unexpectedly ended")
            }
        }
    };

    // Send a UDP packet and make sure TCPDump logs it.
    let sock = fuchsia_async::net::UdpSocket::bind_in_realm(&realm, bind_addr)
        .await
        .expect("create socket");
    let addr = sock.local_addr().expect("get bound socket address");
    const PAYLOAD: [u8; 4] = [1, 2, 3, 4];
    let sent = sock.send_to(&PAYLOAD[..], addr).await.expect("send_to failed");
    assert_eq!(sent, PAYLOAD.len());

    inject_packet().await;

    {
        let wait_for_pattern_fut =
            wait_for_pattern(&mut stdout_reader, &mut stderr_reader, patterns);
        futures::pin_mut!(wait_for_pattern_fut);
        match future::select(wait_for_pattern_fut, svcfs).await {
            future::Either::Left(((), _svcfs)) => {}
            future::Either::Right(((), _wait_for_pattern_fut)) => {
                panic!("service directory unexpectedly ended")
            }
        }
    }

    assert_eq!(
        fasync::OnSignals::new(&process, zx::Signals::PROCESS_TERMINATED)
            .await
            .expect("wait for process termination"),
        zx::Signals::PROCESS_TERMINATED
    );
    let ProcessInfo { return_code, .. } = process.info().expect("process info");
    assert_eq!(return_code, 0);
}

#[fuchsia::test]
async fn version_test() {
    let (process, stdout_reader, stderr_reader) = start_tcpdump(["--version"], Vec::new());

    assert_eq!(
        fasync::OnSignals::new(&process, zx::Signals::PROCESS_TERMINATED)
            .await
            .expect("wait for process termination"),
        zx::Signals::PROCESS_TERMINATED
    );
    let ProcessInfo { return_code, .. } = process.info().expect("process info");
    assert_eq!(return_code, 0);

    let mut stdout_reader = BufReader::new(fasync::Socket::from_socket(stdout_reader));
    let mut stderr_reader = fasync::Socket::from_socket(stderr_reader);

    wait_for_pattern(
        &mut stdout_reader,
        &mut stderr_reader,
        vec![
            Regex::new(r"tcpdump version ").expect("parse tcpdump version regex"),
            Regex::new(r"libpcap version ").expect("parse libpcap version regex"),
        ],
    )
    .await
}

#[netstack_test]
// TODO(https://fxbug.dev/42169332): Fix memory leak and run this with Lsan.
#[cfg_attr(feature = "variant_asan", ignore)]
// TODO(https://fxbug.dev/436867782): Fix memory leak and run this with HWASan.
#[cfg_attr(feature = "variant_hwasan", ignore)]
async fn packet_test(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    start_tcpdump_and_wait_for_patterns(
        &realm,
        ["-c", "1", "--no-promiscuous-mode"],
        std_socket_addr!("127.0.0.1:9875"),
        || futures::future::ready(()),
        vec![
            Regex::new(r"lo\s+In\s+IP 127\.0\.0\.1\.9875 > 127\.0\.0\.1\.9875: UDP, length 4")
                .expect("parse tcpdump packet regex"),
        ],
    )
    .await
}