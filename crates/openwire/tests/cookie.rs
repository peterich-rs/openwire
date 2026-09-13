//! Full-stack cookie cases from RFC 6265, not from Jar internals.

mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use http::header::COOKIE;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{BoxFuture, CallContext, Client, DnsResolver, Jar, RequestBody, Url, WireError};
use openwire_test::{ok_text, spawn_http1, MockResponse, MockWebServer, StaticDnsResolver};

use support::*;

struct LoopbackDns;

impl DnsResolver for LoopbackDns {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        Box::pin(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            ctx.listener().dns_start(&ctx, &host, port);
            ctx.listener().dns_end(&ctx, &host, &[addr]);
            Ok(vec![addr])
        })
    }
}

fn get(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .method(Method::GET)
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

#[tokio::test]
async fn two_set_cookie_headers_are_both_stored() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .header("Set-Cookie", "a=1; Path=/")
            .header("Set-Cookie", "b=2; Path=/")
            .body("set"),
    );
    server.enqueue(MockResponse::new().body("next"));

    let jar = Arc::new(Jar::new());
    let client = Client::builder().cookie_jar(jar).build().expect("client");

    let first = client.execute(get(server.url("/set"))).await.expect("set");
    let _ = first.into_body().text().await.expect("body");
    let second = client
        .execute(get(server.url("/next")))
        .await
        .expect("next");
    let _ = second.into_body().text().await.expect("body");

    let _ = server.take_request().await;
    let replay = server.take_request().await;
    let cookie = replay
        .headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(cookie.contains("a=1"), "missing a=1 in {cookie}");
    assert!(cookie.contains("b=2"), "missing b=2 in {cookie}");
}

#[tokio::test]
async fn redirect_set_cookie_then_cross_origin_does_not_forward_cookie() {
    let origin = MockWebServer::http1().await;
    let target = MockWebServer::http1().await;
    let origin_url = format!("http://origin.test:{}/start", origin.addr().port());
    let target_url = format!("http://target.test:{}/next", target.addr().port());
    origin.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Set-Cookie", "session=origin; Path=/")
            .header("Location", &target_url),
    );
    target.enqueue(MockResponse::new().body("ok"));

    let jar = Arc::new(Jar::new());
    let client = Client::builder()
        .cookie_jar(jar)
        .dns_resolver(LoopbackDns)
        .build()
        .expect("client");
    let response = client.execute(get(origin_url)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let second = target.take_request().await;
    assert!(
        second.headers.get(COOKIE).is_none(),
        "RFC 6265 host-only cookie for origin.test must not be sent to target.test: {:?}",
        second.headers.get(COOKIE)
    );
}

#[tokio::test]
async fn explicit_cookie_header_skips_jar_injection() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().body("ok"));

    let jar = Arc::new(Jar::new());
    jar.add_cookie_str("jar=1; Path=/", &Url::parse(&server.url("/")).expect("url"));
    let client = Client::builder().cookie_jar(jar).build().expect("client");

    let mut request = get(server.url("/"));
    request
        .headers_mut()
        .insert(COOKIE, "manual=1".parse().expect("cookie"));
    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().text().await.expect("body");

    let recorded = server.take_request().await;
    assert_eq!(
        recorded
            .headers
            .get(COOKIE)
            .and_then(|value| value.to_str().ok()),
        Some("manual=1")
    );
}

#[tokio::test]
async fn cookie_jar_sends_preloaded_cookies() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let cookie = request
            .headers()
            .get(COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("none")
            .to_owned();
        ok_text(cookie)
    })
    .await;

    let jar = Jar::default();
    jar.add_cookie_str(
        "session=preloaded; Path=/",
        &format!("http://openwire.test:{}/", server.addr().port())
            .parse::<Url>()
            .expect("url"),
    );

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .cookie_jar(jar)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/cookies",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "session=preloaded");
}

#[tokio::test]
async fn redirect_response_cookies_are_applied_to_follow_up_requests() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        match request.uri().path() {
            "/start" => Response::builder()
                .status(StatusCode::FOUND)
                .header("location", "/finish")
                .header("set-cookie", "session=redirected; Path=/")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response"),
            "/finish" => {
                let cookie = request
                    .headers()
                    .get(COOKIE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("none")
                    .to_owned();
                ok_text(cookie)
            }
            _ => ok_text("unexpected"),
        }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .cookie_jar(Jar::default())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/start",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "session=redirected");
}

#[tokio::test]
async fn redirect_response_cookies_are_ignored_without_cookie_jar() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        match request.uri().path() {
            "/start" => Response::builder()
                .status(StatusCode::FOUND)
                .header("location", "/finish")
                .header("set-cookie", "session=redirected; Path=/")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response"),
            "/finish" => {
                let cookie = request
                    .headers()
                    .get(COOKIE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("none")
                    .to_owned();
                ok_text(cookie)
            }
            _ => ok_text("unexpected"),
        }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/start",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "none");
}

#[tokio::test]
async fn explicit_cookie_header_skips_cookie_jar_injection() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let cookie = request
            .headers()
            .get(COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("none")
            .to_owned();
        ok_text(cookie)
    })
    .await;

    let jar = Jar::default();
    jar.add_cookie_str(
        "session=jar; Path=/",
        &format!("http://openwire.test:{}/", server.addr().port())
            .parse::<Url>()
            .expect("url"),
    );

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .cookie_jar(jar)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/manual",
            server.addr().port()
        ))
        .header(COOKIE, "manual=1")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "manual=1");
}
