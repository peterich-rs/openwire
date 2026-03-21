use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::header::HOST;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Version};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use openwire_core::{
    next_connection_id, BoxConnection, BoxFuture, CallContext, ConnectionId, ConnectionInfo,
    DnsResolver, Exchange, RequestBody, ResponseBody, Runtime, TcpConnector, TlsConnector,
    TokioExecutor, TokioIo, TokioTimer, WireError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::Service;
use tracing::Instrument;

use crate::auth::{
    AuthAttemptState, AuthContext, AuthKind, AuthRequestState, AuthResponseState,
    SharedAuthenticator,
};
use crate::connection::{
    Address, ConnectionProtocol, ExchangeFinder, FastFallbackDialer, Route, RouteKind,
    RoutePlanner, UriScheme,
};
use crate::proxy::Proxy;
use crate::trace::PolicyTraceContext;

#[derive(Clone, Debug, Default)]
pub struct SystemDnsResolver;

impl DnsResolver for SystemDnsResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(addrs) => {
                    let addrs: Vec<_> = addrs.collect();
                    if addrs.is_empty() {
                        let error = WireError::dns(
                            "DNS resolution returned no socket addresses",
                            io::Error::new(io::ErrorKind::NotFound, "empty DNS result"),
                        );
                        ctx.listener().dns_failed(&ctx, &host, &error);
                        return Err(error);
                    }
                    ctx.listener().dns_end(&ctx, &host, &addrs);
                    Ok(addrs)
                }
                Err(error) => {
                    let error = WireError::dns("DNS resolution failed", error);
                    ctx.listener().dns_failed(&ctx, &host, &error);
                    Err(error)
                }
            }
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct TokioTcpConnector;

impl TcpConnector for TokioTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        Box::pin(async move {
            ctx.listener().connect_start(&ctx, addr);
            let connect = tokio::net::TcpStream::connect(addr);
            let stream = match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, connect).await {
                    Ok(result) => {
                        result.map_err(|error| WireError::connect("TCP connect failed", error))?
                    }
                    Err(_error) => {
                        let error = WireError::connect_timeout(format!(
                            "connection timed out after {timeout:?}"
                        ));
                        ctx.listener().connect_failed(&ctx, addr, &error);
                        return Err(error);
                    }
                },
                None => connect
                    .await
                    .map_err(|error| WireError::connect("TCP connect failed", error))?,
            };

            stream
                .set_nodelay(true)
                .map_err(|error| WireError::connect("failed to configure TCP_NODELAY", error))?;

            let info = ConnectionInfo {
                id: next_connection_id(),
                remote_addr: stream.peer_addr().ok(),
                local_addr: stream.local_addr().ok(),
                tls: false,
            };

            ctx.mark_connection_established();
            ctx.listener().connect_end(&ctx, info.id, addr);

            Ok(Box::new(TcpConnection {
                inner: TokioIo::new(stream),
                info,
            }) as BoxConnection)
        })
    }
}

struct TcpConnection {
    inner: TokioIo<tokio::net::TcpStream>,
    info: ConnectionInfo,
}

impl Connection for TcpConnection {
    fn connected(&self) -> Connected {
        Connected::new().extra(self.info.clone())
    }
}

impl hyper::rt::Read for TcpConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TcpConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

#[derive(Clone)]
pub(crate) struct ConnectorStack {
    pub(crate) dns_resolver: Arc<dyn DnsResolver>,
    pub(crate) tcp_connector: Arc<dyn TcpConnector>,
    pub(crate) tls_connector: Option<Arc<dyn TlsConnector>>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) proxies: Vec<Proxy>,
    pub(crate) route_planner: RoutePlanner,
    pub(crate) proxy_authenticator: Option<SharedAuthenticator>,
    pub(crate) max_proxy_auth_attempts: usize,
}

#[derive(Clone)]
struct ProxyConnectDeps {
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    proxy_authenticator: Option<SharedAuthenticator>,
    max_proxy_auth_attempts: usize,
    connect_timeout: Option<Duration>,
}

struct ConnectTunnelParams<'a> {
    ctx: CallContext,
    proxy_addr: SocketAddr,
    target_uri: &'a Uri,
    stream: BoxConnection,
    tcp_connector: Arc<dyn TcpConnector>,
    proxy_authenticator: Option<SharedAuthenticator>,
    max_proxy_auth_attempts: usize,
    connect_timeout: Option<Duration>,
}

