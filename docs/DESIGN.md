# OpenWire Technical Design

Date: 2026-03-22

This is the canonical architecture document for OpenWire. Keep it short,
current, and focused on the code that exists today.

## 1. Product Direction

OpenWire is an OkHttp-inspired async HTTP client for Rust.

Primary goals:

- predictable and observable request behavior
- clear separation between policy and transport
- extensibility through interceptors and transport traits
- cross-platform and mobile-friendly networking
- `hyper` as the HTTP protocol engine, not the entire client architecture

## 2. Repository Shape

```text
openwire/
├── crates/openwire          public API, policy layer, transport integration
├── crates/openwire-cache    application-layer cache interceptor
├── crates/openwire-core     shared body, error, event, executor/timer, transport, and policy traits
├── crates/openwire-tokio    Tokio executor, timer, I/O, DNS, and TCP adapters
├── crates/openwire-rustls   optional Rustls TLS connector
├── crates/openwire-test     test support
└── docs/
    ├── DESIGN.md                    canonical technical design
    ├── core-review.md               completed review summary and doc index
    ├── plans/
    │   └── core-review-plan-spec.md historical core-review closure map
    ├── trait-oriented-redesign.md   completed trait/crate-boundary redesign closure
    └── tasks.md                     deferred follow-on tracker
```

Layering rules:

- `openwire-core` contains shared primitives, not policy behavior
- `openwire` owns public API, follow-up policy, and connection orchestration
- `openwire-rustls` stays swappable behind `TlsConnector`
- `openwire-cache` remains an application-layer crate, not transport logic
- `docs/DESIGN.md` tracks the current implementation; plan docs must call out
  any deliberate baseline deltas explicitly

Source-file map for the main runtime path:

| File | Primary Types / Functions |
|---|---|
| `crates/openwire/src/lib.rs` | public re-exports |
| `crates/openwire/src/client.rs` | `Client`, `ClientBuilder`, `Call`, `build_service_chain()` |
| `crates/openwire/src/policy/follow_up.rs` | `FollowUpPolicyService`, retry / redirect / auth / cookie orchestration |
| `crates/openwire/src/bridge.rs` | `BridgeInterceptor`, request normalization |
| `crates/openwire/src/transport.rs` | `TransportService`, `ConnectorStack`, direct binding, response-body release |
| `crates/openwire/src/connection/exchange_finder.rs` | `ExchangeFinder`, pool-hit / miss preparation |
| `crates/openwire/src/connection/planning.rs` | `Address`, `Route`, `RoutePlan`, `RoutePlanner`, `DefaultRoutePlanner` |
| `crates/openwire/src/connection/limits.rs` | `RequestAdmissionLimiter`, `ConnectionLimiter`, `ConnectionPermit` |
| `crates/openwire/src/connection/fast_fallback.rs` | `FastFallbackDialer`, `ConnectPlan` |
| `crates/openwire/src/connection/pool.rs` | `ConnectionPool` |
| `crates/openwire/src/connection/real_connection.rs` | `RealConnection` |

## 3. Ownership Boundaries

| Area | Ownership | Current Default |
|---|---|---|
| HTTP protocol state | Adopt from `hyper` | `hyper` |
| Request policy / follow-ups | OpenWire-owned | `openwire` |
| Connection acquisition / pooling / route planning | OpenWire-owned | `openwire` |
| Fast fallback for route dialing | OpenWire-owned | `FastFallbackDialer` |
| Task execution / timing | OpenWire-owned trait boundaries | Tokio executor + timer |
| DNS | Trait boundary + default adapter | system resolver |
| TCP | Trait boundary + default adapter | Tokio TCP |
| TLS | Trait boundary + default adapter | Rustls |
| Cache | Separate application-layer crate | `openwire-cache` |

OpenWire owns orchestration, lifecycle semantics, reuse decisions, and
observability. `hyper` owns only HTTP/1.1 and HTTP/2 protocol state machines.

