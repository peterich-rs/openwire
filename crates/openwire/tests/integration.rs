use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{
    BoxFuture, CallContext, Client, DnsResolver, Exchange, Interceptor, Next, RequestBody,
    ResponseBody, RustlsTlsConnector, WireError, WireErrorKind,
};
use openwire_test::{
    ok_text, spawn_http1, spawn_https_http1, RecordingEventListenerFactory, StaticDnsResolver,
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
