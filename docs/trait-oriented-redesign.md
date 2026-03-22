# Trait-Oriented Redesign

Date: 2026-03-22
Status: Draft v3

## Target Crate Layout

```text
openwire-core  (keep name — contains types + traits)
├── body.rs            RequestBody, ResponseBody
├── error.rs           WireError, WireErrorKind, EstablishmentStage
├── context.rs         CallContext, CallId, ConnectionId
├── event.rs           EventListener, EventListenerFactory
├── interceptor.rs     Interceptor, Exchange, Next
├── transport.rs       DnsResolver, TcpConnector, TlsConnector, ConnectionIo, BoxConnection
├── runtime.rs         WireExecutor, HyperExecutor  (replaces Runtime)
├── auth.rs            Authenticator, AuthContext, AuthKind  (moved from openwire)
├── cookie.rs          CookieJar  (moved from openwire)
├── policy.rs          RetryPolicy, RedirectPolicy  (new)
└── lib.rs             re-exports only; NO tokio concrete impls

openwire-tokio  (new crate — all tokio-specific adapters and defaults)
├── executor.rs        TokioExecutor: WireExecutor + hyper::rt::Executor
├── timer.rs           TokioTimer: hyper::rt::Timer
├── io.rs              TokioIo: bidirectional hyper::rt ↔ tokio::io adapter
├── dns.rs             SystemDnsResolver: DnsResolver  (moved from openwire)
├── tcp.rs             TokioTcpConnector: TcpConnector  (moved from openwire)
└── lib.rs             re-exports, TokioRuntime convenience bundle

openwire  (main crate — framework + policy, no direct tokio in framework paths)
├── client.rs          Client, ClientBuilder
├── transport.rs       TransportService, ConnectorStack
│                      bind_http1, bind_http2 — accept generic Executor + Timer
│                      CONNECT/SOCKS tunnel helpers use runtime-neutral IoCompat
├── policy/
│   ├── follow_up.rs   FollowUpPolicyService
│   ├── retry.rs       DefaultRetryPolicy
│   └── redirect.rs    DefaultRedirectPolicy
├── connection/
│   ├── planning.rs    RoutePlanner trait + DefaultRoutePlanner
│   ├── pool.rs        ConnectionPool
│   ├── exchange_finder.rs
│   ├── fast_fallback.rs  — accept dyn WireExecutor + Timer
│   └── real_connection.rs
├── cookie.rs          Jar (concrete impl)
├── auth.rs            blanket impls only
├── proxy.rs           Proxy, NoProxy, ProxySelector
└── bridge.rs          BridgeInterceptor
```

## Dependency Graph

```text
openwire ──────────> openwire-core
openwire ──────────> openwire-tokio (default, feature = "runtime-tokio")
openwire ─ ─ ─ ─ ─> openwire-rustls (optional)
openwire-tokio ────> openwire-core
openwire-tokio ────> tokio
openwire-rustls ───> openwire-core
openwire-rustls ───> openwire-tokio              ← NEW (needs TokioIo)
openwire-rustls ───> tokio-rustls
openwire-cache ────> openwire
openwire-test ─────> openwire-core
openwire-test ─────> openwire-tokio              ← NEW (needs TokioIo + TokioExecutor)
```

Note: `openwire-cache` currently remains tokio-based (`tokio::sync::RwLock` in its store
implementation). This redesign does not change that crate.

### openwire-core dependency budget

Keeps: `hyper` (for `hyper::rt::{Read, Write}` in ConnectionIo),
`hyper-util` (for `Connection` trait + `Connected` in ConnectionIo/BoxConnection),
`http`, `http-body`, `http-body-util`, `tower`, `bytes`, `futures-core`, `futures-util`,
`futures-channel`,
`thiserror`, `tracing`, `pin-project-lite`.

Drops: `tokio`, `async-trait`.

Rationale: `ConnectionIo` extends `hyper::rt::Read + hyper::rt::Write + Connection`.
`Connection` is `hyper_util::client::legacy::connect::Connection`, returning `Connected`.
Replacing these with custom traits would duplicate hyper-util's connection metadata model
and break compatibility with any code that expects `Connected`. The cost of keeping
`hyper` + `hyper-util` as trait-only deps is low and the interop value is high.

---

## Phase 1 — Split openwire-tokio Out of openwire-core

Extract all tokio-specific code into a new crate. Do NOT rename openwire-core.

### What moves

