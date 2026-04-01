use std::task::{Context, Poll};

use http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, LOCATION, PROXY_AUTHORIZATION,
    SET_COOKIE,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use openwire_core::{
    AuthKind, BoxFuture, BoxWireService, CookieJar, Exchange, RedirectContext, RedirectDecision,
    RequestBody, ResponseBody, RetryContext, WireError,
};
use tower::Service;
use url::Url;

use super::{RedirectPolicyConfig, RetryPolicyConfig};
use crate::auth::{
    build_auth_context, AuthAttemptState, AuthRequestState, AuthResponseState, SharedAuthenticator,
};
use crate::client::{CallOptions, EffectiveRequestConfig};
use crate::cookie::SharedCookieJar;
use crate::proxy::ProxySelector;
use crate::trace::PolicyTraceContext;

#[derive(Clone)]
pub(crate) struct PolicyConfig {
    pub(crate) cookie_jar: Option<SharedCookieJar>,
    pub(crate) auth: AuthPolicyConfig,
    pub(crate) retry: RetryPolicyConfig,
    pub(crate) redirect: RedirectPolicyConfig,
}

#[derive(Clone)]
pub(crate) struct AuthPolicyConfig {
    pub(crate) authenticator: Option<SharedAuthenticator>,
    pub(crate) proxy_authenticator: Option<SharedAuthenticator>,
    pub(crate) max_auth_attempts: usize,
}

#[derive(Clone)]
pub(crate) struct FollowUpPolicyService {
    network: BoxWireService,
    config: PolicyConfig,
    proxy_selector: ProxySelector,
}

impl FollowUpPolicyService {
    pub(crate) fn new(
        network: BoxWireService,
        config: PolicyConfig,
        proxy_selector: ProxySelector,
    ) -> Self {
        Self {
            network,
            config,
            proxy_selector,
        }
    }
}