impl ConnectorStack {
    async fn connect(
        &self,
        ctx: CallContext,
        uri: Uri,
        address: Address,
    ) -> Result<BoxConnection, WireError> {
        let proxy = self.proxies.iter().find(|proxy| proxy.matches(&uri));
        let proxy_connect_deps = ProxyConnectDeps {
            dns_resolver: self.dns_resolver.clone(),
            tcp_connector: self.tcp_connector.clone(),
            tls_connector: self.tls_connector.clone(),
            proxy_authenticator: self.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.max_proxy_auth_attempts,
            connect_timeout: self.connect_timeout,
        };

        if let Some(proxy) = proxy {
            if proxy.intercepts_http()
                && uri
                    .scheme_str()
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
            {
                return connect_via_http_forward_proxy(
                    ctx,
                    address,
                    self.route_planner.clone(),
                    proxy_connect_deps.dns_resolver.clone(),
                    proxy_connect_deps.tcp_connector.clone(),
                    proxy_connect_deps.connect_timeout,
                )
                .await;
            }

            if proxy.intercepts_https()
                && uri
                    .scheme_str()
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
            {
                return connect_via_http_proxy(
                    ctx,
                    uri,
                    address,
                    self.route_planner.clone(),
                    proxy_connect_deps,
                )
                .await;
            }
        }

        connect_direct(
            ctx,
            uri,
            address,
            self.route_planner.clone(),
            self.dns_resolver.clone(),
            self.tcp_connector.clone(),
            self.tls_connector.clone(),
            self.connect_timeout,
        )
        .await
    }
}

async fn connect_direct(
    ctx: CallContext,
    uri: Uri,
    address: Address,
    route_planner: RoutePlanner,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    connect_timeout: Option<Duration>,
) -> Result<BoxConnection, WireError> {
    let host = address.authority().host().to_owned();
    let port = address.authority().port();
    let addrs = dns_resolver.resolve(ctx.clone(), host, port).await?;
    let route_plan = route_planner.plan_direct(address, addrs);
    let (stream, outcome) = FastFallbackDialer
        .dial_direct(
            ctx.clone(),
            uri,
            route_plan,
            tcp_connector,
            tls_connector,
            connect_timeout,
        )
        .await?;
    let span = tracing::Span::current();
    span.record("route_count", outcome.route_count as u64);
    span.record("fast_fallback_enabled", outcome.fast_fallback_enabled);
    span.record("connect_race_id", outcome.race_id);
    span.record("connect_winner", outcome.winner_index as u64);
    Ok(stream)
}

async fn connect_via_http_forward_proxy(
    ctx: CallContext,
    address: Address,
    route_planner: RoutePlanner,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    connect_timeout: Option<Duration>,
) -> Result<BoxConnection, WireError> {
    let proxy = address.proxy().ok_or_else(|| {
        WireError::internal(
            "missing proxy config for forward proxy route",
            io::Error::other("missing proxy config"),
        )
    })?;
    let proxy_host = proxy.endpoint().authority().host().to_owned();
    let proxy_port = proxy.endpoint().authority().port();
    let addrs = dns_resolver
        .resolve(ctx.clone(), proxy_host, proxy_port)
        .await?;
    let route_plan = route_planner.plan_http_forward(address, addrs);
    let mut last_error = None;

    for route in route_plan.iter().cloned() {
        let &RouteKind::HttpForwardProxy { proxy: addr } = route.kind() else {
            continue;
        };
        match tcp_connector
            .connect(ctx.clone(), addr, connect_timeout)
            .await
        {
            Ok(stream) => {
                return Ok(Box::new(ProxiedConnection { inner: stream }) as BoxConnection);
            }
            Err(error) => {
                ctx.listener().connect_failed(&ctx, addr, &error);
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        WireError::connect(
            "no proxy socket addresses could be connected",
            io::Error::new(io::ErrorKind::NotConnected, "no proxy address connected"),
        )
    }))
}

async fn connect_via_http_proxy(
    ctx: CallContext,
    target_uri: Uri,
    address: Address,
    route_planner: RoutePlanner,
    deps: ProxyConnectDeps,
) -> Result<BoxConnection, WireError> {
    let target_scheme = target_uri
        .scheme_str()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a scheme"))?;
    if !target_scheme.eq_ignore_ascii_case("https") {
        return Err(WireError::invalid_request(
            "configured proxy currently supports HTTPS requests only",
        ));
    }

    let proxy = address.proxy().ok_or_else(|| {
        WireError::internal(
            "missing proxy config for CONNECT route",
            io::Error::other("missing proxy config"),
        )
    })?;
    let proxy_host = proxy.endpoint().authority().host().to_owned();
    let proxy_port = proxy.endpoint().authority().port();
    let addrs = deps
        .dns_resolver
        .resolve(ctx.clone(), proxy_host, proxy_port)
        .await?;
    let route_plan = route_planner.plan_connect_proxy(address, addrs);
    let mut last_error = None;

    for route in route_plan.iter().cloned() {
        let &RouteKind::ConnectProxy { proxy: addr } = route.kind() else {
            continue;
        };
        match deps
            .tcp_connector
            .connect(ctx.clone(), addr, deps.connect_timeout)
            .await
        {
            Ok(stream) => {
                let tunneled = match establish_connect_tunnel(ConnectTunnelParams {
                    ctx: ctx.clone(),
                    proxy_addr: addr,
                    target_uri: &target_uri,
                    stream,
                    tcp_connector: deps.tcp_connector.clone(),
                    proxy_authenticator: deps.proxy_authenticator.clone(),
                    max_proxy_auth_attempts: deps.max_proxy_auth_attempts,
                    connect_timeout: deps.connect_timeout,
                })
                .await
                {
                    Ok(stream) => stream,
                    Err(error) => {
                        ctx.listener().connect_failed(&ctx, addr, &error);
                        last_error = Some(error);
                        continue;
                    }
                };

                let tls_connector = deps.tls_connector.clone().ok_or_else(|| {
                    WireError::tls(
                        "HTTPS requested but no TLS connector is configured",
                        io::Error::new(io::ErrorKind::Unsupported, "missing TLS connector"),
                    )
                })?;
                return tls_connector
                    .connect(ctx.clone(), target_uri, tunneled)
                    .await;
            }
            Err(error) => {
                ctx.listener().connect_failed(&ctx, addr, &error);
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        WireError::connect(
            "no proxy socket addresses could be connected",
            io::Error::new(io::ErrorKind::NotConnected, "no proxy address connected"),
        )
    }))
}

