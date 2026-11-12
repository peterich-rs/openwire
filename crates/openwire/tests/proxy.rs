mod support;

use std::net::SocketAddr;
use std::time::Duration;

use http::header::PROXY_AUTHENTICATE;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{
    Client, NoProxy, Proxy, ProxyRules, RequestBody, RustlsTlsConnector, WireErrorKind,
};
use openwire_test::{
    ok_text, spawn_http1, spawn_https_http1, text_response, RecordingEventListenerFactory,
    StaticDnsResolver,
};

use support::*;

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
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
    assert_single_realm_challenge(&authenticator.observed_challenges()[0], "Basic", "proxy");
    assert_eq!(
        authenticator.observed_attempts(),
        vec![ObservedAuthAttempt {
            total_attempt: 1,
            retry_count: 0,
            redirect_count: 0,
            auth_count: 0,
        }]
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
async fn redirected_https_connect_proxy_auth_context_keeps_logical_attempt_counts() {
    let secure_server =
        spawn_https_http1(|_request| async move { ok_text("redirected proxied tls auth ok") })
            .await;
    let redirect_target = format!(
        "https://localhost:{}/secure-after-redirect",
        secure_server.addr().port()
    );
    let redirect_server = spawn_http1(move |_request: Request<Incoming>| {
        let redirect_target = redirect_target.clone();
        async move {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", redirect_target)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response")
        }
    })
    .await;
    let proxy = spawn_connect_proxy_requiring_authorization(
        "Proxy-Authorization",
        "Basic cHJveHk6c2VjcmV0",
    )
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(secure_server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let authenticator =
        StaticHeaderAuthenticator::new("proxy-authorization", "Basic cHJveHk6c2VjcmV0");

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("redirect.test".to_owned(), redirect_server.addr()),
            ("proxy.test".to_owned(), proxy.addr()),
        ]))
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .proxy_authenticator(authenticator.clone())
        .tls_connector(tls)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://redirect.test:{}/start",
            redirect_server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "redirected proxied tls auth ok");
    assert_eq!(authenticator.calls(), 1);
    assert_eq!(
        authenticator.observed_attempts(),
        vec![ObservedAuthAttempt {
            total_attempt: 2,
            retry_count: 0,
            redirect_count: 1,
            auth_count: 0,
        }]
    );

    let requests = proxy.requests();
    assert_eq!(requests.len(), 2, "requests = {requests:?}");
    assert!(
        requests[0].starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1",
            secure_server.addr().port()
        )),
        "requests = {requests:?}",
    );
    assert!(
        requests[1].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}",
    );
}

