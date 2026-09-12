# OpenWire Architecture

Date: 2026-03-24

OpenWire is an OkHttp-inspired async HTTP client for Rust. `hyper` provides
the HTTP/1.1 and HTTP/2 protocol state machines; OpenWire owns request policy,
route planning, connection lifecycle, pooling, proxy behavior, and
observability.

This document is the current architecture reference. Completed plan and closure
docs are intentionally removed once their behavior is absorbed into the code.

Related roadmap docs:

- `docs/error-handling-roadmap.md`

## 1. Design Priorities

- keep policy and transport clearly separated
- keep platform integrations swappable through trait boundaries
- keep connection ownership and release semantics explicit
- keep the request execution path observable and predictable
- keep the implementation mobile-friendly and cross-platform

## 2. Crate Boundaries

```mermaid
flowchart LR
    OW[crates/openwire<br/>client API and orchestration]
    CORE[crates/openwire-core<br/>shared traits and primitives]
    TOKIO[crates/openwire-tokio<br/>Tokio runtime adapters]
    RUSTLS[crates/openwire-rustls<br/>TLS connector]
    CACHE[crates/openwire-cache<br/>application-layer cache]
    FASTWS[crates/openwire-fastwebsockets<br/>WebSocket engine adapter]
    TUNGSTENITE[crates/openwire-tungstenite<br/>WebSocket engine adapter]
    TEST[crates/openwire-test<br/>test support]

    OW --> CORE
    OW --> TOKIO
    OW --> RUSTLS
    CACHE --> CORE
    CACHE --> OW
    FASTWS --> CORE
    FASTWS --> TOKIO
    TUNGSTENITE --> CORE
    TUNGSTENITE --> TOKIO
    TOKIO --> CORE
    RUSTLS --> CORE
    RUSTLS --> TOKIO
    TEST --> CORE
    TEST --> TOKIO
```

| Crate | Responsibility |
| --- | --- |
| `crates/openwire` | public client API, interceptor chain, follow-up policy, bridge normalization, transparent compression, transport orchestration, connection management, route planning |
| `crates/openwire-core` | shared body types, errors, call metadata, event traits, executor/timer traits, transport traits, policy traits |
| `crates/openwire-tokio` | Tokio executor, timer, I/O adapter, system DNS, TCP connector |
| `crates/openwire-rustls` | default Rustls-backed TLS connector |
| `crates/openwire-cache` | application-layer cache interceptor and store, with conservative RFC 9111 freshness and reuse handling |
| `crates/openwire-fastwebsockets` | optional `fastwebsockets` WebSocket engine adapter |
| `crates/openwire-tungstenite` | optional `tokio-tungstenite` WebSocket engine adapter |
| `crates/openwire-test` | local test support; not published to crates.io |

## 3. Canonical Request Flow

```mermaid
flowchart TD
    A[User API<br/>Client::execute / Call::execute] --> B[Reuse EventListener from Call construction<br/>Create CallContext]
    B --> C[Application Interceptors]
    C --> D[FollowUpPolicyService]
    D --> E[Request Validation]
    E --> F[Cookie Request Application]
    F --> G[BridgeInterceptor]
    G --> H[Network Interceptors]
    H --> I[TransportService]
    I --> J[ExchangeFinder prepare: resolve Address list]
    J --> JA[Request scheduler: priority lanes + dual limits]
    JA --> K[Pool checkout or fresh connection permit]
    K --> L[ConnectorStack]
    L --> M[RoutePlanner -> DNS -> TCP -> TLS]
    M --> N[hyper::client::conn HTTP/1.1 or HTTP/2 binding]
    N --> O[Observed response body wrapper]
    O --> P[Connection release bookkeeping]
D --> Q[Cookie persistence / auth / redirect follow-up decision]
```

No feature should bypass this chain.

HTTP/2 `421 Misdirected Request` recovery stays inside this chain. Transport
marks responses that arrived over a coalesced HTTP/2 connection; after cookie
persistence and authentication handling, `FollowUpPolicyService` may retry a
replayable `421` request on a non-coalesced connection before redirect handling.

