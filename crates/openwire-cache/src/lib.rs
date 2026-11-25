use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{
    AGE, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_LOCATION, DATE,
    ETAG, EXPIRES, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION, PRAGMA, SET_COOKIE,
    TRANSFER_ENCODING, VARY,
};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use openwire::{
    BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, ResponseTiming, WireError,
};
use tokio::sync::RwLock;
use url::Url;

const MAX_DELTA_SECONDS: u64 = 2_147_483_648;
const LAST_MODIFIED_HEURISTIC_DENOMINATOR: u64 = 10;
/// Default maximum number of URI keys retained by [`MemoryCacheStore`].
pub const DEFAULT_MEMORY_CACHE_MAX_ENTRIES: usize = 1_024;
/// Default maximum total cached body bytes retained by [`MemoryCacheStore`].
pub const DEFAULT_MEMORY_CACHE_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

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
            let ctx = exchange.context().clone();
            let invalidation_target = request_method_invalidates_cache(exchange.request().method())
                .then(|| exchange.request().uri().clone());
            let request_policy = request_cache_policy(exchange.request());
            let cache_key = request_policy
                .is_cacheable
                .then(|| cache_key(exchange.request().uri()));
            let request_headers = exchange.request().headers().clone();

            let mut validation_entry = None;
            if let Some(cache_key) = cache_key.as_ref() {
                for entry in store.get_candidates(cache_key).await.into_iter().rev() {
                    if entry.matches_request(&request_policy, &request_headers) {
                        if request_policy.lookup && entry.is_servable_for(&request_policy) {
                            let response = entry.into_response();
                            ctx.listener().cache_hit(&ctx, &response);
                            return Ok(response);
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
                    let response = gateway_timeout_response();
                    ctx.listener().satisfaction_failure(&ctx, &response);
                    return Ok(response);
                }
            }

            if let Some(entry) = validation_entry.as_ref() {
                let cached = entry.clone().into_response();
                ctx.listener().cache_conditional_hit(&ctx, &cached);
                entry.apply_revalidation_headers(exchange.request_mut().headers_mut());
            } else if cache_key.is_some() {
                ctx.listener().cache_miss(&ctx);
            }

            let response = next.run(exchange).await?;
            let timing = response.extensions().get::<ResponseTiming>().copied();
            if let Some(invalidation_target) = invalidation_target.as_ref() {
                if response_status_invalidates_cache(response.status()) {
                    for key in invalidation_keys(invalidation_target, response.headers()) {
                        store.remove(&key).await;
                    }
                }
            }

            let Some(cache_key) = cache_key else {
                return Ok(response);
            };

            if let Some(entry) = validation_entry.as_ref() {
                if response.status() == StatusCode::NOT_MODIFIED {
                    let (parts, body) = response.into_parts();
                    let _ = body.bytes().await?;
                    let Some(timing) = timing else {
                        // The conditional request was ours, so still reconstruct
                        // its response. Without exchange timing, do not publish
                        // a refreshed entry with an underestimated age.
                        store.remove_candidate(&cache_key, entry).await;
                        let mut validation_headers = parts.headers;
                        ensure_response_date(&mut validation_headers, SystemTime::now());
                        let response = build_response(
                            entry.status,
                            entry.version,
                            merge_304_headers(&entry.headers, &validation_headers),
                            entry.body.clone(),
                        );
                        ctx.listener().cache_hit(&ctx, &response);
                        return Ok(response);
                    };
                    let (freshened, store_response) =
                        entry.freshen_from_not_modified(&parts.headers, &request_headers, timing);
                    if store_response {
                        store.put_candidate(cache_key, freshened.clone()).await;
                    } else {
                        store.remove_candidate(&cache_key, entry).await;
                    }
                    let response = freshened.into_response();
                    ctx.listener().cache_hit(&ctx, &response);
                    return Ok(response);
                }
            }

            if !request_policy.store_response {
                return Ok(response);
            }

            let Some(timing) = timing else {
                if response.status() == StatusCode::OK {
                    remove_unusable_candidate(
                        store.as_ref(),
                        &cache_key,
                        validation_entry.as_ref(),
                        &request_policy,
                        &request_headers,
                    )
                    .await;
                }
                return Ok(response);
            };

            let (mut parts, body) = response.into_parts();
            ensure_response_date(&mut parts.headers, timing.received_at_wall());
            let response_directives = CacheDirectives::from_headers(&parts.headers);
            if response_directives.no_store {
                remove_unusable_candidate(
                    store.as_ref(),
                    &cache_key,
                    validation_entry.as_ref(),
                    &request_policy,
                    &request_headers,
                )
                .await;
                return Ok(Response::from_parts(parts, body));
            }

            let validators = CacheValidators::from_headers(&parts.headers);
            let freshness_lifetime = response_freshness_lifetime(
                &parts.headers,
                &response_directives,
                timing.received_at_wall(),
            )
            .unwrap_or_default();
            if !response_is_storable(
                &parts.headers,
                parts.status,
                &response_directives,
                &validators,
                freshness_lifetime,
                request_policy.has_authorization,
            ) {
                if parts.status == StatusCode::OK {
                    remove_unusable_candidate(
                        store.as_ref(),
                        &cache_key,
                        validation_entry.as_ref(),
                        &request_policy,
                        &request_headers,
                    )
                    .await;
                }
                return Ok(Response::from_parts(parts, body));
            }

            let Some(mut vary) = CapturedVary::capture(&parts.headers, &request_headers) else {
                remove_unusable_candidate(
                    store.as_ref(),
                    &cache_key,
                    validation_entry.as_ref(),
                    &request_policy,
                    &request_headers,
                )
                .await;
                return Ok(Response::from_parts(parts, body));
            };
            if request_policy.has_authorization {
                vary.include_request_header(AUTHORIZATION, &request_headers);
            }

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
                            timing: Some(timing),
                        },
                    ),
                )
                .await;

            Ok(Response::from_parts(parts, ResponseBody::from_bytes(body)))
        })
    }
}