#[tokio::test]
async fn https_proxy_connect_uses_url_embedded_credentials_on_initial_connect() {
    let server =
        spawn_https_http1(|_request| async move { ok_text("proxied tls url auth ok") }).await;
    let proxy =
        spawn_connect_proxy_requiring_authorization("Proxy-Authorization", "Basic dXNlcjpwYXNz")
            .await;
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
        .proxy_selector(proxy_rules(
            Proxy::https(format!(
                "http://user:pass@proxy.test:{}",
                proxy.addr().port()
            ))
            .expect("proxy config"),
        ))
        .tls_connector(tls)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://localhost:{}/secure-embedded-auth",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied tls url auth ok");

    let requests = proxy.requests();
    assert_eq!(requests.len(), 1, "requests = {requests:?}");
    assert!(
        requests[0].starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1",
            server.addr().port()
        )),
        "requests = {requests:?}",
    );
    assert!(
        requests[0].contains("proxy-authorization: Basic dXNlcjpwYXNz"),
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
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
async fn per_request_connect_timeout_overrides_client_default() {
    let server = spawn_http1(|_request| async move { ok_text("connect ok") }).await;
    let connector = RecordingTimeoutTcpConnector::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .connect_timeout(Duration::from_secs(1))
        .build()
        .expect("client");

    let response = client
        .new_call(empty_request(server.http_url("/connect-timeout")))
        .connect_timeout(Duration::from_millis(25))
        .execute()
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(connector.timeouts(), vec![Some(Duration::from_millis(25))]);
}

#[tokio::test]
async fn http_requests_can_route_through_http_proxy_without_origin_dns() {
    let proxy = spawn_plain_http_proxy_response("proxied http ok").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
async fn http_requests_can_route_through_socks5_proxy_without_origin_dns() {
    let server = spawn_http1(|_request| async move { ok_text("proxied socks http") }).await;
    let proxy = spawn_socks5_proxy().await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::socks5(format!("socks5://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://localhost:{}/socks-http",
            server.addr().port()
        )))
        .await
        .expect("response");

    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied socks http");
    assert!(
        proxy
            .requests()
            .iter()
            .any(|request| request == &format!("localhost:{}", server.addr().port())),
        "requests = {:?}",
        proxy.requests(),
    );
}

#[tokio::test]
async fn socks5_proxy_username_password_authentication_is_required_when_server_demands_it() {
    let server = spawn_http1(|_request| async move { ok_text("proxied socks http") }).await;
    let proxy = spawn_socks5_proxy_requiring_auth("alice", "secret").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::socks5(format!("socks5://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(format!(
            "http://localhost:{}/socks-http-auth-required",
            server.addr().port()
        )))
        .await
        .expect_err("missing proxy credentials should fail");

    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert!(
        error
            .to_string()
            .contains("SOCKS5 proxy does not support no-auth authentication"),
        "error = {error}",
    );
}

#[tokio::test]
async fn http_requests_can_route_through_socks5_proxy_with_username_password() {
    let server = spawn_http1(|_request| async move { ok_text("proxied socks http auth") }).await;
    let proxy = spawn_socks5_proxy_requiring_auth("alice", "secret").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::socks5(format!(
                "socks5://alice:secret@proxy.test:{}",
                proxy.addr().port()
            ))
            .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://localhost:{}/socks-http-auth",
            server.addr().port()
        )))
        .await
        .expect("response");

    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied socks http auth");
    assert!(
        proxy
            .requests()
            .iter()
            .any(|request| request == &format!("localhost:{}", server.addr().port())),
        "requests = {:?}",
        proxy.requests(),
    );
}

#[tokio::test]
async fn https_requests_can_tunnel_through_socks5_proxy_without_origin_dns() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxied socks https") }).await;
    let proxy = spawn_socks5_proxy().await;
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
        .proxy_selector(proxy_rules(
            Proxy::socks5(format!("socks5://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .tls_connector(tls)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://localhost:{}/socks-https",
            server.addr().port()
        )))
        .await
        .expect("response");

    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxied socks https");
    assert!(
        proxy
            .requests()
            .iter()
            .any(|request| request == &format!("localhost:{}", server.addr().port())),
        "requests = {:?}",
        proxy.requests(),
    );
}

#[tokio::test]
async fn http_proxy_routes_fast_fallback_across_proxy_ipv4_and_ipv6_endpoints() {
    let proxy = spawn_plain_http_proxy_response("proxied proxy race").await;
    let fake_ipv6 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0], 9301));
    let fake_ipv4 = SocketAddr::from(([192, 0, 2, 41], 9302));
    let resolver = MultiAddrResolver::new([("proxy.test".to_owned(), vec![fake_ipv6, fake_ipv4])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_ipv6,
            TcpAttemptScript {
                actual_addr: proxy.addr(),
                delay: Duration::from_millis(600),
            },
        ),
        (
            fake_ipv4,
            TcpAttemptScript {
                actual_addr: proxy.addr(),
                delay: Duration::from_millis(10),
            },
        ),
    ]);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .event_listener_factory(events.clone())
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let start = std::time::Instant::now();
    let response = client
        .execute(empty_request(
            "http://does-not-resolve.test/proxy-race?fast_fallback=1",
        ))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    let elapsed = start.elapsed();

    assert_eq!(body, "proxied proxy race");
    assert!(
        elapsed < Duration::from_millis(550),
        "elapsed = {elapsed:?} should be faster than sequential proxy fallback",
    );
    assert_eq!(connector.attempts(), 2);

    let requests = proxy.requests();
    assert_eq!(requests.len(), 1, "requests = {requests:?}");
    assert!(
        requests[0]
            .starts_with("GET http://does-not-resolve.test/proxy-race?fast_fallback=1 HTTP/1.1"),
        "requests = {requests:?}",
    );

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "route_plan 2 fast_fallback=true",
            "connect_race_start ",
            "connect_race_start ",
            "connect_race_won ",
            "connect_race_lost ",
        ],
    );
}

