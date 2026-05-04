use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::Method;
use openwire::{Client, RequestBody};
use openwire_core::websocket::{Message, WebSocketError};
use openwire_fastwebsockets::FastWebSocketsEngine;
use openwire_test::{spawn_websocket_echo, spawn_websocket_handler};

fn ws_request(uri: &str) -> http::Request<RequestBody> {
    http::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(RequestBody::empty())
        .expect("request build")
}

#[tokio::test]
async fn text_round_trip_with_fastwebsockets_engine() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let websocket = client
        .new_websocket(request)
        .engine(FastWebSocketsEngine::shared())
        .execute()
        .await
        .expect("ws established");
    let (sender, mut receiver) = websocket.split();

    sender.send_text("hello").await.expect("send");
    match receiver.next().await.expect("frame").expect("ok") {
        Message::Text(text) => assert_eq!(text, "hello"),
        other => panic!("unexpected: {other:?}"),
    }
    sender.close(1000, "bye").await.expect("close");
}

#[tokio::test]
async fn binary_round_trip_with_fastwebsockets_engine() {
    let server = spawn_websocket_echo().await;
    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let websocket = client
        .new_websocket(request)
        .engine(FastWebSocketsEngine::shared())
        .close_timeout(Duration::from_secs(2))
        .execute()
        .await
        .expect("ws established");
    let (sender, mut receiver) = websocket.split();

    sender
        .send_binary(Bytes::from_static(&[9, 8, 7]))
        .await
        .expect("send");
    match receiver.next().await.expect("frame").expect("ok") {
        Message::Binary(bytes) => assert_eq!(bytes.as_ref(), &[9, 8, 7]),
        other => panic!("unexpected: {other:?}"),
    }
    sender.close(1000, "bye").await.expect("close");
}

#[tokio::test]
async fn server_initiated_close_with_fastwebsockets_engine() {
    let server = spawn_websocket_handler(|mut websocket| async move {
        use futures_util::SinkExt;
        let _ = websocket
            .send(tokio_tungstenite::tungstenite::Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code:
                        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                    reason: "server bye".into(),
                },
            )))
            .await;
    })
    .await;

    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));
    let websocket = client
        .new_websocket(request)
        .engine(FastWebSocketsEngine::shared())
        .execute()
        .await
        .expect("ws established");
    let (_sender, mut receiver) = websocket.split();
    match receiver.next().await {
        Some(Err(WebSocketError::ClosedByPeer { code, reason })) => {
            assert_eq!(code, 1000);
            assert_eq!(reason, "server bye");
        }
        other => panic!("expected ClosedByPeer, got {other:?}"),
    }
}
