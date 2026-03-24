use std::cmp;
use std::collections::HashMap;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::task::noop_waker_ref;
use http::header::{HeaderValue, CONNECTION};
use http::{HeaderMap, Method, Request};
use hyper::rt::{Read, ReadBuf, Write};
use hyper::Uri;
use hyper_util::client::legacy::connect::Connected;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tracing::field::{Field, Visit};
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use openwire_core::{
    AuthContext, Authenticator, BoxConnection, BoxFuture, BoxTaskHandle, CallContext,
    DnsResolver, NoopEventListener, RequestBody, SharedTimer, TaskHandle, TcpConnector,
    WireError, WireExecutor,
};

use super::{
    abandon_response_lease, parse_connect_response_head, record_fast_fallback_trace,
    split_connect_response_head, ConnectionTaskRegistry, FastFallbackOutcome,
    PrefetchedTunnelBytes,
};
use crate::connection::ConnectionPool;
use crate::connection::{
    Address, AuthorityKey, ConnectionProtocol, DnsPolicy, PoolSettings, ProtocolPolicy,
    RealConnection, Route, RoutePlan, RoutePlanner, UriScheme,
};
use crate::proxy::ProxySelector;

#[derive(Clone, Debug)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Default)]
struct TraceCaptureInner {
    span_order: Vec<u64>,
    spans: HashMap<u64, CapturedSpan>,
}

#[derive(Clone, Default)]
struct TraceCapture {
    inner: Arc<Mutex<TraceCaptureInner>>,
}

impl TraceCapture {
    fn spans_named(&self, name: &str) -> Vec<CapturedSpan> {
        let inner = self.inner.lock().expect("trace capture lock");
        inner
            .span_order
            .iter()
            .filter_map(|id| inner.spans.get(id))
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &Id,
        _ctx: LayerContext<'_, S>,
    ) {
        let mut visitor = FieldCapture::default();
        attrs.record(&mut visitor);
        let mut inner = self.inner.lock().expect("trace capture lock");
        let id = id.into_u64();
        inner.span_order.push(id);
        inner.spans.insert(
            id,
            CapturedSpan {
                name: attrs.metadata().name().to_owned(),
                fields: visitor.fields,
            },
        );
    }

    fn on_record(
        &self,
        id: &Id,
        values: &tracing::span::Record<'_>,
        _ctx: LayerContext<'_, S>,
    ) {
        let mut visitor = FieldCapture::default();
        values.record(&mut visitor);
        if let Some(span) = self
            .inner
            .lock()
            .expect("trace capture lock")
            .spans
            .get_mut(&id.into_u64())
        {
            span.fields.extend(visitor.fields);
        }
    }
}

#[derive(Default)]
struct FieldCapture {
    fields: HashMap<String, String>,
}

impl Visit for FieldCapture {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[test]
fn record_fast_fallback_trace_records_expected_attempt_fields() {
    let trace = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());
    tracing::dispatcher::with_default(&tracing::Dispatch::new(subscriber), || {
        let span = tracing::debug_span!(
            "openwire.attempt",
            route_count = tracing::field::Empty,
            fast_fallback_enabled = tracing::field::Empty,
            connect_race_id = tracing::field::Empty,
            connect_winner = tracing::field::Empty,
        );
        let _entered = span.enter();
        record_fast_fallback_trace(
            &span,
            FastFallbackOutcome {
                race_id: 7,
                route_count: 2,
                winner_index: 1,
                fast_fallback_enabled: true,
            },
        );
    });

    let spans = trace.spans_named("openwire.attempt");
    assert_eq!(spans.len(), 1, "spans = {spans:?}");
    let span = &spans[0];
    assert_eq!(
        span.fields.get("route_count").map(String::as_str),
        Some("2")
    );
    assert_eq!(
        span.fields.get("fast_fallback_enabled").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        span.fields.get("connect_race_id").map(String::as_str),
        Some("7")
    );
    assert_eq!(
        span.fields.get("connect_winner").map(String::as_str),
        Some("1")
    );
}

