use openwire_core::websocket::{
    Role, SharedWebSocketEngine, WebSocketChannel, WebSocketEngineConfig, WebSocketError,
};
use openwire_core::{BoxConnection, RequestBody};

use super::handshake::{validate_handshake_response, ValidatedHandshake};

/// Drive the HTTP/1.1 handshake on `io`, validate the 101 response, and hand
/// the upgraded socket to the engine.
///
/// Returned tuple: the validated response (status/headers without body), the
/// duplex `WebSocketChannel` from the engine, and the negotiated subprotocol /
/// extensions extracted from the response.
#[allow(dead_code)] // wired in by Task 22/23
pub(crate) async fn execute_websocket_handshake(
    io: BoxConnection,
    request: http::Request<RequestBody>,
    expected_accept: &str,
    offered_subprotocols: &[String],
    engine: SharedWebSocketEngine,
    max_frame_size: usize,
    max_message_size: usize,
) -> Result<(http::Response<()>, WebSocketChannel, ValidatedHandshake), WebSocketError> {
    let (response, upgraded) =
        crate::transport::protocol::bind_websocket_handshake(io, request)
            .await
            .map_err(WebSocketError::Io)?;

    let handshake = validate_handshake_response(&response, expected_accept, offered_subprotocols)
        .map_err(|reason| WebSocketError::handshake(reason, Some(response.status())))?;

    let cfg = WebSocketEngineConfig {
        role: Role::Client,
        subprotocol: handshake.subprotocol.clone(),
        extensions: handshake.extensions.clone(),
        max_frame_size,
        max_message_size,
    };
    let upgraded_io = crate::transport::protocol::upgraded_into_box_connection(upgraded);
    let channel = engine.upgrade(upgraded_io, cfg).await?;
    Ok((response, channel, handshake))
}
