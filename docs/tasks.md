# OpenWire Execution Tasks

Date: 2026-03-21

This is the canonical step-by-step execution tracker for the technical design
defined in [DESIGN.md](DESIGN.md).

Keep this file operational:

- every task has a stable ID
- every task has an explicit status
- tasks are listed in dependency order
- a task is not `DONE` until its exit criteria and verification are satisfied
- when scope changes, update the task row in place instead of creating
  duplicate plan files

## Status Legend

| Status | Meaning |
|---|---|
| `DONE` | Implemented and verified |
| `IN_PROGRESS` | Active work item |
| `TODO` | Accepted, not started |
| `BLOCKED` | Cannot proceed until dependency or decision is resolved |
| `DEFERRED` | Intentionally postponed |

## Maintenance Rules

When updating this file:

1. preserve task IDs
2. update status before adding new child work
3. record blockers explicitly instead of leaving stale TODOs
4. keep verification commands short and concrete
5. prefer adding notes to the existing row over spawning a new overlapping task

## Phase 0: Current Baseline

These items are already landed on the current branch and remain part of the
active baseline.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P0-001 | `DONE` | Stabilize public request API around `http::Request<RequestBody>` + `Client::execute` | None | Examples and README use the standard request path; `cargo test -p openwire --tests` |
| P0-002 | `DONE` | Land follow-up coordinator baseline for retry, redirect, auth, and cookies | P0-001 | Existing retry/redirect/auth/cookie tests pass |
| P0-003 | `DONE` | Land baseline proxy support including HTTP forward proxy and HTTPS CONNECT tunnel | P0-001 | Existing proxy integration tests pass |
| P0-004 | `DONE` | Stabilize response-body observability and connection reuse tracing fields | P0-001 | Event-ordering and tracing regression tests pass |
| P0-005 | `DONE` | Add local performance baseline for warm pooled HTTP/1.1 and HTTPS HTTP/2 | P0-004 | `cargo test -p openwire --test performance_baseline`; `cargo bench -p openwire --bench perf_baseline -- --noplot` |
| P0-006 | `DONE` | Make `DESIGN.md` the canonical technical design and align roadmap/implementation docs | None | Docs reference `DESIGN.md` consistently |

## Phase 1: Scope Freeze For Self-Owned Connection Core

This phase defines exactly what OpenWire will reclaim from
`hyper_util::client::legacy::Client`.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P1-001 | `DONE` | Freeze the target execution chain and ownership split in `DESIGN.md` | P0-006 | `DESIGN.md` includes current chain, target chain, object model, threading/performance/error sections |
| P1-002 | `DONE` | Freeze the initial connection-core scope: exact-address reuse only, no broad coalescing, no HTTP/3 | P1-001 | Scope decision recorded in `DESIGN.md` and reflected in this file |
| P1-003 | `DONE` | Freeze the initial fast-fallback semantics: route-based, dual-stack + single-stack multi-IP, 250ms stagger | P1-001 | Trigger conditions, ordering rules, and winner/loser behavior explicitly documented |
| P1-004 | `DONE` | Freeze the protocol-binding ownership boundary: `hyper` for HTTP codecs, OpenWire for acquisition/pool/orchestration | P1-001 | No remaining ambiguity in `DESIGN.md` on `hyper` vs OpenWire responsibilities |
| P1-005 | `DONE` | Freeze the migration boundary for `hyper-util`: legacy client removed from runtime path, minimal adapter surface retained or replaced | P1-004 | Decision recorded before implementation of Stage 3 begins |

## Phase 2: Planning Types And Route Model

