use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use http::header::{ACCEPT_LANGUAGE, CACHE_CONTROL, EXPIRES, VARY};
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

#[tokio::test]
async fn request_no_cache_bypasses_cache_and_refreshes_entry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("body-{hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
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
        .execute(empty_request(server.http_url("/refresh")))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "body-1");

    let refresh = client
        .execute(request_with_header(
            server.http_url("/refresh"),
            CACHE_CONTROL,
            "no-cache",
        ))
        .await
        .expect("refresh response");
    assert_eq!(refresh.into_body().text().await.expect("body"), "body-2");

    let cached = client
        .execute(empty_request(server.http_url("/refresh")))
        .await
        .expect("cached response");
    assert_eq!(cached.into_body().text().await.expect("body"), "body-2");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_no_store_does_not_store_forwarded_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("body-{hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
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
        .execute(request_with_header(
            server.http_url("/no-store-request"),
            CACHE_CONTROL,
            "no-store",
        ))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "body-1");

    let second = client
        .execute(empty_request(server.http_url("/no-store-request")))
        .await
        .expect("second response");
    assert_eq!(second.into_body().text().await.expect("body"), "body-2");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn expires_header_can_make_response_fresh() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "expires body");
                response.headers_mut().insert(
                    EXPIRES,
                    httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60))
                        .parse()
                        .expect("expires"),
                );
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
        .execute(empty_request(server.http_url("/expires")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "expires body"
    );
    let second = client
        .execute(empty_request(server.http_url("/expires")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "expires body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_control_max_age_overrides_expired_expires() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "max-age wins");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response.headers_mut().insert(
                    EXPIRES,
                    httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60))
                        .parse()
                        .expect("expires"),
                );
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
        .execute(empty_request(server.http_url("/max-age-over-expires")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "max-age wins"
    );
    let second = client
        .execute(empty_request(server.http_url("/max-age-over-expires")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "max-age wins"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn response_age_reduces_max_age_before_storage() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "aged body");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert("age", "60".parse().expect("age"));
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
        .execute(empty_request(server.http_url("/aged")))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "aged body");
    let second = client
        .execute(empty_request(server.http_url("/aged")))
        .await
        .expect("second response");
    assert_eq!(second.into_body().text().await.expect("body"), "aged body");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn vary_header_matches_original_request_headers() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let language = request
                    .headers()
                    .get(ACCEPT_LANGUAGE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("none")
                    .to_owned();
                let mut response = text_response(StatusCode::OK, language);
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(VARY, "Accept-Language".parse().expect("vary"));
                response
            }
        }
    })
    .await;
    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");

    let en = request_with_header(server.http_url("/vary"), ACCEPT_LANGUAGE, "en");
    let first = client.execute(en).await.expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "en");

    let en_again = request_with_header(server.http_url("/vary"), ACCEPT_LANGUAGE, "en");
    let second = client.execute(en_again).await.expect("second response");
    assert_eq!(second.into_body().text().await.expect("body"), "en");

    let fr = request_with_header(server.http_url("/vary"), ACCEPT_LANGUAGE, "fr");
    let third = client.execute(fr).await.expect("third response");
    assert_eq!(third.into_body().text().await.expect("body"), "fr");

    let fr_again = request_with_header(server.http_url("/vary"), ACCEPT_LANGUAGE, "fr");
    let fourth = client.execute(fr_again).await.expect("fourth response");
    assert_eq!(fourth.into_body().text().await.expect("body"), "fr");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn vary_star_responses_are_not_cached() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "vary star");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(VARY, "*".parse().expect("vary"));
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
        .execute(empty_request(server.http_url("/vary-star")))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "vary star");
    let second = client
        .execute(empty_request(server.http_url("/vary-star")))
        .await
        .expect("second response");
    assert_eq!(second.into_body().text().await.expect("body"), "vary star");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn only_if_cached_returns_gateway_timeout_on_miss() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                text_response(StatusCode::OK, "network")
            }
        }
    })
    .await;
    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");

    let response = client
        .execute(request_with_header(
            server.http_url("/only-if-cached"),
            CACHE_CONTROL,
            "only-if-cached",
        ))
        .await
        .expect("cache response");

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

fn empty_request(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

fn request_with_header(
    uri: impl AsRef<str>,
    name: http::header::HeaderName,
    value: &'static str,
) -> Request<RequestBody> {
    let mut request = empty_request(uri);
    request
        .headers_mut()
        .insert(name, value.parse().expect("header value"));
    request
}
