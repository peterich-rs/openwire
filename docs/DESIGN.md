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

## 5. Canonical Execution Chain

The runtime path that matters today is:

```text
User API
  -> Client::execute
    -> create CallContext
    -> create EventListener
    -> application interceptors
      -> FollowUpPolicyService
        -> request validation
        -> cookie request application
        -> bridge normalization
          -> network interceptors
            -> TransportService
              -> derive Address
              -> ExchangeFinder
                -> ConnectionPool lookup
                -> if hit:
                  -> acquire pooled bound connection
                -> if miss:
                  -> RoutePlanner
                    -> build RoutePlan
                  -> ConnectorStack
                    -> proxy selection
                    -> dns
                    -> direct-route fast fallback
                    -> sequential proxy-route dialing
                    -> tls
                  -> hyper http1/http2 binding
                  -> insert RealConnection into pool
              -> request write / response head read
        -> cookie response persistence
        -> auth / redirect follow-up decision
    -> response body wrapper
      -> connection release bookkeeping
```

No feature should bypass this chain.

## 6. Connection Core

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

## 7. Protocol Binding

Protocol ownership is intentionally split:

- OpenWire owns address derivation, routing, acquisition, reuse, and release
- `hyper::client::conn::http1` owns HTTP/1.1 protocol state
- `hyper::client::conn::http2` owns HTTP/2 protocol state

This means:

- no request path should reintroduce a legacy pooled client as a shortcut
- protocol binding happens only after OpenWire chooses a winning connection path
- response body wrappers remain the source of release bookkeeping

## 8. Runtime And Adapter Boundaries

Current default adapters:

- Tokio runtime through `Runtime`
- system DNS through `DnsResolver`
- Tokio TCP through `TcpConnector`
- Rustls through `TlsConnector`

Design constraints:

- do not hold pool locks across DNS, TCP, TLS, or protocol-binding awaits
- preserve swappable DNS / TCP / TLS / runtime boundaries
- keep proxy, cookie, auth, and cache policy above transport

## 9. Observability And Verification

Current tracing baseline:

- `openwire.call`
- `openwire.attempt`

Current event / trace coverage includes:

- retry / redirect / auth counters
- pool hit / miss and connection reuse
- route-plan events and connect-race winner / loser events
- response-body end / failure lifecycle

Baseline verification commands:

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo bench -p openwire --bench perf_baseline -- --noplot
```

## 10. Current Non-Goals

These are intentionally outside the current baseline:

- WebSocket support
- multipart helpers
- response decompression policy
- speculative connection warming
- background pool sweeper ownership
- broad proxy-route fast fallback
- full negotiated HTTP/2 stream-setting ownership
