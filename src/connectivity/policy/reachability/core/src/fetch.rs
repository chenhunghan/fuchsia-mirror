// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fuchsia_async::TimeoutExt;
use fuchsia_async::net::TcpStream;

use futures::io::BufReader;
use futures::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, TryFutureExt};
use std::net;

const FETCH_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Maximum number of bytes to read when parsing the HTTP status line.
/// Standard HTTP status lines are ~20-40 bytes; 1024 bytes provides generous
/// headroom while preventing unbounded memory consumption.
const STATUS_LINE_MAX_BYTES: u64 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("failed to create socket")]
    CreateSocket(#[source] std::io::Error),
    #[error("failed to bind socket to device {interface_name}")]
    BindSocket {
        interface_name: String,
        #[source]
        err: std::io::Error,
    },
    #[error("failed to open TCP stream")]
    ConnectTcpStream(#[source] std::io::Error),
    #[error("timed out connecting TCP stream")]
    ConnectTcpStreamTimeout,
    #[error("failed to write to TCP stream")]
    WriteTcpStream(#[source] std::io::Error),
    #[error("timed out writing data to TCP stream")]
    WriteTcpStreamTimeout,
    #[error("failed to read response from TCP stream")]
    ReadTcpStream(#[source] std::io::Error),
    #[error("timed out reading response from TCP stream")]
    ReadTcpStreamTimeout,
    #[error("failed to parse string from UTF-8 bytes")]
    ParseUtf8(#[from] std::str::Utf8Error),
    #[error("response header malformed: {first_line}")]
    MalformedHeader { first_line: String },
    #[error("failed to parse status code")]
    ParseStatusCode(#[source] std::num::ParseIntError),
}

impl From<std::string::FromUtf8Error> for FetchError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Self::ParseUtf8(err.utf8_error())
    }
}

impl FetchError {
    /// Returns a short, simplified string describing the error.
    /// Each string should only take at most 14 characters, else the row labels for the
    /// `fetch_results` time series in internal visualization tool would be truncated.
    pub fn short_name(&self) -> String {
        let (name, os_err) = match self {
            Self::CreateSocket(err) => ("CreateSock", err.raw_os_error()),
            Self::BindSocket { err, .. } => ("BindSock", err.raw_os_error()),
            Self::ConnectTcpStream(err) => ("ConnectTcp", err.raw_os_error()),
            Self::ConnectTcpStreamTimeout => return "ConnTcpTimeout".to_string(),
            Self::WriteTcpStream(err) => ("WriteTcp", err.raw_os_error()),
            Self::WriteTcpStreamTimeout => return "WriteTcpTOut".to_string(),
            Self::ReadTcpStream(err) => ("ReadTcp", err.raw_os_error()),
            Self::ReadTcpStreamTimeout => return "ReadTcpTimeout".to_string(),
            Self::ParseUtf8(_) => return "ParseUtf8".to_string(),
            Self::MalformedHeader { .. } => return "BadHeader".to_string(),
            Self::ParseStatusCode(_) => return "ParseStatus".to_string(),
        };

        if let Some(code) = os_err { format!("{name}_{code}") } else { name.to_string() }
    }
}

pub(crate) fn fetch_result_short_name(result: &Result<u16, FetchError>) -> String {
    match result {
        Ok(code) => format!("Completed_{}", code),
        Err(e) => format!("e_{}", e.short_name()),
    }
}

fn http_request(path: &str, host: &str) -> String {
    [
        &format!("HEAD {path} HTTP/1.1"),
        &format!("host: {host}"),
        "connection: close",
        "user-agent: fuchsia reachability probe",
    ]
    .join("\r\n")
        + "\r\n\r\n"
}

async fn fetch<FA: FetchAddr + std::marker::Sync>(
    interface_name: &str,
    host: &str,
    path: &str,
    addr: &FA,
) -> Result<u16, FetchError> {
    let timeout = zx::MonotonicInstant::after(FETCH_TIMEOUT);
    let addr = addr.as_socket_addr();
    let socket = socket2::Socket::new(
        match addr {
            net::SocketAddr::V4(_) => socket2::Domain::IPV4,
            net::SocketAddr::V6(_) => socket2::Domain::IPV6,
        },
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .map_err(FetchError::CreateSocket)?;
    socket.bind_device(Some(interface_name.as_bytes())).map_err(|err| FetchError::BindSocket {
        interface_name: interface_name.to_string(),
        err,
    })?;
    let mut stream = TcpStream::connect_from_raw(socket, addr)
        .map_err(FetchError::ConnectTcpStream)?
        .map_err(FetchError::ConnectTcpStream)
        .on_timeout(timeout, || Err(FetchError::ConnectTcpStreamTimeout))
        .await?;
    let message = http_request(path, host);
    stream
        .write_all(message.as_bytes())
        .map_err(FetchError::WriteTcpStream)
        .on_timeout(timeout, || Err(FetchError::WriteTcpStreamTimeout))
        .await?;

    let mut bytes = Vec::new();
    // Wrap stream in BufReader and take at most STATUS_LINE_MAX_BYTES.
    // The stream is dropped at the end of this statement, closing the TCP connection.
    let _: usize = BufReader::new(stream)
        .take(STATUS_LINE_MAX_BYTES)
        .read_until(b'\n', &mut bytes)
        .map_err(FetchError::ReadTcpStream)
        .on_timeout(timeout, || Err(FetchError::ReadTcpStreamTimeout))
        .await?;
    // A missing LF means the status line was cut short by EOF or STATUS_LINE_MAX_BYTES.
    // Per RFC 9112 section 2.2, accept a bare LF and ignore a preceding CR.
    let Some(line) = bytes.strip_suffix(b"\n") else {
        return Err(FetchError::MalformedHeader {
            first_line: String::from_utf8_lossy(&bytes).into_owned(),
        });
    };
    let first_line = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line))?;
    if let [http, code, ..] = first_line.split(' ').collect::<Vec<_>>().as_slice() {
        if !http.starts_with("HTTP/") {
            return Err(FetchError::MalformedHeader { first_line: first_line.to_string() });
        }
        Ok(code.parse().map_err(FetchError::ParseStatusCode)?)
    } else {
        Err(FetchError::MalformedHeader { first_line: first_line.to_string() })
    }
}

pub trait FetchAddr {
    fn as_socket_addr(&self) -> net::SocketAddr;
}

impl FetchAddr for net::SocketAddr {
    fn as_socket_addr(&self) -> net::SocketAddr {
        *self
    }
}

impl FetchAddr for net::IpAddr {
    fn as_socket_addr(&self) -> net::SocketAddr {
        net::SocketAddr::from((*self, 80))
    }
}

#[async_trait]
pub trait Fetch {
    async fn fetch<FA: FetchAddr + std::marker::Sync>(
        &self,
        interface_name: &str,
        host: &str,
        path: &str,
        addr: &FA,
    ) -> Result<u16, FetchError>;
}

pub struct Fetcher;

#[async_trait]
impl Fetch for Fetcher {
    async fn fetch<FA: FetchAddr + std::marker::Sync>(
        &self,
        interface_name: &str,
        host: &str,
        path: &str,
        addr: &FA,
    ) -> Result<u16, FetchError> {
        fetch(interface_name, host, path, addr).await
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use anyhow::Context;
    use assert_matches::assert_matches;
    use std::net::{Ipv4Addr, SocketAddr};

    use fuchsia_async::net::TcpListener;
    use futures::StreamExt;
    use test_case::test_case;

    fn server_with_data(
        data: Vec<u8>,
    ) -> anyhow::Result<(SocketAddr, impl futures::Future<Output = Vec<String>>)> {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        let listener = TcpListener::bind(&addr).context("binding TCP")?;
        let addr = listener.local_addr()?;

        let server_fut = async move {
            let timeout = zx::MonotonicInstant::after(FETCH_TIMEOUT);
            let mut incoming = listener.accept_stream();
            if let Some(result) = incoming
                .next()
                .on_timeout(timeout, || panic!("timeout waiting for connection"))
                .await
            {
                let (stream, _addr) = result.expect("accept incoming TCP connection");
                let mut stream = BufReader::new(stream);
                let mut request = Vec::new();
                loop {
                    let mut s = String::new();
                    let _: usize = stream
                        .read_line(&mut s)
                        .on_timeout(timeout, || panic!("timeout waiting to read data"))
                        .await
                        .expect("read data");
                    if s == "\r\n" {
                        break;
                    }
                    request.push(s.trim().to_string());
                }
                let mut stream = stream.into_inner();
                if !data.is_empty() {
                    stream
                        .write_all(&data)
                        .on_timeout(timeout, || panic!("timeout waiting to write response"))
                        .await
                        .expect("reply to request");
                }
                request
            } else {
                Vec::new()
            }
        };

        Ok((addr, server_fut))
    }

    #[derive(Clone, Copy, Debug)]
    enum HttpStatus {
        Ok,
        NoContent,
        BadRequest,
        NotFound,
    }

    impl HttpStatus {
        const fn code_and_reason(self) -> (u16, &'static str) {
            match self {
                Self::Ok => (200, "OK"),
                Self::NoContent => (204, "No Content"),
                Self::BadRequest => (400, "Bad Request"),
                Self::NotFound => (404, "Not Found"),
            }
        }

        const fn code(self) -> u16 {
            let (code, _reason) = self.code_and_reason();
            code
        }

        fn response_bytes(self) -> Vec<u8> {
            let (code, reason) = self.code_and_reason();
            format!("HTTP/1.1 {code} {reason}\r\n\r\n").into_bytes()
        }
    }

    fn server(
        status: HttpStatus,
    ) -> anyhow::Result<(SocketAddr, impl futures::Future<Output = Vec<String>>)> {
        server_with_data(status.response_bytes())
    }

    #[test_case("http://reachability.test/", HttpStatus::Ok; "base path 200")]
    #[test_case("http://reachability.test/path/", HttpStatus::Ok; "sub path 200")]
    #[test_case("http://reachability.test/", HttpStatus::NoContent; "base path 204")]
    #[test_case("http://reachability.test/", HttpStatus::BadRequest; "base path 400")]
    #[test_case("http://reachability.test/path/", HttpStatus::BadRequest; "sub path 400")]
    #[test_case("http://reachability.test/", HttpStatus::NotFound; "base path 404")]
    #[fuchsia::test]
    async fn test_fetch(url_str: &'static str, status: HttpStatus) -> anyhow::Result<()> {
        let url = url::Url::parse(url_str)?;
        let (addr, server_fut) = server(status)?;
        let domain = url.host().expect("no host").to_string();
        let path = url.path().to_string();

        let (result, request) = futures::join!(fetch("", &domain, &path, &addr), server_fut);

        assert_eq!(result.expect("fetch failed"), status.code());
        assert!(request.contains(&format!("HEAD {path} HTTP/1.1")));
        assert!(request.contains(&format!("host: {domain}")));

        Ok(())
    }

    #[fuchsia::test]
    async fn test_fetch_large_body_connection_held_open() -> anyhow::Result<()> {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        let listener = TcpListener::bind(&addr).context("binding TCP")?;
        let addr = listener.local_addr()?;

        let server_fut = async move {
            let timeout = zx::MonotonicInstant::after(FETCH_TIMEOUT);
            let mut incoming = listener.accept_stream();
            let (stream, _addr) = incoming
                .next()
                .on_timeout(timeout, || panic!("timeout waiting for connection"))
                .await
                .expect("connection stream")
                .expect("accept incoming TCP connection");
            let mut stream = BufReader::new(stream);
            let mut request = Vec::new();
            loop {
                let mut s = String::new();
                let _: usize = stream
                    .read_line(&mut s)
                    .on_timeout(timeout, || panic!("timeout waiting to read data"))
                    .await
                    .expect("read data");
                if s == "\r\n" {
                    break;
                }
                request.push(s.trim().to_string());
            }
            let mut stream = stream.into_inner();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 1000000\r\n\r\n")
                .on_timeout(timeout, || panic!("timeout waiting to write response"))
                .await
                .expect("reply to request");

            let large_body = vec![b'a'; 64 * 1024];
            let _ = stream.write_all(&large_body).await;

            // Hold the connection open until the client drops its stream.
            let mut buf = [0u8; 1];
            match stream
                .read(&mut buf)
                .on_timeout(timeout, || panic!("timeout waiting for client to close"))
                .await
            {
                Ok(0) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                    ) => {}
                res => panic!("unexpected result waiting for client close: {res:?}"),
            }

            request
        };

        let (result, request) =
            futures::join!(fetch("", "reachability.test", "/", &addr), server_fut);

        assert_eq!(result.expect("fetch failed"), 204);
        assert!(request.contains(&"HEAD / HTTP/1.1".to_string()));
        assert!(request.contains(&"host: reachability.test".to_string()));

        Ok(())
    }

    #[test_case(b"HTTP/1.1 204 No Content\r\n", 204; "crlf")]
    #[test_case(b"HTTP/1.1 204 No Content\n", 204; "bare-lf")]
    #[fuchsia::test]
    async fn test_fetch_status_line_formats(data: &[u8], expected_code: u16) -> anyhow::Result<()> {
        let (addr, server_fut) = server_with_data(data.to_vec())?;
        let (result, _) = futures::join!(fetch("", "reachability.test", "/", &addr), server_fut);
        assert_eq!(result.expect("fetch failed"), expected_code);
        Ok(())
    }

    #[test_case(b""; "empty")]
    #[test_case(b"HTTP/1.1 200 OK"; "missing-newline")]
    #[test_case(b"INVALID/1.1 200 OK\r\n"; "invalid-protocol")]
    #[fuchsia::test]
    async fn test_fetch_malformed_header(data: &[u8]) -> anyhow::Result<()> {
        let (addr, server_fut) = server_with_data(data.to_vec())?;
        let (result, _) = futures::join!(fetch("", "reachability.test", "/", &addr), server_fut);
        assert_matches!(result, Err(FetchError::MalformedHeader { .. }));
        Ok(())
    }

    #[test_case(STATUS_LINE_MAX_BYTES, Ok(204); "at-max-bytes")]
    #[test_case(STATUS_LINE_MAX_BYTES + 1, Err(()); "one-byte-over-max")]
    #[test_case(STATUS_LINE_MAX_BYTES * 2, Err(()); "oversized")]
    #[fuchsia::test]
    async fn test_fetch_status_line_length_limit(
        total_bytes: u64,
        expected: Result<u16, ()>,
    ) -> anyhow::Result<()> {
        let prefix = "HTTP/1.1 204 ";
        let suffix = "\r\n";
        let fill = (total_bytes as usize) - prefix.len() - suffix.len();
        let data = format!("{prefix}{}{suffix}", "A".repeat(fill)).into_bytes();

        let (addr, server_fut) = server_with_data(data)?;
        let (result, _) = futures::join!(fetch("", "reachability.test", "/", &addr), server_fut);
        match expected {
            Ok(code) => assert_eq!(result.expect("fetch failed"), code),
            Err(()) => {
                assert_matches!(result, Err(FetchError::MalformedHeader { .. }));
            }
        }
        Ok(())
    }
}
