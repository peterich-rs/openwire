#![cfg(feature = "multipart")]

mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::stream::{self, Stream};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, TE, TRANSFER_ENCODING};
use http::{Request, Version};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use openwire::multipart::{Form, Part};
use openwire::{Client, RustlsTlsConnector, WireError, WireErrorKind};
use openwire_test::{
    collect_request_body, ok_text, spawn_http1, spawn_https_http2_with_hosts,
    RecordingEventListenerFactory, StaticDnsResolver,
};

use support::*;

fn sample_form() -> Form {
    Form::new()
        .text("description", "upload from OpenWire")
        .part(
            "file",
            Part::bytes(Bytes::from_static(b"hello\n"))
                .file_name("hello.txt")
                .mime_str("text/plain; charset=utf-8")
                .expect("mime"),
        )
}

fn content_type_boundary(value: &http::HeaderValue) -> String {
    let value = value.to_str().expect("ascii");
    let prefix = "multipart/form-data; boundary=";
    assert!(value.starts_with(prefix), "content-type = {value}");
    value[prefix.len()..].to_owned()
}

#[tokio::test]
async fn http11_upload_matches_encoded_body_and_content_type() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let interceptor = HeaderCaptureInterceptor::default();
    let events = RecordingEventListenerFactory::default();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let content_type = request
                .headers()
                .get(CONTENT_TYPE)
                .cloned()
                .expect("content-type");
            let content_length = request
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let transfer_encoding = request.headers().get(TRANSFER_ENCODING).cloned();
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") =
                Some((content_type, content_length, transfer_encoding, body));
            ok_text("ok")
        }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .event_listener_factory(events.clone())
        .build()
        .expect("client");

    let request = sample_form()
        .into_request(Request::builder().method("POST").uri(format!(
            "http://openwire.test:{}/upload",
            server.addr().port()
        )))
        .expect("request");
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .expect("content-type");
    let expected_len = request.body().content_length().expect("known length");
    let expected_bytes = request
        .body()
        .try_clone()
        .expect("replayable")
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");

    let captured = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured");
    assert_eq!(captured.0, content_type);
    assert_eq!(
        captured.1.as_deref(),
        Some(expected_len.to_string().as_str())
    );
    assert!(captured.2.is_none());
    assert_eq!(captured.3, expected_bytes);
    assert!(interceptor.take_single().transfer_encoding.is_none());
    events.assert_contains_in_order(&[
        "request_body_start",
        &format!("request_body_end {expected_len}"),
    ]);
    let body_end_count = events
        .events()
        .iter()
        .filter(|event| event.starts_with("request_body_end "))
        .count();
    assert_eq!(body_end_count, 1);
}

#[tokio::test]
async fn http2_upload_omits_transfer_encoding_and_matches_encoded_bytes() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server =
        spawn_https_http2_with_hosts(&["openwire.test"], move |request: Request<Incoming>| {
            let observed = observed_clone.clone();
            async move {
                let version = request.version();
                let content_type = request
                    .headers()
                    .get(CONTENT_TYPE)
                    .cloned()
                    .expect("content-type");
                let content_length = request.headers().get(CONTENT_LENGTH).cloned();
                let transfer_encoding = request.headers().get(TRANSFER_ENCODING).cloned();
                let te = request.headers().get(TE).cloned();
                let connection = request.headers().get(http::header::CONNECTION).cloned();
                let body = collect_request_body(request).await;
                *observed.lock().expect("observed") = Some((
                    version,
                    content_type,
                    content_length,
                    transfer_encoding,
                    te,
                    connection,
                    body,
                ));
                ok_text("h2")
            }
        })
        .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .tls_connector(
            RustlsTlsConnector::builder()
                .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
                .expect("root cert")
                .build()
                .expect("tls"),
        )
        .build()
        .expect("client");

    let request = sample_form()
        .into_request(Request::builder().method("POST").uri(format!(
            "https://openwire.test:{}/upload",
            server.addr().port()
        )))
        .expect("request");
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .expect("content-type");
    let expected_bytes = request
        .body()
        .try_clone()
        .expect("replayable")
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.into_body().text().await.expect("body"), "h2");

    let captured = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured");
    assert_eq!(captured.0, Version::HTTP_2);
    assert_eq!(captured.1, content_type);
    assert!(captured.2.is_some());
    assert!(captured.3.is_none());
    assert!(captured.4.is_none());
    assert!(captured.5.is_none());
    assert_eq!(captured.6, expected_bytes);
}

