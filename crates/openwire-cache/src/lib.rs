use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{AGE, AUTHORIZATION, CACHE_CONTROL, EXPIRES, SET_COOKIE, VARY};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use openwire::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};
use tokio::sync::RwLock;

const MAX_DELTA_SECONDS: u64 = 2_147_483_648;

#[derive(Clone)]
pub struct CacheInterceptor<S = MemoryCacheStore> {
    store: Arc<S>,
}

impl CacheInterceptor<MemoryCacheStore> {
    pub fn memory() -> Self {
        Self::new(MemoryCacheStore::default())
    }
}

impl<S> CacheInterceptor<S> {
    pub fn new(store: S) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
}

impl<S> Interceptor for CacheInterceptor<S>
where
    S: CacheStore,
{
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let store = self.store.clone();
        Box::pin(async move {
            let request_policy = request_cache_policy(exchange.request());
            let cache_key = request_policy
                .is_cacheable
                .then(|| cache_key(exchange.request().uri()));
            let request_headers = exchange.request().headers().clone();

            if let Some(cache_key) = cache_key.as_ref() {
                if request_policy.lookup {
                    if let Some(entry) = store.get(cache_key).await {
                        if entry.matches_request(&request_headers)
                            && entry.is_fresh_for(&request_policy)
                        {
                            return Ok(entry.into_response());
                        }

                        if entry.matches_request(&request_headers) && entry.is_expired() {
                            store.remove(cache_key).await;
                        }
                    }
                }

                if request_policy.only_if_cached {
                    if request_policy.lookup {
                        if let Some(entry) = store.get(cache_key).await {
                            if entry.matches_request(&request_headers)
                                && entry.is_fresh_for(&request_policy)
                            {
                                return Ok(entry.into_response());
                            }
                        }
                    }

                    return Ok(gateway_timeout_response());
                }
            }

            let response = next.run(exchange).await?;
            let Some(cache_key) = cache_key else {
                return Ok(response);
            };

            if !request_policy.store_response {
                return Ok(response);
            }

            let (parts, body) = response.into_parts();
            let response_directives = CacheDirectives::from_headers(&parts.headers);
            if response_directives.no_store || response_directives.no_cache {
                store.remove(&cache_key).await;
                return Ok(Response::from_parts(parts, body));
            }

            let Some(fresh_for) = response_freshness(&parts.headers, &response_directives) else {
                return Ok(Response::from_parts(parts, body));
            };

            if !response_is_cacheable(&parts.headers, parts.status) {
                return Ok(Response::from_parts(parts, body));
            }

            let Some(vary) = CapturedVary::capture(&parts.headers, &request_headers) else {
                return Ok(Response::from_parts(parts, body));
            };

            let body = body.bytes().await?;
            let cached_headers = parts.headers.clone();
            store
                .put(
                    cache_key,
                    CachedResponse::new_with_vary(
                        parts.status,
                        parts.version,
                        cached_headers,
                        body.clone(),
                        fresh_for,
                        vary,
                    ),
                )
                .await;

            Ok(build_response(
                parts.status,
                parts.version,
                parts.headers,
                body,
            ))
        })
    }
}