#[tokio::test]
async fn connect_proxy_fast_fallback_continues_after_proxy_tunnel_failure() {
    let server = spawn_http1(|_request| async move { ok_text("connect proxy fallback") }).await;
    let failing_proxy = spawn_delayed_dropping_connect_proxy(Duration::from_millis(300)).await;
    let working_proxy = spawn_connect_proxy().await;
    let fake_ipv6 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 1], 9303));
    let fake_ipv4 = SocketAddr::from(([192, 0, 2, 42], 9304));
    let resolver = MultiAddrResolver::new([("proxy.test".to_owned(), vec![fake_ipv6, fake_ipv4])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_ipv6,
            TcpAttemptScript {
                actual_addr: failing_proxy.addr(),
                delay: Duration::ZERO,
            },
        ),
        (
            fake_ipv4,
            TcpAttemptScript {
                actual_addr: working_proxy.addr(),
                delay: Duration::from_millis(200),
            },
        ),
    ]);
    let tls = ScriptedPassThroughTlsConnector::new(vec![TlsAttemptScript::Pass(
        Duration::from_millis(1),
    )]);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .tls_connector(tls.clone())
        .event_listener_factory(events.clone())
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", failing_proxy.addr().port()))
                .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://localhost:{}/proxy-connect-race",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "connect proxy fallback");
    assert_eq!(connector.attempts(), 2);
    assert_eq!(tls.calls(), 1);

    let failing_requests = failing_proxy.requests();
    assert_eq!(failing_requests.len(), 1, "requests = {failing_requests:?}");
    assert!(
        failing_requests[0].starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1",
            server.addr().port()
        )),
        "requests = {failing_requests:?}",
    );
    let working_requests = working_proxy.requests();
    assert_eq!(working_requests.len(), 1, "requests = {working_requests:?}");
    assert!(
        working_requests[0].starts_with(&format!(
            "CONNECT localhost:{} HTTP/1.1",
            server.addr().port()
        )),
        "requests = {working_requests:?}",
    );

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "route_plan 2 fast_fallback=true",
            "connect_race_start ",
            "connect_race_won ",
        ],
    );
    let start_events = events
        .iter()
        .filter(|event| event.starts_with("connect_race_start "))
        .collect::<Vec<_>>();
    assert_eq!(start_events.len(), 2, "events = {events:?}");
    let lost_events = events
        .iter()
        .filter(|event| event.starts_with("connect_race_lost "))
        .collect::<Vec<_>>();
    assert_eq!(lost_events.len(), 1, "events = {events:?}");
    assert!(
        lost_events[0].contains("reason=proxy_failed"),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn connect_proxy_fast_fallback_parallelizes_stalled_tunnel_finalization() {
    let server =
        spawn_http1(|_request| async move { ok_text("parallel connect proxy fallback") }).await;
    let stalling_proxy = spawn_stalling_connect_proxy().await;
    let working_proxy = spawn_connect_proxy().await;
    let fake_ipv6 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 2], 9305));
    let fake_ipv4 = SocketAddr::from(([192, 0, 2, 43], 9306));
    let resolver = MultiAddrResolver::new([("proxy.test".to_owned(), vec![fake_ipv6, fake_ipv4])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_ipv6,
            TcpAttemptScript {
                actual_addr: stalling_proxy.addr(),
                delay: Duration::ZERO,
            },
        ),
        (
            fake_ipv4,
            TcpAttemptScript {
                actual_addr: working_proxy.addr(),
                delay: Duration::from_millis(200),
            },
        ),
    ]);
    let tls = ScriptedPassThroughTlsConnector::new(vec![TlsAttemptScript::Pass(
        Duration::from_millis(1),
    )]);
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .tls_connector(tls.clone())
        .proxy_selector(proxy_rules(
            Proxy::https(format!(
                "http://proxy.test:{}",
                stalling_proxy.addr().port()
            ))
            .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let response_task = tokio::spawn({
        let client = client.clone();
        let request = empty_request(format!(
            "https://localhost:{}/proxy-connect-stall",
            server.addr().port()
        ));
        async move { client.execute(request).await }
    });

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if working_proxy.requests().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("working CONNECT attempt should start before stalled tunnel finalization completes");

    let response = tokio::time::timeout(Duration::from_secs(5), response_task)
        .await
        .expect("request should complete without waiting for stalled CONNECT")
        .expect("response task")
        .expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "parallel connect proxy fallback");
    assert_eq!(connector.attempts(), 2);
    assert_eq!(tls.calls(), 1);
    assert_eq!(stalling_proxy.requests().len(), 1);
    assert_eq!(working_proxy.requests().len(), 1);
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
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().host("direct.test")),
        ))
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
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().domain("internal.test")),
        ))
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
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config")
                .no_proxy(NoProxy::new().localhost()),
        ))
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
        .proxy_selector(system_proxy_rules())
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
        .proxy_selector(system_proxy_rules())
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
        .proxy_selector(
            ProxyRules::new()
                .proxy(
                    Proxy::http(format!(
                        "http://explicit-proxy.test:{}",
                        explicit_proxy.addr().port()
                    ))
                    .expect("explicit proxy"),
                )
                .use_system_proxy(true),
        )
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
        .proxy_selector(
            ProxyRules::new()
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
                ),
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
async fn proxy_selector_is_re_evaluated_after_client_build() {
    let first_proxy = spawn_plain_http_proxy_response("proxy one").await;
    let second_proxy = spawn_plain_http_proxy_response("proxy two").await;
    let selector = SwitchingProxySelector::new(Some(
        Proxy::http(format!(
            "http://proxy-one.test:{}",
            first_proxy.addr().port()
        ))
        .expect("proxy one"),
    ));
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("proxy-one.test".to_owned(), first_proxy.addr()),
            ("proxy-two.test".to_owned(), second_proxy.addr()),
        ]))
        .proxy_selector(selector.clone())
        .build()
        .expect("client");

    let first_response = client
        .execute(empty_request("http://does-not-resolve.test/dynamic-first"))
        .await
        .expect("first response");
    assert_eq!(
        first_response.into_body().text().await.expect("first body"),
        "proxy one"
    );
    assert_eq!(first_proxy.requests().len(), 1);
    assert!(second_proxy.requests().is_empty());

    selector.set_proxy(Some(
        Proxy::http(format!(
            "http://proxy-two.test:{}",
            second_proxy.addr().port()
        ))
        .expect("proxy two"),
    ));

    let second_response = client
        .execute(empty_request("http://does-not-resolve.test/dynamic-second"))
        .await
        .expect("second response");
    assert_eq!(
        second_response
            .into_body()
            .text()
            .await
            .expect("second body"),
        "proxy two"
    );
    assert_eq!(first_proxy.requests().len(), 1);
    assert_eq!(second_proxy.requests().len(), 1);
}

