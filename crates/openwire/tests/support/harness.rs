#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use http::header::{
    ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING, USER_AGENT,
};
use http::{Request, Response};
use hyper::rt::{Sleep, Timer};
use openwire::{
    AuthChallenge, AuthContext, Authenticator, BoxFuture, BoxTaskHandle, CallContext,
    DefaultRoutePlanner, DnsResolver, Exchange, Interceptor, Next, Proxy, ProxyRules,
    ProxySelection, ProxySelector, RedirectContext, RedirectDecision, RedirectPolicy, RequestBody,
    ResponseBody, RetryContext, RetryPolicy, RoutePlan, RoutePlanner, TaskHandle, TcpConnector,
    TlsConnector, WireError,
};
use openwire_core::BoxConnection;
use openwire_core::WireExecutor;
use openwire_tokio::TokioTcpConnector;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone)]
pub struct RecordingInterceptor {
    pub label: &'static str,
    pub order: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone)]
pub struct StaticAuthorizationAuthenticator {
    pub header_value: &'static str,
    pub calls: Arc<AtomicUsize>,
    pub observed_auth_counts: Arc<Mutex<Vec<u32>>>,
    pub observed_challenges: Arc<Mutex<Vec<Vec<AuthChallenge>>>>,
}

impl StaticAuthorizationAuthenticator {
    pub fn new(header_value: &'static str) -> Self {
        Self {
            header_value,
            calls: Arc::new(AtomicUsize::new(0)),
            observed_auth_counts: Arc::new(Mutex::new(Vec::new())),
            observed_challenges: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub fn observed_auth_counts(&self) -> Vec<u32> {
        self.observed_auth_counts
            .lock()
            .expect("auth counts")
            .clone()
    }

    pub fn observed_challenges(&self) -> Vec<Vec<AuthChallenge>> {
        self.observed_challenges
            .lock()
            .expect("auth challenges")
            .clone()
    }
}

impl Authenticator for StaticAuthorizationAuthenticator {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let header_value = self.header_value;
        let calls = self.calls.clone();
        let observed_auth_counts = self.observed_auth_counts.clone();
        let observed_challenges = self.observed_challenges.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            observed_auth_counts
                .lock()
                .expect("auth counts")
                .push(ctx.auth_count());
            observed_challenges
                .lock()
                .expect("auth challenges")
                .push(ctx.challenges());
            let Some(mut request) = ctx.try_clone_request() else {
                return Ok(None);
            };
            request
                .headers_mut()
                .insert(AUTHORIZATION, http::HeaderValue::from_static(header_value));
            Ok(Some(request))
        })
    }
}

#[derive(Clone, Default)]
pub struct DecliningAuthenticator {
    pub calls: Arc<AtomicUsize>,
}

impl DecliningAuthenticator {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedAuthAttempt {
    pub total_attempt: u32,
    pub retry_count: u32,
    pub redirect_count: u32,
    pub auth_count: u32,
}

#[derive(Clone)]
pub struct StaticHeaderAuthenticator {
    pub header_name: &'static str,
    pub header_value: &'static str,
    pub calls: Arc<AtomicUsize>,
    pub observed_kinds: Arc<Mutex<Vec<openwire::AuthKind>>>,
    pub observed_challenges: Arc<Mutex<Vec<Vec<AuthChallenge>>>>,
    pub observed_attempts: Arc<Mutex<Vec<ObservedAuthAttempt>>>,
}

impl StaticHeaderAuthenticator {
    pub fn new(header_name: &'static str, header_value: &'static str) -> Self {
        Self {
            header_name,
            header_value,
            calls: Arc::new(AtomicUsize::new(0)),
            observed_kinds: Arc::new(Mutex::new(Vec::new())),
            observed_challenges: Arc::new(Mutex::new(Vec::new())),
            observed_attempts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub fn observed_kinds(&self) -> Vec<openwire::AuthKind> {
        self.observed_kinds.lock().expect("auth kinds").clone()
    }

    pub fn observed_challenges(&self) -> Vec<Vec<AuthChallenge>> {
        self.observed_challenges
            .lock()
            .expect("auth challenges")
            .clone()
    }

    pub fn observed_attempts(&self) -> Vec<ObservedAuthAttempt> {
        self.observed_attempts
            .lock()
            .expect("auth attempts")
            .clone()
    }
}

impl Authenticator for StaticHeaderAuthenticator {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let header_name = self.header_name;
        let header_value = self.header_value;
        let calls = self.calls.clone();
        let observed_kinds = self.observed_kinds.clone();
        let observed_challenges = self.observed_challenges.clone();
        let observed_attempts = self.observed_attempts.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            observed_kinds.lock().expect("auth kinds").push(ctx.kind());
            observed_challenges
                .lock()
                .expect("auth challenges")
                .push(ctx.challenges());
            observed_attempts
                .lock()
                .expect("auth attempts")
                .push(ObservedAuthAttempt {
                    total_attempt: ctx.total_attempt(),
                    retry_count: ctx.retry_count(),
                    redirect_count: ctx.redirect_count(),
                    auth_count: ctx.auth_count(),
                });
            let Some(mut request) = ctx.try_clone_request() else {
                return Ok(None);
            };
            request.headers_mut().insert(
                http::header::HeaderName::from_static(header_name),
                http::HeaderValue::from_static(header_value),
            );
            Ok(Some(request))
        })
    }
}

impl Authenticator for DecliningAuthenticator {
    fn authenticate(
        &self,
        _ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        })
    }
}

