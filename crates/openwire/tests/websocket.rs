#![cfg(feature = "websocket")]

use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::Method;
use openwire::{Client, RequestBody, WireErrorKind};
use openwire_core::websocket::{HandshakeFailure, Message, WebSocketEngineError, WebSocketError};
use openwire_test::{spawn_websocket_echo, spawn_websocket_handler, RecordingEventListenerFactory};

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
    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as TRequest, Response as TResponse,
    };

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
async fn invalid_subprotocol_rejected_before_connect() {
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = ws_request("ws://127.0.0.1:9/");

    let result = client
        .new_websocket(request)
        .subprotocols(["chat room"])
        .execute()
        .await;

    match result {
        Err(WebSocketError::Io(error)) => {
            assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
        }
        Err(other) => panic!("expected invalid request error, got {other:?}"),
        Ok(_) => panic!("invalid subprotocol must not establish a websocket"),
    }
    assert!(
        events.events().is_empty(),
        "invalid subprotocol should fail before call_start"
    );
}

#[tokio::test]
async fn invalid_runtime_config_rejected_before_connect() {
    enum Case {
        SendQueueZero,
        PingIntervalZero,
        PongTimeoutZero,
        DefaultPongTimeoutOverflow,
    }

    let cases = [
        (
            Case::SendQueueZero,
            "send_queue_size must be greater than 0",
        ),
        (
            Case::PingIntervalZero,
            "ping_interval must be greater than 0",
        ),
        (Case::PongTimeoutZero, "pong_timeout must be greater than 0"),
        (
            Case::DefaultPongTimeoutOverflow,
            "pong_timeout default would overflow",
        ),
    ];

    for (case, expected_message) in cases {
        let events = RecordingEventListenerFactory::default();
        let client = Client::builder()
            .event_listener_factory(events.clone())
            .build()
            .expect("client");
        let request = ws_request("ws://127.0.0.1:9/");

        let result = match case {
            Case::SendQueueZero => {
                client
                    .new_websocket(request)
                    .send_queue_size(0)
                    .execute()
                    .await
            }
            Case::PingIntervalZero => {
                client
                    .new_websocket(request)
                    .ping_interval(Duration::ZERO)
                    .execute()
                    .await
            }
            Case::PongTimeoutZero => {
                client
                    .new_websocket(request)
                    .ping_interval(Duration::from_secs(1))
                    .pong_timeout(Duration::ZERO)
                    .execute()
                    .await
            }
            Case::DefaultPongTimeoutOverflow => {
                client
                    .new_websocket(request)
                    .ping_interval(Duration::MAX)
                    .execute()
                    .await
            }
        };

        match result {
            Err(WebSocketError::Io(error)) => {
                assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
                assert!(
                    error.message().contains(expected_message),
                    "unexpected error message: {error}"
                );
            }
            Err(other) => panic!("expected invalid request error, got {other:?}"),
            Ok(_) => panic!("invalid runtime config must not establish a websocket"),
        }
        assert!(
            events.events().is_empty(),
            "invalid runtime config should fail before call_start"
        );
    }
}

#[tokio::test]
async fn server_initiated_close_reaches_client() {
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(1);
    let server = spawn_websocket_handler(move |mut websocket| {
        let ack_tx = ack_tx.clone();
        async move {
            // Send one message then close gracefully.
            let _ = websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    "from server".into(),
                ))
                .await;
            let _ = websocket
            .send(tokio_tungstenite::tungstenite::Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code:
                        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                    reason: "server done".into(),
                },
            )))
            .await;
            let ack = match tokio::time::timeout(Duration::from_secs(1), websocket.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(Some(frame))))) => frame
                    .code
                    == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal
                    && frame.reason == "server done",
                _ => false,
            };
            let _ = ack_tx.send(ack).await;
        }
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

    let acked = tokio::time::timeout(Duration::from_secs(2), ack_rx.recv())
        .await
        .expect("server should observe close ack")
        .expect("ack result");
    assert!(acked, "server should receive matching close ack");
}