## 4. Public API Boundary

Stable external rules:

- requests are `http::Request<RequestBody>`
- send entry points are `Client::execute(request)` and
  `Client::new_call(request).execute()`
- `ClientBuilder` owns transport and policy configuration
- request-scoped metadata lives in `http::Extensions`
- retries, redirects, and authenticator follow-ups preserve request extensions
- custom integration points are traits, not convenience mini-frameworks

Important extension traits:

- `Interceptor`
- `EventListener` / `EventListenerFactory`
- `CookieJar`
- `Authenticator`
- `RetryPolicy`
- `RedirectPolicy`
- `DnsResolver`
- `TcpConnector`
- `TlsConnector`
- `RoutePlanner`
- `WireExecutor`
- `hyper::rt::Timer`

`ClientBuilder::default()` currently resolves to these concrete defaults:

| Setting | Default |
|---|---|
| executor | `TokioExecutor` |
| timer | `TokioTimer` |
| DNS | `SystemDnsResolver` |
| TCP | `TokioTcpConnector` |
| TLS | `RustlsTlsConnector::builder().build()?` when `tls-rustls` is enabled |
| `call_timeout` | `None` |
| `connect_timeout` | `None` |
| `pool_idle_timeout` | `Some(Duration::from_secs(90))` |
| `pool_max_idle_per_host` | `usize::MAX` |
| `http2_keep_alive_interval` | `None` |
| `http2_keep_alive_while_idle` | `false` |
| `max_connections_total` | `usize::MAX` |
| `max_connections_per_host` | `usize::MAX` |
| `max_requests_total` | `usize::MAX` |
| `max_requests_per_host` | `usize::MAX` |
| `retry_canceled_requests` | `false` |
| `retry_on_connection_failure` | `true` |
| `max_retries` | `1` |
| `follow_redirects` | `true` |
| `max_redirects` | `10` |
| `allow_insecure_redirects` | `false` |
| `max_auth_attempts` | `3` |
| `use_system_proxy` | `false` |

Extension boundary contracts:

| Trait | Method Contract | Current Owner |
|---|---|---|
| `CookieJar` | `set_cookies(&mut Iterator<Item=&HeaderValue>, &Url)` and `cookies(&Url) -> Option<HeaderValue>` | `crates/openwire-core/src/cookie.rs` |
| `Authenticator` | `authenticate(AuthContext) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>>` | `crates/openwire-core/src/auth.rs` |
| `RetryPolicy` | `should_retry(&RetryContext) -> Option<&'static str>` | `crates/openwire-core/src/policy.rs` |
| `RedirectPolicy` | `should_redirect(&RedirectContext) -> RedirectDecision` | `crates/openwire-core/src/policy.rs` |
| `DnsResolver` | `resolve(CallContext, host, port) -> BoxFuture<Result<Vec<SocketAddr>, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `TcpConnector` | `connect(CallContext, SocketAddr, Option<Duration>) -> BoxFuture<Result<BoxConnection, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `TlsConnector` | `connect(CallContext, Uri, BoxConnection) -> BoxFuture<Result<BoxConnection, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `RoutePlanner` | `dns_target(&Address) -> (String, u16)` and `plan(&Address, Vec<SocketAddr>) -> Result<RoutePlan, WireError>` | `crates/openwire/src/connection/planning.rs` |
| `WireExecutor` | `spawn(BoxFuture<()>) -> Result<BoxTaskHandle, WireError>` | `crates/openwire-core/src/runtime.rs` |
| `hyper::rt::Timer` | `sleep`, `sleep_until`, `reset`, `now` | external trait configured through `ClientBuilder::timer(...)` |
| `EventListenerFactory` | `create(&Request<RequestBody>) -> SharedEventListener` | `crates/openwire-core/src/event.rs` |

Connector interop helpers exported from `openwire-core`:

- `DnsRequest`, `TcpConnectRequest`, `TlsConnectRequest`
- `TowerDnsResolver`, `TowerTcpConnector`, `TowerTlsConnector`
- `DnsResolverService`, `TcpConnectorService`, `TlsConnectorService`

Current extension-point rules:

- `CookieJar` and `Authenticator` live in `openwire-core`, but remain
  policy-layer integrations and must not own transport decisions
- `DnsResolver`, `TcpConnector`, and `TlsConnector` may affect route execution
  but must not bypass `TransportService`
- `EventListener` is observational only and must not mutate request execution
- custom executors own bound-connection background-task spawning through `WireExecutor`
- custom timers supply call deadlines, body deadlines, CONNECT/SOCKS timeouts, and HTTP/2 binding timers through `ClientBuilder::timer(...)`

## 5. Service Chain Construction

`build_service_chain()` in `crates/openwire/src/client.rs` constructs the
runtime service stack in this exact order:

```text
application interceptors (in insertion order)
  -> FollowUpPolicyService
    -> BridgeInterceptor
      -> network interceptors (in insertion order)
        -> RequestAdmissionService
          -> TransportService