The goal is to introduce the internal planning vocabulary before moving runtime
behavior.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P2-001 | `DONE` | Add internal `Address` type | P1-002 | Fields cover scheme, authority, proxy, TLS identity, protocol policy, DNS policy; unit tests for equality and keying |
| P2-002 | `DONE` | Add internal `Route` type | P2-001 | Supports direct and proxy routes; unit tests for route classification |
| P2-003 | `DONE` | Add internal `RoutePlan` type | P2-002 | Ordered route candidate collection with deterministic iteration |
| P2-004 | `DONE` | Add internal `ConnectPlan` state model | P2-003 | Represents pending/running/won/lost/failed attempt states |
| P2-005 | `DONE` | Add route-family metadata and local-vs-deferred DNS metadata to planning types | P2-002 | Tests cover direct, CONNECT, and future SOCKS-style route representation |
| P2-006 | `DONE` | Add a pure planner test suite for dual-stack ordering and single-stack multi-IP ordering | P2-003 | Tests pass without network I/O |
| P2-007 | `DONE` | Add a pure planner test suite for proxy route construction | P2-003 | Direct, HTTP forward, and CONNECT route cases covered |

## Phase 3: Runtime Adapter Ownership

This phase removes the last runtime-shaped excuses for keeping `hyper-util`
client-side logic in the core runtime path.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P3-001 | `DONE` | Introduce OpenWire-owned Tokio I/O adapter equivalent to the currently used compatibility layer | P1-005 | HTTP/1.1 and HTTP/2 protocol handshakes compile against owned adapter |
| P3-002 | `DONE` | Introduce OpenWire-owned Tokio executor bridge for `hyper` background futures | P3-001 | Handshake/binding code can spawn required tasks without `hyper-util` executor types |
| P3-003 | `DONE` | Introduce OpenWire-owned Tokio timer bridge where `hyper` requires timers | P3-001 | Keep-alive and timeout hooks work through owned runtime adapter types |
| P3-004 | `DONE` | Verify that `openwire`, `openwire-rustls`, and `openwire-test` no longer need `hyper-util` for anything beyond intentionally retained shims | P3-001 | Dependency usage audit documented in code comments or task notes |

## Phase 4: RealConnection And Pool Skeleton

The goal is to own connection lifecycle state before routing request execution
through it.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P4-001 | `DONE` | Add internal `RealConnection` type with connection ID, route, protocol, health, and allocation state | P2-004 | Unit tests cover state transitions |
| P4-002 | `DONE` | Add internal `ConnectionPool` skeleton keyed by `Address` | P4-001 | Pool supports insert, acquire, release, remove |
| P4-003 | `DONE` | Implement exact-address reuse policy for the first milestone | P4-002 | Reuse tests pass with conservative matching only |
| P4-004 | `DONE` | Add idle/in-use bookkeeping without background eviction yet | P4-002 | Pool state tests cover handoff and release |
| P4-005 | `DONE` | Replace `connection_registry`-style reuse detection with pool-owned reuse state | P4-002 | Reuse observability derives from pool state instead of separate HashSet |
| P4-006 | `DONE` | Add eviction settings on the pool object without enabling background cleanup yet | P4-004 | Settings compile and are stored; tests cover config plumbing |

## Phase 5: ExchangeFinder And Acquisition Flow

This phase moves connection acquisition from the legacy pooled client into
OpenWire.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P5-001 | `DONE` | Add internal `ExchangeFinder` that checks the pool before any new connection work | P4-002 | Pool hit / miss behavior covered by unit tests |
| P5-002 | `DONE` | Thread `Address` derivation into request attempts before transport execution | P2-001 | Per-attempt code can derive stable connection keys |
| P5-003 | `DONE` | Route pool hit requests through pooled `RealConnection` handles instead of `hyper_util` pool lookup | P5-001 | Existing reuse integration tests pass against owned pool lookup |
| P5-004 | `DONE` | Route pool miss requests into planner + dialer path | P5-001 | Miss path is covered by integration tests |
| P5-005 | `TODO` | Eliminate `tokio::task_local!` for `CallContext` propagation in the acquisition path | P5-003 | Connector and acquisition code receive explicit context |

## Phase 6: Fast Fallback Direct Routes