pub fn assert_single_realm_challenge(
    challenges: &[AuthChallenge],
    expected_scheme: &str,
    expected_realm: &str,
) {
    assert_eq!(challenges.len(), 1, "challenges = {challenges:?}");
    assert_eq!(challenges[0].scheme(), expected_scheme);
    assert_eq!(challenges[0].realm(), Some(expected_realm));
}

impl RecordingInterceptor {
    pub fn new(label: &'static str, order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self { label, order }
    }
}

impl Interceptor for RecordingInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let label = self.label;
        let order = self.order.clone();
        Box::pin(async move {
            order.lock().expect("order").push(match label {
                "app" => "app:before",
                _ => "net:before",
            });
            let response = next.run(exchange).await;
            order.lock().expect("order").push(match label {
                "app" => "app:after",
                _ => "net:after",
            });
            response
        })
    }
}

#[derive(Clone, Default)]
pub struct HeaderCaptureInterceptor {
    pub seen: Arc<Mutex<Vec<ObservedHeaders>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetainedRequestMarker(pub &'static str);

#[derive(Clone, Default)]
pub struct ExtensionCaptureInterceptor {
    pub seen: Arc<Mutex<Vec<Option<&'static str>>>>,
}

#[derive(Clone)]
pub struct RewriteUriInterceptor {
    pub authority: Arc<str>,
}

impl HeaderCaptureInterceptor {
    pub fn take_single(&self) -> ObservedHeaders {
        let seen = self.seen.lock().expect("captured headers");
        assert_eq!(seen.len(), 1, "expected exactly one captured request");
        seen[0].clone()
    }
}

impl ExtensionCaptureInterceptor {
    pub fn seen(&self) -> Vec<Option<&'static str>> {
        self.seen.lock().expect("captured extensions").clone()
    }
}

impl RewriteUriInterceptor {
    pub fn new(authority: impl Into<Arc<str>>) -> Self {
        Self {
            authority: authority.into(),
        }
    }
}

impl Interceptor for HeaderCaptureInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let seen = self.seen.clone();
        let headers = ObservedHeaders::capture(exchange.request().headers());
        Box::pin(async move {
            seen.lock().expect("captured headers").push(headers);
            next.run(exchange).await
        })
    }
}

impl Interceptor for ExtensionCaptureInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let seen = self.seen.clone();
        let marker = exchange
            .request()
            .extensions()
            .get::<RetainedRequestMarker>()
            .copied()
            .map(|marker| marker.0);
        Box::pin(async move {
            seen.lock().expect("captured extensions").push(marker);
            next.run(exchange).await
        })
    }
}