async fn establish_connect_tunnel(
    params: ConnectTunnelParams<'_>,
) -> Result<BoxConnection, WireError> {
    let authority = params
        .target_uri
        .authority()
        .ok_or_else(|| WireError::invalid_request("request URI is missing an authority"))?
        .as_str()
        .to_owned();
    let mut stream = params.stream;
    let mut auth_count = 0u32;
    let mut connect_headers = HeaderMap::new();

    loop {
        match send_connect_request(&authority, &connect_headers, stream, params.connect_timeout)
            .await?
        {
            ConnectTunnelOutcome::Established(stream) => return Ok(stream),
            ConnectTunnelOutcome::Response(head) => {
                if head.status != StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                    return Err(proxy_connect_status_error(&head.status_line));
                }

                let Some(authenticator) = params.proxy_authenticator.as_ref() else {
                    return Err(proxy_connect_status_error(&head.status_line));
                };

                if auth_count as usize >= params.max_proxy_auth_attempts {
                    return Err(proxy_connect_status_error(&head.status_line));
                }

                let auth_ctx = AuthContext::new(
                    AuthKind::Proxy,
                    AuthRequestState::new(
                        Method::CONNECT,
                        params.target_uri.clone(),
                        Version::HTTP_11,
                        connect_request_headers(&authority, &connect_headers)?,
                        Some(RequestBody::empty()),
                    ),
                    AuthResponseState::new(head.status, head.headers.clone()),
                    AuthAttemptState {
                        total_attempt: 1,
                        retry_count: 0,
                        redirect_count: 0,
                        auth_count,
                    },
                );

                let Some(request) = authenticator.authenticate(auth_ctx).await? else {
                    return Err(proxy_connect_status_error(&head.status_line));
                };

                auth_count += 1;
                tracing::debug!(
                    call_id = params.ctx.call_id().as_u64(),
                    proxy_addr = %params.proxy_addr,
                    connect_auth_count = auth_count,
                    response_status = %head.status,
                    "retrying CONNECT after proxy authentication challenge",
                );

                connect_headers = sanitize_connect_headers(request.headers());
                stream = params
                    .tcp_connector
                    .connect(
                        params.ctx.clone(),
                        params.proxy_addr,
                        params.connect_timeout,
                    )
                    .await?;
            }
        }
    }
}

enum ConnectTunnelOutcome {
    Established(BoxConnection),
    Response(ConnectResponseHead),
}

#[derive(Clone)]
struct ConnectResponseHead {
    status: StatusCode,
    status_line: String,
    headers: HeaderMap,
}

async fn send_connect_request(
    authority: &str,
    headers: &HeaderMap,
    stream: BoxConnection,
    connect_timeout: Option<Duration>,
) -> Result<ConnectTunnelOutcome, WireError> {
    let connect_request = build_connect_request(authority, headers)?;

    let mut stream = TokioIo::new(stream);
    stream
        .write_all(connect_request.as_bytes())
        .await
        .map_err(|error| WireError::connect("failed to write proxy CONNECT request", error))?;
    stream
        .flush()
        .await
        .map_err(|error| WireError::connect("failed to flush proxy CONNECT request", error))?;

    let head = read_connect_response(&mut stream, connect_timeout).await?;
    if head.status == StatusCode::OK {
        return Ok(ConnectTunnelOutcome::Established(stream.into_inner()));
    }

    Ok(ConnectTunnelOutcome::Response(head))
}

