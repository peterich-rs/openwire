//! Example: connect to a WebSocket echo server, send a few messages, print
//! the responses, then close gracefully.
//!
//! Run with: `cargo run --example websocket_echo --features websocket`
//!
//! By default the example connects to a local server at `127.0.0.1:9001`
//! (matching the `echo_server` example shipped with `tokio-tungstenite`).
//! Override with `WS_ECHO_URI=ws://host:port/path`.

#[cfg(not(feature = "websocket"))]
fn main() {
    eprintln!("Run with --features websocket");
}

#[cfg(feature = "websocket")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    use futures_util::StreamExt;
    use http::{Method, Request};
    use openwire::{Client, RequestBody};
    use openwire_core::websocket::Message;

    let uri = std::env::var("WS_ECHO_URI").unwrap_or_else(|_| "ws://127.0.0.1:9001/".to_string());
    let request = Request::builder()
        .method(Method::GET)
        .uri(&uri)
        .body(RequestBody::empty())?;

    let client = Client::builder().build()?;
    let websocket = client
        .new_websocket(request)
        .handshake_timeout(Duration::from_secs(10))
        .execute()
        .await?;

    println!("connected, status = {}", websocket.handshake().status());
    let (sender, mut receiver) = websocket.split();

    sender.send_text("hello").await?;
    sender.send_text("world").await?;

    for _ in 0..2 {
        match receiver.next().await {
            Some(Ok(Message::Text(text))) => println!("server > {text}"),
            Some(Ok(other)) => println!("server > {other:?}"),
            Some(Err(error)) => {
                println!("server error: {error}");
                break;
            }
            None => break,
        }
    }

    sender.close(1000, "client done").await?;
    Ok(())
}
