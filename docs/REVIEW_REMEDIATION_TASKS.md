# OpenWire Review Remediation Execution Tasks

Date: 2026-03-22
Status: Executed and verified
Source design: `docs/REVIEW_REMEDIATION_DESIGN.md`

This file is the execution tracker for the remediation implementation design.
It is intentionally separate from `docs/tasks.md`, which remains the deferred
live-network validation tracker.

## 1. Execution Rules

- execute tasks strictly in dependency order
- each task must land with its tests in the same change set
- do not start a dependent task until the predecessor is green
- keep each task mergeable on its own
- update `docs/DESIGN.md` only at the final documentation sync step unless a
  prior task requires a factual correction to avoid misleading readers

## 2. Status Legend

| Status | Meaning |
|---|---|
| `TODO` | not started |
| `IN PROGRESS` | actively being implemented |
| `BLOCKED` | waiting on a predecessor or a design decision |
| `DONE` | landed and verified |

## 3. Execution Order

| Order | Task ID | Theme | Status |
|---|---|---|---|
| 1 | `RR-001` | Shared proxy selector foundation | `DONE` |
| 2 | `RR-002` | Canonical origin comparison | `DONE` |
| 3 | `RR-003` | Redirect header carry-over matrix | `DONE` |
| 4 | `RR-004` | CONNECT header/tail split parser | `DONE` |
| 5 | `RR-005` | Prefetched CONNECT tail wrapper | `DONE` |
| 6 | `RR-006` | Fresh connection publication ordering | `DONE` |
| 7 | `RR-007` | HTTP/2 body-abandon semantics | `DONE` |
| 8 | `RR-008` | Unhealthy pool pruning | `DONE` |
| 9 | `RR-009` | `retry_canceled_requests` wiring | `DONE` |
| 10 | `RR-010` | Runtime-based request timeout helper | `DONE` |
| 11 | `RR-011` | Body-stage deadline enforcement | `DONE` |
| 12 | `RR-012` | Final docs sync and closeout | `DONE` |

## 4. Task Breakdown

## RR-001 Shared Proxy Selector Foundation

Status: `DONE`
Depends on: none

Objective:

- introduce one reusable proxy-selection path that can be shared by follow-up
  policy and exchange preparation

Files:

- `crates/openwire/src/proxy.rs`
- `crates/openwire/src/connection/exchange_finder.rs`
- `crates/openwire/src/client.rs`

Implementation steps:

1. add internal `ProxyRuleId`, `ProxySelection`, and `ProxySelector`
2. move ordered proxy matching from ad hoc `Vec<Proxy>` scanning into the
   selector
3. update `ExchangeFinder` to hold `Arc<ProxySelector>` instead of `Vec<Proxy>`
4. wire the selector from `ClientBuilder::build()`
5. keep current proxy matching behavior identical for non-redirect paths

Tests:

- existing proxy integration tests must continue to pass unchanged
- add a unit test proving selector returns the first matching rule

Done when:

- `ExchangeFinder` no longer owns raw proxy rules directly
- proxy selection behavior is unchanged for current request execution

## RR-002 Canonical Origin Comparison

Status: `DONE`
Depends on: `RR-001`

Objective:

- replace raw `authority()` equality with origin-aware comparison using
  normalized host and effective port

Files:

- `crates/openwire/src/policy/follow_up.rs`

Implementation steps:

1. add internal `OriginKey`
2. implement `OriginKey::from_uri(&Uri) -> Result<OriginKey, WireError>`
3. replace `same_authority()` with `same_origin()`
4. normalize port handling so `host` and `host:80/443` compare equal

Tests:

- add a unit or integration test for `http://host/... -> http://host:80/...`
- add a unit or integration test for `https://host/... -> https://host:443/...`

Done when:

- redirect logic no longer depends on raw `Uri::authority()` string equality

## RR-003 Redirect Header Carry-Over Matrix

Status: `DONE`
Depends on: `RR-001`, `RR-002`

Objective:

- implement the redirect header policy from the remediation design exactly

Files:

