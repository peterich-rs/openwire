use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CACHE_CONTROL, SET_COOKIE, VARY};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use openwire::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};
use tokio::sync::RwLock;

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
            let cacheable_request = request_is_cacheable(exchange.request());
            let cache_key = cacheable_request.then(|| cache_key(exchange.request().uri()));

            if let Some(cache_key) = cache_key.as_ref() {
                if let Some(entry) = store.get(cache_key).await {
                    if entry.is_fresh() {
                        return Ok(entry.into_response());
                    }

                    store.remove(cache_key).await;
                }
            }

            let response = next.run(exchange).await?;
            let Some(cache_key) = cache_key else {
                return Ok(response);
            };

            let (parts, body) = response.into_parts();
            let Some(fresh_for) = response_freshness(&parts.headers) else {
                return Ok(Response::from_parts(parts, body));
            };

            if !response_is_cacheable(&parts.headers, parts.status) {
                return Ok(Response::from_parts(parts, body));
            }

            let body = body.bytes().await?;
            let cached_headers = parts.headers.clone();
            store
                .put(
                    cache_key,
                    CachedResponse::new(
                        parts.status,
                        parts.version,
                        cached_headers,
                        body.clone(),
                        fresh_for,
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
    fresh_until: Instant,
}

impl CachedResponse {
    pub fn new(
        status: StatusCode,
        version: Version,
        headers: HeaderMap,
        body: Bytes,
        fresh_for: Duration,
    ) -> Self {
        Self {
            status,
            version,
            headers,
            body,
            fresh_until: Instant::now() + fresh_for,
        }
    }

    fn is_fresh(&self) -> bool {
        Instant::now() < self.fresh_until
    }

    fn into_response(self) -> Response<ResponseBody> {
        build_response(self.status, self.version, self.headers, self.body)
    }
}

fn request_is_cacheable(request: &Request<RequestBody>) -> bool {
    request.method() == Method::GET
        && request.body().replayable_len() == Some(0)
        && !request.headers().contains_key(AUTHORIZATION)
}

fn response_is_cacheable(headers: &HeaderMap, status: StatusCode) -> bool {
    status == StatusCode::OK
        && !headers.contains_key(SET_COOKIE)
        && !headers.contains_key(VARY)
        && !cache_control_contains(headers, "no-store")
}

fn response_freshness(headers: &HeaderMap) -> Option<Duration> {
    let cache_control = headers.get(CACHE_CONTROL)?.to_str().ok()?;
    for directive in cache_control.split(',').map(|directive| directive.trim()) {
        if directive.eq_ignore_ascii_case("no-store") {
            return None;
        }

        let Some(value) = directive.strip_prefix("max-age=") else {
            continue;
        };
        let seconds = value.parse::<u64>().ok()?;
        if seconds == 0 {
            return None;
        }
        return Some(Duration::from_secs(seconds));
    }

    None
}

fn cache_control_contains(headers: &HeaderMap, needle: &str) -> bool {
    headers
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(|directive| directive.trim())
                .any(|directive| directive.eq_ignore_ascii_case(needle))
        })
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
