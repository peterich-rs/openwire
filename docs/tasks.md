# OpenWire Execution Tasks

Date: 2026-03-21

This file is the step-by-step execution tracker for the accepted
real-network-validation design in `docs/DESIGN.md`.

Scope of this tracker:

- add a zero-config public live-network validation layer for `openwire`
- keep all production API and runtime behavior unchanged unless a task
  explicitly says otherwise
- preserve the current required CI path

Out of scope for this tracker:

- proxy / CONNECT / SOCKS live validation
- fast-fallback timing validation against public endpoints
- exact tracing / event-ordering validation against public endpoints
- configured external APIs that require tokens, temporary URLs, or accounts

## Status Legend

| Status | Meaning |
|---|---|
| `DONE` | Landed on the current branch and verified |
| `TODO` | Accepted and not yet implemented |
| `DEFERRED` | Accepted but intentionally postponed |

## Execution Rules

- preserve task IDs
- work in dependency order
- keep live-network coverage outside required CI checks
- keep deterministic timing and fault-injection coverage in local tests
- prefer coarse semantic assertions over brittle full-response matching

## Current Execution Slice

This tracker implements `DESIGN.md` section `12. Real-Network Validation Architecture`.

## Phase 0: Design And Scope Freeze

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-000 | `DONE` | Record the live-network validation architecture in `DESIGN.md` | `docs/DESIGN.md` | None | `DESIGN.md` defines file layout, endpoint set, CI constraints, and invocation contract |
| LNV-001 | `DONE` | Align `tasks.md` to the accepted design and freeze the first execution slice | `docs/tasks.md` | LNV-000 | `tasks.md` lists the concrete steps below and no longer uses only theme-level placeholders |
| LNV-002 | `DONE` | Freeze the first-phase public endpoint set to zero-config origins only | `docs/DESIGN.md`, `docs/tasks.md` | LNV-000 | Scope remains limited to `httpbingo`, `badssl`, `postman-echo`, and `jsonplaceholder` |

## Phase 1: Test Harness Skeleton

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-100 | `DONE` | Add a dedicated live integration test target | `crates/openwire/tests/live_network.rs` | LNV-001 | New test target compiles as a separate integration test crate |
| LNV-101 | `DONE` | Add shared live-test support helpers inside the `openwire` test tree | `crates/openwire/tests/support/mod.rs` | LNV-100 | Common request, response, JSON, timeout, and endpoint helpers are shared without modifying `openwire-test` |
| LNV-102 | `DONE` | Add any test-only dependencies required by the live suite | `crates/openwire/Cargo.toml` | LNV-100 | Dev-dependencies are sufficient for JSON parsing and helper logic without changing production dependencies |
| LNV-103 | `DONE` | Define standard live-client profiles from existing `ClientBuilder` methods | `crates/openwire/tests/support/mod.rs` | LNV-101 | Helpers construct at least `standard`, `cookie`, `short-timeout`, and `no-redirect` client profiles using existing builder methods only |
| LNV-104 | `DONE` | Make live tests opt-in and serial by invocation contract | `crates/openwire/tests/live_network.rs`, `docs/DESIGN.md`, `README.md` | LNV-100 | Tests are `ignored`; documented invocation is `cargo test -p openwire --test live_network -- --ignored --test-threads=1` |

## Phase 2: HTTPBingo Coverage

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-200 | `DONE` | Add `httpbingo` GET echo smoke coverage | `crates/openwire/tests/live_network.rs`, `crates/openwire/tests/support/mod.rs` | LNV-103 | A live test verifies status, query echo, and broad header behavior against `https://httpbingo.org/get` or equivalent |
| LNV-201 | `DONE` | Add `httpbingo` POST body / header smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-200 | A live test verifies request body transport and selected request headers on a POST endpoint |
| LNV-202 | `DONE` | Add `httpbingo` redirect-following smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-200 | A live test verifies default redirect-following behavior against `redirect-to` |
| LNV-203 | `DONE` | Add `httpbingo` no-redirect client smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-202 | A live test verifies that `follow_redirects(false)` exposes the 3xx response rather than following it |
| LNV-204 | `DONE` | Add `httpbingo` cookie-jar roundtrip smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | A live test uses `Jar` with `cookies/set` then confirms cookie presence on a follow-up request |
| LNV-205 | `DONE` | Add `httpbingo` delay / timeout smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | A live test verifies `WireErrorKind::Timeout` against a delayed response using a short call timeout |
| LNV-206 | `DONE` | Add `httpbingo` stream or range smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-200 | A live test reads a real streaming or ranged response and verifies coarse success semantics |
| LNV-207 | `DONE` | Add `httpbingo` ETag or cache smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-200 | A live test verifies conditional request or cache-related response behavior with broad assertions only |
| LNV-208 | `DONE` | Add `httpbingo` auth smoke coverage using zero-config public paths only | `crates/openwire/tests/live_network.rs` | LNV-200 | A live test verifies basic or bearer auth request handling without adding credentials outside the test case itself |

