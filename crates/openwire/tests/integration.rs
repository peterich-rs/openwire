use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, COOKIE, HOST, TRANSFER_ENCODING, USER_AGENT};
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{
    AuthContext, Authenticator, BoxFuture, CallContext, Client, DnsResolver, Exchange, Interceptor,
    Jar, Next, NoProxy, Proxy, RequestBody, ResponseBody, RustlsTlsConnector, TcpConnector,
    TokioTcpConnector, Url, WireError, WireErrorKind,
};
use openwire_core::BoxConnection;
use openwire_test::{
    collect_request_body, ok_text, spawn_http1, spawn_https_http1, RecordingEventListenerFactory,
    StaticDnsResolver,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

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
async fn client_call_timeout_applies_to_requests() {
    let server = spawn_http1(|_request| async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        ok_text("slow ok")
    })
    .await;

    let client = Client::builder()
        .call_timeout(std::time::Duration::from_millis(10))
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(server.http_url("/slow")))
        .await
        .expect_err("default timeout should fail");
    assert_eq!(error.kind(), WireErrorKind::Timeout);
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
async fn authenticator_retries_replayable_requests_on_401() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if auth == Some("Bearer good") {
            ok_text("authorized")
        } else {
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("www-authenticate", "Bearer realm=\"openwire\"")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("unauthorized response")
        }
    })
    .await;

    let authenticator = StaticAuthorizationAuthenticator::new("Bearer good");
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .authenticator(authenticator.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/auth",
            server.addr().port()
        )))
        .await
        .expect("response");

    assert_eq!(authenticator.calls(), 1);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "authorized");
}

#[tokio::test]
async fn https_requests_can_tunnel_through_http_proxy() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxied tls ok") }).await;
    let proxy = spawn_connect_proxy().await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .tls_connector(tls)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!("https://localhost:{}/secure", server.addr().port()))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied tls ok");
    assert!(
        proxy
            .requests()
            .iter()
            .any(|request| request.starts_with(&format!(
                "CONNECT localhost:{} HTTP/1.1",
                server.addr().port()
            ))),
        "requests = {:?}",
        proxy.requests(),
    );
}

#[tokio::test]
async fn https_proxy_connect_can_retry_tunnel_after_407_with_proxy_authenticator() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxied tls auth ok") }).await;
    let proxy = spawn_connect_proxy_requiring_authorization(
        "Proxy-Authorization",
        "Basic cHJveHk6c2VjcmV0",
    )
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let authenticator =
        StaticHeaderAuthenticator::new("proxy-authorization", "Basic cHJveHk6c2VjcmV0");

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .proxy_authenticator(authenticator.clone())
        .tls_connector(tls)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://localhost:{}/secure-auth",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied tls auth ok");
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(
        authenticator.observed_kinds(),
        vec![openwire::AuthKind::Proxy]
    );

    let requests = proxy.requests();
    assert_eq!(requests.len(), 2, "requests = {requests:?}");
    assert!(
        requests[0].starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1",
            server.addr().port()
        )),
        "requests = {requests:?}",
    );
    assert!(
        !requests[0].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}",
    );
    assert!(
        requests[1].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}",
    );
}

#[tokio::test]
async fn declining_proxy_authenticator_fails_connect_tunnel_on_407() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxied tls auth ok") }).await;
    let proxy = spawn_connect_proxy_requiring_authorization(
        "Proxy-Authorization",
        "Basic cHJveHk6c2VjcmV0",
    )
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let authenticator = DecliningAuthenticator::default();

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .proxy_authenticator(authenticator.clone())
        .tls_connector(tls)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://localhost:{}/secure-auth",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("connect auth should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(proxy.requests().len(), 1);
    assert!(
        error
            .to_string()
            .contains("407 Proxy Authentication Required"),
        "error = {error:?}",
    );
}

