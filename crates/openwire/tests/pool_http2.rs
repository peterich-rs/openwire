mod support;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http::header::{CONNECTION, TE, TRANSFER_ENCODING, UPGRADE};
use http::{Request, Response, StatusCode, Version};
use hyper::body::Incoming;
use openwire::{
    Client, EstablishmentStage, RequestBody, RustlsTlsConnector, WireError, WireErrorKind,
};
use openwire_test::{
    collect_request_body, ok_text, spawn_http1, spawn_https_http1, spawn_https_http2_with_hosts,
    text_response, RecordingEventListenerFactory, StaticDnsResolver,
};

use support::*;

#[tokio::test]
async fn per_request_retry_override_can_disable_connection_retries() {
    let server = spawn_http1(|_request| async move { ok_text("retry ok") }).await;
    let connector = FailingTcpConnector::new(1);
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .retry_on_connection_failure(true)
        .max_retries(1)
        .build()
        .expect("client");

    let error = client
        .new_call(empty_request(server.http_url("/retry-disabled")))
        .retry_on_connection_failure(false)
        .execute()
        .await
        .expect_err("per-request retry override should win");

    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(connector.attempts(), 1);
}

#[tokio::test]
async fn per_request_retry_overrides_do_not_disable_custom_retry_policy() {
    let server = spawn_http1(|_request| async move { ok_text("retry ok") }).await;
    let connector = FailingTcpConnector::new(1);
    let policy = NeverRetryPolicy::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .retry_policy(policy.clone())
        .build()
        .expect("client");

    let error = client
        .new_call(empty_request(server.http_url("/retry-custom")))
        .retry_on_connection_failure(true)
        .max_retries(3)
        .retry_canceled_requests(true)
        .execute()
        .await
        .expect_err("custom retry policy should remain authoritative");

    assert_eq!(error.kind(), WireErrorKind::Connect);
    assert_eq!(connector.attempts(), 1);
    assert_eq!(policy.calls(), 1);
}

#[tokio::test]
async fn canceled_requests_are_not_retried_by_default() {
    let server = spawn_http1(|_request| async move { ok_text("unused") }).await;
    let interceptor = CancelOnceInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/cancel-default",
            server.addr().port()
        )))
        .await
        .expect_err("canceled request should not retry by default");

    assert_eq!(error.kind(), WireErrorKind::Canceled);
    assert_eq!(interceptor.calls(), 1);
}

#[tokio::test]
async fn canceled_requests_are_retried_when_enabled() {
    let server = spawn_http1(|_request| async move { ok_text("retry ok") }).await;
    let interceptor = CancelOnceInterceptor::default();
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .network_interceptor(interceptor.clone())
        .retry_canceled_requests(true)
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/cancel-enabled",
            server.addr().port()
        )))
        .await
        .expect("canceled request should retry");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "retry ok");
    assert_eq!(interceptor.calls(), 2);
    assert!(
        events
            .events()
            .iter()
            .any(|event| event == "retry 1 canceled"),
        "events = {:?}",
        events.events()
    );
}

#[tokio::test]
async fn retry_follow_up_preserves_request_extensions() {
    let server = spawn_http1(|_request| async move { ok_text("retry ok") }).await;
    let capture = ExtensionCaptureInterceptor::default();
    let cancel = CancelOnceInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(capture.clone())
        .network_interceptor(cancel)
        .retry_canceled_requests(true)
        .build()
        .expect("client");

    let mut request = empty_request(format!(
        "http://openwire.test:{}/retry-extension",
        server.addr().port()
    ));
    request
        .extensions_mut()
        .insert(RetainedRequestMarker("retry"));

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "retry ok");
    assert_eq!(capture.seen(), vec![Some("retry"), Some("retry")]);
}

#[tokio::test]
async fn request_timeout_response_retries_replayable_request() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |_request| {
        let server_attempts = server_attempts.clone();
        async move {
            let attempt = server_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                text_response(StatusCode::REQUEST_TIMEOUT, "timeout")
            } else {
                ok_text("retry ok")
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/request-timeout")))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "retry ok");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        events
            .events()
            .iter()
            .any(|event| event == "retry 1 http_408"),
        "events = {:?}",
        events.events()
    );
}