```

Construction details:

- application interceptors are wrapped in reverse iteration so execution order
  matches insertion order
- network interceptors are wrapped in reverse iteration so execution order
  matches insertion order
- `BridgeInterceptor` always runs after follow-up policy and before network
  interceptors
- `RequestAdmissionService` runs after network interceptors and before transport
  so admission sees the final request URI for that network attempt
- `FollowUpPolicyService` always wraps the entire network chain

`BridgeInterceptor` currently performs exactly three normalization steps:

- `normalize_host_header()`
- `normalize_user_agent_header()`
- `normalize_body_headers()`

Body-header rules are exact:

- absent request body (`RequestBody::absent()` / `Default`): remove both
  `Content-Length` and `Transfer-Encoding`
- replayable or explicit-empty body with known length: set `Content-Length`, remove
  `Transfer-Encoding`
- streaming body on HTTP/1.1: remove `Content-Length`, set
  `Transfer-Encoding: chunked`
- streaming body on non-HTTP/1.1: remove both `Content-Length` and
  `Transfer-Encoding`

Tower readiness rules are also exact:

- interceptor layers move the polled-ready inner service into `Next`
  instead of re-cloning and re-polling it inside `call()`
- `FollowUpPolicyService::poll_ready()` delegates to the network chain
- `RequestAdmissionService::poll_ready()` reflects global request-admission
  readiness
- follow-up retries and redirects re-poll readiness only for subsequent
  network attempts inside the same logical call

## 6. Canonical Execution Chain

Method-level call path:

```text
Client::execute(request)
  -> Client::new_call(request)
    -> Call::execute()
      -> CallContext::from_factory(...)
      -> tower::ServiceExt::ready(...)
      -> Exchange::new(request, ctx, 1)
      -> build_service_chain(...)
        -> application interceptors
        -> FollowUpPolicyService::call()
          -> validate_request()
          -> apply_request_cookies()
          -> BridgeInterceptor::intercept()
          -> network interceptors
          -> RequestAdmissionService::call()
            -> RequestAdmissionLimiter::acquire(final_address)
            -> TransportService::call()
              -> TransportService::execute_exchange()
                -> ExchangeFinder::prepare()
                -> TransportService::acquire_connection()
                  -> wait for reusable connection or fresh-connection permit
                  -> TransportService::bind_fresh_connection() on miss
                    -> ConnectorStack::route_plan()
                    -> connect_route_plan()
                    -> bind_http1() or bind_http2()
                    -> spawn_http1_task() or spawn_http2_task()
                -> send_bound_request()
                -> ObservedIncomingBody::wrap()
            -> request-admission permit stays attached to the response body
          -> store_response_cookies()
          -> authenticate_response()
          -> resolve_redirect_uri()
          -> validate_redirect_target()
