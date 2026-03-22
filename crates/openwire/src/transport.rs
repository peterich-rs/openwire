use std::cmp;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use futures_util::future::{select, Either};
use futures_util::io::{
    AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt,
};
use futures_util::task::AtomicWaker;
use http::header::{CONNECTION, HOST};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Version};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper::rt::Timer;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use openwire_core::{
    next_connection_id, BoxConnection, BoxFuture, BoxTaskHandle, CallContext, CoalescingInfo,
    ConnectionId, ConnectionInfo, DnsResolver, Exchange, HyperExecutor, RequestBody, ResponseBody,
    SharedTimer, TcpConnector, TlsConnector, WireError, WireExecutor,
};
use pin_project_lite::pin_project;
use tower::Service;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::auth::{
    AuthAttemptState, AuthContext, AuthKind, AuthRequestState, AuthResponseState,
    SharedAuthenticator,
};
use crate::connection::{
    Address, ConnectionAvailability, ConnectionLimiter, ConnectionPermit, ConnectionProtocol,
    ExchangeFinder, FastFallbackDialer, FastFallbackOutcome, RequestAdmissionLimiter, Route,
    RouteKind, RoutePlan, RoutePlanner, UriScheme,
};
use crate::proxy::ProxyCredentials;
use crate::sync_util::lock_mutex;
use crate::trace::PolicyTraceContext;

#[derive(Clone)]
pub(crate) struct ConnectorStack {
    pub(crate) dns_resolver: Arc<dyn DnsResolver>,
    pub(crate) tcp_connector: Arc<dyn TcpConnector>,
    pub(crate) tls_connector: Option<Arc<dyn TlsConnector>>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) executor: Arc<dyn WireExecutor>,
    pub(crate) timer: SharedTimer,
    pub(crate) route_planner: RoutePlanner,
    pub(crate) proxy_authenticator: Option<SharedAuthenticator>,
    pub(crate) max_proxy_auth_attempts: usize,
}

#[derive(Clone)]
struct ProxyConnectDeps {
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    proxy_authenticator: Option<SharedAuthenticator>,
    max_proxy_auth_attempts: usize,
    connect_timeout: Option<Duration>,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
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
    timer: SharedTimer,
}

pin_project! {
    struct IoCompat<T> {
        #[pin]
        inner: T,
    }
}

impl<T> IoCompat<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }

    fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> futures_util::io::AsyncRead for IoCompat<T>
where
    T: hyper::rt::Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut read_buf = hyper::rt::ReadBuf::new(buf);
        match hyper::rt::Read::poll_read(self.project().inner, cx, read_buf.unfilled()) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> futures_util::io::AsyncWrite for IoCompat<T>
where
    T: hyper::rt::Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        hyper::rt::Write::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        hyper::rt::Write::poll_flush(self.project().inner, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        hyper::rt::Write::poll_shutdown(self.project().inner, cx)
    }
}

impl ConnectorStack {
    async fn route_plan(
        &self,
        ctx: CallContext,
        address: &Address,
    ) -> Result<RoutePlan, WireError> {
        let (dns_host, dns_port) = self.route_planner.dns_target(address);
        let resolved_addrs = self
            .dns_resolver
            .resolve(ctx.clone(), dns_host.to_owned(), dns_port)
            .await?;
        Ok(self.route_planner.plan(address.clone(), resolved_addrs))
    }

    fn proxy_connect_deps(&self) -> ProxyConnectDeps {
        ProxyConnectDeps {
            tcp_connector: self.tcp_connector.clone(),
            tls_connector: self.tls_connector.clone(),
            proxy_authenticator: self.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.max_proxy_auth_attempts,
            connect_timeout: self.connect_timeout,
            executor: self.executor.clone(),
            timer: self.timer.clone(),
        }
    }
}

async fn connect_route_plan(
    ctx: CallContext,
    uri: Uri,
    route_plan: RoutePlan,
    deps: ProxyConnectDeps,
) -> Result<BoxConnection, WireError> {
    let Some(first_route) = route_plan.route(0) else {
        return Err(WireError::route_exhausted("route plan produced no routes"));
    };

    match first_route.kind() {
        RouteKind::Direct { .. } => {
            connect_direct(
                ctx,
                uri,
                route_plan,
                deps.executor,
                deps.timer,
                deps.tcp_connector,
                deps.tls_connector,
                deps.connect_timeout,
            )
            .await
        }
        RouteKind::HttpForwardProxy { .. } => {
            connect_via_http_forward_proxy(
                ctx,
                uri,
                route_plan,
                deps.executor,
                deps.timer,
                deps.tcp_connector,
                deps.connect_timeout,
            )
            .await
        }
        RouteKind::ConnectProxy { .. } => connect_via_http_proxy(ctx, uri, route_plan, deps).await,
        RouteKind::SocksProxy { .. } => connect_via_socks_proxy(ctx, uri, route_plan, deps).await,
    }
}

async fn connect_direct(
    ctx: CallContext,
    uri: Uri,
    route_plan: RoutePlan,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    connect_timeout: Option<Duration>,
) -> Result<BoxConnection, WireError> {
    let connect_span = tracing::Span::current();
    let (stream, outcome) = FastFallbackDialer
        .dial_direct(
            ctx.clone(),
            uri,
            route_plan,
            executor,
            timer,
            tcp_connector,
            tls_connector,
            connect_timeout,
        )
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
    let span = tracing::Span::current();
    record_fast_fallback_trace(&span, outcome);
    Ok(stream)
}

fn record_fast_fallback_trace(span: &tracing::Span, outcome: FastFallbackOutcome) {
    span.record("route_count", outcome.route_count as u64);
    span.record("fast_fallback_enabled", outcome.fast_fallback_enabled);
    span.record("connect_race_id", outcome.race_id);
    span.record("connect_winner", outcome.winner_index as u64);
}

async fn connect_via_http_forward_proxy(
    ctx: CallContext,
    target_uri: Uri,
    route_plan: RoutePlan,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    tcp_connector: Arc<dyn TcpConnector>,
    connect_timeout: Option<Duration>,
) -> Result<BoxConnection, WireError> {
    let connect_span = tracing::Span::current();
    let (stream, outcome) = FastFallbackDialer
        .dial_route_plan(
            ctx,
            target_uri,
            route_plan,
            executor,
            timer,
            move |ctx, route| {
                let tcp_connector = tcp_connector.clone();
                async move {
                    match route.kind() {
                        RouteKind::HttpForwardProxy { proxy, .. } => {
                            tcp_connector.connect(ctx, *proxy, connect_timeout).await
                        }
                        other => Err(WireError::internal(
                            format!(
                                "unexpected non-forward-proxy route in proxy dialer: {other:?}"
                            ),
                            io::Error::other("unexpected non-forward-proxy route"),
                        )),
                    }
                }
            },
            |_ctx, _uri, _route, stream| async move {
                Ok(Box::new(ProxiedConnection { inner: stream }) as BoxConnection)
            },
        )
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
    let span = tracing::Span::current();
    record_fast_fallback_trace(&span, outcome);
    Ok(stream)
}