#[tokio::test]
async fn service_unavailable_retries_only_with_immediate_retry_after() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let server_attempts = server_attempts.clone();
        async move {
            match request.uri().path() {
                "/immediate" => {
                    let attempt = server_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("retry-after", "0")
                            .body(http_body_util::Full::new(Bytes::from_static(b"busy")))
                            .expect("response")
                    } else {
                        ok_text("retry ok")
                    }
                }
                "/delayed" => Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("retry-after", "1")
                    .body(http_body_util::Full::new(Bytes::from_static(b"busy")))
                    .expect("response"),
                _ => text_response(StatusCode::NOT_FOUND, "not found"),
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/immediate")))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "retry ok");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        events
            .events()
            .iter()
            .any(|event| event == "retry 1 http_503"),
        "events = {:?}",
        events.events()
    );

    let response = client
        .execute(empty_request(server.http_url("/delayed")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "busy");
}

#[tokio::test]
async fn response_status_retries_respect_replayability_and_max_retries() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let server_attempts = server_attempts.clone();
        async move {
            let _ = collect_request_body(request).await;
            server_attempts.fetch_add(1, Ordering::SeqCst);
            text_response(StatusCode::REQUEST_TIMEOUT, "timeout")
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
        .uri(server.http_url("/streaming-timeout"))
        .body(RequestBody::from_stream(stream::iter(vec![Ok::<
            Bytes,
            WireError,
        >(
            Bytes::from_static(b"streaming body"),
        )])))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    let _ = response.into_body().text().await.expect("body");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let events = events.events();
    assert!(
        !events.iter().any(|event| event == "retry 1 http_408"),
        "events = {events:?}",
    );

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .max_retries(0)
        .build()
        .expect("client");
    let response = client
        .execute(empty_request(server.http_url("/max-retries-zero")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    let _ = response.into_body().text().await.expect("body");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let events = events.events();
    assert!(
        !events.iter().any(|event| event == "retry 1 http_408"),
        "events = {events:?}",
    );
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
async fn http1_overlapping_calls_do_not_pipeline_when_per_host_caps_are_unlimited() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let server = spawn_http1({
        let barrier = barrier.clone();
        move |_request| {
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                ok_text("http1")
            }
        }
    })
    .await;
    let client = Client::builder().build().expect("client");
    let client_b = client.clone();
    let url_a = server.http_url("/a");
    let url_b = server.http_url("/b");
    let (response_a, response_b) = tokio::try_join!(
        client.execute(empty_request(url_a)),
        client_b.execute(empty_request(url_b)),
    )
    .expect("overlapping HTTP/1 responses");
    let id_a = response_a
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection a")
        .id;
    let id_b = response_b
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection b")
        .id;
    assert_ne!(
        id_a, id_b,
        "HTTP/1 must use a second connection instead of pipelining"
    );
    let (body_a, body_b) =
        tokio::try_join!(response_a.into_body().text(), response_b.into_body().text(),)
            .expect("bodies");
    assert_eq!(body_a, "http1");
    assert_eq!(body_b, "http1");
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
async fn shared_client_does_not_reuse_http1_connections_marked_close_in_connection_tokens() {
    let server = spawn_http1(|_request| async move {
        Response::builder()
            .status(StatusCode::OK)
            .header("connection", "keep-alive, close")
            .body(http_body_util::Full::new(Bytes::from_static(b"closed")))
            .expect("response")
    })
    .await;
    let client = Client::builder().build().expect("client");

    let response_one = client
        .execute(empty_request(server.http_url("/first")))
        .await
        .expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let response_two = client
        .execute(empty_request(server.http_url("/second")))
        .await
        .expect("response");
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_two.into_body().text().await.expect("body");

    assert_ne!(connection_one, connection_two);
}

#[tokio::test]
async fn shared_client_does_not_reuse_http1_connections_with_malformed_connection_header() {
    let server = spawn_http1(|_request| async move {
        Response::builder()
            .status(StatusCode::OK)
            .header("connection", "keep-alive;timeout=5")
            .body(http_body_util::Full::new(Bytes::from_static(b"closed")))
            .expect("response")
    })
    .await;
    let client = Client::builder().build().expect("client");

    let response_one = client
        .execute(empty_request(server.http_url("/first")))
        .await
        .expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let response_two = client
        .execute(empty_request(server.http_url("/second")))
        .await
        .expect("response");
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_two.into_body().text().await.expect("body");

    assert_ne!(connection_one, connection_two);
}

#[tokio::test]
async fn shared_client_coalesces_https_http2_connections_across_verified_authorities() {
    let server = spawn_https_http2_with_hosts(&["a.test", "b.test"], |_request| async move {
        ok_text("coalesced h2")
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server.addr()),
            ("b.test".to_owned(), server.addr()),
        ]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response_one.version(), Version::HTTP_2);
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let response_two = client
        .execute(empty_request(format!(
            "https://b.test:{}/second",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response_two.version(), Version::HTTP_2);
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_two.into_body().text().await.expect("body");

    assert_eq!(connection_one, connection_two);
    assert_eq!(
        events
            .events()
            .into_iter()
            .filter(|event| event.starts_with("connect_end "))
            .count(),
        1
    );
}

#[tokio::test]
async fn http2_stream_release_wakes_coalesced_waiter_when_connection_cap_is_one() {
    let server = spawn_https_http2_with_hosts(&["a.test", "b.test"], |_request| async move {
        ok_text("coalesced h2")
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server.addr()),
            ("b.test".to_owned(), server.addr()),
        ]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .max_connections_total(1)
        .max_http2_streams(1)
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server.addr().port()
        )))
        .await
        .expect("first response");
    assert_eq!(response_one.version(), Version::HTTP_2);
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;

    let second_done = Arc::new(AtomicBool::new(false));
    let second = {
        let client = client.clone();
        let second_done = second_done.clone();
        let port = server.addr().port();
        tokio::spawn(async move {
            let response = client
                .execute(empty_request(format!("https://b.test:{port}/second")))
                .await
                .expect("second response");
            second_done.store(true, Ordering::Relaxed);
            response
        })
    };

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second_done.load(Ordering::Relaxed),
        "host B should wait while the only HTTP/2 stream on the shared connection is held"
    );

    let body = response_one.into_body().text().await.expect("first body");
    assert_eq!(body, "coalesced h2");

    let response_two = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("host B should proceed after the HTTP/2 stream is released")
        .expect("second task");
    assert_eq!(response_two.version(), Version::HTTP_2);
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_two.into_body().text().await.expect("second body");
    assert_eq!(body, "coalesced h2");
    assert_eq!(connection_one, connection_two);
    assert_eq!(
        events
            .events()
            .into_iter()
            .filter(|event| event.starts_with("connect_end "))
            .count(),
        1
    );
}

