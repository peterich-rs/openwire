mod support;

use http::header::{AUTHORIZATION, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION};
use http::{Method, StatusCode};
use openwire::EstablishmentStage;
use support::{
    assert_timeout, assert_tls, badssl, cookie_client, httpbingo, jsonplaceholder,
    no_redirect_client, postman_echo, request, request_with_body, response_json, response_text,
    short_timeout_client, standard_client,
};

const BASIC_AUTH_OPENWIRE: &str = "Basic b3BlbndpcmU6c3dvcmRmaXNo";

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_get_echo_smoke() {
    let client = standard_client();
    let response = client
        .execute(request(
            Method::GET,
            httpbingo("/get?hello=openwire&phase=200"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["method"].as_str(), Some("GET"));
    assert_eq!(body["args"]["hello"][0].as_str(), Some("openwire"));
    assert_eq!(body["args"]["phase"][0].as_str(), Some("200"));
    assert_eq!(body["headers"]["Host"][0].as_str(), Some("httpbingo.org"));
    assert!(body["headers"]["User-Agent"][0]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_post_body_and_header_smoke() {
    let client = standard_client();
    let mut request =
        request_with_body(Method::POST, httpbingo("/post"), r#"{"hello":"openwire"}"#);
    request.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("content-type"),
    );
    request
        .headers_mut()
        .insert("x-openwire", "phase-201".parse().expect("x-openwire"));

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response_json(response).await;
    assert_eq!(body["method"].as_str(), Some("POST"));
    assert_eq!(body["json"]["hello"].as_str(), Some("openwire"));
    assert!(body["data"]
        .as_str()
        .is_some_and(|value| value.contains("\"hello\":\"openwire\"")));
    assert_eq!(body["headers"]["X-Openwire"][0].as_str(), Some("phase-201"));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_redirect_following_smoke() {
    let client = standard_client();
    let response = client
        .execute(request(
            Method::GET,
            httpbingo("/redirect-to?url=%2Fget%3Ffrom%3Dredirect&status_code=302"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["args"]["from"][0].as_str(), Some("redirect"));
    assert!(body["url"]
        .as_str()
        .is_some_and(|value| value.ends_with("/get?from=redirect")));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_no_redirect_client_smoke() {
    let client = no_redirect_client();
    let response = client
        .execute(request(
            Method::GET,
            httpbingo("/redirect-to?url=%2Fget%3Ffrom%3Dno-redirect&status_code=302"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/get?from=no-redirect")
    );
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_cookie_jar_roundtrip_smoke() {
    let client = cookie_client();
    let set_response = client
        .execute(request(
            Method::GET,
            httpbingo("/cookies/set?session=openwire-live&phase=204"),
        ))
        .await
        .expect("set-cookie response");
    assert!(set_response.status().is_success());
    let _ = response_text(set_response).await;

    let response = client
        .execute(request(Method::GET, httpbingo("/cookies")))
        .await
        .expect("cookies response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response_json(response).await;
    assert_eq!(body["cookies"]["session"].as_str(), Some("openwire-live"));
    assert_eq!(body["cookies"]["phase"].as_str(), Some("204"));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_delay_timeout_smoke() {
    let client = short_timeout_client();
    let error = client
        .execute(request(Method::GET, httpbingo("/delay/2")))
        .await
        .expect_err("delay should time out");

    assert_timeout(&error);
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_stream_smoke() {
    let client = standard_client();
    let response = client
        .execute(request(Method::GET, httpbingo("/stream/3")))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert!(body.contains("\"id\":0"));
    assert!(body.contains("\"id\":1"));
    assert!(body.contains("\"id\":2"));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_etag_smoke() {
    let client = standard_client();
    let first = client
        .execute(request(Method::GET, httpbingo("/etag/openwire-phase-207")))
        .await
        .expect("etag response");
    assert_eq!(first.status(), StatusCode::OK);

    let etag = first
        .headers()
        .get(ETAG)
        .cloned()
        .expect("etag header should be present");
    let _ = response_text(first).await;

    let mut conditional = request(Method::GET, httpbingo("/etag/openwire-phase-207"));
    conditional
        .headers_mut()
        .insert(IF_NONE_MATCH, etag.clone());

    let second = client
        .execute(conditional)
        .await
        .expect("conditional response");
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(second.headers().get(ETAG), Some(&etag));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn httpbingo_basic_auth_smoke() {
    let client = standard_client();
    let mut request = request(Method::GET, httpbingo("/basic-auth/openwire/swordfish"));
    request.headers_mut().insert(
        AUTHORIZATION,
        BASIC_AUTH_OPENWIRE.parse().expect("authorization header"),
    );

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response_json(response).await;
    assert_eq!(body["authenticated"].as_bool(), Some(true));
    assert_eq!(body["authorized"].as_bool(), Some(true));
    assert_eq!(body["user"].as_str(), Some("openwire"));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn postman_echo_get_smoke() {
    let client = standard_client();
    let response = client
        .execute(request(
            Method::GET,
            postman_echo("/get?hello=openwire&phase=400"),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["args"]["hello"].as_str(), Some("openwire"));
    assert_eq!(body["args"]["phase"].as_str(), Some("400"));
    assert_eq!(body["headers"]["host"].as_str(), Some("postman-echo.com"));
    assert!(body["headers"]["user-agent"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn postman_echo_post_smoke() {
    let client = standard_client();
    let mut request = request_with_body(
        Method::POST,
        postman_echo("/post"),
        r#"{"hello":"openwire"}"#,
    );
    request.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("content-type"),
    );
    request
        .headers_mut()
        .insert("x-openwire", "phase-401".parse().expect("x-openwire"));

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response_json(response).await;
    assert_eq!(body["json"]["hello"].as_str(), Some("openwire"));
    assert_eq!(body["data"]["hello"].as_str(), Some("openwire"));
    assert_eq!(body["headers"]["x-openwire"].as_str(), Some("phase-401"));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn jsonplaceholder_get_smoke() {
    let client = standard_client();
    let response = client
        .execute(request(Method::GET, jsonplaceholder("/posts/1")))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["id"].as_i64(), Some(1));
    assert_eq!(body["userId"].as_i64(), Some(1));
    assert!(body["title"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert!(body["body"].as_str().is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn jsonplaceholder_post_smoke() {
    let client = standard_client();
    let mut request = request_with_body(
        Method::POST,
        jsonplaceholder("/posts"),
        r#"{"title":"openwire","body":"phase-403","userId":7}"#,
    );
    request.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("content-type"),
    );

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = response_json(response).await;
    assert_eq!(body["title"].as_str(), Some("openwire"));
    assert_eq!(body["body"].as_str(), Some("phase-403"));
    assert_eq!(body["userId"].as_i64(), Some(7));
    assert!(body["id"].as_i64().is_some());
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn badssl_expired_certificate_smoke() {
    assert_badssl_tls_failure("expired.badssl.com").await;
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn badssl_wrong_hostname_smoke() {
    assert_badssl_tls_failure("wrong.host.badssl.com").await;
}

#[tokio::test]
#[ignore = "live network suite is opt-in; run with --ignored --test-threads=1"]
async fn badssl_self_signed_certificate_smoke() {
    assert_badssl_tls_failure("self-signed.badssl.com").await;
}

async fn assert_badssl_tls_failure(host: &str) {
    let client = standard_client();
    let error = client
        .execute(request(Method::GET, badssl(host)))
        .await
        .unwrap_err();

    assert_tls(&error);
    assert_eq!(error.establishment_stage(), Some(EstablishmentStage::Tls));
}
