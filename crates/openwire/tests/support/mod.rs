use std::sync::Arc;
use std::time::Duration;

use http::{Method, Request, Response};
use openwire::{Client, Jar, RequestBody, ResponseBody, WireError, WireErrorKind};
use serde_json::Value;

const SHORT_TIMEOUT: Duration = Duration::from_millis(500);

pub fn standard_client() -> Client {
    Client::builder().build().expect("standard live client")
}

pub fn cookie_client() -> Client {
    let jar = Arc::new(Jar::new());
    Client::builder()
        .cookie_jar(jar)
        .build()
        .expect("cookie live client")
}

pub fn short_timeout_client() -> Client {
    Client::builder()
        .call_timeout(SHORT_TIMEOUT)
        .build()
        .expect("short-timeout live client")
}

pub fn no_redirect_client() -> Client {
    Client::builder()
        .follow_redirects(false)
        .build()
        .expect("no-redirect live client")
}

pub fn httpbingo(path: &str) -> String {
    format!("https://httpbingo.org{path}")
}

pub fn badssl(host: &str) -> String {
    format!("https://{host}/")
}

pub fn request(method: Method, uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .method(method)
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("live request")
}

pub fn request_with_body(
    method: Method,
    uri: impl AsRef<str>,
    body: impl Into<RequestBody>,
) -> Request<RequestBody> {
    Request::builder()
        .method(method)
        .uri(uri.as_ref())
        .body(body.into())
        .expect("live request")
}

pub async fn response_text(response: Response<ResponseBody>) -> String {
    response
        .into_body()
        .text()
        .await
        .expect("live response body")
}

pub async fn response_json(response: Response<ResponseBody>) -> Value {
    let body = response_text(response).await;
    serde_json::from_str(&body)
        .unwrap_or_else(|error| panic!("invalid live JSON body: {error}; body={body}"))
}

pub fn assert_timeout(error: &WireError) {
    assert_eq!(error.kind(), WireErrorKind::Timeout);
}

pub fn assert_tls(error: &WireError) {
    assert_eq!(error.kind(), WireErrorKind::Tls);
}