#[tokio::test]
async fn coalesced_http2_421_retries_without_coalescing() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler_attempts = attempts.clone();
    let server = spawn_https_http2_with_hosts(&["a.test", "b.test"], move |request| {
        let handler_attempts = handler_attempts.clone();
        async move {
            match request.uri().path() {
                "/first" => ok_text("first h2"),
                "/second" => {
                    let attempt = handler_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        text_response(StatusCode::MISDIRECTED_REQUEST, "misdirected")
                    } else {
                        ok_text("second h2")
                    }
                }
                _ => text_response(StatusCode::NOT_FOUND, "not found"),
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server.addr()),
            ("b.test".to_owned(), server.addr()),
        ]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response_one.version(), Version::HTTP_2);
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_one.into_body().text().await.expect("body");
    assert_eq!(body, "first h2");

    let response_two = client
        .execute(empty_request(format!(
            "https://b.test:{}/second",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response_two.status(), StatusCode::OK);
    assert_eq!(response_two.version(), Version::HTTP_2);
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_two.into_body().text().await.expect("body");

    assert_eq!(body, "second h2");
    assert_ne!(connection_one, connection_two);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let events = events.events();
    assert!(
        events
            .iter()
            .any(|event| event == "retry 1 misdirected_request"),
        "events = {events:?}",
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("connect_end "))
            .count(),
        2,
        "events = {events:?}",
    );
}