#[tokio::test]
async fn connect_proxy_auth_attempts_are_limited() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxied tls auth ok") }).await;
    let proxy = spawn_connect_proxy_requiring_authorization(
        "Proxy-Authorization",
        "Basic cHJveHk6c2VjcmV0",
    )
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let authenticator = StaticHeaderAuthenticator::new("proxy-authorization", "Basic wrong");

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .proxy_authenticator(authenticator.clone())
        .max_auth_attempts(1)
        .tls_connector(tls)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://localhost:{}/secure-auth",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("connect auth should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(proxy.requests().len(), 2);
    assert!(
        error
            .to_string()
            .contains("407 Proxy Authentication Required"),
        "error = {error:?}",
    );
}

#[tokio::test]
async fn connect_timeout_applies_to_proxy_connect_response_reads() {
    let proxy = spawn_stalling_connect_proxy().await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .connect_timeout(Duration::from_millis(25))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("https://localhost:443/slow-connect")
        .body(RequestBody::empty())
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("proxy CONNECT read should time out");
    assert_eq!(error.kind(), WireErrorKind::Timeout);
    assert!(error.is_connect_timeout(), "error = {error:?}");
}

#[tokio::test]
async fn http_requests_can_route_through_http_proxy_without_origin_dns() {
    let proxy = spawn_plain_http_proxy_response("proxied http ok").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/resource?x=1")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied http ok");
    assert!(
        proxy
            .requests()
            .iter()
            .any(|request| request
                .starts_with("GET http://does-not-resolve.test/resource?x=1 HTTP/1.1")),
        "requests = {:?}",
        proxy.requests(),
    );
}

#[tokio::test]
async fn no_proxy_exact_host_bypasses_proxy() {
    let server = spawn_http1(|_request| async move { ok_text("direct exact") }).await;
    let proxy = spawn_plain_http_proxy_response("proxied http ok").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy.test".to_owned(), proxy.addr()),
            ("direct.test".to_owned(), server.addr()),
        ]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().host("direct.test")),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://direct.test:{}/resource",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "direct exact");
    assert!(
        proxy.requests().is_empty(),
        "requests = {:?}",
        proxy.requests()
    );
}

#[tokio::test]
async fn no_proxy_domain_suffix_bypasses_proxy() {
    let server = spawn_http1(|_request| async move { ok_text("direct suffix") }).await;
    let proxy = spawn_plain_http_proxy_response("proxied http ok").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy.test".to_owned(), proxy.addr()),
            ("api.internal.test".to_owned(), server.addr()),
        ]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().domain("internal.test")),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://api.internal.test:{}/resource",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "direct suffix");
    assert!(
        proxy.requests().is_empty(),
        "requests = {:?}",
        proxy.requests()
    );
}

#[tokio::test]
async fn no_proxy_localhost_bypasses_proxy() {
    let server = spawn_http1(|_request| async move { ok_text("direct loopback") }).await;
    let proxy = spawn_plain_http_proxy_response("proxied http ok").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy.test".to_owned(), proxy.addr()),
            ("127.0.0.1".to_owned(), server.addr()),
        ]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().localhost()),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(server.http_url("/loopback"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "direct loopback");
    assert!(
        proxy.requests().is_empty(),
        "requests = {:?}",
        proxy.requests()
    );
}

#[tokio::test]
async fn system_http_proxy_from_env_is_applied() {
    let _guard = environment_lock().lock().await;
    let proxy = spawn_plain_http_proxy_response("env proxy").await;
    let _env = ScopedEnv::set([
        (
            "http_proxy",
            format!("http://127.0.0.1:{}", proxy.addr().port()),
        ),
        ("HTTP_PROXY", String::new()),
        ("https_proxy", String::new()),
        ("HTTPS_PROXY", String::new()),
        ("all_proxy", String::new()),
        ("ALL_PROXY", String::new()),
        ("no_proxy", String::new()),
        ("NO_PROXY", String::new()),
    ]);

    let client = Client::builder()
        .use_system_proxy(true)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/from-env")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "env proxy");
    assert_eq!(proxy.requests().len(), 1);
}

