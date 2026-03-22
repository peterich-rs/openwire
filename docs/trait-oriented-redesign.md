# Trait-Oriented Redesign

Date: 2026-03-22
Status: Draft v7

## Main Sync Notes

This draft is still directionally valid after `main` commit `e820485` ("plan core
review work and harden connection lifecycle cleanup"), but several baseline
assumptions changed and need to stay in scope while refining the redesign:

- `Runtime::spawn()` now returns an abort-capable `TaskHandle`, and
  `TransportService` uses a `ConnectionTaskRegistry` to abort owned connection
  tasks on final client drop. Any `Runtime -> WireExecutor + Timer` split must
  replace that shutdown behavior explicitly; fire-and-forget spawn is no longer
  sufficient.
- request admission and fresh-connection admission are now explicit transport
  lifecycle concerns (`RequestAdmissionLimiter`, `ConnectionLimiter`,
  `ConnectionPermit`). Their leases live across response-body / connection
  lifetime, so they should remain in `openwire` with the rest of transport
  orchestration.
- `FastFallbackDialer` now backs route-plan racing for direct, HTTP forward
  proxy, CONNECT proxy, and SOCKS proxy paths. The de-tokio work in
  `fast_fallback.rs` must target the shared route-plan dialer, not only the
  direct-connect path.
- proxy credentials are now parsed once in `Proxy` and propagated through
  `ProxyConfig` / `RouteKind` into CONNECT and SOCKS establishment. Route
  planning remains coupled to `openwire` proxy types for good reason.

## Relationship To Completed Core Review

The completed core review, summarized in [`docs/core-review.md`](core-review.md)
and [`docs/plans/core-review-plan-spec.md`](plans/core-review-plan-spec.md), is
now part of the baseline that this redesign must preserve, not replace:

- `WS-01` established RAII connection ownership plus `ConnectionTaskRegistry`
  shutdown semantics; the runtime split must keep abortable owned-task tracking.
- `WS-02` established `RequestBody::absent()` vs `explicit_empty()`, downgrade
  redirect validation, and real Tower readiness propagation; policy extraction
  must preserve those semantics.
- `WS-03` established request admission, fresh-connection admission, and
  sharded `ConnectionBindings`; those stay in `openwire` transport orchestration.
- `WS-04` established proxy credential propagation and proxy-path fast fallback;
  route-planning and fast-fallback phases now target the shared route-plan
  machinery rather than only direct-origin dialing.
- `WS-05` established protocol-agnostic idle eviction, coalescing secondary
  indexing, and poison-safe lock helpers; the redesign should treat those as
  current implementation constraints.
- `WS-06` established RFC 9110 `Connection` token parsing and acquire/release
  deadline signaling; later runtime cleanup must not regress those guarantees.

## Redesign Scope After The Core Review

This redesign starts from the post-review codebase on `main`. It is not trying
to reopen or re-solve the review backlog. Its job is to reorganize extension
boundaries and runtime ownership without losing the semantics that the merged
review work already established.

That means the redesign must carry forward, even if code moves across crates:

- RAII cleanup for selected connections, response leases, and response-body release
- tracked owned-connection tasks with explicit shutdown on final client drop
- request-admission and connection-admission behavior and their current lifetime rules
- proxy credential propagation, proxy-path fast fallback, and retryability boundaries
- pool coalescing secondary indexing, protocol-agnostic idle eviction, and poison-safe locks
- the current body semantics, redirect downgrade policy, and Tower readiness guarantees

## Target Crate Layout

```text
openwire-core  (keep name — contains types + traits)
├── body.rs            RequestBody, ResponseBody
├── error.rs           WireError, WireErrorKind, EstablishmentStage
├── context.rs         CallContext, CallId, ConnectionId
├── event.rs           EventListener, EventListenerFactory
├── interceptor.rs     Interceptor, Exchange, Next
├── transport.rs       DnsResolver, TcpConnector, TlsConnector, ConnectionIo, BoxConnection
├── runtime.rs         TaskHandle, WireExecutor, HyperExecutor  (replaces Runtime)
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
│   ├── limits.rs      RequestAdmissionLimiter, ConnectionLimiter, ConnectionPermit
│   ├── pool.rs        ConnectionPool
│   ├── exchange_finder.rs
│   ├── fast_fallback.rs  — accept dyn WireExecutor + Timer
│   └── real_connection.rs
├── cookie.rs          Jar (concrete impl)
├── auth.rs            blanket impls only
├── proxy.rs           Proxy, NoProxy, ProxySelector
└── bridge.rs          BridgeInterceptor
```

Transport-private lifecycle guards introduced on `main` stay internal for now:
`ConnectionTaskRegistry`, `SelectedConnection`, and `ResponseLease` remain in
`transport.rs` until the runtime split is finished. The redesign should not
force an extra module breakup before those ownership boundaries are stable.

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

## Current-To-Target Gap Map

| Area | Current `main` baseline | Target end-state | Migration note |
|------|--------------------------|------------------|----------------|
| Runtime surface | `openwire-core` now exports `Runtime`, `TaskHandle`, `WireExecutor`, `HyperExecutor`, and `SharedTimer`; `TransportService` now carries executor/timer inputs explicitly | `openwire-core` exports only the runtime-neutral surfaces needed by the framework; `Runtime` remains only if call/body deadlines still need it | `M2` landed additively; `M3` and `M6` remove the remaining `Runtime` dependency |
| Tokio adapters | `SystemDnsResolver`, `TokioTcpConnector`, `TokioRuntime`, `TokioExecutor`, `TokioTimer`, and `TokioIo` already live in `openwire-tokio` | the same adapters remain in `openwire-tokio`, but the framework path stops depending on Tokio-specific APIs directly | `M1` landed without final feature-gating; preserve current DNS/connect event behavior exactly |
| Transport Tokio leakage | fast fallback, CONNECT/SOCKS timeouts, tunnel I/O, and call/body deadlines still depend on Tokio-specific APIs or Tokio-owned helpers | framework path uses only `WireExecutor`, `hyper::rt::Timer`, and runtime-neutral I/O adapters | this is the critical-path de-Tokio work |
| Policy traits | `CookieJar`, `Authenticator`, retry logic, and redirect decisions are still centered in `openwire` | traits and contexts move to `openwire-core`; orchestration and request mutation stay in `openwire` | preserve current body and redirect semantics byte-for-byte |
| Planning surface | `ConnectorStack` stores a concrete `RoutePlanner` struct | route planning becomes a replaceable `Arc<dyn RoutePlanner>` boundary in `openwire` | keep proxy credential propagation and shared route-plan fast fallback intact |

## Delivery Milestones

| Milestone | Covers | Goal | Exit criteria |
|------|--------|------|---------------|
| `M1` | Phase 1 | create `openwire-tokio` and move Tokio-only adapters without behavior changes | completed on this branch: adapters live in `openwire-tokio`, defaults still work, and workspace callers import Tokio adapters from the new crate |
| `M2` | Phase 2a-2b | add the runtime-neutral executor surface and generic HTTP/2 binding path | completed on this branch: `WireExecutor`, `HyperExecutor`, and `SharedTimer` exist; `bind_http2` no longer hardcodes Tokio executor/timer construction; connection-task tracking uses the configured executor |
| `M3` | Phase 2c-2h | remove the remaining framework Tokio leakage | non-test framework paths stop depending directly on `tokio::`; fast fallback, tunnels, call deadlines, and body deadlines all use runtime-neutral abstractions |
| `M4` | Phases 3-4 | move policy traits into `openwire-core` | `CookieJar`, `Authenticator`, `RetryPolicy`, and `RedirectPolicy` live in core while `openwire` keeps default policy impls and orchestration |
| `M5` | Phase 5 | turn route planning into a replaceable strategy boundary | `ConnectorStack` depends on `Arc<dyn RoutePlanner>` and public planning types are stable enough for custom planners |
| `M6` | Phase 6 + cleanup | finish adapter interop and remove compatibility shims | `Runtime` and `openwire-core` Tokio re-exports are gone; docs and examples reflect the final crate boundaries |

Updated recommended delivery order from the new baseline: `M3 -> M4 -> M5 -> M6`.
`M3` remains the critical path because the framework still uses Tokio-specific
fast fallback, tunnel timeouts/I/O, and deadline helpers.

### Milestone Checklists

#### `M1` Checklist

- create `crates/openwire-tokio` and move the current `tokio_rt.rs` contents there
- move `SystemDnsResolver` and `TokioTcpConnector` out of `crates/openwire/src/transport.rs`
- update workspace crates (`openwire`, `openwire-rustls`, `openwire-test`) to import Tokio adapters from `openwire-tokio`
- note the current limitation: `runtime-tokio` is still a placeholder feature flag until later phases remove Tokio requirements from the framework path

#### `M2` Checklist

- introduce `TaskHandle`, `WireExecutor`, and `HyperExecutor` in `openwire-core`
- make Tokio adapters implement the new executor surface in `openwire-tokio`
- thread executor/timer inputs through `TransportService` construction and `bind_http2`
- switch owned connection-task tracking to the executor-returned handle type before removing `Runtime`

Current status: landed additively. `Runtime` still exists for call/body deadline
handling, and `WireExecutor::spawn` currently remains fallible so the existing
spawn-failure behavior stays testable during the transition.

#### `M3` Checklist

- rewrite `FastFallbackDialer` around runtime-neutral executor/timer primitives
- replace CONNECT/SOCKS timeout helpers and tunnel I/O with runtime-neutral building blocks
- move `with_call_deadline` and response-body deadline enforcement off `Runtime::sleep`
- prove the de-Tokio result with a non-test `rg -n 'tokio::' crates/openwire/src/` audit

#### `M4` Checklist

- move `CookieJar` and `Authenticator` traits plus their public contexts into `openwire-core`
- extract decision-only `RetryPolicy` and `RedirectPolicy` traits into `openwire-core`
- leave redirect request mutation, cookie application, and follow-up orchestration in `openwire`
- preserve `RequestBody::absent()` / `explicit_empty()` semantics and downgrade redirect behavior exactly

#### `M5` Checklist

- replace the concrete `RoutePlanner` field in `ConnectorStack` with `Arc<dyn RoutePlanner>`
- decide the minimum public surface for `Address`, `RoutePlan`, and related planning types
- keep proxy credential propagation and the shared route-plan fast-fallback contract unchanged
- verify that custom planners still cannot bypass `TransportService` ownership boundaries

#### `M6` Checklist

- add tower adapters only after the core runtime/planning surfaces are stable
- remove temporary re-exports and compatibility shims left from `M1` and `M2`
- delete `Runtime` once no framework path depends on it
- update `README.md`, examples, and crate re-exports to match the final crate split

## Phase 1 — Split openwire-tokio Out of openwire-core

Extract all tokio-specific code into a new crate. Do NOT rename openwire-core.

### What moves

| From | To | Content |
|------|----|---------|
| `openwire-core/src/tokio_rt.rs` | `openwire-tokio/src/` | TokioExecutor, TokioTimer, TokioIo |
| `openwire/src/transport.rs` | `openwire-tokio/src/dns.rs` | SystemDnsResolver |
| `openwire/src/transport.rs` | `openwire-tokio/src/tcp.rs` | TokioTcpConnector, TcpConnection |

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

`ClientBuilder::default()` now gets `TokioRuntime` / `SystemDnsResolver` /
`TokioTcpConnector` from `openwire-tokio`.

Implementation note: after `M1`, the `runtime-tokio` feature name still exists
but is not yet the final compile-time boundary, because the framework path still
depends on Tokio-only helpers such as `TokioIo`, `TokioExecutor`, and
`TokioTimer`. Real feature-gating is deferred until `M2` and `M3` remove those
framework-path requirements.

---

## Phase 2 — True Runtime De-Tokio

### 2a. Replace Runtime trait with WireExecutor

```rust
// openwire-core/src/runtime.rs
pub trait TaskHandle: Send + Sync + 'static {
    fn abort(&self);
}

pub type BoxTaskHandle = Box<dyn TaskHandle>;

pub trait WireExecutor: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError>;
}
```

Incremental-state note: after `M2`, `WireExecutor` is now present in the codebase,
but it is still additive. `Runtime` has not been removed yet because call timeout
and body-deadline helpers still depend on `Runtime::sleep`.

Final-state goal: once `M3` finishes migrating those deadline paths, `Runtime`
can disappear and `WireExecutor::spawn` can be simplified if the compatibility
error path is no longer worth preserving.

`Runtime` is removed only in the end-state. At that point, `TokioRuntime` no
longer exists as the old `spawn + sleep` trait impl.
In openwire-tokio it may remain only as a convenience bundle around:
- `TokioExecutor` for `WireExecutor`
- `TokioTimer` for `hyper::rt::Timer`

The framework depends on the split executor/timer pair, not on a monolithic runtime object.
`ConnectionTaskRegistry` in `openwire` continues to hold the returned task handles so
owned HTTP connection tasks can still be aborted during final client shutdown.

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
        let _ = self.0.spawn(Box::pin(future));
    }
}
```

`TokioExecutor` continues to implement both `WireExecutor` and `Executor<Fut>` directly
(avoids the extra boxing for the default path). `HyperExecutor` is the escape hatch for
custom runtimes that only implement `WireExecutor`.

Incremental-state note: the current implementation already threads `HyperExecutor`
and a cloneable timer wrapper through `TransportService` for HTTP/2 binding, while
still keeping `Runtime` for deadline code that has not been de-Tokioed yet.

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

Main-sync note: this is no longer only about direct-origin Happy Eyeballs.
`main` now funnels direct, HTTP forward proxy, CONNECT proxy, and SOCKS proxy
route plans through the shared `FastFallbackDialer::dial_route_plan()` path, so
the runtime-neutral rewrite must cover the generic dialer API and its route
finalization callbacks.

| Current dependency | Current use | After |
|------|---------|-------|
| `tokio::sync::mpsc` | route-race result channel | `futures_channel::mpsc` |
| `tokio::task::JoinHandle` | spawned attempt ownership | `AbortHandle` + completion receiver |
| `tokio::spawn(...)` | candidate task spawning | `executor.spawn(...)` |
| `tokio::time::sleep(...)` | stagger delay | `timer.sleep(...)` |

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

Replace `tokio::time::timeout` in the CONNECT / SOCKS helper paths with `timer.sleep()` +
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

Until this lands, the current acquire/release memory-ordering guarantees in the
deadline-signal path remain required behavior.

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

Main-sync note: current body semantics now distinguish `RequestBody::absent()`
from `RequestBody::explicit_empty()`. Redirect-policy extraction must preserve
the existing downgrade guard and leave method/body rewriting, including
absent-body preservation when switching to GET, in
`FollowUpPolicyService::into_redirect_request`.

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

`DefaultRetryPolicy` preserves current `retry_reason()` logic,
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

`DefaultRedirectPolicy` preserves current behavior, including default HTTPS to
HTTP downgrade rejection behind `allow_insecure_redirects`. Existing
`ClientBuilder` convenience methods build Default* internally.

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
ProxyMode-based route kind, proxy-credential propagation into route kinds,
alternating IPv4/IPv6 for Happy Eyeballs, and a route plan that remains usable
by the shared fast-fallback dialer across direct and proxy candidates.

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