- `crates/openwire/src/policy/follow_up.rs`
- `crates/openwire/src/client.rs`

Implementation steps:

1. pass `Arc<ProxySelector>` into `FollowUpPolicyService`
2. compute `same_origin` and `same_proxy` inside redirect rebuild
3. always drop `Host`
4. drop `Authorization` and explicit `Cookie` only when origin changes
5. drop `Proxy-Authorization` only when the selected proxy changes
6. keep current method-switch handling for `Content-Length` and `Content-Type`

Tests:

- `redirect_same_origin_default_port_preserves_authorization`
- `redirect_cross_origin_drops_explicit_cookie_header`
- `redirect_same_proxy_preserves_proxy_authorization`
- `redirect_different_proxy_drops_proxy_authorization`

Done when:

- the redirect path uses one centralized carry-over matrix
- proxy credentials survive same-proxy redirects
- explicit cookies do not leak across origins

## RR-004 CONNECT Header/Tail Split Parser

Status: `DONE`
Depends on: none

Objective:

- parse CONNECT headers without validating tail bytes as header text

Files:

- `crates/openwire/src/transport.rs`

Implementation steps:

1. add `ParsedConnectResponse`
2. add `split_connect_head(&[u8]) -> Result<(&[u8], &[u8]), WireError>`
3. change `parse_connect_response_head()` to accept only header bytes
4. keep current header validation rules for names and values
5. return buffered tail bytes separately

Tests:

- `connect_proxy_407_with_body_in_same_read_is_parsed`
- malformed CONNECT status/header tests stay green

Done when:

- CONNECT parsing no longer assumes the full read buffer is UTF-8

## RR-005 Prefetched CONNECT Tail Wrapper

Status: `DONE`
Depends on: `RR-004`

Objective:

- preserve bytes read past the CONNECT header block on successful tunnel setup

Files:

- `crates/openwire/src/transport.rs`

Implementation steps:

1. add `PrefetchedConnection`
2. implement `hyper::rt::Read` so prefetched bytes drain before the inner stream
3. delegate `hyper::rt::Write` and `connected()` to the inner connection
4. wrap successful CONNECT streams when `buffered_tail` is not empty

Tests:

- `connect_proxy_200_with_prefetched_tail_preserves_tunnel_bytes`

Done when:

- successful CONNECT can replay tail bytes into the next consumer of the stream

## RR-006 Fresh Connection Publication Ordering

Status: `DONE`
Depends on: `RR-001`

Objective:

- make a connection pool-visible only after its binding exists

Files:

- `crates/openwire/src/transport.rs`

Implementation steps:

1. reorder `bind_fresh_connection()` so bind happens before pool insert
2. publish the binding before pool visibility
3. acquire the binding for the current request from the fresh bind path itself
4. remove the stale “freshly bound connection was not available” branch if it
   becomes unreachable after the reorder
5. ensure bind failure leaves no pool entry and no orphaned binding

Tests:

- `fresh_http2_connection_is_not_visible_before_binding`
- `http2_bind_failure_leaves_no_pool_entry`

Done when:

- no concurrent request can observe an h2 pool entry without a usable binding

## RR-007 HTTP/2 Body-Abandon Semantics

Status: `DONE`
Depends on: `RR-006`

Objective:

- treat dropped h2 response bodies as stream-local cancellation, not session
  corruption

Files:

- `crates/openwire/src/transport.rs`

Implementation steps:

1. split the HTTP/2 drop path from the HTTP/2 error path
2. keep current discard behavior for HTTP/1.1
3. make `finish_abandoned()` release an h2 allocation without `mark_unhealthy()`
4. keep `finish_with_error()` as the connection-level unhealthy path for h2

Tests:

- `http2_redirect_does_not_poison_connection`
- `http2_auth_followup_does_not_poison_connection`
- `dropping_http2_body_releases_stream_without_discarding_session`

Done when:

- normal redirect/auth follow-ups on h2 do not destroy the shared session

## RR-008 Unhealthy Pool Pruning

Status: `DONE`
Depends on: `RR-007`

Objective:

- ensure unhealthy connections do not persist as dead pool entries