#[tokio::test]
async fn non_coalesced_http2_421_is_returned() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler_attempts = attempts.clone();
    let server = spawn_https_http2_with_hosts(&["a.test"], move |_request| {
        let handler_attempts = handler_attempts.clone();
        async move {
            handler_attempts.fetch_add(1, Ordering::SeqCst);
            text_response(StatusCode::MISDIRECTED_REQUEST, "misdirected")
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([("a.test".to_owned(), server.addr())]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://a.test:{}/misdirected",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "misdirected");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let events = events.events();
    assert!(
        !events
            .iter()
            .any(|event| event.contains("misdirected_request")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn coalesced_http2_421_does_not_retry_streaming_body() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler_attempts = attempts.clone();
    let server = spawn_https_http2_with_hosts(&["a.test", "b.test"], move |request| {
        let handler_attempts = handler_attempts.clone();
        async move {
            match request.uri().path() {
                "/first" => ok_text("first h2"),
                "/streaming" => {
                    let _ = collect_request_body(request).await;
                    handler_attempts.fetch_add(1, Ordering::SeqCst);
                    text_response(StatusCode::MISDIRECTED_REQUEST, "misdirected")
                }
                _ => text_response(StatusCode::NOT_FOUND, "not found"),
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server.addr()),
            ("b.test".to_owned(), server.addr()),
        ]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server.addr().port()
        )))
        .await
        .expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let stream = stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(
        b"streaming body",
    ))]);
    let request = Request::builder()
        .method("POST")
        .uri(format!("https://b.test:{}/streaming", server.addr().port()))
        .body(RequestBody::from_stream(stream))
        .expect("request");
    let response_two = client.execute(request).await.expect("response");
    assert_eq!(response_two.status(), StatusCode::MISDIRECTED_REQUEST);
    assert_eq!(response_two.version(), Version::HTTP_2);
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_two.into_body().text().await.expect("body");

    assert_eq!(body, "misdirected");
    assert_eq!(connection_one, connection_two);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let events = events.events();
    assert!(
        !events
            .iter()
            .any(|event| event.contains("misdirected_request")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn coalesced_http2_421_respects_max_retries() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler_attempts = attempts.clone();
    let server = spawn_https_http2_with_hosts(&["a.test", "b.test"], move |request| {
        let handler_attempts = handler_attempts.clone();
        async move {
            match request.uri().path() {
                "/first" => ok_text("first h2"),
                "/second" => {
                    handler_attempts.fetch_add(1, Ordering::SeqCst);
                    text_response(StatusCode::MISDIRECTED_REQUEST, "misdirected")
                }
                _ => text_response(StatusCode::NOT_FOUND, "not found"),
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server.addr()),
            ("b.test".to_owned(), server.addr()),
        ]))
        .event_listener_factory(events.clone())
        .max_retries(0)
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server.addr().port()
        )))
        .await
        .expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let response_two = client
        .execute(empty_request(format!(
            "https://b.test:{}/second",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response_two.status(), StatusCode::MISDIRECTED_REQUEST);
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_two.into_body().text().await.expect("body");

    assert_eq!(body, "misdirected");
    assert_eq!(connection_one, connection_two);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let events = events.events();
    assert!(
        !events
            .iter()
            .any(|event| event.contains("misdirected_request")),
        "events = {events:?}",
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("connect_end "))
            .count(),
        1,
        "events = {events:?}",
    );
}

#[tokio::test]
async fn http2_send_path_strips_connection_specific_request_headers() {
    #[derive(Debug, PartialEq, Eq)]
    struct HeaderSnapshot {
        has_connection: bool,
        has_keep_alive: bool,
        has_proxy_connection: bool,
        has_transfer_encoding: bool,
        has_upgrade: bool,
        has_connection_nominated_header: bool,
        te_values: Vec<String>,
    }

    let observed = Arc::new(Mutex::new(None));
    let server_observed = observed.clone();
    let server = spawn_https_http2_with_hosts(&["h2.test"], move |request| {
        let observed = server_observed.clone();
        async move {
            let headers = request.headers();
            let snapshot = HeaderSnapshot {
                has_connection: headers.contains_key(CONNECTION),
                has_keep_alive: headers.contains_key("keep-alive"),
                has_proxy_connection: headers.contains_key("proxy-connection"),
                has_transfer_encoding: headers.contains_key(TRANSFER_ENCODING),
                has_upgrade: headers.contains_key(UPGRADE),
                has_connection_nominated_header: headers.contains_key("x-hop"),
                te_values: headers
                    .get_all(TE)
                    .iter()
                    .map(|value| value.to_str().expect("te header").to_owned())
                    .collect(),
            };
            *observed.lock().expect("observed headers") = Some(snapshot);
            ok_text("h2 headers")
        }
    })
    .await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([(
            "h2.test".to_owned(),
            server.addr(),
        )]))
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://h2.test:{}/connection-specific-headers",
            server.addr().port()
        ))
        .header(CONNECTION, "keep-alive, x-hop, upgrade")
        .header("keep-alive", "timeout=5")
        .header("proxy-connection", "keep-alive")
        .header(TRANSFER_ENCODING, "chunked")
        .header(UPGRADE, "websocket")
        .header("x-hop", "secret")
        .header(TE, "gzip")
        .header(TE, "trailers")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.version(), Version::HTTP_2);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "h2 headers");

    let snapshot = observed
        .lock()
        .expect("observed headers")
        .take()
        .expect("observed headers");
    assert_eq!(
        snapshot,
        HeaderSnapshot {
            has_connection: false,
            has_keep_alive: false,
            has_proxy_connection: false,
            has_transfer_encoding: false,
            has_upgrade: false,
            has_connection_nominated_header: false,
            te_values: vec!["trailers".to_owned()],
        }
    );
}

