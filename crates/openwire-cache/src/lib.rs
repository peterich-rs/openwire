use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{
    AGE, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, DATE, ETAG, EXPIRES,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, SET_COOKIE, TRANSFER_ENCODING, VARY,
};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use openwire::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};
use tokio::sync::RwLock;

const MAX_DELTA_SECONDS: u64 = 2_147_483_648;
const LAST_MODIFIED_HEURISTIC_DENOMINATOR: u64 = 10;

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
        mut exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let store = self.store.clone();
        Box::pin(async move {
            let request_policy = request_cache_policy(exchange.request());
            let cache_key = request_policy
                .is_cacheable
                .then(|| cache_key(exchange.request().uri()));
            let request_headers = exchange.request().headers().clone();

            let mut validation_entry = None;
            if let Some(cache_key) = cache_key.as_ref() {
                for entry in store.get_candidates(cache_key).await.into_iter().rev() {
                    if entry.matches_request(&request_headers) {
                        if request_policy.lookup && entry.is_servable_for(&request_policy) {
                            return Ok(entry.into_response());
                        }

                        if !request_policy.only_if_cached
                            && entry.can_revalidate(exchange.request().headers())
                        {
                            validation_entry = Some(entry);
                        }
                        break;
                    }
                }

                if request_policy.only_if_cached {
                    return Ok(gateway_timeout_response());
                }
            }

            if let Some(entry) = validation_entry.as_ref() {
                entry.apply_revalidation_headers(exchange.request_mut().headers_mut());
            }

            let response = next.run(exchange).await?;
            let Some(cache_key) = cache_key else {
                return Ok(response);
            };

            if let Some(entry) = validation_entry {
                if response.status() == StatusCode::NOT_MODIFIED {
                    let (parts, body) = response.into_parts();
                    let _ = body.bytes().await?;
                    let (freshened, store_response) =
                        entry.freshen_from_not_modified(&parts.headers, &request_headers);
                    if store_response {
                        store.put_candidate(cache_key, freshened.clone()).await;
                    } else {
                        store.remove(&cache_key).await;
                    }
                    return Ok(freshened.into_response());
                }
            }

            if !request_policy.store_response {
                return Ok(response);
            }

            let (parts, body) = response.into_parts();
            let response_directives = CacheDirectives::from_headers(&parts.headers);
            if response_directives.no_store {
                store.remove(&cache_key).await;
                return Ok(Response::from_parts(parts, body));
            }

            let validators = CacheValidators::from_headers(&parts.headers);
            let freshness_lifetime =
                response_freshness_lifetime(&parts.headers, &response_directives)
                    .unwrap_or_default();
            if !response_is_storable(
                &parts.headers,
                parts.status,
                &response_directives,
                &validators,
                freshness_lifetime,
            ) {
                if parts.status == StatusCode::OK {
                    store.remove(&cache_key).await;
                }
                return Ok(Response::from_parts(parts, body));
            }

            let Some(vary) = CapturedVary::capture(&parts.headers, &request_headers) else {
                store.remove(&cache_key).await;
                return Ok(Response::from_parts(parts, body));
            };

            let body = body.bytes().await?;
            let cached_headers = parts.headers.clone();
            store
                .put_candidate(
                    cache_key,
                    CachedResponse::new_with_vary(
                        parts.status,
                        parts.version,
                        cached_headers,
                        body.clone(),
                        freshness_lifetime,
                        CachedResponseOptions {
                            vary,
                            must_validate: response_directives.no_cache,
                            must_revalidate: response_directives.must_revalidate,
                        },
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
    max_stale: Option<MaxStale>,
    min_fresh: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
struct CacheDirectives {
    no_cache: bool,
    no_store: bool,
    only_if_cached: bool,
    max_age: Option<Duration>,
    max_stale: Option<MaxStale>,
    min_fresh: Option<Duration>,
    must_revalidate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaxStale {
    Any,
    Limit(Duration),
}

#[derive(Clone, Debug, Default)]
struct CacheValidators {
    etag: Option<HeaderValue>,
    last_modified: Option<HeaderValue>,
}

impl CacheValidators {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            etag: headers.get(ETAG).cloned(),
            last_modified: headers.get(LAST_MODIFIED).cloned(),
        }
    }

    fn has_any(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
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
                    "must-revalidate" => out.must_revalidate = true,
                    "max-age" => {
                        if let Some(value) =
                            directive.value.as_deref().and_then(parse_delta_seconds)
                        {
                            out.max_age = Some(value);
                        }
                    }
                    "max-stale" => match directive.value.as_deref() {
                        Some(value) => {
                            if let Some(value) = parse_delta_seconds(value) {
                                out.max_stale = Some(MaxStale::Limit(value));
                            }
                        }
                        None => out.max_stale = Some(MaxStale::Any),
                    },
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
        && (!directives.max_age.is_some_and(|max_age| max_age.is_zero())
            || directives.max_stale.is_some());

    RequestCachePolicy {
        is_cacheable,
        lookup,
        store_response: is_cacheable && !directives.no_store,
        only_if_cached: is_cacheable && directives.only_if_cached,
        max_age: directives.max_age,
        max_stale: directives.max_stale,
        min_fresh: directives.min_fresh,
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
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

#[derive(Clone, PartialEq, Eq)]
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

    async fn get_candidates(&self, key: &str) -> Vec<CachedResponse> {
        self.get(key).await.into_iter().collect()
    }

    async fn put_candidate(&self, key: String, value: CachedResponse) {
        self.put(key, value).await;
    }
}

#[derive(Clone, Default)]
pub struct MemoryCacheStore {
    entries: Arc<RwLock<HashMap<String, Vec<CachedResponse>>>>,
}

#[async_trait]
impl CacheStore for MemoryCacheStore {
    async fn get(&self, key: &str) -> Option<CachedResponse> {
        self.entries
            .read()
            .await
            .get(key)
            .and_then(|entries| entries.last().cloned())
    }

    async fn put(&self, key: String, value: CachedResponse) {
        self.put_candidate(key, value).await;
    }

    async fn remove(&self, key: &str) {
        self.entries.write().await.remove(key);
    }

    async fn get_candidates(&self, key: &str) -> Vec<CachedResponse> {
        self.entries
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    async fn put_candidate(&self, key: String, value: CachedResponse) {
        let mut entries = self.entries.write().await;
        let candidates = entries.entry(key).or_default();
        if let Some(existing) = candidates
            .iter_mut()
            .find(|candidate| candidate.same_variant_as(&value))
        {
            *existing = value;
        } else {
            candidates.push(value);
        }
    }
}

#[derive(Clone)]
pub struct CachedResponse {
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    stored_at: Instant,
    initial_age: Duration,
    freshness_lifetime: Duration,
    vary: CapturedVary,
    must_validate: bool,
    must_revalidate: bool,
}

#[derive(Clone, Default)]
struct CachedResponseOptions {
    vary: CapturedVary,
    must_validate: bool,
    must_revalidate: bool,
}

impl CachedResponse {
    pub fn new(
        status: StatusCode,
        version: Version,
        headers: HeaderMap,
        body: Bytes,
        freshness_lifetime: Duration,
    ) -> Self {
        Self::new_with_vary(
            status,
            version,
            headers,
            body,
            freshness_lifetime,
            CachedResponseOptions::default(),
        )
    }

    fn new_with_vary(
        status: StatusCode,
        version: Version,
        headers: HeaderMap,
        body: Bytes,
        freshness_lifetime: Duration,
        options: CachedResponseOptions,
    ) -> Self {
        let stored_at = Instant::now();
        let initial_age = response_current_age(&headers, SystemTime::now());
        Self {
            status,
            version,
            headers,
            body,
            stored_at,
            initial_age,
            freshness_lifetime,
            vary: options.vary,
            must_validate: options.must_validate,
            must_revalidate: options.must_revalidate,
        }
    }

    fn is_servable_for(&self, request_policy: &RequestCachePolicy) -> bool {
        if self.must_validate {
            return false;
        }

        let current_age = self.current_age();
        let effective_freshness_lifetime = self.effective_freshness_lifetime(request_policy);
        if self.is_fresh_for(current_age, effective_freshness_lifetime, request_policy) {
            return true;
        }

        self.is_stale_acceptable_for(current_age, effective_freshness_lifetime, request_policy)
    }

    fn effective_freshness_lifetime(&self, request_policy: &RequestCachePolicy) -> Duration {
        request_policy
            .max_age
            .map(|max_age| self.freshness_lifetime.min(max_age))
            .unwrap_or(self.freshness_lifetime)
    }

    fn is_fresh_for(
        &self,
        current_age: Duration,
        effective_freshness_lifetime: Duration,
        request_policy: &RequestCachePolicy,
    ) -> bool {
        if current_age >= effective_freshness_lifetime {
            return false;
        }

        if let Some(min_fresh) = request_policy.min_fresh {
            let Some(required_age) = current_age.checked_add(min_fresh) else {
                return false;
            };
            return required_age <= effective_freshness_lifetime;
        }

        true
    }

    fn is_stale_acceptable_for(
        &self,
        current_age: Duration,
        effective_freshness_lifetime: Duration,
        request_policy: &RequestCachePolicy,
    ) -> bool {
        if self.must_revalidate || request_policy.min_fresh.is_some() {
            return false;
        }

        let staleness = current_age.saturating_sub(effective_freshness_lifetime);
        match request_policy.max_stale {
            Some(MaxStale::Any) => true,
            Some(MaxStale::Limit(limit)) => staleness <= limit,
            None => false,
        }
    }

    fn matches_request(&self, request_headers: &HeaderMap) -> bool {
        self.vary.matches(request_headers)
    }

    fn same_variant_as(&self, other: &Self) -> bool {
        self.vary == other.vary
    }

    fn can_revalidate(&self, request_headers: &HeaderMap) -> bool {
        !request_headers.contains_key(IF_NONE_MATCH)
            && !request_headers.contains_key(IF_MODIFIED_SINCE)
            && CacheValidators::from_headers(&self.headers).has_any()
    }

    fn apply_revalidation_headers(&self, request_headers: &mut HeaderMap) {
        let validators = CacheValidators::from_headers(&self.headers);
        if let Some(etag) = validators.etag {
            request_headers.insert(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = validators.last_modified {
            request_headers.insert(IF_MODIFIED_SINCE, last_modified);
        }
    }

    fn freshen_from_not_modified(
        &self,
        validation_headers: &HeaderMap,
        request_headers: &HeaderMap,
    ) -> (Self, bool) {
        let headers = merge_304_headers(&self.headers, validation_headers);
        let directives = CacheDirectives::from_headers(&headers);
        let validators = CacheValidators::from_headers(&headers);
        let freshness_lifetime =
            response_freshness_lifetime(&headers, &directives).unwrap_or_default();
        let vary = CapturedVary::capture(&headers, request_headers);
        let store_response = vary.is_some()
            && response_is_storable(
                &headers,
                self.status,
                &directives,
                &validators,
                freshness_lifetime,
            );
        let response = Self::new_with_vary(
            self.status,
            self.version,
            headers,
            self.body.clone(),
            freshness_lifetime,
            CachedResponseOptions {
                vary: vary.unwrap_or_default(),
                must_validate: directives.no_cache,
                must_revalidate: directives.must_revalidate,
            },
        );
        (response, store_response)
    }

    fn current_age(&self) -> Duration {
        self.initial_age.saturating_add(self.stored_at.elapsed())
    }

    fn into_response(self) -> Response<ResponseBody> {
        let age = self.current_age().as_secs().min(MAX_DELTA_SECONDS);
        let mut headers = self.headers;
        if let Ok(value) = HeaderValue::from_str(&age.to_string()) {
            headers.insert(AGE, value);
        }
        build_response(self.status, self.version, headers, self.body)
    }
}

fn response_is_storable(
    headers: &HeaderMap,
    status: StatusCode,
    directives: &CacheDirectives,
    validators: &CacheValidators,
    freshness_lifetime: Duration,
) -> bool {
    status == StatusCode::OK
        && !headers.contains_key(SET_COOKIE)
        && !directives.no_store
        && (!directives.no_cache || validators.has_any())
        && (!freshness_lifetime.is_zero() || validators.has_any())
}

fn response_freshness_lifetime(
    headers: &HeaderMap,
    directives: &CacheDirectives,
) -> Option<Duration> {
    let now = SystemTime::now();
    if let Some(max_age) = directives.max_age {
        return Some(max_age);
    }

    if let Some(freshness_lifetime) = explicit_expires_lifetime(headers, now) {
        return Some(freshness_lifetime);
    }

    heuristic_freshness_lifetime(headers, now).filter(|lifetime| !lifetime.is_zero())
}

fn explicit_expires_lifetime(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let expires = match headers.get(EXPIRES) {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .unwrap_or(SystemTime::UNIX_EPOCH),
        None => return None,
    };
    let date = parse_http_date_header(headers, DATE).unwrap_or(now);
    Some(expires.duration_since(date).unwrap_or_default())
}

fn heuristic_freshness_lifetime(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let last_modified = parse_http_date_header(headers, LAST_MODIFIED)?;
    let date = parse_http_date_header(headers, DATE).unwrap_or(now);
    let since_last_modified = date.duration_since(last_modified).ok()?;
    let heuristic_seconds = since_last_modified.as_secs() / LAST_MODIFIED_HEURISTIC_DENOMINATOR;
    (heuristic_seconds > 0).then(|| Duration::from_secs(heuristic_seconds))
}

fn parse_http_date_header(headers: &HeaderMap, name: HeaderName) -> Option<SystemTime> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
}

fn response_current_age(headers: &HeaderMap, now: SystemTime) -> Duration {
    let apparent_age = parse_http_date_header(headers, DATE)
        .and_then(|date| now.duration_since(date).ok())
        .unwrap_or_default();
    apparent_age.max(response_age(headers))
}

fn merge_304_headers(stored: &HeaderMap, validation: &HeaderMap) -> HeaderMap {
    let mut headers = stored.clone();
    headers.remove(AGE);
    for name in validation.keys() {
        if !should_update_from_304(name) {
            continue;
        }

        headers.remove(name);
        for value in validation.get_all(name) {
            headers.append(name, value.clone());
        }
    }
    headers
}

fn should_update_from_304(name: &HeaderName) -> bool {
    *name != CONTENT_LENGTH && *name != CONTENT_ENCODING && *name != TRANSFER_ENCODING
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
