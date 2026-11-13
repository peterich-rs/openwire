mod support;

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream;
use http::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, TRANSFER_ENCODING, USER_AGENT,
};
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use openwire::{Client, RequestBody, WireError};
use openwire_test::{collect_request_body, ok_text, spawn_http1, StaticDnsResolver};

use support::*;

#[tokio::test]
async fn bridge_interceptor_omits_content_length_for_absent_requests_before_network_interceptors() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("normalized")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/absent",
            server.addr().port()
        )))
        .await
        .expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "normalized");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: None,
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: String::new(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_sends_synthesized_host_on_raw_http11_wire() {
    let observed_request = Arc::new(Mutex::new(None));
    let server = spawn_raw_http1_host_guard(observed_request.clone()).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/raw-host",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "host ok");

    let request = observed_request
        .lock()
        .expect("observed request")
        .clone()
        .expect("observed request");
    assert!(
        request.starts_with("GET /raw-host HTTP/1.1\r\n"),
        "request should use HTTP/1.1 origin-form after transport binding: {request:?}",
    );
    assert!(
        request.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!("host: openwire.test:{}", server.addr().port()))
        }),
        "request should include the bridge-synthesized Host header: {request:?}",
    );
}

#[tokio::test]
async fn bridge_interceptor_normalizes_empty_requests_before_network_interceptors() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("normalized")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/empty",
            server.addr().port()
        ))
        .body(RequestBody::explicit_empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "normalized");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: Some("0".to_owned()),
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: String::new(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_preserves_user_agent_and_sets_fixed_body_content_length() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("fixed")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/fixed",
            server.addr().port()
        ))
        .header(USER_AGENT, "custom-agent/1.0")
        .header(CONTENT_LENGTH, "999")
        .body(RequestBody::from_static(b"hello"))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "fixed");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some("custom-agent/1.0".to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: Some("5".to_owned()),
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: "hello".to_owned(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_streaming_body_uses_chunked_without_content_length() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });
            ok_text("streaming")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let stream = stream::iter(vec![
        Ok::<Bytes, WireError>(Bytes::from_static(b"hello ")),
        Ok::<Bytes, WireError>(Bytes::from_static(b"stream")),
    ]);
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/stream",
            server.addr().port()
        ))
        .header(CONTENT_LENGTH, "999")
        .header(TRANSFER_ENCODING, "identity")
        .body(RequestBody::from_stream(stream))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "streaming");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: None,
        transfer_encoding: Some("chunked".to_owned()),
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: "hello stream".to_owned(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_injects_accept_encoding_and_decodes_gzip_responses() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("request body should be utf-8"),
            });

            let compressed = gzip_bytes(b"decoded gzip response");
            Response::builder()
                .header(CONTENT_ENCODING, "gzip")
                .header(CONTENT_LENGTH, compressed.len().to_string())
                .body(http_body_util::Full::new(Bytes::from(compressed)))
                .expect("gzip response")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/gzip",
            server.addr().port()
        )))
        .await
        .expect("response");
    assert!(response.headers().get(CONTENT_ENCODING).is_none());
    assert!(response.headers().get(CONTENT_LENGTH).is_none());
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "decoded gzip response");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: None,
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: String::new(),
        }
    );
}

#[tokio::test]
async fn bridge_interceptor_decodes_zstd_responses() {
    let server = spawn_http1(|_request: Request<Incoming>| async move {
        let compressed = zstd::bulk::compress(b"decoded zstd response", 0).expect("zstd payload");
        Response::builder()
            .header(CONTENT_ENCODING, "zstd")
            .header(CONTENT_LENGTH, compressed.len().to_string())
            .body(http_body_util::Full::new(Bytes::from(compressed)))
            .expect("zstd response")
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let response = client
        .execute(empty_request(format!(
            "http://openwire.test:{}/zstd",
            server.addr().port()
        )))
        .await
        .expect("response");

    assert!(response.headers().get(CONTENT_ENCODING).is_none());
    assert!(response.headers().get(CONTENT_LENGTH).is_none());
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "decoded zstd response");
}

#[tokio::test]
async fn bridge_interceptor_preserves_explicit_accept_encoding_and_wire_body() {
    let compressed = Arc::new(gzip_bytes(b"caller manages gzip"));
    let expected_wire_body = compressed.clone();
    let server = spawn_http1(move |_request: Request<Incoming>| {
        let compressed = compressed.clone();
        async move {
            Response::builder()
                .header(CONTENT_ENCODING, "gzip")
                .header(CONTENT_LENGTH, compressed.len().to_string())
                .body(http_body_util::Full::new(Bytes::from(
                    compressed.as_ref().clone(),
                )))
                .expect("gzip response")
        }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let request = Request::builder()
        .uri(format!(
            "http://openwire.test:{}/explicit-encoding",
            server.addr().port()
        ))
        .header(ACCEPT_ENCODING, "gzip")
        .body(RequestBody::empty())
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(
        response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok()),
        Some(expected_wire_body.len())
    );
    let body = response.into_body().bytes().await.expect("body");
    assert_eq!(body.as_ref(), expected_wire_body.as_slice());
}

#[tokio::test]
async fn known_length_one_shot_stream_uses_content_length() {
    let observed_server_request = Arc::new(Mutex::new(None));
    let observed_server_request_clone = observed_server_request.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed_server_request = observed_server_request_clone.clone();
        async move {
            let headers = ObservedHeaders::capture(request.headers());
            let body = collect_request_body(request).await;
            *observed_server_request
                .lock()
                .expect("observed server request") = Some(ObservedServerRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("utf-8"),
            });
            ok_text("one-shot")
        }
    })
    .await;

    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/one-shot-length",
            server.addr().port()
        ))
        .header(CONTENT_LENGTH, "999")
        .header(TRANSFER_ENCODING, "identity")
        .body(RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"abc"))]),
            3,
        ))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "one-shot");

    let expected = ObservedHeaders {
        host: Some(format!("openwire.test:{}", server.addr().port())),
        user_agent: Some(default_user_agent().to_owned()),
        accept_encoding: Some(default_accept_encoding().to_owned()),
        content_length: Some("3".to_owned()),
        transfer_encoding: None,
    };
    assert_eq!(interceptor.take_single(), expected);
    assert_eq!(
        take_observed_server_request(&observed_server_request),
        ObservedServerRequest {
            headers: expected,
            body: "abc".to_owned(),
        }
    );
}

