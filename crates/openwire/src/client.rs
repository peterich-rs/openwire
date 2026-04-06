use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{select, Either};
use http::header::PROXY_AUTHORIZATION;
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
    Address, CachedAddresses, ConnectionPool, DefaultRoutePlanner, ExchangeFinder, PoolSettings,
    RequestAdmissionLimiter, RequestAdmissionPermit, ResolvedAddress, RoutePlanner,
};
use crate::cookie::SharedCookieJar;
use crate::policy::{
    AuthPolicyConfig, FollowUpPolicyService, PolicyConfig, RedirectPolicyConfig, RetryPolicyConfig,
};
use crate::proxy::{
    resolved_proxy_candidates, ProxyRules, ProxySelector, SelectedProxy, SharedProxySelector,
};
use crate::sync_util::lock_mutex;
use crate::transport::{ConnectorStack, TransportService, TransportServiceInit};

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    event_listener_factory: SharedEventListenerFactory,
    timer: SharedTimer,
    request_config: EffectiveRequestConfig,
    service: BoxWireService,
    pool_reaper: Arc<PoolReaperController>,
}

pub struct Call {
    client: Client,
    request: Request<RequestBody>,
    options: CallOptions,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallOptions {
    call_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    follow_redirects: Option<bool>,
    max_redirects: Option<usize>,
    retry_on_connection_failure: Option<bool>,
    max_retries: Option<usize>,
    retry_canceled_requests: Option<bool>,
    allow_insecure_redirects: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveRequestConfig {
    pub(crate) call_timeout: Option<Duration>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) follow_redirects: bool,
    pub(crate) max_redirects: usize,
    pub(crate) retry_on_connection_failure: bool,
    pub(crate) max_retries: usize,
    pub(crate) retry_canceled_requests: bool,
    pub(crate) allow_insecure_redirects: bool,
}

pub struct ClientBuilder {
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    event_listener_factory: SharedEventListenerFactory,
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    call_timeout: Option<Duration>,
    transport: TransportConfig,
    policy: PolicyConfig,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
    route_planner: Arc<dyn RoutePlanner>,
    proxy_selector: SharedProxySelector,
}

const MIN_POOL_REAPER_CADENCE: Duration = Duration::from_secs(5);
const MAX_POOL_REAPER_CADENCE: Duration = Duration::from_secs(60);

#[derive(Default)]
struct PoolReaperController {
    state: Mutex<PoolReaperState>,
}

#[derive(Default)]
struct PoolReaperState {
    handle: Option<BoxTaskHandle>,
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