```

No feature should bypass this chain.

`FollowUpPolicyService::call()` owns the loop for:

- retry
- redirect
- origin authentication
- proxy authentication
- cookie request application
- cookie response persistence
- default rejection of `https -> http` downgrade redirects unless
  `allow_insecure_redirects` is enabled

`TransportService::execute_exchange()` owns:

- pool-hit / miss trace recording
- connection acquisition
- protocol binding on miss path
- request execution on bound connection
- pre-response-body RAII cleanup for acquired connections and leases
- response-body release wrapping

## 7. Connection Core

Key internal types:

- `Address`: stable logical destination and primary reuse key
- `Route`: one concrete network path candidate
- `RoutePlan`: ordered route candidates for one attempt
- `ConnectPlan`: per-route attempt state during fast fallback
- `RealConnection`: owned live connection state
- `ConnectionPool`: reusable connection storage and eviction policy
- `ExchangeFinder`: pool lookup plus miss-path acquisition coordinator
- `RoutePlanner`: replaceable direct and proxy route construction
- `FastFallbackDialer`: staged TCP racing plus route-local finalization across
  direct and proxy candidates

Current connection-core rules:

- exact-address reuse remains the first lookup path
- direct HTTPS HTTP/2 may coalesce only when certificate SANs authorize the
  target authority and the resolved route plan overlaps the connected remote
  address
- same-authority traffic still requires the exact-address reuse path; the
  coalescing path is only for alternate verified authorities
- direct routes race resolved target addresses
- proxy routes race resolved proxy endpoints and then run CONNECT / SOCKS /
  TLS finalization on the winning or next still-viable route
- retryable post-connect establishment failures continue to later route
  candidates; non-retryable failures stop the race immediately
- target addresses behind forward proxies / CONNECT / SOCKS tunnels are not
  independently raced once a proxy connection is established
- request admission is enforced inside the network chain after application,
  follow-up, bridge, and network interceptors finalize the request for that
  attempt
- request admission does not hold a global permit while waiting for a per-address
  slot
- request-admission permits are released when the returned response body is
  dropped or consumed
- fresh-connection admission waits for either an existing reusable connection
  or an available connection slot keyed by the logical `Address`
- HTTP/1.1 reuse is single-exchange and body-lifecycle-driven; either the
  request or response `Connection: close` directive disables pooling
- HTTP/2 reuse is admitted by bound-sender readiness instead of a separate fixed
  local stream cap
- HTTP/1.1 idle timeout and max-idle limits are still enforced on foreground
  pool touch points, and a best-effort background reaper now calls whole-pool
  prune passes so idle addresses can age out without future traffic
- the background reaper is started lazily on the first successful pooled insert
  instead of during `Client::build()`, so client construction does not require
  an already-running executor

Exact planning and acquisition path:

```text
ExchangeFinder::prepare(request)
  -> Address::from_uri(request.uri(), matching_proxy)
  -> ConnectionPool::acquire(&address)
  -> PreparedExchange { PoolHit | PoolMiss }

TransportService::bind_fresh_connection(...)
  -> ConnectorStack::route_plan(ctx, prepared.address())
    -> RoutePlanner::dns_target(address)
    -> DnsResolver::resolve(...)
    -> RoutePlanner::plan(address, resolved_addrs)
  -> try_acquire_coalesced(...)
  -> connect_route_plan(...)
    -> connect_direct(...) for `RouteKind::Direct`
    -> connect_via_http_forward_proxy(...) for `RouteKind::HttpForwardProxy`
    -> connect_via_http_proxy(...) for `RouteKind::ConnectProxy`
    -> connect_via_socks_proxy(...) for `RouteKind::SocksProxy`