## Phase 3: TLS Failure Coverage Through BadSSL

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-300 | `DONE` | Add expired-certificate failure coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | Request to `expired.badssl.com` fails as `WireErrorKind::Tls` |
| LNV-301 | `DONE` | Add wrong-hostname failure coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | Request to `wrong.host.badssl.com` fails as `WireErrorKind::Tls` |
| LNV-302 | `DONE` | Add self-signed or untrusted-root failure coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | Request to `self-signed.badssl.com` or `untrusted-root.badssl.com` fails as `WireErrorKind::Tls` |
| LNV-303 | `DONE` | Decide and document whether TLS-version-policy live checks belong in phase 1 or a later slice | `docs/DESIGN.md`, `docs/tasks.md` | LNV-300 | TLS-version-policy live checks are explicitly deferred from the zero-config first slice |

## Phase 4: Secondary Public-Origin Coverage

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-400 | `DONE` | Add `postman-echo` GET smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | A second public echo service verifies broad GET interoperability |
| LNV-401 | `DONE` | Add `postman-echo` POST smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-400 | A second public echo service verifies broad POST interoperability |
| LNV-402 | `DONE` | Add `jsonplaceholder` GET smoke coverage | `crates/openwire/tests/live_network.rs` | LNV-103 | A live test verifies JSON response parsing against a stable public REST response |
| LNV-403 | `DONE` | Add `jsonplaceholder` POST smoke coverage with coarse assertions only | `crates/openwire/tests/live_network.rs` | LNV-402 | A live test verifies request/response interoperability without asserting persistence semantics |

## Phase 5: Invocation, Documentation, And Maintenance

| ID | Status | Task | File Targets | Depends On | Exit Criteria / Verification |
|---|---|---|---|---|---|
| LNV-500 | `DONE` | Document the live-suite command and non-CI status in user-facing docs | `README.md`, `docs/DESIGN.md` | LNV-104 | Docs explain how to run live tests locally and clarify that they are not part of the required CI gate |
| LNV-501 | `DONE` | Add a short maintenance section for endpoint drift and flaky-public-origin handling | `docs/DESIGN.md`, `docs/tasks.md` | LNV-500 | Docs explain that public endpoints require coarse assertions and may need periodic review |
| LNV-502 | `DONE` | Add a separate manual or scheduled live-network workflow without touching `CI Gate` | `.github/workflows/live-network.yml` | LNV-400 | Workflow is independent from required checks and runs only by dispatch or schedule |

## Explicit Deferrals For This Execution Slice

| ID | Status | Theme | Notes |
|---|---|---|---|
| LNV-D01 | `DEFERRED` | GitHub REST API live validation | Requires rate-limit management and optional credentials; outside zero-config phase 1 |
| LNV-D02 | `DEFERRED` | Webhook.site validation | Requires a temporary URL and is not zero-config |
| LNV-D03 | `DEFERRED` | ReqRes validation | Requires `x-api-key`; not part of the zero-config first slice |
| LNV-D04 | `DEFERRED` | Public proxy / tunnel live validation | Proxy and tunnel semantics remain covered by deterministic local fixtures |
| LNV-D05 | `DEFERRED` | Public fast-fallback timing validation | Public networks are not suitable for strict race-timing assertions |
| LNV-D06 | `DEFERRED` | TLS-version-policy live validation | `badssl` `tls-v1-*` outcomes vary with platform verifier and OS policy defaults, so the check is deferred beyond the zero-config first slice |