impl Service<Exchange> for FollowUpPolicyService {
    type Response = Response<ResponseBody>;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.network.poll_ready(cx)
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        let replacement = self.network.clone();
        let mut network = std::mem::replace(&mut self.network, replacement);
        let proxy_selector = self.proxy_selector.clone();
        let mut config = self.config.clone();
        Box::pin(async move {
            let (mut request, ctx, mut attempt) = exchange.into_parts();
            let request_config = request
                .extensions()
                .get::<EffectiveRequestConfig>()
                .copied();
            let request_options = request
                .extensions()
                .get::<CallOptions>()
                .copied()
                .unwrap_or_default();
            apply_request_overrides(&mut config, request_config, request_options);
            let mut policy_trace = request
                .extensions()
                .get::<PolicyTraceContext>()
                .copied()
                .unwrap_or_default();
            let mut retries = policy_trace.retry_count;
            let mut redirects = policy_trace.redirect_count;
            let mut auths = policy_trace.auth_count;
            let mut first_network_attempt = true;

            loop {
                validate_request(&request)?;
                policy_trace.retry_count = retries;
                policy_trace.redirect_count = redirects;
                policy_trace.auth_count = auths;
                request.extensions_mut().insert(policy_trace);

                let snapshot = RequestSnapshot::capture(&request);
                apply_request_cookies(&mut request, config.cookie_jar.as_deref())?;
                let exchange = Exchange::new(request, ctx.clone(), attempt);
                let result = if first_network_attempt {
                    first_network_attempt = false;
                    network.call(exchange).await
                } else {
                    tower::ServiceExt::ready(&mut network)
                        .await
                        .map_err(|error| WireError::internal("network chain not ready", error))?
                        .call(exchange)
                        .await
                };

                match result {
                    Ok(response) => {
                        store_response_cookies(
                            &response,
                            &snapshot.uri,
                            config.cookie_jar.as_deref(),
                        )?;

                        if let Some((next_request, next_auth_count)) = authenticate_response(
                            &snapshot,
                            &response,
                            attempt,
                            retries,
                            redirects,
                            auths,
                            &config.auth,
                        )
                        .await?
                        {
                            auths = next_auth_count;
                            let next_attempt = attempt + 1;
                            policy_trace.retry_count = retries;
                            policy_trace.redirect_count = redirects;
                            policy_trace.auth_count = auths;
                            tracing::debug!(
                                call_id = ctx.call_id().as_u64(),
                                attempt = next_attempt,
                                retry_count = policy_trace.retry_count,
                                redirect_count = policy_trace.redirect_count,
                                auth_count = policy_trace.auth_count,
                                response_status = %response.status(),
                                "following authentication challenge",
                            );
                            request = next_request;
                            attempt = next_attempt;
                            continue;
                        }

                        if let Some(policy) = config.redirect.default_policy() {
                            if !policy.follow_redirects() {
                                return Ok(response);
                            }
                        }

                        if !is_redirect_status(response.status()) {
                            return Ok(response);
                        }

                        let Some(location) = response
                            .headers()
                            .get(LOCATION)
                            .and_then(|value| value.to_str().ok())
                        else {
                            return Ok(response);
                        };

                        let next_uri = resolve_redirect_uri(&snapshot.uri, location)?;
                        validate_request_uri(&next_uri)?;

                        match config
                            .redirect
                            .policy()
                            .should_redirect(&RedirectContext::new(
                                &snapshot.method,
                                &snapshot.uri,
                                response.status(),
                                &next_uri,
                                redirects,
                                snapshot.is_replayable(),
                            )) {
                            RedirectDecision::Follow => {}
                            RedirectDecision::Stop => return Ok(response),
                            RedirectDecision::Error(error) => return Err(error),
                        }

                        ctx.listener().redirect(&ctx, redirects + 1, &next_uri);

                        let next_attempt = attempt + 1;
                        policy_trace.retry_count = retries;
                        policy_trace.redirect_count = redirects + 1;
                        policy_trace.auth_count = auths;
                        tracing::debug!(
                            call_id = ctx.call_id().as_u64(),
                            attempt = next_attempt,
                            retry_count = policy_trace.retry_count,
                            redirect_count = policy_trace.redirect_count,
                            auth_count = policy_trace.auth_count,
                            redirect_location = %next_uri,
                            "following redirect",
                        );

                        request = snapshot.into_redirect_request(
                            response.status(),
                            next_uri,
                            policy_trace,
                            &proxy_selector,
                        )?;
                        redirects += 1;
                        attempt = next_attempt;
                    }
                    Err(error) => {
                        let retry_ctx = RetryContext::new(
                            &error,
                            retries,
                            snapshot.is_replayable(),
                            &snapshot.method,
                        );
                        let Some(reason) = config.retry.policy().should_retry(&retry_ctx) else {
                            return Err(error);
                        };

                        retries += 1;
                        attempt += 1;
                        policy_trace.retry_count = retries;
                        policy_trace.redirect_count = redirects;
                        policy_trace.auth_count = auths;
                        ctx.listener().retry(&ctx, retries, reason);
                        tracing::debug!(
                            call_id = ctx.call_id().as_u64(),
                            attempt,
                            retry_count = policy_trace.retry_count,
                            redirect_count = policy_trace.redirect_count,
                            auth_count = policy_trace.auth_count,
                            retry_reason = reason,
                            "retrying request after connection-establishment failure",
                        );

                        request = snapshot.to_retry_request(policy_trace)?;
                    }
                }
            }
        })
    }
}

fn apply_request_overrides(
    config: &mut PolicyConfig,
    request_config: Option<EffectiveRequestConfig>,
    request_options: CallOptions,
) {
    let Some(request_config) = request_config else {
        return;
    };

    if request_options.has_retry_overrides() {
        let retry = config.retry.default_mut();
        retry.set_retry_on_connection_failure(request_config.retry_on_connection_failure);
        retry.set_max_retries(request_config.max_retries);
        retry.set_retry_canceled_requests(request_config.retry_canceled_requests);
    }

    if request_options.has_redirect_overrides() {
        let redirect = config.redirect.default_mut();
        redirect.set_follow_redirects(request_config.follow_redirects);
        redirect.set_max_redirects(request_config.max_redirects);
        redirect.set_allow_insecure_redirects(request_config.allow_insecure_redirects);
    }
}

