# Core Review Plan Spec — Historical Closure Map

Date: 2026-03-22
Source review: [../core-review.md](../core-review.md)
Status: completed and merged into `main`

This file no longer describes an active backlog. It preserves the workstream
split from the finished core review so future refactors can quickly see which
architectural constraints were established by that review and where they now
live in the codebase and docs.

## Landed Workstreams

| Workstream | Landed outcomes | Current anchors |
| --- | --- | --- |
| `WS-01 Lifecycle / RAII` | RAII cleanup for acquired connections and response leases; tracked connection tasks abort on final client drop | `crates/openwire/src/transport/bindings.rs`; `crates/openwire/src/transport/body.rs`; `docs/DESIGN.md` sections 6, 8, 9 |
| `WS-02 Policy / Body Semantics` | `RequestBody::absent()` vs `explicit_empty()`; downgrade redirect guard; real readiness propagation | `crates/openwire-core/src/body.rs`; `crates/openwire/src/bridge.rs`; `crates/openwire/src/policy/follow_up.rs`; `docs/DESIGN.md` sections 5, 6 |
| `WS-03 Connectivity / Admission Control` | request and connection admission limits; sharded `ConnectionBindings`; single-owner connect failure signaling | `crates/openwire/src/client.rs`; `crates/openwire/src/connection/limits.rs`; `crates/openwire/src/transport/bindings.rs`; `crates/openwire/src/transport/service.rs`; `docs/DESIGN.md` sections 4, 6, 7 |
| `WS-04 Proxy / Route Dialing` | proxy credentials propagate through planning; proxy routes use shared fast fallback; SOCKS5 username/password auth | `crates/openwire/src/proxy.rs`; `crates/openwire/src/connection/planning.rs`; `crates/openwire/src/connection/fast_fallback.rs`; `crates/openwire/src/transport/connect.rs`; `docs/DESIGN.md` section 7 |
| `WS-05 Pool / State Management` | protocol-agnostic idle eviction; coalescing secondary index; poison-safe lock recovery | `crates/openwire/src/connection/pool.rs`; `crates/openwire/src/cookie.rs`; `crates/openwire/src/sync_util.rs`; `docs/DESIGN.md` section 7 |
| `WS-06 Protocol / Low-level Correctness` | RFC 9110 `Connection` token parsing; acquire/release ordering for body deadline signal | `crates/openwire/src/transport/protocol.rs`; `crates/openwire/src/transport/body.rs`; `docs/DESIGN.md` sections 8, 9 |

## Verification That Now Guards These Outcomes

| Area | Representative coverage |
| --- | --- |
| Lifecycle | cancellation / timeout cleanup, HTTP/2 allocation release, client-drop task abort tests |
| Policy | absent-body framing, downgrade redirect rejection, readiness propagation tests |
| Connectivity | request-limit and connection-limit integration tests, connect failure lifecycle checks |
| Proxy | SOCKS5 username/password fixtures, proxy fast-fallback integration tests |
| Pool | HTTP/2 idle eviction, coalescing index correctness, poison recovery tests |
| Protocol | multi-value `Connection` header parsing, deadline ordering smoke tests |

## Ongoing Relevance

Subsequent refactors should treat the following as established baseline behavior,
not as open backlog:

- RAII ownership from connection acquisition through response-body release
- abort-capable task handles for owned connection tasks
- request / connection admission and real Tower backpressure propagation
- proxy credential propagation and proxy-path fast fallback
- protocol-agnostic pool eviction and poison-safe lock recovery
- RFC-compliant HTTP/1.1 reuse checks and deadline memory ordering

For current behavior, prefer [`docs/DESIGN.md`](../DESIGN.md).
For the final trait / crate boundary closure, prefer
[`docs/trait-oriented-redesign.md`](../trait-oriented-redesign.md).