```

Current `DefaultRoutePlanner::default()` uses a fixed fast-fallback stagger of
`Duration::from_millis(250)`.

Route ownership rules:

- `RoutePlanner::dns_target()` resolves the proxy endpoint when a proxy is
  configured, otherwise resolves the origin authority
- `DefaultRoutePlanner::plan_*()` always returns an ordered `RoutePlan`
- `RoutePlan` carries the fast-fallback stagger consumed by the shared dialer
- proxy URL credentials are parsed once on `Proxy` construction and carried
  through `ProxyConfig` / `RouteKind` instead of being re-read from raw URLs in
  transport code
- `order_for_fast_fallback()` alternates IPv4 / IPv6 where both families are
  present and otherwise preserves resolver order within the single family

Pool and reuse rules:

- `ExchangeFinder::prepare()` performs the first exact-address pool lookup
- `TransportService::try_acquire_coalesced()` performs the secondary HTTPS
  HTTP/2 coalesced lookup after route planning on miss path, using a
  secondary index keyed by direct target address instead of scanning the full
  pool
- `ConnectionBindings` is sharded by `ConnectionId` so exact-key binding
  operations do not serialize through one global mutex
- `ObservedIncomingBody` drives `ConnectionPool::release()` on body completion
- `SelectedConnection` uses RAII cleanup so call timeout / cancellation before
  request publication does not leak allocations
- abandoning an HTTP/2 response body releases only the stream allocation; it
  does not poison the whole session
- `ResponseLease` uses RAII cleanup so call timeout / cancellation after
  response publication but before body wrapping does not leak allocations
- broken HTTP/1.1 exchanges remove the connection instead of returning it to
  idle reuse

Operational invariants:

- `RealConnection::try_acquire()` must fail for unhealthy or closed connections
- HTTP/1.1 may have at most one active allocation per `RealConnection`
- HTTP/2 allocation count is bookkeeping for release and lifecycle handling;
  sender readiness decides whether another exchange can be dispatched
- `RealConnection::release()` increments `completed_exchanges` and restores
  `idle_since` only when the allocation count reaches zero
- `ConnectionPool::remove()` always closes the removed connection
- `ConnectionPool` keeps a `ConnectionId -> Address` reverse index so exact-id
  removals do not scan every address bucket
- `ConnectionPool::release()` is valid only for connections that were
  previously acquired
- pool pruning evicts non-healthy connections and idle HTTP/1.1 or HTTP/2
  connections past `idle_timeout`
- background idle reaping uses the same `prune_all()` / `prune_address()`
  paths as foreground pool touches so reverse-index and coalescing-index cleanup
  stay centralized
- the client keeps the reaper task handle and aborts it on final client drop
- `pool_max_idle_per_host` applies to every idle connection stored under the
  logical address, not only HTTP/1.1
- coalescing is limited to direct HTTPS HTTP/2 connections with verified server
  name authorization, route overlap, and a different authority than the pooled
  connection
- cookie jar, pool state, real connection state, and connection-task registry
  locks recover from poisoning instead of cascading panics through subsequent
  calls

## 8. Protocol Binding

Protocol ownership is intentionally split:

- OpenWire owns address derivation, routing, acquisition, reuse, and release
- `hyper::client::conn::http1` owns HTTP/1.1 protocol state
- `hyper::client::conn::http2` owns HTTP/2 protocol state

This means:

- no request path should reintroduce a legacy pooled client as a shortcut
- protocol binding happens only after OpenWire chooses a winning connection path
- response body wrappers remain the source of release bookkeeping

Current binding path on a fresh connection:

```text
connect_route_plan(...)
  -> BoxConnection
  -> connection_info_from_connected(...)
  -> determine_protocol(...)
  -> RealConnection::with_id_and_coalescing(...)
  -> bind_http1(...) or bind_http2(...)
  -> ConnectionBindings::insert_http1(...) or insert_http2(...)
  -> ConnectionBindings::acquire(...) for the current request
  -> ConnectionPool::insert(...)
  -> spawn_http1_task(...) or spawn_http2_task(...)
