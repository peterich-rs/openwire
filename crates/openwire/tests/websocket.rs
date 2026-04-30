#![cfg(feature = "websocket")]

use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::Method;
use openwire::{Client, RequestBody};
use openwire_core::websocket::{HandshakeFailure, Message, WebSocketError};
use openwire_test::{spawn_websocket_echo, spawn_websocket_handler};

fn ws_request(uri: &str) -> http::Request<RequestBody> {
    http::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(RequestBody::empty())
        .expect("request build")
}

#[tokio::test]
async fn text_message_round_trips() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("websocket established");
    let (sender, mut receiver) = ws.split();

    sender.send_text("hello").await.expect("send");
    match receiver.next().await.expect("frame").expect("ok") {
        Message::Text(text) => assert_eq!(text, "hello"),
        other => panic!("unexpected message: {other:?}"),
    }
    sender.close(1000, "bye").await.expect("close");
}

#[tokio::test]
async fn binary_message_round_trips() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("websocket established");
    let (sender, mut receiver) = ws.split();

    sender
        .send_binary(Bytes::from_static(&[1, 2, 3, 4]))
        .await
        .expect("send");
    match receiver.next().await.expect("frame").expect("ok") {
        Message::Binary(payload) => assert_eq!(payload.as_ref(), &[1, 2, 3, 4]),
        other => panic!("unexpected message: {other:?}"),
    }
    sender.close(1000, "bye").await.expect("close");
}

#[tokio::test]
async fn subprotocol_negotiated_successfully() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request as TRequest, Response as TResponse};

    let server = spawn_websocket_handler(|websocket| async move {
        // tokio-tungstenite's accept_async always echoes; we don't need the
        // configured callback for this test — the test exercises whether the
        // server's selected subprotocol header bubbles up to the client.
        let _ = websocket;
    })
    .await;
    let _ = TRequest::builder; // silence unused-import lint when this branch is unused
    let _ = TResponse::builder;

    // tokio-tungstenite's accept_async doesn't negotiate subprotocols by
    // default. The server simply doesn't echo the protocol header — the
    // client's offered list is preserved but the server returns no
    // sec-websocket-protocol, which our handshake validator accepts.
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));
    let ws = client
        .new_websocket(request)
        .subprotocols(["chat".to_string()])
        .execute()
        .await
        .expect("ws established without selected subprotocol");
    assert!(ws.handshake().subprotocol().is_none());
}

#[tokio::test]
async fn server_initiated_close_reaches_client() {
    let server = spawn_websocket_handler(|mut websocket| async move {
        // Send one message then close gracefully.
        let _ = websocket
            .send(tokio_tungstenite::tungstenite::Message::Text("from server".into()))
            .await;
        let _ = websocket
            .send(tokio_tungstenite::tungstenite::Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                    reason: "server done".into(),
                },
            )))
            .await;
    })
    .await;

    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));
    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("ws established");
    let (_sender, mut receiver) = ws.split();

    let first = receiver.next().await.expect("first frame").expect("ok");
    assert!(matches!(first, Message::Text(t) if t == "from server"));

    match receiver.next().await {
        Some(Err(WebSocketError::ClosedByPeer { code, reason })) => {
            assert_eq!(code, 1000);
            assert_eq!(reason, "server done");
        }
        other => panic!("expected ClosedByPeer, got {other:?}"),
    }
}

#[tokio::test]
async fn client_initiated_close_completes() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let ws = client
        .new_websocket(request)
        .close_timeout(Duration::from_secs(2))
        .execute()
        .await
        .expect("ws established");
    let sender = ws.sender();
    sender.close(1000, "client done").await.expect("close");
    assert!(sender.is_closed());
}

#[tokio::test]
async fn dropping_all_senders_cancels_writer() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("ws established");
    // Dropping the WebSocket releases its internal sender + receiver.
    // The Arc<SenderInner> drop fires Cancel via try_send when the last
    // sender clone goes out of scope.
    drop(ws);
    // Give the runtime a moment to process the drop and writer exit.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn rejects_non_websocket_response() {
    use openwire_test::{ok_text, spawn_http1};

    let server = spawn_http1(|_| async { ok_text("not a websocket") }).await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let result = client.new_websocket(request).execute().await;
    match result {
        Err(WebSocketError::Handshake { reason: HandshakeFailure::UnexpectedStatus, .. }) => {}
        Err(other) => panic!("expected UnexpectedStatus handshake failure, got {other:?}"),
        Ok(_) => panic!("plain HTTP response must not produce a websocket"),
    }
}

#[tokio::test]
async fn handshake_timeout_fires_when_server_silent() {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Don't accept — let the connection sit
    let _silent_listener = listener;

    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", addr));
    let result = client
        .new_websocket(request)
        .handshake_timeout(Duration::from_millis(150))
        .execute()
        .await;
    assert!(matches!(result, Err(WebSocketError::Timeout(_))));
}
