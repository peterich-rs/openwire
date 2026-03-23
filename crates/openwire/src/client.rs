use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{select, Either};
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::rt::Timer;
use openwire_core::{
    Authenticator, BoxTaskHandle, BoxWireService, CallContext, CookieJar, DnsResolver,
    EventListenerFactory, Exchange, InterceptorLayer, NoopEventListenerFactory, RedirectPolicy,
    RequestBody, ResponseBody, RetryPolicy, SharedEventListenerFactory, SharedInterceptor,
    SharedTimer, TcpConnector, TlsConnector, WireError, WireExecutor,
};
use openwire_tokio::{SystemDnsResolver, TokioExecutor, TokioTcpConnector, TokioTimer};
use pin_project_lite::pin_project;
use tower::layer::Layer;
use tower::util::BoxCloneSyncService;
use tower::Service;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::auth::SharedAuthenticator;
use crate::bridge::BridgeInterceptor;
use crate::connection::{
    Address, ConnectionPool, DefaultRoutePlanner, ExchangeFinder, PoolSettings,
    RequestAdmissionLimiter, RequestAdmissionPermit, RoutePlanner,
};
use crate::cookie::SharedCookieJar;
use crate::policy::{
    AuthPolicyConfig, FollowUpPolicyService, PolicyConfig, RedirectPolicyConfig, RetryPolicyConfig,
};
use crate::proxy::{system_proxies_from_env, Proxy, ProxySelector};
use crate::transport::{ConnectorStack, TransportService};

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    event_listener_factory: SharedEventListenerFactory,
    timer: SharedTimer,
    call_timeout: Option<Duration>,
    service: BoxWireService,
    pool_reaper_task: Option<BoxTaskHandle>,
}

pub struct Call {
    client: Client,
    request: Request<RequestBody>,
}

#[derive(Clone)]
pub(crate) struct TransportConfig {
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) pool_idle_timeout: Option<Duration>,
    pub(crate) pool_max_idle_per_host: usize,
    pub(crate) http2_keep_alive_interval: Option<Duration>,
    pub(crate) http2_keep_alive_while_idle: bool,
    pub(crate) max_connections_total: usize,
    pub(crate) max_connections_per_host: usize,
    pub(crate) max_requests_total: usize,
    pub(crate) max_requests_per_host: usize,
}

pub struct ClientBuilder {
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    event_listener_factory: SharedEventListenerFactory,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    transport: TransportConfig,
    policy: PolicyConfig,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    route_planner: Arc<dyn RoutePlanner>,
    proxies: Vec<Proxy>,
    use_system_proxy: bool,
}

const MIN_POOL_REAPER_CADENCE: Duration = Duration::from_secs(5);
const MAX_POOL_REAPER_CADENCE: Duration = Duration::from_secs(60);

