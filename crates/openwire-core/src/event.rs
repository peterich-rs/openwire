use std::net::SocketAddr;
use std::sync::Arc;

use http::{Request, Response, Uri};

use crate::{CallContext, ConnectionId, RequestBody, ResponseBody, WireError};

pub type SharedEventListener = Arc<dyn EventListener>;
pub type SharedEventListenerFactory = Arc<dyn EventListenerFactory>;

pub trait EventListener: Send + Sync + 'static {
    fn call_start(&self, _ctx: &CallContext, _request: &Request<RequestBody>) {}
    fn call_end(&self, _ctx: &CallContext, _response: &Response<ResponseBody>) {}
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
    fn response_body_end(&self, _ctx: &CallContext, _bytes_read: u64) {}
    fn response_body_failed(&self, _ctx: &CallContext, _error: &WireError) {}

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
