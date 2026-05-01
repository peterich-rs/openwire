//! Example: connect to a WebSocket server while offering one or more
//! subprotocols and print whichever the server selected.
//!
//! Run with: `cargo run --example websocket_subprotocol --features websocket`
//!
//! Set `WS_URI` (default `ws://127.0.0.1:9001/`) and `WS_SUBPROTOCOLS`
//! (comma-separated list, default `chat,v1.json`).

#[cfg(not(feature = "websocket"))]
fn main() {
    eprintln!("Run with --features websocket");
}

#[cfg(feature = "websocket")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use http::{Method, Request};
    use openwire::{Client, RequestBody};

    let uri = std::env::var("WS_URI").unwrap_or_else(|_| "ws://127.0.0.1:9001/".to_string());
    let subprotocols: Vec<String> = std::env::var("WS_SUBPROTOCOLS")
        .unwrap_or_else(|_| "chat,v1.json".to_string())
        .split(',')
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .collect();

    let request = Request::builder()
        .method(Method::GET)
        .uri(&uri)
        .body(RequestBody::empty())?;

    let client = Client::builder().build()?;
    let websocket = client
        .new_websocket(request)
        .subprotocols(subprotocols.clone())
        .execute()
        .await?;

    println!("offered subprotocols: {subprotocols:?}");
    match websocket.handshake().subprotocol() {
        Some(selected) => println!("server selected: {selected}"),
        None => println!("server selected: <none>"),
    }
    println!(
        "server extensions: {:?}",
        websocket.handshake().extensions()
    );

    websocket.sender().close(1000, "demo done").await?;
    Ok(())
}