This phase lands the first end-to-end OpenWire-owned connection race for
direct routes.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P6-001 | `TODO` | Add `FastFallbackDialer` over `RoutePlan` | P2-004 | Supports queued/staggered attempts and explicit winner/loser states |
| P6-002 | `TODO` | Implement staggered TCP racing for dual-stack routes | P6-001 | Integration tests prove reduced sequential-fallback behavior |
| P6-003 | `TODO` | Implement staggered TCP racing for single-stack multi-IP routes | P6-001 | Integration tests cover multiple A and multiple AAAA scenarios |
| P6-004 | `TODO` | Ensure only the winning TCP connection enters TLS | P6-001 | Loser sockets are canceled before TLS starts |
| P6-005 | `TODO` | Continue remaining route candidates if the TCP winner later fails in TLS or protocol binding | P6-004 | Tests cover TLS failure after TCP success |
| P6-006 | `TODO` | Add cleanup for loser attempts, including completed sockets and aborted handshakes | P6-001 | No resource leaks in race tests |
| P6-007 | `TODO` | Add observability for route count, route index, connect race ID, and winner/loser events | P6-001 | Tracing and event tests cover fast-fallback fields |

## Phase 7: Direct Protocol Binding

After OpenWire can establish a winner connection, it must bind HTTP protocol
state directly through `hyper`.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P7-001 | `TODO` | Add direct HTTP/1.1 protocol binding via `hyper::client::conn::http1` | P3-001, P6-004 | Requests can execute without `hyper_util::client::legacy::Client` on HTTP/1.1 |
| P7-002 | `TODO` | Add direct HTTP/2 protocol binding via `hyper::client::conn::http2` | P3-001, P6-004 | Requests can execute without `hyper_util::client::legacy::Client` on HTTP/2 |
| P7-003 | `TODO` | Preserve connection metadata propagation into responses under the owned protocol path | P7-001 | Existing connection info tests continue to pass |
| P7-004 | `TODO` | Preserve response-body lifecycle wrapping and release bookkeeping under the owned protocol path | P7-001 | Existing body-end/body-failure tests continue to pass |
| P7-005 | `TODO` | Preserve current event ordering guarantees while running through the owned binding path | P7-001 | Existing lifecycle integration tests continue to pass |

## Phase 8: HTTP/1.1 Pool Reuse

HTTP/1.1 should become pool-managed by OpenWire before HTTP/2 multiplexing is
fully generalized.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P8-001 | `TODO` | Mark HTTP/1.1 `RealConnection` busy during an active exchange | P7-001 | Pool refuses parallel reuse on the same HTTP/1.1 connection |
| P8-002 | `TODO` | Return reusable HTTP/1.1 connections to idle state after response body lifecycle completes | P8-001 | Existing reuse integration tests pass |
| P8-003 | `TODO` | Drop broken or abandoned HTTP/1.1 connections instead of reusing them | P8-001 | Tests cover body failure and abandonment paths |
| P8-004 | `TODO` | Add idle timeout and max idle per address enforcement for HTTP/1.1 | P4-006 | Pool eviction tests pass |

## Phase 9: HTTP/2 Multiplex Reuse

The goal is to let OpenWire own HTTP/2 multiplexing decisions and state.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P9-001 | `TODO` | Add HTTP/2-capable `RealConnection` state with multiplex metadata | P7-002 | Pool can distinguish H1 and H2 connection behavior |
| P9-002 | `TODO` | Route multiple exchanges over one HTTP/2 connection when eligible | P9-001 | Existing HTTP/2 multiplex test continues to pass on owned path |
| P9-003 | `TODO` | Track conservative stream usage limits in pool state | P9-001 | Tests prove no invalid reuse under active load |
| P9-004 | `TODO` | Integrate HTTP/2 keep-alive settings with the owned connection model | P9-001 | Existing keep-alive config still plumbed correctly |

## Phase 10: Proxy Route Ownership

