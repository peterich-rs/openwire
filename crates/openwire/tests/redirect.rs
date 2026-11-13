//! Full-stack redirect cases whose expected values come from RFC 9110 §15.4
//! and Fetch, not from OpenWire's current branch structure.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, RustlsTlsConnector, WireError, WireErrorKind};
use openwire_test::{
    collect_request_body, ok_text, spawn_http1, spawn_https_http1, text_response, MockResponse,
    MockWebServer, RecordingEventListenerFactory, StaticDnsResolver,
};

use support::*;

fn empty_get(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .method(Method::GET)
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

#[tokio::test]
async fn follows_relative_location() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("arrived"));

    let client = Client::builder().build().expect("client");
    let response = client
        .execute(empty_get(server.url("/start")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().text().await.expect("body"), "arrived");

    assert_eq!(server.take_request().await.uri.path(), "/start");
    assert_eq!(server.take_request().await.uri.path(), "/next");
}

#[tokio::test]
async fn follows_protocol_relative_location() {
    let server = MockWebServer::http1().await;
    let location = format!("//{}/next", server.addr());
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", location)
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("arrived"));

    let client = Client::builder().build().expect("client");
    let response = client
        .execute(empty_get(server.url("/start")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().text().await.expect("body"), "arrived");
    assert_eq!(server.take_request().await.uri.path(), "/start");
    assert_eq!(server.take_request().await.uri.path(), "/next");
}

#[tokio::test]
async fn post_302_second_hop_is_get_without_body() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::POST)
        .uri(server.url("/start"))
        .body(RequestBody::from_static(b"payload"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let first = server.take_request().await;
    assert_eq!(first.method, Method::POST);
    assert_eq!(first.body.as_ref(), b"payload");

    let second = server.take_request().await;
    assert_eq!(second.method, Method::GET);
    assert!(
        second.body.is_empty(),
        "RFC 9110/Fetch: 302 POST becomes GET with no body, got {:?}",
        second.body
    );
}

#[tokio::test]
async fn post_307_second_hop_replays_body() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::POST)
        .uri(server.url("/start"))
        .body(RequestBody::from_static(b"payload"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let second = {
        let _first = server.take_request().await;
        server.take_request().await
    };
    assert_eq!(
        second.method,
        Method::POST,
        "RFC 9110 §15.4.8 preserves POST"
    );
    assert_eq!(second.body.as_ref(), b"payload");
}

#[tokio::test]
async fn max_redirects_errors_after_limit() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/b"),
    );
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/c"),
    );
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/d"),
    );

    let client = Client::builder().max_redirects(2).build().expect("client");
    let error = client
        .execute(empty_get(server.url("/a")))
        .await
        .expect_err("third redirect must fail");
    assert_eq!(error.kind(), WireErrorKind::Redirect);
    assert!(
        error.message().contains("too many redirects"),
        "error = {error}"
    );
}

#[tokio::test]
async fn cross_origin_second_hop_has_no_authorization() {
    let origin = MockWebServer::http1().await;
    let target = MockWebServer::http1().await;
    origin.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", target.url("/next")),
    );
    target.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let mut request = empty_get(origin.url("/start"));
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        "Bearer secret".parse().expect("authorization"),
    );
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let first = origin.take_request().await;
    assert_eq!(
        first
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret")
    );
    let second = target.take_request().await;
    assert!(
        second.headers.get(http::header::AUTHORIZATION).is_none(),
        "cross-origin redirect must drop Authorization, got {:?}",
        second.headers
    );
}