#[tokio::test]
async fn known_length_one_shot_form_uses_content_length() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let content_length = request
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let transfer_encoding = request.headers().get(TRANSFER_ENCODING).cloned();
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") = Some((content_length, transfer_encoding, body));
            ok_text("ok")
        }
    })
    .await;

    let payload = Bytes::from_static(b"xyz");
    let form = Form::new().part(
        "file",
        Part::stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(payload.clone())]),
            payload.len() as u64,
        ),
    );
    let (_, encoded) = Form::new()
        .part("file", Part::bytes(payload.clone()))
        .into_parts()
        .expect("expected");
    let expected_len = encoded.content_length().expect("length");

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");
    let request = form
        .into_request(Request::builder().method("POST").uri(format!(
            "http://openwire.test:{}/one-shot",
            server.addr().port()
        )))
        .expect("request");
    assert!(request.body().try_clone().is_none());
    assert_eq!(request.body().content_length(), Some(expected_len));

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    let captured = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured");
    assert_eq!(
        captured.0.as_deref(),
        Some(expected_len.to_string().as_str())
    );
    assert!(captured.1.is_none());
}

#[tokio::test]
async fn unknown_length_stream_form_uses_http11_chunked() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let content_length = request.headers().get(CONTENT_LENGTH).cloned();
            let transfer_encoding = request
                .headers()
                .get(TRANSFER_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") = Some((content_length, transfer_encoding, body));
            ok_text("ok")
        }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");
    let request = Form::new()
        .part(
            "file",
            Part::stream(stream::iter(vec![Ok::<Bytes, WireError>(
                Bytes::from_static(b"chunked-payload"),
            )])),
        )
        .into_request(Request::builder().method("POST").uri(format!(
            "http://openwire.test:{}/chunked",
            server.addr().port()
        )))
        .expect("request");
    assert!(request.body().content_length().is_none());

    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    let captured = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("captured");
    assert!(captured.0.is_none());
    assert_eq!(captured.1.as_deref(), Some("chunked"));
    assert!(captured
        .2
        .windows(15)
        .any(|window| window == b"chunked-payload"));
}

#[tokio::test]
async fn http10_unknown_length_form_is_rejected_before_io() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler_attempts = attempts.clone();
    let interceptor = HeaderCaptureInterceptor::default();
    let server = spawn_http1(move |_request: Request<Incoming>| {
        handler_attempts.fetch_add(1, Ordering::SeqCst);
        async move { ok_text("should not run") }
    })
    .await;

    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .network_interceptor(interceptor.clone())
        .build()
        .expect("client");
    let request = Form::new()
        .part(
            "file",
            Part::stream(stream::iter(vec![Ok::<Bytes, WireError>(
                Bytes::from_static(b"nope"),
            )])),
        )
        .into_request(
            Request::builder()
                .method("POST")
                .version(Version::HTTP_10)
                .uri(format!(
                    "http://openwire.test:{}/http10",
                    server.addr().port()
                )),
        )
        .expect("request");

    let error = client.execute(request).await.expect_err("http/1.0");
    assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert!(interceptor.seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn source_read_error_does_not_read_later_parts() {
    let later_reads = Arc::new(AtomicUsize::new(0));
    let later = later_reads.clone();
    let failing = stream::iter(vec![Err(WireError::internal(
        "source read failed",
        std::io::Error::other("read"),
    ))]);
    let later_stream = stream::unfold((), move |_| {
        let later = later.clone();
        async move {
            later.fetch_add(1, Ordering::SeqCst);
            Some((Ok::<Bytes, WireError>(Bytes::from_static(b"later")), ()))
        }
    });

    let server = spawn_http1(|request: Request<Incoming>| async move {
        let _ = collect_request_body(request).await;
        ok_text("unused")
    })
    .await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");
    let request = Form::new()
        .part("first", Part::stream(failing))
        .part("second", Part::stream(later_stream))
        .into_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://openwire.test:{}/err", server.addr().port())),
        )
        .expect("request");
    let error = client.execute(request).await.expect_err("body error");
    assert_eq!(error.kind(), WireErrorKind::Body);
    assert_eq!(later_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn declared_length_mismatch_does_not_emit_suffix() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") = Some(body);
            ok_text("unused")
        }
    })
    .await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");
    let request = Form::new()
        .part(
            "file",
            Part::stream_with_length(
                stream::iter(vec![
                    Ok::<Bytes, WireError>(Bytes::from_static(b"ab")),
                    Ok(Bytes::from_static(b"c")),
                ]),
                2,
            ),
        )
        .into_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://openwire.test:{}/len", server.addr().port())),
        )
        .expect("request");
    let error = client.execute(request).await.expect_err("length");
    assert_eq!(error.kind(), WireErrorKind::Body);
    let maybe_body = observed.lock().expect("observed").clone();
    if let Some(body) = maybe_body {
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.matches("--").count() <= 1,
            "closing delimiter must not hide a length error: {text:?}"
        );
    }
}