#[test]
fn connection_header_close_matches_multiple_values() {
    let mut headers = HeaderMap::new();
    headers.append(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.append(CONNECTION, HeaderValue::from_static("close"));

    assert!(super::connection_header_requests_close(&headers));
}

#[test]
fn connection_header_close_matches_comma_separated_tokens() {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, close"));

    assert!(super::connection_header_requests_close(&headers));
}

#[test]
fn connection_header_close_ignores_non_close_tokens() {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, upgrade"));

    assert!(!super::connection_header_requests_close(&headers));
}

#[test]
fn connection_header_close_tolerates_empty_members_and_tabs() {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONNECTION,
        HeaderValue::from_static("keep-alive,\t,\tclose\t,"),
    );

    assert!(super::connection_header_requests_close(&headers));
}

#[test]
fn connection_header_close_conservatively_rejects_malformed_members() {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive;timeout=5"));

    assert!(super::connection_header_requests_close(&headers));
}

fn make_address() -> Address {
    Address::new(
        UriScheme::Https,
        AuthorityKey::new("example.com", 443),
        None,
        None,
        ProtocolPolicy::Http1OrHttp2,
        DnsPolicy::System,
    )
}

fn make_connection(protocol: ConnectionProtocol) -> RealConnection {
    let route = Route::direct(
        make_address(),
        std::net::SocketAddr::from(([192, 0, 2, 10], 443)),
    );
    RealConnection::new(route, protocol)
}

fn make_call_context() -> CallContext {
    CallContext::new(Arc::new(NoopEventListener), None)
}

struct DuplexConnection {
    inner: openwire_tokio::TokioIo<tokio::io::DuplexStream>,
}

impl DuplexConnection {
    fn new(stream: tokio::io::DuplexStream) -> Self {
        Self {
            inner: openwire_tokio::TokioIo::new(stream),
        }
    }
}

impl hyper_util::client::legacy::connect::Connection for DuplexConnection {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl Read for DuplexConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl Write for DuplexConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn duplex_box_connection(capacity: usize) -> (BoxConnection, tokio::io::DuplexStream) {
    let (client, server) = tokio::io::duplex(capacity);
    (
        Box::new(DuplexConnection::new(client)) as BoxConnection,
        server,
    )
}

struct ScriptedConnection {
    reads: Vec<u8>,
    read_pos: usize,
    writes: Arc<Mutex<Vec<u8>>>,
}

impl ScriptedConnection {
    fn new(reads: &[u8]) -> Self {
        Self {
            reads: reads.to_vec(),
            read_pos: 0,
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl hyper_util::client::legacy::connect::Connection for ScriptedConnection {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl Read for ScriptedConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.read_pos >= this.reads.len() {
            return Poll::Ready(Ok(()));
        }

        let copy_len = cmp::min(buf.remaining(), this.reads.len() - this.read_pos);
        buf.put_slice(&this.reads[this.read_pos..this.read_pos + copy_len]);
        this.read_pos += copy_len;
        Poll::Ready(Ok(()))
    }
}

impl Write for ScriptedConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.writes
            .lock()
            .expect("scripted writes lock")
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let written = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        let this = self.get_mut();
        let mut writes = this.writes.lock().expect("scripted writes lock");
        for buf in bufs {
            writes.extend_from_slice(buf);
        }
        Poll::Ready(Ok(written))
    }
}

#[derive(Clone, Default)]
struct CountingSpawnRuntime {
    spawns: Arc<AtomicUsize>,
}

impl CountingSpawnRuntime {
    fn spawns(&self) -> usize {
        self.spawns.load(std::sync::atomic::Ordering::Relaxed)
    }
}

struct NoopTaskHandle;

impl TaskHandle for NoopTaskHandle {
    fn abort(&self) {}
}

impl WireExecutor for CountingSpawnRuntime {
    fn spawn(&self, _future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        self.spawns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Box::new(NoopTaskHandle))
    }
}