| From | To | Content |
|------|----|---------|
| `openwire-core/src/tokio_rt.rs` | `openwire-tokio/src/` | TokioExecutor, TokioTimer, TokioIo |
| `openwire/src/transport.rs:41-77` | `openwire-tokio/src/dns.rs` | SystemDnsResolver |
| `openwire/src/transport.rs:77-127` | `openwire-tokio/src/tcp.rs` | TokioTcpConnector, TcpConnection |

SystemDnsResolver and TokioTcpConnector are tokio-specific concrete impls. Moving them
to openwire-tokio makes the runtime boundary explicit: openwire (framework) has zero
direct tokio imports; openwire-tokio (runtime adapter) provides all tokio defaults.

### Downstream import migration

| Crate | Before | After |
|-------|--------|-------|
| openwire | `use openwire_core::{TokioExecutor, TokioIo, ...}` | `use openwire_tokio::{...}` |
| openwire-rustls | `use openwire_core::TokioIo` | `use openwire_tokio::TokioIo` + add dep |
| openwire-test | `use openwire_core::{TokioExecutor, TokioIo}` | `use openwire_tokio::{...}` + add dep |

### Feature gating

openwire's `runtime-tokio` feature (default) gates the `openwire-tokio` dependency.
`ClientBuilder::default()` uses `TokioRuntime` / `SystemDnsResolver` / `TokioTcpConnector`
only when `runtime-tokio` is active; without it, the user must provide an equivalent
runtime configuration plus DNS/TCP defaults explicitly.

Deferred note: the exact semantics of `ClientBuilder::default()` / `Client::builder()`
when `runtime-tokio` is disabled are intentionally left open for now. The initial rollout
may simply gate the default builder path behind `runtime-tokio`, or require explicit
executor/timer/dns/tcp injection when the feature is off. Revisit this when adding
first-class multi-runtime support.

---

## Phase 2 — True Runtime De-Tokio

### 2a. Replace Runtime trait with WireExecutor

```rust
// openwire-core/src/runtime.rs
pub trait WireExecutor: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>);
}
```

`spawn` is infallible — tokio::spawn never fails in practice.

`Runtime` is removed. `TokioRuntime` no longer exists as the old `spawn + sleep` trait impl.
In openwire-tokio it may remain only as a convenience bundle around:
- `TokioExecutor` for `WireExecutor`
- `TokioTimer` for `hyper::rt::Timer`

The framework depends on the split executor/timer pair, not on a monolithic runtime object.

### 2b. HyperExecutor adapter for HTTP/2

hyper 1.8's `http2::Builder::new(executor)` requires `E: hyper::rt::Executor<Fut>` where
`Fut` is hyper's internal H2 connection future — NOT just `BoxFuture<()>`. The current
`TokioExecutor` satisfies this via blanket `impl<Fut> Executor<Fut>`. A `dyn WireExecutor`
cannot directly satisfy this generic bound.

Solution: `HyperExecutor` adapter in openwire-core that wraps any `WireExecutor`:

```rust
// openwire-core/src/runtime.rs

/// Bridges WireExecutor to hyper::rt::Executor<Fut> for any Fut.
/// Boxes the future and delegates to WireExecutor::spawn.
#[derive(Clone)]
pub struct HyperExecutor(pub Arc<dyn WireExecutor>);

impl<Fut> hyper::rt::Executor<Fut> for HyperExecutor
where
    Fut: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: Fut) {
        self.0.spawn(Box::pin(future));
    }
}
```

`TokioExecutor` continues to implement both `WireExecutor` and `Executor<Fut>` directly
(avoids the extra boxing for the default path). `HyperExecutor` is the escape hatch for
custom runtimes that only implement `WireExecutor`.

Rationale for location: `HyperExecutor` is runtime-neutral. Keeping it in openwire-core
lets non-tokio runtimes use the adapter without pulling in openwire-tokio.

bind_http2 becomes:

```rust
async fn bind_http2<E, T>(
    stream: BoxConnection,
    config: &TransportConfig,
    executor: E,
    timer: T,
) -> Result<(http2::SendRequest<RequestBody>, http2::Connection<..., E>), WireError>
where
    E: Clone + Unpin + Send + 'static,  // plus the exact H2 client executor bound required by hyper
    T: hyper::rt::Timer + Clone + 'static,
```

Important: use the same executor bound required by
`hyper::client::conn::http2::Builder` / `handshake` for the client H2 connection future.
Do NOT narrow this to `Executor<BoxFuture<()>>`. `TokioExecutor` satisfies the bound
directly; `HyperExecutor` satisfies it via its blanket `impl<Fut> Executor<Fut>`.

