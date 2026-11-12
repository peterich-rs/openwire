//! HTTP/1 framing cases from RFC 9112. Hyper owns the parser; these tests
//! assert the client-visible outcome, not a reimplementation of h1.

use std::net::SocketAddr;
use std::time::Duration;

use http::{Request, StatusCode};
use openwire::{Client, RequestBody, WireErrorKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct RawServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for RawServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn serve_once(write: impl FnOnce(Vec<u8>) -> Vec<u8> + Send + 'static) -> RawServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw listener");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else { return };
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = write(buf);
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        }
    });
    RawServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

fn get(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

#[tokio::test]
async fn te_overrides_content_length_per_rfc9112() {
    // RFC 9112 §6.3.3: if both Transfer-Encoding and Content-Length are
    // present, Transfer-Encoding overrides Content-Length.
    let server = serve_once(|_| {
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 100\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
            .to_vec()
    })
    .await;
    let client = Client::builder().build().expect("client");
    let response = client
        .execute(get(format!("http://{}/te", server.addr)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "hello");
}

#[tokio::test]
async fn truncated_chunked_body_fails_the_body() {
    let server = serve_once(|_| {
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello"
            .to_vec()
    })
    .await;
    let client = Client::builder().build().expect("client");
    let response = client
        .execute(get(format!("http://{}/trunc", server.addr)))
        .await
        .expect("headers");
    let error = response
        .into_body()
        .text()
        .await
        .expect_err("truncated chunked body must fail");
    assert!(
        matches!(error.kind(), WireErrorKind::Body | WireErrorKind::Protocol),
        "truncated chunked should be body/protocol, got {:?}",
        error.kind()
    );
}

#[tokio::test]
async fn http10_close_delimited_body_is_complete() {
    // RFC 9112 §6.3: HTTP/1.0 without Content-Length ends at connection close.
    let server = serve_once(|_| b"HTTP/1.0 200 OK\r\n\r\nhello-close".to_vec()).await;
    let client = Client::builder().build().expect("client");
    let response = client
        .execute(get(format!("http://{}/http10", server.addr)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().text().await.expect("body");
    assert_eq!(body, "hello-close");
}

#[tokio::test]
async fn headers_then_stall_hits_call_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stall listener");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let client = Client::builder()
        .call_timeout(Duration::from_millis(50))
        .build()
        .expect("client");
    let error = match client.execute(get(format!("http://{addr}/stall"))).await {
        Err(error) => error,
        Ok(response) => response
            .into_body()
            .text()
            .await
            .expect_err("stalled body must time out"),
    };
    assert_eq!(error.kind(), WireErrorKind::Timeout);
}

#[tokio::test]
async fn reason_phrase_may_be_omitted() {
    // RFC 9112 §4: reason-phrase is optional.
    let server = serve_once(|_| {
        b"HTTP/1.1 200\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
    })
    .await;
    let client = Client::builder().build().expect("client");
    let response = client
        .execute(get(format!("http://{}/reason", server.addr)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().text().await.expect("body"), "ok");
}