#[derive(Clone)]
struct RecordingRetryTcpConnector {
    stream: Arc<Mutex<Option<BoxConnection>>>,
    timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
}

impl RecordingRetryTcpConnector {
    fn new(stream: BoxConnection) -> Self {
        Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            timeouts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn timeouts(&self) -> Vec<Option<Duration>> {
        self.timeouts.lock().expect("tcp timeout lock").clone()
    }
}

impl TcpConnector for RecordingRetryTcpConnector {
    fn connect(
        &self,
        _ctx: CallContext,
        _addr: std::net::SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        self.timeouts
            .lock()
            .expect("tcp timeout lock")
            .push(timeout);
        let stream = self
            .stream
            .lock()
            .expect("tcp stream lock")
            .take()
            .expect("retry tcp stream");
        Box::pin(async move { Ok(stream) })
    }
}

#[derive(Clone, Default)]
struct StaticProxyAuthenticator {
    calls: Arc<AtomicUsize>,
}

impl StaticProxyAuthenticator {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Authenticator for StaticProxyAuthenticator {
    fn authenticate(
        &self,
        _ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async move {
            let mut request = Request::new(RequestBody::empty());
            *request.method_mut() = Method::CONNECT;
            *request.uri_mut() = "https://example.com:443/".parse().expect("auth uri");
            request.headers_mut().insert(
                "proxy-authorization",
                HeaderValue::from_static("Basic dXNlcjpwYXNz"),
            );
            Ok(Some(request))
        })
    }
}

#[derive(Clone)]
struct RecordingDnsResolver {
    calls: Arc<Mutex<Vec<(String, u16)>>>,
    resolved_addrs: Vec<std::net::SocketAddr>,
}

impl DnsResolver for RecordingDnsResolver {
    fn resolve(
        &self,
        _ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<std::net::SocketAddr>, WireError>> {
        self.calls
            .lock()
            .expect("dns calls lock")
            .push((host, port));
        let resolved_addrs = self.resolved_addrs.clone();
        Box::pin(async move { Ok(resolved_addrs) })
    }
}

#[derive(Clone)]
struct UnusedTcpConnector;

impl TcpConnector for UnusedTcpConnector {
    fn connect(
        &self,
        _ctx: CallContext,
        _addr: std::net::SocketAddr,
        _timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        Box::pin(async { panic!("tcp connector should not be used in route planner test") })
    }
}

type PlannedRoutes = Arc<Mutex<Vec<(Address, Vec<std::net::SocketAddr>)>>>;

#[derive(Clone)]
struct RecordingRoutePlanner {
    planned: PlannedRoutes,
}

impl RoutePlanner for RecordingRoutePlanner {
    fn dns_target(&self, _address: &Address) -> (String, u16) {
        ("planner.test".to_owned(), 8443)
    }