fn apply_request_cookies(
    request: &mut Request<RequestBody>,
    jar: Option<&dyn CookieJar>,
) -> Result<(), WireError> {
    let Some(jar) = jar else {
        return Ok(());
    };

    if request.headers().contains_key(COOKIE) {
        return Ok(());
    }

    let url = request_url(request.uri())?;
    if let Some(value) = jar.cookies(&url) {
        request.headers_mut().insert(COOKIE, value);
    }
    Ok(())
}

async fn authenticate_response(
    snapshot: &RequestSnapshot,
    response: &Response<ResponseBody>,
    attempt: u32,
    retries: u32,
    redirects: u32,
    auths: u32,
    config: &AuthPolicyConfig,
) -> Result<Option<(Request<RequestBody>, u32)>, WireError> {
    let (kind, authenticator): (AuthKind, Option<&SharedAuthenticator>) = match response.status() {
        StatusCode::UNAUTHORIZED => (AuthKind::Origin, config.authenticator.as_ref()),
        StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            (AuthKind::Proxy, config.proxy_authenticator.as_ref())
        }
        _ => (AuthKind::Origin, None),
    };

    let Some(authenticator) = authenticator else {
        return Ok(None);
    };

    if auths as usize >= config.max_auth_attempts || !snapshot.is_replayable() {
        return Ok(None);
    }

    let ctx = build_auth_context(
        kind,
        AuthRequestState::new(
            snapshot.method.clone(),
            snapshot.uri.clone(),
            snapshot.version,
            snapshot.headers.clone(),
            snapshot.extensions.clone(),
            snapshot.body.as_ref().and_then(RequestBody::try_clone),
        ),
        AuthResponseState::new(response.status(), response.headers().clone()),
        AuthAttemptState {
            total_attempt: attempt,
            retry_count: retries,
            redirect_count: redirects,
            auth_count: auths,
        },
    );

    if let Some(mut request) = authenticator
        .authenticate(ctx)
        .await
        .map_err(|error| error.with_response_status(response.status()))?
    {
        let next_auth_count = auths + 1;
        request.extensions_mut().insert(PolicyTraceContext {
            retry_count: retries,
            redirect_count: redirects,
            auth_count: next_auth_count,
        });
        return Ok(Some((request, next_auth_count)));
    }

    Ok(None)
}

fn store_response_cookies(
    response: &Response<ResponseBody>,
    request_uri: &Uri,
    jar: Option<&dyn CookieJar>,
) -> Result<(), WireError> {
    let Some(jar) = jar else {
        return Ok(());
    };

    let cookies = response.headers().get_all(SET_COOKIE);
    if cookies.iter().next().is_none() {
        return Ok(());
    }

    let url = request_url(request_uri)?;
    let mut cookies = cookies.iter();
    jar.set_cookies(&mut cookies, &url);
    Ok(())
}

struct RequestSnapshot {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    extensions: http::Extensions,
    body: Option<RequestBody>,
}

fn request_url(uri: &Uri) -> Result<Url, WireError> {
    Url::parse(&uri.to_string()).map_err(|error| {
        WireError::invalid_request(format!("request URI is not a valid URL: {error}"))
    })
}

impl RequestSnapshot {
    fn capture(request: &Request<RequestBody>) -> Self {
        Self {
            method: request.method().clone(),
            uri: request.uri().clone(),
            version: request.version(),
            headers: request.headers().clone(),
            extensions: request.extensions().clone(),
            body: request.body().try_clone(),
        }
    }

    fn is_replayable(&self) -> bool {
        self.body.is_some()
    }

    fn to_retry_request(
        &self,
        policy_trace: PolicyTraceContext,
    ) -> Result<Request<RequestBody>, WireError> {
        let body = self
            .body
            .as_ref()
            .and_then(RequestBody::try_clone)
            .ok_or_else(|| {
                WireError::internal(
                    "captured replayable body is no longer cloneable",
                    std::io::Error::other("body clone failed on retry"),
                )
            })?;
        let mut request = Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .version(self.version)
            .body(body)?;
        *request.headers_mut() = self.headers.clone();
        *request.extensions_mut() = self.extensions.clone();
        request.extensions_mut().insert(policy_trace);
        Ok(request)
    }

