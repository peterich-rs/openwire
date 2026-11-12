use std::net::SocketAddr;

use http::{Method, StatusCode};
use openwire_test::{MockResponse, MockWebServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn enqueue_two_responses_are_served_in_order() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().body("first").close_connection());
    server.enqueue(MockResponse::new().body("second").close_connection());

    let first = http1_get(server.addr(), "/one").await;
    let second = http1_get(server.addr(), "/two").await;

    assert!(
        first.starts_with("HTTP/1.1 200 OK"),
        "first status line missing: {first}"
    );
    assert!(first.contains("first"), "first body missing: {first}");
    assert!(
        second.starts_with("HTTP/1.1 200 OK"),
        "second status line missing: {second}"
    );
    assert!(second.contains("second"), "second body missing: {second}");
}

#[tokio::test]
async fn take_request_sees_method_path_headers_body() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().body("ok").close_connection());

    let _response = http1_post(
        server.addr(),
        "/v1/echo",
        "x-openwire: phase-1",
        b"hello mock",
    )
    .await;

    let recorded = server.take_request().await;
    assert_eq!(recorded.method, Method::POST);
    assert_eq!(recorded.uri.path(), "/v1/echo");
    assert_eq!(
        recorded
            .headers
            .get("x-openwire")
            .and_then(|value| value.to_str().ok()),
        Some("phase-1")
    );
    assert_eq!(recorded.body.as_ref(), b"hello mock");
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn https_http1_exposes_tls_url_and_serves_enqueued_response() {
    let server = MockWebServer::https_http1().await;
    assert!(server.tls_root_pem().is_some());
    assert!(
        server.url("/secure").starts_with("https://"),
        "url = {}",
        server.url("/secure")
    );
    server.enqueue(MockResponse::new().status(StatusCode::NO_CONTENT));
    assert_eq!(server.request_count(), 0);
}

#[tokio::test]
async fn empty_queue_returns_500_and_is_visible_in_request_count() {
    let server = MockWebServer::http1().await;
    let response = http1_get(server.addr(), "/missing").await;

    assert!(
        response.starts_with("HTTP/1.1 500"),
        "expected 500, got {response}"
    );
    assert!(response.contains("mock queue empty"), "{response}");
    assert_eq!(server.request_count(), 1);
    assert_eq!(server.underflow_count(), 1);
    let recorded = server.take_request().await;
    assert_eq!(recorded.uri.path(), "/missing");
}

async fn http1_get(addr: SocketAddr, path: &str) -> String {
    http1_exchange(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await
}

async fn http1_post(addr: SocketAddr, path: &str, extra_header: &str, body: &[u8]) -> String {
    http1_exchange(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n{extra_header}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).expect("utf8 body"),
        ),
    )
    .await
}

async fn http1_exchange(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect mock server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.expect("read response");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(headers_end) = find_headers_end(&buf) {
            if response_body_complete(&buf, headers_end) {
                break;
            }
        }
    }
    String::from_utf8(buf).unwrap_or_else(|error| {
        panic!(
            "mock response was not utf8: {error}; bytes={:?}",
            error.as_bytes()
        )
    })
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn response_body_complete(buf: &[u8], headers_end: usize) -> bool {
    let headers = std::str::from_utf8(&buf[..headers_end]).unwrap_or("");
    let body = &buf[headers_end + 4..];
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return body.windows(5).any(|window| window == b"0\r\n\r\n");
    }
    if let Some(length) = content_length(headers) {
        return body.len() >= length;
    }
    headers.to_ascii_lowercase().contains("connection: close") && !body.is_empty()
        || headers.starts_with("HTTP/1.1 204")
        || headers.starts_with("HTTP/1.1 304")
}

fn content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}