    fn plan(
        &self,
        address: &Address,
        resolved_addrs: Vec<std::net::SocketAddr>,
    ) -> Result<RoutePlan, WireError> {
        self.planned
            .lock()
            .expect("route planner calls lock")
            .push((address.clone(), resolved_addrs.clone()));
        Ok(RoutePlan::new(
            vec![Route::direct(address.clone(), resolved_addrs[0])],
            Duration::from_millis(37),
        ))
    }
}

fn poll_read_exact<T>(io: &mut T, out: &mut [u8])
where
    T: Read + Unpin,
{
    let waker = noop_waker_ref();
    let mut cx = Context::from_waker(waker);
    let mut read_buf = ReadBuf::new(out);
    let result = Pin::new(io).poll_read(&mut cx, read_buf.unfilled());
    match result {
        Poll::Ready(Ok(())) => assert_eq!(read_buf.filled().len(), out.len()),
        Poll::Ready(Err(error)) => panic!("unexpected read error: {error}"),
        Poll::Pending => panic!("unexpected pending read"),
    }
}

async fn read_until_headers_end(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let read = stream.read(&mut buf).await.expect("read request bytes");
        assert!(read > 0, "peer closed before headers completed");
        response.extend_from_slice(&buf[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            return response;
        }
    }
}

async fn read_socks5_connect_request(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .expect("read socks5 request head");
    let mut request = head.to_vec();
    match head[3] {
        0x01 => {
            let mut remainder = [0u8; 6];
            stream
                .read_exact(&mut remainder)
                .await
                .expect("read socks5 ipv4 remainder");
            request.extend_from_slice(&remainder);
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .expect("read socks5 host len");
            request.extend_from_slice(&len);
            let mut remainder = vec![0u8; len[0] as usize + 2];
            stream
                .read_exact(&mut remainder)
                .await
                .expect("read socks5 host remainder");
            request.extend_from_slice(&remainder);
        }
        0x04 => {
            let mut remainder = [0u8; 18];
            stream
                .read_exact(&mut remainder)
                .await
                .expect("read socks5 ipv6 remainder");
            request.extend_from_slice(&remainder);
        }
        atyp => panic!("unexpected SOCKS5 atyp {atyp:#04x}"),
    }
    request
}

#[test]
fn parse_connect_response_head_preserves_prefetched_tunnel_bytes() {
    let response =
        b"HTTP/1.1 200 Connection Established\r\nProxy-Agent: openwire\r\n\r\n\x00\xffprefetch";
    let split_at = super::find_connect_response_end(response).expect("split");
    let (head, prefetched) =
        split_connect_response_head(response, split_at).expect("connect response split");
    let parsed = parse_connect_response_head(head, prefetched.clone()).expect("parsed");

    assert_eq!(parsed.status, http::StatusCode::OK);
    assert_eq!(parsed.status_line, "HTTP/1.1 200 Connection Established");
    assert_eq!(
        parsed
            .headers
            .get("proxy-agent")
            .and_then(|value| value.to_str().ok()),
        Some("openwire")
    );
    assert_eq!(parsed.prefetched, prefetched);
    assert_eq!(parsed.prefetched, Bytes::from_static(b"\x00\xffprefetch"));
}

#[test]
fn parse_connect_response_head_ignores_407_body_bytes_in_same_read() {
    let response = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 4\r\n\r\nbody";
    let split_at = super::find_connect_response_end(response).expect("split");
    let (head, prefetched) =
        split_connect_response_head(response, split_at).expect("connect response split");
    let parsed = parse_connect_response_head(head, prefetched.clone()).expect("parsed");

    assert_eq!(
        parsed.status,
        http::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );
    assert_eq!(
        parsed
            .headers
            .get("proxy-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"proxy\"")
    );
    assert_eq!(parsed.prefetched, Bytes::from_static(b"body"));
    assert_eq!(parsed.prefetched, prefetched);
}

#[test]
fn connect_budget_preserves_none_semantics_without_deadlines() {
    let budget = super::ConnectBudget::new(None, None);
    assert_eq!(budget.remaining(), None);
}

#[test]
fn connect_budget_uses_the_earliest_available_deadline() {
    let budget = super::ConnectBudget::new(
        Some(Duration::from_secs(30)),
        Some(Instant::now() + Duration::from_millis(250)),
    );

    let remaining = budget.remaining().expect("remaining budget");
    assert!(remaining < Duration::from_secs(1));
}

#[test]
fn connect_budget_handles_huge_timeouts_without_instant_overflow() {
    let budget = super::ConnectBudget::new(Some(Duration::MAX), None);

    let remaining = budget.remaining().expect("remaining budget");
    assert!(remaining > Duration::from_secs(60), "{remaining:?}");
}

#[test]
fn prefetched_tunnel_bytes_replays_prefetched_bytes_before_stream() {
    let inner = Box::new(ScriptedConnection::new(b"world")) as openwire_core::BoxConnection;
    let mut stream = PrefetchedTunnelBytes::new(inner, Bytes::from_static(b"hello"));

    let mut first = [0u8; 5];
    poll_read_exact(&mut stream, &mut first);
    assert_eq!(&first, b"hello");

    let mut second = [0u8; 5];
    poll_read_exact(&mut stream, &mut second);
    assert_eq!(&second, b"world");
}

#[test]
fn abandoned_http2_lease_does_not_poison_the_session() {
    let connection = make_connection(ConnectionProtocol::Http2);
    assert!(connection.try_acquire());
    let exchange_finder = Arc::new(crate::connection::ExchangeFinder::new(
        Arc::new(ConnectionPool::new(PoolSettings::default())),
        ProxySelector::new(Vec::new()),
    ));
    abandon_response_lease(Some(super::ResponseLease::http2(
        connection.clone(),
        super::ResponseLeaseShared::new(
            exchange_finder.clone(),
            make_call_context(),
            super::ConnectionTaskRegistry::default(),
            super::ConnectionAvailability::default(),
        ),
    )));

    let snapshot = connection.snapshot();
    assert_eq!(
        snapshot.health,
        crate::connection::ConnectionHealth::Healthy
    );
    assert_eq!(
        snapshot.allocation,
        crate::connection::ConnectionAllocationState::Idle
    );
}

#[test]
fn spawn_body_deadline_signal_marks_expired_deadlines_without_spawning() {
    let runtime = Arc::new(CountingSpawnRuntime::default());
    let timer = SharedTimer::new(openwire_tokio::TokioTimer::new());
    let ctx = CallContext::new(Arc::new(NoopEventListener), Some(Duration::ZERO));

    let signal = super::spawn_body_deadline_signal(runtime.clone(), timer, &ctx)
        .expect("deadline signal")
        .expect("signal");

    assert!(signal.expired.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(runtime.spawns(), 0);
}

#[tokio::test]
async fn connector_stack_uses_custom_route_planner() {
    let dns_calls = Arc::new(Mutex::new(Vec::new()));
    let planned = Arc::new(Mutex::new(Vec::new()));
    let resolved = vec![std::net::SocketAddr::from(([203, 0, 113, 10], 443))];
    let connector = super::ConnectorStack {
        dns_resolver: Arc::new(RecordingDnsResolver {
            calls: dns_calls.clone(),
            resolved_addrs: resolved.clone(),
        }),
        tcp_connector: Arc::new(UnusedTcpConnector),
        tls_connector: None,
        connect_timeout: None,
        executor: Arc::new(CountingSpawnRuntime::default()),
        timer: SharedTimer::new(openwire_tokio::TokioTimer::new()),
        route_planner: Arc::new(RecordingRoutePlanner {
            planned: planned.clone(),
        }),
        proxy_authenticator: None,
        max_proxy_auth_attempts: 0,
    };

    let route_plan = connector
        .route_plan(make_call_context(), &make_address())
        .await
        .expect("route plan");

    assert_eq!(
        dns_calls.lock().expect("dns calls lock").as_slice(),
        &[("planner.test".to_owned(), 8443)]
    );
    let planned = planned.lock().expect("route planner calls lock");
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].0, make_address());
    assert_eq!(planned[0].1, resolved);
    assert_eq!(
        route_plan.fast_fallback_stagger(),
        Duration::from_millis(37)
    );
}

#[tokio::test]
async fn connect_tunnel_shares_budget_across_407_redial_and_response_read() {
    let timer = SharedTimer::new(openwire_tokio::TokioTimer::new());
    let target_uri: Uri = "https://example.com:443/".parse().expect("target uri");
    let (initial_stream, mut initial_peer) = duplex_box_connection(1024);
    let (retry_stream, mut retry_peer) = duplex_box_connection(1024);
    let retry_connector = RecordingRetryTcpConnector::new(retry_stream);
    let authenticator_impl = Arc::new(StaticProxyAuthenticator::default());
    let authenticator = authenticator_impl.clone() as super::SharedAuthenticator;

    let first_response = tokio::spawn(async move {
        let request = read_until_headers_end(&mut initial_peer).await;
        let request = String::from_utf8(request).expect("CONNECT request utf8");
        assert!(request.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        initial_peer
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\n\r\n",
            )
            .await
            .expect("write 407");
    });
    let second_response = tokio::spawn(async move {
        let request = read_until_headers_end(&mut retry_peer).await;
        let request = String::from_utf8(request).expect("retry CONNECT request utf8");
        assert!(request.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
        assert!(request.contains("proxy-authorization: Basic dXNlcjpwYXNz\r\n"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = retry_peer
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await;
    });

    let error = match super::establish_connect_tunnel(super::ConnectTunnelParams {
        ctx: make_call_context(),
        proxy_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 8080)),
        target_uri: &target_uri,
        stream: initial_stream,
        tcp_connector: Arc::new(retry_connector.clone()),
        proxy_authenticator: Some(authenticator.clone()),
        max_proxy_auth_attempts: 1,
        budget: super::ConnectBudget::new(Some(Duration::from_millis(80)), None),
        timer: timer.clone(),
    })
    .await
    {
        Ok(_) => panic!("shared connect budget should time out"),
        Err(error) => error,
    };

    assert!(error.is_connect_timeout(), "{error:?}");
    assert!(
        error.message().contains("proxy CONNECT timed out"),
        "{error:?}"
    );
    let timeouts = retry_connector.timeouts();
    assert_eq!(timeouts.len(), 1);
    assert!(timeouts[0].is_some(), "retry dial should keep a timeout");
    assert!(
        timeouts[0].expect("retry timeout") < Duration::from_millis(80),
        "retry timeout should use remaining budget: {timeouts:?}"
    );
    assert_eq!(authenticator_impl.calls(), 1);

    first_response.await.expect("first response task");
    second_response.await.expect("second response task");
}

#[tokio::test]
async fn socks5_tunnel_shares_budget_across_handshake_steps() {
    let timer = SharedTimer::new(openwire_tokio::TokioTimer::new());
    let target_uri: Uri = "https://example.com:443/".parse().expect("target uri");
    let (stream, mut peer) = duplex_box_connection(1024);

    let server = tokio::spawn(async move {
        let mut greeting = [0u8; 3];
        peer.read_exact(&mut greeting).await.expect("read greeting");
        assert_eq!(greeting, [0x05, 0x01, 0x00]);
        tokio::time::sleep(Duration::from_millis(30)).await;
        peer.write_all(&[0x05, 0x00])
            .await
            .expect("write server choice");

        let request = read_socks5_connect_request(&mut peer).await;
        assert_eq!(&request[..3], &[0x05, 0x01, 0x00]);
        assert_eq!(request[3], 0x03);
        assert_eq!(request[4] as usize, "example.com".len());
        assert_eq!(&request[5..5 + "example.com".len()], b"example.com");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = peer
            .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0x01, 0xbb])
            .await;
    });

    let error = match super::establish_socks5_tunnel(
        make_call_context(),
        &target_uri,
        std::net::SocketAddr::from(([127, 0, 0, 1], 1080)),
        stream,
        super::ConnectBudget::new(Some(Duration::from_millis(80)), None),
        timer,
        None,
    )
    .await
    {
        Ok(_) => panic!("shared SOCKS5 budget should time out"),
        Err(error) => error,
    };

    assert!(error.is_connect_timeout(), "{error:?}");
    assert!(
        error.message().contains("SOCKS5 handshake timed out"),
        "{error:?}"
    );

    server.await.expect("socks5 server task");
}

#[test]
fn connection_task_registry_recovers_after_mutex_poisoning() {
    let registry = ConnectionTaskRegistry::default();

    let _ = panic::catch_unwind(AssertUnwindSafe(|| registry.poison_handles_for_test()));

    let (task_id, _weak) = registry.reserve();
    registry.cancel(task_id);
}
