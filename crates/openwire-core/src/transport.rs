use std::net::SocketAddr;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use tower::util::BoxCloneSyncService;
use tower::{Service, ServiceExt};

use crate::{BoxFuture, CallContext, WireError};

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: crate::ConnectionId,
    pub remote_addr: Option<SocketAddr>,
    pub local_addr: Option<SocketAddr>,
    pub tls: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoalescingInfo {
    pub verified_server_names: Vec<String>,
}

impl CoalescingInfo {
    pub fn new(verified_server_names: Vec<String>) -> Self {
        Self {
            verified_server_names,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.verified_server_names.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Connected {
    info: Option<ConnectionInfo>,
    coalescing: CoalescingInfo,
    proxied: bool,
    negotiated_h2: bool,
}

impl Connected {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn info(mut self, info: ConnectionInfo) -> Self {
        self.info = Some(info);
        self
    }

    pub fn coalescing(mut self, coalescing: CoalescingInfo) -> Self {
        self.coalescing = coalescing;
        self
    }

    pub fn proxy(mut self, proxied: bool) -> Self {
        self.proxied = proxied;
        self
    }

    pub fn negotiated_h2(mut self, negotiated_h2: bool) -> Self {
        self.negotiated_h2 = negotiated_h2;
        self
    }

    pub fn is_proxied(&self) -> bool {
        self.proxied
    }

    pub fn is_negotiated_h2(&self) -> bool {
        self.negotiated_h2
    }

    pub fn connection_info(&self) -> Option<&ConnectionInfo> {
        self.info.as_ref()
    }

    pub fn connection_info_or_default(&self) -> ConnectionInfo {
        self.info.clone().unwrap_or_else(|| ConnectionInfo {
            id: crate::next_connection_id(),
            remote_addr: None,
            local_addr: None,
            tls: false,
        })
    }

    pub fn coalescing_info(&self) -> &CoalescingInfo {
        &self.coalescing
    }
}

pub trait Connection {
    fn connected(&self) -> Connected;
}

pub trait ConnectionIo:
    hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send + 'static
{
}

impl<T> ConnectionIo for T where
    T: hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send + 'static
{
}

pub type BoxConnection = Box<dyn ConnectionIo>;

impl Connection for BoxConnection {
    fn connected(&self) -> Connected {
        (**self).connected()
    }
}

#[derive(Clone, Debug)]
pub struct DnsRequest {
    pub ctx: CallContext,
    pub host: String,
    pub port: u16,
}

impl DnsRequest {
    pub fn new(ctx: CallContext, host: String, port: u16) -> Self {
        Self { ctx, host, port }
    }
}

#[derive(Clone, Debug)]
pub struct TcpConnectRequest {
    pub ctx: CallContext,
    pub addr: SocketAddr,
    pub timeout: Option<Duration>,
}

impl TcpConnectRequest {
    pub fn new(ctx: CallContext, addr: SocketAddr, timeout: Option<Duration>) -> Self {
        Self { ctx, addr, timeout }
    }
}

pub struct TlsConnectRequest {
    pub ctx: CallContext,
    pub uri: Uri,
    pub stream: BoxConnection,
}

impl TlsConnectRequest {
    pub fn new(ctx: CallContext, uri: Uri, stream: BoxConnection) -> Self {
        Self { ctx, uri, stream }
    }
}

pub type BoxDnsService = BoxCloneSyncService<DnsRequest, Vec<SocketAddr>, WireError>;
pub type BoxTcpService = BoxCloneSyncService<TcpConnectRequest, BoxConnection, WireError>;
pub type BoxTlsService = BoxCloneSyncService<TlsConnectRequest, BoxConnection, WireError>;

pub trait DnsResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>>;
}

pub trait TcpConnector: Send + Sync + 'static {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>>;
}

pub trait TlsConnector: Send + Sync + 'static {
    fn connect(
        &self,
        ctx: CallContext,
        uri: Uri,
        stream: BoxConnection,
    ) -> BoxFuture<Result<BoxConnection, WireError>>;
}

#[derive(Clone)]
pub struct TowerDnsResolver(BoxDnsService);

impl TowerDnsResolver {
    pub fn new<S>(service: S) -> Self
    where
        S: Service<DnsRequest, Response = Vec<SocketAddr>, Error = WireError>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        Self(BoxCloneSyncService::new(service))
    }

    pub fn service(&self) -> BoxDnsService {
        self.0.clone()
    }
}

impl DnsResolver for TowerDnsResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let service = self.0.clone();
        Box::pin(async move { service.oneshot(DnsRequest::new(ctx, host, port)).await })
    }
}

#[derive(Clone)]
pub struct TowerTcpConnector(BoxTcpService);

impl TowerTcpConnector {
    pub fn new<S>(service: S) -> Self
    where
        S: Service<TcpConnectRequest, Response = BoxConnection, Error = WireError>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        Self(BoxCloneSyncService::new(service))
    }

    pub fn service(&self) -> BoxTcpService {
        self.0.clone()
    }
}

