use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream;
use http::header::{CONTENT_LENGTH, HOST, TRANSFER_ENCODING, USER_AGENT};
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{
    BoxFuture, CallContext, Client, DnsResolver, Exchange, Interceptor, Next, RequestBody,
    ResponseBody, RustlsTlsConnector, TcpConnector, TokioTcpConnector, WireError, WireErrorKind,
};
use openwire_core::BoxConnection;
use openwire_test::{
    collect_request_body, ok_text, spawn_http1, spawn_https_http1, RecordingEventListenerFactory,
    StaticDnsResolver,
};

#[tokio::test]
async fn basic_get_returns_body() {
    let server = spawn_http1(|_request| async move { ok_text("hello openwire") }).await;
    let client = Client::builder().build().expect("client");

    let request = Request::builder()
        .uri(server.http_url("/hello"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "hello openwire");
}

#[tokio::test]
async fn follows_redirects_for_get_requests() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        match request.uri().path() {
            "/redirect" => Response::builder()
                .status(StatusCode::FOUND)
                .header("location", "/final")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response"),
            _ => ok_text("redirect complete"),
        }
    })
    .await;

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .uri(server.http_url("/redirect"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "redirect complete");
}

#[tokio::test]
async fn cross_authority_redirect_drops_authorization_header() {
    let target = spawn_http1(|request: Request<Incoming>| async move {
        let auth = request
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("none")
            .to_owned();
        ok_text(auth)
    })
    .await;

    let target_url = format!("http://target.test:{}/done", target.addr().port());
    let source = spawn_http1(move |_request: Request<Incoming>| {
        let target_url = target_url.clone();
        async move {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", target_url)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response")
        }
    })
    .await;

    let resolver = HostMapResolver::new([
        ("source.test".to_owned(), source.addr()),
        ("target.test".to_owned(), target.addr()),
    ]);
    let client = Client::builder()
        .dns_resolver(resolver)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!("http://source.test:{}/start", source.addr().port()))
        .header(http::header::AUTHORIZATION, "Bearer secret")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "none");
}