#[tokio::test]
async fn redirect_chain_emits_follow_up_and_redirect_events() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let response = client
        .execute(empty_get(server.url("/start")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    events.assert_contains_in_order(&[
        "call_start GET",
        "follow_up_decision 302",
        "redirect 1",
        "response_headers_end 200 OK",
        "call_end",
    ]);
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
async fn does_not_automatically_follow_multiple_choices() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        match request.uri().path() {
            "/choices" => Response::builder()
                .status(StatusCode::MULTIPLE_CHOICES)
                .header("location", "/chosen")
                .body(http_body_util::Full::new(Bytes::from_static(b"pick one")))
                .expect("redirect response"),
            "/chosen" => ok_text("chosen"),
            _ => text_response(StatusCode::NOT_FOUND, "not found"),
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/choices")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::MULTIPLE_CHOICES);
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "pick one");
    assert!(
        !events
            .events()
            .iter()
            .any(|event| event.starts_with("redirect ")),
        "events = {:?}",
        events.events()
    );
}

#[tokio::test]
async fn preserve_method_redirect_with_streaming_body_is_returned() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let server_attempts = server_attempts.clone();
        async move {
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let _ = collect_request_body(request).await;
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", "/final")
                .body(http_body_util::Full::new(Bytes::from_static(
                    b"cannot replay",
                )))
                .expect("redirect response")
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(server.http_url("/streaming-redirect"))
        .body(RequestBody::from_stream(stream::iter(vec![Ok::<
            Bytes,
            WireError,
        >(
            Bytes::from_static(b"streaming body"),
        )])))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "cannot replay");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let events = events.events();
    assert!(
        !events.iter().any(|event| event.starts_with("redirect ")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn per_request_follow_redirects_override_can_disable_redirects() {
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

    let client = Client::builder()
        .follow_redirects(true)
        .build()
        .expect("client");

    let response = client
        .new_call(empty_request(server.http_url("/redirect")))
        .follow_redirects(false)
        .execute()
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FOUND);
}

#[tokio::test]
async fn per_request_redirect_overrides_do_not_disable_custom_redirect_policy() {
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
    let policy = StopRedirectPolicy::default();
    let client = Client::builder()
        .redirect_policy(policy.clone())
        .build()
        .expect("client");

    let response = client
        .new_call(empty_request(server.http_url("/redirect")))
        .follow_redirects(true)
        .max_redirects(5)
        .allow_insecure_redirects(true)
        .execute()
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(policy.calls(), 1);
}

#[tokio::test]
async fn downgrade_redirects_are_rejected_by_default() {
    let insecure_server = spawn_http1(|_request| async move { ok_text("downgraded") }).await;
    let insecure_target = format!(
        "http://localhost:{}/downgraded",
        insecure_server.addr().port()
    );
    let secure_server = spawn_https_http1(move |_request| {
        let insecure_target = insecure_target.clone();
        async move {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", insecure_target)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response")
        }
    })
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(secure_server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let client = Client::builder()
        .tls_connector(tls)
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(format!(
            "https://localhost:{}/redirect",
            secure_server.addr().port()
        )))
        .await
        .expect_err("downgrade redirect should fail");

    assert_eq!(error.kind(), WireErrorKind::Redirect);
    assert!(
        error
            .to_string()
            .contains("refusing insecure redirect from https://localhost:"),
        "error = {error}",
    );
}

#[tokio::test]
async fn insecure_redirects_can_be_enabled_explicitly() {
    let insecure_server = spawn_http1(|_request| async move { ok_text("downgraded") }).await;
    let insecure_target = format!(
        "http://localhost:{}/downgraded",
        insecure_server.addr().port()
    );
    let secure_server = spawn_https_http1(move |_request| {
        let insecure_target = insecure_target.clone();
        async move {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", insecure_target)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .expect("redirect response")
        }
    })
    .await;
    let tls = RustlsTlsConnector::builder()
        .add_root_certificates_pem(secure_server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector");
    let client = Client::builder()
        .tls_connector(tls)
        .allow_insecure_redirects(true)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://localhost:{}/redirect",
            secure_server.addr().port()
        )))
        .await
        .expect("redirect should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "downgraded");
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
async fn redirect_follow_up_preserves_request_extensions() {
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
    let capture = ExtensionCaptureInterceptor::default();
    let client = Client::builder()
        .network_interceptor(capture.clone())
        .build()
        .expect("client");

    let mut request = empty_request(server.http_url("/redirect"));
    request
        .extensions_mut()
        .insert(RetainedRequestMarker("redirect"));

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "redirect complete");
    assert_eq!(capture.seen(), vec![Some("redirect"), Some("redirect")]);
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
async fn put_301_second_hop_preserves_method_and_body() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::PUT)
        .uri(server.url("/start"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(RequestBody::from_static(b"payload"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let first = server.take_request().await;
    assert_eq!(first.method, Method::PUT);
    assert_eq!(first.body.as_ref(), b"payload");

    let second = server.take_request().await;
    assert_eq!(second.method, Method::PUT);
    assert_eq!(second.body.as_ref(), b"payload");
    assert_eq!(
        second
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn patch_302_second_hop_preserves_method_and_body() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(server.url("/start"))
        .body(RequestBody::from_static(b"patch-body"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let second = {
        let _first = server.take_request().await;
        server.take_request().await
    };
    assert_eq!(second.method, Method::PATCH);
    assert_eq!(second.body.as_ref(), b"patch-body");
}

#[tokio::test]
async fn put_301_streaming_body_is_returned() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let server_attempts = server_attempts.clone();
        async move {
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let _ = collect_request_body(request).await;
            Response::builder()
                .status(StatusCode::MOVED_PERMANENTLY)
                .header("location", "/final")
                .body(http_body_util::Full::new(Bytes::from_static(
                    b"cannot replay",
                )))
                .expect("redirect response")
        }
    })
    .await;
    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::PUT)
        .uri(server.http_url("/streaming-put"))
        .body(RequestBody::from_stream(stream::iter(vec![Ok::<
            Bytes,
            WireError,
        >(
            Bytes::from_static(b"streaming"),
        )])))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn post_302_strips_content_language_and_digest_headers() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Request::builder()
        .method(Method::POST)
        .uri(server.url("/start"))
        .header(http::header::CONTENT_TYPE, "text/plain")
        .header(http::header::CONTENT_LANGUAGE, "en")
        .header(http::header::CONTENT_LOCATION, "/item")
        .header(http::header::LAST_MODIFIED, "Wed, 21 Oct 2015 07:28:00 GMT")
        .header("digest", "sha-256=abc")
        .header("content-digest", "sha-256=:abc:")
        .header("repr-digest", "sha-256=:abc:")
        .body(RequestBody::from_static(b"payload"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");
    let _first = server.take_request().await;
    let second = server.take_request().await;
    assert_eq!(second.method, Method::GET);
    assert!(second.headers.get(http::header::CONTENT_TYPE).is_none());
    assert!(second.headers.get(http::header::CONTENT_LANGUAGE).is_none());
    assert!(second.headers.get(http::header::CONTENT_LOCATION).is_none());
    assert!(second.headers.get(http::header::LAST_MODIFIED).is_none());
    assert!(second.headers.get("digest").is_none());
    assert!(second.headers.get("content-digest").is_none());
    assert!(second.headers.get("repr-digest").is_none());
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_307_replays_identical_boundary_and_payload() {
    use openwire::multipart::{Form, Part};

    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));

    let client = Client::builder().build().expect("client");
    let request = Form::new()
        .text("description", "upload from OpenWire")
        .part(
            "file",
            Part::bytes(Bytes::from_static(b"hello\n")).file_name("hello.txt"),
        )
        .into_request(Request::builder().method("POST").uri(server.url("/start")))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");

    let first = server.take_request().await;
    let second = server.take_request().await;
    assert_eq!(second.method, Method::POST);
    assert_eq!(first.body, second.body);
    assert_eq!(
        first.headers.get(http::header::CONTENT_TYPE),
        second.headers.get(http::header::CONTENT_TYPE)
    );
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_one_shot_308_is_returned() {
    use openwire::multipart::{Form, Part};

    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let server_attempts = server_attempts.clone();
        async move {
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let _ = collect_request_body(request).await;
            Response::builder()
                .status(StatusCode::PERMANENT_REDIRECT)
                .header("location", "/final")
                .body(http_body_util::Full::new(Bytes::from_static(b"stop")))
                .expect("redirect")
        }
    })
    .await;
    let client = Client::builder().build().expect("client");
    let request = Form::new()
        .part(
            "file",
            Part::stream(stream::iter(vec![Ok::<Bytes, WireError>(
                Bytes::from_static(b"one-shot"),
            )])),
        )
        .into_request(
            Request::builder()
                .method("POST")
                .uri(server.http_url("/one-shot-308")),
        )
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_302_get_rewrite_strips_content_type() {
    use openwire::multipart::{Form, Part};

    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .status(StatusCode::FOUND)
            .header("Location", "/next")
            .body("redirect"),
    );
    server.enqueue(MockResponse::new().body("ok"));
    let client = Client::builder().build().expect("client");
    let request = Form::new()
        .text("a", "b")
        .part("file", Part::bytes(Bytes::from_static(b"x")))
        .into_request(Request::builder().method("POST").uri(server.url("/start")))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().text().await.expect("body");
    let _first = server.take_request().await;
    let second = server.take_request().await;
    assert_eq!(second.method, Method::GET);
    assert!(second.body.is_empty());
    assert!(second.headers.get(http::header::CONTENT_TYPE).is_none());
    assert!(second.headers.get(http::header::CONTENT_LENGTH).is_none());
}