TransportService stores:
```rust
executor: E,  // generic, resolved at Client construction
timer: T,     // generic, resolved at Client construction
```

Type-erasure boundary: monomorphization happens inside `ClientBuilder::build()`.
`TransportService<E, T>` is instantiated there, `build_service_chain` becomes generic over
the concrete transport service type, and the result is then erased into `BoxWireService`.

```rust
fn build_service_chain<S>(
    transport: S,
    ...
) -> BoxWireService
where
    S: Service<Exchange, Response = Response<ResponseBody>, Error = WireError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
```

This keeps executor/timer generics local to construction time. `ClientInner` stores the
boxed service plus its call-deadline timer; it does not need to carry `E` and `T` as
top-level type parameters.

Implementation note: there are two valid construction strategies at the `build()` boundary:
- simpler path: always use `HyperExecutor` over a `WireExecutor`
- optimized default path: monomorphize a concrete `TokioExecutor` variant for the default
  tokio feature, while custom runtimes use `HyperExecutor`

The choice is an internal builder/storage detail. The public architecture only requires
that monomorphization happen before boxing into `BoxWireService`.

### 2c. Fix fast-fallback tokio leakage

| Line | Current | After |
|------|---------|-------|
| 8 | `tokio::sync::mpsc` | `futures_channel::mpsc` |
| 9 | `tokio::task::JoinHandle` | `AbortHandle` + completion receiver |
| 70 | `tokio::spawn(...)` | `executor.spawn(...)` |
| 73 | `tokio::time::sleep(...)` | `timer.sleep(...)` |

`FastFallbackDialer` gains `executor: Arc<dyn WireExecutor>` and
`timer: Arc<dyn hyper::rt::Timer>`, passed from ConnectorStack.

Cancellation must remain explicit because Happy Eyeballs correctness depends on aborting
losing connection attempts. Do NOT rely on "drop the future" semantics.

Use a pure futures-based cancellation scheme local to fast_fallback:
- wrap each spawned candidate future in `futures_util::future::Abortable`
- store its `AbortHandle` in the race state
- pair each task with a small completion receiver so cleanup can wait until every loser has
  actually terminated and dropped its channel sender

Sketch:
```rust
struct RaceTask {
    abort: AbortHandle,
    done: oneshot::Receiver<()>,
}
```

`cleanup_losers` then:
1. aborts every loser via `abort.abort()`
2. awaits loser completion receivers
3. drains any already-sent `Finished { Ok(stream) }` messages and drops those streams

This preserves the current "abort losers, then deterministically clean up leftover streams"
behavior without requiring cancel-capable handles from `WireExecutor`.

### 2d. Fix proxy/SOCKS timeout tokio leakage

Replace `tokio::time::timeout` (transport.rs:723, 922) with `timer.sleep()` +
`futures_util::future::select`:

```rust
let sleep = timer.sleep(timeout);
pin_mut!(sleep);
match futures_util::future::select(operation, sleep).await {
    Either::Left((result, _)) => result,
    Either::Right(_) => Err(WireError::timeout(...)),
}
```

ConnectorStack passes its `timer` reference into `connect_via_http_proxy` and
`connect_via_socks_proxy`.

### 2e. Fix CONNECT/SOCKS tunnel I/O tokio leakage

Replacing `tokio::time::timeout` is not sufficient by itself. The current tunnel helpers
still use:
- `TokioIo::new(stream)` to adapt `BoxConnection`
- `tokio::io::{AsyncReadExt, AsyncWriteExt}` for `read`, `read_exact`, `write_all`, `flush`

After: add a small runtime-neutral `IoCompat<T>` helper local to `transport.rs`
(or move it to openwire-core later if it becomes reusable). This is not a trivial
one-liner; expect a careful adapter plus unit tests.

Chosen approach:
- `IoCompat<T>` implements `futures::io::AsyncRead + AsyncWrite` for
  `T: hyper::rt::Read + hyper::rt::Write + Unpin`
- tunnel helpers then use `futures_util::io::AsyncReadExt/AsyncWriteExt` for
  `read`, `read_exact`, `write_all`, `flush`

Dependency note: this requires runtime-neutral futures I/O/channel utilities in the
framework crate, e.g. `futures-channel` for `mpsc`/`oneshot` and `futures-util` with
the `io` extension traits enabled.

This keeps the handshake code readable while containing the low-level poll/buffer
translation in one place.

