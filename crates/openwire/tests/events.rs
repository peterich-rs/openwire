//! Event order is specified by `EventListener` + `docs/ARCHITECTURE.md` §3,
//! not by dumping whatever the current build emits.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{Method, Request, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, WireErrorKind};
use openwire_test::{
    ok_text, spawn_http1, MockResponse, MockWebServer, RecordingEventListenerFactory,
};

use support::*;

fn get(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .method(Method::GET)
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

#[tokio::test]
async fn known_length_composites_emit_body_completion_once() {
    use bytes::Bytes;
    use futures_util::stream;
    use openwire::WireError;
    use openwire_test::collect_request_body;

    let server = spawn_http1(|request: Request<Incoming>| async move {
        ok_text(collect_request_body(request).await)
    })
    .await;
    for (prefix, payload, nested) in [
        ("", "abc", false),
        ("prefix", "abc", false),
        ("", "", false),
        ("", "abc", true),
    ] {
        let expected = format!("{prefix}{payload}");
        let stream_body = RequestBody::from_stream_with_length(
            stream::iter([Ok::<Bytes, WireError>(Bytes::copy_from_slice(
                payload.as_bytes(),
            ))]),
            payload.len() as u64,
        );
        let mut body = RequestBody::concat(vec![RequestBody::from(prefix.to_owned()), stream_body])
            .expect("concat");
        if nested {
            body = RequestBody::concat(vec![body]).expect("nested concat");
        }
        let events = RecordingEventListenerFactory::default();
        let client = Client::builder()
            .event_listener_factory(events.clone())
            .build()
            .expect("client");
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}/upload", server.addr()))
            .body(body)
            .expect("request");
        let response = client.execute(request).await.expect("response");
        assert_eq!(response.into_body().text().await.expect("body"), expected);
        let recorded = events.events();
        for event in [
            "request_body_start".to_owned(),
            format!("request_body_end {}", expected.len()),
        ] {
            assert_eq!(
                recorded.iter().filter(|actual| *actual == &event).count(),
                1,
                "{recorded:?}"
            );
        }
        events.assert_contains_in_order(&[
            "request_headers_end",
            "request_body_start",
            "request_body_end",
            "call_end",
        ]);
    }
}

#[tokio::test]
async fn successful_http1_get_emits_architecture_lifecycle() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().body("ordered"));

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let response = client
        .execute(get(server.url("/ordered")))
        .await
        .expect("ok");
    let _ = response.into_body().text().await.expect("body");

    events.assert_contains_in_order(&[
        "call_start GET",
        "proxy_select_start",
        "proxy_select_end",
        "dns_start",
        "dns_end",
        "route_plan",
        "connect_start",
        "connect_end",
        "connection_acquired",
        "request_headers_start",
        "request_headers_end",
        "response_headers_start",
        "response_headers_end 200",
        "response_body_start",
        "response_body_end",
        "connection_released",
        "call_end",
    ]);
}

#[tokio::test]
async fn reused_http1_connection_skips_dial_and_reports_pool_hit() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().body("one"));
    server.enqueue(MockResponse::new().body("two"));

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    for path in ["/one", "/two"] {
        let response = client.execute(get(server.url(path))).await.expect("ok");
        let _ = response.into_body().text().await.expect("body");
    }

    events.assert_contains_in_order(&[
        "pool_miss",
        "connection_acquired",
        "call_end",
        "pool_hit",
        "connection_acquired",
        "call_end",
    ]);
}

#[tokio::test]
async fn timeout_emits_call_failed_not_call_end() {
    let server = MockWebServer::http1().await;
    server.enqueue(
        MockResponse::new()
            .delay(Duration::from_secs(2))
            .body("late"),
    );

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .call_timeout(Duration::from_millis(20))
        .build()
        .expect("client");
    let error = client
        .execute(get(server.url("/slow")))
        .await
        .expect_err("timeout");
    assert_eq!(error.kind(), WireErrorKind::Timeout);

    let recorded = events.events();
    assert!(
        recorded
            .iter()
            .any(|event| event.starts_with("call_failed")),
        "timeout must emit call_failed: {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|event| event == "call_end"),
        "EventListener: call_failed replaces call_end on failure: {recorded:?}"
    );
}

#[tokio::test]
async fn not_modified_is_not_a_redirect() {
    let server = MockWebServer::http1().await;
    server.enqueue(MockResponse::new().status(StatusCode::NOT_MODIFIED));

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let response = client
        .execute(get(server.url("/fresh")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let _ = response.into_body().text().await;

    let recorded = events.events();
    assert!(
        !recorded.iter().any(|event| event.starts_with("redirect ")),
        "RFC 9110: 304 is not a redirect: {recorded:?}"
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
            "connection_acquired ",
            "request_headers_start",
            "request_headers_end",
            "response_headers_start",
            "response_headers_end 200 OK",
            "response_body_end 7",
            "connection_released ",
            "call_end",
        ],
    );
}

#[tokio::test]
async fn request_send_events_surround_transport_handoff() {
    let server = spawn_http1(|_request| async move { ok_text("sent") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(server.http_url("/request-send-events")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "connection_acquired ",
            "request_headers_start",
            "request_headers_end",
            "response_headers_start",
            "response_headers_end 200 OK",
        ],
    );
}