impl ClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn application_interceptor<I>(mut self, interceptor: I) -> Self
    where
        I: openwire_core::Interceptor,
    {
        self.application_interceptors.push(Arc::new(interceptor));
        self
    }

    pub fn network_interceptor<I>(mut self, interceptor: I) -> Self
    where
        I: openwire_core::Interceptor,
    {
        self.network_interceptors.push(Arc::new(interceptor));
        self
    }

    pub fn event_listener_factory<F>(mut self, factory: F) -> Self
    where
        F: EventListenerFactory,
    {
        self.event_listener_factory = Arc::new(factory);
        self
    }

    pub fn executor<E>(mut self, executor: E) -> Self
    where
        E: WireExecutor,
    {
        self.executor = Arc::new(executor);
        self
    }

    pub fn timer<T>(mut self, timer: T) -> Self
    where
        T: Timer + Send + Sync + 'static,
    {
        self.timer = SharedTimer::new(timer);
        self
    }

    pub fn dns_resolver<R>(mut self, resolver: R) -> Self
    where
        R: DnsResolver,
    {
        self.dns_resolver = Arc::new(resolver);
        self
    }

    pub fn tcp_connector<C>(mut self, connector: C) -> Self
    where
        C: TcpConnector,
    {
        self.tcp_connector = Arc::new(connector);
        self
    }

    pub fn tls_connector<T>(mut self, connector: T) -> Self
    where
        T: TlsConnector,
    {
        self.tls_connector = Some(Arc::new(connector));
        self
    }

    pub fn route_planner<P>(mut self, planner: P) -> Self
    where
        P: RoutePlanner,
    {
        self.route_planner = Arc::new(planner);
        self
    }

    /// Configures a cookie jar for automatic cookie loading and persistence.
    pub fn cookie_jar<J>(mut self, jar: J) -> Self
    where
        J: CookieJar,
    {
        self.policy.cookie_jar = Some(Arc::new(jar) as SharedCookieJar);
        self
    }

    /// Adds a proxy rule to the client.
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxies.push(proxy);
        self
    }

    /// Opts into reading proxy rules from standard environment variables.
    pub fn use_system_proxy(mut self, enabled: bool) -> Self {
        self.use_system_proxy = enabled;
        self
    }

    pub fn authenticator<A>(mut self, authenticator: A) -> Self
    where
        A: Authenticator,
    {
        self.policy.auth.authenticator = Some(Arc::new(authenticator) as SharedAuthenticator);
        self
    }

    pub fn proxy_authenticator<A>(mut self, authenticator: A) -> Self
    where
        A: Authenticator,
    {
        self.policy.auth.proxy_authenticator = Some(Arc::new(authenticator) as SharedAuthenticator);
        self
    }

    /// Sets the maximum number of automatic auth follow-ups per logical call.
    pub fn max_auth_attempts(mut self, max_auth_attempts: usize) -> Self {
        self.policy.auth.max_auth_attempts = max_auth_attempts;
        self
    }

    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.policy.call_timeout = Some(timeout);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.transport.connect_timeout = Some(timeout);
        self
    }

    pub fn follow_redirects(mut self, enabled: bool) -> Self {
        self.policy
            .redirect
            .default_mut()
            .set_follow_redirects(enabled);
        self
    }

    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.policy
            .redirect
            .default_mut()
            .set_max_redirects(max_redirects);
        self
    }

    pub fn allow_insecure_redirects(mut self, enabled: bool) -> Self {
        self.policy
            .redirect
            .default_mut()
            .set_allow_insecure_redirects(enabled);
        self
    }

    pub fn retry_on_connection_failure(mut self, enabled: bool) -> Self {
        self.policy
            .retry
            .default_mut()
            .set_retry_on_connection_failure(enabled);
        self
    }

    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.policy.retry.default_mut().set_max_retries(max_retries);
        self
    }

    pub fn pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.transport.pool_idle_timeout = Some(timeout);
        self
    }

    pub fn pool_max_idle_per_host(mut self, max_idle: usize) -> Self {
        self.transport.pool_max_idle_per_host = max_idle;
        self
    }

    pub fn http2_keep_alive_interval(mut self, interval: Duration) -> Self {
        self.transport.http2_keep_alive_interval = Some(interval);
        self
    }

    pub fn http2_keep_alive_while_idle(mut self, enabled: bool) -> Self {
        self.transport.http2_keep_alive_while_idle = enabled;
        self
    }

    pub fn max_connections_total(mut self, max_connections: usize) -> Self {
        self.transport.max_connections_total = max_connections;
        self
    }

    pub fn max_connections_per_host(mut self, max_connections: usize) -> Self {
        self.transport.max_connections_per_host = max_connections;
        self
    }

    pub fn max_requests_total(mut self, max_requests: usize) -> Self {
        self.transport.max_requests_total = max_requests;
        self
    }

    pub fn max_requests_per_host(mut self, max_requests: usize) -> Self {
        self.transport.max_requests_per_host = max_requests;
        self
    }

    pub fn retry_canceled_requests(mut self, enabled: bool) -> Self {
        self.policy
            .retry
            .default_mut()
            .set_retry_canceled_requests(enabled);
        self
    }

    pub fn retry_policy<P>(mut self, policy: P) -> Self
    where
        P: RetryPolicy,
    {
        self.policy.retry.set_custom(policy);
        self
    }

    pub fn redirect_policy<P>(mut self, policy: P) -> Self
    where
        P: RedirectPolicy,
    {
        self.policy.redirect.set_custom(policy);
        self
    }

    pub fn build(self) -> Result<Client, WireError> {
        #[cfg(feature = "tls-rustls")]
        let tls_connector = match self.tls_connector {
            Some(tls_connector) => Some(tls_connector),
            None => Some(
                Arc::new(openwire_rustls::RustlsTlsConnector::builder().build()?)
                    as Arc<dyn TlsConnector>,
            ),
        };

        #[cfg(not(feature = "tls-rustls"))]
        let tls_connector = self.tls_connector;

        let mut proxies = self.proxies;
        if self.use_system_proxy {
            proxies.extend(system_proxies_from_env()?);
        }

        let pool = Arc::new(ConnectionPool::new(PoolSettings {
            idle_timeout: self.transport.pool_idle_timeout,
            max_idle_per_address: self.transport.pool_max_idle_per_host,
        }));
        let pool_reaper_task = spawn_pool_reaper(self.executor.clone(), self.timer.clone(), &pool)?;
        let proxy_selector = ProxySelector::new(proxies);
        let request_admission = RequestAdmissionLimiter::new(
            self.transport.max_requests_total,
            self.transport.max_requests_per_host,
        );
        let exchange_finder = Arc::new(ExchangeFinder::new(pool, proxy_selector.clone()));
        let connector = ConnectorStack {
            dns_resolver: self.dns_resolver,
            tcp_connector: self.tcp_connector,
            tls_connector,
            connect_timeout: self.transport.connect_timeout,
            executor: self.executor.clone(),
            timer: self.timer.clone(),
            route_planner: self.route_planner,
            proxy_authenticator: self.policy.auth.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.policy.auth.max_auth_attempts,
        };

        let transport = TransportService::new(
            connector,
            self.transport.clone(),
            self.executor.clone(),
            self.timer.clone(),
            exchange_finder,
        );
        let service = build_service_chain(
            transport,
            request_admission,
            self.application_interceptors,
            self.network_interceptors,
            self.policy.clone(),
            proxy_selector.clone(),
        );

        Ok(Client {
            inner: Arc::new(ClientInner {
                event_listener_factory: self.event_listener_factory,
                timer: self.timer,
                call_timeout: self.policy.call_timeout,
                service,
                pool_reaper_task,
            }),
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            application_interceptors: Vec::new(),
            network_interceptors: Vec::new(),
            event_listener_factory: Arc::new(NoopEventListenerFactory),
            executor: Arc::new(TokioExecutor::new()),
            timer: SharedTimer::new(TokioTimer::new()),
            transport: TransportConfig {
                connect_timeout: None,
                pool_idle_timeout: Some(Duration::from_secs(90)),
                pool_max_idle_per_host: usize::MAX,
                http2_keep_alive_interval: None,
                http2_keep_alive_while_idle: false,
                max_connections_total: usize::MAX,
                max_connections_per_host: usize::MAX,
                max_requests_total: usize::MAX,
                max_requests_per_host: usize::MAX,
            },
            policy: PolicyConfig {
                call_timeout: None,
                cookie_jar: None,
                auth: AuthPolicyConfig {
                    authenticator: None,
                    proxy_authenticator: None,
                    max_auth_attempts: 3,
                },
                retry: RetryPolicyConfig::default(),
                redirect: RedirectPolicyConfig::default(),
            },
            dns_resolver: Arc::new(SystemDnsResolver),
            tcp_connector: Arc::new(TokioTcpConnector),
            tls_connector: None,
            route_planner: Arc::new(DefaultRoutePlanner::default()),
            proxies: Vec::new(),
            use_system_proxy: false,
        }
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub fn new_call(&self, request: Request<RequestBody>) -> Call {
        Call {
            client: self.clone(),
            request,
        }
    }

    pub async fn execute(
        &self,
        request: Request<RequestBody>,
    ) -> Result<Response<ResponseBody>, WireError> {
        self.new_call(request).execute().await
    }
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Some(handle) = self.pool_reaper_task.take() {
            handle.abort();
        }
    }
}

