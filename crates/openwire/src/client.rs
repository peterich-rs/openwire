use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, LOCATION, PROXY_AUTHORIZATION,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::Client as HyperClient;
use openwire_core::{
    BoxFuture, BoxWireService, CallContext, DnsResolver, EventListenerFactory, Exchange,
    InterceptorLayer, NoopEventListenerFactory, RequestBody, ResponseBody, Runtime,
    SharedEventListenerFactory, SharedInterceptor, TcpConnector, TlsConnector, WireError,
};
use tower::layer::Layer;
use tower::util::BoxCloneSyncService;
use tower::Service;
use tracing::Instrument;
use url::Url;

use crate::transport::{
    build_hyper_client, ConnectorStack, SystemDnsResolver, TokioRuntime, TokioTcpConnector,
    TransportService,
};

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    application_interceptors: Vec<SharedInterceptor>,
    network_interceptors: Vec<SharedInterceptor>,
    event_listener_factory: SharedEventListenerFactory,
    runtime: Arc<dyn Runtime>,
    transport: TransportConfig,
    policy: PolicyConfig,
    dns_resolver: Arc<dyn DnsResolver>,
    tcp_connector: Arc<dyn TcpConnector>,
    tls_connector: Option<Arc<dyn TlsConnector>>,
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

#[derive(Clone)]
struct PolicyConfig {
    call_timeout: Option<Duration>,
    follow_redirects: bool,
    max_redirects: usize,
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

    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.policy.call_timeout = Some(timeout);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.transport.connect_timeout = Some(timeout);
        self
    }

    pub fn follow_redirects(mut self, enabled: bool) -> Self {
        self.policy.follow_redirects = enabled;
        self
    }

    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.policy.max_redirects = max_redirects;
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
        Ok(Client {
            inner: Arc::new(ClientInner {
                application_interceptors: self.application_interceptors,
                network_interceptors: self.network_interceptors,
                event_listener_factory: self.event_listener_factory,
                runtime: self.runtime,
                transport: self.transport,
                policy: self.policy,
                dns_resolver: self.dns_resolver,
                tcp_connector: self.tcp_connector,
                tls_connector: self.tls_connector,
            }),
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        #[cfg(feature = "tls-rustls")]
        let tls_connector = Some(Arc::new(
            openwire_rustls::RustlsTlsConnector::builder()
                .build()
                .expect("failed to build default rustls connector"),
        ) as Arc<dyn TlsConnector>);

        #[cfg(not(feature = "tls-rustls"))]
        let tls_connector = None;

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
                retry_canceled_requests: true,
            },
            policy: PolicyConfig {
                call_timeout: None,
                follow_redirects: true,
                max_redirects: 10,
            },
            dns_resolver: Arc::new(SystemDnsResolver),
            tcp_connector: Arc::new(TokioTcpConnector),
            tls_connector,
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

    fn build_service(&self, ctx: CallContext) -> BoxWireService {
        let connector = ConnectorStack {
            dns_resolver: self.inner.dns_resolver.clone(),
            tcp_connector: self.inner.tcp_connector.clone(),
            tls_connector: self.inner.tls_connector.clone(),
            connect_timeout: self.inner.transport.connect_timeout,
            ctx,
        };

        let hyper_client: HyperClient<_, RequestBody> =
            build_hyper_client(connector, &self.inner.transport);
        let transport = TransportService {
            client: hyper_client,
        };

        let mut network: BoxWireService = BoxCloneSyncService::new(transport);
        for interceptor in self.inner.network_interceptors.iter().rev() {
            network =
                BoxCloneSyncService::new(InterceptorLayer::new(interceptor.clone()).layer(network));
        }

        let policy = PolicyService {
            network,
            config: self.inner.policy.clone(),
        };

        let mut service: BoxWireService = BoxCloneSyncService::new(policy);
        for interceptor in self.inner.application_interceptors.iter().rev() {
            service =
                BoxCloneSyncService::new(InterceptorLayer::new(interceptor.clone()).layer(service));
        }

        service
    }
}