impl TcpConnector for TowerTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let service = self.0.clone();
        Box::pin(async move {
            service
                .oneshot(TcpConnectRequest::new(ctx, addr, timeout))
                .await
        })
    }
}

#[derive(Clone)]
pub struct TowerTlsConnector(BoxTlsService);

impl TowerTlsConnector {
    pub fn new<S>(service: S) -> Self
    where
        S: Service<TlsConnectRequest, Response = BoxConnection, Error = WireError>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        Self(BoxCloneSyncService::new(service))
    }

    pub fn service(&self) -> BoxTlsService {
        self.0.clone()
    }
}

impl TlsConnector for TowerTlsConnector {
    fn connect(
        &self,
        ctx: CallContext,
        uri: Uri,
        stream: BoxConnection,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let service = self.0.clone();
        Box::pin(async move {
            service
                .oneshot(TlsConnectRequest::new(ctx, uri, stream))
                .await
        })
    }
}

#[derive(Clone)]
pub struct DnsResolverService<R> {
    resolver: R,
}

impl<R> DnsResolverService<R> {
    pub fn new(resolver: R) -> Self {
        Self { resolver }
    }

    pub fn into_inner(self) -> R {
        self.resolver
    }
}

impl<R> Service<DnsRequest> for DnsResolverService<R>
where
    R: DnsResolver,
{
    type Response = Vec<SocketAddr>;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: DnsRequest) -> Self::Future {
        self.resolver
            .resolve(request.ctx, request.host, request.port)
    }
}

#[derive(Clone)]
pub struct TcpConnectorService<C> {
    connector: C,
}

impl<C> TcpConnectorService<C> {
    pub fn new(connector: C) -> Self {
        Self { connector }
    }

    pub fn into_inner(self) -> C {
        self.connector
    }
}

impl<C> Service<TcpConnectRequest> for TcpConnectorService<C>
where
    C: TcpConnector,
{
    type Response = BoxConnection;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: TcpConnectRequest) -> Self::Future {
        self.connector
            .connect(request.ctx, request.addr, request.timeout)
    }
}

#[derive(Clone)]
pub struct TlsConnectorService<C> {
    connector: C,
}

impl<C> TlsConnectorService<C> {
    pub fn new(connector: C) -> Self {
        Self { connector }
    }

    pub fn into_inner(self) -> C {
        self.connector
    }
}