impl Interceptor for RewriteUriInterceptor {
    fn intercept(
        &self,
        mut exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let authority = self.authority.clone();
        let rewritten_uri = rewrite_request_uri(exchange.request().uri(), authority.as_ref());
        Box::pin(async move {
            *exchange.request_mut().uri_mut() = rewritten_uri?;
            next.run(exchange).await
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedHeaders {
    pub host: Option<String>,
    pub user_agent: Option<String>,
    pub accept_encoding: Option<String>,
    pub content_length: Option<String>,
    pub transfer_encoding: Option<String>,
}

impl ObservedHeaders {
    pub fn capture(headers: &http::HeaderMap) -> Self {
        Self {
            host: header_value(headers, HOST),
            user_agent: header_value(headers, USER_AGENT),
            accept_encoding: header_value(headers, ACCEPT_ENCODING),
            content_length: header_value(headers, CONTENT_LENGTH),
            transfer_encoding: header_value(headers, TRANSFER_ENCODING),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedServerRequest {
    pub headers: ObservedHeaders,
    pub body: String,
}

pub fn take_observed_server_request(
    observed: &Arc<Mutex<Option<ObservedServerRequest>>>,
) -> ObservedServerRequest {
    observed
        .lock()
        .expect("observed server request")
        .clone()
        .expect("observed server request")
}

pub fn header_value(headers: &http::HeaderMap, name: http::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

pub fn rewrite_request_uri(uri: &http::Uri, authority: &str) -> Result<http::Uri, WireError> {
    let scheme = uri.scheme_str().unwrap_or("http");
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    format!("{scheme}://{authority}{path_and_query}")
        .parse()
        .map_err(|error| {
            WireError::invalid_request(format!("failed to rewrite request URI: {error}"))
        })
}

pub fn empty_request(uri: impl AsRef<str>) -> Request<RequestBody> {
    Request::builder()
        .uri(uri.as_ref())
        .body(RequestBody::empty())
        .expect("request")
}

pub fn proxy_rules(proxy: Proxy) -> ProxyRules {
    ProxyRules::new().proxy(proxy)
}

pub fn system_proxy_rules() -> ProxyRules {
    ProxyRules::new().use_system_proxy(true)
}

#[derive(Clone, Default)]
pub struct SwitchingProxySelector {
    pub current: Arc<Mutex<Option<Proxy>>>,
}

impl SwitchingProxySelector {
    pub fn new(proxy: Option<Proxy>) -> Self {
        Self {
            current: Arc::new(Mutex::new(proxy)),
        }
    }

    pub fn set_proxy(&self, proxy: Option<Proxy>) {
        *self.current.lock().expect("switching proxy selector lock") = proxy;
    }
}

impl ProxySelector for SwitchingProxySelector {
    fn select(&self, _uri: &http::Uri) -> Result<ProxySelection, WireError> {
        Ok(self
            .current
            .lock()
            .expect("switching proxy selector lock")
            .clone()
            .map(|proxy| ProxySelection::new().push_proxy(proxy))
            .unwrap_or_else(ProxySelection::direct))
    }
}

pub fn default_user_agent() -> &'static str {
    concat!("openwire/", env!("CARGO_PKG_VERSION"))
}

pub fn default_accept_encoding() -> &'static str {
    "br, gzip, deflate, zstd"
}

pub fn gzip_bytes(input: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(input).expect("write gzip payload");
    encoder.finish().expect("finish gzip payload")
}

pub fn environment_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

pub struct ScopedEnv {
    pub previous: Vec<(String, Option<String>)>,
}

impl ScopedEnv {
    pub fn set<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: AsRef<str>,
    {
        // Windows treats environment variable names as case-insensitive, so
        // pairs like `http_proxy` and `HTTP_PROXY` must collapse to one logical
        // entry or later cleanup entries will erase the value we just set.
        let mut merged = Vec::<(String, String, String)>::new();
        for (key, value) in vars {
            let key = key.into();
            let value = value.as_ref().to_owned();
            let normalized = normalize_env_key_for_platform(&key);

            if let Some((_, existing_key, existing_value)) = merged
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == normalized)
            {
                if existing_value.is_empty() && !value.is_empty() {
                    *existing_key = key;
                    *existing_value = value;
                }
                continue;
            }

            merged.push((normalized, key, value));
        }

        let mut previous = Vec::new();
        for (_, key, value) in merged {
            previous.push((key.clone(), std::env::var(&key).ok()));
            if value.is_empty() {
                unsafe {
                    std::env::remove_var(&key);
                }
            } else {
                unsafe {
                    std::env::set_var(&key, value);
                }
            }
        }
        Self { previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            match value {
                Some(value) => unsafe {
                    std::env::set_var(&key, value);
                },
                None => unsafe {
                    std::env::remove_var(&key);
                },
            }
        }
    }
}

pub fn normalize_env_key_for_platform(key: &str) -> String {
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key.to_owned()
    }
}

#[derive(Clone, Default)]
pub struct NeverRetryPolicy {
    pub calls: Arc<AtomicUsize>,
}

impl NeverRetryPolicy {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl RetryPolicy for NeverRetryPolicy {
    fn should_retry(&self, _ctx: &RetryContext<'_>) -> Option<&'static str> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        None
    }
}

#[derive(Clone, Default)]
pub struct RetryIfUncommittedPolicy {
    pub request_committed: Arc<Mutex<Vec<bool>>>,
}

impl RetryIfUncommittedPolicy {
    pub fn observed_request_committed(&self) -> Vec<bool> {
        self.request_committed
            .lock()
            .expect("request committed observations")
            .clone()
    }
}

impl RetryPolicy for RetryIfUncommittedPolicy {
    fn should_retry(&self, ctx: &RetryContext<'_>) -> Option<&'static str> {
        let request_committed = ctx.error().request_committed();
        self.request_committed
            .lock()
            .expect("request committed observations")
            .push(request_committed);
        (!request_committed && ctx.attempt() == 0).then_some("uncommitted")
    }
}

#[derive(Clone, Default)]
pub struct StopRedirectPolicy {
    pub calls: Arc<AtomicUsize>,
}

impl StopRedirectPolicy {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl RedirectPolicy for StopRedirectPolicy {
    fn should_redirect(&self, _ctx: &RedirectContext<'_>) -> RedirectDecision {
        self.calls.fetch_add(1, Ordering::Relaxed);
        RedirectDecision::Stop
    }
}

#[derive(Clone)]
pub struct FailingTcpConnector {
    pub failures_remaining: Arc<Mutex<usize>>,
    pub attempts: Arc<Mutex<usize>>,
}

#[derive(Clone, Default)]
pub struct CancelOnceInterceptor {
    pub calls: Arc<AtomicUsize>,
}

impl CancelOnceInterceptor {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Interceptor for CancelOnceInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if call == 0 {
                return Err(WireError::canceled("scripted canceled request"));
            }
            next.run(exchange).await
        })
    }
}

#[derive(Clone)]
pub struct CountingTimer {
    pub sleep_calls: Arc<AtomicUsize>,
    pub immediate_sleep: bool,
}

impl CountingTimer {
    pub fn immediate() -> Self {
        Self {
            sleep_calls: Arc::new(AtomicUsize::new(0)),
            immediate_sleep: true,
        }
    }

    pub fn tokio() -> Self {
        Self {
            sleep_calls: Arc::new(AtomicUsize::new(0)),
            immediate_sleep: false,
        }
    }

    pub fn sleep_calls(&self) -> usize {
        self.sleep_calls.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct ImmediateSleep;

impl std::future::Future for ImmediateSleep {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Ready(())
    }
}

impl Sleep for ImmediateSleep {}

impl Timer for CountingTimer {
    fn sleep(&self, duration: Duration) -> std::pin::Pin<Box<dyn Sleep>> {
        self.sleep_calls.fetch_add(1, Ordering::Relaxed);
        if self.immediate_sleep {
            Box::pin(ImmediateSleep)
        } else {
            openwire_tokio::TokioTimer::new().sleep(duration)
        }
    }

    fn sleep_until(&self, deadline: std::time::Instant) -> std::pin::Pin<Box<dyn Sleep>> {
        self.sleep_calls.fetch_add(1, Ordering::Relaxed);
        if self.immediate_sleep {
            Box::pin(ImmediateSleep)
        } else {
            openwire_tokio::TokioTimer::new().sleep_until(deadline)
        }
    }

    fn reset(&self, sleep: &mut std::pin::Pin<Box<dyn Sleep>>, new_deadline: std::time::Instant) {
        if self.immediate_sleep {
            *sleep = Box::pin(ImmediateSleep);
        } else {
            openwire_tokio::TokioTimer::new().reset(sleep, new_deadline);
        }
    }
}

#[derive(Clone, Default)]
pub struct AllowTwoSpawnsThenFailRuntime {
    pub spawns: Arc<AtomicUsize>,
}

impl WireExecutor for AllowTwoSpawnsThenFailRuntime {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        let spawn_index = self.spawns.fetch_add(1, Ordering::Relaxed);
        if spawn_index < 2 {
            Ok(Box::new(NoopTaskHandle(tokio::spawn(future))))
        } else {
            Err(WireError::internal(
                "scripted spawn failure",
                io::Error::other("scripted spawn failure"),
            ))
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct AlwaysFailSpawnRuntime;

impl WireExecutor for AlwaysFailSpawnRuntime {
    fn spawn(&self, _future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        Err(WireError::internal(
            "scripted spawn failure",
            io::Error::other("scripted spawn failure"),
        ))
    }
}

#[derive(Clone, Default)]
pub struct AbortCountingRuntime {
    pub aborts: Arc<AtomicUsize>,
}

impl AbortCountingRuntime {
    pub fn aborts(&self) -> usize {
        self.aborts.load(Ordering::Relaxed)
    }
}

pub struct NoopTaskHandle(pub tokio::task::JoinHandle<()>);

impl TaskHandle for NoopTaskHandle {
    fn abort(&self) {
        self.0.abort();
    }
}

pub struct AbortCountingTaskHandle {
    pub handle: tokio::task::JoinHandle<()>,
    pub aborts: Arc<AtomicUsize>,
}

impl TaskHandle for AbortCountingTaskHandle {
    fn abort(&self) {
        self.aborts.fetch_add(1, Ordering::Relaxed);
        self.handle.abort();
    }
}

impl WireExecutor for AbortCountingRuntime {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        Ok(Box::new(AbortCountingTaskHandle {
            handle: tokio::spawn(future),
            aborts: self.aborts.clone(),
        }))
    }
}

impl FailingTcpConnector {
    pub fn new(failures: usize) -> Self {
        Self {
            failures_remaining: Arc::new(Mutex::new(failures)),
            attempts: Arc::new(Mutex::new(0)),
        }
    }

    pub fn attempts(&self) -> usize {
        *self.attempts.lock().expect("connector attempts")
    }
}

impl TcpConnector for FailingTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let failures_remaining = self.failures_remaining.clone();
        let attempts = self.attempts.clone();
        Box::pin(async move {
            *attempts.lock().expect("connector attempts") += 1;

            let should_fail = {
                let mut failures_remaining = failures_remaining
                    .lock()
                    .expect("remaining connector failures");
                if *failures_remaining == 0 {
                    false
                } else {
                    *failures_remaining -= 1;
                    true
                }
            };

            if should_fail {
                ctx.listener().connect_start(&ctx, addr);
                let error = WireError::connect(
                    "scripted connect failure",
                    io::Error::new(io::ErrorKind::ConnectionRefused, "scripted connect failure"),
                );
                ctx.listener().connect_failed(&ctx, addr, &error);
                return Err(error);
            }

            TokioTcpConnector::default()
                .connect(ctx, addr, timeout)
                .await
        })
    }
}

#[derive(Clone, Default)]
pub struct AttemptCountingTcpConnector {
    pub attempts: Arc<AtomicUsize>,
}

impl AttemptCountingTcpConnector {
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl TcpConnector for AttemptCountingTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let attempts = self.attempts.clone();
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            TokioTcpConnector::default()
                .connect(ctx, addr, timeout)
                .await
        })
    }
}

#[derive(Clone, Default)]
pub struct RecordingTimeoutTcpConnector {
    pub timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
}

impl RecordingTimeoutTcpConnector {
    pub fn timeouts(&self) -> Vec<Option<Duration>> {
        self.timeouts.lock().expect("connector timeouts").clone()
    }
}

impl TcpConnector for RecordingTimeoutTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let timeouts = self.timeouts.clone();
        Box::pin(async move {
            timeouts.lock().expect("connector timeouts").push(timeout);
            TokioTcpConnector::default()
                .connect(ctx, addr, timeout)
                .await
        })
    }
}

