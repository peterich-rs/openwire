use std::cmp;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use futures_util::future::{select, Either};
use futures_util::io::{
    AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt,
};
use http::header::HOST;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
use hyper::rt::Timer;
use hyper::Uri;
use openwire_core::{
    AuthKind, BoxConnection, CallContext, Connected, Connection, DnsResolver, EstablishmentStage,
    FailurePhase, RequestBody, SharedTimer, TcpConnector, TlsConnector, WireError, WireErrorKind,
    WireExecutor,
};
use pin_project_lite::pin_project;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::auth::{
    build_auth_context, AuthAttemptState, AuthRequestState, AuthResponseState, SharedAuthenticator,
};
use crate::connection::{
    Address, DirectDialDeps, FastFallbackDialer, FastFallbackOutcome, FastFallbackRuntime,
    RouteKind, RoutePlan, RoutePlanner,
};
use crate::proxy::ProxyCredentials;

#[derive(Clone)]
pub(crate) struct ConnectorStack {
    pub(crate) dns_resolver: Arc<dyn DnsResolver>,
    pub(crate) tcp_connector: Arc<dyn TcpConnector>,
    pub(crate) tls_connector: Option<Arc<dyn TlsConnector>>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) executor: Arc<dyn WireExecutor>,
    pub(crate) timer: SharedTimer,
    pub(crate) route_planner: Arc<dyn RoutePlanner>,
    pub(crate) proxy_authenticator: Option<SharedAuthenticator>,
    pub(crate) max_proxy_auth_attempts: usize,
}

#[derive(Clone)]
pub(crate) struct ProxyConnectDeps {
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    proxy_authenticator: Option<SharedAuthenticator>,
    max_proxy_auth_attempts: usize,
    connect_timeout: Option<Duration>,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
}

pub(super) struct ConnectTunnelParams<'a> {
    pub(super) ctx: CallContext,
    pub(super) proxy_addr: SocketAddr,
    pub(super) target_uri: &'a Uri,
    pub(super) stream: BoxConnection,
    pub(super) tcp_connector: Arc<dyn TcpConnector>,
    pub(super) initial_proxy_credentials: Option<ProxyCredentials>,
    pub(super) proxy_authenticator: Option<SharedAuthenticator>,
    pub(super) max_proxy_auth_attempts: usize,
    pub(super) budget: ConnectBudget,
    pub(super) timer: SharedTimer,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ConnectBudget {
    started_at: Instant,
    connect_timeout: Option<Duration>,
    call_deadline: Option<Instant>,
}

impl ConnectBudget {
    pub(super) fn new(connect_timeout: Option<Duration>, call_deadline: Option<Instant>) -> Self {
        Self {
            started_at: Instant::now(),
            connect_timeout,
            call_deadline,
        }
    }

    pub(super) fn remaining(&self) -> Option<Duration> {
        let connect_remaining = self
            .connect_timeout
            .map(|timeout| timeout.saturating_sub(self.started_at.elapsed()));
        let call_remaining = self
            .call_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));

        match (connect_remaining, call_remaining) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
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
    pub(crate) async fn route_plan(
        &self,
        ctx: CallContext,
        address: &Address,
    ) -> Result<RoutePlan, WireError> {
        let (dns_host, dns_port) = self.route_planner.dns_target(address);
        let resolved_addrs = self
            .dns_resolver
            .resolve(ctx.clone(), dns_host, dns_port)
            .await?;
        self.route_planner.plan(address, resolved_addrs)
    }

    pub(crate) fn proxy_connect_deps(&self, connect_timeout: Option<Duration>) -> ProxyConnectDeps {
        ProxyConnectDeps {
            tcp_connector: self.tcp_connector.clone(),
            tls_connector: self.tls_connector.clone(),
            proxy_authenticator: self.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.max_proxy_auth_attempts,
            connect_timeout,
            executor: self.executor.clone(),
            timer: self.timer.clone(),
        }
    }
}

pub(crate) async fn connect_route_plan(
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
            let deps = DirectDialDeps::new(
                deps.executor,
                deps.timer,
                deps.tcp_connector,
                deps.tls_connector,
                deps.connect_timeout,
            );
            connect_direct(ctx, uri, route_plan, deps).await
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
    deps: DirectDialDeps,
) -> Result<BoxConnection, WireError> {
    let connect_span = tracing::Span::current();
    let (stream, outcome) = FastFallbackDialer
        .dial_direct(ctx.clone(), uri, route_plan, deps)
        .instrument(connect_span)
        .with_current_subscriber()
        .await?;
    let span = tracing::Span::current();
    record_fast_fallback_trace(&span, outcome);
    Ok(stream)
}