#[tokio::test]
async fn system_no_proxy_from_env_bypasses_proxy() {
    let _guard = environment_lock().lock().await;
    let server = spawn_http1(|_request| async move { ok_text("env direct") }).await;
    let proxy = spawn_plain_http_proxy_response("env proxy").await;
    let _env = ScopedEnv::set([
        (
            "http_proxy",
            format!("http://proxy.test:{}", proxy.addr().port()),
        ),
        ("HTTP_PROXY", String::new()),
        ("https_proxy", String::new()),
        ("HTTPS_PROXY", String::new()),
        ("all_proxy", String::new()),
        ("ALL_PROXY", String::new()),
        ("no_proxy", "example.internal".to_owned()),
        ("NO_PROXY", String::new()),
    ]);

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy.test".to_owned(), proxy.addr()),
            ("api.example.internal".to_owned(), server.addr()),
        ]))
        .use_system_proxy(true)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://api.example.internal:{}/env-no-proxy",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "env direct");
    assert!(proxy.requests().is_empty());
}

#[tokio::test]
async fn explicit_proxy_rules_take_priority_over_system_proxy() {
    let _guard = environment_lock().lock().await;
    let explicit_proxy = spawn_plain_http_proxy_response("explicit").await;
    let env_proxy = spawn_plain_http_proxy_response("env").await;
    let _env = ScopedEnv::set([
        (
            "http_proxy",
            format!("http://env-proxy.test:{}", env_proxy.addr().port()),
        ),
        ("HTTP_PROXY", String::new()),
        ("https_proxy", String::new()),
        ("HTTPS_PROXY", String::new()),
        ("all_proxy", String::new()),
        ("ALL_PROXY", String::new()),
        ("no_proxy", String::new()),
        ("NO_PROXY", String::new()),
    ]);

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("explicit-proxy.test".to_owned(), explicit_proxy.addr()),
            ("env-proxy.test".to_owned(), env_proxy.addr()),
        ]))
        .proxy(
            Proxy::http(format!(
                "http://explicit-proxy.test:{}",
                explicit_proxy.addr().port()
            ))
            .expect("explicit proxy"),
        )
        .use_system_proxy(true)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/priority")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "explicit");
    assert_eq!(explicit_proxy.requests().len(), 1);
    assert!(env_proxy.requests().is_empty());
}

#[tokio::test]
async fn proxy_rules_use_first_matching_entry() {
    let first_proxy = spawn_plain_http_proxy_response("proxy one").await;
    let second_proxy = spawn_plain_http_proxy_response("proxy two").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy-one.test".to_owned(), first_proxy.addr()),
            ("proxy-two.test".to_owned(), second_proxy.addr()),
        ]))
        .proxy(
            Proxy::http(format!(
                "http://proxy-one.test:{}",
                first_proxy.addr().port()
            ))
            .expect("proxy one"),
        )
        .proxy(
            Proxy::all(format!(
                "http://proxy-two.test:{}",
                second_proxy.addr().port()
            ))
            .expect("proxy two"),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/ordered")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxy one");
    assert_eq!(first_proxy.requests().len(), 1);
    assert!(second_proxy.requests().is_empty());
}

#[tokio::test]
async fn http_proxy_can_retry_requests_after_407_with_proxy_authenticator() {
    let proxy =
        spawn_proxy_requiring_authorization("Proxy-Authorization", "Basic cHJveHk6c2VjcmV0").await;
    let authenticator =
        StaticHeaderAuthenticator::new("proxy-authorization", "Basic cHJveHk6c2VjcmV0");
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .proxy_authenticator(authenticator.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/proxy-auth")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxy authorized");
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(
        authenticator.observed_kinds(),
        vec![openwire::AuthKind::Proxy]
    );
}