#[tokio::test]
async fn custom_dns_routes_custom_host() {
    let server = spawn_http1(|_request| async move { ok_text("dns ok") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "dns ok");
}

#[tokio::test]
async fn shared_client_reuses_connection_pool_across_calls() {
    let server = spawn_http1(|_request| async move { ok_text("pooled") }).await;
    let client = Client::builder().build().expect("client");

    let request_one = Request::builder()
        .uri(server.http_url("/first"))
        .body(RequestBody::empty())
        .expect("request");
    let response_one = client.execute(request_one).await.expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let request_two = Request::builder()
        .uri(server.http_url("/second"))
        .body(RequestBody::empty())
        .expect("request");
    let response_two = client.execute(request_two).await.expect("response");
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_two.into_body().text().await.expect("body");

    assert_eq!(connection_one, connection_two);
}

#[tokio::test]
async fn interceptors_wrap_transport_in_expected_order() {
    let server = spawn_http1(|_request| async move { ok_text("ok") }).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let client = Client::builder()
        .application_interceptor(RecordingInterceptor::new("app", order.clone()))
        .network_interceptor(RecordingInterceptor::new("net", order.clone()))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(server.http_url("/"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().bytes().await.expect("body");

    let order = order.lock().expect("order").clone();
    assert_eq!(
        order,
        vec!["app:before", "net:before", "net:after", "app:after"]
    );
}

#[tokio::test]
async fn event_listener_observes_full_call_lifecycle() {
    let server = spawn_http1(|_request| async move { ok_text("event stream") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(server.http_url("/events"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().text().await.expect("body");

    let events = events.events().join("\n");
    assert!(events.contains("call_start GET"));
    assert!(events.contains("dns_start"));
    assert!(events.contains("response_headers_end 200 OK"));
    assert!(events.contains("response_body_end 12"));
    assert!(events.contains("connection_released"));
}

#[tokio::test]
async fn custom_root_tls_request_succeeds() {
    let server = spawn_https_http1(|_request| async move { ok_text("tls ok") }).await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");

    let client = Client::builder()
        .tls_connector(tls)
        .build()
        .expect("client");
    let request = Request::builder()
        .uri(format!("https://localhost:{}/secure", server.addr().port()))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "tls ok");
}

#[tokio::test]
async fn connect_failure_is_classified() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(addr))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!("http://refused.test:{}/", addr.port()))
        .body(RequestBody::empty())
        .expect("request");

    let error = client.execute(request).await.expect_err("should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
}

#[tokio::test]
async fn retries_replayable_requests_on_connection_failure() {
    let server = spawn_http1(|_request| async move { ok_text("retry ok") }).await;
    let connector = FailingTcpConnector::new(1);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/retry-once",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "retry ok");
    assert_eq!(connector.attempts(), 2);

    let events = events.events();
    assert!(
        events.iter().any(|event| event == "retry 1 connect"),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn retry_exhaustion_returns_connect_error() {
    let server = spawn_http1(|_request| async move { ok_text("never reached") }).await;
    let connector = FailingTcpConnector::new(2);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/retry-exhausted",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let error = client.execute(request).await.expect_err("should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(connector.attempts(), 2);

    let retry_events = events
        .events()
        .into_iter()
        .filter(|event| event.starts_with("retry "))
        .count();
    assert_eq!(retry_events, 1);
}

#[tokio::test]
async fn streaming_request_bodies_are_not_retried_on_connection_failure() {
    let server = spawn_http1(|_request| async move { ok_text("never reached") }).await;
    let connector = FailingTcpConnector::new(1);
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/streaming-no-retry",
            server.addr().port()
        ))
        .body(RequestBody::from_stream(stream::iter(vec![Ok::<
            Bytes,
            WireError,
        >(
            Bytes::from_static(b"streaming body"),
        )])))
        .expect("request");

    let error = client.execute(request).await.expect_err("should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(connector.attempts(), 1);
}

#[tokio::test]
async fn redirect_count_remains_independent_from_retry_count() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        match request.uri().path() {
            "/redirect-after-retry" => Response::builder()
                .status(StatusCode::FOUND)
                .header("location", "/redirect-target")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response"),
            _ => ok_text("redirect after retry"),
        }
    })
    .await;

    let connector = FailingTcpConnector::new(1);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector)
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/redirect-after-retry",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "redirect after retry");

    let events = events.events();
    assert!(
        events.iter().any(|event| event == "retry 1 connect"),
        "events = {events:?}",
    );
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("redirect 1 http://openwire.test:")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn bridge_interceptor_normalizes_empty_requests_before_network_interceptors() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("normalized")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/empty",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "normalized");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        content_length: Some("0".to_owned()),
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: String::new(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_preserves_user_agent_and_sets_fixed_body_content_length() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("fixed")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/fixed",
            server.addr().port()
        ))
        .header(USER_AGENT, "custom-agent/1.0")
        .header(CONTENT_LENGTH, "999")
        .body(RequestBody::from_static(b"hello"))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "fixed");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some("custom-agent/1.0".to_owned()),
        content_length: Some("5".to_owned()),
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: "hello".to_owned(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_streaming_body_uses_chunked_without_content_length() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("streaming")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let stream = stream::iter(vec![
        Ok::<Bytes, WireError>(Bytes::from_static(b"hello ")),
        Ok::<Bytes, WireError>(Bytes::from_static(b"stream")),
    ]);
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/stream",
            server.addr().port()
        ))
        .header(CONTENT_LENGTH, "999")
        .header(TRANSFER_ENCODING, "identity")
        .body(RequestBody::from_stream(stream))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "streaming");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        content_length: None,
        transfer_encoding: Some("chunked".to_owned()),
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: "hello stream".to_owned(),
        }
    );
}