#[tokio::test]
async fn proxy_rules_try_next_proxy_after_connect_proxy_failure() {
    let server = spawn_https_http1(|_request| async move { ok_text("proxy fallback") }).await;
    let failing_proxy = spawn_rejecting_connect_proxy().await;
    let working_proxy = spawn_connect_proxy().await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");

    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("failing-proxy.test".to_owned(), failing_proxy.addr()),
            ("working-proxy.test".to_owned(), working_proxy.addr()),
        ]))
        .proxy_selector(
            ProxyRules::new()
                .proxy(
                    Proxy::https(format!(
                        "http://failing-proxy.test:{}",
                        failing_proxy.addr().port()
                    ))
                    .expect("failing proxy"),
                )
                .proxy(
                    Proxy::https(format!(
                        "http://working-proxy.test:{}",
                        working_proxy.addr().port()
                    ))
                    .expect("working proxy"),
                ),
        )
        .tls_connector(tls)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://localhost:{}/proxy-candidate-fallback",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "proxy fallback");
    assert_eq!(failing_proxy.requests().len(), 1);
    assert_eq!(working_proxy.requests().len(), 1);
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
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
    assert_single_realm_challenge(&authenticator.observed_challenges()[0], "Basic", "proxy");
}

#[tokio::test]
async fn http_forward_proxy_uses_url_embedded_credentials_on_initial_request() {
    let proxy =
        spawn_proxy_requiring_authorization("Proxy-Authorization", "Basic dXNlcjpwYXNz").await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::http(format!(
                "http://user:pass@proxy.test:{}",
                proxy.addr().port()
            ))
            .expect("proxy config"),
        ))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://does-not-resolve.test/proxy-url-auth")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "proxy authorized");

    let requests = proxy.requests();
    assert_eq!(requests.len(), 1, "requests = {requests:?}");
    assert!(
        requests[0].contains("proxy-authorization: Basic dXNlcjpwYXNz"),
        "requests = {requests:?}",
    );
}