impl Call {
    pub async fn execute(self) -> Result<Response<ResponseBody>, WireError> {
        let ctx = CallContext::from_factory(
            &self.client.inner.event_listener_factory,
            &self.request,
            self.client.inner.call_timeout,
        );

        let span = tracing::info_span!(
            "openwire.call",
            call_id = ctx.call_id().as_u64(),
            method = %self.request.method(),
            uri = %self.request.uri(),
        );

        async move {
            ctx.listener().call_start(&ctx, &self.request);

            let mut service = self.client.inner.service.clone();
            let execute_ctx = ctx.clone();
            let execute = async move {
                tower::ServiceExt::ready(&mut service)
                    .await
                    .map_err(|error| WireError::internal("service chain not ready", error))?;
                service
                    .call(Exchange::new(self.request, execute_ctx, 1))
                    .await
            };

            let result =
                with_call_deadline(self.client.inner.timer.clone(), ctx.deadline(), execute).await;

            match result {
                Ok(response) => {
                    ctx.listener().call_end(&ctx, &response);
                    Ok(response)
                }
                Err(error) => {
                    ctx.listener().call_failed(&ctx, &error);
                    Err(error)
                }
            }
        }
        .instrument(span)
        .with_current_subscriber()
        .await
    }
}

fn attach_request_admission(
    response: Response<ResponseBody>,
    permit: RequestAdmissionPermit,
) -> Response<ResponseBody> {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        ResponseBody::new(
            RequestAdmissionBody {
                inner: body,
                _permit: Some(permit),
            }
            .boxed(),
        ),
    )
}