`Call::execute()` and `Call::enqueue()` both enter this same chain. Queued calls
only move dispatch onto the client's configured `WireExecutor`; they do not get
a separate transport path. There is no host-aware dispatcher at `Call::enqueue`
because `Address` is unknown until after interceptors. Transport resolves
candidate `Address` values first, then waits for the request scheduler, and only
then checks out a pooled connection. Waiting for a request slot must not pin an
HTTP/1 allocation. `pool_lookup` is emitted after that admission wait, when the
exchange either reuses a pooled connection or decides to dial.

Admission is a client-wide `RequestScheduler`. Waiters from every `Address`
share one ordered set keyed by RFC 9218 urgency (`0` highest … `7` lowest,
default `3`) then enqueue sequence. Per-`Address` caps only skip a waiter
whose host is already at `max_requests_per_host` (optional origin protection;
default unlimited). They do not give each host its own resource budget. Client
sockets and in-flight calls are capped by `max_connections_total` and
`max_requests_total`. So a `u=0` API call can beat a queued `u=7` telemetry
call on a different host when a client slot frees. `Interactive` / `Normal` /
`Bulk` are aliases for `0` / `3` / `7`. Same urgency is FIFO. Aging promotes
the oldest eligible waiter after 8 high-urgency (`0..=2`) promotions, or
~250ms wait. Spare global capacity still lets different hosts run in parallel
when nobody higher-priority is waiting. A freed slot wakes exactly one
promoted waiter. `Call::priority` / `Call::urgency` control both local
admission and, unless the caller already set `Priority`, the outgoing RFC 9218
`Priority: u=N` header. They do not implement HTTP/2 frame PRIORITY. Cache
hits never consume scheduler slots because the scheduler lives in transport.
Scheduler permits are not held across redirects: follow-up drains the
intermediate body, then the next network attempt re-acquires. The permit that
covers the caller-visible response is held until that body is dropped.

`Call::enqueue` emits `dispatcher_queue_start` at enqueue time and inserts a
marker so transport can emit `dispatcher_queue_end` after successful scheduler
acquire. Direct `execute()` emits no dispatcher events. `CallHandle::cancel()`
races against the in-flight execution at the `Client::execute` boundary, and
the response body wrapper keeps observing cancellation after response headers
have been returned so `call_failed` still reflects body-phase cancellation.

`Call::try_clone()` is a request-template operation, not a transport shortcut.
It creates a fresh unexecuted call only when the request body is replayable, and
the cloned call re-enters the canonical flow when executed.

`BridgeInterceptor` owns HTTP request/response normalization that is above the
transport byte stream but below user-facing application interceptors. That
includes `Host`, `User-Agent`, request body framing headers, WebSocket handshake
headers, and transparent compression. When OpenWire synthesizes `Host`, it uses
the URI authority's normal form by omitting default `:80` / `:443` ports while
preserving explicit caller-supplied `Host` values. When any compression codec feature is
enabled (`gzip`, `deflate`, `brotli`, `zstd`; default umbrella `compression`
enables all four), bridge injects `Accept-Encoding` listing only the enabled
codecs, and only for
requests that did not already specify `Accept-Encoding` and are not range
requests. Matching compressed responses are decoded as a stream on the return
path, with `Content-Encoding` and compressed `Content-Length` removed before the
response reaches application interceptors or callers. Transparent decoding
stops with a body error once the decompressed output exceeds the configured
`max_decompressed_body_bytes` (default 128 MiB). For `deflate`, OpenWire peeks
the first two bytes and selects zlib-wrapped (RFC 1950) vs raw DEFLATE. Network interceptors still
observe the normalized request and wire response for each network attempt.

The transport protocol-binding step applies protocol-specific final shaping.
Direct HTTP/1.1 requests are converted to origin-form before they enter hyper's
HTTP/1.1 client binding. HTTP/2 requests keep their absolute URI but strip
connection-specific fields, including fields named by `Connection`; `TE` is
preserved only for the RFC 9113 `trailers` value.

Transport observability is anchored at the same binding boundary. Once a
connection sender is acquired, `connection_acquired` is emitted before
`request_headers_start`; `request_headers_end` means the request has been
handed to hyper's connection sender. If hyper later reports a send failure
without recovering the request message, the surfaced `WireError` is marked
`request_committed` so retry policy can distinguish unapplied failures from
potentially applied requests.

## 4. Transport Layering