Files:

- `crates/openwire/src/connection/real_connection.rs`
- `crates/openwire/src/connection/pool.rs`

Implementation steps:

1. add `RealConnection::is_healthy()`
2. update `prune_connections()` to remove non-healthy entries
3. ensure stats do not count unhealthy idle entries as reusable capacity

Tests:

- add or extend a pool-level test showing unhealthy entries disappear on the
  next pool touch point

Done when:

- pool touch points cannot keep unhealthy entries around indefinitely

## RR-009 `retry_canceled_requests` Wiring

Status: `DONE`
Depends on: `RR-006`

Objective:

- move canceled-request retries into retry policy and make the flag effective

Files:

- `crates/openwire/src/client.rs`
- `crates/openwire/src/policy/follow_up.rs`

Implementation steps:

1. move `retry_canceled_requests` from `TransportConfig` to `RetryPolicyConfig`
2. keep the public builder method name unchanged
3. change `retry_reason()` to accept retry config
4. classify `WireErrorKind::Canceled` as retryable only when the flag is on

Tests:

- `canceled_requests_are_not_retried_by_default`
- `canceled_requests_are_retried_when_enabled`

Done when:

- enabling the public knob changes behavior

## RR-010 Runtime-Based Request Timeout Helper

Status: `DONE`
Depends on: none

Objective:

- remove direct Tokio timeout usage from `Call::execute()`

Files:

- `crates/openwire/src/client.rs`

Implementation steps:

1. add `with_call_deadline(...)`
2. implement it with `Runtime::sleep(Duration)` and a future race
3. replace `tokio::time::timeout` in `Call::execute()`
4. keep error kind and message shape compatible with current timeout behavior

Tests:

- `call_timeout_uses_runtime_sleep_not_tokio_timeout`
- existing request-timeout test still passes

Done when:

- `call_timeout` no longer directly depends on Tokio in the request path

## RR-011 Body-Stage Deadline Enforcement

Status: `DONE`
Depends on: `RR-007`, `RR-010`

Objective:

- apply the same logical call deadline to response-body polling

Files:

- `crates/openwire/src/transport.rs`

Implementation steps:

1. add `runtime`, `deadline_sleep`, and deadline bookkeeping to
   `ObservedIncomingBody`
2. initialize the deadline future from `ctx.deadline()`
3. poll the deadline future before polling the inner body
4. on deadline fire, synthesize `WireError::timeout(...)`
5. route timeout through `finish_with_error(...)`
6. preserve current event semantics:
   - `call_end` still means headers were returned
   - body timeout reports `response_body_failed(timeout)`

Tests:

- `call_timeout_can_fail_during_body_read`
- body timeout must not emit `call_failed`

Done when:

- body reads respect the same call deadline as the request future

## RR-012 Final Docs Sync and Closeout

Status: `DONE`
Depends on: `RR-001` through `RR-011`

Objective:

- bring user-facing docs back in sync after all remediation code lands

Files:

- `docs/DESIGN.md`
- `README.md`
- `docs/REVIEW_REMEDIATION_DESIGN.md`
- `docs/REVIEW_REMEDIATION_TASKS.md`

Implementation steps:

1. update `docs/DESIGN.md` from “planned” to “implemented” behavior
2. remove any stale caveats that no longer apply
3. keep `README.md` doc references current
4. mark completed task rows as `DONE`

Verification:

- run the full workspace test suite
- ensure every test named in this task file exists and passes
- confirm the design doc no longer describes dead config or incorrect behavior

Done when:

- docs describe the landed implementation rather than the pre-fix state

## 5. Completion Gate

This execution tracker is complete only when:

- every task above is marked `DONE`
- each task’s named tests exist in-tree
- the final workspace test run is green
- `docs/DESIGN.md` no longer conflicts with the landed code

Completion status:

- all tasks above are now marked `DONE`
- named regression tests were added for redirect header policy, CONNECT parsing,
  HTTP/2 abandonment, publication failure, canceled retries, and call-timeout
  behavior
- `cargo test --workspace --all-targets` passed on this branch