fn build_connect_request(authority: &str, headers: &HeaderMap) -> Result<String, WireError> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    for (name, value) in headers {
        if *name == HOST {
            continue;
        }

        let value = value.to_str().map_err(|error| {
            WireError::invalid_request(format!(
                "CONNECT header {} is not valid ASCII: {error}",
                name.as_str()
            ))
        })?;
        request.push_str(name.as_str());
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    Ok(request)
}

async fn read_connect_response(
    stream: &mut TokioIo<BoxConnection>,
    connect_timeout: Option<Duration>,
) -> Result<ConnectResponseHead, WireError> {
    if let Some(timeout) = connect_timeout {
        return tokio::time::timeout(timeout, read_connect_response_inner(stream))
            .await
            .map_err(|_| {
                WireError::connect_timeout(format!("proxy CONNECT timed out after {timeout:?}"))
            })?;
    }

    read_connect_response_inner(stream).await
}

async fn read_connect_response_inner(
    stream: &mut TokioIo<BoxConnection>,
) -> Result<ConnectResponseHead, WireError> {
    let mut response = Vec::with_capacity(1024);
    let mut buf = [0u8; 512];
    loop {
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|error| WireError::connect("failed to read proxy CONNECT response", error))?;
        if read == 0 {
            return Err(WireError::connect(
                "proxy closed connection during CONNECT",
                io::Error::new(io::ErrorKind::UnexpectedEof, "proxy closed connection"),
            ));
        }
        response.extend_from_slice(&buf[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if response.len() > 8192 {
            return Err(WireError::connect(
                "proxy CONNECT response headers exceeded 8KiB",
                io::Error::new(io::ErrorKind::InvalidData, "proxy response too large"),
            ));
        }
    }

    parse_connect_response_head(&response)
}

fn parse_connect_response_head(response: &[u8]) -> Result<ConnectResponseHead, WireError> {
    let head = std::str::from_utf8(response)
        .map_err(|error| WireError::connect("proxy CONNECT response was not valid UTF-8", error))?;
    let head = head
        .split_once("\r\n\r\n")
        .map(|(head, _)| head)
        .unwrap_or(head);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| {
            WireError::connect(
                "proxy returned an empty CONNECT response",
                io::Error::new(io::ErrorKind::InvalidData, "empty proxy response"),
            )
        })?
        .to_owned();

    let status = parse_connect_status(&status_line)?;
    let mut headers = HeaderMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }

        let (name, value) = line.split_once(':').ok_or_else(|| {
            WireError::connect(
                format!("invalid proxy CONNECT header line: {line}"),
                io::Error::new(io::ErrorKind::InvalidData, "malformed proxy header"),
            )
        })?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|error| {
            WireError::connect(
                format!("invalid proxy CONNECT header name {name:?}"),
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;
        let value = HeaderValue::from_str(value.trim()).map_err(|error| {
            WireError::connect(
                format!("invalid proxy CONNECT header value for {}", name.as_str()),
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;
        headers.append(name, value);
    }

    Ok(ConnectResponseHead {
        status,
        status_line,
        headers,
    })
}

fn parse_connect_status(status_line: &str) -> Result<StatusCode, WireError> {
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next().unwrap_or_default();
    if protocol != "HTTP/1.1" && protocol != "HTTP/1.0" {
        return Err(WireError::connect(
            format!("proxy CONNECT returned an invalid status line: {status_line}"),
            io::Error::new(io::ErrorKind::InvalidData, "invalid proxy status line"),
        ));
    }

    let status = parts
        .next()
        .ok_or_else(|| {
            WireError::connect(
                format!("proxy CONNECT returned an invalid status line: {status_line}"),
                io::Error::new(io::ErrorKind::InvalidData, "missing proxy status code"),
            )
        })?
        .parse::<u16>()
        .map_err(|error| {
            WireError::connect(
                format!("proxy CONNECT returned an invalid status line: {status_line}"),
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;

    StatusCode::from_u16(status).map_err(|error| {
        WireError::connect(
            format!("proxy CONNECT returned an invalid status line: {status_line}"),
            io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })
}

fn connect_request_headers(authority: &str, headers: &HeaderMap) -> Result<HeaderMap, WireError> {
    let mut request_headers = sanitize_connect_headers(headers);
    let host = HeaderValue::from_str(authority).map_err(|error| {
        WireError::invalid_request(format!(
            "proxy CONNECT authority is not a valid Host header: {error}"
        ))
    })?;
    request_headers.insert(HOST, host);
    Ok(request_headers)
}

fn sanitize_connect_headers(headers: &HeaderMap) -> HeaderMap {
    let mut sanitized = HeaderMap::new();
    for (name, value) in headers {
        if *name == HOST {
            continue;
        }
        sanitized.append(name.clone(), value.clone());
    }
    sanitized
}

fn proxy_connect_status_error(status_line: &str) -> WireError {
    WireError::connect_non_retryable(format!("proxy CONNECT failed: {status_line}"))
}

struct ProxiedConnection {
    inner: BoxConnection,
}

impl Connection for ProxiedConnection {
    fn connected(&self) -> Connected {
        self.inner.connected().proxy(true)
    }
}

impl hyper::rt::Read for ProxiedConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for ProxiedConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

#[derive(Clone, Default)]
struct ConnectionBindings {
    inner: Arc<Mutex<HashMap<ConnectionId, ConnectionBinding>>>,
}

enum ConnectionBinding {
    Http1(Http1Binding),
    Http2(Http2Binding),
}

struct Http1Binding {
    info: ConnectionInfo,
    sender: Option<http1::SendRequest<RequestBody>>,
}

struct Http2Binding {
    info: ConnectionInfo,
    sender: http2::SendRequest<RequestBody>,
}

enum AcquiredBinding {
    Http1 {
        info: ConnectionInfo,
        sender: http1::SendRequest<RequestBody>,
    },
    Http2 {
        info: ConnectionInfo,
        sender: http2::SendRequest<RequestBody>,
    },
}

impl ConnectionBindings {
    fn insert_http1(
        &self,
        connection_id: ConnectionId,
        info: ConnectionInfo,
        sender: http1::SendRequest<RequestBody>,
    ) {
        self.inner.lock().expect("connection bindings lock").insert(
            connection_id,
            ConnectionBinding::Http1(Http1Binding {
                info,
                sender: Some(sender),
            }),
        );
    }

    fn insert_http2(
        &self,
        connection_id: ConnectionId,
        info: ConnectionInfo,
        sender: http2::SendRequest<RequestBody>,
    ) {
        self.inner.lock().expect("connection bindings lock").insert(
            connection_id,
            ConnectionBinding::Http2(Http2Binding { info, sender }),
        );
    }

    fn acquire(&self, connection_id: ConnectionId) -> Option<AcquiredBinding> {
        let mut bindings = self.inner.lock().expect("connection bindings lock");
        let mut remove_stale = false;
        let acquired = match bindings.get_mut(&connection_id)? {
            ConnectionBinding::Http1(binding) => {
                let sender = binding.sender.take()?;
                if sender.is_closed() {
                    remove_stale = true;
                    None
                } else {
                    Some(AcquiredBinding::Http1 {
                        info: binding.info.clone(),
                        sender,
                    })
                }
            }
            ConnectionBinding::Http2(binding) => {
                if binding.sender.is_closed() {
                    remove_stale = true;
                    None
                } else {
                    Some(AcquiredBinding::Http2 {
                        info: binding.info.clone(),
                        sender: binding.sender.clone(),
                    })
                }
            }
        };
        if remove_stale {
            bindings.remove(&connection_id);
        }
        acquired
    }

    fn release_http1(
        &self,
        connection_id: ConnectionId,
        sender: http1::SendRequest<RequestBody>,
    ) -> bool {
        if sender.is_closed() {
            self.remove(connection_id);
            return false;
        }

        let mut bindings = self.inner.lock().expect("connection bindings lock");
        let Some(ConnectionBinding::Http1(binding)) = bindings.get_mut(&connection_id) else {
            return false;
        };
        debug_assert!(
            binding.sender.is_none(),
            "HTTP/1 sender should be checked out"
        );
        binding.sender = Some(sender);
        true
    }

    fn remove(&self, connection_id: ConnectionId) {
        self.inner
            .lock()
            .expect("connection bindings lock")
            .remove(&connection_id);
    }
}

struct SelectedConnection {
    connection: crate::connection::RealConnection,
    binding: AcquiredBinding,
    reused: bool,
}

struct BoundResponse {
    response: Response<Incoming>,
    release: ResponseLease,
    reused: bool,
}

enum ResponseLease {
    Http1 {
        connection: crate::connection::RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
    },
    Http2 {
        connection: crate::connection::RealConnection,
    },
}

#[derive(Clone)]
pub(crate) struct TransportService {
    connector: ConnectorStack,
    config: crate::client::TransportConfig,
    runtime: Arc<dyn Runtime>,
    exchange_finder: Arc<ExchangeFinder>,
    bindings: Arc<ConnectionBindings>,
}

impl TransportService {
    pub(crate) fn new(
        connector: ConnectorStack,
        config: crate::client::TransportConfig,
        runtime: Arc<dyn Runtime>,
        exchange_finder: Arc<ExchangeFinder>,
    ) -> Self {
        Self {
            connector,
            config,
            runtime,
            exchange_finder,
            bindings: Arc::new(ConnectionBindings::default()),
        }
    }

    async fn execute_exchange(
        self,
        exchange: Exchange,
    ) -> Result<Response<ResponseBody>, WireError> {
        let (request, ctx, attempt) = exchange.into_parts();
        let prepared = self.exchange_finder.prepare(&request)?;
        let request_body_len = request.body().replayable_len();
        let policy_trace = request
            .extensions()
            .get::<PolicyTraceContext>()
            .copied()
            .unwrap_or_default();

        let call_span = attempt_span(&ctx, &request, attempt, policy_trace);
        async move {
            let selected = self
                .acquire_connection(&prepared, &request, ctx.clone(), tracing::Span::current())
                .await?;
            let response = send_bound_request(
                request,
                selected,
                self.exchange_finder.clone(),
                self.bindings.clone(),
            )
            .await?;

            if let Some(bytes) = request_body_len {
                ctx.listener().request_body_end(&ctx, bytes);
            }

            let connection_info = response
                .response
                .extensions()
                .get::<ConnectionInfo>()
                .cloned()
                .expect("owned response should carry connection info");
            ctx.listener()
                .connection_acquired(&ctx, connection_info.id, response.reused);
            let span = tracing::Span::current();
            span.record("connection_id", connection_info.id.as_u64());
            span.record("connection_reused", response.reused);

            ctx.listener().response_headers_start(&ctx);
            let (parts, body) = response.response.into_parts();
            let body = ObservedIncomingBody::wrap(
                body,
                ctx.clone(),
                attempt,
                Some(response.release),
                self.exchange_finder.clone(),
                tracing::Span::current(),
            );
            let response = Response::from_parts(parts, body);
            ctx.listener().response_headers_end(&ctx, &response);
            Ok(response)
        }
        .instrument(call_span)
        .await
    }

    async fn acquire_connection(
        &self,
        prepared: &crate::connection::PreparedExchange,
        request: &Request<RequestBody>,
        ctx: CallContext,
        span: tracing::Span,
    ) -> Result<SelectedConnection, WireError> {
        if let Some(connection) = prepared.reserved_connection() {
            if let Some(binding) = self.bindings.acquire(connection.id()) {
                return Ok(SelectedConnection {
                    connection: connection.clone(),
                    binding,
                    reused: true,
                });
            }

            let _ = self.exchange_finder.pool().remove(connection.id());
        }

        self.bind_fresh_connection(prepared, request, ctx, span)
            .await
    }

    async fn bind_fresh_connection(
        &self,
        prepared: &crate::connection::PreparedExchange,
        request: &Request<RequestBody>,
        ctx: CallContext,
        span: tracing::Span,
    ) -> Result<SelectedConnection, WireError> {
        let stream = self
            .connector
            .connect(ctx, request.uri().clone(), prepared.address().clone())
            .await?;
        let connected = stream.connected();
        let info = connection_info_from_connected(&connected);
        let protocol = determine_protocol(prepared.address(), &connected);
        let route = Route::from_observed(prepared.address().clone(), info.remote_addr);
        let connection = crate::connection::RealConnection::with_id(info.id, route, protocol);
        let _ = connection.try_acquire();
        self.exchange_finder.pool().insert(connection.clone());

        match protocol {
            ConnectionProtocol::Http1 => {
                let (sender, task) = bind_http1(stream).await?;
                self.bindings.insert_http1(info.id, info, sender);
                self.spawn_http1_task(connection.clone(), task, span)?;
            }
            ConnectionProtocol::Http2 => {
                let (sender, task) = bind_http2(stream, &self.config).await?;
                self.bindings.insert_http2(info.id, info, sender);
                self.spawn_http2_task(connection.clone(), task, span)?;
            }
        }

        let binding = self.bindings.acquire(connection.id()).ok_or_else(|| {
            self.bindings.remove(connection.id());
            let _ = self.exchange_finder.pool().remove(connection.id());
            WireError::internal(
                "freshly bound connection was not available for request execution",
                io::Error::other("bound connection missing immediately after insert"),
            )
        })?;

        Ok(SelectedConnection {
            connection,
            binding,
            reused: false,
        })
    }

    fn spawn_http1_task(
        &self,
        connection: crate::connection::RealConnection,
        task: http1::Connection<BoxConnection, RequestBody>,
        span: tracing::Span,
    ) -> Result<(), WireError> {
        let connection_id = connection.id();
        let bindings = self.bindings.clone();
        let pool = self.exchange_finder.pool().clone();
        self.runtime.spawn(Box::pin(
            async move {
                let result = task.await;
                bindings.remove(connection_id);
                let _ = pool.remove(connection_id);
                if let Err(error) = result {
                    tracing::debug!(
                        connection_id = connection_id.as_u64(),
                        error = %error,
                        "owned HTTP/1 connection task failed",
                    );
                }
            }
            .instrument(span),
        ))
    }

    fn spawn_http2_task(
        &self,
        connection: crate::connection::RealConnection,
        task: http2::Connection<BoxConnection, RequestBody, TokioExecutor>,
        span: tracing::Span,
    ) -> Result<(), WireError> {
        let connection_id = connection.id();
        let bindings = self.bindings.clone();
        let pool = self.exchange_finder.pool().clone();
        self.runtime.spawn(Box::pin(
            async move {
                let result = task.await;
                bindings.remove(connection_id);
                let _ = pool.remove(connection_id);
                if let Err(error) = result {
                    tracing::debug!(
                        connection_id = connection_id.as_u64(),
                        error = %error,
                        "owned HTTP/2 connection task failed",
                    );
                }
            }
            .instrument(span),
        ))
    }
}

impl Service<Exchange> for TransportService {
    type Response = Response<ResponseBody>;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        Box::pin(self.clone().execute_exchange(exchange))
    }
}

fn attempt_span(
    ctx: &CallContext,
    request: &Request<RequestBody>,
    attempt: u32,
    policy_trace: PolicyTraceContext,
) -> tracing::Span {
    // Canonical attempt-level tracing fields. Downstream tooling should rely on
    // these names instead of ad hoc transport-specific labels.
    tracing::debug_span!(
        "openwire.attempt",
        call_id = ctx.call_id().as_u64(),
        attempt,
        retry_count = policy_trace.retry_count,
        redirect_count = policy_trace.redirect_count,
        auth_count = policy_trace.auth_count,
        method = %request.method(),
        uri = %request.uri(),
        route_count = tracing::field::Empty,
        fast_fallback_enabled = tracing::field::Empty,
        connect_race_id = tracing::field::Empty,
        connect_winner = tracing::field::Empty,
        connection_id = tracing::field::Empty,
        connection_reused = tracing::field::Empty,
    )
}

struct ObservedIncomingBody {
    inner: Incoming,
    ctx: CallContext,
    attempt: u32,
    bytes_read: u64,
    release: Option<ResponseLease>,
    exchange_finder: Arc<ExchangeFinder>,
    span: tracing::Span,
    finished: bool,
}

impl ObservedIncomingBody {
    fn wrap(
        body: Incoming,
        ctx: CallContext,
        attempt: u32,
        release: Option<ResponseLease>,
        exchange_finder: Arc<ExchangeFinder>,
        span: tracing::Span,
    ) -> ResponseBody {
        ResponseBody::new(
            Self {
                inner: body,
                ctx,
                attempt,
                bytes_read: 0,
                release,
                exchange_finder,
                span,
                finished: false,
            }
            .boxed(),
        )
    }

    fn finish_successfully(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ctx
            .listener()
            .response_body_end(&self.ctx, self.bytes_read);
        self.release_connection();
    }

    fn finish_with_error(&mut self, error: &WireError) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.span.in_scope(|| {
            tracing::debug!(
                call_id = self.ctx.call_id().as_u64(),
                attempt = self.attempt,
                error_kind = %error.kind(),
                error_message = %error.message(),
                bytes_read = self.bytes_read,
                "response body failed",
            );
        });
        self.ctx.listener().response_body_failed(&self.ctx, error);
        self.discard_connection();
    }

    fn finish_abandoned(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.discard_connection();
    }

    fn release_connection(&mut self) {
        if let Some(release) = self.release.take() {
            match release {
                ResponseLease::Http1 {
                    connection,
                    bindings,
                    sender,
                    reusable,
                } => {
                    if reusable
                        && bindings.release_http1(connection.id(), sender)
                        && self.exchange_finder.release(&connection)
                    {
                        self.ctx
                            .listener()
                            .connection_released(&self.ctx, connection.id());
                        return;
                    }

                    bindings.remove(connection.id());
                    let _ = self.exchange_finder.pool().remove(connection.id());
                    self.ctx
                        .listener()
                        .connection_released(&self.ctx, connection.id());
                }
                ResponseLease::Http2 { connection } => {
                    let _ = self.exchange_finder.release(&connection);
                    self.ctx
                        .listener()
                        .connection_released(&self.ctx, connection.id());
                }
            }
        }
    }

    fn discard_connection(&mut self) {
        if let Some(release) = self.release.take() {
            match release {
                ResponseLease::Http1 {
                    connection,
                    bindings,
                    ..
                } => {
                    bindings.remove(connection.id());
                    let _ = self.exchange_finder.pool().remove(connection.id());
                    self.ctx
                        .listener()
                        .connection_released(&self.ctx, connection.id());
                }
                ResponseLease::Http2 { connection } => {
                    connection.mark_unhealthy();
                    let _ = self.exchange_finder.release(&connection);
                    self.ctx
                        .listener()
                        .connection_released(&self.ctx, connection.id());
                }
            }
        }
    }
}

impl Drop for ObservedIncomingBody {
    fn drop(&mut self) {
        self.finish_abandoned();
    }
}

impl Body for ObservedIncomingBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes_read += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                let error = WireError::from(error);
                this.finish_with_error(&error);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish_successfully();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

async fn send_bound_request(
    request: Request<RequestBody>,
    selected: SelectedConnection,
    exchange_finder: Arc<ExchangeFinder>,
    bindings: Arc<ConnectionBindings>,
) -> Result<BoundResponse, WireError> {
    let connection = selected.connection;
    let reused = selected.reused;
    let request = prepare_bound_request(request, connection.protocol(), connection.route().kind())?;

    match selected.binding {
        AcquiredBinding::Http1 { info, mut sender } => {
            sender.ready().await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings);
                map_hyper_error(error)
            })?;
            let mut response = sender.send_request(request).await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings);
                map_hyper_error(error)
            })?;
            let reusable = http1_response_allows_reuse(&response);
            response.extensions_mut().insert(info);
            Ok(BoundResponse {
                response,
                release: ResponseLease::Http1 {
                    connection,
                    bindings,
                    sender,
                    reusable,
                },
                reused,
            })
        }
        AcquiredBinding::Http2 { info, mut sender } => {
            sender.ready().await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings);
                map_hyper_error(error)
            })?;
            let mut response = sender.send_request(request).await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings);
                map_hyper_error(error)
            })?;
            response.extensions_mut().insert(info);
            Ok(BoundResponse {
                response,
                release: ResponseLease::Http2 { connection },
                reused,
            })
        }
    }
}

