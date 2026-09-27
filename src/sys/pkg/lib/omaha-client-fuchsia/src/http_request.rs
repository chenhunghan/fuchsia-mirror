// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_mem as fmem;
use fidl_fuchsia_net_http as fnet_http;
use fuchsia_async::{self as fasync, TimeoutExt as _};
use futures::future::BoxFuture;
use futures::prelude::*;
use omaha_client::http_request::{Body, Error, HttpRequest, Request, Response};
use std::time::Duration;

const MAX_RESPONSE_BODY_SIZE: usize = 1 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
enum LoaderRequestError {
    #[error("failed to connect to fuchsia.net.http.Loader protocol")]
    ConnectToProtocol(#[source] anyhow::Error),
    #[error("failed to create VMO for request body")]
    CreateVmo(#[source] zx::Status),
    #[error("failed to write request body to VMO")]
    WriteVmo(#[source] zx::Status),
    #[error("FIDL error calling Loader.Fetch")]
    Fidl(#[source] fidl::Error),
    #[error("Loader returned network error: {0:?}")]
    Loader(fnet_http::Error),
    #[error("Loader response missing status code")]
    MissingStatusCode,
    #[error("invalid HTTP status code: {0}")]
    InvalidStatusCode(u32),
    #[error("invalid HTTP header name")]
    InvalidHeaderName(#[source] http::header::InvalidHeaderName),
    #[error("invalid HTTP header value")]
    InvalidHeaderValue(#[source] http::header::InvalidHeaderValue),
    #[error("failed to read response body from socket")]
    ReadBodySocket(#[source] std::io::Error),
    #[error("response body exceeds maximum allowed size of {MAX_RESPONSE_BODY_SIZE} bytes")]
    ResponseBodyTooLarge,
}

impl From<LoaderRequestError> for Error {
    fn from(e: LoaderRequestError) -> Self {
        Error::new_transport(e)
    }
}

pub struct FuchsiaHttpRequest {
    timeout: Duration,
}

impl HttpRequest for FuchsiaHttpRequest {
    fn request(&mut self, req: Request<Body>) -> BoxFuture<'_, Result<Response<Vec<u8>>, Error>> {
        let timeout = self.timeout;

        make_request(req, timeout).on_timeout(timeout, || Err(Error::new_timeout())).boxed()
    }
}

async fn make_request(req: Request<Body>, timeout: Duration) -> Result<Response<Vec<u8>>, Error> {
    let loader = fuchsia_component::client::connect_to_protocol::<fnet_http::LoaderMarker>()
        .map_err(LoaderRequestError::ConnectToProtocol)?;

    let (parts, body) = req.into_parts();

    let fidl_body = if let Some(body_bytes) = body.into_inner() {
        let size = body_bytes.len() as u64;
        let vmo = zx::Vmo::create(size).map_err(LoaderRequestError::CreateVmo)?;
        vmo.write(&body_bytes, 0).map_err(LoaderRequestError::WriteVmo)?;
        Some(fnet_http::Body::Buffer(fmem::Buffer { vmo, size }))
    } else {
        None
    };

    let headers: Vec<fnet_http::Header> = parts
        .headers
        .iter()
        .map(|(name, value)| fnet_http::Header {
            name: name.as_str().as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();

    let deadline = fasync::MonotonicInstant::after(timeout.into()).into_nanos();
    let fidl_req = fnet_http::Request {
        method: Some(parts.method.as_str().to_string()),
        url: Some(parts.uri.to_string()),
        headers: if headers.is_empty() { None } else { Some(headers) },
        body: fidl_body,
        deadline: Some(deadline),
        ..Default::default()
    };

    let fidl_resp = loader.fetch(fidl_req).await.map_err(LoaderRequestError::Fidl)?;

    if let Some(err) = fidl_resp.error {
        return match err {
            fnet_http::Error::DeadlineExceeded => Err(Error::new_timeout()),
            other => Err(LoaderRequestError::Loader(other).into()),
        };
    }

    let status_code = fidl_resp.status_code.ok_or(LoaderRequestError::MissingStatusCode)?;
    let status = u16::try_from(status_code)
        .ok()
        .and_then(|code| http::StatusCode::from_u16(code).ok())
        .ok_or(LoaderRequestError::InvalidStatusCode(status_code))?;

    let mut resp_body = Vec::new();
    if let Some(zx_socket) = fidl_resp.body {
        let socket = fasync::Socket::from_socket(zx_socket);
        socket
            .take(MAX_RESPONSE_BODY_SIZE as u64 + 1)
            .read_to_end(&mut resp_body)
            .await
            .map_err(LoaderRequestError::ReadBodySocket)?;
        if resp_body.len() > MAX_RESPONSE_BODY_SIZE {
            return Err(LoaderRequestError::ResponseBodyTooLarge.into());
        }
    }

    let mut response = Response::new(resp_body);
    *response.status_mut() = status;
    if let Some(headers) = fidl_resp.headers {
        for fnet_http::Header { name, value } in headers {
            let header_name = http::header::HeaderName::from_bytes(&name)
                .map_err(LoaderRequestError::InvalidHeaderName)?;
            let header_value = http::header::HeaderValue::from_maybe_shared(value)
                .map_err(LoaderRequestError::InvalidHeaderValue)?;
            response.headers_mut().append(header_name, header_value);
        }
    }

    Ok(response)
}

impl FuchsiaHttpRequest {
    /// Construct a new client that uses a default timeout.
    pub fn new() -> Self {
        Self::using_timeout(Duration::from_secs(30))
    }

    /// Construct a new client which always uses the provided duration instead of the default.
    pub fn using_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for FuchsiaHttpRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_hyper_test_support::TestServer;
    use fuchsia_hyper_test_support::fault_injection::{Hang, HangBody};
    use fuchsia_hyper_test_support::handler::StaticResponse;

    /// Helper that constructs a Request for a given path on the given test server.
    fn make_request_for(server: &TestServer, path: &str) -> Request<Body> {
        Request::builder().uri(server.local_url_for_path(path)).body(Body::default()).unwrap()
    }

    /// Test that the HttpRequest implementation works against a simple server and returns
    /// the expected response body.
    #[fuchsia::test]
    async fn test_simple_request() {
        let server =
            TestServer::builder().handler(StaticResponse::ok_body("some data")).start().await;
        let mut client = FuchsiaHttpRequest::using_timeout(Duration::from_secs(5));
        let response = client.request(make_request_for(&server, "some/path")).await.unwrap();
        let string = String::from_utf8(response.into_body()).unwrap();
        assert_eq!(string, "some data");
    }

    /// Test that the HttpRequest implementation properly times out if the server doesn't return
    /// a response over the socket after accepting the connection.
    #[fuchsia::test]
    async fn test_hang() {
        let server = TestServer::builder().handler(Hang).start().await;
        let mut client = FuchsiaHttpRequest::using_timeout(Duration::from_secs(1));
        let response = client.request(make_request_for(&server, "some/path")).await;
        assert!(response.unwrap_err().is_timeout());
    }

    /// Test that the HttpRequest implementation properly times out if the server doesn't return
    /// a the entire body that's expected (after returning a response header).
    #[fuchsia::test]
    async fn test_hang_body() {
        let server = TestServer::builder().handler(HangBody::content_length(500)).start().await;
        let mut client = FuchsiaHttpRequest::using_timeout(Duration::from_secs(1));
        let response = client.request(make_request_for(&server, "some/path")).await;
        assert!(response.unwrap_err().is_timeout());
    }

    /// Test that POST requests with body and headers preserve headers, status code, and body
    /// even for non-200 HTTP status codes.
    #[fuchsia::test]
    async fn test_post_and_headers_with_non_200_status() {
        struct CustomPostHandler;
        impl fuchsia_hyper_test_support::Handler for CustomPostHandler {
            fn handles(
                &self,
                request: &hyper::Request<hyper::body::Incoming>,
            ) -> Option<BoxFuture<'_, hyper::Response<fuchsia_hyper_test_support::Body>>>
            {
                if request.method() == hyper::Method::POST && request.uri().path() == "/update" {
                    assert_eq!(request.headers().get("x-custom-req").unwrap(), "req-val");
                    assert_eq!(
                        request.headers().get("content-length").unwrap(),
                        "{\"request\":\"ping\"}".len().to_string().as_str()
                    );
                    let resp = hyper::Response::builder()
                        .status(hyper::StatusCode::SERVICE_UNAVAILABLE)
                        .header("x-retry-after", "3600")
                        .header("etag", "sig:hash")
                        .body(b"error body".to_vec().into())
                        .unwrap();
                    Some(futures::future::ready(resp).boxed())
                } else {
                    None
                }
            }
        }

        let server = TestServer::builder().handler(CustomPostHandler).start().await;
        let mut client = FuchsiaHttpRequest::using_timeout(Duration::from_secs(5));
        let req = Request::builder()
            .method(http::Method::POST)
            .uri(server.local_url_for_path("update"))
            .header("x-custom-req", "req-val")
            .body(omaha_client::http_request::body_from("{\"request\":\"ping\"}"))
            .unwrap();

        let resp = client.request(req).await.unwrap();
        assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("x-retry-after").unwrap(), "3600");
        assert_eq!(resp.headers().get("etag").unwrap(), "sig:hash");
        assert_eq!(resp.into_body(), b"error body");
    }

    #[fuchsia::test]
    async fn test_response_body_too_large() {
        use std::error::Error as _;

        let server = TestServer::builder()
            .handler(StaticResponse::ok_body(vec![b'a'; MAX_RESPONSE_BODY_SIZE + 1]))
            .start()
            .await;
        let mut client = FuchsiaHttpRequest::using_timeout(Duration::from_secs(5));
        let response = client.request(make_request_for(&server, "some/path")).await;
        let err = response.unwrap_err();
        std::assert_matches!(
            err.source().and_then(|s| s.downcast_ref::<LoaderRequestError>()),
            Some(LoaderRequestError::ResponseBodyTooLarge)
        );
    }
}
