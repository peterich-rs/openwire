//! Event order is specified by `EventListener` + `docs/ARCHITECTURE.md` §3,
//! not by dumping whatever the current build emits.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, WireErrorKind};
use openwire_test::{
    ok_text, spawn_http1, MockResponse, MockWebServer, RecordingEventListenerFactory,
    StaticDnsResolver,
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
async fn tracing_attempt_spans_record_stable_connection_reuse_fields() {
    let _trace_test_guard = trace_test_lock().lock().await;
    let server = spawn_http1(|_request| async move { ok_text("pooled") }).await;
    let trace = TraceCapture::default();
    let client = Client::builder().build().expect("client");

    with_trace_capture(&trace, async {
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
    })
    .await;

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
        spans[0].fields.get("pool_hit").map(String::as_str),
        Some("false")
    );
    assert!(!spans[0].fields.contains_key("pool_connection_id"));
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
        spans[1].fields.get("pool_hit").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        spans[1].fields.get("pool_connection_id"),
        spans[1].fields.get("connection_id"),
        "spans = {spans:?}",
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

#[tokio::test]
async fn retry_and_redirect_events_follow_stable_order_and_trace_fields() {
    let _trace_test_guard = trace_test_lock().lock().await;
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

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector)
        .event_listener_factory(events.clone())
        .max_retries(1)
        .build()
        .expect("client");

    with_trace_capture(&trace, async {
        let response = client
            .execute(empty_request(format!(
                "http://openwire.test:{}/redirect-after-retry",
                server.addr().port()
            )))
            .await
            .expect("response");
        let _ = response.into_body().text().await.expect("body");
    })
    .await;

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
            // Intermediate redirect body is drained so the connection can be
            // reused before the follow-up hop starts.
            "response_body_end ",
            "connection_released ",
            "redirect 1 http://openwire.test:",
            "connection_acquired ",
            "response_headers_end 200 OK",
            "response_body_end 20",
            "connection_released ",
            "call_end",
        ],
    );

    let retry_event = trace
        .event_by_message("retrying request after connection-establishment failure")
        .or_else(|| trace.event_with_field("retry_reason", "connect"))
        .unwrap_or_else(|| {
            panic!(
                "retry trace event missing; captured messages = {:?}",
                trace.event_messages()
            )
        });
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
        retry_event
            .fields
            .get("retry_reason")
            .map(|value| normalize_trace_message(value)),
        Some("connect".to_owned())
    );

    let redirect_event = trace
        .event_by_message("following redirect")
        .or_else(|| {
            trace
                .inner
                .lock()
                .expect("trace capture lock")
                .events
                .iter()
                .find(|event| event.fields.contains_key("redirect_location"))
                .cloned()
        })
        .unwrap_or_else(|| {
            panic!(
                "redirect trace event missing; captured messages = {:?}",
                trace.event_messages()
            )
        });
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