async fn connect_via_http_proxy(
    ctx: CallContext,
    target_uri: Uri,
    route_plan: RoutePlan,
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
    let connect_span = tracing::Span::current();
    let (stream, outcome) = FastFallbackDialer
        .dial_route_plan(
            ctx,
            target_uri,
            route_plan,
            deps.executor.clone(),
            deps.timer.clone(),
            {
                let deps = deps.clone();
                move |ctx, route| {
                    let deps = deps.clone();
                    async move {
                        match route.kind() {
                            RouteKind::ConnectProxy { proxy, .. } => {
                                deps.tcp_connector
                                    .connect(ctx, *proxy, deps.connect_timeout)
                                    .await
                            }
                            other => Err(WireError::internal(
                                format!(
                                    "unexpected non-CONNECT-proxy route in proxy dialer: {other:?}"
                                ),
                                io::Error::other("unexpected non-CONNECT-proxy route"),
                            )),
                        }
                    }
                }
            },
            {
                let deps = deps.clone();
                move |ctx, target_uri, route, stream| {
                    let deps = deps.clone();
                    async move {
                        let proxy_addr = match route.kind() {
                            RouteKind::ConnectProxy { proxy, .. } => *proxy,
                            other => {
                                return Err(WireError::internal(
                                    format!(
                                        "unexpected non-CONNECT-proxy route in tunnel finalizer: {other:?}"
                                    ),
                                    io::Error::other("unexpected non-CONNECT-proxy route"),
                                ));
                            }
                        };
                        let tunneled = establish_connect_tunnel(ConnectTunnelParams {
                            ctx: ctx.clone(),
                            proxy_addr,
                            target_uri: &target_uri,
                            stream,
                            tcp_connector: deps.tcp_connector.clone(),
                            proxy_authenticator: deps.proxy_authenticator.clone(),
                            max_proxy_auth_attempts: deps.max_proxy_auth_attempts,
                            connect_timeout: deps.connect_timeout,
                            timer: deps.timer.clone(),
                        })
                        .await?;
                        connect_target_tls_if_needed(
                            ctx,
                            target_uri,
                            tunneled,
                            deps.tls_connector.clone(),
                        )
                        .await
                    }
                }
            },
        )
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
    let span = tracing::Span::current();
    record_fast_fallback_trace(&span, outcome);
    Ok(stream)
}

async fn connect_via_socks_proxy(
    ctx: CallContext,
    target_uri: Uri,
    route_plan: RoutePlan,
    deps: ProxyConnectDeps,
) -> Result<BoxConnection, WireError> {
    let connect_span = tracing::Span::current();
    let (stream, outcome) = FastFallbackDialer
        .dial_route_plan(
            ctx,
            target_uri,
            route_plan,
            deps.executor.clone(),
            deps.timer.clone(),
            {
                let deps = deps.clone();
                move |ctx, route| {
                    let deps = deps.clone();
                    async move {
                        match route.kind() {
                            RouteKind::SocksProxy { proxy, .. } => {
                                deps.tcp_connector
                                    .connect(ctx, *proxy, deps.connect_timeout)
                                    .await
                            }
                            other => Err(WireError::internal(
                                format!(
                                    "unexpected non-SOCKS-proxy route in proxy dialer: {other:?}"
                                ),
                                io::Error::other("unexpected non-SOCKS-proxy route"),
                            )),
                        }
                    }
                }
            },
            {
                let deps = deps.clone();
                move |ctx, target_uri, route, stream| {
                    let deps = deps.clone();
                    async move {
                        let (proxy_addr, credentials) = match route.kind() {
                            RouteKind::SocksProxy { proxy, credentials } => {
                                (*proxy, credentials.clone())
                            }
                            other => {
                                return Err(WireError::internal(
                                    format!(
                                        "unexpected non-SOCKS-proxy route in tunnel finalizer: {other:?}"
                                    ),
                                    io::Error::other("unexpected non-SOCKS-proxy route"),
                                ));
                            }
                        };
                        let tunneled = establish_socks5_tunnel(
                            ctx.clone(),
                            &target_uri,
                            proxy_addr,
                            stream,
                            deps.connect_timeout,
                            deps.timer.clone(),
                            credentials.as_ref(),
                        )
                        .await?;
                        let proxied = Box::new(ProxiedConnection { inner: tunneled })
                            as BoxConnection;
                        connect_target_tls_if_needed(
                            ctx,
                            target_uri,
                            proxied,
                            deps.tls_connector.clone(),
                        )
                        .await
                    }
                }
            },
        )
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
    let span = tracing::Span::current();
    record_fast_fallback_trace(&span, outcome);
    Ok(stream)
}