pub(super) fn record_fast_fallback_trace(span: &tracing::Span, outcome: FastFallbackOutcome) {
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
            FastFallbackRuntime::new(executor, timer),
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
            FastFallbackRuntime::new(deps.executor.clone(), deps.timer.clone()),
            {
                let deps = deps.clone();
                move |ctx, route| {
                    let deps = deps.clone();
                    async move {
                        let budget = ConnectBudget::new(deps.connect_timeout, ctx.deadline());
                        match route.kind() {
                            RouteKind::ConnectProxy { proxy, .. } => {
                                deps.tcp_connector
                                    .connect(ctx, *proxy, budget.remaining())
                                    .await
                                    .map(|stream| (stream, budget))
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
                move |ctx, target_uri, route, (stream, budget)| {
                    let deps = deps.clone();
                    async move {
                        let (proxy_addr, credentials) = match route.kind() {
                            RouteKind::ConnectProxy { proxy, credentials } => {
                                (*proxy, credentials.clone())
                            }
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
                            initial_proxy_credentials: credentials,
                            proxy_authenticator: deps.proxy_authenticator.clone(),
                            max_proxy_auth_attempts: deps.max_proxy_auth_attempts,
                            budget,
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
            FastFallbackRuntime::new(deps.executor.clone(), deps.timer.clone()),
            {
                let deps = deps.clone();
                move |ctx, route| {
                    let deps = deps.clone();
                    async move {
                        let budget = ConnectBudget::new(deps.connect_timeout, ctx.deadline());
                        match route.kind() {
                            RouteKind::SocksProxy { proxy, .. } => {
                                deps.tcp_connector
                                    .connect(ctx, *proxy, budget.remaining())
                                    .await
                                    .map(|stream| (stream, budget))
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
                move |ctx, target_uri, route, (stream, budget)| {
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
                            budget,
                            deps.timer.clone(),
                            credentials.as_ref(),
                        )
                        .await?;
                        let proxied =
                            Box::new(ProxiedConnection { inner: tunneled }) as BoxConnection;
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
        .with_authority_from_uri(&target_uri)
    })?;
    tls_connector.connect(ctx, target_uri, stream).await
}

pub(super) async fn establish_connect_tunnel(
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
    if let Some(credentials) = &params.initial_proxy_credentials {
        let value =
            HeaderValue::from_str(&credentials.basic_auth_header_value()).map_err(|error| {
                WireError::with_source(
                    WireErrorKind::InvalidRequest,
                    "proxy basic auth header is not valid ASCII",
                    error,
                )
                .with_proxy_addr(params.proxy_addr)
            })?;
        connect_headers.insert(http::header::PROXY_AUTHORIZATION, value);
    }

    loop {
        match send_connect_request(
            &authority,
            &connect_headers,
            stream,
            &params.timer,
            params.budget.remaining(),
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

                let auth_ctx = build_auth_context(
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

                let Some(request) =
                    authenticator
                        .authenticate(auth_ctx)
                        .await
                        .map_err(|error| {
                            error
                                .with_response_status(head.status)
                                .with_proxy_addr(params.proxy_addr)
                        })?
                else {
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
                        params.budget.remaining(),
                    )
                    .await?;
            }
        }
    }
}

pub(super) async fn establish_socks5_tunnel(
    ctx: CallContext,
    target_uri: &Uri,
    proxy_addr: SocketAddr,
    stream: BoxConnection,
    budget: ConnectBudget,
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

    socks5_write_client_greeting(
        &mut stream,
        &timer,
        budget.remaining(),
        credentials.is_some(),
    )
    .await?;
    match socks5_read_server_choice(
        &mut stream,
        &timer,
        budget.remaining(),
        credentials.is_some(),
    )
    .await?
    {
        Socks5AuthMethod::NoAuth => {}
        Socks5AuthMethod::UsernamePassword => {
            let credentials = credentials.ok_or_else(|| {
                WireError::proxy_tunnel_non_retryable(
                    "SOCKS5 proxy requested username/password authentication but no credentials were configured",
                )
            })?;
            socks5_write_password_auth_request(
                &mut stream,
                &timer,
                budget.remaining(),
                credentials,
            )
            .await?;
            socks5_read_password_auth_response(&mut stream, &timer, budget.remaining()).await?;
        }
    }
    socks5_write_connect_request(&mut stream, &timer, host, port, budget.remaining()).await?;
    socks5_read_connect_response(&mut stream, &timer, budget.remaining()).await?;
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
pub(super) struct ConnectResponseHead {
    pub(super) status: StatusCode,
    pub(super) status_line: String,
    pub(super) headers: HeaderMap,
    pub(super) prefetched: Bytes,
}

pub(super) struct PrefetchedTunnelBytes {
    inner: BoxConnection,
    prefetched: Option<Bytes>,
}

impl PrefetchedTunnelBytes {
    pub(super) fn new(inner: BoxConnection, prefetched: Bytes) -> Self {
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
                WireError::new(
                    WireErrorKind::Timeout,
                    format!("proxy CONNECT timed out after {timeout:?}"),
                )
                .with_phase(FailurePhase::ProxyTunnel)
                .with_establishment(EstablishmentStage::ProxyTunnel, true)
                .with_connect_timeout()
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
            WireError::new(
                WireErrorKind::Timeout,
                format!("SOCKS5 handshake timed out after {timeout:?}"),
            )
            .with_phase(FailurePhase::ProxyTunnel)
            .with_establishment(EstablishmentStage::ProxyTunnel, true)
            .with_connect_timeout()
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

pub(super) fn find_connect_response_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

pub(super) fn split_connect_response_head(
    response: &[u8],
    head_end: usize,
) -> Result<(&[u8], Bytes), WireError> {
    Ok((
        &response[..head_end],
        Bytes::copy_from_slice(&response[head_end..]),
    ))
}

pub(super) fn parse_connect_response_head(
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

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use openwire_core::{BoxConnection, Connected, Connection};

    use super::ProxiedConnection;

    struct TestConnection;

    impl Connection for TestConnection {
        fn connected(&self) -> Connected {
            Connected::new()
        }
    }

    impl hyper::rt::Read for TestConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl hyper::rt::Write for TestConnection {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn proxied_connection_marks_stream_as_proxied() {
        let stream = ProxiedConnection {
            inner: Box::new(TestConnection) as BoxConnection,
        };

        assert!(stream.connected().is_proxied());
    }
}