```mermaid
flowchart LR
    TS[TransportService] --> EF[ExchangeFinder]
    TS --> CS[ConnectorStack]
    CS --> RP[RoutePlanner]
    RP --> DNS[DnsResolver]
    CS --> TCP[TcpConnector]
    CS --> TLS[TlsConnector]
    CS --> HB[hyper binding]
    HB --> RB[ResponseLease and ObservedIncomingBody]
    RB --> POOL[ConnectionPool release / eviction]
```

Transport is split to keep lifecycle-sensitive code isolated and to preserve a
one-way dependency shape from orchestration down to connection establishment and
response-body cleanup.

| File | Responsibility |
| --- | --- |
| `crates/openwire/src/transport/mod.rs` | wiring and re-exports |
| `crates/openwire/src/transport/service.rs` | acquisition, orchestration, bound send path |
| `crates/openwire/src/transport/connect.rs` | route dialing, proxy tunnel setup, DNS/TCP/TLS handoff |
| `crates/openwire/src/transport/protocol.rs` | HTTP/1.1 and HTTP/2 binding, bound-request normalization |
| `crates/openwire/src/transport/bindings.rs` | binding registry and owned connection-task tracking |
| `crates/openwire/src/transport/body.rs` | response-body lifecycle and release semantics |

Primary runtime anchors outside transport:

- `crates/openwire/src/client.rs`
- `crates/openwire/src/policy/follow_up.rs`
- `crates/openwire/src/bridge.rs`
- `crates/openwire/src/connection/`

## 5. Extension Boundaries

These are the intended customization points:

| Trait / Surface | Role |
| --- | --- |
| `Interceptor` | application or network request/response interception |
| `EventListener` / `EventListenerFactory` | call-level and transport-level observability. Factory runs at `Call` construction so `canceled` and dispatcher events have a listener before execute |
| `CookieJar` | request cookie application and response cookie persistence |
| `Authenticator` | origin and proxy authentication follow-ups, with `AuthContext::challenges()` exposing RFC 9110 / RFC 7235 `WWW-Authenticate` and `Proxy-Authenticate` challenges |
| `RetryPolicy` | connection-failure and response-status retry decisions |
| `RedirectPolicy` | redirect decisions |
| `ProxySelector` | per-attempt ordered proxy candidate resolution |
| `DnsResolver` | host resolution |
| `TcpConnector` | TCP transport establishment |
| `TlsConnector` | TLS handshake / stream wrapping |
| `RoutePlanner` | direct and proxy route construction |
| `WireExecutor` | background task spawning |
| `hyper::rt::Timer` | timer integration |

Typical `EventListener` nesting (OkHttp-aligned). OpenWire extras are marked with `*`:

```
call_start / call_end / call_failed / canceled
  dispatcher_queue_start / dispatcher_queue_end   (enqueue only; end means admitted)
  proxy_select_start / proxy_select_end
  dns_start / dns_end / dns_failed*
  connect_start / connect_end / connect_failed
    tls_start / tls_handshake* / tls_end / tls_failed*
  pool_lookup* / route_plan* / connect_race_*
  connection_acquired / connection_released
    request_headers_start / request_headers_end
    request_body_start / request_body_end / request_failed
    response_headers_start / response_headers_end
    response_body_start / response_body_end / response_body_failed / response_failed
  retry / retry_decision / redirect / follow_up_decision
  cache_hit / cache_miss / cache_conditional_hit / satisfaction_failure
```

`EventListenerFactory::create` runs when `Client::new_call` (or `Call::try_clone`) builds the `Call`, not at execute time. Direct `execute()` does not emit dispatcher events. For `Call::enqueue`, `dispatcher_queue_end` means the scheduler admitted the call, not that the executor task started.

## 5b. Cargo features

Default: `tls-rustls`, `platform-verifier`, `compression`, `cookies`.

Codec features `gzip`, `deflate`, `brotli`, and `zstd` can be enabled independently. `compression` is the umbrella of all four. `compression-core` is the internal module gate pulled in by any codec; enabling it with no codec compiles and injects no `Accept-Encoding`. Workspace pins follow a minimum-feature policy: `tokio` and `hyper` no longer enable `full`; `tower` is `util` only; `rustls` / `tokio-rustls` keep `std` + `tls12` + `aws_lc_rs` and drop `logging`; `futures-util` keeps `std` + `async-await` and adds `io`/`sink` only where used. Production crates request Tokio `rt`/`net`/`time`/`io-util`/`sync` and hyper `client`/`http1`/`http2`. `openwire-test` additionally enables `hyper/server`. `openwire-cache` depends on `openwire` with `default-features = false`.