```

Protocol-specific rules:

- HTTP/1.1 uses one active exchange per connection
- HTTP/1.1 response reuse checks `Connection` header members with RFC 9110
  token parsing; malformed `Connection` values conservatively disable reuse
- HTTP/2 reuses a bound sender for multiple exchanges
- CONNECT tunnel setup preserves any bytes already read past the proxy response
  header block before handing the stream to TLS or HTTP
- spawned background connection tasks are tracked per client lifetime, abort on
  final owner drop, and remove bindings and pool entries on termination
- `record_fast_fallback_trace()` writes `route_count`,
  `fast_fallback_enabled`, `connect_race_id`, and `connect_winner` into
  `openwire.attempt`

## 9. Execution And Adapter Boundaries

Current default adapters:

- Tokio executor through `openwire-tokio::TokioExecutor`
- Tokio timer through `openwire-tokio::TokioTimer`
- system DNS through `openwire-tokio::SystemDnsResolver`
- Tokio TCP through `openwire-tokio::TokioTcpConnector`
- Rustls through `openwire_rustls::RustlsTlsConnector`
- Tokio adapter types are imported from `openwire-tokio` directly; `openwire`
  no longer re-exports them

Design constraints:

- do not hold pool locks across DNS, TCP, TLS, or protocol-binding awaits
- preserve swappable DNS / TCP / TLS / runtime boundaries
- keep proxy, cookie, auth, and cache policy above transport

Adapter boundaries that are part of the current code shape:

- DNS resolution happens only through `DnsResolver`
- TCP establishment happens only through `TcpConnector`
- TLS establishment happens only through `TlsConnector`
- `openwire-core` exposes `TowerDnsResolver`, `TowerTcpConnector`,
  `TowerTlsConnector`, `DnsResolverService`, `TcpConnectorService`, and
  `TlsConnectorService` for tower interop without changing the core traits
- HTTP/2 binding uses `HyperExecutor` over the configured `WireExecutor` plus
  the configured `hyper::rt::Timer`
- background protocol tasks are spawned only through `WireExecutor::spawn`
- `WireExecutor::spawn` must return a handle that can abort tracked background
  protocol tasks during client shutdown
- `call_timeout`, response-body deadlines, and proxy tunnel timeouts use the
  configured `hyper::rt::Timer`
- response-body deadline enforcement uses the same logical call deadline as the
  request future, with a release/acquire timeout signal between the spawned
  timer task and body polling
- request policy code does not depend on Tokio-specific networking primitives

## 10. Observability And Verification

Current tracing baseline:

- `openwire.call`
- `openwire.attempt`

Current event / trace coverage includes:

- retry / redirect / auth counters
- pool hit / miss and connection reuse
- route-plan events and connect-race winner / loser events
- response-body end / failure lifecycle

Current `EventListener` hook surface includes these call sites:

- call lifecycle: `call_start`, `call_end`, `call_failed`
- DNS lifecycle: `dns_start`, `dns_end`, `dns_failed`
- connect lifecycle: `connect_start`, `connect_end`, `connect_failed`
- raw TCP connect events are emitted by the connector itself exactly once per
  dial attempt; proxy/tunnel failures do not synthesize a second `connect_failed`
- TLS lifecycle: `tls_start`, `tls_end`, `tls_failed`
- request / response body lifecycle
- pool lookup and connection acquire / release
- route-plan and connect-race observability
- retry and redirect callbacks

`openwire.attempt` currently records these stable fields:

- `call_id`
- `attempt`
- `retry_count`
- `redirect_count`
- `auth_count`
- `method`
- `uri`
- `pool_hit`
- `pool_connection_id`
- `route_count`
- `fast_fallback_enabled`
- `connect_race_id`
- `connect_winner`
- `connection_id`
- `connection_reused`

Current test topology:

| File | Scope |
|---|---|
| `crates/openwire/tests/integration.rs` | deterministic end-to-end policy + transport verification |
| `crates/openwire/tests/performance_baseline.rs` | local warm-path / HTTP/2 baseline checks |
| `crates/openwire-test/src/lib.rs` | local HTTP/HTTPS test server and event helpers |

Baseline verification commands:

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo bench -p openwire --bench perf_baseline -- --noplot
```