#[derive(Clone, Debug, Default)]
struct RequestCachePolicy {
    is_cacheable: bool,
    has_authorization: bool,
    lookup: bool,
    store_response: bool,
    only_if_cached: bool,
    max_age: Option<Duration>,
    max_stale: Option<MaxStale>,
    min_fresh: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
struct CacheDirectives {
    public: bool,
    no_cache: bool,
    no_store: bool,
    only_if_cached: bool,
    max_age: Option<Duration>,
    s_maxage: Option<Duration>,
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
                    "public" => out.public = true,
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
                    "s-maxage" => {
                        if let Some(value) =
                            directive.value.as_deref().and_then(parse_delta_seconds)
                        {
                            out.s_maxage = Some(value);
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
    let directives = request_cache_directives(request.headers());
    let has_authorization = request.headers().contains_key(AUTHORIZATION);
    let is_cacheable =
        request.method() == Method::GET && request.body().replayable_len() == Some(0);
    let lookup = is_cacheable
        && !directives.no_cache
        && (!directives.max_age.is_some_and(|max_age| max_age.is_zero())
            || directives.max_stale.is_some());

    RequestCachePolicy {
        is_cacheable,
        has_authorization,
        lookup,
        store_response: is_cacheable && !directives.no_store,
        only_if_cached: is_cacheable && directives.only_if_cached,
        max_age: directives.max_age,
        max_stale: directives.max_stale,
        min_fresh: directives.min_fresh,
    }
}

fn request_cache_directives(headers: &HeaderMap) -> CacheDirectives {
    let mut directives = CacheDirectives::from_headers(headers);
    if !headers.contains_key(CACHE_CONTROL) && request_pragma_no_cache(headers) {
        directives.no_cache = true;
    }
    directives
}

fn request_pragma_no_cache(headers: &HeaderMap) -> bool {
    headers.get_all(PRAGMA).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            cache_control_directives(value).any(|directive| directive.name == "no-cache")
        })
    })
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

    fn include_request_header(&mut self, name: HeaderName, request_headers: &HeaderMap) {
        if let Some(field) = self.fields.iter_mut().find(|field| field.name == name) {
            field.values = request_headers.get_all(&name).iter().cloned().collect();
            return;
        }

        self.fields.push(VaryField {
            values: request_headers.get_all(&name).iter().cloned().collect(),
            name,
        });
    }

    fn tracks_request_header(&self, name: HeaderName) -> bool {
        self.fields.iter().any(|field| field.name == name)
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

    async fn remove_candidate(&self, key: &str, value: &CachedResponse) {
        let _ = value;
        self.remove(key).await;
    }
}

