use std::sync::Arc;
use std::time::Duration;

use http::{Request, Uri};

use openwire_core::websocket::{
    Role, SharedWebSocketEngine, WebSocketChannel, WebSocketEngineConfig, WebSocketError,
    WebSocketHandshake,
};
use openwire_core::{BoxConnection, CallContext, RequestBody, WireError};

use crate::client::WebSocketCall;
use crate::connection::Address;
use crate::proxy::{ProxyChoice, ProxySelector, SelectedProxy};
use crate::transport::{connect_route_plan, ProxyConnectDeps};
use crate::websocket::handshake::{
    validate_handshake_response, ValidatedHandshake, WebSocketRequestMarker,
};
use crate::websocket::native::NativeEngine;
use crate::websocket::public::{WebSocket, WebSocketReceiver, WebSocketSender};
use crate::websocket::writer::{spawn_session, HeartbeatConfig, SessionConfig};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_SEND_QUEUE_SIZE: usize = 32;

pub(crate) async fn execute(
    call: WebSocketCall<'_>,
) -> Result<WebSocket, WebSocketError> {
    let WebSocketCall {
        client,
        mut request,
        handshake_timeout,
        close_timeout,
        max_frame_size,
        max_message_size,
        send_queue_size,
        ping_interval,
        pong_timeout,
        subprotocols,
        deliver_control_frames,
        engine,
    } = call;

    let handshake_timeout = handshake_timeout.unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT);
    let close_timeout = close_timeout.unwrap_or(DEFAULT_CLOSE_TIMEOUT);
    let max_frame_size = max_frame_size.unwrap_or(DEFAULT_MAX_FRAME_SIZE);
    let max_message_size = max_message_size.unwrap_or(DEFAULT_MAX_MESSAGE_SIZE);
    let send_queue_size = send_queue_size.unwrap_or(DEFAULT_SEND_QUEUE_SIZE);
    let engine: SharedWebSocketEngine = engine.unwrap_or_else(|| Arc::new(NativeEngine::new()));

    request
        .extensions_mut()
        .insert(WebSocketRequestMarker::new(subprotocols.clone()));
    crate::bridge::normalize_request(&mut request).map_err(WebSocketError::Io)?;

    let expected_accept = request
        .extensions()
        .get::<WebSocketRequestMarker>()
        .map(|marker| marker.expected_accept.clone())
        .ok_or_else(|| {
            WebSocketError::Io(WireError::internal(
                "WebSocketRequestMarker missing after bridge normalization",
                std::io::Error::other("missing marker"),
            ))
        })?;

    let ctx = CallContext::from_factory(
        client.event_listener_factory(),
        &request,
        Some(handshake_timeout),
    );
    ctx.listener().call_start(&ctx, &request);

    let connect = async {
        let address = build_address(client, request.uri())?;
        let route_plan = client
            .ws_connector()
            .route_plan(ctx.clone(), &address)
            .await?;
        let deps: ProxyConnectDeps = client
            .ws_connector()
            .proxy_connect_deps(client.ws_connector().connect_timeout);
        let io = connect_route_plan(ctx.clone(), request.uri().clone(), route_plan, deps).await?;
        Ok::<BoxConnection, WireError>(io)
    };

    let io = match tokio::time::timeout(handshake_timeout, connect).await {
        Ok(Ok(io)) => io,
        Ok(Err(error)) => return Err(WebSocketError::Io(error)),
        Err(_) => {
            return Err(WebSocketError::Timeout(
                openwire_core::websocket::TimeoutKind::Handshake,
            ))
        }
    };

    let handshake_future = run_handshake(
        io,
        request,
        expected_accept,
        subprotocols,
        engine,
        max_frame_size,
        max_message_size,
    );
    let (response, channel, validated) =
        match tokio::time::timeout(handshake_timeout, handshake_future).await {
            Ok(Ok(triple)) => triple,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(WebSocketError::Timeout(
                    openwire_core::websocket::TimeoutKind::Handshake,
                ))
            }
        };

    let handshake = WebSocketHandshake::new(
        response.status(),
        response.headers().clone(),
        validated.subprotocol,
        validated.extensions,
    );

    let heartbeat = ping_interval.map(|interval| HeartbeatConfig {
        interval,
        pong_timeout: pong_timeout.unwrap_or(interval * 2),
    });
    ctx.listener().websocket_open(&ctx, &handshake);
    let session = spawn_session(
        channel,
        SessionConfig {
            queue_size: send_queue_size,
            deliver_control_frames,
            close_timeout,
            heartbeat,
            ctx: Some(ctx.clone()),
            listener: Some(ctx.listener().clone()),
        },
    );
    let sender = WebSocketSender::new(session.sender_tx);
    let receiver = WebSocketReceiver {
        rx: session.receiver_rx,
    };

    Ok(WebSocket {
        sender,
        receiver,
        handshake,
    })
}

async fn run_handshake(
    io: BoxConnection,
    request: Request<RequestBody>,
    expected_accept: String,
    offered_subprotocols: Vec<String>,
    engine: SharedWebSocketEngine,
    max_frame_size: usize,
    max_message_size: usize,
) -> Result<(http::Response<()>, WebSocketChannel, ValidatedHandshake), WebSocketError> {
    let (response, upgraded) =
        crate::transport::protocol::bind_websocket_handshake(io, request)
            .await
            .map_err(WebSocketError::Io)?;

    let validated =
        validate_handshake_response(&response, &expected_accept, &offered_subprotocols)
            .map_err(|reason| WebSocketError::handshake(reason, Some(response.status())))?;
    let upgraded = upgraded.ok_or_else(|| {
        WebSocketError::handshake(
            openwire_core::websocket::HandshakeFailure::Other(
                "server returned 101 without an upgradable connection".into(),
            ),
            Some(response.status()),
        )
    })?;

    let cfg = WebSocketEngineConfig {
        role: Role::Client,
        subprotocol: validated.subprotocol.clone(),
        extensions: validated.extensions.clone(),
        max_frame_size,
        max_message_size,
    };
    let upgraded_io = crate::transport::protocol::upgraded_into_box_connection(upgraded);
    let channel = engine.upgrade(upgraded_io, cfg).await?;
    Ok((response, channel, validated))
}

fn build_address(client: &crate::Client, uri: &Uri) -> Result<Address, WireError> {
    let selection = client.ws_proxy_selector().select(uri)?;
    let selected_proxy = selection.iter().find_map(|choice| match choice {
        ProxyChoice::Direct => None,
        ProxyChoice::Proxy(proxy) => Some(SelectedProxy::from_proxy(proxy)),
    });
    Address::from_uri(uri, selected_proxy.as_ref())
}