#[tokio::test]
async fn declining_proxy_authenticator_returns_407_response() {
    let proxy =
        spawn_proxy_requiring_authorization("Proxy-Authorization", "Basic cHJveHk6c2VjcmV0").await;
    let authenticator = DecliningAuthenticator::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .proxy_authenticator(authenticator.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/proxy-auth-decline")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(response.status(), StatusCode::PROXY_AUTHENTICATION_REQUIRED);
}

#[tokio::test]
async fn proxy_connect_failures_return_connect_errors() {
    let proxy = spawn_rejecting_connect_proxy().await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("https://localhost:443/secure")
        .body(RequestBody::empty())
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("proxy should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);
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

#[tokio::test]
async fn declining_authenticator_returns_401_response() {
    let server = spawn_http1(|_request: Request<Incoming>| async move {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Bearer realm=\"openwire\"")
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .expect("unauthorized response")
    })
    .await;

    let authenticator = DecliningAuthenticator::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .authenticator(authenticator.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/decline",
            server.addr().port()
        )))
        .await
        .expect("response");

    assert_eq!(authenticator.calls(), 1);
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
async fn tracing_attempt_spans_record_stable_connection_reuse_fields() {
    let server = spawn_http1(|_request| async move { ok_text("pooled") }).await;
    let trace = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let client = Client::builder().build().expect("client");

    let response = client
        .execute(empty_request(server.http_url("/first")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let response = client
        .execute(empty_request(server.http_url("/second")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let spans = trace.spans_named("openwire.attempt");
    assert_eq!(spans.len(), 2, "spans = {spans:?}");
    assert_eq!(
        spans[0].fields.get("attempt").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        spans[0].fields.get("retry_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        spans[0].fields.get("redirect_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        spans[0].fields.get("auth_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        spans[0].fields.get("connection_reused").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        spans[1].fields.get("attempt").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        spans[1].fields.get("auth_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        spans[1].fields.get("connection_reused").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        spans[0].fields.get("connection_id"),
        spans[1].fields.get("connection_id"),
        "spans = {spans:?}",
    );
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
async fn success_events_follow_stable_order() {
    let server = spawn_http1(|_request| async move { ok_text("ordered") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/ordered")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "call_start GET",
            "dns_start",
            "dns_end",
            "connect_end",
            "request_body_end 0",
            "connection_acquired ",
            "response_headers_start",
            "response_headers_end 200 OK",
            "call_end 200 OK",
            "response_body_end 7",
            "connection_released ",
        ],
    );
}

#[tokio::test]
async fn dropping_response_body_without_consuming_it_does_not_emit_response_body_end() {
    let server = spawn_http1(|_request| async move { ok_text("abandoned") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/abandoned")))
        .await
        .expect("response");
    drop(response);

    let events = events.events();
    assert_event_subsequence(&events, &["call_end 200 OK", "connection_released "]);
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_body_end ")),
        "events = {events:?}",
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_body_failed ")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn response_body_failures_do_not_emit_response_body_end_or_call_failed() {
    let server = spawn_raw_http1_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabc".to_vec(),
    )
    .await;
    let events = RecordingEventListenerFactory::default();
    let trace = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/broken",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let error = response
        .into_body()
        .text()
        .await
        .expect_err("body should fail");
    assert_eq!(error.kind(), WireErrorKind::Protocol);

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "call_start GET",
            "response_headers_end 200 OK",
            "call_end 200 OK",
            "response_body_failed protocol",
            "connection_released ",
        ],
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_body_end ")),
        "events = {events:?}",
    );
    assert!(
        !events.iter().any(|event| event.starts_with("call_failed ")),
        "events = {events:?}",
    );

    let body_failure = trace
        .event_by_message("response body failed")
        .expect("body failure trace event");
    assert_eq!(
        body_failure.fields.get("error_kind").map(String::as_str),
        Some("protocol")
    );
    assert_eq!(
        body_failure.fields.get("bytes_read").map(String::as_str),
        Some("3")
    );
    assert_eq!(
        body_failure.fields.get("attempt").map(String::as_str),
        Some("1")
    );
    assert!(
        body_failure.fields.contains_key("call_id"),
        "body failure event = {body_failure:?}",
    );
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
async fn retry_and_redirect_events_follow_stable_order_and_trace_fields() {
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
    let trace = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector)
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/redirect-after-retry",
            server.addr().port()
        )))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "call_start GET",
            "connect_failed ",
            "retry 1 connect",
            "connect_end ",
            "connection_acquired ",
            "response_headers_end 302 Found",
            "redirect 1 http://openwire.test:",
            "connection_released ",
            "connection_acquired ",
            "response_headers_end 200 OK",
            "call_end 200 OK",
            "response_body_end 20",
        ],
    );

    let retry_event = trace
        .event_by_message("retrying request after connection-establishment failure")
        .expect("retry trace event");
    assert_eq!(
        retry_event.fields.get("attempt").map(String::as_str),
        Some("2")
    );
    assert_eq!(
        retry_event.fields.get("retry_count").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        retry_event.fields.get("redirect_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        retry_event.fields.get("auth_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        retry_event.fields.get("retry_reason").map(String::as_str),
        Some("connect")
    );

    let redirect_event = trace
        .event_by_message("following redirect")
        .expect("redirect trace event");
    assert_eq!(
        redirect_event.fields.get("attempt").map(String::as_str),
        Some("3")
    );
    assert_eq!(
        redirect_event.fields.get("retry_count").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        redirect_event
            .fields
            .get("redirect_count")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        redirect_event.fields.get("auth_count").map(String::as_str),
        Some("0")
    );
    assert!(
        redirect_event
            .fields
            .get("redirect_location")
            .is_some_and(|location| location.contains("/redirect-target")),
        "redirect event = {redirect_event:?}",
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

#[tokio::test]
async fn streaming_request_bodies_are_not_authenticated_on_401() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let _ = collect_request_body(request).await;
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Bearer realm=\"openwire\"")
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .expect("unauthorized response")
    })
    .await;

    let authenticator = StaticAuthorizationAuthenticator::new("Bearer good");
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .authenticator(authenticator.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/stream-auth",
            server.addr().port()
        ))
        .body(RequestBody::from_stream(stream::iter(vec![Ok::<
            Bytes,
            WireError,
        >(
            Bytes::from_static(b"streaming auth body"),
        )])))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(authenticator.calls(), 0);
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_attempts_are_limited_and_counted_independently() {
    let server = spawn_http1(|_request: Request<Incoming>| async move {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Bearer realm=\"openwire\"")
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .expect("unauthorized response")
    })
    .await;

    let authenticator = StaticAuthorizationAuthenticator::new("Bearer wrong");
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .authenticator(authenticator.clone())
        .max_auth_attempts(2)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/loop-auth",
            server.addr().port()
        )))
        .await
        .expect("response");

    assert_eq!(authenticator.calls(), 2);
    assert_eq!(authenticator.observed_auth_counts(), vec![0, 1]);
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[derive(Clone)]
struct RecordingInterceptor {
    label: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone)]