#[derive(Clone)]
pub struct MemoryCacheStore {
    entries: Arc<RwLock<HashMap<String, Vec<CachedResponse>>>>,
    max_entries: usize,
    max_body_bytes: usize,
}

impl Default for MemoryCacheStore {
    fn default() -> Self {
        Self::with_limits(
            DEFAULT_MEMORY_CACHE_MAX_ENTRIES,
            DEFAULT_MEMORY_CACHE_MAX_BODY_BYTES,
        )
    }
}

impl MemoryCacheStore {
    /// Creates a memory store with default entry and body-byte limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a memory store with explicit capacity limits.
    pub fn with_limits(max_entries: usize, max_body_bytes: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_entries: max_entries.max(1),
            max_body_bytes,
        }
    }

    fn total_body_bytes(entries: &HashMap<String, Vec<CachedResponse>>) -> usize {
        entries
            .values()
            .flat_map(|candidates| candidates.iter())
            .map(|entry| entry.body.len())
            .sum()
    }

    fn enforce_limits(
        entries: &mut HashMap<String, Vec<CachedResponse>>,
        max_entries: usize,
        max_body_bytes: usize,
    ) {
        while entries.len() > max_entries {
            if let Some(oldest) = entries.keys().next().cloned() {
                entries.remove(&oldest);
            } else {
                break;
            }
        }

        while Self::total_body_bytes(entries) > max_body_bytes && !entries.is_empty() {
            if let Some(oldest) = entries.keys().next().cloned() {
                entries.remove(&oldest);
            } else {
                break;
            }
        }
    }
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
        if value.body.len() > self.max_body_bytes {
            // Oversized bodies are never stored.
            return;
        }
        let candidates = entries.entry(key).or_default();
        if let Some(existing) = candidates
            .iter_mut()
            .find(|candidate| candidate.same_variant_as(&value))
        {
            *existing = value;
        } else {
            candidates.push(value);
        }
        Self::enforce_limits(&mut entries, self.max_entries, self.max_body_bytes);
    }

    async fn remove_candidate(&self, key: &str, value: &CachedResponse) {
        let mut entries = self.entries.write().await;
        let Some(candidates) = entries.get_mut(key) else {
            return;
        };
        candidates.retain(|candidate| !candidate.same_variant_as(value));
        if candidates.is_empty() {
            entries.remove(key);
        }
    }
}

#[derive(Clone)]
pub struct CachedResponse {
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    received_at: Instant,
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
    timing: Option<ResponseTiming>,
}