#[derive(Clone)]
pub struct ScriptedRaceTcpConnector {
    pub scripts: Arc<HashMap<SocketAddr, TcpAttemptScript>>,
    pub attempts: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
pub struct TcpAttemptScript {
    pub actual_addr: SocketAddr,
    pub delay: Duration,
}

impl ScriptedRaceTcpConnector {
    pub fn new(entries: impl IntoIterator<Item = (SocketAddr, TcpAttemptScript)>) -> Self {
        Self {
            scripts: Arc::new(entries.into_iter().collect()),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl TcpConnector for ScriptedRaceTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<std::time::Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let script = self
            .scripts
            .get(&addr)
            .copied()
            .expect("missing tcp script");
        let attempts = self.attempts.clone();
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(script.delay).await;
            TokioTcpConnector::default()
                .connect(ctx, script.actual_addr, timeout)
                .await
        })
    }
}

#[derive(Clone)]
pub struct MultiAddrResolver {
    pub map: Arc<HashMap<String, Vec<SocketAddr>>>,
}

impl MultiAddrResolver {
    pub fn new(entries: impl IntoIterator<Item = (String, Vec<SocketAddr>)>) -> Self {
        Self {
            map: Arc::new(entries.into_iter().collect()),
        }
    }
}

impl DnsResolver for MultiAddrResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let map = self.map.clone();
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            let addrs = map.get(&host).cloned().ok_or_else(|| {
                WireError::dns(
                    "host not found in resolver map",
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing host"),
                )
            })?;
            ctx.listener().dns_end(&ctx, &host, &addrs);
            Ok(addrs)
        })
    }
}

