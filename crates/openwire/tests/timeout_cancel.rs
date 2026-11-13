mod support;

use std::io::{self};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http::{Request, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, WireError, WireErrorKind};
use openwire_test::{ok_text, spawn_http1, RecordingEventListenerFactory, StaticDnsResolver};
use tokio::sync::oneshot;

use support::*;

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
async fn per_request_call_timeout_overrides_client_default() {
    let server = spawn_http1(|_request| async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        ok_text("slow ok")
    })
    .await;

    let client = Client::builder()
        .call_timeout(std::time::Duration::from_millis(250))
        .build()
        .expect("client");

    let error = client
        .new_call(empty_request(server.http_url("/slow")))
        .call_timeout(std::time::Duration::from_millis(10))
        .execute()
        .await
        .expect_err("per-request timeout should win");
    assert_eq!(error.kind(), WireErrorKind::Timeout);
}

#[tokio::test]
async fn call_timeout_uses_configured_timer() {
    let server = spawn_http1(|_request| async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ok_text("slow ok")
    })
    .await;

    let timer = CountingTimer::immediate();
    let client = Client::builder()
        .timer(timer.clone())
        .call_timeout(Duration::from_millis(25))
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(server.http_url("/slow-runtime")))
        .await
        .expect_err("timeout should use configured timer");
    assert_eq!(error.kind(), WireErrorKind::Timeout);
    assert_eq!(timer.sleep_calls(), 1);
}

