# Trait-Oriented Redesign — Completed Closure

Date: 2026-03-22
Status: implemented on `main`

This document records the implemented trait and crate-boundary redesign. Use
[`docs/DESIGN.md`](DESIGN.md) as the source of truth for current runtime flow;
use this file for the final crate split, public boundary decisions, and the
milestone closure record.

## Final Outcome

The redesign completed six goals:

- moved Tokio-specific concrete adapters into `openwire-tokio`
- replaced the old runtime bundle with explicit executor and timer boundaries
- removed direct Tokio usage from non-test framework paths
- moved policy traits and contexts into `openwire-core`
- made route planning a public strategy boundary in `openwire`
- added tower interop adapters for DNS / TCP / TLS connectors in `openwire-core`

## Final Crate Boundaries

| Crate | Owns |
|---|---|
| `openwire-core` | shared request/response bodies, errors, call/connection context, event and interceptor traits, executor/timer traits, transport traits, policy traits, and connector tower adapters |
| `openwire-tokio` | `TokioExecutor`, `TokioTimer`, `TokioIo`, `SystemDnsResolver`, and `TokioTcpConnector` |
| `openwire` | public client API, follow-up orchestration, bridge normalization, transport orchestration, connection management, route planning, `Jar`, `DefaultRetryPolicy`, and `DefaultRedirectPolicy` |
| `openwire-rustls` | default Rustls-backed `TlsConnector` implementation |
| `openwire-cache` | application-layer cache integration; still intentionally separate from transport |

## Public Boundary Decisions

### Runtime / Execution

- `Runtime` and `TokioRuntime` are gone.
- `openwire-core` exports `TaskHandle`, `BoxTaskHandle`, `WireExecutor`,
  `HyperExecutor`, and `SharedTimer`.
- `ClientBuilder` configures execution with `executor(...)` and `timer(...)`.
- `ClientBuilder::default()` uses `openwire_tokio::TokioExecutor` plus
  `openwire_tokio::TokioTimer`.

### Policy

- `CookieJar`, `Authenticator`, `RetryPolicy`, and `RedirectPolicy` live in
  `openwire-core`.
- `openwire` re-exports those traits and contexts for convenience.
- `openwire` keeps policy orchestration plus concrete defaults:
  `Jar`, `DefaultRetryPolicy`, and `DefaultRedirectPolicy`.

### Planning

- `RoutePlanner` is a public trait in `openwire`.
- `ConnectorStack` stores `Arc<dyn RoutePlanner>`.
- `ClientBuilder::route_planner(...)` accepts custom planners.
- `openwire` publicly re-exports `Address`, `AuthorityKey`, `Route`,
  `RouteFamily`, `RoutePlan`, `DefaultRoutePlanner`, `ProxyConfig`,
  `ProxyEndpoint`, `ProxyMode`, `ProxyScheme`, `TlsIdentity`,
  `ProtocolPolicy`, `DnsPolicy`, and `UriScheme`.

### Connector Tower Interop

`openwire-core` exports bidirectional tower adapters:

- request types: `DnsRequest`, `TcpConnectRequest`, `TlsConnectRequest`
- trait-to-service wrappers: `DnsResolverService`, `TcpConnectorService`,
  `TlsConnectorService`
- service-to-trait wrappers: `TowerDnsResolver`, `TowerTcpConnector`,
  `TowerTlsConnector`
- boxed service aliases: `BoxDnsService`, `BoxTcpService`, `BoxTlsService`

These adapters preserve the existing `&self` connector traits while allowing
tower-based composition when callers want it.

## Import Rules For Downstream Code

Use these imports for the final split:

| Need | Import from |
|---|---|
| client API, request bodies, policy traits, planning types | `openwire` |
| Tokio executor / timer / DNS / TCP / `TokioIo` | `openwire-tokio` |
| connector tower adapters and boxed connector services | `openwire-core` |
| default Rustls TLS connector | `openwire-rustls` or the `openwire` re-export when the feature is enabled |

Important consequence: `openwire` no longer re-exports Tokio adapter types.

## Milestone Closure

| Milestone | Final result |
|---|---|
| `M1` | Tokio-only adapters moved to `openwire-tokio` |
| `M2` | executor/timer split established with `WireExecutor`, `HyperExecutor`, and `SharedTimer` |
| `M3` | non-test framework paths no longer depend directly on Tokio APIs |
| `M4` | policy traits moved into `openwire-core`; `openwire` kept orchestration and defaults |
| `M5` | route planning became a public strategy boundary through `RoutePlanner` |
| `M6` | `Runtime` compatibility layer removed and connector tower adapters added |

## Behavior That Had To Stay Intact

The redesign preserved the baseline established by the completed core review:

- RAII cleanup from connection acquisition through response-body release
- abort-capable tracking of owned connection tasks on final client drop
- explicit request-admission and fresh-connection admission limits
- `RequestBody::absent()` vs `RequestBody::explicit_empty()` semantics
- default rejection of `https -> http` downgrade redirects
- proxy credential propagation and proxy-path fast fallback
- pool coalescing indexing, idle eviction, and poison-safe lock recovery
- Tower readiness and backpressure behavior across interceptor and follow-up layers

## Verification Summary

Representative verification used while landing the redesign:

- `cargo test --workspace --lib`
- `cargo test -p openwire --tests --examples --no-run`
- `cargo clippy --workspace --all-targets -- -D warnings`

Direct coverage was also added for:

- custom route planners through `ConnectorStack`
- custom `WireExecutor` failure and task-abort paths
- connector tower adapter round-trips in `openwire-core`

## Relationship To Other Docs

- [`docs/DESIGN.md`](DESIGN.md): current architecture and execution chain
- [`docs/core-review.md`](core-review.md): completed core-review summary
- [`docs/plans/core-review-plan-spec.md`](plans/core-review-plan-spec.md):
  historical closure map for the core-review workstreams
- [`docs/tasks.md`](tasks.md): remaining deferred live-network validation work

If a future refactor changes these boundaries again, update this file to record
the new final decision set rather than turning it back into a speculative plan.