Proxy handling should move from connector branching into explicit route
planning.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P10-001 | `TODO` | Move direct/proxy route selection logic into `RoutePlanner` | P2-007, P5-004 | Planner owns direct, forward-proxy, and CONNECT route generation |
| P10-002 | `TODO` | Represent HTTP forward proxy routes as first-class `Route` variants | P10-001 | Absolute-form request routing works through route planner |
| P10-003 | `TODO` | Represent HTTPS CONNECT routes as first-class `Route` variants | P10-001 | CONNECT tunnel path works through route planner |
| P10-004 | `TODO` | Preserve proxy authentication loops in the owned acquisition path | P10-003 | Existing proxy auth integration tests continue to pass |
| P10-005 | `TODO` | Decide whether fast fallback races proxy endpoint addresses, target addresses, or both for each route type | P10-001 | Decision recorded in `DESIGN.md` before broader proxy work proceeds |

## Phase 11: Error Model Refinement

The current `WireErrorKind` surface is sufficient for the baseline but not for
fine-grained route and fast-fallback decisions.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P11-001 | `TODO` | Refine internal connection-establishment outcomes into route-aware categories | P6-005 | Internal error mapping distinguishes TCP, TLS, protocol bind, and route exhaustion |
| P11-002 | `TODO` | Refine retry policy so non-retryable TLS policy failures are not retried like transient connect failures | P11-001 | Retry tests cover TLS-policy vs transient-TLS behavior |
| P11-003 | `TODO` | Ensure fast-fallback continuation uses refined establishment outcomes instead of broad `WireErrorKind` matching | P11-001 | Winner failure continuation tests pass |

## Phase 12: Observability, Benchmarks, And Concurrency Hardening

The goal is to preserve and extend the current strong debugging story.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P12-001 | `TODO` | Add pool hit / miss events and trace fields | P5-001 | Event and tracing tests cover pool lookup results |
| P12-002 | `TODO` | Add route planning and connect-race observability | P6-001 | Fast-fallback tests assert route fields and winner events |
| P12-003 | `TODO` | Add warm-path and cold-connect benchmarks for the owned connection core | P7-001 | Benchmarks cover warm H1/H2 plus cold route racing |
| P12-004 | `TODO` | Add concurrent client usage tests with owned pool and HTTP/2 multiplex reuse | P9-002 | Tests pass under concurrent load without race regressions |
| P12-005 | `TODO` | Add idle eviction timing tests for the owned pool | P8-004 | Deterministic tests cover eviction behavior |

## Phase 13: `hyper-util` Runtime-Path Removal

The last migration step is to retire the legacy pooled client from the request
path.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P13-001 | `TODO` | Remove `hyper_util::client::legacy::Client` from `TransportService` | P7-001, P8-002, P9-002 | Runtime request path no longer builds through legacy client |
| P13-002 | `TODO` | Remove now-unused `hyper-util` client-side feature flags from crate manifests | P13-001 | Dependency graph reduced and verified |
| P13-003 | `TODO` | Audit remaining `hyper-util` usage and keep only intentionally retained adapters or remove the crate entirely | P13-001 | Manifest and code references match the design decision |
| P13-004 | `TODO` | Refresh README and docs to reflect the new owned connection core baseline | P13-001 | Documentation no longer describes legacy client as the runtime core |

## Phase 14: Deferred Work

These items remain intentionally outside the near-term execution path.

| ID | Status | Task | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|
| P14-001 | `DEFERRED` | Broad connection coalescing beyond exact-address reuse | P9-002 | Resume only after the owned pool is stable |
| P14-002 | `DEFERRED` | HTTP cache crate (`openwire-cache`) on top of the owned connection core | P13-001 | Resume after pool/acquisition ownership lands |
| P14-003 | `DEFERRED` | SOCKS support beyond route planning groundwork | P10-005 | Resume after proxy route ownership is stable |
| P14-004 | `DEFERRED` | HTTP/3 | None | No work before connection core stabilization |
| P14-005 | `DEFERRED` | Serde / JSON convenience helpers | None | No work before connection core stabilization |

## Immediate Next Slice

If execution starts now, the next contiguous slice should be:

1. `P5-005`
2. `P6-001` through `P6-007`
3. `P7-001` through `P7-005`

Rationale:

- acquisition now uses `Address` + pool + planner, but still relies on
  temporary task-local propagation into the legacy connector path
- fast fallback is the next missing behavior on the owned acquisition path
- direct protocol binding should follow once the route-race semantics are in place
