# OpenWire Technical Design

Date: 2026-03-21

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
├── crates/openwire-core     shared body, error, event, runtime, transport traits
├── crates/openwire-rustls   optional Rustls TLS connector
├── crates/openwire-test     test support
└── docs/
    ├── DESIGN.md            canonical technical design
    └── tasks.md             active / deferred execution tracker
```

Layering rules:

- `openwire-core` contains shared primitives, not policy behavior
- `openwire` owns public API, follow-up policy, and connection orchestration
- `openwire-rustls` stays swappable behind `TlsConnector`
- `openwire-cache` remains an application-layer crate, not transport logic

Source-file map for the main runtime path:

| File | Primary Types / Functions |
|---|---|
| `crates/openwire/src/lib.rs` | public re-exports |
| `crates/openwire/src/client.rs` | `Client`, `ClientBuilder`, `Call`, `build_service_chain()` |
| `crates/openwire/src/policy/follow_up.rs` | `FollowUpPolicyService`, retry / redirect / auth / cookie orchestration |
| `crates/openwire/src/bridge.rs` | `BridgeInterceptor`, request normalization |
| `crates/openwire/src/transport.rs` | `TransportService`, `ConnectorStack`, direct binding, response-body release |
| `crates/openwire/src/connection/exchange_finder.rs` | `ExchangeFinder`, pool-hit / miss preparation |
| `crates/openwire/src/connection/planning.rs` | `Address`, `Route`, `RoutePlan`, `RoutePlanner` |
| `crates/openwire/src/connection/fast_fallback.rs` | `FastFallbackDialer`, `ConnectPlan` |
| `crates/openwire/src/connection/pool.rs` | `ConnectionPool` |
| `crates/openwire/src/connection/real_connection.rs` | `RealConnection` |

## 3. Ownership Boundaries

| Area | Ownership | Current Default |
|---|---|---|
| HTTP protocol state | Adopt from `hyper` | `hyper` |
| Request policy / follow-ups | OpenWire-owned | `openwire` |
| Connection acquisition / pooling / route planning | OpenWire-owned | `openwire` |
| Fast fallback for direct routes | OpenWire-owned | `FastFallbackDialer` |
| Runtime integration | OpenWire-owned trait boundary | Tokio |
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
- custom integration points are traits, not convenience mini-frameworks

Important extension traits:

- `Interceptor`
- `EventListener` / `EventListenerFactory`
- `CookieJar`
- `Authenticator`
- `DnsResolver`
- `TcpConnector`
- `TlsConnector`
- `Runtime`

`ClientBuilder::default()` currently resolves to these concrete defaults:

| Setting | Default |
|---|---|
| runtime | `TokioRuntime` |
| DNS | `SystemDnsResolver` |
| TCP | `TokioTcpConnector` |
| TLS | `RustlsTlsConnector::builder().build()?` when `tls-rustls` is enabled |
| `call_timeout` | `None` |
| `connect_timeout` | `None` |
| `pool_idle_timeout` | `Some(Duration::from_secs(90))` |
| `pool_max_idle_per_host` | `usize::MAX` |
| `http2_keep_alive_interval` | `None` |
| `http2_keep_alive_while_idle` | `false` |
| `retry_canceled_requests` | `false` |
| `retry_on_connection_failure` | `true` |
| `max_retries` | `1` |
| `follow_redirects` | `true` |
| `max_redirects` | `10` |
| `max_auth_attempts` | `3` |
| `use_system_proxy` | `false` |

Extension boundary contracts:

| Trait | Method Contract | Current Owner |
|---|---|---|
| `CookieJar` | `set_cookies(&mut Iterator<Item=&HeaderValue>, &Url)` and `cookies(&Url) -> Option<HeaderValue>` | `crates/openwire/src/cookie.rs` |
| `Authenticator` | `authenticate(AuthContext) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>>` | `crates/openwire/src/auth.rs` |
| `DnsResolver` | `resolve(CallContext, host, port) -> BoxFuture<Result<Vec<SocketAddr>, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `TcpConnector` | `connect(CallContext, SocketAddr, Option<Duration>) -> BoxFuture<Result<BoxConnection, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `TlsConnector` | `connect(CallContext, Uri, BoxConnection) -> BoxFuture<Result<BoxConnection, WireError>>` | `crates/openwire-core/src/transport.rs` |
| `Runtime` | `spawn(BoxFuture<()>)` and `sleep(Duration)` | `crates/openwire-core` |
| `EventListenerFactory` | `create(&Request<RequestBody>) -> SharedEventListener` | `crates/openwire-core/src/event.rs` |

Current extension-point rules:

- `CookieJar` and `Authenticator` are policy-layer integrations and must not
  own transport decisions
- `DnsResolver`, `TcpConnector`, and `TlsConnector` may affect route execution
  but must not bypass `TransportService`
- `EventListener` is observational only and must not mutate request execution
- custom runtimes must preserve `Runtime::spawn` semantics expected by bound
  connection background tasks

## 5. Service Chain Construction

`build_service_chain()` in `crates/openwire/src/client.rs` constructs the
runtime service stack in this exact order:

```text
application interceptors (in insertion order)
  -> FollowUpPolicyService
    -> BridgeInterceptor
      -> network interceptors (in insertion order)
        -> TransportService