async fn connect_target_tls_if_needed(
    ctx: CallContext,
    target_uri: Uri,
    stream: BoxConnection,
    tls_connector: Option<Arc<dyn TlsConnector>>,
) -> Result<BoxConnection, WireError> {
    if !target_uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
    {
        return Ok(stream);
    }

    let tls_connector = tls_connector.ok_or_else(|| {
        WireError::tls(
            "HTTPS requested but no TLS connector is configured",
            io::Error::new(io::ErrorKind::Unsupported, "missing TLS connector"),
        )
    })?;
    tls_connector.connect(ctx, target_uri, stream).await
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
        match send_connect_request(
            &authority,
            &connect_headers,
            stream,
            &params.timer,
            params.connect_timeout,
        )
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
                        http::Extensions::new(),
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

async fn establish_socks5_tunnel(
    ctx: CallContext,
    target_uri: &Uri,
    proxy_addr: SocketAddr,
    stream: BoxConnection,
    timeout: Option<Duration>,
    timer: SharedTimer,
    credentials: Option<&ProxyCredentials>,
) -> Result<BoxConnection, WireError> {
    let host = target_uri
        .host()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a host"))?;
    let port = target_uri.port_u16().unwrap_or_else(|| {
        if target_uri.scheme_str() == Some("https") {
            443
        } else {
            80
        }
    });
    let mut stream = IoCompat::new(stream);

    socks5_write_client_greeting(&mut stream, &timer, timeout, credentials.is_some()).await?;
    match socks5_read_server_choice(&mut stream, &timer, timeout, credentials.is_some()).await? {
        Socks5AuthMethod::NoAuth => {}
        Socks5AuthMethod::UsernamePassword => {
            let credentials = credentials.ok_or_else(|| {
                WireError::proxy_tunnel_non_retryable(
                    "SOCKS5 proxy requested username/password authentication but no credentials were configured",
                )
            })?;
            socks5_write_password_auth_request(&mut stream, &timer, timeout, credentials).await?;
            socks5_read_password_auth_response(&mut stream, &timer, timeout).await?;
        }
    }
    socks5_write_connect_request(&mut stream, &timer, host, port, timeout).await?;
    socks5_read_connect_response(&mut stream, &timer, timeout).await?;
    tracing::debug!(
        call_id = ctx.call_id().as_u64(),
        proxy_addr = %proxy_addr,
        target_host = host,
        target_port = port,
        "established SOCKS5 tunnel"
    );
    Ok(stream.into_inner())
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
    prefetched: Bytes,
}

struct PrefetchedTunnelBytes {
    inner: BoxConnection,
    prefetched: Option<Bytes>,
}

impl PrefetchedTunnelBytes {
    fn new(inner: BoxConnection, prefetched: Bytes) -> Self {
        Self {
            inner,
            prefetched: (!prefetched.is_empty()).then_some(prefetched),
        }
    }
}

impl Connection for PrefetchedTunnelBytes {
    fn connected(&self) -> Connected {
        self.inner.connected()
    }
}

impl hyper::rt::Read for PrefetchedTunnelBytes {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        if let Some(mut prefetched) = this.prefetched.take() {
            let copy_len = cmp::min(prefetched.len(), buf.remaining());
            buf.put_slice(&prefetched[..copy_len]);
            prefetched.advance(copy_len);
            if !prefetched.is_empty() {
                this.prefetched = Some(prefetched);
            }
            return Poll::Ready(Ok(()));
        }

        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for PrefetchedTunnelBytes {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }
}

async fn send_connect_request(
    authority: &str,
    headers: &HeaderMap,
    stream: BoxConnection,
    timer: &SharedTimer,
    connect_timeout: Option<Duration>,
) -> Result<ConnectTunnelOutcome, WireError> {
    let connect_request = build_connect_request(authority, headers)?;

    let mut stream = IoCompat::new(stream);
    stream
        .write_all(connect_request.as_bytes())
        .await
        .map_err(|error| WireError::proxy_tunnel("failed to write proxy CONNECT request", error))?;
    stream
        .flush()
        .await
        .map_err(|error| WireError::proxy_tunnel("failed to flush proxy CONNECT request", error))?;

    let head = read_connect_response(&mut stream, timer, connect_timeout).await?;
    if head.status == StatusCode::OK {
        let stream = PrefetchedTunnelBytes::new(stream.into_inner(), head.prefetched);
        return Ok(ConnectTunnelOutcome::Established(
            Box::new(stream) as BoxConnection
        ));
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
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    connect_timeout: Option<Duration>,
) -> Result<ConnectResponseHead, WireError> {
    if let Some(timeout) = connect_timeout {
        return timeout_future(
            timer,
            timeout,
            read_connect_response_inner(stream),
            |timeout| {
                WireError::connect_timeout(format!("proxy CONNECT timed out after {timeout:?}"))
            },
        )
        .await;
    }

    read_connect_response_inner(stream).await
}

async fn read_connect_response_inner(
    stream: &mut IoCompat<BoxConnection>,
) -> Result<ConnectResponseHead, WireError> {
    let mut response = Vec::with_capacity(1024);
    let mut buf = [0u8; 512];
    let head_end = loop {
        let read = stream.read(&mut buf).await.map_err(|error| {
            WireError::proxy_tunnel("failed to read proxy CONNECT response", error)
        })?;
        if read == 0 {
            return Err(WireError::proxy_tunnel(
                "proxy closed connection during CONNECT",
                io::Error::new(io::ErrorKind::UnexpectedEof, "proxy closed connection"),
            ));
        }
        response.extend_from_slice(&buf[..read]);
        if let Some(split_at) = find_connect_response_end(&response) {
            break split_at;
        }
        if response.len() > 8192 {
            return Err(WireError::proxy_tunnel(
                "proxy CONNECT response headers exceeded 8KiB",
                io::Error::new(io::ErrorKind::InvalidData, "proxy response too large"),
            ));
        }
    };
    let (head, prefetched) = split_connect_response_head(&response, head_end)?;
    parse_connect_response_head(head, prefetched)
}

async fn socks5_write_client_greeting(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    timeout: Option<Duration>,
    allow_username_password: bool,
) -> Result<(), WireError> {
    let methods = if allow_username_password {
        &[0x00, 0x02][..]
    } else {
        &[0x00][..]
    };
    let mut greeting = Vec::with_capacity(2 + methods.len());
    greeting.push(0x05);
    greeting.push(methods.len() as u8);
    greeting.extend_from_slice(methods);
    socks5_timeout(timer, timeout, async {
        stream
            .write_all(&greeting)
            .await
            .map_err(|error| WireError::proxy_tunnel("failed to write SOCKS5 greeting", error))?;
        stream
            .flush()
            .await
            .map_err(|error| WireError::proxy_tunnel("failed to flush SOCKS5 greeting", error))
    })
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Socks5AuthMethod {
    NoAuth,
    UsernamePassword,
}

async fn socks5_read_server_choice(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    timeout: Option<Duration>,
    offered_username_password: bool,
) -> Result<Socks5AuthMethod, WireError> {
    let mut response = [0u8; 2];
    socks5_timeout(timer, timeout, async {
        stream.read_exact(&mut response).await.map_err(|error| {
            WireError::proxy_tunnel("failed to read SOCKS5 greeting response", error)
        })
    })
    .await?;

    if response[0] != 0x05 {
        return Err(WireError::proxy_tunnel_non_retryable(format!(
            "SOCKS5 proxy returned unsupported version {}",
            response[0]
        )));
    }
    match response[1] {
        0x00 => Ok(Socks5AuthMethod::NoAuth),
        0x02 => Ok(Socks5AuthMethod::UsernamePassword),
        0xff if offered_username_password => Err(WireError::proxy_tunnel_non_retryable(
            "SOCKS5 proxy did not accept no-auth or username/password authentication",
        )),
        0xff => Err(WireError::proxy_tunnel_non_retryable(
            "SOCKS5 proxy does not support no-auth authentication",
        )),
        method => Err(WireError::proxy_tunnel_non_retryable(format!(
            "SOCKS5 proxy selected unsupported authentication method {method:#04x}",
        ))),
    }
}

async fn socks5_write_password_auth_request(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    timeout: Option<Duration>,
    credentials: &ProxyCredentials,
) -> Result<(), WireError> {
    if credentials.username().len() > u8::MAX as usize {
        return Err(WireError::invalid_request(
            "SOCKS5 proxy username exceeds 255 bytes",
        ));
    }
    if credentials.password().len() > u8::MAX as usize {
        return Err(WireError::invalid_request(
            "SOCKS5 proxy password exceeds 255 bytes",
        ));
    }

    let username = credentials.username().as_bytes();
    let password = credentials.password().as_bytes();
    let mut request = Vec::with_capacity(3 + username.len() + password.len());
    request.push(0x01);
    request.push(username.len() as u8);
    request.extend_from_slice(username);
    request.push(password.len() as u8);
    request.extend_from_slice(password);

    socks5_timeout(timer, timeout, async {
        stream.write_all(&request).await.map_err(|error| {
            WireError::proxy_tunnel("failed to write SOCKS5 username/password request", error)
        })?;
        stream.flush().await.map_err(|error| {
            WireError::proxy_tunnel("failed to flush SOCKS5 username/password request", error)
        })
    })
    .await
}

async fn socks5_read_password_auth_response(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    timeout: Option<Duration>,
) -> Result<(), WireError> {
    let mut response = [0u8; 2];
    socks5_timeout(timer, timeout, async {
        stream.read_exact(&mut response).await.map_err(|error| {
            WireError::proxy_tunnel("failed to read SOCKS5 username/password response", error)
        })
    })
    .await?;

    if response[0] != 0x01 {
        return Err(WireError::proxy_tunnel_non_retryable(format!(
            "SOCKS5 username/password auth returned unsupported version {}",
            response[0]
        )));
    }
    if response[1] != 0x00 {
        return Err(WireError::proxy_tunnel_non_retryable(
            "SOCKS5 username/password authentication failed",
        ));
    }

    Ok(())
}

async fn socks5_write_connect_request(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    host: &str,
    port: u16,
    timeout: Option<Duration>,
) -> Result<(), WireError> {
    let mut request = vec![0x05, 0x01, 0x00];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > u8::MAX as usize {
                return Err(WireError::invalid_request(
                    "SOCKS5 target host exceeds 255 bytes",
                ));
            }
            request.push(0x03);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());

    socks5_timeout(timer, timeout, async {
        stream.write_all(&request).await.map_err(|error| {
            WireError::proxy_tunnel("failed to write SOCKS5 connect request", error)
        })?;
        stream.flush().await.map_err(|error| {
            WireError::proxy_tunnel("failed to flush SOCKS5 connect request", error)
        })
    })
    .await
}

async fn socks5_read_connect_response(
    stream: &mut IoCompat<BoxConnection>,
    timer: &SharedTimer,
    timeout: Option<Duration>,
) -> Result<(), WireError> {
    let mut head = [0u8; 4];
    socks5_timeout(timer, timeout, async {
        stream.read_exact(&mut head).await.map_err(|error| {
            WireError::proxy_tunnel("failed to read SOCKS5 connect response", error)
        })
    })
    .await?;

    if head[0] != 0x05 {
        return Err(WireError::proxy_tunnel_non_retryable(format!(
            "SOCKS5 proxy returned unsupported version {}",
            head[0]
        )));
    }
    if head[1] != 0x00 {
        return Err(WireError::proxy_tunnel_non_retryable(format!(
            "SOCKS5 connect failed: {}",
            socks5_reply_reason(head[1])
        )));
    }

    match head[3] {
        0x01 => {
            let mut remainder = [0u8; 6];
            socks5_timeout(timer, timeout, async {
                stream.read_exact(&mut remainder).await.map_err(|error| {
                    WireError::proxy_tunnel("failed to read SOCKS5 IPv4 bind address", error)
                })
            })
            .await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            socks5_timeout(timer, timeout, async {
                stream.read_exact(&mut len).await.map_err(|error| {
                    WireError::proxy_tunnel("failed to read SOCKS5 bind host length", error)
                })
            })
            .await?;
            let mut remainder = vec![0u8; len[0] as usize + 2];
            socks5_timeout(timer, timeout, async {
                stream.read_exact(&mut remainder).await.map_err(|error| {
                    WireError::proxy_tunnel("failed to read SOCKS5 bind host", error)
                })
            })
            .await?;
        }
        0x04 => {
            let mut remainder = [0u8; 18];
            socks5_timeout(timer, timeout, async {
                stream.read_exact(&mut remainder).await.map_err(|error| {
                    WireError::proxy_tunnel("failed to read SOCKS5 IPv6 bind address", error)
                })
            })
            .await?;
        }
        atyp => {
            return Err(WireError::proxy_tunnel_non_retryable(format!(
                "SOCKS5 proxy returned unknown address type {atyp}"
            )));
        }
    }

    Ok(())
}

async fn socks5_timeout<T>(
    timer: &SharedTimer,
    timeout: Option<Duration>,
    future: impl std::future::Future<Output = Result<T, WireError>>,
) -> Result<T, WireError> {
    if let Some(timeout) = timeout {
        return timeout_future(timer, timeout, future, |timeout| {
            WireError::connect_timeout(format!("SOCKS5 handshake timed out after {timeout:?}"))
        })
        .await;
    }

    future.await
}

async fn timeout_future<T, F>(
    timer: &SharedTimer,
    timeout: Duration,
    future: F,
    on_timeout: impl FnOnce(Duration) -> WireError,
) -> Result<T, WireError>
where
    F: std::future::Future<Output = Result<T, WireError>>,
{
    let future = Box::pin(future);
    let sleep = timer.sleep(timeout);
    futures_util::pin_mut!(sleep);
    match select(future, sleep).await {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => Err(on_timeout(timeout)),
    }
}

fn socks5_reply_reason(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown failure",
    }
}

fn find_connect_response_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn split_connect_response_head(
    response: &[u8],
    head_end: usize,
) -> Result<(&[u8], Bytes), WireError> {
    Ok((
        &response[..head_end],
        Bytes::copy_from_slice(&response[head_end..]),
    ))
}

fn parse_connect_response_head(
    head: &[u8],
    prefetched: Bytes,
) -> Result<ConnectResponseHead, WireError> {
    let head = std::str::from_utf8(head).map_err(|error| {
        WireError::proxy_tunnel("proxy CONNECT response was not valid UTF-8", error)
    })?;
    let head = head
        .split_once("\r\n\r\n")
        .map(|(head, _)| head)
        .unwrap_or(head);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| {
            WireError::proxy_tunnel(
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
            WireError::proxy_tunnel(
                format!("invalid proxy CONNECT header line: {line}"),
                io::Error::new(io::ErrorKind::InvalidData, "malformed proxy header"),
            )
        })?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|error| {
            WireError::proxy_tunnel(
                format!("invalid proxy CONNECT header name {name:?}"),
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;
        let value = HeaderValue::from_str(value.trim()).map_err(|error| {
            WireError::proxy_tunnel(
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
        prefetched,
    })
}

fn parse_connect_status(status_line: &str) -> Result<StatusCode, WireError> {
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next().unwrap_or_default();
    if protocol != "HTTP/1.1" && protocol != "HTTP/1.0" {
        return Err(WireError::proxy_tunnel(
            format!("proxy CONNECT returned an invalid status line: {status_line}"),
            io::Error::new(io::ErrorKind::InvalidData, "invalid proxy status line"),
        ));
    }

    let status = parts
        .next()
        .ok_or_else(|| {
            WireError::proxy_tunnel(
                format!("proxy CONNECT returned an invalid status line: {status_line}"),
                io::Error::new(io::ErrorKind::InvalidData, "missing proxy status code"),
            )
        })?
        .parse::<u16>()
        .map_err(|error| {
            WireError::proxy_tunnel(
                format!("proxy CONNECT returned an invalid status line: {status_line}"),
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;

    StatusCode::from_u16(status).map_err(|error| {
        WireError::proxy_tunnel(
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
    WireError::proxy_tunnel_non_retryable(format!("proxy CONNECT failed: {status_line}"))
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

const CONNECTION_BINDING_SHARDS: usize = 32;

#[derive(Clone)]
struct ConnectionBindings {
    shards: Arc<[Mutex<HashMap<ConnectionId, ConnectionBinding>>]>,
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

enum BindingAcquireResult {
    Acquired(AcquiredBinding),
    Busy,
    Stale,
}

impl ConnectionBindings {
    fn shard(
        &self,
        connection_id: ConnectionId,
    ) -> &Mutex<HashMap<ConnectionId, ConnectionBinding>> {
        &self.shards[(connection_id.as_u64() as usize) % self.shards.len()]
    }

    fn insert_http1(
        &self,
        connection_id: ConnectionId,
        info: ConnectionInfo,
        sender: http1::SendRequest<RequestBody>,
    ) {
        lock_mutex(self.shard(connection_id)).insert(
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
        lock_mutex(self.shard(connection_id)).insert(
            connection_id,
            ConnectionBinding::Http2(Http2Binding { info, sender }),
        );
    }

    fn acquire(&self, connection_id: ConnectionId) -> BindingAcquireResult {
        let mut bindings = lock_mutex(self.shard(connection_id));
        let mut remove_stale = false;
        let acquired = match bindings.get_mut(&connection_id) {
            Some(ConnectionBinding::Http1(binding)) => {
                let Some(sender) = binding.sender.take() else {
                    return BindingAcquireResult::Busy;
                };
                if sender.is_closed() {
                    remove_stale = true;
                    BindingAcquireResult::Stale
                } else {
                    BindingAcquireResult::Acquired(AcquiredBinding::Http1 {
                        info: binding.info.clone(),
                        sender,
                    })
                }
            }
            Some(ConnectionBinding::Http2(binding)) => {
                if binding.sender.is_closed() {
                    remove_stale = true;
                    BindingAcquireResult::Stale
                } else if !binding.sender.is_ready() {
                    BindingAcquireResult::Busy
                } else {
                    BindingAcquireResult::Acquired(AcquiredBinding::Http2 {
                        info: binding.info.clone(),
                        sender: binding.sender.clone(),
                    })
                }
            }
            None => BindingAcquireResult::Stale,
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

        let mut bindings = lock_mutex(self.shard(connection_id));
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
        lock_mutex(self.shard(connection_id)).remove(&connection_id);
    }
}

impl Default for ConnectionBindings {
    fn default() -> Self {
        let shards = (0..CONNECTION_BINDING_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect::<Vec<_>>();
        Self {
            shards: Arc::<[Mutex<HashMap<ConnectionId, ConnectionBinding>>]>::from(shards),
        }
    }
}

#[derive(Clone, Default)]
struct ConnectionTaskRegistry {
    inner: Arc<ConnectionTaskRegistryInner>,
}

#[derive(Default)]
struct ConnectionTaskRegistryInner {
    next_id: AtomicU64,
    handles: Mutex<HashMap<u64, Option<BoxTaskHandle>>>,
}

impl ConnectionTaskRegistry {
    fn reserve(&self) -> (u64, Weak<ConnectionTaskRegistryInner>) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        lock_mutex(&self.inner.handles).insert(id, None);
        (id, Arc::downgrade(&self.inner))
    }

    fn attach(&self, task_id: u64, handle: BoxTaskHandle) {
        let mut handles = lock_mutex(&self.inner.handles);
        if let Some(slot) = handles.get_mut(&task_id) {
            *slot = Some(handle);
            return;
        }
        drop(handles);
        handle.abort();
    }

    fn cancel(&self, task_id: u64) {
        lock_mutex(&self.inner.handles).remove(&task_id);
    }

    fn complete_weak(inner: &Weak<ConnectionTaskRegistryInner>, task_id: u64) {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        lock_mutex(&inner.handles).remove(&task_id);
    }
}

impl Drop for ConnectionTaskRegistryInner {
    fn drop(&mut self) {
        let handles = lock_mutex(&self.handles);
        for handle in handles.values().filter_map(Option::as_ref) {
            handle.abort();
        }
    }
}

struct SelectedConnection {
    connection: Option<crate::connection::RealConnection>,
    binding: Option<AcquiredBinding>,
    reused: bool,
    exchange_finder: Arc<ExchangeFinder>,
    bindings: Arc<ConnectionBindings>,
    availability: ConnectionAvailability,
}

impl SelectedConnection {
    fn new(
        connection: crate::connection::RealConnection,
        binding: AcquiredBinding,
        reused: bool,
        exchange_finder: Arc<ExchangeFinder>,
        bindings: Arc<ConnectionBindings>,
        availability: ConnectionAvailability,
    ) -> Self {
        Self {
            connection: Some(connection),
            binding: Some(binding),
            reused,
            exchange_finder,
            bindings,
            availability,
        }
    }

    fn into_send_parts(
        mut self,
    ) -> (
        crate::connection::RealConnection,
        AcquiredBinding,
        bool,
        Arc<ExchangeFinder>,
        Arc<ConnectionBindings>,
        ConnectionAvailability,
    ) {
        let connection = self
            .connection
            .take()
            .expect("selected connection should contain a connection");
        let binding = self
            .binding
            .take()
            .expect("selected connection should contain a binding");
        (
            connection,
            binding,
            self.reused,
            self.exchange_finder.clone(),
            self.bindings.clone(),
            self.availability.clone(),
        )
    }
}

impl Drop for SelectedConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let Some(binding) = self.binding.take() else {
            return;
        };

        release_acquired_connection(
            &self.exchange_finder,
            &self.bindings,
            &self.availability,
            connection,
            binding,
        );
    }
}

struct BoundResponse {
    response: Response<Incoming>,
    release: ResponseLease,
    reused: bool,
}

struct ResponseLease {
    state: Option<ResponseLeaseState>,
}

struct ResponseLeaseShared {
    exchange_finder: Arc<ExchangeFinder>,
    ctx: CallContext,
    _tasks: ConnectionTaskRegistry,
    availability: ConnectionAvailability,
}

enum ResponseLeaseState {
    Http1 {
        connection: crate::connection::RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
        shared: ResponseLeaseShared,
    },
    Http2 {
        connection: crate::connection::RealConnection,
        shared: ResponseLeaseShared,
    },
}

impl ResponseLeaseShared {
    fn new(
        exchange_finder: Arc<ExchangeFinder>,
        ctx: CallContext,
        tasks: ConnectionTaskRegistry,
        availability: ConnectionAvailability,
    ) -> Self {
        Self {
            exchange_finder,
            ctx,
            _tasks: tasks,
            availability,
        }
    }
}

impl ResponseLease {
    fn http1(
        connection: crate::connection::RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
        shared: ResponseLeaseShared,
    ) -> Self {
        Self {
            state: Some(ResponseLeaseState::Http1 {
                connection,
                bindings,
                sender,
                reusable,
                shared,
            }),
        }
    }

    fn http2(connection: crate::connection::RealConnection, shared: ResponseLeaseShared) -> Self {
        Self {
            state: Some(ResponseLeaseState::Http2 { connection, shared }),
        }
    }

    fn release(mut self) {
        if let Some(state) = self.state.take() {
            release_response_lease(state);
        }
    }

    fn discard(mut self) {
        if let Some(state) = self.state.take() {
            discard_response_lease(state);
        }
    }
}

impl Drop for ResponseLease {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            abandon_response_lease_state(state);
        }
    }
}

#[derive(Clone)]
pub(crate) struct TransportService {
    connector: ConnectorStack,
    config: crate::client::TransportConfig,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    exchange_finder: Arc<ExchangeFinder>,
    request_admission: RequestAdmissionLimiter,
    connection_limiter: ConnectionLimiter,
    connection_availability: ConnectionAvailability,
    bindings: Arc<ConnectionBindings>,
    connection_tasks: ConnectionTaskRegistry,
}

impl TransportService {
    pub(crate) fn new(
        connector: ConnectorStack,
        config: crate::client::TransportConfig,
        executor: Arc<dyn WireExecutor>,
        timer: SharedTimer,
        exchange_finder: Arc<ExchangeFinder>,
        request_admission: RequestAdmissionLimiter,
    ) -> Self {
        let connection_availability = ConnectionAvailability::default();
        let connection_limiter = ConnectionLimiter::new(
            config.max_connections_total,
            config.max_connections_per_host,
            connection_availability.clone(),
        );
        Self {
            connector,
            config,
            executor,
            timer,
            exchange_finder,
            request_admission,
            connection_limiter,
            connection_availability,
            bindings: Arc::new(ConnectionBindings::default()),
            connection_tasks: ConnectionTaskRegistry::default(),
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
        record_pool_lookup_trace(&call_span, &prepared);
        async move {
            ctx.listener()
                .pool_lookup(&ctx, prepared.pool_hit(), prepared.pool_connection_id());
            let selected = self
                .acquire_connection(&prepared, &request, ctx.clone(), tracing::Span::current())
                .await?;
            let response = send_bound_request(
                request,
                selected,
                ctx.clone(),
                self.connection_tasks.clone(),
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
            let deadline_expired =
                spawn_body_deadline_signal(self.executor.clone(), self.timer.clone(), &ctx)?;
            let body = ObservedIncomingBody::wrap(
                body,
                ctx.clone(),
                attempt,
                Some(response.release),
                deadline_expired,
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
        let mut exact_candidate = prepared.reserved_connection().cloned();
        let mut route_plan = None;

        loop {
            let wait_for_availability = self.connection_availability.listen();

            let connection = match exact_candidate.take() {
                Some(connection) => Some(connection),
                None => self.exchange_finder.pool().acquire(prepared.address()),
            };
            if let Some(connection) = connection {
                match self.bindings.acquire(connection.id()) {
                    BindingAcquireResult::Acquired(binding) => {
                        return Ok(SelectedConnection::new(
                            connection,
                            binding,
                            true,
                            self.exchange_finder.clone(),
                            self.bindings.clone(),
                            self.connection_availability.clone(),
                        ));
                    }
                    BindingAcquireResult::Busy => {
                        let _ = self.exchange_finder.release(&connection);
                    }
                    BindingAcquireResult::Stale => {
                        let _ = self.exchange_finder.pool().remove(connection.id());
                        self.connection_availability.notify();
                    }
                }
            }

            if route_plan.is_none() {
                route_plan = Some(
                    self.connector
                        .route_plan(ctx.clone(), prepared.address())
                        .await?,
                );
            }
            if let Some(selected) = self
                .try_acquire_coalesced(prepared.address(), route_plan.as_ref().expect("route plan"))
            {
                return Ok(selected);
            }

            let Some(connection_permit) = self
                .connection_limiter
                .try_acquire(prepared.address().clone())
            else {
                wait_for_availability.await;
                continue;
            };

            return self
                .bind_fresh_connection(
                    prepared,
                    request,
                    ctx,
                    span,
                    route_plan.take().expect("route plan"),
                    connection_permit,
                )
                .await;
        }
    }

    async fn bind_fresh_connection(
        &self,
        prepared: &crate::connection::PreparedExchange,
        request: &Request<RequestBody>,
        ctx: CallContext,
        span: tracing::Span,
        route_plan: RoutePlan,
        connection_permit: ConnectionPermit,
    ) -> Result<SelectedConnection, WireError> {
        let connect_span = tracing::Span::current();
        let stream = connect_route_plan(
            ctx,
            request.uri().clone(),
            route_plan,
            self.connector.proxy_connect_deps(),
        )
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
        let connected = stream.connected();
        let info = connection_info_from_connected(&connected);
        let coalescing = coalescing_info_from_connected(&connected);
        let protocol = determine_protocol(prepared.address(), &connected);
        let route = Route::from_observed(prepared.address().clone(), info.remote_addr);
        let connection = crate::connection::RealConnection::with_id_permit_and_coalescing(
            info.id,
            route,
            protocol,
            Some(connection_permit),
            coalescing,
        );
        let _ = connection.try_acquire();

        let binding = match protocol {
            ConnectionProtocol::Http1 => {
                let (sender, task) = bind_http1(stream).await?;
                self.bindings.insert_http1(info.id, info, sender);
                let binding = match self.bindings.acquire(connection.id()) {
                    BindingAcquireResult::Acquired(binding) => binding,
                    BindingAcquireResult::Busy | BindingAcquireResult::Stale => {
                        self.bindings.remove(connection.id());
                        return Err(WireError::internal(
                            "freshly bound HTTP/1 connection was not available for request execution",
                            io::Error::other(
                                "bound HTTP/1 connection missing immediately after insert",
                            ),
                        ));
                    }
                };
                self.exchange_finder.pool().insert(connection.clone());
                if let Err(error) = self.spawn_http1_task(connection.clone(), task, span) {
                    self.bindings.remove(connection.id());
                    let _ = self.exchange_finder.pool().remove(connection.id());
                    self.connection_availability.notify();
                    return Err(error);
                }
                binding
            }
            ConnectionProtocol::Http2 => {
                let (sender, task) = bind_http2(
                    stream,
                    &self.config,
                    HyperExecutor(self.executor.clone()),
                    self.timer.clone(),
                )
                .await?;
                self.bindings.insert_http2(info.id, info, sender);
                let binding = match self.bindings.acquire(connection.id()) {
                    BindingAcquireResult::Acquired(binding) => binding,
                    BindingAcquireResult::Busy | BindingAcquireResult::Stale => {
                        self.bindings.remove(connection.id());
                        return Err(WireError::internal(
                            "freshly bound HTTP/2 connection was not available for request execution",
                            io::Error::other(
                                "bound HTTP/2 connection missing immediately after insert",
                            ),
                        ));
                    }
                };
                self.exchange_finder.pool().insert(connection.clone());
                if let Err(error) = self.spawn_http2_task(connection.clone(), task, span) {
                    self.bindings.remove(connection.id());
                    let _ = self.exchange_finder.pool().remove(connection.id());
                    self.connection_availability.notify();
                    return Err(error);
                }
                binding
            }
        };

        Ok(SelectedConnection::new(
            connection,
            binding,
            false,
            self.exchange_finder.clone(),
            self.bindings.clone(),
            self.connection_availability.clone(),
        ))
    }

    fn try_acquire_coalesced(
        &self,
        address: &Address,
        route_plan: &RoutePlan,
    ) -> Option<SelectedConnection> {
        let connection = self
            .exchange_finder
            .pool()
            .acquire_coalesced(address, route_plan)?;
        match self.bindings.acquire(connection.id()) {
            BindingAcquireResult::Acquired(binding) => {
                return Some(SelectedConnection::new(
                    connection,
                    binding,
                    true,
                    self.exchange_finder.clone(),
                    self.bindings.clone(),
                    self.connection_availability.clone(),
                ));
            }
            BindingAcquireResult::Busy => {
                let _ = self.exchange_finder.release(&connection);
            }
            BindingAcquireResult::Stale => {
                let _ = self.exchange_finder.pool().remove(connection.id());
                self.connection_availability.notify();
            }
        }

        None
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
        let availability = self.connection_availability.clone();
        let (task_id, registry) = self.connection_tasks.reserve();
        let future = Box::pin(
            async move {
                let result = task.await;
                bindings.remove(connection_id);
                let _ = pool.remove(connection_id);
                availability.notify();
                if let Err(error) = result {
                    tracing::debug!(
                        connection_id = connection_id.as_u64(),
                        error = %error,
                        "owned HTTP/1 connection task failed",
                    );
                }
                ConnectionTaskRegistry::complete_weak(&registry, task_id);
            }
            .instrument(span),
        );
        match self.executor.spawn(future) {
            Ok(handle) => {
                self.connection_tasks.attach(task_id, handle);
                Ok(())
            }
            Err(error) => {
                self.connection_tasks.cancel(task_id);
                Err(error)
            }
        }
    }

    fn spawn_http2_task(
        &self,
        connection: crate::connection::RealConnection,
        task: http2::Connection<BoxConnection, RequestBody, HyperExecutor>,
        span: tracing::Span,
    ) -> Result<(), WireError> {
        let connection_id = connection.id();
        let bindings = self.bindings.clone();
        let pool = self.exchange_finder.pool().clone();
        let availability = self.connection_availability.clone();
        let (task_id, registry) = self.connection_tasks.reserve();
        let future = Box::pin(
            async move {
                let result = task.await;
                bindings.remove(connection_id);
                let _ = pool.remove(connection_id);
                availability.notify();
                if let Err(error) = result {
                    tracing::debug!(
                        connection_id = connection_id.as_u64(),
                        error = %error,
                        "owned HTTP/2 connection task failed",
                    );
                }
                ConnectionTaskRegistry::complete_weak(&registry, task_id);
            }
            .instrument(span),
        );
        match self.executor.spawn(future) {
            Ok(handle) => {
                self.connection_tasks.attach(task_id, handle);
                Ok(())
            }
            Err(error) => {
                self.connection_tasks.cancel(task_id);
                Err(error)
            }
        }
    }
}

impl Service<Exchange> for TransportService {
    type Response = Response<ResponseBody>;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.request_admission.poll_ready(cx)
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        Box::pin(
            self.clone()
                .execute_exchange(exchange)
                .with_current_subscriber(),
        )
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
        pool_hit = tracing::field::Empty,
        pool_connection_id = tracing::field::Empty,
        route_count = tracing::field::Empty,
        fast_fallback_enabled = tracing::field::Empty,
        connect_race_id = tracing::field::Empty,
        connect_winner = tracing::field::Empty,
        connection_id = tracing::field::Empty,
        connection_reused = tracing::field::Empty,
    )
}

fn record_pool_lookup_trace(span: &tracing::Span, prepared: &crate::connection::PreparedExchange) {
    span.record("pool_hit", prepared.pool_hit());
    if let Some(connection_id) = prepared.pool_connection_id() {
        span.record("pool_connection_id", connection_id.as_u64());
    }
}

struct ObservedIncomingBody {
    inner: Incoming,
    ctx: CallContext,
    attempt: u32,
    bytes_read: u64,
    release: Option<ResponseLease>,
    deadline_signal: Option<Arc<BodyDeadlineSignal>>,
    span: tracing::Span,
    finished: bool,
}

impl ObservedIncomingBody {
    fn wrap(
        body: Incoming,
        ctx: CallContext,
        attempt: u32,
        release: Option<ResponseLease>,
        deadline_signal: Option<Arc<BodyDeadlineSignal>>,
        span: tracing::Span,
    ) -> ResponseBody {
        ResponseBody::new(
            Self {
                inner: body,
                ctx,
                attempt,
                bytes_read: 0,
                release,
                deadline_signal,
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
        abandon_response_lease(self.release.take());
    }

    fn poll_deadline(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, WireError>>> {
        let Some(deadline_signal) = self.deadline_signal.as_ref() else {
            return Poll::Pending;
        };
        if !deadline_signal.expired.load(Ordering::Acquire) {
            deadline_signal.waker.register(cx.waker());
            if !deadline_signal.expired.load(Ordering::Acquire) {
                return Poll::Pending;
            }
        }

        let deadline = self
            .ctx
            .deadline()
            .expect("deadline signal exists only when call timeout is configured");
        let timeout = deadline.saturating_duration_since(self.ctx.created_at());
        let error = WireError::timeout(format!("call timed out after {timeout:?}"));
        self.finish_with_error(&error);
        Poll::Ready(Some(Err(error)))
    }

    fn release_connection(&mut self) {
        if let Some(release) = self.release.take() {
            release.release();
        }
    }

    fn discard_connection(&mut self) {
        if let Some(release) = self.release.take() {
            release.discard();
        }
    }
}

struct BodyDeadlineSignal {
    expired: AtomicBool,
    waker: AtomicWaker,
}

fn abandon_response_lease(release: Option<ResponseLease>) {
    drop(release);
}

fn release_acquired_connection(
    exchange_finder: &Arc<ExchangeFinder>,
    bindings: &Arc<ConnectionBindings>,
    availability: &ConnectionAvailability,
    connection: crate::connection::RealConnection,
    binding: AcquiredBinding,
) {
    match binding {
        AcquiredBinding::Http1 { sender, .. } => {
            if bindings.release_http1(connection.id(), sender)
                && exchange_finder.release(&connection)
            {
                availability.notify();
                return;
            }
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
        }
        AcquiredBinding::Http2 { .. } => {
            if exchange_finder.release(&connection) {
                availability.notify();
                return;
            }
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
        }
    }
}

fn release_response_lease(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            sender,
            reusable,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            if reusable
                && bindings.release_http1(connection.id(), sender)
                && exchange_finder.release(&connection)
            {
                availability.notify();
                ctx.listener().connection_released(&ctx, connection.id());
                return;
            }
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
        ResponseLeaseState::Http2 {
            connection,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            let _ = exchange_finder.release(&connection);
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn discard_response_lease(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
        ResponseLeaseState::Http2 {
            connection,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            connection.mark_unhealthy();
            let _ = exchange_finder.release(&connection);
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn abandon_response_lease_state(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
        ResponseLeaseState::Http2 {
            connection,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            let _ = exchange_finder.release(&connection);
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn spawn_body_deadline_signal(
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    ctx: &CallContext,
) -> Result<Option<Arc<BodyDeadlineSignal>>, WireError> {
    let Some(deadline) = ctx.deadline() else {
        return Ok(None);
    };

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let signal = Arc::new(BodyDeadlineSignal {
        expired: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    if remaining == Duration::ZERO {
        signal.expired.store(true, Ordering::Release);
        return Ok(Some(signal));
    }

    let signal_task = signal.clone();
    let _ = executor.spawn(Box::pin(async move {
        timer.sleep(remaining).await;
        signal_task.expired.store(true, Ordering::Release);
        signal_task.waker.wake();
    }))?;

    Ok(Some(signal))
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
        if this.finished {
            return Poll::Ready(None);
        }
        if let Poll::Ready(result) = this.poll_deadline(cx) {
            return Poll::Ready(result);
        }
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
    ctx: CallContext,
    tasks: ConnectionTaskRegistry,
) -> Result<BoundResponse, WireError> {
    let (connection, binding, reused, exchange_finder, bindings, availability) =
        selected.into_send_parts();
    let request = prepare_bound_request(request, connection.protocol(), connection.route().kind())?;

    match binding {
        AcquiredBinding::Http1 { info, mut sender } => {
            sender.ready().await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings, &availability);
                map_hyper_error(error)
            })?;
            let mut response = sender.send_request(request).await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings, &availability);
                map_hyper_error(error)
            })?;
            let reusable = http1_response_allows_reuse(&response);
            response.extensions_mut().insert(info);
            Ok(BoundResponse {
                response,
                release: ResponseLease::http1(
                    connection,
                    bindings,
                    sender,
                    reusable,
                    ResponseLeaseShared::new(exchange_finder, ctx, tasks, availability),
                ),
                reused,
            })
        }
        AcquiredBinding::Http2 { info, mut sender } => {
            sender.ready().await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings, &availability);
                map_hyper_error(error)
            })?;
            let mut response = sender.send_request(request).await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings, &availability);
                map_hyper_error(error)
            })?;
            response.extensions_mut().insert(info);
            Ok(BoundResponse {
                response,
                release: ResponseLease::http2(
                    connection,
                    ResponseLeaseShared::new(exchange_finder, ctx, tasks, availability),
                ),
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
        .map_err(|error| WireError::protocol_binding("HTTP/1.1 client handshake failed", error))
}

async fn bind_http2(
    stream: BoxConnection,
    config: &crate::client::TransportConfig,
    executor: HyperExecutor,
    timer: SharedTimer,
) -> Result<
    (
        http2::SendRequest<RequestBody>,
        http2::Connection<BoxConnection, RequestBody, HyperExecutor>,
    ),
    WireError,
> {
    let mut builder = http2::Builder::new(executor);
    builder.timer(timer);
    if let Some(interval) = config.http2_keep_alive_interval {
        builder.keep_alive_interval(interval);
        builder.keep_alive_while_idle(config.http2_keep_alive_while_idle);
    }
    builder
        .handshake(stream)
        .await
        .map_err(|error| WireError::protocol_binding("HTTP/2 client handshake failed", error))
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

    !connection_header_requests_close(response.headers())
}

fn connection_header_requests_close(headers: &HeaderMap) -> bool {
    headers
        .get_all(CONNECTION)
        .iter()
        .any(|value| connection_header_value_requests_close(value).unwrap_or(true))
}

fn connection_header_value_requests_close(value: &HeaderValue) -> Result<bool, ()> {
    let value = value.to_str().map_err(|_| ())?;
    let bytes = value.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        while index < bytes.len() && is_optional_whitespace(bytes[index]) {
            index += 1;
        }
        if index == bytes.len() {
            return Ok(false);
        }
        if bytes[index] == b',' {
            index += 1;
            continue;
        }

        let token_start = index;
        while index < bytes.len() && is_tchar(bytes[index]) {
            index += 1;
        }
        if token_start == index {
            return Err(());
        }

        let token = &value[token_start..index];
        while index < bytes.len() && is_optional_whitespace(bytes[index]) {
            index += 1;
        }

        match bytes.get(index).copied() {
            None => return Ok(token.eq_ignore_ascii_case("close")),
            Some(b',') => {
                if token.eq_ignore_ascii_case("close") {
                    return Ok(true);
                }
                index += 1;
            }
            Some(_) => return Err(()),
        }
    }

    Ok(false)
}

fn is_optional_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn is_tchar(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
}

fn cleanup_failed_request(
    connection: &crate::connection::RealConnection,
    exchange_finder: &Arc<ExchangeFinder>,
    bindings: &Arc<ConnectionBindings>,
    availability: &ConnectionAvailability,
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
    availability.notify();
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

fn coalescing_info_from_connected(connected: &Connected) -> CoalescingInfo {
    let mut extensions = http::Extensions::new();
    connected.get_extras(&mut extensions);
    extensions.remove::<CoalescingInfo>().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::cmp;
    use std::collections::HashMap;
    use std::io;
    use std::panic::{self, AssertUnwindSafe};
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::task::noop_waker_ref;
    use http::header::{HeaderValue, CONNECTION};
    use http::HeaderMap;
    use hyper::rt::{Read, ReadBuf, Write};
    use hyper_util::client::legacy::connect::Connected;
    use tracing::field::{Field, Visit};
    use tracing::{Id, Subscriber};
    use tracing_subscriber::layer::{Context as LayerContext, Layer};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::LookupSpan;

    use openwire_core::{
        BoxFuture, BoxTaskHandle, CallContext, NoopEventListener, Runtime, SharedTimer, TaskHandle,
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
        RealConnection, Route, UriScheme,
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

    impl Runtime for CountingSpawnRuntime {
        fn spawn(&self, _future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
            self.spawns
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(NoopTaskHandle))
        }

        fn sleep(&self, _duration: Duration) -> BoxFuture<()> {
            Box::pin(async {})
        }
    }

    impl WireExecutor for CountingSpawnRuntime {
        fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
            Runtime::spawn(self, future)
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

    #[test]
    fn connection_task_registry_recovers_after_mutex_poisoning() {
        let registry = ConnectionTaskRegistry::default();

        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = registry
                .inner
                .handles
                .lock()
                .expect("poison connection task registry lock for test");
            panic!("poison connection task registry");
        }));

        let (task_id, _weak) = registry.reserve();
        registry.cancel(task_id);
    }
}