impl<C> Service<TlsConnectRequest> for TlsConnectorService<C>
where
    C: TlsConnector,
{
    type Response = BoxConnection;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: TlsConnectRequest) -> Self::Future {
        self.connector
            .connect(request.ctx, request.uri, request.stream)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use http::Request;
    use hyper::Uri;
    use tower::{service_fn, Service, ServiceExt};

    use super::{
        BoxConnection, Connected, Connection, ConnectionInfo, DnsRequest, DnsResolver,
        DnsResolverService, TcpConnectRequest, TcpConnector, TcpConnectorService,
        TlsConnectRequest, TlsConnector, TlsConnectorService, TowerDnsResolver, TowerTcpConnector,
        TowerTlsConnector,
    };
    use crate::{CallContext, NoopEventListenerFactory, RequestBody, WireError};

    fn make_call_context() -> CallContext {
        let request = Request::builder()
            .uri("http://example.com/")
            .body(RequestBody::absent())
            .expect("request");
        let factory = Arc::new(NoopEventListenerFactory) as crate::SharedEventListenerFactory;
        CallContext::from_factory(&factory, &request, None)
    }

    fn dummy_connection() -> BoxConnection {
        Box::new(NoopConnection)
    }

    struct NoopConnection;

    impl hyper::rt::Read for NoopConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl hyper::rt::Write for NoopConnection {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Connection for NoopConnection {
        fn connected(&self) -> Connected {
            Connected::new()
        }
    }

    #[test]
    fn connected_negotiated_h2_tracks_requested_state() {
        assert!(Connected::new().negotiated_h2(true).is_negotiated_h2());
        assert!(!Connected::new().negotiated_h2(false).is_negotiated_h2());
    }

    #[test]
    fn connected_connection_info_or_default_preserves_explicit_metadata() {
        let info = ConnectionInfo {
            id: crate::next_connection_id(),
            remote_addr: Some(([192, 0, 2, 10], 443).into()),
            local_addr: Some(([192, 0, 2, 20], 50000).into()),
            tls: true,
        };

        let actual = Connected::new()
            .info(info.clone())
            .connection_info_or_default();

        assert_eq!(actual.id, info.id);
        assert_eq!(actual.remote_addr, info.remote_addr);
        assert_eq!(actual.local_addr, info.local_addr);
        assert_eq!(actual.tls, info.tls);
    }

    #[test]
    fn connected_connection_info_or_default_falls_back_to_placeholder() {
        let actual = Connected::new().connection_info_or_default();

        assert_eq!(actual.remote_addr, None);
        assert_eq!(actual.local_addr, None);
        assert!(!actual.tls);
    }

    #[tokio::test]
    async fn tower_dns_resolver_calls_service() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let resolver = TowerDnsResolver::new(service_fn({
            let calls = calls.clone();
            move |request: DnsRequest| {
                let calls = calls.clone();
                async move {
                    calls
                        .lock()
                        .expect("dns calls")
                        .push((request.host, request.port));
                    Ok::<_, WireError>(vec![std::net::SocketAddr::from(([127, 0, 0, 1], 443))])
                }
            }
        }));

        let resolved = resolver
            .resolve(make_call_context(), "example.com".to_owned(), 443)
            .await
            .expect("resolved addrs");

        assert_eq!(
            calls.lock().expect("dns calls").as_slice(),
            &[("example.com".to_owned(), 443)]
        );
        assert_eq!(
            resolved,
            vec![std::net::SocketAddr::from(([127, 0, 0, 1], 443))]
        );
    }

    struct StaticDnsResolver;

    impl DnsResolver for StaticDnsResolver {
        fn resolve(
            &self,
            _ctx: CallContext,
            host: String,
            port: u16,
        ) -> crate::BoxFuture<Result<Vec<std::net::SocketAddr>, WireError>> {
            Box::pin(async move {
                assert_eq!(host, "resolver.test");
                assert_eq!(port, 8443);
                Ok(vec![std::net::SocketAddr::from(([192, 0, 2, 10], 8443))])
            })
        }
    }

    #[tokio::test]
    async fn dns_resolver_service_calls_resolver() {
        let mut service = DnsResolverService::new(StaticDnsResolver);
        let resolved = service
            .ready()
            .await
            .expect("service ready")
            .call(DnsRequest::new(
                make_call_context(),
                "resolver.test".to_owned(),
                8443,
            ))
            .await
            .expect("resolved addrs");

        assert_eq!(
            resolved,
            vec![std::net::SocketAddr::from(([192, 0, 2, 10], 8443))]
        );
    }

    #[tokio::test]
    async fn tower_tcp_connector_calls_service() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connector = TowerTcpConnector::new(service_fn({
            let calls = calls.clone();
            move |request: TcpConnectRequest| {
                let calls = calls.clone();
                async move {
                    calls
                        .lock()
                        .expect("tcp calls")
                        .push((request.addr, request.timeout));
                    Ok::<_, WireError>(dummy_connection())
                }
            }
        }));

        connector
            .connect(
                make_call_context(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 8080)),
                Some(Duration::from_secs(5)),
            )
            .await
            .expect("tcp stream");

        assert_eq!(
            calls.lock().expect("tcp calls").as_slice(),
            &[(
                std::net::SocketAddr::from(([127, 0, 0, 1], 8080)),
                Some(Duration::from_secs(5))
            )]
        );
    }

    struct StaticTcpConnector;

    impl TcpConnector for StaticTcpConnector {
        fn connect(
            &self,
            _ctx: CallContext,
            addr: std::net::SocketAddr,
            timeout: Option<Duration>,
        ) -> crate::BoxFuture<Result<BoxConnection, WireError>> {
            Box::pin(async move {
                assert_eq!(addr, std::net::SocketAddr::from(([192, 0, 2, 20], 80)));
                assert_eq!(timeout, Some(Duration::from_secs(1)));
                Ok(dummy_connection())
            })
        }
    }

    #[tokio::test]
    async fn tcp_connector_service_calls_connector() {
        let mut service = TcpConnectorService::new(StaticTcpConnector);
        service
            .ready()
            .await
            .expect("service ready")
            .call(TcpConnectRequest::new(
                make_call_context(),
                std::net::SocketAddr::from(([192, 0, 2, 20], 80)),
                Some(Duration::from_secs(1)),
            ))
            .await
            .expect("tcp stream");
    }

    #[tokio::test]
    async fn tower_tls_connector_calls_service() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connector = TowerTlsConnector::new(service_fn({
            let calls = calls.clone();
            move |request: TlsConnectRequest| {
                let calls = calls.clone();
                async move {
                    calls
                        .lock()
                        .expect("tls calls")
                        .push(request.uri.to_string());
                    Ok::<_, WireError>(request.stream)
                }
            }
        }));

        connector
            .connect(
                make_call_context(),
                "https://tls.test/".parse().expect("uri"),
                dummy_connection(),
            )
            .await
            .expect("tls stream");

        assert_eq!(
            calls.lock().expect("tls calls").as_slice(),
            &["https://tls.test/".to_owned()]
        );
    }

    struct StaticTlsConnector;

    impl TlsConnector for StaticTlsConnector {
        fn connect(
            &self,
            _ctx: CallContext,
            uri: Uri,
            stream: BoxConnection,
        ) -> crate::BoxFuture<Result<BoxConnection, WireError>> {
            Box::pin(async move {
                assert_eq!(uri, "https://service.test/".parse::<Uri>().expect("uri"));
                Ok(stream)
            })
        }
    }

    #[tokio::test]
    async fn tls_connector_service_calls_connector() {
        let mut service = TlsConnectorService::new(StaticTlsConnector);
        service
            .ready()
            .await
            .expect("service ready")
            .call(TlsConnectRequest::new(
                make_call_context(),
                "https://service.test/".parse().expect("uri"),
                dummy_connection(),
            ))
            .await
            .expect("tls stream");
    }
}
