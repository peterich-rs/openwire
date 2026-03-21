use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use http::header::CACHE_CONTROL;
use http::{Request, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody};
use openwire_cache::CacheInterceptor;
use openwire_test::{spawn_http1, text_response, RecordingEventListenerFactory};

#[tokio::test]
async fn fresh_get_responses_are_served_from_cache() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "cached body");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
            }
        }
    })
    .await;

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(server.http_url("/cached")))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "cached body");

    let second = client
        .execute(empty_request(server.http_url("/cached")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "cached body"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 1);
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
async fn no_store_responses_are_not_cached() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "uncached body");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "no-store".parse().expect("header"));
                response
            }
        }
    })
    .await;

    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");

    let first = client
        .execute(empty_request(server.http_url("/uncached")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "uncached body"
    );

    let second = client
        .execute(empty_request(server.http_url("/uncached")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "uncached body"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

fn empty_request(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}
