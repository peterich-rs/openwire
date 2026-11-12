mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Response, StatusCode};
use openwire::{Client, RequestPriority, WireErrorKind};
use openwire_test::{ok_text, spawn_http1, RecordingEventListenerFactory, StaticDnsResolver};

use support::*;

#[tokio::test]
async fn request_limit_total_blocks_parallel_calls_across_hosts() {
    let first_server = spawn_http1(|_request| async move { ok_text("first") }).await;
    let second_server = spawn_http1(|_request| async move { ok_text("second") }).await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("first.test".to_owned(), first_server.addr()),
            ("second.test".to_owned(), second_server.addr()),
        ]))
        .max_requests_total(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://first.test:{}/resource",
            first_server.addr().port()
        )))
        .await
        .expect("first response");
    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request(format!(
                "http://second.test:{}/resource",
                second_server.addr().port()
            )))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should remain blocked while the first response is still live",
    );

    drop(first);

    let response = second.await.expect("second task");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn request_limit_per_host_blocks_parallel_calls_to_same_address() {
    let server = spawn_http1(|_request| async move { ok_text("serialized") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .max_requests_total(2)
        .max_requests_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        )))
        .await
        .expect("first response");
    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request(format!(
                "http://openwire.test:{}/resource",
                server.addr().port()
            )))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should remain blocked while the first same-host response is still live",
    );

    drop(first);

    let response = second.await.expect("second task");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn request_limit_per_host_uses_interceptor_rewritten_address() {
    let server = spawn_http1(|_request| async move { ok_text("rewritten") }).await;
    let client = Client::builder()
        .application_interceptor(RewriteUriInterceptor::new(format!(
            "backend.test:{}",
            server.addr().port()
        )))
        .dns_resolver(HostMapResolver::new([(
            "backend.test".to_owned(),
            server.addr(),
        )]))
        .max_requests_total(2)
        .max_requests_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request("http://placeholder-one.test/resource"))
        .await
        .expect("first response");
    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request("http://placeholder-two.test/resource"))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should remain blocked while the rewritten backend response is still live",
    );

    drop(first);

    let response = second.await.expect("second task");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn request_limit_waiting_on_same_host_does_not_consume_global_capacity() {
    let server_a = spawn_http1(|_request| async move { ok_text("host-a") }).await;
    let server_b = spawn_http1(|_request| async move { ok_text("host-b") }).await;
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("a.test".to_owned(), server_a.addr()),
            ("b.test".to_owned(), server_b.addr()),
        ]))
        .max_requests_total(2)
        .max_requests_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://a.test:{}/resource",
            server_a.addr().port()
        )))
        .await
        .expect("first response");

    let queued_same_host_done = Arc::new(AtomicBool::new(false));
    let queued_same_host_done_clone = queued_same_host_done.clone();
    let queued_same_host_client = client.clone();
    let queued_same_host = tokio::spawn(async move {
        let response = queued_same_host_client
            .execute(empty_request(format!(
                "http://a.test:{}/queued",
                server_a.addr().port()
            )))
            .await
            .expect("queued same-host response");
        queued_same_host_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !queued_same_host_done.load(Ordering::Relaxed),
        "same-host queued call should still be waiting for the per-host slot",
    );

    let other_host_client = client.clone();
    let other_host = tokio::spawn(async move {
        other_host_client
            .execute(empty_request(format!(
                "http://b.test:{}/resource",
                server_b.addr().port()
            )))
            .await
            .expect("other-host response")
    });

    let other_host_response = tokio::time::timeout(Duration::from_millis(200), other_host)
        .await
        .expect("other host should not wait for a same-host queue")
        .expect("other host task");
    assert_eq!(other_host_response.status(), StatusCode::OK);
    assert!(
        !queued_same_host_done.load(Ordering::Relaxed),
        "same-host queued call should remain blocked until the first response is released",
    );

    drop(first);

    let queued_same_host_response =
        tokio::time::timeout(Duration::from_millis(200), queued_same_host)
            .await
            .expect("same-host queued call should complete after release")
            .expect("same-host queued task");
    assert_eq!(queued_same_host_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn request_priority_admits_interactive_before_later_bulk() {
    let server = spawn_http1(|_request| async move { ok_text("priority") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .max_requests_total(1)
        .max_requests_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/held",
            server.addr().port()
        )))
        .await
        .expect("held response");

    let order = Arc::new(Mutex::new(Vec::new()));
    let bulk = {
        let client = client.clone();
        let order = order.clone();
        let url = format!("http://openwire.test:{}/bulk", server.addr().port());
        tokio::spawn(async move {
            let response = client
                .new_call(empty_request(url))
                .priority(RequestPriority::Bulk)
                .execute()
                .await
                .expect("bulk response");
            order.lock().expect("order").push("bulk");
            response
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;

    let interactive = {
        let client = client.clone();
        let order = order.clone();
        let url = format!("http://openwire.test:{}/interactive", server.addr().port());
        tokio::spawn(async move {
            let response = client
                .new_call(empty_request(url))
                .priority(RequestPriority::Interactive)
                .execute()
                .await
                .expect("interactive response");
            order.lock().expect("order").push("interactive");
            response
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;

    drop(first);
    let interactive_response = interactive.await.expect("interactive task");
    assert_eq!(&*order.lock().expect("order"), &["interactive"]);
    drop(interactive_response);
    let bulk_response = bulk.await.expect("bulk task");
    assert_eq!(&*order.lock().expect("order"), &["interactive", "bulk"]);
    drop(bulk_response);
}

#[tokio::test]
async fn max_queued_requests_fails_with_capacity_before_taking_a_slot() {
    let server = spawn_http1(|_request| async move { ok_text("queued") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .max_requests_total(1)
        .max_requests_per_host(1)
        .max_queued_requests(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/held",
            server.addr().port()
        )))
        .await
        .expect("held response");

    let queued = {
        let client = client.clone();
        let url = format!("http://openwire.test:{}/waiter", server.addr().port());
        tokio::spawn(async move { client.execute(empty_request(url)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let error = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/overflow",
            server.addr().port()
        )))
        .await
        .expect_err("queue should be full");
    assert_eq!(error.kind(), WireErrorKind::Capacity);

    drop(first);
    queued
        .await
        .expect("queued task")
        .expect("queued request should complete after a slot frees");
}

#[tokio::test]
async fn connection_limit_per_host_waits_for_existing_connection_to_become_reusable() {
    let server = spawn_http1(|_request| async move { ok_text("reused") }).await;
    let connector = AttemptCountingTcpConnector::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tcp_connector(connector.clone())
        .max_requests_total(2)
        .max_connections_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        )))
        .await
        .expect("first response");
    assert_eq!(connector.attempts(), 1);

    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request(format!(
                "http://openwire.test:{}/resource",
                server.addr().port()
            )))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(connector.attempts(), 1);
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should wait instead of opening a second same-host connection",
    );

    let body = first.into_body().text().await.expect("first body");
    assert_eq!(body, "reused");

    let response = second.await.expect("second task");
    let body = response.into_body().text().await.expect("second body");
    assert_eq!(body, "reused");
    assert_eq!(connector.attempts(), 1);
}

#[tokio::test]
async fn connection_limit_per_host_tolerates_dns_failure_when_busy_connection_can_be_reused() {
    let server = spawn_http1(|_request| async move { ok_text("reused after dns failure") }).await;
    let connector = AttemptCountingTcpConnector::default();
    let resolver = ScriptedDnsResolver::new([(
        "openwire.test".to_owned(),
        vec![
            DnsAttemptScript::Success(server.addr()),
            DnsAttemptScript::Fail,
        ],
    )]);
    let client = Client::builder()
        .dns_resolver(resolver.clone())
        .tcp_connector(connector.clone())
        .max_requests_total(2)
        .max_connections_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        )))
        .await
        .expect("first response");
    assert_eq!(connector.attempts(), 1);
    assert_eq!(resolver.calls(), 1);

    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request(format!(
                "http://openwire.test:{}/resource",
                server.addr().port()
            )))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(connector.attempts(), 1);
    assert_eq!(resolver.calls(), 2);
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should wait for the busy pooled connection instead of failing DNS",
    );

    let body = first.into_body().text().await.expect("first body");
    assert_eq!(body, "reused after dns failure");

    let response = tokio::time::timeout(Duration::from_millis(200), second)
        .await
        .expect("second call should complete after release")
        .expect("second task");
    let body = response.into_body().text().await.expect("second body");
    assert_eq!(body, "reused after dns failure");
    assert_eq!(connector.attempts(), 1);
    assert_eq!(resolver.calls(), 2);
}

