mod support;

use bytes::Bytes;
use futures_util::stream;
use http::header::AUTHORIZATION;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, WireError};
use openwire_test::{collect_request_body, ok_text, spawn_http1, StaticDnsResolver};

use support::*;

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
    assert_single_realm_challenge(
        &authenticator.observed_challenges()[0],
        "Bearer",
        "openwire",
    );
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "authorized");
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
async fn auth_follow_up_preserves_request_extensions() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let authorized = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some("Bearer good");
        if authorized {
            ok_text("auth ok")
        } else {
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("www-authenticate", "Bearer realm=\"openwire\"")
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("unauthorized response")
        }
    })
    .await;
    let capture = ExtensionCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(capture.clone())
        .authenticator(StaticAuthorizationAuthenticator::new("Bearer good"))
        .build()
        .expect("client");

    let mut request = empty_request(format!(
        "http://openwire.test:{}/auth-extension",
        server.addr().port()
    ));
    request
        .extensions_mut()
        .insert(RetainedRequestMarker("auth"));

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "auth ok");
    assert_eq!(capture.seen(), vec![Some("auth"), Some("auth")]);
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