The built-in `Jar` is behind `cookies`. Custom `CookieJar` implementations do not need that feature. `json` gates JSON body helpers and `LoggerInterceptor` pretty-print.

`openwire-cache` is intentionally an application interceptor rather than a
transport feature. Fresh cache hits short-circuit before the follow-up
coordinator; cache misses continue through the canonical request flow. The
crate currently implements explicit and conservative heuristic freshness rules
for private in-process caching: request `Cache-Control` directives such as
`no-cache`, `no-store`, `max-age=0`, `max-stale`, `min-fresh`, and
`only-if-cached`, plus HTTP/1.0-compatible request `Pragma: no-cache` when
`Cache-Control` is absent; response `max-age`, `must-revalidate`, `no-cache`,
`no-store`, `public`, `s-maxage`, `Expires`, `Date` apparent age,
Last-Modified heuristic freshness, `Age`, invalid duplicate freshness fields,
and `Vary` matching, including multiple stored variants per URI. Responses to
authenticated requests are stored only when `public`, `s-maxage`, or
`must-revalidate` explicitly permits it; stored authenticated responses also
require the original `Authorization` value to match, even when the server does
not include `Authorization` in `Vary`. Because this is a private cache,
`s-maxage` is treated as an authenticated-storage permit rather than as a
private freshness override. It also
revalidates stale stored responses that carry `ETag` or `Last-Modified`
validators, refreshing stored metadata on `304 Not Modified` before returning
the cached body as `200 OK`. Explicit `max-stale` requests can reuse stale
stored responses when the cached response does not require validation; stale
if-error and background stale revalidation are not implemented. Non-error `2xx`
/ `3xx` unsafe-method responses invalidate stored responses for the request
target URI, plus same-host `Location` and `Content-Location` response URIs
when present.

Default runtime stack from `ClientBuilder::default()`:

- Tokio executor and timer
- direct connection proxy policy via an empty `ProxyRules`
- system DNS resolver
- Tokio TCP connector
- Rustls TLS connector when the `tls-rustls` feature is enabled

## 5a. WebSocket Upgrade Path (`feature = "websocket"`)

`Client::new_websocket(request)` is the dedicated WebSocket entry point.
It returns a `WebSocketCall` builder; `.execute()` performs the handshake
and returns a `WebSocket` (sender + receiver halves).

```mermaid
flowchart TD
    W[Client::new_websocket]
    W --> WB[Bridge: inject Sec-WebSocket-* headers, force HTTP/1.1]
    WB --> WC[ConnectorStack: route_plan + connect_route_plan]
    WC --> WH[bind_websocket_handshake: HTTP/1.1 GET + hyper::upgrade]
    WH --> WV[Validate 101 response]
    WV --> WE[WebSocketEngine::upgrade]
    WE --> WS[spawn_session: writer + reader + heartbeat]
    WS --> WSOK[WebSocket: Sender + Receiver]
```

The WebSocket flow follows `bridge → ConnectorStack → bind_websocket_handshake`,
diverging from the HTTP path at the binding step (it uses
`http1::handshake(io).with_upgrades()` and `hyper::upgrade::on` instead
of `bind_http1` / `bind_http2`). Engine selection is pluggable via
`WebSocketEngine`; the bundled `NativeEngine` implements RFC 6455 directly.

In v1 the WS path does not reuse `TransportService` or its application /
network interceptors, and its connection is not pooled. See
`docs/websocket-design.md` for the full specification and the v2
follow-ups (pool reuse, interceptor chain integration).

## 6. Operating Rules

- `FollowUpPolicyService` owns retry, redirect, auth, and cookie follow-ups.
  Response-status retries are policy decisions after cookie persistence and
  authentication handling and before redirect handling. The default retry policy
  only retries **idempotent** replayable `408 Request Timeout` responses and
  `503 Service Unavailable` responses that explicitly carry `Retry-After: 0`
  (set `retry_non_idempotent(true)` to extend response-status retries to
  non-idempotent methods); delayed, invalid, or duplicate `Retry-After` values
  remain caller-visible responses. Default redirect handling follows `301`,
  `302`, `303`, `307`, and `308` when a valid `Location` is present and policy
  permits it; `300 Multiple Choices` is returned to the caller without automatic
  following. Cross-origin redirects strip `Authorization`, `Cookie`, and common
  API-token headers (`X-Api-Key`, `X-Auth-Token`, and related) while preserving
  `Proxy-Authorization` for sticky proxy routing. Preserve-method redirects
  (`307` / `308`) require a replayable request body; otherwise the original
  redirect response is returned to the caller.