```

Construction details:

- application interceptors are wrapped in reverse iteration so execution order
  matches insertion order
- network interceptors are wrapped in reverse iteration so execution order
  matches insertion order
- `BridgeInterceptor` always runs after follow-up policy and before network
  interceptors
- `FollowUpPolicyService` always wraps the entire network chain

`BridgeInterceptor` currently performs exactly three normalization steps:

- `normalize_host_header()`
- `normalize_user_agent_header()`
- `normalize_body_headers()`

Body-header rules are exact:

- replayable body with known length: set `Content-Length`, remove
  `Transfer-Encoding`
- streaming body on HTTP/1.1: remove `Content-Length`, set
  `Transfer-Encoding: chunked`
- streaming body on non-HTTP/1.1: remove both `Content-Length` and
  `Transfer-Encoding`

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
          -> TransportService::call()
            -> TransportService::execute_exchange()
              -> ExchangeFinder::prepare()
              -> TransportService::acquire_connection()
                -> TransportService::bind_fresh_connection() on miss
                  -> ConnectorStack::route_plan()
                  -> connect_route_plan()
                  -> bind_http1() or bind_http2()
                  -> spawn_http1_task() or spawn_http2_task()
              -> send_bound_request()
              -> ObservedIncomingBody::wrap()
          -> store_response_cookies()
          -> authenticate_response()
          -> resolve_redirect_uri()
```

No feature should bypass this chain.

`FollowUpPolicyService::call()` owns the loop for:

- retry
- redirect
- origin authentication
- proxy authentication
- cookie request application
- cookie response persistence

`TransportService::execute_exchange()` owns:

- pool-hit / miss trace recording
- connection acquisition
- protocol binding on miss path
- request execution on bound connection
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
- `RoutePlanner`: direct and proxy route construction
- `FastFallbackDialer`: staged racing across direct-route candidates

Current connection-core rules:

- exact-address reuse remains the first lookup path
- direct HTTPS HTTP/2 may coalesce only when certificate SANs authorize the
  target authority and the resolved route plan overlaps the connected remote
  address
- direct routes race resolved target addresses
- proxy routes resolve proxy endpoints into `RoutePlan`, but currently dial
  sequentially
- target addresses behind forward proxies / CONNECT tunnels are not part of the
  current fast-fallback race
- HTTP/1.1 reuse is single-exchange and body-lifecycle-driven
- HTTP/2 reuse uses a conservative fixed stream cap until peer settings are
  modeled directly
- HTTP/1.1 idle timeout and max-idle limits are enforced opportunistically on
  pool touch points; there is no background sweeper today

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

Current `RoutePlanner::default()` uses a fixed fast-fallback stagger of
`Duration::from_millis(250)`.

Route ownership rules:

- `RoutePlanner::dns_target()` resolves the proxy endpoint when a proxy is
  configured, otherwise resolves the origin authority
- `RoutePlanner::plan_*()` always returns an ordered `RoutePlan`
- `order_for_fast_fallback()` alternates IPv4 / IPv6 where both families are
  present and otherwise preserves resolver order within the single family

Pool and reuse rules:

- `ExchangeFinder::prepare()` performs the first exact-address pool lookup
- `TransportService::try_acquire_coalesced()` performs the secondary HTTPS
  HTTP/2 coalesced lookup after route planning on miss path
- `ObservedIncomingBody` drives `ConnectionPool::release()` on body completion
- broken HTTP/1.1 exchanges remove the connection instead of returning it to
  idle reuse

Operational invariants:

- `RealConnection::try_acquire()` must fail for unhealthy or closed connections
- HTTP/1.1 may have at most one active allocation per `RealConnection`
- HTTP/2 allocation count is capped at the current fixed limit of `100`
- `RealConnection::release()` increments `completed_exchanges` and restores
  `idle_since` only when the allocation count reaches zero
- `ConnectionPool::remove()` always closes the removed connection
- `ConnectionPool::release()` is valid only for connections that were
  previously acquired
- pool pruning may evict only closed connections or idle HTTP/1.1 connections
  past `idle_timeout`
- HTTP/2 connections are not subject to HTTP/1.1 idle eviction rules
- coalescing is limited to direct HTTPS HTTP/2 connections with verified server
  name authorization and route overlap

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
  -> spawn_http1_task(...) or spawn_http2_task(...)
```

Protocol-specific rules:

- HTTP/1.1 uses one active exchange per connection
- HTTP/2 reuses a bound sender for multiple exchanges
- spawned background connection tasks remove bindings and pool entries on
  termination
- `record_fast_fallback_trace()` writes `route_count`,
  `fast_fallback_enabled`, `connect_race_id`, and `connect_winner` into
  `openwire.attempt`

## 9. Runtime And Adapter Boundaries

Current default adapters:

- Tokio runtime through `Runtime`
- system DNS through `DnsResolver`
- Tokio TCP through `TcpConnector`
- Rustls through `TlsConnector`

Design constraints:

- do not hold pool locks across DNS, TCP, TLS, or protocol-binding awaits
- preserve swappable DNS / TCP / TLS / runtime boundaries
- keep proxy, cookie, auth, and cache policy above transport

Adapter boundaries that are part of the current code shape:

- DNS resolution happens only through `DnsResolver`
- TCP establishment happens only through `TcpConnector`
- TLS establishment happens only through `TlsConnector`
- background protocol tasks are spawned only through `Runtime::spawn`
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

Retryability contracts:

- request validation, redirect limit, interceptor, and body errors are not
  retried by connection-failure policy
- retry-on-connection-failure applies only when the request snapshot is
  replayable
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

Accepted next-step design:

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

Phase-1 zero-config public targets:

- `https://httpbingo.org/` for HTTP methods, headers, cookies, redirects,
  status codes, delays, streaming, range, auth, and cache / ETag semantics
- `https://badssl.com/` for certificate failures and TLS-version policy checks
- `https://postman-echo.com/` for a second public echo surface
- `https://jsonplaceholder.typicode.com/` for basic JSON REST smoke checks

Phase-1 live coverage is intentionally limited to:

- request / response interoperability against real public origins
- DNS + TCP + TLS establishment against the public internet
- redirect following, cookie-jar roundtrip, timeout, and basic auth smoke paths
- public JSON response parsing and broad header behavior

The following remain local-only in the first live-validation phase:

- custom proxy and proxy-auth flows
- fast-fallback race timing
- exact event ordering and tracing field stability
- HTTP/2 multiplexing and pool reuse detail
- cache semantics beyond narrow smoke coverage

Configured external APIs such as GitHub, Webhook.site, or ReqRes are explicitly
deferred to a later verification phase because they introduce tokens, temporary
URLs, rate limits, or project provisioning.

Exact invocation contract for the planned live suite:

```bash
cargo test -p openwire --test live_network -- --ignored --test-threads=1
```

The live suite remains outside the required CI gate and does not change the
existing `.github/workflows/ci.yml` required path.

Current live-network coverage on this branch includes:

- `httpbingo` GET / POST echo smoke checks
- `httpbingo` redirect-following and no-redirect client behavior
- `httpbingo` cookie-jar roundtrip, delay-timeout, streaming, ETag, and basic-auth smoke checks
- `badssl` expired, wrong-hostname, and self-signed certificate failures classified as TLS errors
- `postman-echo` GET / POST interoperability smoke checks
- `jsonplaceholder` GET / POST JSON REST smoke checks

TLS-version-policy live checks against `tls-v1-*` `badssl` endpoints remain
deferred to a later slice because zero-config outcomes vary with the active
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

## 13. Current Non-Goals

These are intentionally outside the current baseline:

- WebSocket support
- multipart helpers
- response decompression policy
- speculative connection warming
- background pool sweeper ownership
- broad proxy-route fast fallback
- full negotiated HTTP/2 stream-setting ownership
