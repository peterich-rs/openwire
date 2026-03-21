use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::header::HOST;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Version};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client as HyperClient;
use openwire_core::{
    next_connection_id, BoxConnection, BoxFuture, CallContext, ConnectionInfo, DnsResolver,
    Exchange, RequestBody, ResponseBody, TcpConnector, TlsConnector, TokioExecutor, TokioIo,
    TokioTimer, WireError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::Service;
use tracing::Instrument;

use crate::auth::{
    AuthAttemptState, AuthContext, AuthKind, AuthRequestState, AuthResponseState,
    SharedAuthenticator,
};
use crate::connection::{Address, ConnectionProtocol, ExchangeFinder, RouteKind, RoutePlanner};
use crate::proxy::Proxy;
use crate::trace::PolicyTraceContext;

tokio::task_local! {
    static CURRENT_CALL_CONTEXT: CallContext;
}

tokio::task_local! {
    static CURRENT_REQUEST_ADDRESS: Address;
}

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

impl Service<Uri> for ConnectorStack {
    type Response = BoxConnection;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let dns_resolver = self.dns_resolver.clone();
        let tcp_connector = self.tcp_connector.clone();
        let tls_connector = self.tls_connector.clone();
        let connect_timeout = self.connect_timeout;
        let proxies = self.proxies.clone();
        let route_planner = self.route_planner.clone();
        let proxy_connect_deps = ProxyConnectDeps {
            dns_resolver,
            tcp_connector,
            tls_connector,
            proxy_authenticator: self.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.max_proxy_auth_attempts,
            connect_timeout,
        };

        Box::pin(async move {
            let ctx = current_call_context()?;
            let proxy = proxies.iter().find(|proxy| proxy.matches(&uri));
            let address = current_request_address()
                .map(Ok)
                .unwrap_or_else(|| Address::from_uri(&uri, proxy))?;

            if let Some(proxy) = proxy {
                if proxy.intercepts_http()
                    && uri
                        .scheme_str()
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
                {
                    return connect_via_http_forward_proxy(
                        ctx,
                        address,
                        route_planner.clone(),
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
                        route_planner,
                        proxy_connect_deps,
                    )
                    .await;
                }
            }

            connect_direct(
                ctx,
                uri,
                address,
                route_planner,
                proxy_connect_deps.dns_resolver,
                proxy_connect_deps.tcp_connector,
                proxy_connect_deps.tls_connector,
                proxy_connect_deps.connect_timeout,
            )
            .await
        })
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
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a scheme"))?;
    let host = address.authority().host().to_owned();
    let port = address.authority().port();
    let addrs = dns_resolver.resolve(ctx.clone(), host, port).await?;
    let route_plan = route_planner.plan_direct(address, addrs);
    let mut last_error = None;

    for route in route_plan.iter().cloned() {
        let &RouteKind::Direct { target: addr } = route.kind() else {
            continue;
        };
        match tcp_connector
            .connect(ctx.clone(), addr, connect_timeout)
            .await
        {
            Ok(stream) => {
                if scheme.eq_ignore_ascii_case("https") {
                    let tls_connector = tls_connector.clone().ok_or_else(|| {
                        WireError::tls(
                            "HTTPS requested but no TLS connector is configured",
                            io::Error::new(io::ErrorKind::Unsupported, "missing TLS connector"),
                        )
                    })?;
                    return tls_connector.connect(ctx.clone(), uri, stream).await;
                }
                return Ok(stream);
            }
            Err(error) => {
                ctx.listener().connect_failed(&ctx, addr, &error);
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        WireError::connect(
            "no socket addresses could be connected",
            io::Error::new(io::ErrorKind::NotConnected, "no address connected"),
        )
    }))
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

#[derive(Clone)]
pub(crate) struct TransportService {
    pub(crate) client: HyperClient<ConnectorStack, RequestBody>,
    exchange_finder: Arc<ExchangeFinder>,
}

impl TransportService {
    pub(crate) fn new(
        client: HyperClient<ConnectorStack, RequestBody>,
        exchange_finder: Arc<ExchangeFinder>,
    ) -> Self {
        Self {
            client,
            exchange_finder,
        }
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
        let client = self.client.clone();
        let exchange_finder = self.exchange_finder.clone();
        Box::pin(async move {
            let (request, ctx, attempt) = exchange.into_parts();
            let prepared = exchange_finder.prepare(&request)?;
            let request_body_len = request.body().replayable_len();
            let policy_trace = request
                .extensions()
                .get::<PolicyTraceContext>()
                .copied()
                .unwrap_or_default();

            let call_span = attempt_span(&ctx, &request, attempt, policy_trace);

            async move {
                let request_address = prepared.address().clone();
                let result = with_call_context(
                    ctx.clone(),
                    with_request_address(request_address, async move {
                        client.request(request).await.map_err(map_client_error)
                    }),
                )
                .await;
                let response = match result {
                    Ok(response) => response,
                    Err(error) => {
                        exchange_finder.discard_prepared(&prepared);
                        return Err(error);
                    }
                };

                if let Some(bytes) = request_body_len {
                    ctx.listener().request_body_end(&ctx, bytes);
                }

                let connection_info = response.extensions().get::<ConnectionInfo>().cloned();
                let mut observed_connection = None;
                if let Some(info) = &connection_info {
                    let protocol = if response.version() == Version::HTTP_2 {
                        ConnectionProtocol::Http2
                    } else {
                        ConnectionProtocol::Http1
                    };
                    let observed = exchange_finder.observe_connection(&prepared, info, protocol);
                    let reused = observed.reused();
                    observed_connection = Some(observed.connection().clone());
                    ctx.listener().connection_acquired(&ctx, info.id, reused);
                    let span = tracing::Span::current();
                    span.record("connection_id", info.id.as_u64());
                    span.record("connection_reused", reused);
                } else {
                    exchange_finder.discard_prepared(&prepared);
                }

                ctx.listener().response_headers_start(&ctx);
                let (parts, body) = response.into_parts();
                let body = ObservedIncomingBody::wrap(
                    body,
                    ctx.clone(),
                    attempt,
                    observed_connection,
                    exchange_finder.clone(),
                    tracing::Span::current(),
                );
                let response = Response::from_parts(parts, body);
                ctx.listener().response_headers_end(&ctx, &response);

                Ok(response)
            }
            .instrument(call_span)
            .await
        })
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
        connection_id = tracing::field::Empty,
        connection_reused = tracing::field::Empty,
    )
}

pub(crate) fn build_hyper_client(
    connector: ConnectorStack,
    config: &crate::client::TransportConfig,
) -> HyperClient<ConnectorStack, RequestBody> {
    let mut builder = HyperClient::builder(TokioExecutor::new());
    builder.timer(TokioTimer::new());
    builder.pool_timer(TokioTimer::new());
    builder.pool_max_idle_per_host(config.pool_max_idle_per_host);
    builder.retry_canceled_requests(config.retry_canceled_requests);
    builder.pool_idle_timeout(config.pool_idle_timeout);
    if let Some(interval) = config.http2_keep_alive_interval {
        builder.http2_keep_alive_interval(interval);
        builder.http2_keep_alive_while_idle(config.http2_keep_alive_while_idle);
    }
    builder.build(connector)
}

struct ObservedIncomingBody {
    inner: Incoming,
    ctx: CallContext,
    attempt: u32,
    bytes_read: u64,
    connection: Option<crate::connection::RealConnection>,
    exchange_finder: Arc<ExchangeFinder>,
    span: tracing::Span,
    finished: bool,
}

impl ObservedIncomingBody {
    fn wrap(
        body: Incoming,
        ctx: CallContext,
        attempt: u32,
        connection: Option<crate::connection::RealConnection>,
        exchange_finder: Arc<ExchangeFinder>,
        span: tracing::Span,
    ) -> ResponseBody {
        ResponseBody::new(
            Self {
                inner: body,
                ctx,
                attempt,
                bytes_read: 0,
                connection,
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
        self.release_connection();
    }

    fn finish_abandoned(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.release_connection();
    }

    fn release_connection(&mut self) {
        if let Some(connection) = &self.connection {
            let _ = self.exchange_finder.release(connection);
            self.ctx
                .listener()
                .connection_released(&self.ctx, connection.id());
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

fn map_client_error(error: hyper_util::client::legacy::Error) -> WireError {
    if error.is_connect() {
        if let Some(source) = find_wire_error(&error) {
            return source.clone();
        }
        return WireError::connect("transport connection failed", error);
    }

    if let Some(source) = std::error::Error::source(&error) {
        if let Some(source) = source.downcast_ref::<hyper::Error>() {
            if source.is_canceled() {
                return WireError::canceled("request canceled");
            }
            if source.is_timeout() {
                return WireError::timeout("request timed out");
            }
        }
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

fn current_call_context() -> Result<CallContext, WireError> {
    CURRENT_CALL_CONTEXT
        .try_with(Clone::clone)
        .map_err(|error| {
            WireError::internal(
                "call context unavailable inside connector stack",
                io::Error::other(error.to_string()),
            )
        })
}

pub(crate) async fn with_call_context<F, T>(ctx: CallContext, future: F) -> T
where
    F: Future<Output = T>,
{
    CURRENT_CALL_CONTEXT.scope(ctx, future).await
}

async fn with_request_address<F, T>(address: Address, future: F) -> T
where
    F: Future<Output = T>,
{
    CURRENT_REQUEST_ADDRESS.scope(address, future).await
}

fn current_request_address() -> Option<Address> {
    CURRENT_REQUEST_ADDRESS.try_with(Clone::clone).ok()
}
