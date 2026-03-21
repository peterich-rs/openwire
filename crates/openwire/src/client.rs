use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response};
use openwire_core::{
    BoxWireService, CallContext, DnsResolver, EventListenerFactory, Exchange, InterceptorLayer,
    NoopEventListenerFactory, RequestBody, ResponseBody, Runtime, SharedEventListenerFactory,
    SharedInterceptor, TcpConnector, TlsConnector, TokioRuntime, WireError,
};
use tower::layer::Layer;
use tower::util::BoxCloneSyncService;
use tower::Service;
use tracing::Instrument;

use crate::auth::{Authenticator, SharedAuthenticator};
use crate::bridge::BridgeInterceptor;
use crate::connection::{ConnectionPool, ExchangeFinder, PoolSettings, RoutePlanner};
use crate::cookie::{CookieJar, SharedCookieJar};
use crate::policy::{
    AuthPolicyConfig, FollowUpPolicyService, PolicyConfig, RedirectPolicyConfig, RetryPolicyConfig,
};
use crate::proxy::{system_proxies_from_env, Proxy};
use crate::transport::{
    build_hyper_client, ConnectorStack, SystemDnsResolver, TokioTcpConnector, TransportService,
};

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    event_listener_factory: SharedEventListenerFactory,
    runtime: Arc<dyn Runtime>,
    call_timeout: Option<Duration>,
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
    pub(crate) retry_canceled_requests: bool,
}

pub struct ClientBuilder {
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    event_listener_factory: SharedEventListenerFactory,
    runtime: Arc<dyn Runtime>,
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
        R: Runtime,
    {
        self.runtime = Arc::new(runtime);
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

    pub fn retry_canceled_requests(mut self, enabled: bool) -> Self {
        self.transport.retry_canceled_requests = enabled;
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
        let exchange_finder = Arc::new(ExchangeFinder::new(pool, proxies.clone()));
        let connector = ConnectorStack {
            dns_resolver: self.dns_resolver,
            tcp_connector: self.tcp_connector,
            tls_connector,
            connect_timeout: self.transport.connect_timeout,
            proxies,
            route_planner: RoutePlanner::default(),
            proxy_authenticator: self.policy.auth.proxy_authenticator.clone(),
            max_proxy_auth_attempts: self.policy.auth.max_auth_attempts,
        };

        let transport = TransportService::new(
            build_hyper_client(connector, &self.transport),
            exchange_finder,
        );
        let service = build_service_chain(
            transport,
            self.application_interceptors,
            self.network_interceptors,
            self.policy.clone(),
        );

        Ok(Client {
            inner: Arc::new(ClientInner {
                event_listener_factory: self.event_listener_factory,
                runtime: self.runtime,
                call_timeout: self.policy.call_timeout,
                service,
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
            runtime: Arc::new(TokioRuntime),
            transport: TransportConfig {
                connect_timeout: None,
                pool_idle_timeout: Some(Duration::from_secs(90)),
                pool_max_idle_per_host: usize::MAX,
                http2_keep_alive_interval: None,
                http2_keep_alive_while_idle: false,
                retry_canceled_requests: false,
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
                },
                redirect: RedirectPolicyConfig {
                    follow_redirects: true,
                    max_redirects: 10,
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
                    .map_err(|error| WireError::internal("service chain not ready", error))?
                    .call(Exchange::new(self.request, execute_ctx, 1))
                    .await
            };

            let result = match ctx
                .deadline()
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Some(timeout) => tokio::time::timeout(timeout, execute)
                    .await
                    .map_err(|_| WireError::timeout(format!("call timed out after {timeout:?}")))?,
                None => execute.await,
            };

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
        .await
    }
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