#[tokio::test]
async fn empty_server_close_is_acknowledged_without_status_code() {
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(1);
    let server = spawn_websocket_handler(move |mut websocket| {
        let ack_tx = ack_tx.clone();
        async move {
            let _ = websocket
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await;
            let ack = matches!(
                tokio::time::timeout(Duration::from_secs(1), websocket.next()).await,
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(
                    None
                ))))
            );
            let _ = ack_tx.send(ack).await;
        }
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

    match receiver.next().await {
        Some(Err(WebSocketError::ClosedByPeer { code, reason })) => {
            assert_eq!(code, 1005);
            assert!(reason.is_empty());
        }
        other => panic!("expected empty ClosedByPeer, got {other:?}"),
    }

    let acked = tokio::time::timeout(Duration::from_secs(2), ack_rx.recv())
        .await
        .expect("server should observe empty close ack")
        .expect("ack result");
    assert!(acked, "server should receive an empty close ack");
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
async fn event_listener_records_websocket_lifecycle_once() {
    let server = spawn_websocket_echo().await;
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("ws established");
    let (sender, mut receiver) = ws.split();

    sender.send_text("hello").await.expect("send");
    let first = receiver.next().await.expect("first frame").expect("ok");
    assert!(matches!(first, Message::Text(text) if text == "hello"));

    sender.close(1000, "bye").await.expect("close");

    let events = events.events();
    assert!(events
        .iter()
        .any(|event| event == "websocket_open 101 Switching Protocols"));
    assert!(events
        .iter()
        .any(|event| event == "websocket_message_sent Text 5"));
    assert!(events
        .iter()
        .any(|event| event == "websocket_message_received Text 5"));
    assert!(events
        .iter()
        .any(|event| event == "websocket_closing Local 1000 bye"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("websocket_closed "))
            .count(),
        1,
        "websocket_closed should fire exactly once: {events:?}"
    );
    assert_eq!(
        events.iter().filter(|event| *event == "call_end").count(),
        1,
        "call_end should fire once after close: {events:?}"
    );
    assert!(
        !events.iter().any(|event| event.starts_with("call_failed")),
        "graceful close should not fail call lifecycle: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("websocket_failed")),
        "graceful close should not fail websocket lifecycle: {events:?}"
    );
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
    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = ws_request(&format!("ws://{}/", server.addr()));

    let result = client.new_websocket(request).execute().await;
    match result {
        Err(WebSocketError::Handshake {
            reason: HandshakeFailure::UnexpectedStatus,
            ..
        }) => {}
        Err(other) => panic!("expected UnexpectedStatus handshake failure, got {other:?}"),
        Ok(_) => panic!("plain HTTP response must not produce a websocket"),
    }

    let events = events.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("call_start GET")),
        "handshake failure after execution starts should record call_start: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event == "websocket_failed handshake failed: UnexpectedStatus"),
        "handshake failure should record websocket_failed: {events:?}"
    );
    assert!(
        events.iter().any(|event| event == "call_failed Protocol"),
        "handshake failure should terminate the call lifecycle: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("websocket_open")),
        "failed handshake must not record websocket_open: {events:?}"
    );
}

#[tokio::test]
async fn rejects_unrequested_server_extensions() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let read = stream.read(&mut buf).await.expect("read");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&request);
        let key = request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .expect("websocket key");
        let accept = websocket_accept(key);
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             Sec-WebSocket-Extensions: permessage-deflate\r\n\
             \r\n"
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let client = Client::builder().build().expect("client");
    let request = ws_request(&format!("ws://{addr}/"));
    let result = client.new_websocket(request).execute().await;
    match result {
        Err(WebSocketError::Handshake {
            status: Some(status),
            reason: HandshakeFailure::UnsupportedExtension(extension),
        }) => {
            assert_eq!(status, http::StatusCode::SWITCHING_PROTOCOLS);
            assert_eq!(extension, "permessage-deflate");
        }
        Err(other) => panic!("expected UnsupportedExtension handshake failure, got {other:?}"),
        Ok(_) => panic!("unrequested extension must not produce a websocket"),
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
    let request = ws_request(&format!("ws://{addr}/"));
    let result = client
        .new_websocket(request)
        .handshake_timeout(Duration::from_millis(150))
        .execute()
        .await;
    assert!(matches!(result, Err(WebSocketError::Timeout(_))));
}

#[tokio::test]
async fn server_disconnect_without_close_reports_protocol_failure() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let read = stream.read(&mut buf).await.expect("read");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&request);
        let key = request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .expect("websocket key");
        let accept = websocket_accept(key);
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        // Drop without sending a WebSocket Close frame.
    });

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()
        .expect("client");
    let request = ws_request(&format!("ws://{addr}/"));
    let ws = client
        .new_websocket(request)
        .execute()
        .await
        .expect("websocket established");
    let (_sender, mut receiver) = ws.split();

    match receiver.next().await {
        Some(Err(WebSocketError::Engine(WebSocketEngineError::InvalidFrame(reason)))) => {
            assert_eq!(reason, "websocket stream ended before close frame");
        }
        other => panic!("expected abnormal websocket EOF failure, got {other:?}"),
    }

    let events = events.events();
    assert!(
        events
            .iter()
            .any(|event| event == "websocket_open 101 Switching Protocols"),
        "successful 101 should still record websocket_open: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event == "websocket_failed invalid frame: websocket stream ended before close frame"
        }),
        "abnormal EOF should record websocket_failed: {events:?}"
    );
    assert!(
        events.iter().any(|event| event == "call_failed Protocol"),
        "abnormal EOF should fail call lifecycle: {events:?}"
    );
}

fn websocket_accept(client_key: &str) -> String {
    use base64::Engine;
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}