fn build_service_chain(
    transport: TransportService,
    request_admission: RequestAdmissionLimiter,
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    policy: PolicyConfig,
    proxy_selector: ProxySelector,
) -> BoxWireService {
    let mut network: BoxWireService = BoxCloneSyncService::new(transport);
    network = BoxCloneSyncService::new(
        RequestAdmissionLayer::new(request_admission, proxy_selector.clone()).layer(network),
    );
    for interceptor in network_interceptors.iter().rev() {
        network =
            BoxCloneSyncService::new(InterceptorLayer::new(interceptor.clone()).layer(network));
    }
    network = BoxCloneSyncService::new(
        InterceptorLayer::new(Arc::new(BridgeInterceptor) as SharedInterceptor).layer(network),
    );

    let mut service: BoxWireService =
        BoxCloneSyncService::new(FollowUpPolicyService::new(network, policy, proxy_selector));
    for interceptor in application_interceptors.iter().rev() {
        service =
            BoxCloneSyncService::new(InterceptorLayer::new(interceptor.clone()).layer(service));
    }

    service
}

#[derive(Clone)]
struct RequestAdmissionLayer {
    limiter: RequestAdmissionLimiter,
    proxy_selector: ProxySelector,
}

impl RequestAdmissionLayer {
    fn new(limiter: RequestAdmissionLimiter, proxy_selector: ProxySelector) -> Self {
        Self {
            limiter,
            proxy_selector,
        }
    }
}

impl<S> Layer<S> for RequestAdmissionLayer
where
    S: Service<Exchange, Response = Response<ResponseBody>, Error = WireError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    type Service = RequestAdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestAdmissionService {
            inner,
            limiter: self.limiter.clone(),
            proxy_selector: self.proxy_selector.clone(),
        }
    }
}

#[derive(Clone)]
struct RequestAdmissionService<S> {
    inner: S,
    limiter: RequestAdmissionLimiter,
    proxy_selector: ProxySelector,
}

impl<S> Service<Exchange> for RequestAdmissionService<S>
where
    S: Service<Exchange, Response = Response<ResponseBody>, Error = WireError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<ResponseBody>;
    type Error = WireError;
    type Future = openwire_core::BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            std::task::Poll::Ready(Ok(())) => self.limiter.poll_ready(cx),
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(Err(error)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let limiter = self.limiter.clone();
        let proxy_selector = self.proxy_selector.clone();
        Box::pin(async move {
            let request_address = Address::from_uri(
                exchange.request().uri(),
                proxy_selector.select(exchange.request().uri()),
            )?;
            let permit = limiter.acquire(request_address).await?;
            let response = inner.call(exchange).await?;
            Ok(attach_request_admission(response, permit))
        })
    }
}

async fn with_call_deadline<F>(
    timer: SharedTimer,
    deadline: Option<std::time::Instant>,
    future: F,
) -> Result<Response<ResponseBody>, WireError>
where
    F: std::future::Future<Output = Result<Response<ResponseBody>, WireError>>,
{
    let Some(deadline) = deadline else {
        return future.await;
    };

    let timeout = deadline.saturating_duration_since(std::time::Instant::now());
    let future = Box::pin(future);
    let sleep = timer.sleep(timeout);

    match select(future, sleep).await {
        Either::Left((result, _sleep)) => result,
        Either::Right((_ready, _future)) => Err(WireError::timeout(format!(
            "call timed out after {timeout:?}"
        ))),
    }
}

