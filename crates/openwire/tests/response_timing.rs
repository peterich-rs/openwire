//! Exchange timing is captured at final headers and belongs to the response's
//! actual network attempt, independently of body consumption and follow-ups.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{Request, Response, StatusCode, Version};
use openwire::{
    BoxFuture, Client, Exchange, Interceptor, Next, RequestBody, ResponseBody, ResponseTiming,
    WireError,
};
use openwire_test::{MockResponse, MockWebServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[cfg(feature = "tls-rustls")]
use openwire::RustlsTlsConnector;
#[cfg(feature = "tls-rustls")]
use openwire_test::{ok_text, spawn_https_http2, StaticDnsResolver};

const WATCHDOG: Duration = Duration::from_secs(10);

fn get(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::absent())
        .expect("request")
}

#[tokio::test]
async fn http1_timing_is_available_before_the_peer_releases_the_body() {
    tokio::time::timeout(WATCHDOG, async {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw peer");
        let addr = listener.local_addr().expect("peer address");
        let (release_body, body_released) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept client");
            let mut head = Vec::new();
            let mut buffer = [0; 1024];
            while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("read request head");
                assert_ne!(read, 0, "client closed before sending its request head");
                head.extend_from_slice(&buffer[..read]);
                assert!(head.len() <= 16 * 1024, "unexpectedly large request head");
            }
            assert!(head.starts_with(b"GET /timing HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response head");
            body_released.await.expect("release body after headers");
            stream.write_all(b"hello").await.expect("write body");
            stream.shutdown().await.expect("close raw peer");
        });

        let client = Client::builder().build().expect("client");
        let before_execute = Instant::now();
        // The peer cannot complete its body until execute has returned and the
        // response extension has been inspected below.
        let response = client
            .execute(get(format!("http://{addr}/timing")))
            .await
            .expect("response headers without waiting for body");
        let after_execute = Instant::now();
        assert_eq!(response.version(), Version::HTTP_11);
        let timing = *response
            .extensions()
            .get::<ResponseTiming>()
            .expect("transport timing at headers");
        let request_sent_at = timing
            .received_at()
            .checked_sub(timing.response_delay())
            .expect("request dispatch precedes response receipt");
        assert!(request_sent_at >= before_execute);
        assert!(timing.received_at() <= after_execute);

        release_body
            .send(())
            .expect("peer still waits for body gate");
        assert_eq!(response.into_body().text().await.expect("body"), "hello");
        peer.await.expect("raw peer completed without panic");
    })
    .await
    .expect("HTTP/1 headers/body timing test must make progress");
}

#[cfg(feature = "tls-rustls")]
#[tokio::test]
async fn negotiated_http2_response_preserves_transport_timing() {
    tokio::time::timeout(WATCHDOG, async {
        let server = spawn_https_http2(|_request| async move { ok_text("h2 timing") }).await;
        let tls = RustlsTlsConnector::builder()
            .add_root_certificates_pem(server.tls_root_pem().expect("local root"))
            .expect("root certificate")
            .build()
            .expect("TLS connector");
        let client = Client::builder()
            .tls_connector(tls)
            .dns_resolver(StaticDnsResolver::new(server.addr()))
            .build()
            .expect("client");
        let before_execute = Instant::now();
        let response = client
            .execute(get(format!(
                "https://localhost:{}/timing",
                server.addr().port()
            )))
            .await
            .expect("HTTP/2 response");
        assert_eq!(response.version(), Version::HTTP_2);
        let timing = response
            .extensions()
            .get::<ResponseTiming>()
            .expect("HTTP/2 transport timing");
        let request_sent_at = timing
            .received_at()
            .checked_sub(timing.response_delay())
            .expect("request dispatch precedes response receipt");
        assert!(request_sent_at >= before_execute);
        assert!(timing.received_at() <= Instant::now());
        assert_eq!(
            response.into_body().text().await.expect("body"),
            "h2 timing"
        );
    })
    .await
    .expect("HTTP/2 timing test must make progress");
}

#[derive(Clone, Default)]
struct HopTimings(Arc<Mutex<Vec<(String, ResponseTiming)>>>);

impl Interceptor for HopTimings {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let path = exchange.request().uri().path().to_owned();
        let timings = self.0.clone();
        Box::pin(async move {
            let response = next.run(exchange).await?;
            let timing = *response
                .extensions()
                .get::<ResponseTiming>()
                .expect("each network attempt carries transport timing");
            timings.lock().expect("timings lock").push((path, timing));
            Ok(response)
        })
    }
}

#[tokio::test]
async fn redirect_response_carries_only_the_final_hops_exchange_timing() {
    tokio::time::timeout(WATCHDOG, async {
        let server = MockWebServer::http1().await;
        server.enqueue(
            MockResponse::new()
                .status(StatusCode::FOUND)
                .header("Location", "/final")
                .body("intermediate"),
        );
        server.enqueue(MockResponse::new().body("arrived"));
        let timings = HopTimings::default();
        let client = Client::builder()
            .network_interceptor(timings.clone())
            .build()
            .expect("client");
        let response = client
            .execute(get(server.url("/start")))
            .await
            .expect("redirected response");
        assert_eq!(response.status(), StatusCode::OK);
        let returned = *response
            .extensions()
            .get::<ResponseTiming>()
            .expect("final response timing");
        let hops = timings.0.lock().expect("timings lock").clone();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].0, "/start");
        assert_eq!(hops[1].0, "/final");
        let first = hops[0].1;
        let last = hops[1].1;
        assert_eq!(returned.received_at(), last.received_at());
        assert_eq!(returned.received_at_wall(), last.received_at_wall());
        assert_eq!(returned.response_delay(), last.response_delay());
        let final_request_sent_at = returned
            .received_at()
            .checked_sub(returned.response_delay())
            .expect("final request dispatch precedes its response");
        assert!(
            final_request_sent_at >= first.received_at(),
            "the final exchange must start after the redirect response; its delay cannot include the previous hop"
        );
        assert_eq!(response.into_body().text().await.expect("body"), "arrived");
        assert_eq!(server.request_count(), 2);
        assert_eq!(server.underflow_count(), 0);
    })
    .await
    .expect("redirect timing test must make progress");
}