#[tokio::test]
async fn from_body_exact_size_hint_keeps_chunked_http11_and_trailers() {
    use http::HeaderMap;
    use http_body::{Body, Frame, SizeHint};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct DataThenTrailers {
        data: Option<Bytes>,
        trailers: Option<HeaderMap>,
    }

    impl Body for DataThenTrailers {
        type Data = Bytes;
        type Error = WireError;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            if let Some(data) = this.data.take() {
                if !data.is_empty() {
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
            }
            if let Some(trailers) = this.trailers.take() {
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
            Poll::Ready(None)
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(
                self.data
                    .as_ref()
                    .map(|data| data.len() as u64)
                    .unwrap_or(0),
            )
        }
    }

    let observed = Arc::new(Mutex::new(None));
    let server = spawn_raw_http1_capture_request(observed.clone()).await;
    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let mut trailer_map = HeaderMap::new();
    trailer_map.insert("x-test-trailer", "1".parse().expect("trailer"));
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/trailers",
            server.addr().port()
        ))
        .header(http::header::TRAILER, "x-test-trailer")
        .body(RequestBody::from_body(DataThenTrailers {
            data: Some(Bytes::from_static(b"xyz")),
            trailers: Some(trailer_map),
        }))
        .expect("request");
    assert_eq!(request.body().size_hint().exact(), None);
    assert_eq!(request.body().content_length(), None);

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    let captured = interceptor.take_single();
    assert!(captured.content_length.is_none());
    assert_eq!(captured.transfer_encoding.as_deref(), Some("chunked"));

    let wire = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured request");
    let text = String::from_utf8_lossy(&wire);
    assert!(
        text.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("content-length:"),
        "{text}"
    );
    assert!(text.contains("3\r\nxyz\r\n"), "{text:?}");
    assert!(
        text.to_ascii_lowercase().contains("x-test-trailer: 1"),
        "{text}"
    );
}

#[tokio::test]
async fn from_body_zero_data_with_trailers_stays_chunked() {
    use http::HeaderMap;
    use http_body::{Body, Frame, SizeHint};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct TrailersOnly(HeaderMap);

    impl Body for TrailersOnly {
        type Data = Bytes;
        type Error = WireError;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            if this.0.is_empty() {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(Ok(Frame::trailers(std::mem::take(&mut this.0)))))
            }
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(0)
        }
    }

    let observed = Arc::new(Mutex::new(None));
    let server = spawn_raw_http1_capture_request(observed.clone()).await;
    let interceptor = HeaderCaptureInterceptor::default();
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");

    let mut trailer_map = HeaderMap::new();
    trailer_map.insert("x-empty-trailer", "yes".parse().expect("trailer"));
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/zero-trailers",
            server.addr().port()
        ))
        .header(http::header::TRAILER, "x-empty-trailer")
        .body(RequestBody::from_body(TrailersOnly(trailer_map)))
        .expect("request");

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    let captured = interceptor.take_single();
    assert!(captured.content_length.is_none());
    assert_eq!(captured.transfer_encoding.as_deref(), Some("chunked"));
    let wire = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured request");
    let text = String::from_utf8_lossy(&wire);
    assert!(
        text.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("content-length:"),
        "{text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("x-empty-trailer: yes"),
        "{text}"
    );
}
