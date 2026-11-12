use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue, CONNECTION};
use http::{HeaderMap, Method, Request, StatusCode, Uri, Version};
use hyper::body::Incoming;
use tokio::sync::Notify;

use crate::{
    collect_request_body, spawn_http1, spawn_https_http1, spawn_https_http2, text_response,
    TestResponse, TestServer,
};

/// Scriptable local HTTP server: enqueue responses, then inspect recorded requests.
///
/// One instance per test. Bind address is ephemeral (`127.0.0.1:0`).
pub struct MockWebServer {
    server: TestServer,
    state: Arc<MockState>,
    tls: bool,
}

struct MockState {
    inner: Mutex<MockInner>,
    recorded: Notify,
}

struct MockInner {
    responses: VecDeque<MockResponse>,
    recorded: VecDeque<RecordedRequest>,
    received: usize,
    underflow: usize,
}

/// Canned response served in enqueue order.
#[derive(Clone, Debug)]
pub struct MockResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    delay: Option<Duration>,
    close_connection: bool,
}

/// One request the mock server observed.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl MockResponse {
    pub fn new() -> Self {
        Self {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            delay: None,
            close_connection: false,
        }
    }

    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        let name = HeaderName::from_bytes(name.as_ref().as_bytes()).expect("header name");
        let value = HeaderValue::from_str(value.as_ref()).expect("header value");
        self.headers.append(name, value);
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    pub fn close_connection(mut self) -> Self {
        self.close_connection = true;
        self
    }
}

impl Default for MockResponse {
    fn default() -> Self {
        Self::new()
    }
}

impl MockWebServer {
    pub async fn http1() -> Self {
        Self::spawn(MockKind::Http1).await
    }

    pub async fn https_http1() -> Self {
        Self::spawn(MockKind::HttpsHttp1).await
    }

    pub async fn https_http2() -> Self {
        Self::spawn(MockKind::HttpsHttp2).await
    }

    pub fn enqueue(&self, response: MockResponse) {
        self.state
            .inner
            .lock()
            .expect("mock queue lock")
            .responses
            .push_back(response);
    }

    pub fn url(&self, path: &str) -> String {
        if self.tls {
            self.server.https_url(path)
        } else {
            self.server.http_url(path)
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    pub fn tls_root_pem(&self) -> Option<&str> {
        self.server.tls_root_pem()
    }

    /// Wait up to 5s for the next recorded request.
    pub async fn take_request(&self) -> RecordedRequest {
        const TIMEOUT: Duration = Duration::from_secs(5);
        let started = tokio::time::Instant::now();
        loop {
            if let Some(request) = self.take_request_nowait() {
                return request;
            }
            let remaining = TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                self.panic_take_timeout();
            }
            match tokio::time::timeout(remaining, self.state.recorded.notified()).await {
                Ok(()) => {}
                Err(_) => self.panic_take_timeout(),
            }
        }
    }

    pub fn take_request_nowait(&self) -> Option<RecordedRequest> {
        self.state
            .inner
            .lock()
            .expect("mock queue lock")
            .recorded
            .pop_front()
    }

    pub fn request_count(&self) -> usize {
        self.state.inner.lock().expect("mock queue lock").received
    }

    pub fn underflow_count(&self) -> usize {
        self.state.inner.lock().expect("mock queue lock").underflow
    }

    fn panic_take_timeout(&self) -> ! {
        let inner = self.state.inner.lock().expect("mock queue lock");
        panic!(
            "timed out waiting for mock request (received={}, queued_responses={}, underflow={})",
            inner.received,
            inner.responses.len(),
            inner.underflow
        );
    }

    async fn spawn(kind: MockKind) -> Self {
        let state = Arc::new(MockState {
            inner: Mutex::new(MockInner {
                responses: VecDeque::new(),
                recorded: VecDeque::new(),
                received: 0,
                underflow: 0,
            }),
            recorded: Notify::new(),
        });
        let handler_state = state.clone();
        let handler = move |request: Request<Incoming>| {
            let handler_state = handler_state.clone();
            async move { serve_mock_request(handler_state, request).await }
        };

        let (server, tls) = match kind {
            MockKind::Http1 => (spawn_http1(handler).await, false),
            MockKind::HttpsHttp1 => (spawn_https_http1(handler).await, true),
            MockKind::HttpsHttp2 => (spawn_https_http2(handler).await, true),
        };

        Self { server, state, tls }
    }
}

enum MockKind {
    Http1,
    HttpsHttp1,
    HttpsHttp2,
}

async fn serve_mock_request(state: Arc<MockState>, request: Request<Incoming>) -> TestResponse {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let version = request.version();
    let headers = request.headers().clone();
    let body = collect_request_body(request).await;

    let canned = {
        let mut inner = state.inner.lock().expect("mock queue lock");
        inner.received += 1;
        inner.recorded.push_back(RecordedRequest {
            method,
            uri,
            version,
            headers,
            body,
        });
        match inner.responses.pop_front() {
            Some(response) => response,
            None => {
                inner.underflow += 1;
                MockResponse::new()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("mock queue empty")
            }
        }
    };
    state.recorded.notify_one();

    if let Some(delay) = canned.delay {
        tokio::time::sleep(delay).await;
    }

    let mut response = text_response(canned.status, canned.body);
    for (name, value) in canned.headers.iter() {
        response.headers_mut().append(name.clone(), value.clone());
    }
    if canned.close_connection {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}