- The default `Jar` cookie store loads an embedded public suffix list so
  `Domain=.com`-style cookies are rejected (RFC 6265 §5.3) and honors `Secure`.
- Client resource caps (`max_requests_total`, `max_connections_total`) are
  independent of HTTP protocol rules. HTTP/1 remains one exchange per
  connection (exclusive `SendRequest` checkout; overlapping calls to the same
  host open additional connections rather than pipelining). HTTP/2 multiplexes
  on a reused connection up to the local stream budget and peer SETTINGS.
  Optional per-host caps default to unlimited and only constrain origin
  stampede, not those protocol gates. Pool defaults still include idle timeout,
  max idle per address (eviction hygiene), absolute max lifetime, and the HTTP/2
  stream budget. Dual-stack route planning prefers starting with IPv6 when both
  families are present (staggered dial, not full Happy Eyeballs v2).
- Request validation rejects non-HTTP(S) schemes, missing authorities or hosts,
  and HTTP URI authorities that include userinfo before bridge normalization can
  derive `Host` or transport can route the request.
- `TransportService` owns connection acquisition, route execution, protocol
  binding, and bound request dispatch.
- HTTP/2 coalescing remains a transport optimization. `TransportService` tags
  caller-visible responses that used a coalesced HTTP/2 connection, while
  `FollowUpPolicyService` retries only replayable `421 Misdirected Request`
  responses carrying that internal tag. The retry request carries an internal
  no-coalescing marker so the next attempt opens or reuses an exact-authority
  connection instead of another coalesced route.
- Forward-proxy HTTP `407` follow-ups are only attempted when the transport
  response carries a selected proxy route. Direct-origin `407` responses remain
  caller-visible responses and do not invoke the proxy authenticator.
- CONNECT proxy `407` challenges are handled during tunnel establishment because
  no end-to-end HTTP response exists yet. That tunnel-local proxy auth loop must
  still receive the logical call counters from `FollowUpPolicyService`; the
  `AuthContext` passed to the proxy authenticator carries the current total
  attempt, retry count, redirect count, and logical auth count plus any completed
  CONNECT-local auth retries. The same logical auth budget gates this loop, so
  CONNECT tunnel proxy authentication cannot exceed the per-call
  `max_auth_attempts` limit by resetting its own local counter. CONNECT retry
  headers are sanitized to the synthetic tunnel `Host` plus proxy-authentication
  headers, so origin auth, cookies, body framing, and other request headers are
  not forwarded into the proxy tunnel handshake.
- `Client::execute` owns call cancellation, final call completion, and wraps the
  returned response body so `call_end` / `call_failed` reflect the whole call.
- `Call::enqueue` is executor-backed dispatch for the same `Call::execute`
  behavior, not a separate policy or transport implementation. The scheduler
  seam is after interceptors, inside `TransportService`.
- `ResponseLease` and `ObservedIncomingBody` own final release bookkeeping.
- HTTP/1.1 reuse is single-exchange and response-body-lifecycle-driven.
- HTTP/2 multiplexing is governed by connection health, allocation tracking,
  a local concurrent-stream budget (default 100), and bound-sender readiness.
- `hyper` owns protocol engines and HTTP I/O; OpenWire owns client semantics,
  admission, pooling, and connection wait. OpenWire does not pipeline HTTP/1,
  split HTTP sockets, or implement HTTP/2 frame PRIORITY.

## 7. Verification Strategy

- unit tests guard protocol parsing, pooling, route planning, timeout, and
  response-lease behavior
- integration tests guard retry/redirect/auth/cookie flow, proxy behavior, and
  connection lifecycle
- the live-network suite is opt-in and not part of the required CI gate

Primary verification commands:

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Optional live-network smoke suite:

```bash
cargo test -p openwire --test live_network -- --ignored --test-threads=1
```

## Connection teardown and connect budgets