impl CachedResponse {
    /// Creates a manually supplied entry received at the current time.
    ///
    /// No request timing is available here, so transfer delay is assumed zero.
    /// Automatic interceptor storage instead uses the response's `ResponseTiming`.
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
        mut headers: HeaderMap,
        body: Bytes,
        freshness_lifetime: Duration,
        options: CachedResponseOptions,
    ) -> Self {
        let timing = options.timing.unwrap_or_else(|| {
            let now = Instant::now();
            ResponseTiming::new(now, now, SystemTime::now())
        });
        ensure_response_date(&mut headers, timing.received_at_wall());
        let initial_age = response_current_age(&headers, timing);
        Self {
            status,
            version,
            headers,
            body,
            received_at: timing.received_at(),
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

    fn matches_request(
        &self,
        request_policy: &RequestCachePolicy,
        request_headers: &HeaderMap,
    ) -> bool {
        if request_policy.has_authorization && !self.vary.tracks_request_header(AUTHORIZATION) {
            return false;
        }
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
        timing: ResponseTiming,
    ) -> (Self, bool) {
        let mut validation_headers = validation_headers.clone();
        ensure_response_date(&mut validation_headers, timing.received_at_wall());
        let headers = merge_304_headers(&self.headers, &validation_headers);
        let directives = CacheDirectives::from_headers(&headers);
        let validators = CacheValidators::from_headers(&headers);
        let freshness_lifetime =
            response_freshness_lifetime(&headers, &directives, timing.received_at_wall())
                .unwrap_or_default();
        let mut vary = CapturedVary::capture(&headers, request_headers);
        if let Some(vary) = vary.as_mut() {
            if self.vary.tracks_request_header(AUTHORIZATION) {
                vary.include_request_header(AUTHORIZATION, request_headers);
            }
        }
        let store_response = vary.is_some()
            && response_is_storable(
                &headers,
                self.status,
                &directives,
                &validators,
                freshness_lifetime,
                request_headers.contains_key(AUTHORIZATION),
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
                timing: Some(timing),
            },
        );
        (response, store_response)
    }

    fn current_age(&self) -> Duration {
        self.current_age_at(Instant::now())
    }

    fn current_age_at(&self, now: Instant) -> Duration {
        self.initial_age
            .saturating_add(now.saturating_duration_since(self.received_at))
    }

    fn into_response(self) -> Response<ResponseBody> {
        self.into_response_at(Instant::now())
    }

    fn into_response_at(self, now: Instant) -> Response<ResponseBody> {
        let age = self.current_age_at(now).as_secs().min(MAX_DELTA_SECONDS);
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
    request_has_authorization: bool,
) -> bool {
    status == StatusCode::OK
        && !headers.contains_key(SET_COOKIE)
        && !directives.no_store
        && (!request_has_authorization || response_allows_authorized_storage(directives))
        && (!directives.no_cache || validators.has_any())
        && (!freshness_lifetime.is_zero() || validators.has_any())
}

fn response_allows_authorized_storage(directives: &CacheDirectives) -> bool {
    directives.public || directives.s_maxage.is_some() || directives.must_revalidate
}

async fn remove_unusable_candidate<S>(
    store: &S,
    cache_key: &str,
    validation_entry: Option<&CachedResponse>,
    request_policy: &RequestCachePolicy,
    request_headers: &HeaderMap,
) where
    S: CacheStore,
{
    if request_policy.has_authorization {
        if let Some(entry) = validation_entry {
            store.remove_candidate(cache_key, entry).await;
            return;
        }

        for entry in store.get_candidates(cache_key).await {
            if entry.matches_request(request_policy, request_headers) {
                store.remove_candidate(cache_key, &entry).await;
            }
        }
        return;
    }

    store.remove(cache_key).await;
}

fn request_method_invalidates_cache(method: &Method) -> bool {
    !matches!(
        method,
        &Method::GET | &Method::HEAD | &Method::OPTIONS | &Method::TRACE
    )
}

fn response_status_invalidates_cache(status: StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

fn invalidation_keys(target_uri: &Uri, response_headers: &HeaderMap) -> Vec<String> {
    let mut keys = vec![cache_key(target_uri)];
    for name in [LOCATION, CONTENT_LOCATION] {
        for value in response_headers.get_all(name) {
            let Some(uri) = same_host_invalidation_uri(target_uri, value) else {
                continue;
            };
            let key = cache_key(&uri);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

fn same_host_invalidation_uri(target_uri: &Uri, value: &HeaderValue) -> Option<Uri> {
    let target = Url::parse(&target_uri.to_string()).ok()?;
    let value = value.to_str().ok()?;
    let uri = target.join(value).ok()?;
    if !same_host(&target, &uri) {
        return None;
    }
    uri.as_str().parse().ok()
}

fn same_host(left: &Url, right: &Url) -> bool {
    left.host_str()
        .zip(right.host_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn response_freshness_lifetime(
    headers: &HeaderMap,
    directives: &CacheDirectives,
    now: SystemTime,
) -> Option<Duration> {
    if cache_control_directive_count(headers, "max-age") > 1 {
        return Some(Duration::ZERO);
    }
    if let Some(max_age) = directives.max_age {
        return Some(max_age);
    }

    if header_value_count(headers, EXPIRES) > 1 {
        return Some(Duration::ZERO);
    }
    if let Some(freshness_lifetime) = explicit_expires_lifetime(headers, now) {
        return Some(freshness_lifetime);
    }

    heuristic_freshness_lifetime(headers, now).filter(|lifetime| !lifetime.is_zero())
}

fn cache_control_directive_count(headers: &HeaderMap, name: &str) -> usize {
    headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(cache_control_directives)
        .filter(|directive| directive.name == name)
        .count()
}

fn header_value_count(headers: &HeaderMap, name: HeaderName) -> usize {
    headers.get_all(name).iter().count()
}

fn explicit_expires_lifetime(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let expires = headers
        .get(EXPIRES)?
        .to_str()
        .ok()
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);
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

fn ensure_response_date(headers: &mut HeaderMap, received_at: SystemTime) {
    // RFC 9110 §6.6.1: a recipient with a clock records a missing Date at
    // receipt when caching. A 304's missing Date must not inherit the old Date.
    if !headers.contains_key(DATE) {
        headers.insert(
            DATE,
            HeaderValue::from_str(&httpdate::fmt_http_date(received_at))
                .expect("formatted HTTP date is a valid header value"),
        );
    }
}

fn response_current_age(headers: &HeaderMap, timing: ResponseTiming) -> Duration {
    // RFC 9111 §4.2.3: preserve fractional durations, then choose the
    // conservative maximum before adding residence since header receipt.
    let apparent_age = parse_http_date_header(headers, DATE)
        .and_then(|date| timing.received_at_wall().duration_since(date).ok())
        .unwrap_or_default();
    let corrected_age_value = response_age(headers).saturating_add(timing.response_delay());
    apparent_age.max(corrected_age_value)
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
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|value| {
            value.bytes().fold(0u64, |seconds, digit| {
                seconds
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit - b'0'))
                    .min(MAX_DELTA_SECONDS)
            })
        })
        .map(Duration::from_secs)
        .unwrap_or_default()
}

fn cache_key(uri: &Uri) -> String {
    normalize_cache_uri(uri)
}

fn normalize_cache_uri(uri: &Uri) -> String {
    let Ok(mut url) = Url::parse(&uri.to_string()) else {
        return uri.to_string();
    };

    if let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) {
        let _ = url.set_host(Some(&host));
    }

    // Drop default ports so http://example.com and http://example.com:80 share a key.
    match url.scheme() {
        "http" if url.port() == Some(80) => {
            let _ = url.set_port(None);
        }
        "https" if url.port() == Some(443) => {
            let _ = url.set_port(None);
        }
        _ => {}
    }

    if url.path().is_empty() {
        url.set_path("/");
    }

    url.as_str().to_owned()
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
    use std::time::{Duration, Instant, SystemTime};

    use bytes::Bytes;
    use http::header::{AGE, CONTENT_LOCATION, DATE, LOCATION};
    use http::{HeaderMap, StatusCode, Uri, Version};
    use openwire::ResponseTiming;

    use super::{
        invalidation_keys, parse_delta_seconds, response_age, response_current_age, CachedResponse,
        CachedResponseOptions, RequestCachePolicy, MAX_DELTA_SECONDS,
    };

    #[test]
    fn response_age_includes_request_delay_and_residence() {
        // RFC 9111 §4.2.3: request=1000, response=1002, Date=900,
        // Age=200, now=1005. Corrected initial age is 202, residence is 3.
        let mut headers = HeaderMap::new();
        headers.insert(AGE, "200".parse().unwrap());
        headers.insert(
            DATE,
            httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(900))
                .parse()
                .unwrap(),
        );
        let request_sent_at = Instant::now();
        let timing = ResponseTiming::new(
            request_sent_at,
            request_sent_at + Duration::from_secs(2),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1002),
        );
        let initial_age = response_current_age(&headers, timing);
        assert_eq!(
            initial_age + Duration::from_secs(3),
            Duration::from_secs(205)
        );
        let entry = timed_entry(headers, timing, Duration::from_secs(205));
        let now = request_sent_at + Duration::from_secs(5);
        assert_eq!(entry.current_age_at(now), Duration::from_secs(205));
        assert!(!entry.is_fresh_for(
            entry.current_age_at(now),
            Duration::from_secs(205),
            &RequestCachePolicy::default(),
        ));
        assert_eq!(entry.into_response_at(now).headers()[AGE], "205");
    }

    fn timed_entry(
        headers: HeaderMap,
        timing: ResponseTiming,
        freshness_lifetime: Duration,
    ) -> CachedResponse {
        CachedResponse::new_with_vary(
            StatusCode::OK,
            Version::HTTP_11,
            headers,
            Bytes::from_static(b"stored body"),
            freshness_lifetime,
            CachedResponseOptions {
                timing: Some(timing),
                ..CachedResponseOptions::default()
            },
        )
    }

    #[test]
    fn response_age_vectors_choose_corrected_or_apparent_age() {
        // RFC 9111 §4.2.3. Request=100, response=105, now=112.
        // Expected values are literal applications of the RFC formula.
        for (name, date, age, expected) in [
            ("corrected_age", Some(103), Some("10"), 22),
            ("apparent_age", Some(50), Some("10"), 62),
            ("future_date", Some(120), None, 12),
            ("absent_fields", None, None, 12),
            ("invalid_date", None, Some("10"), 22),
        ] {
            let request = Instant::now();
            let received = request + Duration::from_secs(5);
            let timing = ResponseTiming::new(
                request,
                received,
                SystemTime::UNIX_EPOCH + Duration::from_secs(105),
            );
            let mut headers = HeaderMap::new();
            if let Some(date) = date {
                headers.insert(
                    DATE,
                    httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(date))
                        .parse()
                        .unwrap(),
                );
            } else if name == "invalid_date" {
                headers.insert(DATE, "not a date".parse().unwrap());
            }
            if let Some(age) = age {
                headers.insert(AGE, age.parse().unwrap());
            }
            let entry = timed_entry(headers, timing, Duration::from_secs(1000));
            assert_eq!(
                entry.current_age_at(request + Duration::from_secs(12)),
                Duration::from_secs(expected),
                "{name}",
            );
            if name == "absent_fields" {
                assert_eq!(
                    entry.headers[DATE],
                    httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(105)),
                );
            }
        }
    }

    #[test]
    fn response_age_keeps_fractional_delay_and_residence_until_serialization() {
        let request = Instant::now();
        let received = request + Duration::from_millis(600);
        let timing = ResponseTiming::new(request, received, SystemTime::UNIX_EPOCH);
        let entry = timed_entry(HeaderMap::new(), timing, Duration::from_secs(10));
        let now = request + Duration::from_millis(1200);
        assert_eq!(entry.current_age_at(now), Duration::from_millis(1200));
        assert_eq!(entry.into_response_at(now).headers()[AGE], "1");
    }

    #[test]
    fn buffering_before_storage_does_not_restart_response_age() {
        // Model storage three seconds after receipt without sleeping. The
        // explicit lookup time is independent of constructor execution time.
        let stored_at = Instant::now();
        let received_at = stored_at - Duration::from_secs(3);
        let timing = ResponseTiming::new(
            received_at - Duration::from_secs(2),
            received_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1002),
        );
        let mut headers = HeaderMap::new();
        headers.insert(AGE, "200".parse().unwrap());
        headers.insert(
            DATE,
            httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(900))
                .parse()
                .unwrap(),
        );
        let entry = timed_entry(headers, timing, Duration::from_secs(1000));
        assert_eq!(entry.current_age_at(stored_at), Duration::from_secs(205));
        assert_eq!(
            entry.current_age_at(stored_at + Duration::from_secs(4)),
            Duration::from_secs(209),
        );
    }

    #[test]
    fn validation_age_uses_the_new_exchange_and_not_stored_date_or_age() {
        // RFC 9111 §§4.2.3, 4.3.3–4; RFC 9110 §6.6.1.
        // Old Age=900/Date=0; validation request=200, response=205, now=208.
        for (date, age, expected) in [
            (Some(203), Some("7"), 15),
            (Some(203), None, 8),
            (None, None, 8),
            (Some(0), None, 208),
        ] {
            let request = Instant::now();
            let timing = ResponseTiming::new(
                request,
                request + Duration::from_secs(5),
                SystemTime::UNIX_EPOCH + Duration::from_secs(205),
            );
            let mut old_headers = HeaderMap::new();
            old_headers.insert(AGE, "900".parse().unwrap());
            old_headers.insert(
                DATE,
                httpdate::fmt_http_date(SystemTime::UNIX_EPOCH)
                    .parse()
                    .unwrap(),
            );
            old_headers.insert(http::header::ETAG, "\"v1\"".parse().unwrap());
            let old = timed_entry(old_headers, timing, Duration::ZERO);
            let mut validation_headers = HeaderMap::new();
            validation_headers.insert(http::header::CACHE_CONTROL, "max-age=1000".parse().unwrap());
            if let Some(date) = date {
                validation_headers.insert(
                    DATE,
                    httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(date))
                        .parse()
                        .unwrap(),
                );
            }
            if let Some(age) = age {
                validation_headers.insert(AGE, age.parse().unwrap());
            }
            let (freshened, store) =
                old.freshen_from_not_modified(&validation_headers, &HeaderMap::new(), timing);
            assert!(store);
            assert_eq!(freshened.body, old.body);
            let now = request + Duration::from_secs(8);
            assert_eq!(freshened.current_age_at(now), Duration::from_secs(expected));
            assert_eq!(
                freshened.into_response_at(now).headers()[AGE],
                expected.to_string()
            );
        }
    }

    #[test]
    fn response_age_uses_first_list_member_and_rejects_invalid_values() {
        // RFC 9111 §5.1: first-member handling is the selected SHOULD behavior.
        for (value, expected) in [
            ("5, 99", 5),
            ("bad, 5", 0),
            ("+1", 0),
            ("-1", 0),
            ("1.5", 0),
            ("", 0),
            ("0005", 5),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(AGE, value.parse().unwrap());
            assert_eq!(
                response_age(&headers),
                Duration::from_secs(expected),
                "{value:?}"
            );
        }
    }

    #[test]
    fn response_age_overflow_saturates_instead_of_becoming_zero() {
        // RFC 9111 §1.2.2: a valid delta-seconds overflow is never a young age.
        let mut headers = HeaderMap::new();
        headers.insert(
            AGE,
            "999999999999999999999999999999999999999999999"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            response_age(&headers),
            Duration::from_secs(MAX_DELTA_SECONDS)
        );
    }

    #[test]
    fn delta_seconds_are_clamped_for_overflow_safety() {
        assert_eq!(parse_delta_seconds("60"), Some(Duration::from_secs(60)));
        assert_eq!(
            parse_delta_seconds("999999999999999999999999999"),
            Some(Duration::from_secs(MAX_DELTA_SECONDS))
        );
    }

    #[test]
    fn invalidation_keys_include_same_host_location_uris() {
        let target: Uri = "http://example.com/mutate".parse().expect("target");
        let mut headers = HeaderMap::new();
        headers.append(LOCATION, "/resource".parse().expect("location"));
        headers.append(
            CONTENT_LOCATION,
            "http://example.com/representation"
                .parse()
                .expect("content-location"),
        );
        headers.append(
            LOCATION,
            "http://elsewhere.example/resource"
                .parse()
                .expect("cross-host location"),
        );

        assert_eq!(
            invalidation_keys(&target, &headers),
            vec![
                "http://example.com/mutate".to_owned(),
                "http://example.com/resource".to_owned(),
                "http://example.com/representation".to_owned(),
            ]
        );
    }
}