#[tokio::test]
async fn http_proxy_redirects_keep_proxy_authorization_for_same_proxy() {
    let proxy = spawn_proxy_redirect_requiring_authorization(
        "Proxy-Authorization",
        "Basic cHJveHk6c2VjcmV0",
    )
    .await;
    let authenticator =
        StaticHeaderAuthenticator::new("proxy-authorization", "Basic cHJveHk6c2VjcmV0");
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
        .proxy_authenticator(authenticator.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .uri("http://source.test/start")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "redirect authorized");
    assert_eq!(authenticator.calls(), 1);

    let requests = proxy.requests();
    assert_eq!(requests.len(), 3, "requests = {requests:?}");
    assert!(
        !requests[0].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}"
    );
    assert!(
        requests[1].contains("GET http://source.test/start HTTP/1.1")
            && requests[1].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}"
    );
    assert!(
        requests[2].contains("GET http://target.test/finish HTTP/1.1")
            && requests[2].contains("proxy-authorization: Basic cHJveHk6c2VjcmV0"),
        "requests = {requests:?}"
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
        .proxy_selector(proxy_rules(
            Proxy::http(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
async fn direct_407_does_not_invoke_proxy_authenticator() {
    let server = spawn_http1(|_request| async move {
        let mut response = text_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, "direct 407");
        response.headers_mut().insert(
            PROXY_AUTHENTICATE,
            "Basic realm=\"origin\"".parse().expect("challenge"),
        );
        response
    })
    .await;
    let authenticator = DecliningAuthenticator::default();
    let client = Client::builder()
        .proxy_authenticator(authenticator.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/direct-407")))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::PROXY_AUTHENTICATION_REQUIRED);
    assert_eq!(
        response.into_body().text().await.expect("body"),
        "direct 407"
    );
    assert_eq!(authenticator.calls(), 0);
}

#[tokio::test]
async fn proxy_connect_failures_return_connect_errors() {
    let proxy = spawn_rejecting_connect_proxy().await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "proxy.test".to_owned(),
            proxy.addr(),
        )]))
        .proxy_selector(proxy_rules(
            Proxy::https(format!("http://proxy.test:{}", proxy.addr().port()))
                .expect("proxy config"),
        ))
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