    fn into_redirect_request(
        self,
        status: StatusCode,
        next_uri: Uri,
        policy_trace: PolicyTraceContext,
        proxy_selector: &ProxySelector,
    ) -> Result<Request<RequestBody>, WireError> {
        let same_origin = same_origin(&self.uri, &next_uri)?;
        let same_proxy =
            proxy_selector.selection_for(&self.uri) == proxy_selector.selection_for(&next_uri);
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
            RequestBody::absent()
        } else {
            self.body.unwrap_or_else(RequestBody::absent)
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
        if !same_origin {
            headers.remove(AUTHORIZATION);
            headers.remove(COOKIE);
        }
        if !same_proxy {
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
        *request.extensions_mut() = self.extensions;
        request.extensions_mut().insert(policy_trace);
        Ok(request)
    }
}

fn validate_request(request: &Request<RequestBody>) -> Result<(), WireError> {
    validate_request_uri(request.uri())
}

fn validate_request_uri(uri: &Uri) -> Result<(), WireError> {
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a scheme"))?;

    if !matches!(scheme, "http" | "https") {
        return Err(WireError::invalid_request(
            "request URI scheme must be http or https",
        ));
    }

    if uri.host().is_none() {
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct OriginKey {
    scheme: &'static str,
    host: String,
    port: u16,
}

impl OriginKey {
    fn from_uri(uri: &Uri) -> Result<Self, WireError> {
        let scheme = match uri.scheme_str() {
            Some("http") => "http",
            Some("https") => "https",
            Some(other) => {
                return Err(WireError::invalid_request(format!(
                    "request URI scheme must be http or https, found {other}"
                )));
            }
            None => {
                return Err(WireError::invalid_request(
                    "request URI is missing a scheme",
                ))
            }
        };
        let host = uri
            .host()
            .ok_or_else(|| WireError::invalid_request("request URI is missing a host"))?
            .to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(match scheme {
            "http" => 80,
            "https" => 443,
            _ => {
                return Err(WireError::invalid_request(format!(
                    "unsupported scheme for origin comparison: {scheme}"
                )))
            }
        });
        Ok(Self { scheme, host, port })
    }
}

fn same_origin(left: &Uri, right: &Uri) -> Result<bool, WireError> {
    Ok(OriginKey::from_uri(left)? == OriginKey::from_uri(right)?)
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use http::header::{AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION};
    use http::{HeaderValue, Request, Response, StatusCode};
    use openwire_core::{
        BoxFuture, BoxWireService, CallContext, Exchange, NoopEventListenerFactory, ResponseBody,
        WireError,
    };
    use tower::util::BoxCloneSyncService;
    use tower::{Service, ServiceExt};

    use super::{
        same_origin, AuthPolicyConfig, FollowUpPolicyService, PolicyConfig, PolicyTraceContext,
        RequestSnapshot,
    };
    use crate::policy::{RedirectPolicyConfig, RetryPolicyConfig};
    use crate::proxy::{NoProxy, Proxy, ProxySelector};
    use crate::RequestBody;

    fn snapshot_with_headers(uri: &str, headers: &[(&http::HeaderName, &str)]) -> RequestSnapshot {
        let mut request = Request::builder()
            .uri(uri)
            .body(RequestBody::absent())
            .expect("request");
        for (name, value) in headers {
            request.headers_mut().insert(
                (*name).clone(),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        RequestSnapshot::capture(&request)
    }

    #[test]
    fn same_origin_normalizes_default_ports() {
        let implicit = "http://example.com/path".parse().expect("implicit uri");
        let explicit = "http://example.com:80/other".parse().expect("explicit uri");
        assert!(same_origin(&implicit, &explicit).expect("same origin"));
    }

    #[test]
    fn redirect_to_same_origin_default_port_preserves_authorization() {
        let snapshot = snapshot_with_headers(
            "http://example.com/start",
            &[(&AUTHORIZATION, "Bearer secret")],
        );

        let request = snapshot
            .into_redirect_request(
                StatusCode::FOUND,
                "http://example.com:80/next".parse().expect("redirect uri"),
                PolicyTraceContext::default(),
                &ProxySelector::new(Vec::new()),
            )
            .expect("redirect request");

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret")
        );
    }

    #[test]
    fn cross_origin_redirect_drops_explicit_cookie_header() {
        let snapshot = snapshot_with_headers("http://source.test/start", &[(&COOKIE, "manual=1")]);

        let request = snapshot
            .into_redirect_request(
                StatusCode::FOUND,
                "http://target.test/next".parse().expect("redirect uri"),
                PolicyTraceContext::default(),
                &ProxySelector::new(Vec::new()),
            )
            .expect("redirect request");

        assert!(request.headers().get(COOKIE).is_none());
    }

    #[test]
    fn cross_origin_redirect_through_same_proxy_preserves_proxy_authorization() {
        let snapshot = snapshot_with_headers(
            "http://source.test/start",
            &[(&PROXY_AUTHORIZATION, "Basic cHJveHk6c2VjcmV0")],
        );
        let proxy_selector =
            ProxySelector::new(vec![Proxy::http("http://proxy.test:8080").expect("proxy")]);

        let request = snapshot
            .into_redirect_request(
                StatusCode::FOUND,
                "http://target.test/next".parse().expect("redirect uri"),
                PolicyTraceContext::default(),
                &proxy_selector,
            )
            .expect("redirect request");

        assert_eq!(
            request
                .headers()
                .get(PROXY_AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Basic cHJveHk6c2VjcmV0")
        );
    }

    #[test]
    fn cross_origin_redirect_to_different_proxy_drops_proxy_authorization() {
        let snapshot = snapshot_with_headers(
            "http://source.test/start",
            &[(&PROXY_AUTHORIZATION, "Basic cHJveHk6c2VjcmV0")],
        );
        let proxy_selector = ProxySelector::new(vec![
            Proxy::http("http://first.test:8080")
                .expect("first proxy")
                .no_proxy(NoProxy::new().domain("target.test")),
            Proxy::http("http://second.test:8080").expect("second proxy"),
        ]);

        let request = snapshot
            .into_redirect_request(
                StatusCode::FOUND,
                "http://target.test/next".parse().expect("redirect uri"),
                PolicyTraceContext::default(),
                &proxy_selector,
            )
            .expect("redirect request");

        assert!(request.headers().get(PROXY_AUTHORIZATION).is_none());
    }

    struct ReadinessTrackingService {
        was_polled: bool,
        is_clone: bool,
    }

    impl Clone for ReadinessTrackingService {
        fn clone(&self) -> Self {
            Self {
                was_polled: false,
                is_clone: true,
            }
        }
    }

    impl Service<Exchange> for ReadinessTrackingService {
        type Response = Response<ResponseBody>;
        type Error = WireError;
        type Future = BoxFuture<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            assert!(
                !self.is_clone,
                "poll_ready should not be re-run against a cloned network service",
            );
            self.was_polled = true;
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _exchange: Exchange) -> Self::Future {
            let was_polled = std::mem::take(&mut self.was_polled);
            Box::pin(async move {
                assert!(
                    was_polled,
                    "call must use the network service instance that was polled ready",
                );
                Ok(Response::new(ResponseBody::empty()))
            })
        }
    }

    fn default_policy_config() -> PolicyConfig {
        PolicyConfig {
            cookie_jar: None,
            auth: AuthPolicyConfig {
                authenticator: None,
                proxy_authenticator: None,
                max_auth_attempts: 3,
            },
            retry: RetryPolicyConfig::default(),
            redirect: RedirectPolicyConfig::default(),
        }
    }

    fn test_exchange() -> Exchange {
        let request = Request::builder()
            .uri("http://example.com/")
            .body(RequestBody::absent())
            .expect("request");
        let factory = std::sync::Arc::new(NoopEventListenerFactory)
            as openwire_core::SharedEventListenerFactory;
        let ctx = CallContext::from_factory(&factory, &request, None);
        Exchange::new(request, ctx, 1)
    }

    #[tokio::test]
    async fn follow_up_policy_service_preserves_ready_network_service() {
        let network: BoxWireService = BoxCloneSyncService::new(ReadinessTrackingService {
            was_polled: false,
            is_clone: false,
        });
        let mut service = FollowUpPolicyService::new(
            network,
            default_policy_config(),
            ProxySelector::new(Vec::new()),
        );

        let response = service
            .ready()
            .await
            .expect("service ready")
            .call(test_exchange())
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }
}