struct StaticAuthorizationAuthenticator {
    header_value: &'static str,
    calls: Arc<AtomicUsize>,
    observed_auth_counts: Arc<Mutex<Vec<u32>>>,
}

impl StaticAuthorizationAuthenticator {
    fn new(header_value: &'static str) -> Self {
        Self {
            header_value,
            calls: Arc::new(AtomicUsize::new(0)),
            observed_auth_counts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn observed_auth_counts(&self) -> Vec<u32> {
        self.observed_auth_counts
            .lock()
            .expect("auth counts")
            .clone()
    }
}

impl Authenticator for StaticAuthorizationAuthenticator {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let header_value = self.header_value;
        let calls = self.calls.clone();
        let observed_auth_counts = self.observed_auth_counts.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            observed_auth_counts
                .lock()
                .expect("auth counts")
                .push(ctx.auth_count());
            let Some(mut request) = ctx.try_clone_request() else {
                return Ok(None);
            };
            request
                .headers_mut()
                .insert(AUTHORIZATION, http::HeaderValue::from_static(header_value));
            Ok(Some(request))
        })
    }
}

#[derive(Clone, Default)]
struct DecliningAuthenticator {
    calls: Arc<AtomicUsize>,
}

impl DecliningAuthenticator {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct StaticHeaderAuthenticator {
    header_name: &'static str,
    header_value: &'static str,
    calls: Arc<AtomicUsize>,
    observed_kinds: Arc<Mutex<Vec<openwire::AuthKind>>>,
}

impl StaticHeaderAuthenticator {
    fn new(header_name: &'static str, header_value: &'static str) -> Self {
        Self {
            header_name,
            header_value,
            calls: Arc::new(AtomicUsize::new(0)),
            observed_kinds: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn observed_kinds(&self) -> Vec<openwire::AuthKind> {
        self.observed_kinds.lock().expect("auth kinds").clone()
    }
}

impl Authenticator for StaticHeaderAuthenticator {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let header_name = self.header_name;
        let header_value = self.header_value;
        let calls = self.calls.clone();
        let observed_kinds = self.observed_kinds.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            observed_kinds.lock().expect("auth kinds").push(ctx.kind());
            let Some(mut request) = ctx.try_clone_request() else {
                return Ok(None);
            };
            request.headers_mut().insert(
                http::header::HeaderName::from_static(header_name),
                http::HeaderValue::from_static(header_value),
            );
            Ok(Some(request))
        })
    }
}