#[derive(Clone)]
struct RecordingInterceptor {
    label: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingInterceptor {
    fn new(label: &'static str, order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self { label, order }
    }
}

impl Interceptor for RecordingInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let label = self.label;
        let order = self.order.clone();
        Box::pin(async move {
            order.lock().expect("order").push(match label {
                "app" => "app:before",
                _ => "net:before",
            });
            let response = next.run(exchange).await;
            order.lock().expect("order").push(match label {
                "app" => "app:after",
                _ => "net:after",
            });
            response
        })
    }
}

#[derive(Clone, Default)]
struct HeaderCaptureInterceptor {
    seen: Arc<Mutex<Vec<ObservedHeaders>>>,
}

impl HeaderCaptureInterceptor {
    fn take_single(&self) -> ObservedHeaders {
        let seen = self.seen.lock().expect("captured headers");
        assert_eq!(seen.len(), 1, "expected exactly one captured request");
        seen[0].clone()
    }
}

impl Interceptor for HeaderCaptureInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let seen = self.seen.clone();
        let headers = ObservedHeaders::capture(exchange.request().headers());
        Box::pin(async move {
            seen.lock().expect("captured headers").push(headers);
            next.run(exchange).await
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedHeaders {
    host: Option<String>,
    user_agent: Option<String>,
    content_length: Option<String>,
    transfer_encoding: Option<String>,
}

impl ObservedHeaders {
    fn capture(headers: &http::HeaderMap) -> Self {
        Self {
            host: header_value(headers, HOST),
            user_agent: header_value(headers, USER_AGENT),
            content_length: header_value(headers, CONTENT_LENGTH),
            transfer_encoding: header_value(headers, TRANSFER_ENCODING),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedServerRequest {
    headers: ObservedHeaders,
    body: String,
}

fn take_observed_server_request(
    observed: &Arc<Mutex<Option<ObservedServerRequest>>>,
) -> ObservedServerRequest {
    observed
        .lock()
        .expect("observed server request")
        .clone()
        .expect("observed server request")
}

fn header_value(headers: &http::HeaderMap, name: http::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn default_user_agent() -> &'static str {
    concat!("openwire/", env!("CARGO_PKG_VERSION"))
}

#[derive(Clone)]
struct FailingTcpConnector {
    failures_remaining: Arc<Mutex<usize>>,
    attempts: Arc<Mutex<usize>>,
}

impl FailingTcpConnector {
    fn new(failures: usize) -> Self {
        Self {
            failures_remaining: Arc::new(Mutex::new(failures)),
            attempts: Arc::new(Mutex::new(0)),
        }
    }

    fn attempts(&self) -> usize {
        *self.attempts.lock().expect("connector attempts")
    }
}

impl TcpConnector for FailingTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let failures_remaining = self.failures_remaining.clone();
        let attempts = self.attempts.clone();
        Box::pin(async move {
            *attempts.lock().expect("connector attempts") += 1;

            let should_fail = {
                let mut failures_remaining = failures_remaining
                    .lock()
                    .expect("remaining connector failures");
                if *failures_remaining == 0 {
                    false
                } else {
                    *failures_remaining -= 1;
                    true
                }
            };

            if should_fail {
                ctx.listener().connect_start(&ctx, addr);
                return Err(WireError::connect(
                    "scripted connect failure",
                    io::Error::new(io::ErrorKind::ConnectionRefused, "scripted connect failure"),
                ));
            }

            TokioTcpConnector.connect(ctx, addr, timeout).await
        })
    }
}

#[derive(Clone)]
struct HostMapResolver {
    map: Arc<HashMap<String, SocketAddr>>,
}

impl HostMapResolver {
    fn new(entries: impl IntoIterator<Item = (String, SocketAddr)>) -> Self {
        Self {
            map: Arc::new(entries.into_iter().collect()),
        }
    }
}

impl DnsResolver for HostMapResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let map = self.map.clone();
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            let mut addr = map.get(&host).copied().ok_or_else(|| {
                WireError::dns(
                    "host not found in resolver map",
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing host"),
                )
            })?;
            addr.set_port(port);
            ctx.listener().dns_end(&ctx, &host, &[addr]);
            Ok(vec![addr])
        })
    }
}