#[tokio::test]
async fn http2_publication_failure_does_not_leave_pool_entry() {
    let server =
        spawn_https_http2_with_hosts(
            &["a.test"],
            |_request| async move { ok_text("unreachable") },
        )
        .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([("a.test".to_owned(), server.addr())]))
        .executor(AlwaysFailSpawnRuntime)
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    for path in ["/first", "/second"] {
        let error = client
            .execute(empty_request(format!(
                "https://a.test:{}{path}",
                server.addr().port()
            )))
            .await
            .expect_err("spawn failure should abort request");
        assert_eq!(error.kind(), WireErrorKind::Internal);
    }

    let events = events.events();
    let pool_misses = events.iter().filter(|event| *event == "pool_miss").count();
    assert_eq!(pool_misses, 2, "events = {events:?}");
    assert!(
        !events.iter().any(|event| event.starts_with("pool_hit")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn pool_reaper_start_does_not_preempt_http1_connection_task_spawn() {
    let events = RecordingEventListenerFactory::default();
    let server = spawn_http1(|_request| async move { ok_text("published") }).await;
    let client = Client::builder()
        .executor(AllowTwoSpawnsThenFailRuntime::default())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    for path in ["/first", "/second"] {
        let response = client
            .execute(empty_request(server.http_url(path)))
            .await
            .expect("request should succeed even if optional reaper spawn fails");
        let body = response.into_body().text().await.expect("body");
        assert_eq!(body, "published");
    }

    let events = events.events();
    let pool_misses = events.iter().filter(|event| *event == "pool_miss").count();
    let pool_hits = events
        .iter()
        .filter(|event| event.starts_with("pool_hit "))
        .count();
    let connect_ends = events
        .iter()
        .filter(|event| event.starts_with("connect_end "))
        .count();
    assert_eq!(pool_misses, 1, "events = {events:?}");
    assert_eq!(pool_hits, 1, "events = {events:?}");
    assert_eq!(connect_ends, 1, "events = {events:?}");
}

#[tokio::test]
async fn dropping_client_aborts_owned_connection_tasks() {
    let server = spawn_http1(|_request| async move { ok_text("keep-alive") }).await;
    let runtime = AbortCountingRuntime::default();
    let client = Client::builder()
        .executor(runtime.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/abort-on-drop")))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "keep-alive");
    assert_eq!(runtime.aborts(), 0);

    drop(client);

    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.aborts() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client drop should abort idle connection tasks");

    assert!(runtime.aborts() >= 1);
}

#[tokio::test]
async fn https_http2_coalescing_requires_route_overlap() {
    let server_one = spawn_https_http2_with_hosts(&["a.test", "b.test"], |_request| async move {
        ok_text("first h2")
    })
    .await;
    let server_two =
        spawn_https_http2_with_hosts(&["b.test"], |_request| async move { ok_text("second h2") })
            .await;
    let roots = format!(
        "{}\n{}",
        server_one.tls_root_pem().expect("root pem"),
        server_two.tls_root_pem().expect("root pem"),
    );
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server_one.addr()),
            ("b.test".to_owned(), server_two.addr()),
        ]))
        .event_listener_factory(events.clone())
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(roots)
                .expect("root certs")
                .build()
                .expect("tls connector"),
        )
        .build()
        .expect("client");

    let response_one = client
        .execute(empty_request(format!(
            "https://a.test:{}/first",
            server_one.addr().port()
        )))
        .await
        .expect("response");
    let connection_one = response_one
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response_one.into_body().text().await.expect("body");

    let response_two = client
        .execute(empty_request(format!(
            "https://b.test:{}/second",
            server_two.addr().port()
        )))
        .await
        .expect("response");
    let connection_two = response_two
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let body = response_two.into_body().text().await.expect("body");

    assert_ne!(connection_one, connection_two);
    assert_eq!(body, "second h2");
    assert_eq!(
        events
            .events()
            .into_iter()
            .filter(|event| event.starts_with("connect_end "))
            .count(),
        2
    );
}

