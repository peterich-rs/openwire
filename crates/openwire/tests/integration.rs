mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use http::{Request, StatusCode};
use openwire::{Client, RequestBody, WireErrorKind};
use openwire_test::{ok_text, spawn_http1};

use support::*;

#[tokio::test]
async fn basic_get_returns_body() {
    let server = spawn_http1(|_request| async move { ok_text("hello openwire") }).await;
    let client = Client::builder().build().expect("client");

    let request = Request::builder()
        .uri(server.http_url("/hello"))
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "hello openwire");
}

#[tokio::test]
async fn request_uri_userinfo_is_rejected_before_network() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_http1({
        let hits = hits.clone();
        move |_request| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                ok_text("should not be reached")
            }
        }
    })
    .await;
    let client = Client::builder().build().expect("client");

    let error = client
        .execute(empty_request(format!(
            "http://user:pass@127.0.0.1:{}/userinfo",
            server.addr().port()
        )))
        .await
        .expect_err("userinfo must be rejected before network");

    assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    assert!(
        error.message().contains("must not include userinfo"),
        "error = {error}",
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