## 11. Error Model

The public `WireErrorKind` surface is fixed to:

- `InvalidRequest`
- `Timeout`
- `Canceled`
- `Dns`
- `Connect`
- `Tls`
- `Protocol`
- `Redirect`
- `Body`
- `Interceptor`
- `Internal`

Connection-establishment classification is carried separately through
`WireError::establishment_stage()` and currently distinguishes:

- `Dns`
- `Tcp`
- `Tls`
- `ProtocolBinding`
- `ProxyTunnel`
- `RouteExhausted`

Current error-model rules:

- connect timeout is represented as `WireErrorKind::Timeout` plus
  `EstablishmentStage::Tcp`
- route exhaustion is represented as `WireErrorKind::Connect` plus
  `EstablishmentStage::RouteExhausted`
- proxy tunnel failures are represented as `WireErrorKind::Connect` plus
  `EstablishmentStage::ProxyTunnel`
- non-retryable TLS policy failures use `WireErrorKind::Tls` with a
  non-retryable establishment flag
- retry policy decisions and fast-fallback continuation both consume the same
  establishment metadata instead of matching only on `WireErrorKind`
- `WireError` display output includes the underlying source text when one is
  present

Retryability contracts:

- request validation, redirect limit, interceptor, and body errors are not
  retried by connection-failure policy
- retry-on-connection-failure applies only when the request snapshot is
  replayable
- canceled requests are retried only when `retry_canceled_requests` is enabled
  and the request snapshot is replayable
- non-retryable TCP / TLS / proxy-tunnel establishment failures terminate the
  current retry or race path immediately

## 12. Real-Network Validation Architecture

The repository already has strong deterministic local coverage through
`crates/openwire/tests/integration.rs`, `crates/openwire/tests/performance_baseline.rs`,
and the local server helpers in `crates/openwire-test`.

That local coverage remains the primary verification layer for:

- proxy and tunnel behavior
- pool reuse and release bookkeeping
- fast-fallback timing and winner / loser lifecycle
- retry / redirect / auth event ordering
- response-body failure and half-close handling

Current live-validation architecture:

- add a separate live-network integration test target under
  `crates/openwire/tests/`
- keep live helpers inside the `openwire` test tree instead of expanding
  `openwire-test`, which is reserved for local deterministic fixtures
- do not add new production API surface merely to support live-network tests
- do not change the required CI path in `.github/workflows/ci.yml`

Live-suite layout:

```text
crates/openwire/tests/
├── integration.rs
├── performance_baseline.rs
├── live_network.rs
└── support/
    └── mod.rs
```

Live-network suite rules:

- tests are opt-in and `ignored` by default
- first-phase endpoints must be publicly reachable without tokens, accounts,
  temporary URLs, or project provisioning
- the suite should run serially when invoked to reduce public-endpoint flake
- assertions must stay coarse and semantic: status, key headers, JSON fields,
  and `WireErrorKind` classification
- live tests must not become the only coverage for any behavior that depends on
  strict timing or server-side fault injection

Current zero-config public targets:

- `https://httpbingo.org/` for HTTP methods, headers, cookies, redirects,
  status codes, delays, streaming, range, auth, and cache / ETag semantics
- `https://badssl.com/` for certificate failures and TLS-version policy checks
- `https://postman-echo.com/` for a second public echo surface
- `https://jsonplaceholder.typicode.com/` for basic JSON REST smoke checks

Current live coverage is intentionally limited to:

- request / response interoperability against real public origins
- DNS + TCP + TLS establishment against the public internet
- redirect following, cookie-jar roundtrip, timeout, and basic auth smoke paths
- public JSON response parsing and broad header behavior

The following remain local-only:

- custom proxy and proxy-auth flows
- fast-fallback race timing
- exact event ordering and tracing field stability
- HTTP/2 multiplexing and pool reuse detail
- cache semantics beyond narrow smoke coverage

Configured external APIs such as GitHub, Webhook.site, or ReqRes remain outside
the baseline live suite because they introduce tokens, temporary URLs, rate
limits, or project provisioning.

Exact invocation contract for the live suite:

```bash
cargo test -p openwire --test live_network -- --ignored --test-threads=1
```

The live suite remains outside the required CI gate and does not change the
existing `.github/workflows/ci.yml` required path.

Automation contract for repository-hosted execution:

- `.github/workflows/live-network.yml` is a separate workflow from `CI`
- it runs only by `workflow_dispatch` or a weekly `schedule`
- it executes the same serial command used for local live validation:
  `cargo test -p openwire --test live_network -- --ignored --test-threads=1`
- it must never become a dependency of the `CI Gate` job in `.github/workflows/ci.yml`

Current live-network coverage includes:

- `httpbingo` GET / POST echo smoke checks
- `httpbingo` redirect-following and no-redirect client behavior
- `httpbingo` cookie-jar roundtrip, delay-timeout, streaming, ETag, and basic-auth smoke checks
- `badssl` expired, wrong-hostname, and self-signed certificate failures classified as TLS errors
- `postman-echo` GET / POST interoperability smoke checks
- `jsonplaceholder` GET / POST JSON REST smoke checks

TLS-version-policy live checks against `tls-v1-*` `badssl` endpoints remain
deferred because zero-config outcomes vary with the active
platform verifier and OS certificate / policy defaults.

Live-suite maintenance rules:

- keep assertions coarse enough to tolerate benign public-endpoint drift such as
  header casing, CDN-added headers, and incidental payload formatting changes
- prefer checking stable semantic fields like status, echoed query / JSON
  values, redirect location, and `WireErrorKind` classification
- if a public origin becomes flaky or changes shape materially, first loosen the
  assertion if behavior is still semantically equivalent; replace the endpoint
  only when the drift makes the smoke contract no longer credible
- keep the live suite opt-in and serial unless there is strong evidence that a
  parallel contract remains stable across providers

Deferred follow-on themes outside the baseline live suite:

- GitHub REST API: unauthenticated public metadata reads such as
  `GET https://api.github.com/repos/peterich-rs/openwire` work within the
  public rate limit, but broader validation should stay explicitly
  rate-limit-aware and any authenticated or mutating paths require external
  credentials
- Webhook.site: zero-config temporary URLs can be created with
  `POST https://webhook.site/token`, then inspected through
  `/token/{uuid}/requests`, but this adds stateful multi-call orchestration and
  public request-log polling beyond the current baseline smoke suite
- ReqRes: the live API currently requires `x-api-key` from `app.reqres.in`, and
  anonymous requests may return either `missing_api_key` responses or Cloudflare
  challenge pages depending on edge behavior
- Public proxy / tunnel validation: using arbitrary public proxies is not an
  acceptable default because trust, retention, and availability are outside the
  repository's control; proxy semantics remain covered by deterministic local
  fixtures unless a trusted external proxy contract is introduced explicitly
- Public fast-fallback timing: the repository should not claim correctness from
  timing-sensitive assertions on public networks because resolver order, route
  health, and cross-region latency are too unstable
- TLS-version-policy checks: `badssl` still exposes `tls-v1-0`, `tls-v1-1`, and
  `tls-v1-2` endpoints, but pass / fail expectations depend on the active TLS
  backend, verifier, and platform policy rather than only on OpenWire logic

## 13. Current Non-Goals

These are intentionally outside the current baseline:

- WebSocket support
- multipart helpers
- response decompression policy
- speculative connection warming
- nested origin-address racing inside an already-established proxy tunnel
- full negotiated HTTP/2 stream-setting ownership