Tunnel helpers switch from:
```rust
let mut stream = TokioIo::new(stream);
stream.write_all(...).await?;
stream.read_exact(...).await?;
```

to:
```rust
let mut stream = IoCompat::new(stream);
stream.write_all(...).await?;
stream.read_exact(...).await?;
```

This keeps CONNECT/SOCKS handshake logic in the framework crate while removing the last
direct dependency on tokio I/O traits from `transport.rs`.

### 2f. Client call deadline also uses Timer

Current `Client::execute` still relies on `runtime.sleep(timeout)` inside
`with_call_deadline`.

After:
- `ClientBuilder` stores enough runtime configuration to construct transport and deadline
  timers at build time
- `ClientInner` stores the boxed service plus its call-deadline timer
- `with_call_deadline` switches from `runtime.sleep(timeout)` to `timer.sleep(timeout)`

```rust
async fn with_call_deadline<F>(
    timer: Arc<dyn hyper::rt::Timer>,
    deadline: Option<Instant>,
    future: F,
) -> Result<Response<ResponseBody>, WireError>
where
    F: Future<Output = Result<Response<ResponseBody>, WireError>>,
{
    // select(future, timer.sleep(timeout))
}
```

This keeps the whole framework on a single timer abstraction:
call timeout, fast-fallback staggering, CONNECT/SOCKS timeouts, HTTP/2 timers,
and response-body deadlines all use `hyper::rt::Timer`.

### 2g. Body deadline: eliminate background task

Current: spawns a background task (sleep + AtomicWaker) per response body.

After: `ObservedIncomingBody` holds `Option<Pin<Box<dyn Sleep>>>` and polls it
inline in `poll_frame`:

```rust
impl Body for ObservedIncomingBody {
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>>
    {
        if let Some(sleep) = &mut self.deadline_sleep {
            if sleep.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Some(Err(WireError::timeout(...))));
            }
        }
        // poll inner body ...
    }
}
```

Eliminates: one spawn, one AtomicWaker, one background task per response body.
Requires `ObservedIncomingBody::wrap()` to accept `Option<Pin<Box<dyn Sleep>>>`
instead of `Option<(Arc<dyn WireExecutor>, Duration)>`.

### 2h. Full tokio removal audit

After Phase 2, verify zero direct tokio usage in framework paths:

| File | Remaining tokio refs | Status |
|------|---------------------|--------|
| transport.rs | None in framework code | Clean |
| fast_fallback.rs | None | Clean |
| client.rs | None | Clean |
| connection/*.rs | None | Clean |

Default impls (`SystemDnsResolver`, `TokioTcpConnector`) live in openwire-tokio —
outside the framework crate entirely.

---

## Phase 3 — Move CookieJar and Authenticator to openwire-core

### CookieJar

Trait moves to `openwire-core/src/cookie.rs`. Add `url` dep (lightweight, pure Rust).
`Jar` (concrete cookie_store-backed impl) stays in openwire.

### Authenticator

Trait + AuthKind + AuthContext move to `openwire-core/src/auth.rs`.

Current constructor depends on three `pub(crate)` structs:
```rust
pub(crate) fn new(kind, request: AuthRequestState, response: AuthResponseState, attempts: AuthAttemptState)
```

External impls only consume AuthContext via read-only getters. Solution: flatten
constructor to primitive fields:

```rust
impl AuthContext {
    pub fn new(
        kind: AuthKind,
        method: Method, uri: Uri, version: Version,
        headers: HeaderMap, extensions: Extensions,
        body: Option<RequestBody>,
        response_status: StatusCode, response_headers: HeaderMap,
        total_attempt: u32, retry_count: u32,
        redirect_count: u32, auth_count: u32,
    ) -> Self { ... }
}
```

Call site in follow_up.rs destructures the three snapshot structs — mechanical conversion.
`AuthRequestState` / `AuthResponseState` / `AuthAttemptState` become openwire-internal
convenience types that are never exposed.

---

## Phase 4 — Extract RetryPolicy and RedirectPolicy

Both traits are **decision-only**. Request mutation (method rewriting, cross-origin
header cleaning) stays in `FollowUpPolicyService::into_redirect_request`.

### RetryPolicy

```rust
// openwire-core/src/policy.rs
pub trait RetryPolicy: Send + Sync + 'static {
    fn should_retry(&self, ctx: &RetryContext) -> Option<&'static str>;
}

pub struct RetryContext<'a> {
    pub error: &'a WireError,
    pub attempt: u32,
    pub is_body_replayable: bool,
}
```

`DefaultRetryPolicy` preserves current `retry_reason()` logic (follow_up.rs:531-562),
including establishment-stage awareness.

### RedirectPolicy

```rust
pub trait RedirectPolicy: Send + Sync + 'static {
    fn should_redirect(&self, ctx: &RedirectContext) -> RedirectDecision;
}

pub struct RedirectContext<'a> {
    pub request_method: &'a Method,
    pub request_uri: &'a Uri,
    pub response_status: StatusCode,
    pub location: &'a Uri,
    pub redirect_count: u32,
    pub is_body_replayable: bool,
}

pub enum RedirectDecision { Follow, Stop, Error(WireError) }
```

`DefaultRedirectPolicy` preserves current behavior. Existing `ClientBuilder` convenience
methods build Default* internally.

---

## Phase 5 — RoutePlanner Trait

RoutePlanner trait stays **in openwire** (not openwire-core). It depends on `Address`,
which depends on `ProxyConfig`, which converts from `crate::proxy::Proxy`. Moving to
openwire-core would require decoupling Address from proxy types — unnecessary complexity.

```rust
// openwire/src/connection/planning.rs

pub trait RoutePlanner: Send + Sync + 'static {
    fn dns_target(&self, address: &Address) -> (String, u16);
    fn plan(&self, address: &Address, resolved: Vec<SocketAddr>) -> Result<RoutePlan, WireError>;
    fn fast_fallback_stagger(&self) -> Duration;
}
```

`DefaultRoutePlanner` preserves current logic: proxy-aware DNS target selection,
ProxyMode-based route kind, alternating IPv4/IPv6 for Happy Eyeballs, 250ms stagger.

`Address` and `RoutePlan` become `pub` (currently `pub(crate)`).
`ConnectorStack` changes from:
```rust
route_planner: RoutePlanner
```

to:
```rust
route_planner: Arc<dyn RoutePlanner>
```

This keeps the rest of transport concrete while making route planning itself
replaceable.

---

## Phase 6 — Connector Tower Adapters

Lowest priority. Connector traits keep `&self` signatures. Bidirectional adapters
provide tower interop for power users.

```text
┌───────────────────┐         ┌────────────────────────┐
│ DnsResolver trait  │ ◄─────► │ tower::Service<DnsReq>  │
│ TcpConnector trait │ ◄─────► │ tower::Service<TcpReq>  │
│ TlsConnector trait │ ◄─────► │ tower::Service<TlsReq>  │
└───────────────────┘         └────────────────────────┘
     simple &self API              composable with tower
```

Request types carry `CallContext` for observability propagation.

---

## Unchanged (and why)

| Item | Reason |
|------|--------|
| Interceptor | Already tower-bridged via InterceptorLayer/InterceptorService |
| EventListener / EventListenerFactory | Domain-specific; no ecosystem equivalent |
| DnsResolver / TcpConnector / TlsConnector signatures | `&self` naturally concurrent; adapters (Phase 6) provide interop |
| ConnectionIo / BoxConnection | Already composes hyper::rt + hyper_util::Connection |
| ProxySelector | Coupled to redirect proxy-stability checks; defer |
| ConnectionPool / RealConnection | Implementation detail, not a strategy boundary |
| RequestBody try_clone / replayable_len | Retry/redirect depend on these; correct beyond http_body::Body |
| Redirect request mutation | HTTP spec compliance; not a user-customization point |
| openwire-cache runtime model | `openwire-cache` remains tokio-based for now; out of scope for this redesign |

---

## Phase Dependencies

```text
Phase 1 (split openwire-tokio) ─────┐
    │                                │
    ├──> Phase 2 (de-tokio runtime)  │
    │         │                      │
    │         └──> Phase 5 (RoutePlanner) ──> Phase 6 (tower adapters)
    │
    ├──> Phase 3 (move CookieJar / Authenticator)
    │         │
    │         └──> Phase 4 (RetryPolicy / RedirectPolicy)
    │
    └──> Phase 6 can also start after Phase 1 independently
```

Recommended order: 1 → 2 → 3 → 4 → 5 → 6

## Verification

After each phase:
```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Phase 2 additionally — verify no direct tokio in framework paths:
```bash
grep -rn 'tokio::' crates/openwire/src/ --include='*.rs' \
  | grep -v '#\[cfg(test)\]'
# Should return zero lines after Phase 2
```