async fn bind_http1(
    stream: BoxConnection,
) -> Result<
    (
        http1::SendRequest<RequestBody>,
        http1::Connection<BoxConnection, RequestBody>,
    ),
    WireError,
> {
    http1::Builder::new()
        .handshake(stream)
        .await
        .map_err(|error| WireError::protocol("HTTP/1.1 client handshake failed", error))
}

async fn bind_http2(
    stream: BoxConnection,
    config: &crate::client::TransportConfig,
) -> Result<
    (
        http2::SendRequest<RequestBody>,
        http2::Connection<BoxConnection, RequestBody, TokioExecutor>,
    ),
    WireError,
> {
    let mut builder = http2::Builder::new(TokioExecutor::new());
    builder.timer(TokioTimer::new());
    if let Some(interval) = config.http2_keep_alive_interval {
        builder.keep_alive_interval(interval);
        builder.keep_alive_while_idle(config.http2_keep_alive_while_idle);
    }
    builder
        .handshake(stream)
        .await
        .map_err(|error| WireError::protocol("HTTP/2 client handshake failed", error))
}

fn determine_protocol(address: &Address, connected: &Connected) -> ConnectionProtocol {
    if address.scheme() == UriScheme::Http {
        ConnectionProtocol::Http1
    } else if connected.is_negotiated_h2() {
        ConnectionProtocol::Http2
    } else {
        ConnectionProtocol::Http1
    }
}