impl Authenticator for DecliningAuthenticator {
    fn authenticate(
        &self,
        _ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        })
    }
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

fn empty_request(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

fn default_user_agent() -> &'static str {
    concat!("openwire/", env!("CARGO_PKG_VERSION"))
}

fn environment_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

struct ScopedEnv {
    previous: Vec<(String, Option<String>)>,
}

impl ScopedEnv {
    fn set<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: AsRef<str>,
    {
        let mut previous = Vec::new();
        for (key, value) in vars {
            let key = key.into();
            let value = value.as_ref();
            previous.push((key.clone(), std::env::var(&key).ok()));
            if value.is_empty() {
                unsafe {
                    std::env::remove_var(&key);
                }
            } else {
                unsafe {
                    std::env::set_var(&key, value);
                }
            }
        }
        Self { previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            match value {
                Some(value) => unsafe {
                    std::env::set_var(&key, value);
                },
                None => unsafe {
                    std::env::remove_var(&key);
                },
            }
        }
    }
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

fn assert_event_subsequence(events: &[String], expected_prefixes: &[&str]) {
    let mut cursor = 0usize;
    for expected in expected_prefixes {
        while cursor < events.len() && !events[cursor].starts_with(expected) {
            cursor += 1;
        }
        assert!(
            cursor < events.len(),
            "missing event starting with {expected:?} in {events:?}",
        );
        cursor += 1;
    }
}

struct RawHttpServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RawHttpServer {
    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RawHttpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct ConnectProxyServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ConnectProxyServer {
    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for ConnectProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct AuthorizationProxyServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl AuthorizationProxyServer {
    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for AuthorizationProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct PlainHttpProxyServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl PlainHttpProxyServer {
    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for PlainHttpProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn spawn_raw_http1_response(response: Vec<u8>) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let mut buffer = [0u8; 1024];
                    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buffer)).await;
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

async fn spawn_connect_proxy() -> ConnectProxyServer {
    spawn_proxy_impl(true).await
}

async fn spawn_rejecting_connect_proxy() -> ConnectProxyServer {
    spawn_proxy_impl(false).await
}

async fn spawn_stalling_connect_proxy() -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request);
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

async fn spawn_connect_proxy_requiring_authorization(
    header_name: &'static str,
    expected_value: &'static str,
) -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());