#[derive(Clone, Debug, Default)]
struct RequestCachePolicy {
    is_cacheable: bool,
    lookup: bool,
    store_response: bool,
    only_if_cached: bool,
    max_age: Option<Duration>,
    min_fresh: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
struct CacheDirectives {
    no_cache: bool,
    no_store: bool,
    only_if_cached: bool,
    max_age: Option<Duration>,
    min_fresh: Option<Duration>,
}

impl CacheDirectives {
    fn from_headers(headers: &HeaderMap) -> Self {
        let mut out = Self::default();
        for value in headers.get_all(CACHE_CONTROL) {
            let Ok(value) = value.to_str() else {
                continue;
            };

            for directive in cache_control_directives(value) {
                match directive.name.as_str() {
                    "no-cache" => out.no_cache = true,
                    "no-store" => out.no_store = true,
                    "only-if-cached" => out.only_if_cached = true,
                    "max-age" => {
                        if let Some(value) =
                            directive.value.as_deref().and_then(parse_delta_seconds)
                        {
                            out.max_age = Some(value);
                        }
                    }
                    "min-fresh" => {
                        if let Some(value) =
                            directive.value.as_deref().and_then(parse_delta_seconds)
                        {
                            out.min_fresh = Some(value);
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

#[derive(Debug)]
struct CacheDirective {
    name: String,
    value: Option<String>,
}

fn request_cache_policy(request: &Request<RequestBody>) -> RequestCachePolicy {
    let directives = CacheDirectives::from_headers(request.headers());
    let is_cacheable = request.method() == Method::GET
        && request.body().replayable_len() == Some(0)
        && !request.headers().contains_key(AUTHORIZATION);
    let lookup = is_cacheable
        && !directives.no_cache
        && !directives.max_age.is_some_and(|max_age| max_age.is_zero());

    RequestCachePolicy {
        is_cacheable,
        lookup,
        store_response: is_cacheable && !directives.no_store,
        only_if_cached: is_cacheable && directives.only_if_cached,
        max_age: directives.max_age,
        min_fresh: directives.min_fresh,
    }
}

#[derive(Clone, Default)]
struct CapturedVary {
    fields: Vec<VaryField>,
}

impl CapturedVary {
    fn capture(response_headers: &HeaderMap, request_headers: &HeaderMap) -> Option<Self> {
        let mut fields = Vec::new();
        for value in response_headers.get_all(VARY) {
            let value = value.to_str().ok()?;
            for name in value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                if name == "*" {
                    return None;
                }
                let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
                if fields.iter().any(|field: &VaryField| field.name == name) {
                    continue;
                }
                fields.push(VaryField {
                    values: request_headers.get_all(&name).iter().cloned().collect(),
                    name,
                });
            }
        }
        Some(Self { fields })
    }

    fn matches(&self, request_headers: &HeaderMap) -> bool {
        self.fields.iter().all(|field| {
            let current = request_headers
                .get_all(&field.name)
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            current == field.values
        })
    }
}

#[derive(Clone)]
struct VaryField {
    name: HeaderName,
    values: Vec<HeaderValue>,
}

fn cache_control_directives(value: &str) -> impl Iterator<Item = CacheDirective> + '_ {
    value.split(',').filter_map(|raw| {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let (name, value) = match raw.split_once('=') {
            Some((name, value)) => (name.trim(), Some(unquote(value.trim()).to_owned())),
            None => (raw, None),
        };
        Some(CacheDirective {
            name: name.to_ascii_lowercase(),
            value,
        })
    })
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn parse_delta_seconds(value: &str) -> Option<Duration> {
    value
        .parse::<u128>()
        .ok()
        .map(|seconds| seconds.min(MAX_DELTA_SECONDS as u128) as u64)
        .map(Duration::from_secs)
}

#[async_trait]
pub trait CacheStore: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Option<CachedResponse>;
    async fn put(&self, key: String, value: CachedResponse);
    async fn remove(&self, key: &str);
}

#[derive(Clone, Default)]
pub struct MemoryCacheStore {
    entries: Arc<RwLock<HashMap<String, CachedResponse>>>,
}

#[async_trait]
impl CacheStore for MemoryCacheStore {
    async fn get(&self, key: &str) -> Option<CachedResponse> {
        self.entries.read().await.get(key).cloned()
    }

    async fn put(&self, key: String, value: CachedResponse) {
        self.entries.write().await.insert(key, value);
    }

    async fn remove(&self, key: &str) {
        self.entries.write().await.remove(key);
    }
}

#[derive(Clone)]
pub struct CachedResponse {
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    stored_at: Instant,
    fresh_until: Instant,
    vary: CapturedVary,
}

impl CachedResponse {
    pub fn new(
        status: StatusCode,
        version: Version,
        headers: HeaderMap,
        body: Bytes,
        fresh_for: Duration,
    ) -> Self {
        Self::new_with_vary(
            status,
            version,
            headers,
            body,
            fresh_for,
            CapturedVary::default(),
        )
    }

    fn new_with_vary(
        status: StatusCode,
        version: Version,
        headers: HeaderMap,
        body: Bytes,
        fresh_for: Duration,
        vary: CapturedVary,
    ) -> Self {
        let stored_at = Instant::now();
        Self {
            status,
            version,
            headers,
            body,
            stored_at,
            fresh_until: stored_at
                .checked_add(fresh_for)
                .unwrap_or_else(|| stored_at + Duration::from_secs(MAX_DELTA_SECONDS)),
            vary,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.fresh_until
    }

    fn is_fresh_for(&self, request_policy: &RequestCachePolicy) -> bool {
        let now = Instant::now();
        if now >= self.fresh_until {
            return false;
        }

        if let Some(max_age) = request_policy.max_age {
            if now.duration_since(self.stored_at) > max_age {
                return false;
            }
        }

        if let Some(min_fresh) = request_policy.min_fresh {
            let Some(required_fresh_until) = now.checked_add(min_fresh) else {
                return false;
            };
            if required_fresh_until > self.fresh_until {
                return false;
            }
        }

        true
    }

    fn matches_request(&self, request_headers: &HeaderMap) -> bool {
        self.vary.matches(request_headers)
    }

    fn into_response(self) -> Response<ResponseBody> {
        build_response(self.status, self.version, self.headers, self.body)
    }
}

fn response_is_cacheable(headers: &HeaderMap, status: StatusCode) -> bool {
    status == StatusCode::OK && !headers.contains_key(SET_COOKIE)
}

fn response_freshness(headers: &HeaderMap, directives: &CacheDirectives) -> Option<Duration> {
    if let Some(max_age) = directives.max_age {
        if max_age.is_zero() {
            return None;
        }
        let age = response_age(headers);
        let remaining = max_age.checked_sub(age)?;
        if remaining.is_zero() {
            return None;
        }
        return Some(remaining);
    }

    let expires = headers.get(EXPIRES)?.to_str().ok()?;
    let expires = httpdate::parse_http_date(expires).ok()?;
    let remaining = expires.duration_since(SystemTime::now()).ok()?;
    (!remaining.is_zero()).then_some(remaining)
}

fn response_age(headers: &HeaderMap) -> Duration {
    headers
        .get(AGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default()
}

fn cache_key(uri: &Uri) -> String {
    uri.to_string()
}

fn build_response(
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
) -> Response<ResponseBody> {
    let mut response = Response::new(ResponseBody::from_bytes(body));
    *response.status_mut() = status;
    *response.version_mut() = version;
    *response.headers_mut() = headers;
    response
}

fn gateway_timeout_response() -> Response<ResponseBody> {
    let mut response = Response::new(ResponseBody::empty());
    *response.status_mut() = StatusCode::GATEWAY_TIMEOUT;
    response
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{parse_delta_seconds, MAX_DELTA_SECONDS};

    #[test]
    fn delta_seconds_are_clamped_for_overflow_safety() {
        assert_eq!(parse_delta_seconds("60"), Some(Duration::from_secs(60)));
        assert_eq!(
            parse_delta_seconds("999999999999999999999999999"),
            Some(Duration::from_secs(MAX_DELTA_SECONDS))
        );
    }
}