fn spawn_pool_reaper(
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    pool: &Arc<ConnectionPool>,
) -> Result<Option<BoxTaskHandle>, WireError> {
    let Some(idle_timeout) = pool.settings().idle_timeout else {
        return Ok(None);
    };

    let cadence = pool_reaper_cadence(idle_timeout);
    let weak_pool = Arc::downgrade(pool);
    executor
        .spawn(Box::pin(async move {
            loop {
                timer.sleep(cadence).await;
                let Some(pool) = weak_pool.upgrade() else {
                    break;
                };
                pool.prune_all();
            }
        }))
        .map(Some)
}

fn pool_reaper_cadence(idle_timeout: Duration) -> Duration {
    (idle_timeout / 2).clamp(MIN_POOL_REAPER_CADENCE, MAX_POOL_REAPER_CADENCE)
}

pin_project! {
    struct RequestAdmissionBody {
        #[pin]
        inner: ResponseBody,
        _permit: Option<RequestAdmissionPermit>,
    }
}

impl Body for RequestAdmissionBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use http::Uri;
    use openwire_core::{BoxConnection, BoxFuture, TaskHandle};

    use super::{pool_reaper_cadence, spawn_pool_reaper, Client, ConnectionPool, PoolSettings};
    struct CountingTaskHandle {
        aborts: Arc<AtomicUsize>,
    }

    impl TaskHandle for CountingTaskHandle {
        fn abort(&self) {
            self.aborts.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Clone, Default)]
    struct CountingExecutor {
        spawns: Arc<AtomicUsize>,
        aborts: Arc<AtomicUsize>,
    }

    impl CountingExecutor {
        fn spawns(&self) -> usize {
            self.spawns.load(Ordering::Relaxed)
        }

        fn aborts(&self) -> usize {
            self.aborts.load(Ordering::Relaxed)
        }
    }

    impl openwire_core::WireExecutor for CountingExecutor {
        fn spawn(
            &self,
            _future: BoxFuture<()>,
        ) -> Result<openwire_core::BoxTaskHandle, openwire_core::WireError> {
            self.spawns.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(CountingTaskHandle {
                aborts: self.aborts.clone(),
            }))
        }
    }

    #[derive(Clone)]
    struct PassthroughTlsConnector;

    impl openwire_core::TlsConnector for PassthroughTlsConnector {
        fn connect(
            &self,
            _ctx: openwire_core::CallContext,
            _uri: Uri,
            stream: BoxConnection,
        ) -> BoxFuture<Result<BoxConnection, openwire_core::WireError>> {
            Box::pin(async move { Ok(stream) })
        }
    }

    #[test]
    fn pool_reaper_cadence_is_clamped() {
        assert_eq!(
            pool_reaper_cadence(Duration::from_secs(2)),
            Duration::from_secs(5)
        );
        assert_eq!(
            pool_reaper_cadence(Duration::from_secs(20)),
            Duration::from_secs(10)
        );
        assert_eq!(
            pool_reaper_cadence(Duration::from_secs(180)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn spawn_pool_reaper_skips_when_idle_timeout_is_disabled() {
        let executor = CountingExecutor::default();
        let timer = openwire_core::SharedTimer::new(openwire_tokio::TokioTimer::new());
        let pool = Arc::new(ConnectionPool::new(PoolSettings {
            idle_timeout: None,
            max_idle_per_address: usize::MAX,
        }));

        let handle =
            spawn_pool_reaper(Arc::new(executor.clone()), timer, &pool).expect("spawn reaper");

        assert!(handle.is_none());
        assert_eq!(executor.spawns(), 0);
        assert_eq!(executor.aborts(), 0);
    }

    #[test]
    fn dropping_final_client_aborts_pool_reaper_task() {
        let executor = CountingExecutor::default();
        let client = Client::builder()
            .executor(executor.clone())
            .tls_connector(PassthroughTlsConnector)
            .build()
            .expect("build client");

        assert_eq!(executor.spawns(), 1);
        assert_eq!(executor.aborts(), 0);

        drop(client);

        assert_eq!(executor.aborts(), 1);
    }
}