#[tokio::test]
async fn connection_limit_per_host_does_not_suppress_non_dns_route_plan_failure() {
    let server =
        spawn_http1(|_request| async move { ok_text("reused after planner failure") }).await;
    let connector = AttemptCountingTcpConnector::default();
    let planner = ScriptedRoutePlanner::fail_on_plan_call(2);
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .route_planner(planner.clone())
        .tcp_connector(connector.clone())
        .max_requests_total(2)
        .max_connections_per_host(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        )))
        .await
        .expect("first response");
    assert_eq!(connector.attempts(), 1);
    assert_eq!(planner.calls(), 1);

    let error = tokio::time::timeout(
        Duration::from_millis(100),
        client.execute(empty_request(format!(
            "http://openwire.test:{}/resource",
            server.addr().port()
        ))),
    )
    .await
    .expect("non-dns route-plan failure should surface without waiting")
    .expect_err("second response should fail");

    assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    assert_eq!(error.message(), "scripted route planner failure");
    assert_eq!(connector.attempts(), 1);
    assert_eq!(planner.calls(), 2);

    let body = first.into_body().text().await.expect("first body");
    assert_eq!(body, "reused after planner failure");
}

#[tokio::test]
async fn connection_limit_total_waits_for_an_existing_connection_to_close() {
    let first_server = spawn_http1(|_request| async move {
        Response::builder()
            .status(StatusCode::OK)
            .header("connection", "close")
            .body(http_body_util::Full::new(Bytes::from_static(b"first")))
            .expect("response")
    })
    .await;
    let second_server = spawn_http1(|_request| async move { ok_text("second") }).await;
    let connector = AttemptCountingTcpConnector::default();
    let client = Client::builder()
        .dns_resolver(HostMapResolver::new([
            ("first.test".to_owned(), first_server.addr()),
            ("second.test".to_owned(), second_server.addr()),
        ]))
        .tcp_connector(connector.clone())
        .max_requests_total(2)
        .max_connections_total(1)
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(format!(
            "http://first.test:{}/resource",
            first_server.addr().port()
        )))
        .await
        .expect("first response");
    assert_eq!(connector.attempts(), 1);

    let second_done = Arc::new(AtomicBool::new(false));
    let second_done_clone = second_done.clone();
    let second_client = client.clone();
    let second = tokio::spawn(async move {
        let response = second_client
            .execute(empty_request(format!(
                "http://second.test:{}/resource",
                second_server.addr().port()
            )))
            .await
            .expect("second response");
        second_done_clone.store(true, Ordering::Relaxed);
        response
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(connector.attempts(), 1);
    assert!(
        !second_done.load(Ordering::Relaxed),
        "second call should wait while the only allowed connection slot is occupied",
    );

    let body = first.into_body().text().await.expect("first body");
    assert_eq!(body, "first");

    let response = second.await.expect("second task");
    let body = response.into_body().text().await.expect("second body");
    assert_eq!(body, "second");
    assert_eq!(connector.attempts(), 2);
}

#[tokio::test]
async fn tokio_tcp_connector_emits_single_connect_failed_event_for_connection_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let addr = listener.local_addr().expect("listener addr");
    drop(listener);

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(addr))
        .event_listener_factory(events.clone())
        .retry_on_connection_failure(false)
        .max_retries(0)
        .build()
        .expect("client");

    let error = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/refused",
            addr.port()
        )))
        .await
        .expect_err("connect should fail");
    assert_eq!(error.kind(), WireErrorKind::Connect);

    let events = events.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("connect_failed "))
            .count(),
        1,
        "events = {events:?}",
    );
    assert!(
        events
            .iter()
            .all(|event| !event.starts_with("connect_end ")),
        "events = {events:?}",
    );
}