#[derive(Clone)]
pub struct ScriptedDnsResolver {
    pub scripts: Arc<Mutex<HashMap<String, VecDeque<DnsAttemptScript>>>>,
    pub calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub struct ScriptedRoutePlanner {
    pub inner: DefaultRoutePlanner,
    pub calls: Arc<AtomicUsize>,
    pub fail_on_plan_call: usize,
}

impl ScriptedRoutePlanner {
    pub fn fail_on_plan_call(fail_on_plan_call: usize) -> Self {
        Self {
            inner: DefaultRoutePlanner::default(),
            calls: Arc::new(AtomicUsize::new(0)),
            fail_on_plan_call,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl RoutePlanner for ScriptedRoutePlanner {
    fn dns_target(&self, address: &openwire::Address) -> (String, u16) {
        self.inner.dns_target(address)
    }

    fn plan(
        &self,
        address: &openwire::Address,
        resolved_addrs: Vec<SocketAddr>,
    ) -> Result<RoutePlan, WireError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == self.fail_on_plan_call {
            return Err(WireError::invalid_request("scripted route planner failure"));
        }

        self.inner.plan(address, resolved_addrs)
    }
}

#[derive(Clone, Copy)]
pub enum DnsAttemptScript {
    Success(SocketAddr),
    Fail,
}

impl ScriptedDnsResolver {
    pub fn new(entries: impl IntoIterator<Item = (String, Vec<DnsAttemptScript>)>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(
                entries
                    .into_iter()
                    .map(|(host, scripts)| (host, VecDeque::from(scripts)))
                    .collect(),
            )),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl DnsResolver for ScriptedDnsResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let scripts = self.scripts.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            ctx.listener().dns_start(&ctx, &host, port);
            let script = scripts
                .lock()
                .expect("scripted dns scripts")
                .get_mut(&host)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| {
                    WireError::dns(
                        "missing scripted dns response",
                        io::Error::new(io::ErrorKind::NotFound, "missing scripted dns response"),
                    )
                })?;

            match script {
                DnsAttemptScript::Success(mut addr) => {
                    addr.set_port(port);
                    ctx.listener().dns_end(&ctx, &host, &[addr]);
                    Ok(vec![addr])
                }
                DnsAttemptScript::Fail => {
                    let error = WireError::dns(
                        "scripted dns failure",
                        io::Error::new(io::ErrorKind::TimedOut, "scripted dns failure"),
                    );
                    ctx.listener().dns_failed(&ctx, &host, &error);
                    Err(error)
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct ScriptedPassThroughTlsConnector {
    pub scripts: Arc<Mutex<Vec<TlsAttemptScript>>>,
    pub calls: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
pub enum TlsAttemptScript {
    Pass(Duration),
    Fail(Duration),
}

impl ScriptedPassThroughTlsConnector {
    pub fn new(scripts: Vec<TlsAttemptScript>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl TlsConnector for ScriptedPassThroughTlsConnector {
    fn connect(
        &self,
        ctx: CallContext,
        uri: hyper::Uri,
        stream: BoxConnection,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let host = uri.host().unwrap_or("openwire.test").to_owned();
        let scripts = self.scripts.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            let script = scripts.lock().expect("tls scripts").remove(0);
            ctx.listener().tls_start(&ctx, &host);
            match script {
                TlsAttemptScript::Pass(delay) => {
                    tokio::time::sleep(delay).await;
                    ctx.listener().tls_end(&ctx, &host);
                    Ok(stream)
                }
                TlsAttemptScript::Fail(delay) => {
                    tokio::time::sleep(delay).await;
                    let error = WireError::tls(
                        "scripted tls failure",
                        io::Error::new(io::ErrorKind::ConnectionAborted, "scripted tls failure"),
                    );
                    ctx.listener().tls_failed(&ctx, &host, &error);
                    Err(error)
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct HostMapResolver {
    pub map: Arc<HashMap<String, SocketAddr>>,
}

impl HostMapResolver {
    pub fn new(entries: impl IntoIterator<Item = (String, SocketAddr)>) -> Self {
        Self {
            map: Arc::new(entries.into_iter().collect()),
        }
    }
}

impl DnsResolver for HostMapResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let map = self.map.clone();
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            let mut addr = map.get(&host).copied().ok_or_else(|| {
                WireError::dns(
                    "host not found in resolver map",
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing host"),
                )
            })?;
            addr.set_port(port);
            ctx.listener().dns_end(&ctx, &host, &[addr]);
            Ok(vec![addr])
        })
    }
}

pub fn assert_event_subsequence(events: &[String], expected_prefixes: &[&str]) {
    let mut cursor = 0usize;
    for expected in expected_prefixes {
        while cursor < events.len() && !events[cursor].starts_with(expected) {
            cursor += 1;
        }
        assert!(
            cursor < events.len(),
            "missing event starting with {expected:?} in {events:?}",
        );
        cursor += 1;
    }
}

pub struct RawHttpServer {
    pub addr: SocketAddr,
    pub shutdown: Option<oneshot::Sender<()>>,
}

impl RawHttpServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RawHttpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub struct ConnectProxyServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<oneshot::Sender<()>>,
}

impl ConnectProxyServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for ConnectProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub struct AuthorizationProxyServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<oneshot::Sender<()>>,
}

impl AuthorizationProxyServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for AuthorizationProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub struct PlainHttpProxyServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<oneshot::Sender<()>>,
}

impl PlainHttpProxyServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for PlainHttpProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub struct Socks5ProxyServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<oneshot::Sender<()>>,
}

impl Socks5ProxyServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("proxy requests").clone()
    }
}

impl Drop for Socks5ProxyServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub async fn spawn_raw_http1_response(response: Vec<u8>) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let mut buffer = [0u8; 1024];
                    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buffer)).await;
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_raw_http1_headers_then_delayed_body(
    delay: Duration,
    body: &'static [u8],
) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let mut buffer = [0u8; 1024];
                    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buffer)).await;
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.flush().await;
                    tokio::time::sleep(delay).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_raw_http1_headers_then_stall_body(delay: Duration) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let mut buffer = [0u8; 1024];
                    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buffer)).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream.flush().await;
                    tokio::time::sleep(delay).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_raw_http1_host_guard(
    observed_request: Arc<Mutex<Option<String>>>,
) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut shutdown_rx => {}
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 256];
                    let deadline = tokio::time::sleep(Duration::from_millis(500));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            read = stream.read(&mut buffer) => {
                                let Ok(bytes_read) = read else {
                                    break;
                                };
                                if bytes_read == 0 {
                                    break;
                                }
                                request.extend_from_slice(&buffer[..bytes_read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }

                    let request = String::from_utf8_lossy(&request).into_owned();
                    let has_host = request
                        .lines()
                        .any(|line| line.to_ascii_lowercase().starts_with("host: "));
                    *observed_request.lock().expect("observed request") = Some(request);

                    let response = if has_host {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nhost ok"
                            .as_slice()
                    } else {
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\nConnection: close\r\n\r\nmissing host"
                            .as_slice()
                    };
                    let _ = stream.write_all(response).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_raw_http1_close_after_request_head(
    accepted_connections: Arc<AtomicUsize>,
) -> RawHttpServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw http listener");
    let addr = listener.local_addr().expect("raw http listener addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else {
                        break;
                    };
                    accepted_connections.fetch_add(1, Ordering::Relaxed);
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 256];
                    let deadline = tokio::time::sleep(Duration::from_millis(500));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            read = stream.read(&mut buffer) => {
                                let Ok(bytes_read) = read else {
                                    break;
                                };
                                if bytes_read == 0 {
                                    break;
                                }
                                request.extend_from_slice(&buffer[..bytes_read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = stream.shutdown().await;
                }
            }
        }
    });

    RawHttpServer {
        addr,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_connect_proxy() -> ConnectProxyServer {
    spawn_proxy_impl(true).await
}

pub async fn spawn_rejecting_connect_proxy() -> ConnectProxyServer {
    spawn_proxy_impl(false).await
}

pub async fn spawn_stalling_connect_proxy() -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request);
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_delayed_dropping_connect_proxy(delay: Duration) -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request);
                        tokio::time::sleep(delay).await;
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_connect_proxy_requiring_authorization(
    header_name: &'static str,
    expected_value: &'static str,
) -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());

                        let authorized = request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("{header_name}: {expected_value}"))
                        });

                        if !authorized {
                            client
                                .write_all(
                                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                )
                                .await
                                .expect("write proxy auth challenge");
                            let _ = client.shutdown().await;
                            return;
                        }

                        let target = request
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("CONNECT "))
                            .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                            .expect("CONNECT request line")
                            .to_owned();

                        let mut upstream = tokio::net::TcpStream::connect(&target)
                            .await
                            .expect("connect upstream through proxy");
                        client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await
                            .expect("write proxy response");

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_plain_http_proxy_response(body: &'static str) -> PlainHttpProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind plain proxy listener");
    let addr = listener.local_addr().expect("plain proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request);

                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        client
                            .write_all(response.as_bytes())
                            .await
                            .expect("write plain proxy response");
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    PlainHttpProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_socks5_proxy() -> Socks5ProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind socks proxy listener");
    let addr = listener.local_addr().expect("socks proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut greeting = [0u8; 2];
                        if client.read_exact(&mut greeting).await.is_err() {
                            return;
                        }
                        if greeting[0] != 0x05 {
                            return;
                        }
                        let mut methods = vec![0u8; greeting[1] as usize];
                        if client.read_exact(&mut methods).await.is_err() {
                            return;
                        }
                        if !methods.contains(&0x00) {
                            let _ = client.write_all(&[0x05, 0xff]).await;
                            let _ = client.shutdown().await;
                            return;
                        }
                        if client.write_all(&[0x05, 0x00]).await.is_err() {
                            return;
                        }

                        let mut head = [0u8; 4];
                        if client.read_exact(&mut head).await.is_err() {
                            return;
                        }
                        if head[0] != 0x05 || head[1] != 0x01 {
                            let _ = client.shutdown().await;
                            return;
                        }

                        let host = match head[3] {
                            0x01 => {
                                let mut addr = [0u8; 4];
                                if client.read_exact(&mut addr).await.is_err() {
                                    return;
                                }
                                std::net::Ipv4Addr::from(addr).to_string()
                            }
                            0x03 => {
                                let mut len = [0u8; 1];
                                if client.read_exact(&mut len).await.is_err() {
                                    return;
                                }
                                let mut host = vec![0u8; len[0] as usize];
                                if client.read_exact(&mut host).await.is_err() {
                                    return;
                                }
                                match String::from_utf8(host) {
                                    Ok(host) => host,
                                    Err(_) => return,
                                }
                            }
                            0x04 => {
                                let mut addr = [0u8; 16];
                                if client.read_exact(&mut addr).await.is_err() {
                                    return;
                                }
                                std::net::Ipv6Addr::from(addr).to_string()
                            }
                            _ => {
                                let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                                let _ = client.shutdown().await;
                                return;
                            }
                        };

                        let mut port = [0u8; 2];
                        if client.read_exact(&mut port).await.is_err() {
                            return;
                        }
                        let port = u16::from_be_bytes(port);
                        requests
                            .lock()
                            .expect("proxy requests")
                            .push(format!("{host}:{port}"));

                        let mut upstream = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                            Ok(upstream) => upstream,
                            Err(_) => {
                                let _ = client.write_all(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                                let _ = client.shutdown().await;
                                return;
                            }
                        };

                        if client
                            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                            .await
                            .is_err()
                        {
                            return;
                        }

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    Socks5ProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_socks5_proxy_requiring_auth(
    expected_username: &'static str,
    expected_password: &'static str,
) -> Socks5ProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind socks proxy listener");
    let addr = listener.local_addr().expect("socks proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut greeting = [0u8; 2];
                        if client.read_exact(&mut greeting).await.is_err() {
                            return;
                        }
                        if greeting[0] != 0x05 {
                            return;
                        }
                        let mut methods = vec![0u8; greeting[1] as usize];
                        if client.read_exact(&mut methods).await.is_err() {
                            return;
                        }
                        if !methods.contains(&0x02) {
                            let _ = client.write_all(&[0x05, 0xff]).await;
                            let _ = client.shutdown().await;
                            return;
                        }
                        if client.write_all(&[0x05, 0x02]).await.is_err() {
                            return;
                        }

                        let mut auth_version = [0u8; 1];
                        if client.read_exact(&mut auth_version).await.is_err() {
                            return;
                        }
                        if auth_version[0] != 0x01 {
                            let _ = client.write_all(&[0x01, 0x01]).await;
                            let _ = client.shutdown().await;
                            return;
                        }

                        let mut username_len = [0u8; 1];
                        if client.read_exact(&mut username_len).await.is_err() {
                            return;
                        }
                        let mut username = vec![0u8; username_len[0] as usize];
                        if client.read_exact(&mut username).await.is_err() {
                            return;
                        }

                        let mut password_len = [0u8; 1];
                        if client.read_exact(&mut password_len).await.is_err() {
                            return;
                        }
                        let mut password = vec![0u8; password_len[0] as usize];
                        if client.read_exact(&mut password).await.is_err() {
                            return;
                        }

                        let username = match String::from_utf8(username) {
                            Ok(username) => username,
                            Err(_) => return,
                        };
                        let password = match String::from_utf8(password) {
                            Ok(password) => password,
                            Err(_) => return,
                        };
                        if username != expected_username || password != expected_password {
                            let _ = client.write_all(&[0x01, 0x01]).await;
                            let _ = client.shutdown().await;
                            return;
                        }
                        if client.write_all(&[0x01, 0x00]).await.is_err() {
                            return;
                        }

                        let mut head = [0u8; 4];
                        if client.read_exact(&mut head).await.is_err() {
                            return;
                        }
                        if head[0] != 0x05 || head[1] != 0x01 {
                            let _ = client.shutdown().await;
                            return;
                        }

                        let host = match head[3] {
                            0x01 => {
                                let mut addr = [0u8; 4];
                                if client.read_exact(&mut addr).await.is_err() {
                                    return;
                                }
                                std::net::Ipv4Addr::from(addr).to_string()
                            }
                            0x03 => {
                                let mut len = [0u8; 1];
                                if client.read_exact(&mut len).await.is_err() {
                                    return;
                                }
                                let mut host = vec![0u8; len[0] as usize];
                                if client.read_exact(&mut host).await.is_err() {
                                    return;
                                }
                                match String::from_utf8(host) {
                                    Ok(host) => host,
                                    Err(_) => return,
                                }
                            }
                            0x04 => {
                                let mut addr = [0u8; 16];
                                if client.read_exact(&mut addr).await.is_err() {
                                    return;
                                }
                                std::net::Ipv6Addr::from(addr).to_string()
                            }
                            _ => {
                                let _ = client.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                                let _ = client.shutdown().await;
                                return;
                            }
                        };

                        let mut port = [0u8; 2];
                        if client.read_exact(&mut port).await.is_err() {
                            return;
                        }
                        let port = u16::from_be_bytes(port);
                        requests
                            .lock()
                            .expect("proxy requests")
                            .push(format!("{host}:{port}"));

                        let mut upstream = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                            Ok(upstream) => upstream,
                            Err(_) => {
                                let _ = client.write_all(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                                let _ = client.shutdown().await;
                                return;
                            }
                        };

                        if client
                            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                            .await
                            .is_err()
                        {
                            return;
                        }

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    Socks5ProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_proxy_requiring_authorization(
    header_name: &'static str,
    expected_value: &'static str,
) -> AuthorizationProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth proxy listener");
    let addr = listener.local_addr().expect("auth proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());
                        let authorized = request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("{header_name}: {expected_value}"))
                        });

                        let response = if authorized {
                            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nproxy authorized"
                        } else {
                            "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        };

                        client
                            .write_all(response.as_bytes())
                            .await
                            .expect("write auth proxy response");
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    AuthorizationProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_proxy_redirect_requiring_authorization(
    header_name: &'static str,
    expected_value: &'static str,
) -> AuthorizationProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth proxy listener");
    let addr = listener.local_addr().expect("auth proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());
                        let authorized = request.lines().any(|line| {
                            line.eq_ignore_ascii_case(&format!("{header_name}: {expected_value}"))
                        });

                        let response = if !authorized {
                            "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                        } else if request.starts_with("GET http://source.test/start HTTP/1.1\r\n") {
                            "HTTP/1.1 302 Found\r\nLocation: http://target.test/finish\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                        } else if request.starts_with("GET http://target.test/finish HTTP/1.1\r\n") {
                            "HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\nredirect authorized".to_owned()
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\nConnection: close\r\n\r\nunexpected".to_owned()
                        };

                        client
                            .write_all(response.as_bytes())
                            .await
                            .expect("write auth proxy response");
                        let _ = client.shutdown().await;
                    });
                }
            }
        }
    });

    AuthorizationProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