    /// Configures how proxies are resolved for each request attempt.
    pub fn proxy_selector<S>(mut self, selector: S) -> Self
    where
        S: ProxySelector,
    {
        self.proxy_selector = Arc::new(selector);
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
        self.call_timeout = Some(timeout);
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

        let request_config = EffectiveRequestConfig::from_defaults(
            self.call_timeout,
            self.transport.connect_timeout,
            &self.policy,
        );

        let pool = Arc::new(ConnectionPool::new(PoolSettings {
            idle_timeout: self.transport.pool_idle_timeout,
            max_idle_per_address: self.transport.pool_max_idle_per_host,
        }));
        let pool_reaper = Arc::new(PoolReaperController::default());
        let on_pooled_connection_published = if pool.settings().idle_timeout.is_some() {
            let reaper = pool_reaper.clone();
            let executor = self.executor.clone();
            let timer = self.timer.clone();
            let weak_pool = Arc::downgrade(&pool);
            Some(Arc::new(move || {
                reaper.ensure_started(executor.clone(), timer.clone(), weak_pool.clone());
            }) as Arc<dyn Fn() + Send + Sync>)
        } else {
            None
        };
        let proxy_selector = self.proxy_selector;
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

        let transport = TransportService::new(TransportServiceInit {
            connector,
            config: self.transport.clone(),
            executor: self.executor.clone(),
            timer: self.timer.clone(),
            exchange_finder,
            request_admission,
            proxy_selector: proxy_selector.clone(),
            on_pooled_connection_published,
        });
        let service = build_service_chain(
            transport,
            self.application_interceptors,
            self.network_interceptors,
            self.policy.clone(),
        );

        Ok(Client {
            inner: Arc::new(ClientInner {
                event_listener_factory: self.event_listener_factory,
                timer: self.timer,
                request_config,
                service,
                pool_reaper,
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
            call_timeout: None,
            transport: TransportConfig {
                connect_timeout: None,
                pool_idle_timeout: Some(Duration::from_secs(300)),
                pool_max_idle_per_host: 5,
                http2_keep_alive_interval: None,
                http2_keep_alive_while_idle: false,
                max_connections_total: usize::MAX,
                max_connections_per_host: usize::MAX,
                max_requests_total: 64,
                max_requests_per_host: 5,
            },
            policy: PolicyConfig {
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
            proxy_selector: Arc::new(ProxyRules::new()),
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
            options: CallOptions::default(),
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
        self.pool_reaper.abort();
    }
}

impl PoolReaperController {
    fn ensure_started(
        &self,
        executor: Arc<dyn WireExecutor>,
        timer: SharedTimer,
        pool: Weak<ConnectionPool>,
    ) {
        let mut state = lock_mutex(&self.state);
        if state.handle.is_some() {
            return;
        }

        let Some(pool) = pool.upgrade() else {
            return;
        };

        match spawn_pool_reaper(executor, timer, &pool) {
            Ok(handle) => state.handle = handle,
            Err(error) => tracing::warn!(%error, "failed to start pool reaper task"),
        }
    }

    fn abort(&self) {
        if let Some(handle) = lock_mutex(&self.state).handle.take() {
            handle.abort();
        }
    }
}

impl Call {
    pub fn options(mut self, options: CallOptions) -> Self {
        self.options.apply(options);
        self
    }

    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.options.call_timeout = Some(timeout);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.options.connect_timeout = Some(timeout);
        self
    }

    pub fn follow_redirects(mut self, enabled: bool) -> Self {
        self.options.follow_redirects = Some(enabled);
        self
    }

    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.options.max_redirects = Some(max_redirects);
        self
    }

    pub fn retry_on_connection_failure(mut self, enabled: bool) -> Self {
        self.options.retry_on_connection_failure = Some(enabled);
        self
    }

    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.options.max_retries = Some(max_retries);
        self
    }

    pub fn retry_canceled_requests(mut self, enabled: bool) -> Self {
        self.options.retry_canceled_requests = Some(enabled);
        self
    }

    pub fn allow_insecure_redirects(mut self, enabled: bool) -> Self {
        self.options.allow_insecure_redirects = Some(enabled);
        self
    }

    pub async fn execute(mut self) -> Result<Response<ResponseBody>, WireError> {
        let request_config = self
            .client
            .inner
            .request_config
            .with_overrides(self.options);
        self.request.extensions_mut().insert(request_config);
        self.request.extensions_mut().insert(self.options);
        let ctx = CallContext::from_factory(
            &self.client.inner.event_listener_factory,
            &self.request,
            request_config.call_timeout,
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

pub(crate) fn attach_request_admission(
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
        BoxCloneSyncService::new(FollowUpPolicyService::new(network, policy));
    for interceptor in application_interceptors.iter().rev() {
        service =
            BoxCloneSyncService::new(InterceptorLayer::new(interceptor.clone()).layer(service));
    }

    service
}

impl CallOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = Some(timeout);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    pub fn follow_redirects(mut self, enabled: bool) -> Self {
        self.follow_redirects = Some(enabled);
        self
    }

    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.max_redirects = Some(max_redirects);
        self
    }

    pub fn retry_on_connection_failure(mut self, enabled: bool) -> Self {
        self.retry_on_connection_failure = Some(enabled);
        self
    }

    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    pub fn retry_canceled_requests(mut self, enabled: bool) -> Self {
        self.retry_canceled_requests = Some(enabled);
        self
    }

    pub fn allow_insecure_redirects(mut self, enabled: bool) -> Self {
        self.allow_insecure_redirects = Some(enabled);
        self
    }

    pub(crate) fn has_retry_overrides(self) -> bool {
        self.retry_on_connection_failure.is_some()
            || self.max_retries.is_some()
            || self.retry_canceled_requests.is_some()
    }

    pub(crate) fn has_redirect_overrides(self) -> bool {
        self.follow_redirects.is_some()
            || self.max_redirects.is_some()
            || self.allow_insecure_redirects.is_some()
    }

    fn apply(&mut self, other: Self) {
        self.call_timeout = other.call_timeout.or(self.call_timeout);
        self.connect_timeout = other.connect_timeout.or(self.connect_timeout);
        self.follow_redirects = other.follow_redirects.or(self.follow_redirects);
        self.max_redirects = other.max_redirects.or(self.max_redirects);
        self.retry_on_connection_failure = other
            .retry_on_connection_failure
            .or(self.retry_on_connection_failure);
        self.max_retries = other.max_retries.or(self.max_retries);
        self.retry_canceled_requests = other
            .retry_canceled_requests
            .or(self.retry_canceled_requests);
        self.allow_insecure_redirects = other
            .allow_insecure_redirects
            .or(self.allow_insecure_redirects);
    }
}

impl EffectiveRequestConfig {
    fn from_defaults(
        call_timeout: Option<Duration>,
        connect_timeout: Option<Duration>,
        policy: &PolicyConfig,
    ) -> Self {
        let retry = policy.retry.default_config();
        let redirect = policy.redirect.default_config();
        Self {
            call_timeout,
            connect_timeout,
            follow_redirects: redirect.follow_redirects(),
            max_redirects: redirect.max_redirects(),
            retry_on_connection_failure: retry.retry_on_connection_failure(),
            max_retries: retry.max_retries(),
            retry_canceled_requests: retry.retry_canceled_requests(),
            allow_insecure_redirects: redirect.allow_insecure_redirects(),
        }
    }

    fn with_overrides(self, options: CallOptions) -> Self {
        Self {
            call_timeout: options.call_timeout.or(self.call_timeout),
            connect_timeout: options.connect_timeout.or(self.connect_timeout),
            follow_redirects: options.follow_redirects.unwrap_or(self.follow_redirects),
            max_redirects: options.max_redirects.unwrap_or(self.max_redirects),
            retry_on_connection_failure: options
                .retry_on_connection_failure
                .unwrap_or(self.retry_on_connection_failure),
            max_retries: options.max_retries.unwrap_or(self.max_retries),
            retry_canceled_requests: options
                .retry_canceled_requests
                .unwrap_or(self.retry_canceled_requests),
            allow_insecure_redirects: options
                .allow_insecure_redirects
                .unwrap_or(self.allow_insecure_redirects),
        }
    }
}

pub(crate) fn cache_request_addresses(
    request: &mut Request<RequestBody>,
    proxy_selector: &dyn ProxySelector,
) -> Result<Arc<[ResolvedAddress]>, WireError> {
    let previous_selected_proxy = request.extensions().get::<SelectedProxy>().cloned();
    let candidates = resolved_proxy_candidates(
        proxy_selector.select(request.uri())?,
        previous_selected_proxy.clone(),
    );
    let next_selected_proxy = candidates.first().cloned().flatten();
    if previous_selected_proxy.is_some() && previous_selected_proxy != next_selected_proxy {
        request.headers_mut().remove(PROXY_AUTHORIZATION);
    }

    let mut addresses = Vec::new();
    for candidate in candidates {
        let resolved = ResolvedAddress::new(
            Address::from_uri(request.uri(), candidate.as_ref())?,
            candidate,
        );
        if !addresses.iter().any(|existing: &ResolvedAddress| {
            existing.address() == resolved.address()
                && existing.selected_proxy() == resolved.selected_proxy()
        }) {
            addresses.push(resolved);
        }
    }
    let addresses = Arc::<[ResolvedAddress]>::from(addresses);
    let extensions = request.extensions_mut();
    extensions.insert(CachedAddresses(addresses.clone()));
    Ok(addresses)
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

    use http::header::PROXY_AUTHORIZATION;
    use http::Request;
    use openwire_core::{BoxFuture, RequestBody, TaskHandle, WireError};

    use super::{
        cache_request_addresses, pool_reaper_cadence, spawn_pool_reaper, CallOptions,
        ClientBuilder, ConnectionPool, EffectiveRequestConfig, PoolReaperController, PoolSettings,
    };
    use crate::connection::CachedAddresses;
    use crate::proxy::{Proxy, ProxyRules, SelectedProxy};
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

    #[derive(Clone, Default)]
    struct FailOnceExecutor {
        spawns: Arc<AtomicUsize>,
        aborts: Arc<AtomicUsize>,
    }

    impl FailOnceExecutor {
        fn spawns(&self) -> usize {
            self.spawns.load(Ordering::Relaxed)
        }

        fn aborts(&self) -> usize {
            self.aborts.load(Ordering::Relaxed)
        }
    }

    impl openwire_core::WireExecutor for FailOnceExecutor {
        fn spawn(&self, _future: BoxFuture<()>) -> Result<openwire_core::BoxTaskHandle, WireError> {
            let attempt = self.spawns.fetch_add(1, Ordering::Relaxed);
            if attempt == 0 {
                return Err(WireError::internal(
                    "scripted spawn failure",
                    std::io::Error::other("scripted spawn failure"),
                ));
            }

            Ok(Box::new(CountingTaskHandle {
                aborts: self.aborts.clone(),
            }))
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
        let timer = openwire_core::SharedTimer::new(openwire_tokio::TokioTimer::new());
        let pool = Arc::new(ConnectionPool::new(PoolSettings::default()));
        let reaper = PoolReaperController::default();

        reaper.ensure_started(Arc::new(executor.clone()), timer, Arc::downgrade(&pool));
        assert_eq!(executor.spawns(), 1);
        assert_eq!(executor.aborts(), 0);

        reaper.abort();
        assert_eq!(executor.aborts(), 1);
    }

    #[test]
    fn cache_request_addresses_inserts_cached_extension() {
        let mut request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");

        let addresses =
            cache_request_addresses(&mut request, &ProxyRules::new()).expect("addresses");

        assert_eq!(
            request
                .extensions()
                .get::<CachedAddresses>()
                .map(|cached| cached.0.clone()),
            Some(addresses)
        );
    }

    #[test]
    fn cache_request_addresses_preserve_sticky_proxy_authorization() {
        let first_proxy = Proxy::http("http://first.test:8080").expect("first proxy");
        let second_proxy = Proxy::http("http://second.test:8080").expect("second proxy");
        let mut request = Request::builder()
            .uri("http://example.com/resource")
            .header(PROXY_AUTHORIZATION, "Basic cHJveHk6b2xk")
            .body(RequestBody::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(SelectedProxy::from_proxy(&first_proxy));

        let addresses =
            cache_request_addresses(&mut request, &ProxyRules::new().proxy(second_proxy.clone()))
                .expect("addresses");

        assert_eq!(
            request
                .headers()
                .get(PROXY_AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Basic cHJveHk6b2xk")
        );
        assert_eq!(
            addresses
                .first()
                .and_then(|candidate| candidate.selected_proxy()),
            Some(&SelectedProxy::from_proxy(&first_proxy))
        );
    }

    #[test]
    fn client_builder_defaults_use_bounded_pool_and_request_limits() {
        let builder = ClientBuilder::default();

        assert_eq!(builder.transport.connect_timeout, None);
        assert_eq!(
            builder.transport.pool_idle_timeout,
            Some(Duration::from_secs(300))
        );
        assert_eq!(builder.transport.pool_max_idle_per_host, 5);
        assert_eq!(builder.transport.max_requests_total, 64);
        assert_eq!(builder.transport.max_requests_per_host, 5);
    }

    #[test]
    fn call_options_merge_prefers_newly_supplied_overrides() {
        let mut options = CallOptions::new()
            .call_timeout(Duration::from_millis(50))
            .follow_redirects(true)
            .max_retries(1);
        options.apply(
            CallOptions::new()
                .call_timeout(Duration::from_millis(25))
                .connect_timeout(Duration::from_millis(10))
                .max_retries(3),
        );

        assert_eq!(options.call_timeout, Some(Duration::from_millis(25)));
        assert_eq!(options.connect_timeout, Some(Duration::from_millis(10)));
        assert_eq!(options.follow_redirects, Some(true));
        assert_eq!(options.max_retries, Some(3));
    }

    #[test]
    fn effective_request_config_applies_call_overrides() {
        let defaults = EffectiveRequestConfig {
            call_timeout: Some(Duration::from_secs(1)),
            connect_timeout: Some(Duration::from_millis(250)),
            follow_redirects: true,
            max_redirects: 10,
            retry_on_connection_failure: true,
            max_retries: 1,
            retry_canceled_requests: false,
            allow_insecure_redirects: false,
        };

        let effective = defaults.with_overrides(
            CallOptions::new()
                .call_timeout(Duration::from_millis(25))
                .follow_redirects(false)
                .max_redirects(2)
                .retry_on_connection_failure(false)
                .max_retries(0)
                .retry_canceled_requests(true)
                .allow_insecure_redirects(true),
        );

        assert_eq!(effective.call_timeout, Some(Duration::from_millis(25)));
        assert_eq!(effective.connect_timeout, Some(Duration::from_millis(250)));
        assert!(!effective.follow_redirects);
        assert_eq!(effective.max_redirects, 2);
        assert!(!effective.retry_on_connection_failure);
        assert_eq!(effective.max_retries, 0);
        assert!(effective.retry_canceled_requests);
        assert!(effective.allow_insecure_redirects);
    }

    #[test]
    fn pool_reaper_retries_after_spawn_failure() {
        let executor = FailOnceExecutor::default();
        let timer = openwire_core::SharedTimer::new(openwire_tokio::TokioTimer::new());
        let pool = Arc::new(ConnectionPool::new(PoolSettings::default()));
        let reaper = PoolReaperController::default();

        reaper.ensure_started(
            Arc::new(executor.clone()),
            timer.clone(),
            Arc::downgrade(&pool),
        );
        assert_eq!(executor.spawns(), 1);
        assert_eq!(executor.aborts(), 0);

        reaper.ensure_started(Arc::new(executor.clone()), timer, Arc::downgrade(&pool));
        assert_eq!(executor.spawns(), 2);

        reaper.ensure_started(
            Arc::new(executor.clone()),
            openwire_core::SharedTimer::new(openwire_tokio::TokioTimer::new()),
            Arc::downgrade(&pool),
        );
        assert_eq!(executor.spawns(), 2);

        reaper.abort();
        assert_eq!(executor.aborts(), 1);
    }
}