- Pool eviction (idle timeout, max idle, max lifetime, explicit remove) aborts the
  owned hyper connection task and clears protocol bindings so sockets are closed,
  not only removed from reuse indexes.
- `connect_timeout` covers TCP establishment, TLS handshake, and protocol binding
  for direct and proxy-tunneled paths.
- HTTP/2 temporary sender unreadiness is handled by awaiting `ready()` on the
  acquired sender under the call deadline, not by parking on pool availability.
- Intermediate follow-up responses (auth / redirect / status retry) drain the
  body up to a small cap before the next network attempt so HTTP/1 connections
  can be reused when possible.
- Transparent decompression failures mark the call for connection discard so
  HTTP/2 connections are not returned to the pool as healthy after a body error.

## Performance notes

- The connection pool, request-scheduler per-address index, connection-wait
  index, and connection-limiter maps are sharded by address (`32` shards) so
  independent hosts do not share a mutex. Address keys keep SipHash;
  `ConnectionId` and coalescing `SocketAddr` maps use FxHash. Per-address
  connection lists are `SmallVec`. Short critical sections use `parking_lot`.
  HTTP/2 coalescing still uses a shared index keyed by direct route target.
- Connection wait is per-`Address` with `Notify::notify_one` for HTTP/1 busy
  and same-host reuse. A waiter for host A does not complete when only host B
  is notified. A freed **global** connection slot uses `notify_waiters` so a
  host that can actually use the total cap is not starved by a host still at
  its per-address cap. `listen` constructs the `Notified` futures before the
  caller probes capacity, so a `notify_waiters` that lands after `try_acquire`
  fails is not lost. HTTP/2 stream release also `notify_one`s currently
  listening authorities that can coalesce onto that connection (verified
  server names, same port, direct HTTPS); the socket staying open means the
  global channel would not run. Notification storage is inserted only by
  `listen` and reclaimed when the last waiter for that address drops, so
  sequential destinations do not retain an entry per historical host. The
  wait future is created after pool/binding mutexes are released.
- HTTP/2 `TransportConfig` knobs (window sizes, frame/header limits, keep-alive
  timeout, reset-stream cap) and HTTP/1 `writev` / `title_case_headers` are
  applied in `bind_http1` / `bind_http2`. TCP linger and buffer sizes are
  optional on `TokioTcpConnector` and are set on `TcpSocket` before connect.
  Defaults keep nodelay on and leave buffers/linger to the OS.
- Dual-stack route plans share a single `Arc<Address>` across candidate routes
  instead of cloning the full address key per IP.
- Follow-up `RequestSnapshot` stores headers and extensions behind `Arc` so
  auth challenge construction and retry rebuilds avoid re-cloning large maps
  when only metadata is inspected.
- `ResponseBody::text()` reclaims the collected `Bytes` buffer via `into()` when
  unique, avoiding an extra `to_vec()` copy on the common path.

- Default clients wrap the system resolver in `CachingDnsResolver` (30s positive /
  5s negative TTL) to avoid repeated system lookups under connection churn.
- Follow-up snapshots stay light when redirects, retries, and authenticators are
  all disabled, so the common single-shot path skips header/extension cloning.
- Request admission permits are held via response extensions into the call
  lifecycle body, avoiding an extra `BoxBody` layer on the returned response.
- Pool checkout is a post-admission step. `ExchangeFinder::prepare` only
  resolves addresses; an idle HTTP/1 connection is not reserved while the call
  is still waiting for a request slot. A non-acquiring `has_acquirable_connection`
  hint may reorder candidates so a host with an idle connection is tried first.
- Request admission waiters sit in one client-wide ordered set: RFC 9218
  urgency then FIFO seq, across hosts. Per-address caps are eligibility
  filters, not separate queues. A freed slot wakes the next promoted waiter,
  not every waiter. Client resource caps are `max_requests_total` and
  `max_connections_total`. Optional per-address caps default to unlimited.
  When a per-address cap is set, waiting on one host still does not consume a
  client-wide request slot. Queue precedence ignores waiters that cannot run
  because their per-address cap is full, so a different host with spare
  global and per-host capacity is not rejected with `Capacity` while the
  queue holds only ineligible waiters. `max_queued_requests` fails
  with `WireErrorKind::Capacity` before taking a running slot. Dropping a
  queued or already-promoted acquire future must not leak running counts.

