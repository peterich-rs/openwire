use std::cmp;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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
    next_connection_id, AuthKind, BoxConnection, BoxFuture, BoxTaskHandle, CallContext,
    CoalescingInfo, ConnectionId, ConnectionInfo, DnsResolver, Exchange, HyperExecutor,
    RequestBody, ResponseBody, SharedTimer, TcpConnector, TlsConnector, WireError, WireExecutor,
};
use pin_project_lite::pin_project;
use tower::Service;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::auth::{
    build_auth_context, AuthAttemptState, AuthRequestState, AuthResponseState, SharedAuthenticator,
};
use crate::connection::{
    Address, ConnectionAvailability, ConnectionLimiter, ConnectionPermit, ConnectionProtocol,
    DirectDialDeps, ExchangeFinder, FastFallbackDialer, FastFallbackOutcome, FastFallbackRuntime,
    Route, RouteKind, RoutePlan, RoutePlanner, UriScheme,
};
use crate::proxy::ProxyCredentials;
use crate::sync_util::lock_mutex;
use crate::trace::PolicyTraceContext;

mod connect;

pub(crate) use connect::ConnectorStack;
use self::connect::{
    connect_route_plan, establish_connect_tunnel, establish_socks5_tunnel,
    find_connect_response_end, parse_connect_response_head, record_fast_fallback_trace,
    split_connect_response_head, ConnectBudget, ConnectTunnelParams, PrefetchedTunnelBytes,
};

mod bindings;

use self::bindings::{
    release_acquired_connection, AcquiredBinding, BindingAcquireResult, ConnectionBindings,
    ConnectionTaskRegistry,
};

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

mod body;

#[cfg(test)]
use self::body::abandon_response_lease;
use self::body::{
    spawn_body_deadline_signal, BoundResponse, ObservedIncomingBody, ResponseLease,
    ResponseLeaseShared,
};

#[derive(Clone)]
pub(crate) struct TransportService {
    connector: ConnectorStack,
    config: crate::client::TransportConfig,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    exchange_finder: Arc<ExchangeFinder>,
    on_pooled_connection_published: Option<Arc<dyn Fn() + Send + Sync>>,
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
        on_pooled_connection_published: Option<Arc<dyn Fn() + Send + Sync>>,
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
            on_pooled_connection_published,
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
                self.start_pool_reaper_if_needed();
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
                self.start_pool_reaper_if_needed();
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

    fn start_pool_reaper_if_needed(&self) {
        if let Some(hook) = self.on_pooled_connection_published.as_ref() {
            hook();
        }
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

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
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
            let request_requests_close = connection_header_requests_close(request.headers());
            let mut response = sender.send_request(request).await.map_err(|error| {
                cleanup_failed_request(&connection, &exchange_finder, &bindings, &availability);
                map_hyper_error(error)
            })?;
            let reusable = http1_exchange_allows_reuse(request_requests_close, &response);
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

fn http1_exchange_allows_reuse(
    request_requests_close: bool,
    response: &Response<Incoming>,
) -> bool {
    if request_requests_close {
        return false;
    }
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
mod tests;
