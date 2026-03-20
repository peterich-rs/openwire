use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::Response;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioIo;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use openwire_core::{
    next_connection_id, BoxConnection, BoxFuture, CallContext, ConnectionInfo, DnsResolver,
    Exchange, RequestBody, ResponseBody, Runtime, TcpConnector, TlsConnector, WireError,
};
use tower::Service;
use tracing::Instrument;

#[derive(Clone)]
pub struct TokioRuntime;

impl Runtime for TokioRuntime {
    fn spawn(&self, future: openwire_core::BoxFuture<()>) -> Result<(), WireError> {
        tokio::spawn(future);
        Ok(())
    }

    fn sleep(&self, duration: Duration) -> openwire_core::BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
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
                        let error =
                            WireError::timeout(format!("connection timed out after {timeout:?}"));
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
    pub(crate) ctx: CallContext,
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
        let ctx = self.ctx.clone();

        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| WireError::invalid_request("request URI is missing a host"))?
                .to_owned();

            let scheme = uri
                .scheme_str()
                .ok_or_else(|| WireError::invalid_request("request URI is missing a scheme"))?;
            let port = uri.port_u16().unwrap_or_else(|| {
                if scheme.eq_ignore_ascii_case("https") {
                    443
                } else {
                    80
                }
            });

            let addrs = dns_resolver
                .resolve(ctx.clone(), host.clone(), port)
                .await?;
            let mut last_error = None;

            for addr in addrs {
                match tcp_connector
                    .connect(ctx.clone(), addr, connect_timeout)
                    .await
                {
                    Ok(stream) => {
                        if scheme.eq_ignore_ascii_case("https") {
                            let tls_connector = tls_connector.clone().ok_or_else(|| {
                                WireError::tls(
                                    "HTTPS requested but no TLS connector is configured",
                                    io::Error::new(
                                        io::ErrorKind::Unsupported,
                                        "missing TLS connector",
                                    ),
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
        })
    }
}

#[derive(Clone)]
pub(crate) struct TransportService {
    pub(crate) client: HyperClient<ConnectorStack, RequestBody>,
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
        Box::pin(async move {
            let (request, ctx, attempt) = exchange.into_parts();
            let request_body_len = request.body().replayable_len();

            let call_span = tracing::debug_span!(
                "openwire.attempt",
                call_id = ctx.call_id().as_u64(),
                attempt,
                method = %request.method(),
                uri = %request.uri(),
            );

            async move {
                ctx.listener().request_headers_start(&ctx);
                ctx.listener().request_headers_end(&ctx);

                let response = client.request(request).await.map_err(map_client_error)?;

                if let Some(bytes) = request_body_len {
                    ctx.listener().request_body_end(&ctx, bytes);
                }

                let connection_info = response.extensions().get::<ConnectionInfo>().cloned();
                if let Some(info) = &connection_info {
                    ctx.listener().connection_acquired(&ctx, info.id, false);
                }

                ctx.listener().response_headers_start(&ctx);
                let (parts, body) = response.into_parts();
                let body = ObservedIncomingBody::wrap(body, ctx.clone(), connection_info);
                let response = Response::from_parts(parts, body);
                ctx.listener().response_headers_end(&ctx, &response);

                Ok(response)
            }
            .instrument(call_span)
            .await
        })
    }
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
    bytes_read: u64,
    connection_id: Option<openwire_core::ConnectionId>,
    finished: bool,
}

impl ObservedIncomingBody {
    fn wrap(body: Incoming, ctx: CallContext, info: Option<ConnectionInfo>) -> ResponseBody {
        ResponseBody::new(
            Self {
                inner: body,
                ctx,
                bytes_read: 0,
                connection_id: info.map(|info| info.id),
                finished: false,
            }
            .boxed(),
        )
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ctx
            .listener()
            .response_body_end(&self.ctx, self.bytes_read);
        if let Some(connection_id) = self.connection_id {
            self.ctx
                .listener()
                .connection_released(&self.ctx, connection_id);
        }
    }
}

impl Drop for ObservedIncomingBody {
    fn drop(&mut self) {
        self.finish();
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
                this.ctx.listener().call_failed(&this.ctx, &error);
                this.finish();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish();
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
