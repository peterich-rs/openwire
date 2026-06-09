use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use http::header::{
    ACCEPT_LANGUAGE, AGE, AUTHORIZATION, CACHE_CONTROL, CONTENT_LOCATION, DATE, ETAG, EXPIRES,
    HOST, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION, PRAGMA, VARY,
};
use http::{Method, Request, StatusCode};
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
async fn authorized_response_without_explicit_permission_is_not_cached() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("authorized body {hit}"));
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
            server.http_url("/authorized-private"),
            AUTHORIZATION,
            "Bearer one",
        ))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "authorized body 1"
    );

    let second = client
        .execute(request_with_header(
            server.http_url("/authorized-private"),
            AUTHORIZATION,
            "Bearer one",
        ))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "authorized body 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authorized_public_response_is_cached_per_authorization_value() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("anonymous")
                    .to_owned();
                let mut response =
                    text_response(StatusCode::OK, format!("{authorization} hit {hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "public, max-age=60".parse().expect("header"));
                response
            }
        }
    })
    .await;

    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");
    let uri = server.http_url("/authorized-public");

    let first = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "Bearer one hit 1"
    );

    let same_token = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("same token response");
    assert_eq!(
        same_token.into_body().text().await.expect("body"),
        "Bearer one hit 1"
    );

    let different_token = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer two"))
        .await
        .expect("different token response");
    assert_eq!(
        different_token.into_body().text().await.expect("body"),
        "Bearer two hit 2"
    );

    let unauthenticated = client
        .execute(empty_request(&uri))
        .await
        .expect("unauthenticated response");
    assert_eq!(
        unauthenticated.into_body().text().await.expect("body"),
        "anonymous hit 3"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn authorized_must_revalidate_response_is_stored_and_keeps_authorization_match_after_304() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("anonymous")
                    .to_owned();

                if hit == 2 {
                    assert_eq!(authorization, "Bearer one");
                    assert_eq!(
                        request.headers().get(IF_NONE_MATCH).expect("if-none-match"),
                        "\"authorized-v1\""
                    );
                    let mut response = text_response(StatusCode::NOT_MODIFIED, "");
                    response.headers_mut().insert(
                        CACHE_CONTROL,
                        "public, max-age=60, must-revalidate"
                            .parse()
                            .expect("header"),
                    );
                    response
                        .headers_mut()
                        .insert(ETAG, "\"authorized-v1\"".parse().expect("etag"));
                    return response;
                }

                let mut response =
                    text_response(StatusCode::OK, format!("{authorization} body {hit}"));
                response.headers_mut().insert(
                    CACHE_CONTROL,
                    "max-age=0, must-revalidate".parse().expect("header"),
                );
                response
                    .headers_mut()
                    .insert(ETAG, "\"authorized-v1\"".parse().expect("etag"));
                response
            }
        }
    })
    .await;

    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");
    let uri = server.http_url("/authorized-must-revalidate");

    let first = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "Bearer one body 1"
    );

    let revalidated = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("revalidated response");
    assert_eq!(revalidated.status(), StatusCode::OK);
    assert_eq!(
        revalidated.into_body().text().await.expect("body"),
        "Bearer one body 1"
    );

    let cached_after_304 = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("cached response");
    assert_eq!(
        cached_after_304.into_body().text().await.expect("body"),
        "Bearer one body 1"
    );

    let different_token = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer two"))
        .await
        .expect("different token response");
    assert_eq!(
        different_token.into_body().text().await.expect("body"),
        "Bearer two body 3"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn authorized_request_does_not_reuse_generic_cached_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("anonymous")
                    .to_owned();
                let mut response =
                    text_response(StatusCode::OK, format!("{authorization} generic {hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "public, max-age=60".parse().expect("header"));
                response
            }
        }
    })
    .await;

    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");
    let uri = server.http_url("/generic-then-authorized");

    let first = client
        .execute(empty_request(&uri))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "anonymous generic 1"
    );

    let authorized = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("authorized response");
    assert_eq!(
        authorized.into_body().text().await.expect("body"),
        "Bearer one generic 2"
    );

    let anonymous_again = client
        .execute(empty_request(&uri))
        .await
        .expect("anonymous cached response");
    assert_eq!(
        anonymous_again.into_body().text().await.expect("body"),
        "anonymous generic 1"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authorized_no_store_response_is_not_cached() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("no-store auth {hit}"));
                response.headers_mut().insert(
                    CACHE_CONTROL,
                    "public, max-age=60, no-store".parse().expect("header"),
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
    let uri = server.http_url("/authorized-no-store");

    let first = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "no-store auth 1"
    );

    let second = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "no-store auth 2"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authorized_no_store_refresh_removes_matching_authorization_variant() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("anonymous")
                    .to_owned();
                let mut response =
                    text_response(StatusCode::OK, format!("{authorization} refresh {hit}"));
                if request.headers().contains_key(CACHE_CONTROL) {
                    response.headers_mut().insert(
                        CACHE_CONTROL,
                        "public, max-age=60, no-store".parse().expect("header"),
                    );
                } else {
                    response
                        .headers_mut()
                        .insert(CACHE_CONTROL, "public, max-age=60".parse().expect("header"));
                }
                response
            }
        }
    })
    .await;

    let client = Client::builder()
        .application_interceptor(CacheInterceptor::memory())
        .build()
        .expect("client");
    let uri = server.http_url("/authorized-no-store-refresh");

    let first = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "Bearer one refresh 1"
    );

    let bypass = client
        .execute(request_with_headers(
            &uri,
            AUTHORIZATION,
            "Bearer one",
            CACHE_CONTROL,
            "no-cache",
        ))
        .await
        .expect("bypass response");
    assert_eq!(
        bypass.into_body().text().await.expect("body"),
        "Bearer one refresh 2"
    );

    let after_no_store = client
        .execute(request_with_header(&uri, AUTHORIZATION, "Bearer one"))
        .await
        .expect("after no-store response");
    assert_eq!(
        after_no_store.into_body().text().await.expect("body"),
        "Bearer one refresh 3"
    );

    assert_eq!(hits.load(Ordering::SeqCst), 3);
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
async fn request_pragma_no_cache_without_cache_control_bypasses_cache_and_refreshes_entry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("pragma-body-{hit}"));
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
        .execute(empty_request(server.http_url("/pragma-refresh")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "pragma-body-1"
    );

    let refresh = client
        .execute(request_with_header(
            server.http_url("/pragma-refresh"),
            PRAGMA,
            "no-cache",
        ))
        .await
        .expect("refresh response");
    assert_eq!(
        refresh.into_body().text().await.expect("body"),
        "pragma-body-2"
    );

    let cached = client
        .execute(empty_request(server.http_url("/pragma-refresh")))
        .await
        .expect("cached response");
    assert_eq!(
        cached.into_body().text().await.expect("body"),
        "pragma-body-2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_cache_control_takes_precedence_over_pragma_no_cache() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response =
                    text_response(StatusCode::OK, format!("cache-control-body-{hit}"));
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
        .execute(empty_request(server.http_url("/pragma-precedence")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "cache-control-body-1"
    );

    let cached = client
        .execute(request_with_headers(
            server.http_url("/pragma-precedence"),
            CACHE_CONTROL,
            "max-age=60",
            PRAGMA,
            "no-cache",
        ))
        .await
        .expect("cached response");
    assert_eq!(
        cached.into_body().text().await.expect("body"),
        "cache-control-body-1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
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
async fn successful_unsafe_method_invalidates_cached_target_uri() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if request.method() == Method::POST {
                    return text_response(StatusCode::OK, "updated");
                }

                let mut response = text_response(StatusCode::OK, format!("cached-body-{hit}"));
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
    let uri = server.http_url("/unsafe-invalidation");

    let first = client
        .execute(empty_request(&uri))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "cached-body-1"
    );

    let post = client
        .execute(method_request(&uri, Method::POST))
        .await
        .expect("post response");
    assert_eq!(post.into_body().text().await.expect("body"), "updated");

    let refreshed = client
        .execute(empty_request(&uri))
        .await
        .expect("refreshed response");
    assert_eq!(
        refreshed.into_body().text().await.expect("body"),
        "cached-body-3"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn successful_unsafe_method_invalidates_same_host_location_uris() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let path = request.uri().path().to_owned();
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if request.method() == Method::POST {
                    let host = request
                        .headers()
                        .get(HOST)
                        .expect("host header")
                        .to_str()
                        .expect("host value");
                    let mut response = text_response(StatusCode::CREATED, "created");
                    response.headers_mut().insert(
                        LOCATION,
                        format!("http://{host}/location-resource")
                            .parse()
                            .expect("location"),
                    );
                    response.headers_mut().insert(
                        CONTENT_LOCATION,
                        "/content-location-resource"
                            .parse()
                            .expect("content-location"),
                    );
                    return response;
                }

                let mut response = text_response(StatusCode::OK, format!("{path}-{hit}"));
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

    let location_uri = server.http_url("/location-resource");
    let content_location_uri = server.http_url("/content-location-resource");
    assert_eq!(
        client
            .execute(empty_request(&location_uri))
            .await
            .expect("first location response")
            .into_body()
            .text()
            .await
            .expect("body"),
        "/location-resource-1"
    );
    assert_eq!(
        client
            .execute(empty_request(&content_location_uri))
            .await
            .expect("first content-location response")
            .into_body()
            .text()
            .await
            .expect("body"),
        "/content-location-resource-2"
    );

    let post = client
        .execute(method_request(
            server.http_url("/unsafe-create"),
            Method::POST,
        ))
        .await
        .expect("post response");
    assert_eq!(post.status(), StatusCode::CREATED);
    assert_eq!(post.into_body().text().await.expect("body"), "created");

    assert_eq!(
        client
            .execute(empty_request(&location_uri))
            .await
            .expect("refreshed location response")
            .into_body()
            .text()
            .await
            .expect("body"),
        "/location-resource-4"
    );
    assert_eq!(
        client
            .execute(empty_request(&content_location_uri))
            .await
            .expect("refreshed content-location response")
            .into_body()
            .text()
            .await
            .expect("body"),
        "/content-location-resource-5"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn unsafe_method_does_not_invalidate_cross_host_location_uri() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if request.method() == Method::POST {
                    let mut response = text_response(StatusCode::OK, "updated elsewhere");
                    response.headers_mut().insert(
                        LOCATION,
                        "http://untrusted.example/cross-host-resource"
                            .parse()
                            .expect("location"),
                    );
                    return response;
                }

                let mut response = text_response(StatusCode::OK, format!("cached-body-{hit}"));
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
    let uri = server.http_url("/cross-host-resource");

    let first = client
        .execute(empty_request(&uri))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "cached-body-1"
    );

    let post = client
        .execute(method_request(
            server.http_url("/unsafe-cross-host"),
            Method::POST,
        ))
        .await
        .expect("post response");
    assert_eq!(
        post.into_body().text().await.expect("body"),
        "updated elsewhere"
    );

    let cached = client
        .execute(empty_request(&uri))
        .await
        .expect("cached response");
    assert_eq!(
        cached.into_body().text().await.expect("body"),
        "cached-body-1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn error_response_to_unsafe_method_keeps_cached_target_uri() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if request.method() == Method::POST {
                    return text_response(StatusCode::INTERNAL_SERVER_ERROR, "failed");
                }

                let mut response = text_response(StatusCode::OK, format!("cached-body-{hit}"));
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
    let uri = server.http_url("/unsafe-error-keeps-cache");

    let first = client
        .execute(empty_request(&uri))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "cached-body-1"
    );

    let post = client
        .execute(method_request(&uri, Method::POST))
        .await
        .expect("post response");
    assert_eq!(post.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(post.into_body().text().await.expect("body"), "failed");

    let cached = client
        .execute(empty_request(&uri))
        .await
        .expect("cached response");
    assert_eq!(
        cached.into_body().text().await.expect("body"),
        "cached-body-1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_max_age_uses_response_apparent_age() {
    let hits = Arc::new(AtomicUsize::new(0));
    let date = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(120));
    let server = spawn_http1({
        let hits = hits.clone();
        let date = date.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            let date = date.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("apparent-age {hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=300".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(DATE, date.parse().expect("date"));
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
        .execute(empty_request(server.http_url("/request-max-age")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "apparent-age 1"
    );

    let second = client
        .execute(request_with_header(
            server.http_url("/request-max-age"),
            CACHE_CONTROL,
            "max-age=60",
        ))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "apparent-age 2"
    );
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
async fn last_modified_response_uses_heuristic_freshness_when_explicit_freshness_absent() {
    let hits = Arc::new(AtomicUsize::new(0));
    let now = SystemTime::now();
    let date = httpdate::fmt_http_date(now);
    let last_modified = httpdate::fmt_http_date(now - Duration::from_secs(1_000));
    let server = spawn_http1({
        let hits = hits.clone();
        let date = date.clone();
        let last_modified = last_modified.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            let date = date.clone();
            let last_modified = last_modified.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "heuristic body");
                response
                    .headers_mut()
                    .insert(DATE, date.parse().expect("date"));
                response
                    .headers_mut()
                    .insert(LAST_MODIFIED, last_modified.parse().expect("last-modified"));
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
        .execute(empty_request(server.http_url("/heuristic")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "heuristic body"
    );
    let second = client
        .execute(empty_request(server.http_url("/heuristic")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "heuristic body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_expired_expires_prevents_last_modified_heuristic_reuse() {
    let hits = Arc::new(AtomicUsize::new(0));
    let now = SystemTime::now();
    let date = httpdate::fmt_http_date(now);
    let expires = httpdate::fmt_http_date(now - Duration::from_secs(60));
    let last_modified = httpdate::fmt_http_date(now - Duration::from_secs(1_000));
    let server = spawn_http1({
        let hits = hits.clone();
        let date = date.clone();
        let expires = expires.clone();
        let last_modified = last_modified.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            let date = date.clone();
            let expires = expires.clone();
            let last_modified = last_modified.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit > 1 {
                    assert_eq!(
                        request
                            .headers()
                            .get(IF_MODIFIED_SINCE)
                            .expect("if-modified-since")
                            .to_str()
                            .expect("header value"),
                        last_modified
                    );
                }
                let mut response = text_response(StatusCode::OK, format!("expired body {hit}"));
                response
                    .headers_mut()
                    .insert(DATE, date.parse().expect("date"));
                response
                    .headers_mut()
                    .insert(EXPIRES, expires.parse().expect("expires"));
                response
                    .headers_mut()
                    .insert(LAST_MODIFIED, last_modified.parse().expect("last-modified"));
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
        .execute(empty_request(server.http_url("/expired-expires")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "expired body 1"
    );
    let second = client
        .execute(empty_request(server.http_url("/expired-expires")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "expired body 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn invalid_expires_prevents_last_modified_heuristic_reuse() {
    let hits = Arc::new(AtomicUsize::new(0));
    let now = SystemTime::now();
    let date = httpdate::fmt_http_date(now);
    let last_modified = httpdate::fmt_http_date(now - Duration::from_secs(1_000));
    let server = spawn_http1({
        let hits = hits.clone();
        let date = date.clone();
        let last_modified = last_modified.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            let date = date.clone();
            let last_modified = last_modified.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit > 1 {
                    assert_eq!(
                        request
                            .headers()
                            .get(IF_MODIFIED_SINCE)
                            .expect("if-modified-since")
                            .to_str()
                            .expect("header value"),
                        last_modified
                    );
                }
                let mut response =
                    text_response(StatusCode::OK, format!("invalid expires body {hit}"));
                response
                    .headers_mut()
                    .insert(DATE, date.parse().expect("date"));
                response
                    .headers_mut()
                    .insert(EXPIRES, "0".parse().expect("expires"));
                response
                    .headers_mut()
                    .insert(LAST_MODIFIED, last_modified.parse().expect("last-modified"));
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
        .execute(empty_request(server.http_url("/invalid-expires")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "invalid expires body 1"
    );
    let second = client
        .execute(empty_request(server.http_url("/invalid-expires")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "invalid expires body 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
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
async fn duplicate_response_max_age_directives_are_stale() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response =
                    text_response(StatusCode::OK, format!("duplicate max-age {hit}"));
                response.headers_mut().insert(
                    CACHE_CONTROL,
                    "max-age=0, max-age=60".parse().expect("header"),
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
        .execute(empty_request(server.http_url("/duplicate-max-age")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "duplicate max-age 1"
    );
    let second = client
        .execute(empty_request(server.http_url("/duplicate-max-age")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "duplicate max-age 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn duplicate_response_max_age_across_header_fields_is_stale() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response =
                    text_response(StatusCode::OK, format!("duplicate max-age field {hit}"));
                response
                    .headers_mut()
                    .append(CACHE_CONTROL, "max-age=0".parse().expect("header"));
                response
                    .headers_mut()
                    .append(CACHE_CONTROL, "max-age=60".parse().expect("header"));
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
        .execute(empty_request(server.http_url("/duplicate-max-age-fields")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "duplicate max-age field 1"
    );
    let second = client
        .execute(empty_request(server.http_url("/duplicate-max-age-fields")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "duplicate max-age field 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn duplicate_expires_headers_are_stale_when_max_age_is_absent() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response =
                    text_response(StatusCode::OK, format!("duplicate expires {hit}"));
                response.headers_mut().append(
                    EXPIRES,
                    httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60))
                        .parse()
                        .expect("expires"),
                );
                response.headers_mut().append(
                    EXPIRES,
                    httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120))
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
        .execute(empty_request(server.http_url("/duplicate-expires")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "duplicate expires 1"
    );
    let second = client
        .execute(empty_request(server.http_url("/duplicate-expires")))
        .await
        .expect("second response");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "duplicate expires 2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
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
async fn stale_etag_response_revalidates_with_if_none_match() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit == 1 {
                    let mut response = text_response(StatusCode::OK, "etag body");
                    response
                        .headers_mut()
                        .insert(CACHE_CONTROL, "max-age=0".parse().expect("header"));
                    response
                        .headers_mut()
                        .insert(AGE, "30".parse().expect("age"));
                    response
                        .headers_mut()
                        .insert(ETAG, "\"v1\"".parse().expect("etag"));
                    return response;
                }

                assert_eq!(
                    request.headers().get(IF_NONE_MATCH).expect("if-none-match"),
                    "\"v1\""
                );
                let mut response = text_response(StatusCode::NOT_MODIFIED, "");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(ETAG, "\"v1\"".parse().expect("etag"));
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
        .execute(empty_request(server.http_url("/etag-revalidate")))
        .await
        .expect("first response");
    assert_eq!(first.into_body().text().await.expect("body"), "etag body");

    let revalidated = client
        .execute(empty_request(server.http_url("/etag-revalidate")))
        .await
        .expect("revalidated response");
    assert_eq!(revalidated.status(), StatusCode::OK);
    let revalidated_age = revalidated
        .headers()
        .get(AGE)
        .expect("age")
        .to_str()
        .expect("age string")
        .parse::<u64>()
        .expect("age seconds");
    assert!(revalidated_age < 30);
    assert_eq!(
        revalidated.into_body().text().await.expect("body"),
        "etag body"
    );

    let cached_after_304 = client
        .execute(empty_request(server.http_url("/etag-revalidate")))
        .await
        .expect("cached response");
    assert_eq!(
        cached_after_304.into_body().text().await.expect("body"),
        "etag body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stale_last_modified_response_revalidates_with_if_modified_since() {
    let hits = Arc::new(AtomicUsize::new(0));
    let last_modified = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(120));
    let server = spawn_http1({
        let hits = hits.clone();
        let last_modified = last_modified.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            let last_modified = last_modified.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit == 1 {
                    let mut response = text_response(StatusCode::OK, "last-modified body");
                    response
                        .headers_mut()
                        .insert(CACHE_CONTROL, "max-age=0".parse().expect("header"));
                    response
                        .headers_mut()
                        .insert(LAST_MODIFIED, last_modified.parse().expect("last-modified"));
                    return response;
                }

                assert_eq!(
                    request
                        .headers()
                        .get(IF_MODIFIED_SINCE)
                        .expect("if-modified-since")
                        .to_str()
                        .expect("header value"),
                    last_modified
                );
                let mut response = text_response(StatusCode::NOT_MODIFIED, "");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(LAST_MODIFIED, last_modified.parse().expect("last-modified"));
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
        .execute(empty_request(server.http_url("/last-modified-revalidate")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "last-modified body"
    );

    let revalidated = client
        .execute(empty_request(server.http_url("/last-modified-revalidate")))
        .await
        .expect("revalidated response");
    assert_eq!(revalidated.status(), StatusCode::OK);
    assert_eq!(
        revalidated.into_body().text().await.expect("body"),
        "last-modified body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_max_stale_serves_explicitly_permitted_stale_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = text_response(StatusCode::OK, format!("stale body {hit}"));
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=0".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(ETAG, "\"stale-v1\"".parse().expect("etag"));
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
        .execute(empty_request(server.http_url("/max-stale")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "stale body 1"
    );

    let stale = client
        .execute(request_with_header(
            server.http_url("/max-stale"),
            CACHE_CONTROL,
            "max-stale",
        ))
        .await
        .expect("stale response");
    assert_eq!(
        stale.into_body().text().await.expect("body"),
        "stale body 1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_max_age_zero_with_max_stale_can_serve_recent_stale_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response =
                    text_response(StatusCode::OK, format!("recent stale body {hit}"));
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
        .execute(empty_request(server.http_url("/max-age-zero-max-stale")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "recent stale body 1"
    );

    let stale = client
        .execute(request_with_header(
            server.http_url("/max-age-zero-max-stale"),
            CACHE_CONTROL,
            "max-age=0, max-stale=60",
        ))
        .await
        .expect("stale response");
    assert_eq!(
        stale.into_body().text().await.expect("body"),
        "recent stale body 1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_max_stale_limit_revalidates_when_staleness_exceeds_limit() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit == 1 {
                    let mut response = text_response(StatusCode::OK, "too stale body");
                    response
                        .headers_mut()
                        .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                    response
                        .headers_mut()
                        .insert(AGE, "120".parse().expect("age"));
                    response
                        .headers_mut()
                        .insert(ETAG, "\"too-stale-v1\"".parse().expect("etag"));
                    return response;
                }

                assert_eq!(
                    request.headers().get(IF_NONE_MATCH).expect("if-none-match"),
                    "\"too-stale-v1\""
                );
                let mut response = text_response(StatusCode::NOT_MODIFIED, "");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(ETAG, "\"too-stale-v1\"".parse().expect("etag"));
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
        .execute(empty_request(server.http_url("/max-stale-limit")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "too stale body"
    );

    let revalidated = client
        .execute(request_with_header(
            server.http_url("/max-stale-limit"),
            CACHE_CONTROL,
            "max-stale=30",
        ))
        .await
        .expect("revalidated response");
    assert_eq!(
        revalidated.into_body().text().await.expect("body"),
        "too stale body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn response_must_revalidate_blocks_request_max_stale() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                if hit == 1 {
                    let mut response = text_response(StatusCode::OK, "must revalidate body");
                    response.headers_mut().insert(
                        CACHE_CONTROL,
                        "max-age=0, must-revalidate".parse().expect("header"),
                    );
                    response
                        .headers_mut()
                        .insert(ETAG, "\"must-revalidate-v1\"".parse().expect("etag"));
                    return response;
                }

                assert_eq!(
                    request.headers().get(IF_NONE_MATCH).expect("if-none-match"),
                    "\"must-revalidate-v1\""
                );
                let mut response = text_response(StatusCode::NOT_MODIFIED, "");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(ETAG, "\"must-revalidate-v1\"".parse().expect("etag"));
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
        .execute(empty_request(server.http_url("/must-revalidate")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "must revalidate body"
    );

    let revalidated = client
        .execute(request_with_header(
            server.http_url("/must-revalidate"),
            CACHE_CONTROL,
            "max-stale=60",
        ))
        .await
        .expect("revalidated response");
    assert_eq!(
        revalidated.into_body().text().await.expect("body"),
        "must revalidate body"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cached_hits_generate_current_age_header() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "aged cache hit");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=60".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(AGE, "5".parse().expect("age"));
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
        .execute(empty_request(server.http_url("/cache-age")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "aged cache hit"
    );

    let second = client
        .execute(empty_request(server.http_url("/cache-age")))
        .await
        .expect("cached response");
    let age = second
        .headers()
        .get(AGE)
        .expect("age")
        .to_str()
        .expect("age string")
        .parse::<u64>()
        .expect("age seconds");
    assert!(age >= 5);
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "aged cache hit"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cached_hits_include_apparent_age_from_response_date() {
    let hits = Arc::new(AtomicUsize::new(0));
    let date = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(120));
    let server = spawn_http1({
        let hits = hits.clone();
        let date = date.clone();
        move |_request: Request<Incoming>| {
            let hits = hits.clone();
            let date = date.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut response = text_response(StatusCode::OK, "dated cache hit");
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, "max-age=300".parse().expect("header"));
                response
                    .headers_mut()
                    .insert(DATE, date.parse().expect("date"));
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
        .execute(empty_request(server.http_url("/cache-date-age")))
        .await
        .expect("first response");
    assert_eq!(
        first.into_body().text().await.expect("body"),
        "dated cache hit"
    );

    let second = client
        .execute(empty_request(server.http_url("/cache-date-age")))
        .await
        .expect("cached response");
    let age = second
        .headers()
        .get(AGE)
        .expect("age")
        .to_str()
        .expect("age string")
        .parse::<u64>()
        .expect("age seconds");
    assert!(age >= 120, "age = {age}");
    assert_eq!(
        second.into_body().text().await.expect("body"),
        "dated cache hit"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
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

    let en_after_fr = request_with_header(server.http_url("/vary"), ACCEPT_LANGUAGE, "en");
    let fifth = client.execute(en_after_fr).await.expect("fifth response");
    assert_eq!(fifth.into_body().text().await.expect("body"), "en");
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

fn method_request(uri: impl AsRef<str>, method: Method) -> Request<RequestBody> {
    Request::builder()
        .method(method)
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

fn request_with_headers(
    uri: impl AsRef<str>,
    first_name: http::header::HeaderName,
    first_value: &'static str,
    second_name: http::header::HeaderName,
    second_value: &'static str,
) -> Request<RequestBody> {
    let mut request = request_with_header(uri, first_name, first_value);
    request
        .headers_mut()
        .insert(second_name, second_value.parse().expect("header value"));
    request
}