#[tokio::test]
async fn pool_lookup_events_report_miss_then_hit_for_reused_connections() {
    let server = spawn_http1(|_request| async move { ok_text("pooled") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/first")))
        .await
        .expect("response");
    let connection_id = response
        .extensions()
        .get::<openwire::ConnectionInfo>()
        .expect("connection info")
        .id;
    let _ = response.into_body().text().await.expect("body");

    let response = client
        .execute(empty_request(server.http_url("/second")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "call_start GET",
            "proxy_select_start",
            "proxy_select_end",
            "pool_miss",
            "dns_start",
            "connect_end",
            "connection_acquired ",
            "response_body_start",
            "connection_released ",
            "call_start GET",
            "pool_hit ",
            "connection_acquired ",
            "connection_released ",
        ],
    );
    assert!(
        events.iter().any(|event| event == "pool_miss"),
        "events = {events:?}",
    );
    assert!(
        events
            .iter()
            .any(|event| event == &format!("pool_hit {}", connection_id.as_u64())),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn http1_pool_does_not_reuse_connection_when_request_asks_to_close() {
    let server = spawn_http1(|_request| async move { ok_text("close requested") }).await;
    let connector = AttemptCountingTcpConnector::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/first",
            server.addr().port()
        ))
        .header(CONNECTION, "close")
        .body(RequestBody::empty())
        .expect("request");
    let response = client.execute(request).await.expect("first response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "close requested");
    assert_eq!(connector.attempts(), 1);

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/second",
            server.addr().port()
        )))
        .await
        .expect("second response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "close requested");
    assert_eq!(connector.attempts(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn fast_fallback_dual_stack_routes_emit_observability_and_reduce_latency() {
    let server = spawn_http1(|_request| async move { ok_text("dual-stack race") }).await;
    let fake_ipv6 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0], 9001));
    let fake_ipv4 = SocketAddr::from(([192, 0, 2, 20], 9002));
    let resolver =
        MultiAddrResolver::new([("openwire.test".to_owned(), vec![fake_ipv6, fake_ipv4])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_ipv6,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::from_millis(600),
            },
        ),
        (
            fake_ipv4,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::from_millis(10),
            },
        ),
    ]);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let start = std::time::Instant::now();
    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/race",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    let elapsed = start.elapsed();

    assert_eq!(body, "dual-stack race");
    assert!(
        elapsed < Duration::from_millis(550),
        "elapsed = {elapsed:?} should be faster than sequential fallback",
    );
    assert_eq!(connector.attempts(), 2);

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
async fn fast_fallback_single_stack_multi_ip_routes_do_not_wait_for_first_ipv6() {
    let server = spawn_http1(|_request| async move { ok_text("single-stack race") }).await;
    let fake_aaaa_1 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 1], 9101));
    let fake_aaaa_2 = SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 2], 9102));
    let resolver =
        MultiAddrResolver::new([("openwire.test".to_owned(), vec![fake_aaaa_1, fake_aaaa_2])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_aaaa_1,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::from_millis(600),
            },
        ),
        (
            fake_aaaa_2,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::from_millis(10),
            },
        ),
    ]);
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .build()
        .expect("client");

    let start = std::time::Instant::now();
    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/race-v6",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    let elapsed = start.elapsed();

    assert_eq!(body, "single-stack race");
    assert!(
        elapsed < Duration::from_millis(550),
        "elapsed = {elapsed:?} should be faster than sequential fallback",
    );
    assert_eq!(connector.attempts(), 2);
}