pub async fn spawn_proxy_impl(accept_connect: bool) -> ConnectProxyServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("proxy listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = requests.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else {
                        break;
                    };
                    let requests = requests_clone.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let read = client.read(&mut buf).await.expect("read proxy request");
                            if read == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let request = String::from_utf8_lossy(&head).to_string();
                        requests.lock().expect("proxy requests").push(request.clone());

                        let target = request
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("CONNECT "))
                            .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                            .expect("CONNECT request line")
                            .to_owned();

                        if !accept_connect {
                            client
                                .write_all(
                                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
                                )
                                .await
                                .expect("write proxy rejection");
                            let _ = client.shutdown().await;
                            return;
                        }

                        let mut upstream = tokio::net::TcpStream::connect(&target)
                            .await
                            .expect("connect upstream through proxy");
                        client
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await
                            .expect("write proxy response");

                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    });
                }
            }
        }
    });

    ConnectProxyServer {
        addr,
        requests,
        shutdown: Some(shutdown_tx),
    }
}

#[derive(Clone, Debug)]
pub struct CapturedSpan {
    pub name: String,
    pub fields: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct CapturedEvent {
    pub fields: HashMap<String, String>,
}

#[derive(Default)]
pub struct TraceCaptureInner {
    pub span_order: Vec<u64>,
    pub spans: HashMap<u64, CapturedSpan>,
    pub events: Vec<CapturedEvent>,
}

#[derive(Clone, Default)]
pub struct TraceCapture {
    pub inner: Arc<Mutex<TraceCaptureInner>>,
}

impl TraceCapture {
    pub fn spans_named(&self, name: &str) -> Vec<CapturedSpan> {
        let inner = self.inner.lock().expect("trace capture lock");
        inner
            .span_order
            .iter()
            .filter_map(|id| inner.spans.get(id))
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }

    pub fn event_by_message(&self, message: &str) -> Option<CapturedEvent> {
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .iter()
            .find(|event| {
                event
                    .fields
                    .get("message")
                    .is_some_and(|value| normalize_trace_message(value) == message)
            })
            .cloned()
    }

    pub fn event_with_field(&self, name: &str, expected: &str) -> Option<CapturedEvent> {
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .iter()
            .find(|event| {
                event
                    .fields
                    .get(name)
                    .is_some_and(|value| normalize_trace_message(value) == expected)
            })
            .cloned()
    }

    pub fn event_messages(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .iter()
            .filter_map(|event| event.fields.get("message").cloned())
            .collect()
    }
}

pub fn normalize_trace_message(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('\'').to_owned()
}

pub async fn with_trace_capture<F, T>(trace: &TraceCapture, future: F) -> T
where
    F: Future<Output = T>,
{
    // Explicit TRACE filter keeps debug/trace events even when RUST_LOG is set
    // in CI or the parent process environment.
    let subscriber = tracing_subscriber::registry()
        .with(trace.clone())
        .with(tracing_subscriber::filter::LevelFilter::TRACE);
    future
        .with_subscriber(tracing::Dispatch::new(subscriber))
        .await
}

pub fn trace_test_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &Id,
        _ctx: LayerContext<'_, S>,
    ) {
        let mut visitor = FieldCapture::default();
        attrs.record(&mut visitor);
        let mut inner = self.inner.lock().expect("trace capture lock");
        let id = id.into_u64();
        inner.span_order.push(id);
        inner.spans.insert(
            id,
            CapturedSpan {
                name: attrs.metadata().name().to_owned(),
                fields: visitor.fields,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: LayerContext<'_, S>) {
        let mut visitor = FieldCapture::default();
        values.record(&mut visitor);
        if let Some(span) = self
            .inner
            .lock()
            .expect("trace capture lock")
            .spans
            .get_mut(&id.into_u64())
        {
            span.fields.extend(visitor.fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let mut visitor = FieldCapture::default();
        event.record(&mut visitor);
        let _ = ctx.event_scope(event);
        self.inner
            .lock()
            .expect("trace capture lock")
            .events
            .push(CapturedEvent {
                fields: visitor.fields,
            });
    }
}

#[derive(Default)]
pub struct FieldCapture {
    pub fields: HashMap<String, String>,
}

impl Visit for FieldCapture {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}