#[tokio::test]
async fn canceled_call_fails_before_network_chain() {
    let requests = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let requests = requests.clone();
        move |_request| {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                ok_text("should not run")
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let call = client.new_call(empty_request(server.http_url("/cancel-before")));
    let handle = call.handle();

    assert!(!call.is_executed());
    call.cancel();
    assert!(call.is_canceled());

    let error = call.execute().await.expect_err("canceled call should fail");

    assert_eq!(error.kind(), WireErrorKind::Canceled);
    assert!(handle.is_executed());
    assert!(handle.is_canceled());
    assert_eq!(requests.load(Ordering::Relaxed), 0);
    assert_event_subsequence(
        &events.events(),
        &["call_start GET", "call_failed Canceled"],
    );
}

#[tokio::test]
async fn call_handle_can_cancel_in_flight_execute() {
    let (seen_tx, seen_rx) = oneshot::channel();
    let seen_tx = Arc::new(Mutex::new(Some(seen_tx)));
    let server = spawn_http1({
        let seen_tx = seen_tx.clone();
        move |_request| {
            let seen_tx = seen_tx.clone();
            async move {
                if let Some(sender) = seen_tx.lock().expect("seen lock").take() {
                    let _ = sender.send(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                ok_text("too late")
            }
        }
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let call = client.new_call(empty_request(server.http_url("/cancel-running")));
    let handle = call.handle();
    let task = tokio::spawn(async move { call.execute().await });

    seen_rx.await.expect("server saw request");
    handle.cancel();
    let error = task
        .await
        .expect("execute task")
        .expect_err("cancel should fail in-flight call");

    assert_eq!(error.kind(), WireErrorKind::Canceled);
    assert!(handle.is_executed());
    assert!(handle.is_canceled());
    assert_event_subsequence(
        &events.events(),
        &["call_start GET", "call_failed Canceled"],
    );
}

#[tokio::test]
async fn cancel_after_response_headers_fails_body_read() {
    let server = spawn_http1(|_request| async move { ok_text("cancelable body") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let call = client.new_call(empty_request(server.http_url("/cancel-body")));
    let handle = call.handle();

    let response = call.execute().await.expect("response headers");
    handle.cancel();
    let error = response
        .into_body()
        .text()
        .await
        .expect_err("canceled body should fail");

    assert_eq!(error.kind(), WireErrorKind::Canceled);
    assert_event_subsequence(
        &events.events(),
        &[
            "call_start GET",
            "response_headers_end 200 OK",
            "canceled",
            "call_failed Canceled",
        ],
    );
}

#[tokio::test]
async fn cancel_before_execute_emits_canceled() {
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let call = client.new_call(empty_request("http://example.com/unused"));
    call.cancel();
    assert!(events.events().iter().any(|event| event == "canceled"));
    assert!(
        !events
            .events()
            .iter()
            .any(|event| event.starts_with("call_start")),
        "cancel before execute should not start the call: {:?}",
        events.events()
    );
}

#[tokio::test]
async fn enqueue_emits_dispatcher_queue_events() {
    let server = spawn_http1(|_request| async move { ok_text("queued events") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .new_call(empty_request(server.http_url("/queued-events")))
        .enqueue()
        .expect("queued call")
        .await_response()
        .await
        .expect("queued response");
    let _ = response.into_body().text().await.expect("body");

    assert_event_subsequence(
        &events.events(),
        &[
            "dispatcher_queue_start",
            "call_start GET",
            "proxy_select_start",
            "proxy_select_end",
            "dispatcher_queue_end",
            "follow_up_decision 200 OK next=none",
            "response_body_start",
            "response_body_end",
            "call_end",
        ],
    );
}

#[tokio::test]
async fn present_request_body_emits_start_and_end() {
    let server = spawn_http1(|_request| async move { ok_text("posted") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(server.http_url("/body-events"))
        .body(RequestBody::from_static(b"hello"))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().text().await.expect("body");

    assert_event_subsequence(
        &events.events(),
        &[
            "request_body_start",
            "request_body_end 5",
            "follow_up_decision 200 OK next=none",
            "response_body_start",
            "response_body_end",
        ],
    );
}

#[tokio::test]
async fn absent_request_body_skips_request_body_events() {
    let server = spawn_http1(|_request| async move { ok_text("absent") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let response = client
        .execute(empty_request(server.http_url("/absent-body")))
        .await
        .expect("response");
    let _ = response.into_body().text().await.expect("body");

    assert!(
        !events
            .events()
            .iter()
            .any(|event| event.starts_with("request_body_")),
        "absent bodies should not emit request body events: {:?}",
        events.events()
    );
}

#[tokio::test]
async fn explicit_empty_request_body_emits_start_and_end_zero() {
    let server = spawn_http1(|_request| async move { ok_text("posted") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(server.http_url("/explicit-empty-body-events"))
        .body(RequestBody::explicit_empty())
        .expect("request");
    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().text().await.expect("body");

    assert_event_subsequence(
        &events.events(),
        &[
            "request_headers_start",
            "request_headers_end",
            "request_body_start",
            "request_body_end 0",
            "follow_up_decision 200 OK next=none",
        ],
    );
}

#[tokio::test]
async fn empty_static_request_body_emits_start_and_end_zero() {
    let server = spawn_http1(|_request| async move { ok_text("posted") }).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(server.http_url("/empty-static-body-events"))
        .body(RequestBody::from_static(b""))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    let _ = response.into_body().text().await.expect("body");

    assert_event_subsequence(
        &events.events(),
        &["request_body_start", "request_body_end 0"],
    );
}

#[tokio::test]
async fn streaming_request_body_error_emits_request_failed_not_response_failed() {
    let server = spawn_http1(|request: Request<Incoming>| async move {
        let _ = http_body_util::BodyExt::collect(request.into_body()).await;
        ok_text("unused")
    })
    .await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .max_retries(0)
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(server.http_url("/upload-fail"))
        .body(RequestBody::from_stream(stream::iter(vec![
            Ok::<Bytes, WireError>(Bytes::from_static(b"chunk")),
            Err(WireError::body(
                "upload failed",
                io::Error::other("upload failed"),
            )),
        ])))
        .expect("request");

    let error = client
        .execute(request)
        .await
        .expect_err("upload should fail");
    assert_eq!(error.kind(), WireErrorKind::Body);

    let events = events.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("request_failed")),
        "events = {events:?}",
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_failed")),
        "upload failure should not also emit response_failed: {events:?}",
    );
}

#[tokio::test]
async fn absent_body_header_exchange_failure_emits_response_failed() {
    let accepted_connections = Arc::new(AtomicUsize::new(0));
    let server = spawn_raw_http1_close_after_request_head(accepted_connections.clone()).await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .max_retries(0)
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/drop-after-head",
            server.addr().port()
        )))
        .await
        .expect_err("server closes before response headers");
    assert!(error.request_committed(), "error = {error:?}");

    let events = events.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("response_failed")),
        "events = {events:?}",
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("request_failed")),
        "absent-body send completion is a response-header failure: {events:?}",
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("request_body_")),
        "absent bodies should not emit request body events: {events:?}",
    );
}

#[tokio::test]
async fn response_body_start_waits_for_first_data() {
    let server =
        spawn_raw_http1_headers_then_delayed_body(Duration::from_millis(150), b"hello").await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/delayed-body",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !events
            .events()
            .iter()
            .any(|event| event == "response_body_start"),
        "response_body_start must wait for first byte: {:?}",
        events.events()
    );

    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "hello");
    assert_event_subsequence(
        &events.events(),
        &["response_body_start", "response_body_end 5", "call_end"],
    );
}

#[tokio::test]
async fn queued_call_executes_on_client_executor() {
    let server = spawn_http1(|_request| async move { ok_text("queued ok") }).await;
    let client = Client::builder().build().expect("client");

    let queued = client
        .new_call(empty_request(server.http_url("/queued")))
        .enqueue()
        .expect("queued call");

    assert!(queued.is_executed());
    let response = queued.await_response().await.expect("queued response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "queued ok");
}

#[tokio::test]
async fn queued_call_can_be_canceled() {
    let (seen_tx, seen_rx) = oneshot::channel();
    let seen_tx = Arc::new(Mutex::new(Some(seen_tx)));
    let server = spawn_http1({
        let seen_tx = seen_tx.clone();
        move |_request| {
            let seen_tx = seen_tx.clone();
            async move {
                if let Some(sender) = seen_tx.lock().expect("seen lock").take() {
                    let _ = sender.send(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                ok_text("too late")
            }
        }
    })
    .await;
    let client = Client::builder().build().expect("client");
    let queued = client
        .new_call(empty_request(server.http_url("/queued-cancel")))
        .enqueue()
        .expect("queued call");

    seen_rx.await.expect("server saw request");
    queued.cancel();
    let error = queued
        .await_response()
        .await
        .expect_err("cancel should fail queued call");

    assert_eq!(error.kind(), WireErrorKind::Canceled);
}

#[tokio::test]
async fn dropping_response_body_without_consuming_it_ends_the_body_and_call() {
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
    assert_event_subsequence(
        &events,
        &["response_body_end 0", "connection_released ", "call_end"],
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_body_failed ")),
        "events = {events:?}",
    );
}

#[tokio::test]
async fn call_timeout_can_fail_during_body_read() {
    let server = spawn_raw_http1_headers_then_stall_body(Duration::from_millis(200)).await;
    let events = RecordingEventListenerFactory::default();
    let timer = CountingTimer::tokio();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .timer(timer.clone())
        .event_listener_factory(events.clone())
        .call_timeout(Duration::from_millis(25))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/stalling-body",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let error = response
        .into_body()
        .text()
        .await
        .expect_err("body should time out");
    assert_eq!(error.kind(), WireErrorKind::Timeout);
    assert!(
        timer.sleep_calls() >= 2,
        "call timeout path should use the configured timer for request and body deadlines"
    );

    let events = events.events();
    assert_event_subsequence(
        &events,
        &[
            "call_start GET",
            "response_headers_end 200 OK",
            "response_body_failed timeout",
            "connection_released ",
            "call_failed Timeout",
        ],
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("response_body_end ")),
        "events = {events:?}",
    );
}