fn prepare_bound_request(
    mut request: Request<RequestBody>,
    protocol: ConnectionProtocol,
    route_kind: &RouteKind,
) -> Result<Request<RequestBody>, WireError> {
    if protocol != ConnectionProtocol::Http1
        || matches!(route_kind, RouteKind::HttpForwardProxy { .. })
    {
        return Ok(request);
    }

    let origin_form = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    *request.uri_mut() = origin_form.parse().map_err(|error| {
        WireError::internal(
            "failed to normalize request URI for direct HTTP/1.1 binding",
            error,
        )
    })?;
    Ok(request)
}

fn http1_response_allows_reuse(response: &Response<Incoming>) -> bool {
    if response.version() == Version::HTTP_10 {
        return false;
    }

    !response.headers().get("connection").is_some_and(|value| {
        value
            .to_str()
            .map(|value| value.eq_ignore_ascii_case("close"))
            .unwrap_or(false)
    })
}

fn cleanup_failed_request(
    connection: &crate::connection::RealConnection,
    exchange_finder: &Arc<ExchangeFinder>,
    bindings: &Arc<ConnectionBindings>,
) {
    match connection.protocol() {
        ConnectionProtocol::Http1 => {
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
        }
        ConnectionProtocol::Http2 => {
            connection.mark_unhealthy();
            let _ = exchange_finder.release(connection);
        }
    }
}

fn map_hyper_error(error: hyper::Error) -> WireError {
    if let Some(source) = find_wire_error(&error) {
        return source.clone();
    }

    if error.is_canceled() {
        return WireError::canceled("request canceled");
    }
    if error.is_timeout() {
        return WireError::timeout("request timed out");
    }

    WireError::protocol("transport request failed", error)
}

fn find_wire_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a WireError> {
    let mut current = Some(error);
    while let Some(source) = current {
        if let Some(wire_error) = source.downcast_ref::<WireError>() {
            return Some(wire_error);
        }
        current = source.source();
    }
    None
}

fn connection_info_from_connected(connected: &Connected) -> ConnectionInfo {
    let mut extensions = http::Extensions::new();
    connected.get_extras(&mut extensions);
    extensions
        .remove::<ConnectionInfo>()
        .unwrap_or(ConnectionInfo {
            id: next_connection_id(),
            remote_addr: None,
            local_addr: None,
            tls: false,
        })
}