                        let authorized = request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("{header_name}: {expected_value}"))
                        });

                        if !authorized {
                            client
                                .write_all(
                                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                )
                                .await
                                .expect("write proxy auth challenge");
                            let _ = client.shutdown().await;
                            return;
                        }

                        let target = request
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("CONNECT "))
                            .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                            .expect("CONNECT request line")
                            .to_owned();

                        let mut upstream = tokio::net::TcpStream::connect(&target)
                            .await
                            .expect("connect upstream through proxy");
                        client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await
                            .expect("write proxy response");

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

async fn spawn_plain_http_proxy_response(body: &'static str) -> PlainHttpProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind plain proxy listener");
    let addr = listener.local_addr().expect("plain proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request);

                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        client
                            .write_all(response.as_bytes())
                            .await
                            .expect("write plain proxy response");
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    PlainHttpProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

async fn spawn_proxy_requiring_authorization(
    header_name: &'static str,
    expected_value: &'static str,
) -> AuthorizationProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth proxy listener");
    let addr = listener.local_addr().expect("auth proxy listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        let authorized = request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("{header_name}: {expected_value}"))
                        });

                        let response = if authorized {
                            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nproxy authorized"
                        } else {
                            "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        };

                        client
                            .write_all(response.as_bytes())
                            .await
                            .expect("write auth proxy response");
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    AuthorizationProxyServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

async fn spawn_proxy_impl(accept_connect: bool) -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());

                        let target = request
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("CONNECT "))
                            .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                            .expect("CONNECT request line")
                            .to_owned();

                        if !accept_connect {
                            client
                                .write_all(
                                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
                                )
                                .await
                                .expect("write proxy rejection");
                            let _ = client.shutdown().await;
                            return;
                        }

                        let mut upstream = tokio::net::TcpStream::connect(&target)
                            .await
                            .expect("connect upstream through proxy");
                        client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await
                            .expect("write proxy response");

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

#[derive(Clone, Debug)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Clone, Debug)]
struct CapturedEvent {
    fields: HashMap<String, String>,
}

#[derive(Default)]
struct TraceCaptureInner {
    span_order: Vec<u64>,
    spans: HashMap<u64, CapturedSpan>,
    events: Vec<CapturedEvent>,
}

#[derive(Clone, Default)]
struct TraceCapture {
    inner: Arc<Mutex<TraceCaptureInner>>,
}

impl TraceCapture {
    fn spans_named(&self, name: &str) -> Vec<CapturedSpan> {
        let inner = self.inner.lock().expect("trace capture lock");
        inner
            .span_order
            .iter()
            .filter_map(|id| inner.spans.get(id))
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }

    fn event_by_message(&self, message: &str) -> Option<CapturedEvent> {
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .iter()
            .find(|event| {
                event
                    .fields
                    .get("message")
                    .is_some_and(|value| value == message)
            })
            .cloned()
    }
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &Id,
        _ctx: LayerContext<'_, S>,
    ) {
        let mut visitor = FieldCapture::default();
        attrs.record(&mut visitor);
        let mut inner = self.inner.lock().expect("trace capture lock");
        let id = id.into_u64();
        inner.span_order.push(id);
        inner.spans.insert(
            id,
            CapturedSpan {
                name: attrs.metadata().name().to_owned(),
                fields: visitor.fields,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: LayerContext<'_, S>) {
        let mut visitor = FieldCapture::default();
        values.record(&mut visitor);
        if let Some(span) = self
            .inner
            .lock()
            .expect("trace capture lock")
            .spans
            .get_mut(&id.into_u64())
        {
            span.fields.extend(visitor.fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let mut visitor = FieldCapture::default();
        event.record(&mut visitor);
        let _ = ctx.event_scope(event);
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .push(CapturedEvent {
                fields: visitor.fields,
            });
    }
}

#[derive(Default)]
struct FieldCapture {
    fields: HashMap<String, String>,
}

impl Visit for FieldCapture {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}