#[tokio::test]
async fn stream_collision_never_emits_forbidden_sequence() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") = Some(body);
            ok_text("unused")
        }
    })
    .await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");

    let shared_boundary = Arc::new(Mutex::new(None::<String>));
    let stream_boundary = shared_boundary.clone();
    let colliding = stream::unfold(0u8, move |step| {
        let stream_boundary = stream_boundary.clone();
        async move {
            match step {
                0 => Some((Ok::<Bytes, WireError>(Bytes::from_static(b"ok\r\n")), 1)),
                1 => {
                    let boundary = loop {
                        if let Some(boundary) = stream_boundary.lock().expect("boundary").clone() {
                            break boundary;
                        }
                        tokio::task::yield_now().await;
                    };
                    Some((Ok(Bytes::from(format!("--{boundary}"))), 2))
                }
                _ => None,
            }
        }
    });
    let request = Form::new()
        .part("file", Part::stream(colliding))
        .into_request(Request::builder().method("POST").uri(format!(
            "http://openwire.test:{}/collision",
            server.addr().port()
        )))
        .expect("request");
    let boundary = content_type_boundary(request.headers().get(CONTENT_TYPE).expect("ct"));
    *shared_boundary.lock().expect("boundary") = Some(boundary.clone());
    let error = client.execute(request).await.expect_err("collision");
    assert_eq!(error.kind(), WireErrorKind::Body);
    let maybe_body = observed.lock().expect("observed").clone();
    if let Some(body) = maybe_body {
        let needle = format!("--{boundary}").into_bytes();
        let closer = format!("--{boundary}--").into_bytes();
        let dash_boundary_hits = body
            .windows(needle.len())
            .filter(|window| *window == needle.as_slice())
            .count();
        assert!(
            dash_boundary_hits <= 1,
            "payload must not emit a second dash-boundary: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            !body.windows(closer.len()).any(|window| window == closer),
            "closing delimiter must not be emitted after collision"
        );
    }
}

struct PendingOnce<S> {
    pending: bool,
    inner: S,
}

impl<S: Stream<Item = Result<Bytes, WireError>> + Unpin> Stream for PendingOnce<S> {
    type Item = Result<Bytes, WireError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pending {
            this.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

#[tokio::test]
async fn pending_producer_still_uploads_complete_form() {
    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let server = spawn_http1(move |request: Request<Incoming>| {
        let observed = observed_clone.clone();
        async move {
            let body = collect_request_body(request).await;
            *observed.lock().expect("observed") = Some(body);
            ok_text("ok")
        }
    })
    .await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .build()
        .expect("client");
    let request = Form::new()
        .part(
            "file",
            Part::stream(PendingOnce {
                pending: true,
                inner: stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"xyz"))]),
            }),
        )
        .into_request(Request::builder().method("POST").uri(format!(
            "http://openwire.test:{}/pending",
            server.addr().port()
        )))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    let body = observed.lock().expect("observed").clone().expect("body");
    assert!(body.windows(3).any(|window| window == b"xyz"));
}

#[tokio::test]
async fn top_level_known_length_stream_emits_body_end() {
    let events = RecordingEventListenerFactory::default();
    let server = spawn_http1(|_request: Request<Incoming>| async move { ok_text("ok") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/len-events",
            server.addr().port()
        ))
        .body(openwire::RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"abc"))]),
            3,
        ))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    events.assert_contains_in_order(&["request_body_start", "request_body_end 3"]);
}

#[tokio::test]
async fn top_level_zero_length_stream_emits_body_events() {
    let events = RecordingEventListenerFactory::default();
    let server = spawn_http1(|_request: Request<Incoming>| async move { ok_text("ok") }).await;
    let client = Client::builder()
        .dns_resolver(StaticDnsResolver::new(server.addr()))
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://openwire.test:{}/zero-len-events",
            server.addr().port()
        ))
        .body(openwire::RequestBody::from_stream_with_length(
            stream::iter(Vec::<Result<Bytes, WireError>>::new()),
            0,
        ))
        .expect("request");
    let response = client.execute(request).await.expect("response");
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
    events.assert_contains_in_order(&["request_body_start", "request_body_end 0"]);
}
