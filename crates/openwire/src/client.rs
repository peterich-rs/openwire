use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{select, Either};
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::rt::Timer;
use openwire_core::{
    BoxWireService, CallContext, DnsResolver, EventListenerFactory, Exchange, InterceptorLayer,
    NoopEventListenerFactory, RequestBody, ResponseBody, Runtime, SharedEventListenerFactory,
    SharedInterceptor, SharedTimer, TcpConnector, TlsConnector, WireError, WireExecutor,
};
use openwire_tokio::{SystemDnsResolver, TokioRuntime, TokioTcpConnector, TokioTimer};
use pin_project_lite::pin_project;
use tower::layer::Layer;
use tower::util::BoxCloneSyncService;
use tower::Service;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::auth::{Authenticator, SharedAuthenticator};
use crate::bridge::BridgeInterceptor;
use crate::connection::{
    Address, ConnectionPool, ExchangeFinder, PoolSettings, RequestAdmissionLimiter,
    RequestAdmissionPermit, RoutePlanner,
};
use crate::cookie::{CookieJar, SharedCookieJar};
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
    runtime: Arc<dyn Runtime>,
    call_timeout: Option<Duration>,
    proxy_selector: ProxySelector,
    request_admission: RequestAdmissionLimiter,
    service: BoxWireService,
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
    runtime: Arc<dyn Runtime>,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    transport: TransportConfig,
    policy: PolicyConfig,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    proxies: Vec<Proxy>,
    use_system_proxy: bool,
}

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

    pub fn runtime<R>(mut self, runtime: R) -> Self
    where
        R: Runtime + WireExecutor,
    {
        let runtime = Arc::new(runtime);
        self.runtime = runtime.clone();
        self.executor = runtime;
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
        self.policy.redirect.follow_redirects = enabled;
        self
    }

    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.policy.redirect.max_redirects = max_redirects;
        self
    }

    pub fn allow_insecure_redirects(mut self, enabled: bool) -> Self {
        self.policy.redirect.allow_insecure_redirects = enabled;
        self
    }

    pub fn retry_on_connection_failure(mut self, enabled: bool) -> Self {
        self.policy.retry.retry_on_connection_failure = enabled;
        self
    }

    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.policy.retry.max_retries = max_retries;
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
        self.policy.retry.retry_canceled_requests = enabled;
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
            route_planner: RoutePlanner::default(),
            proxy_authenticator: self.policy.auth.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.policy.auth.max_auth_attempts,
        };

        let transport = TransportService::new(
            connector,
            self.transport.clone(),
            self.runtime.clone(),
            self.executor.clone(),
            self.timer.clone(),
            exchange_finder,
            request_admission.clone(),
        );
        let service = build_service_chain(
            transport,
            self.application_interceptors,
            self.network_interceptors,
            self.policy.clone(),
            proxy_selector.clone(),
        );

        Ok(Client {
            inner: Arc::new(ClientInner {
                event_listener_factory: self.event_listener_factory,
                runtime: self.runtime,
                call_timeout: self.policy.call_timeout,
                proxy_selector,
                request_admission,
                service,
            }),
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        let runtime = Arc::new(TokioRuntime);
        Self {
            application_interceptors: Vec::new(),
            network_interceptors: Vec::new(),
            event_listener_factory: Arc::new(NoopEventListenerFactory),
            runtime: runtime.clone(),
            executor: runtime,
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
                retry: RetryPolicyConfig {
                    retry_on_connection_failure: true,
                    max_retries: 1,
                    retry_canceled_requests: false,
                },
                redirect: RedirectPolicyConfig {
                    follow_redirects: true,
                    max_redirects: 10,
                    allow_insecure_redirects: false,
                },
            },
            dns_resolver: Arc::new(SystemDnsResolver),
            tcp_connector: Arc::new(TokioTcpConnector),
            tls_connector: None,
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

    pub fn runtime(&self) -> Arc<dyn Runtime> {
        self.inner.runtime.clone()
    }

    pub async fn execute(
        &self,
        request: Request<RequestBody>,
    ) -> Result<Response<ResponseBody>, WireError> {
        self.new_call(request).execute().await
    }
}

impl Call {
    pub async fn execute(self) -> Result<Response<ResponseBody>, WireError> {
        let request_address = Address::from_uri(
            self.request.uri(),
            self.client.inner.proxy_selector.select(self.request.uri()),
        )?;
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
            let request_admission = self.client.inner.request_admission.clone();
            let execute = async move {
                tower::ServiceExt::ready(&mut service)
                    .await
                    .map_err(|error| WireError::internal("service chain not ready", error))?;
                let permit = request_admission.acquire(request_address).await?;
                let response = service
                    .call(Exchange::new(self.request, execute_ctx, 1))
                    .await?;
                Ok(attach_request_admission(response, permit))
            };

            let result =
                with_call_deadline(self.client.inner.runtime.clone(), ctx.deadline(), execute)
                    .await;

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
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    policy: PolicyConfig,
    proxy_selector: ProxySelector,
) -> BoxWireService {
    let mut network: BoxWireService = BoxCloneSyncService::new(transport);
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

async fn with_call_deadline<F>(
    runtime: Arc<dyn Runtime>,
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
    let sleep = runtime.sleep(timeout);

    match select(future, sleep).await {
        Either::Left((result, _sleep)) => result,
        Either::Right((_ready, _future)) => Err(WireError::timeout(format!(
            "call timed out after {timeout:?}"
        ))),
    }
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
