use std::net::SocketAddr;
use std::sync::Arc;

use http::{Request, Response, Uri};

use crate::{CallContext, ConnectionId, RequestBody, ResponseBody, WireError};

pub type SharedEventListener = Arc<dyn EventListener>;
pub type SharedEventListenerFactory = Arc<dyn EventListenerFactory>;

pub trait EventListener: Send + Sync + 'static {
    /// Fired once when a call begins execution.
    ///
    /// Retries, redirects, and authentication follow-ups remain inside the same
    /// `call_start` to `call_end` / `call_failed` pair.
    fn call_start(&self, _ctx: &CallContext, _request: &Request<RequestBody>) {}

    /// Fired once when the entire call completes successfully.
    ///
    /// This includes delayed response-body consumption by the caller.
    fn call_end(&self, _ctx: &CallContext) {}

    /// Fired once when the entire call fails permanently.
    ///
    /// This may follow a body-phase callback such as `response_body_failed`
    /// when the terminal failure happens during response-body consumption.
    fn call_failed(&self, _ctx: &CallContext, _error: &WireError) {}

    fn dns_start(&self, _ctx: &CallContext, _host: &str, _port: u16) {}
    fn dns_end(&self, _ctx: &CallContext, _host: &str, _addrs: &[SocketAddr]) {}
    fn dns_failed(&self, _ctx: &CallContext, _host: &str, _error: &WireError) {}

    fn connect_start(&self, _ctx: &CallContext, _addr: SocketAddr) {}
    fn connect_end(&self, _ctx: &CallContext, _connection_id: ConnectionId, _addr: SocketAddr) {}
    fn connect_failed(&self, _ctx: &CallContext, _addr: SocketAddr, _error: &WireError) {}

    fn tls_start(&self, _ctx: &CallContext, _server_name: &str) {}
    fn tls_end(&self, _ctx: &CallContext, _server_name: &str) {}
    fn tls_failed(&self, _ctx: &CallContext, _server_name: &str, _error: &WireError) {}

    fn request_headers_start(&self, _ctx: &CallContext) {}
    fn request_headers_end(&self, _ctx: &CallContext) {}
    fn request_body_end(&self, _ctx: &CallContext, _bytes_sent: u64) {}

    fn response_headers_start(&self, _ctx: &CallContext) {}
    fn response_headers_end(&self, _ctx: &CallContext, _response: &Response<ResponseBody>) {}

    /// Fired when the response body is closed or exhausted successfully.
    ///
    /// If the caller abandons the body early, `bytes_read` is the number of
    /// bytes delivered before close.
    fn response_body_end(&self, _ctx: &CallContext, _bytes_read: u64) {}

    /// Fired when the response body cannot be read to the point it was closed.
    fn response_body_failed(&self, _ctx: &CallContext, _error: &WireError) {}

    fn pool_lookup(&self, _ctx: &CallContext, _hit: bool, _connection_id: Option<ConnectionId>) {}

    fn connection_acquired(&self, _ctx: &CallContext, _connection_id: ConnectionId, _reused: bool) {
    }

    fn connection_released(&self, _ctx: &CallContext, _connection_id: ConnectionId) {}

    fn route_plan(&self, _ctx: &CallContext, _route_count: usize, _fast_fallback_enabled: bool) {}

    fn connect_race_start(
        &self,
        _ctx: &CallContext,
        _race_id: u64,
        _route_index: usize,
        _route_count: usize,
        _route_family: &str,
    ) {
    }

    fn connect_race_won(
        &self,
        _ctx: &CallContext,
        _race_id: u64,
        _route_index: usize,
        _route_count: usize,
    ) {
    }

    fn connect_race_lost(
        &self,
        _ctx: &CallContext,
        _race_id: u64,
        _route_index: usize,
        _route_count: usize,
        _reason: &str,
    ) {
    }

    fn retry(&self, _ctx: &CallContext, _attempt: u32, _reason: &str) {}
    fn redirect(&self, _ctx: &CallContext, _attempt: u32, _location: &Uri) {}

    // ─── WebSocket lifecycle (feature = "websocket") ───
    /// Fired once when the 101 response has been validated and the engine has
    /// taken ownership of the upgraded IO.
    #[cfg(feature = "websocket")]
    fn websocket_open(
        &self,
        _ctx: &CallContext,
        _handshake: &crate::websocket::WebSocketHandshake,
    ) {
    }

    #[cfg(feature = "websocket")]
    fn websocket_message_sent(
        &self,
        _ctx: &CallContext,
        _kind: crate::websocket::MessageKind,
        _payload_len: usize,
    ) {
    }

    #[cfg(feature = "websocket")]
    fn websocket_message_received(
        &self,
        _ctx: &CallContext,
        _kind: crate::websocket::MessageKind,
        _payload_len: usize,
    ) {
    }

    #[cfg(feature = "websocket")]
    fn websocket_ping_sent(&self, _ctx: &CallContext) {}

    #[cfg(feature = "websocket")]
    fn websocket_pong_received(&self, _ctx: &CallContext) {}

    /// Fired when a close frame is being sent or received. `initiator`
    /// distinguishes user-initiated close from peer-initiated.
    #[cfg(feature = "websocket")]
    fn websocket_closing(
        &self,
        _ctx: &CallContext,
        _code: u16,
        _reason: &str,
        _initiator: crate::websocket::CloseInitiator,
    ) {
    }

    /// Fired once after the close handshake completes (or times out).
    #[cfg(feature = "websocket")]
    fn websocket_closed(&self, _ctx: &CallContext, _code: u16, _reason: &str) {}

    /// Fired when the WebSocket session terminates with an error before a
    /// graceful close handshake completes.
    #[cfg(feature = "websocket")]
    fn websocket_failed(
        &self,
        _ctx: &CallContext,
        _error: &crate::websocket::WebSocketError,
    ) {
    }
}

pub trait EventListenerFactory: Send + Sync + 'static {
    fn create(&self, request: &Request<RequestBody>) -> SharedEventListener;
}

#[derive(Debug, Default)]
pub struct NoopEventListener;

impl EventListener for NoopEventListener {}

#[derive(Debug, Default)]
pub struct NoopEventListenerFactory;

impl EventListenerFactory for NoopEventListenerFactory {
    fn create(&self, _request: &Request<RequestBody>) -> SharedEventListener {
        Arc::new(NoopEventListener)
    }
}
