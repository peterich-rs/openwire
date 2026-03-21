use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http::{Request, Version};
use hyper::body::Incoming;
use openwire::{Client, ConnectionInfo, RequestBody, RustlsTlsConnector};
use openwire_test::{ok_text, spawn_https_http2, RecordingEventListenerFactory, TestServer};
use tokio::sync::Barrier;

#[tokio::test]
async fn https_http2_request_negotiates_http2() {
    let server = spawn_https_http2(|_request| async move { ok_text("h2 ok") }).await;
    let client = Client::builder()
        .tls_connector(tls_connector(&server))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "https://localhost:{}/h2",
            server.addr().port()
        )))
        .await
        .expect("response");

    assert_eq!(response.version(), Version::HTTP_2);
    assert!(
        response.extensions().get::<ConnectionInfo>().is_some(),
        "response extensions should include connection info",
    );

    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "h2 ok");
}

#[tokio::test]
async fn warmed_http2_connection_multiplexes_parallel_requests() {
    let active_requests = Arc::new(AtomicUsize::new(0));
    let max_active_requests = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let server = spawn_https_http2({
        let active_requests = active_requests.clone();
        let max_active_requests = max_active_requests.clone();
        let barrier = barrier.clone();
        move |request: Request<Incoming>| {
            let active_requests = active_requests.clone();
            let max_active_requests = max_active_requests.clone();
            let barrier = barrier.clone();
            async move {
                if request.uri().path() == "/warmup" {
                    return ok_text("warm");
                }

                let current = active_requests.fetch_add(1, Ordering::SeqCst) + 1;
                update_max(&max_active_requests, current);
                barrier.wait().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                active_requests.fetch_sub(1, Ordering::SeqCst);

                ok_text("parallel")
            }
        }
    })
    .await;

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_while_idle(true)
        .tls_connector(tls_connector(&server))
        .build()
        .expect("client");

    let warmup = client
        .execute(empty_request(format!(
            "https://localhost:{}/warmup",
            server.addr().port()
        )))
        .await
        .expect("warmup response");
    assert_eq!(warmup.version(), Version::HTTP_2);
    let warmup_connection = connection_id(&warmup);
    let _ = warmup.into_body().text().await.expect("warmup body");

    let request_a = empty_request(format!(
        "https://localhost:{}/parallel-a",
        server.addr().port()
    ));
    let request_b = empty_request(format!(
        "https://localhost:{}/parallel-b",
        server.addr().port()
    ));

    let client_a = client.clone();
    let client_b = client.clone();
    let (response_a, response_b) = tokio::try_join!(
        async move { client_a.execute(request_a).await },
        async move { client_b.execute(request_b).await },
    )
    .expect("parallel responses");

    assert_eq!(response_a.version(), Version::HTTP_2);
    assert_eq!(response_b.version(), Version::HTTP_2);

    let connection_a = connection_id(&response_a);
    let connection_b = connection_id(&response_b);
    assert_eq!(connection_a, warmup_connection);
    assert_eq!(connection_b, warmup_connection);
    assert_eq!(max_active_requests.load(Ordering::SeqCst), 2);

    let (body_a, body_b) = tokio::try_join!(
        async move { response_a.into_body().text().await },
        async move { response_b.into_body().text().await },
    )
    .expect("parallel bodies");
    assert_eq!(body_a, "parallel");
    assert_eq!(body_b, "parallel");

    let connect_events = events
        .events()
        .into_iter()
        .filter(|event| event.starts_with("connect_end "))
        .count();
    assert_eq!(connect_events, 1);
}

fn empty_request(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

fn tls_connector(server: &TestServer) -> RustlsTlsConnector {
    RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector")
}

fn connection_id(response: &http::Response<openwire::ResponseBody>) -> openwire::ConnectionId {
    response
        .extensions()
        .get::<ConnectionInfo>()
        .expect("connection info")
        .id
}

fn update_max(target: &AtomicUsize, candidate: usize) {
    let mut current = target.load(Ordering::SeqCst);
    while candidate > current {
        match target.compare_exchange(current, candidate, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}
