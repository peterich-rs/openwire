use std::task::{Context, Poll};
use std::time::Duration;

use http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, LOCATION, PROXY_AUTHORIZATION,
    SET_COOKIE,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use openwire_core::{
    BoxFuture, BoxWireService, EstablishmentStage, Exchange, RequestBody, ResponseBody, WireError,
    WireErrorKind,
};
use tower::Service;
use url::Url;

use crate::auth::{
    AuthAttemptState, AuthContext, AuthKind, AuthRequestState, AuthResponseState,
    SharedAuthenticator,
};
use crate::cookie::SharedCookieJar;
use crate::proxy::ProxySelector;
use crate::trace::PolicyTraceContext;

#[derive(Clone)]
pub(crate) struct PolicyConfig {
    pub(crate) call_timeout: Option<Duration>,
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
pub(crate) struct RetryPolicyConfig {
    pub(crate) retry_on_connection_failure: bool,
    pub(crate) max_retries: usize,
    pub(crate) retry_canceled_requests: bool,
}

#[derive(Clone)]
pub(crate) struct RedirectPolicyConfig {
    pub(crate) follow_redirects: bool,
    pub(crate) max_redirects: usize,
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

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        let network = self.network.clone();
        let config = self.config.clone();
        let proxy_selector = self.proxy_selector.clone();
        Box::pin(async move {
            let (mut request, ctx, mut attempt) = exchange.into_parts();
            let mut policy_trace = request
                .extensions()
                .get::<PolicyTraceContext>()
                .copied()
                .unwrap_or_default();
            let mut retries = policy_trace.retry_count;
            let mut redirects = policy_trace.redirect_count;
            let mut auths = policy_trace.auth_count;

            loop {
                validate_request(&request)?;
                policy_trace.retry_count = retries;
                policy_trace.redirect_count = redirects;
                policy_trace.auth_count = auths;
                request.extensions_mut().insert(policy_trace);

                let snapshot = RequestSnapshot::capture(&request);
                apply_request_cookies(&mut request, config.cookie_jar.as_deref())?;
                let mut svc = network.clone();
                let result = tower::ServiceExt::ready(&mut svc)
                    .await
                    .map_err(|error| WireError::internal("network chain not ready", error))?
                    .call(Exchange::new(request, ctx.clone(), attempt))
                    .await;

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

                        if !config.redirect.follow_redirects {
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

                        if redirects as usize >= config.redirect.max_redirects {
                            return Err(WireError::redirect(format!(
                                "too many redirects (max {})",
                                config.redirect.max_redirects
                            )));
                        }

                        let next_uri = resolve_redirect_uri(&snapshot.uri, location)?;
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
                        let Some(reason) = retry_reason(&error, &config.retry) else {
                            return Err(error);
                        };

                        if (reason != "canceled" && !config.retry.retry_on_connection_failure)
                            || retries as usize >= config.retry.max_retries
                            || !snapshot.is_replayable()
                        {
                            return Err(error);
                        }

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

fn apply_request_cookies(
    request: &mut Request<RequestBody>,
    jar: Option<&dyn crate::cookie::CookieJar>,
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
    let (kind, authenticator) = match response.status() {
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

    let ctx = AuthContext::new(
        kind,
        AuthRequestState::new(
            snapshot.method.clone(),
            snapshot.uri.clone(),
            snapshot.version,
            snapshot.headers.clone(),
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

    if let Some(mut request) = authenticator.authenticate(ctx).await? {
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
    jar: Option<&dyn crate::cookie::CookieJar>,
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
    // Follow-up requests currently preserve protocol-visible request state plus
    // internal tracing metadata. Arbitrary http::Extensions are intentionally
    // not copied until those semantics are designed explicitly.
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
            .expect("captured replayable body must remain replayable");
        let mut request = Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .version(self.version)
            .body(body)?;
        *request.headers_mut() = self.headers.clone();
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
        request.extensions_mut().insert(policy_trace);
        Ok(request)
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
            _ => unreachable!("validated scheme"),
        });
        Ok(Self { scheme, host, port })
    }
}

fn same_origin(left: &Uri, right: &Uri) -> Result<bool, WireError> {
    Ok(OriginKey::from_uri(left)? == OriginKey::from_uri(right)?)
}

fn retry_reason(error: &WireError, config: &RetryPolicyConfig) -> Option<&'static str> {
    match error.establishment_stage() {
        Some(EstablishmentStage::Dns) if error.is_retryable_establishment() => return Some("dns"),
        Some(EstablishmentStage::Tcp) if error.is_connect_timeout() => {
            return Some("connect_timeout")
        }
        Some(EstablishmentStage::Tcp | EstablishmentStage::ProtocolBinding)
            if error.is_retryable_establishment() =>
        {
            return Some("connect");
        }
        Some(EstablishmentStage::Tls) if error.is_retryable_establishment() => {
            return Some("tls");
        }
        Some(EstablishmentStage::RouteExhausted | EstablishmentStage::ProxyTunnel)
            if error.is_retryable_establishment() =>
        {
            return Some("connect");
        }
        Some(_) => return None,
        None => {}
    }

    match error.kind() {
        WireErrorKind::Canceled if config.retry_canceled_requests => Some("canceled"),
        WireErrorKind::Dns => Some("dns"),
        WireErrorKind::Connect if !error.is_non_retryable_connect() => Some("connect"),
        WireErrorKind::Tls => Some("tls"),
        WireErrorKind::Timeout if error.is_connect_timeout() => Some("connect_timeout"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use http::header::{AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION};
    use http::{HeaderValue, Request, StatusCode};

    use super::{same_origin, PolicyTraceContext, RequestSnapshot};
    use crate::proxy::{NoProxy, Proxy, ProxySelector};
    use crate::RequestBody;

    fn snapshot_with_headers(uri: &str, headers: &[(&http::HeaderName, &str)]) -> RequestSnapshot {
        let mut request = Request::builder()
            .uri(uri)
            .body(RequestBody::empty())
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
}