#[tokio::test]
async fn fast_fallback_continues_after_tls_failure_and_only_tlses_the_winner() {
    let server = spawn_http1(|_request| async move { ok_text("tls fallback") }).await;
    let fake_1 = SocketAddr::from(([192, 0, 2, 31], 9201));
    let fake_2 = SocketAddr::from(([192, 0, 2, 32], 9202));
    let resolver = MultiAddrResolver::new([("openwire.test".to_owned(), vec![fake_1, fake_2])]);
    let connector = ScriptedRaceTcpConnector::new([
        (
            fake_1,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::ZERO,
            },
        ),
        (
            fake_2,
            TcpAttemptScript {
                actual_addr: server.addr(),
                delay: Duration::from_millis(10),
            },
        ),
    ]);
    let tls = ScriptedPassThroughTlsConnector::new(vec![
        TlsAttemptScript::Fail(Duration::from_millis(5)),
        TlsAttemptScript::Pass(Duration::from_millis(1)),
    ]);
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(resolver)
        .tcp_connector(connector.clone())
        .tls_connector(tls.clone())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://openwire.test:{}/race-tls",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");

    assert_eq!(body, "tls fallback");
    assert_eq!(connector.attempts(), 2);
    assert_eq!(tls.calls(), 2);

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "route_plan 2 fast_fallback=true",
            "connect_race_lost ",
            "connect_race_won ",
        ],
    );
    let lost_events = events
        .iter()
        .filter(|event| event.starts_with("connect_race_lost "))
        .collect::<Vec<_>>();
    assert_eq!(lost_events.len(), 1, "events = {events:?}");
    assert!(
        lost_events[0].contains("reason=tls_failed"),
        "events = {events:?}",
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
async fn tls_policy_failures_are_not_retried() {
    let server = spawn_https_http1(|_request| async move { ok_text("tls no retry") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "https://localhost:{}/secure-no-retry",
            server.addr().port()
        ))
        .body(RequestBody::empty())
        .expect("request");

    let error = client.execute(request).await.expect_err("tls should fail");
    assert_eq!(error.kind(), WireErrorKind::Tls);
    assert_eq!(error.establishment_stage(), Some(EstablishmentStage::Tls));
    assert!(!error.is_retryable_establishment());

    let events = events.events();
    assert!(
        !events.iter().any(|event| event.starts_with("retry ")),
        "events = {events:?}",
    );
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
async fn post_send_failure_marks_request_committed_for_retry_policy() {
    let accepted_connections = Arc::new(AtomicUsize::new(0));
    let server = spawn_raw_http1_close_after_request_head(accepted_connections.clone()).await;
    let events = RecordingEventListenerFactory::default();
    let policy = RetryIfUncommittedPolicy::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .retry_policy(policy.clone())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/drop-after-send",
            server.addr().port()
        ))
        .body(RequestBody::from_static(b"committed"))
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("server closes before response headers");

    assert!(error.request_committed(), "error = {error:?}");
    assert_eq!(policy.observed_request_committed(), vec![true]);
    assert_eq!(accepted_connections.load(Ordering::Relaxed), 1);

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "connection_acquired ",
            "request_headers_start",
            "request_headers_end",
            "connection_released ",
            "call_failed ",
        ],
    );
    assert!(
        !events.iter().any(|event| event.starts_with("retry ")),
        "events = {events:?}",
    );
}