impl Call {
    pub async fn execute(self) -> Result<Response<ResponseBody>, WireError> {
        let ctx = CallContext::from_factory(
            &self.client.inner.event_listener_factory,
            &self.request,
            self.client.inner.policy.call_timeout,
        );

        let span = tracing::info_span!(
            "openwire.call",
            call_id = ctx.call_id().as_u64(),
            method = %self.request.method(),
            uri = %self.request.uri(),
        );

        async move {
            ctx.listener().call_start(&ctx, &self.request);

            let service = self.client.build_service(ctx.clone());
            let mut service = service;
            let execute_ctx = ctx.clone();
            let execute = async move {
                tower::ServiceExt::ready(&mut service)
                    .await
                    .map_err(|error| WireError::internal("service chain not ready", error))?
                    .call(Exchange::new(self.request, execute_ctx, 1))
                    .await
            };

            let result = match self.client.inner.policy.call_timeout {
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

#[derive(Clone)]
struct PolicyService {
    network: BoxWireService,
    config: PolicyConfig,
}

impl Service<Exchange> for PolicyService {
    type Response = Response<ResponseBody>;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        let network = self.network.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let (mut request, ctx, mut attempt) = exchange.into_parts();

            loop {
                validate_request(&request)?;
                let redirect_snapshot = RedirectSnapshot::capture(&request);

                let mut svc = network.clone();
                let response = tower::ServiceExt::ready(&mut svc)
                    .await
                    .map_err(|error| WireError::internal("network chain not ready", error))?
                    .call(Exchange::new(request, ctx.clone(), attempt))
                    .await?;

                if !config.follow_redirects {
                    return Ok(response);
                }

                let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                else {
                    return Ok(response);
                };

                if !is_redirect_status(response.status()) {
                    return Ok(response);
                }

                if attempt as usize >= config.max_redirects {
                    return Err(WireError::redirect(format!(
                        "too many redirects (max {})",
                        config.max_redirects
                    )));
                }

                let next_uri = resolve_redirect_uri(&redirect_snapshot.uri, location)?;
                ctx.listener().redirect(&ctx, attempt, &next_uri);

                request = redirect_snapshot.into_redirect_request(response.status(), next_uri)?;
                attempt += 1;
            }
        })
    }
}

fn validate_request(request: &Request<RequestBody>) -> Result<(), WireError> {
    let scheme = request
        .uri()
        .scheme_str()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a scheme"))?;

    if !matches!(scheme, "http" | "https") {
        return Err(WireError::invalid_request(
            "request URI scheme must be http or https",
        ));
    }

    if request.uri().host().is_none() {
        return Err(WireError::invalid_request("request URI is missing a host"));
    }

    Ok(())
}

fn is_redirect_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn resolve_redirect_uri(base: &Uri, location: &str) -> Result<Uri, WireError> {
    let base = Url::parse(&base.to_string())
        .map_err(|error| WireError::redirect(format!("invalid base URL for redirect: {error}")))?;
    let joined = base
        .join(location)
        .map_err(|error| WireError::redirect(format!("invalid redirect URL: {error}")))?;
    joined
        .as_str()
        .parse::<Uri>()
        .map_err(|error| WireError::redirect(format!("failed to parse redirect URI: {error}")))
}

struct RedirectSnapshot {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    body: Option<RequestBody>,
}

impl RedirectSnapshot {
    fn capture(request: &Request<RequestBody>) -> Self {
        Self {
            method: request.method().clone(),
            uri: request.uri().clone(),
            version: request.version(),
            headers: request.headers().clone(),
            body: request.body().try_clone(),
        }
    }

    fn into_redirect_request(
        self,
        status: StatusCode,
        next_uri: Uri,
    ) -> Result<Request<RequestBody>, WireError> {
        let same_authority = same_authority(&self.uri, &next_uri);
        let should_switch_to_get = matches!(
            status,
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
        ) && self.method != Method::GET
            && self.method != Method::HEAD;

        let preserve_body = matches!(
            status,
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
        );

        let body = if preserve_body {
            self.body.ok_or_else(|| {
                WireError::redirect("cannot follow redirect for a non-replayable request body")
            })?
        } else if should_switch_to_get {
            RequestBody::empty()
        } else {
            self.body.unwrap_or_else(RequestBody::empty)
        };

        let method = if preserve_body {
            self.method
        } else if should_switch_to_get {
            Method::GET
        } else {
            self.method
        };

        let mut headers = self.headers;
        headers.remove(HOST);
        if !same_authority {
            headers.remove(AUTHORIZATION);
            headers.remove(PROXY_AUTHORIZATION);
        }
        if should_switch_to_get {
            headers.remove(CONTENT_LENGTH);
            headers.remove(CONTENT_TYPE);
        }

        let mut request = Request::builder()
            .method(method)
            .uri(next_uri)
            .version(self.version)
            .body(body)?;
        *request.headers_mut() = headers;
        Ok(request)
    }
}

fn same_authority(left: &Uri, right: &Uri) -> bool {
    left.scheme_str() == right.scheme_str() && left.authority() == right.authority()
}
