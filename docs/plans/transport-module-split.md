# Transport Module Split Plan

Date: 2026-03-24
Branch: `refactor/transport-module-split`
Status: proposed

## Scope

- split `crates/openwire/src/transport.rs` into cohesive sibling modules
- keep behavior unchanged while reducing file size, review scope, and local
  cognitive load
- preserve the current execution chain, ownership model, and verification
  baseline
- make later protocol work safer by separating unrelated transport concerns

## Non-goals

- no WebSocket, SSE, decompression, or other feature work in this branch
- no retry / redirect / auth / cookie policy changes
- no pool behavior changes
- no request / response semantic changes
- no opportunistic transport cleanup beyond what is required for the split

## Why This Refactor Exists

`crates/openwire/src/transport.rs` is currently about 3700 lines. It mixes:

- route-plan connection setup, CONNECT tunneling, and SOCKS5 handshake logic
- binding registry and background connection-task ownership
- `TransportService` connection acquisition and fresh binding flow
- response-body lifecycle, release bookkeeping, and deadline handling
- protocol binding, request preparation, and Hyper error mapping
- roughly 900 lines of transport-local unit tests

That shape makes feature work riskier because unrelated concerns live in one
file, private helper movement is expensive to reason about, and regressions in
RAII cleanup or protocol binding are hard to isolate in review.

## Current External Surface

Today the rest of the crate depends on only two transport entry points:

- `ConnectorStack`
- `TransportService`

`client.rs` constructs both directly. The refactor should preserve that
top-level surface by re-exporting from `transport/mod.rs`, even if the concrete
definitions move to child modules.

## Locked Decisions

- this is a no-behavior-change refactor
- keep the canonical execution chain intact
- keep response-body-driven release ownership as the source of truth
- prefer `pub(super)` over widening visibility
- move inline unit tests out of the root module early so production movement is
  easier to review
- do not introduce a generic `utils.rs` catch-all module

## Canonical Execution Chain To Preserve

```text
User API
  -> Client::execute
    -> create CallContext
    -> create EventListener
    -> application interceptors
      -> follow-up coordinator
        -> request validation
        -> cookie request application
        -> bridge normalization
          -> network interceptors
            -> transport service
              -> connector stack
                -> proxy selection
                -> dns
                -> tcp
                -> tls
                -> hyper conn/client
        -> cookie response persistence
        -> auth / redirect follow-up decision
    -> response body wrapper
      -> connection release bookkeeping
```

## Target Module Layout

- `crates/openwire/src/transport/mod.rs`
  module declarations, narrow re-exports, transport-local shared imports only
  if unavoidable
- `crates/openwire/src/transport/connect.rs`
  route-plan connection setup, CONNECT / SOCKS tunnel logic, timeout helpers,
  `ConnectorStack`
- `crates/openwire/src/transport/bindings.rs`
  `ConnectionBindings`, acquired binding types, connection-task registry,
  acquired-connection cleanup before body publication
- `crates/openwire/src/transport/body.rs`
  `BoundResponse`, `ResponseLease`, `ObservedIncomingBody`, body deadlines,
  response-body lifecycle release helpers
- `crates/openwire/src/transport/protocol.rs`
  HTTP/1 and HTTP/2 binding, request preparation, Hyper error mapping,
  connection-info extraction
- `crates/openwire/src/transport/service.rs`
  `TransportService`, `SelectedConnection`, acquisition / publication flow,
  attempt tracing, bound-request send path
- `crates/openwire/src/transport/tests.rs`
  transport-local unit tests and fixtures

## Dependency Direction

The split should keep a one-way dependency shape:

- `connect.rs`
  depends only on crate-level transport inputs such as auth, connection
  planning, DNS / TCP / TLS traits, and timer / executor types
- `bindings.rs`
  depends on protocol sender types plus crate-level connection types needed for
  acquired-binding cleanup; it should not depend on other transport siblings
- `protocol.rs`
  depends on crate-level connection types and may depend on no sibling module
  other than shared type imports
- `body.rs`
  may depend on `bindings.rs`, but not on `connect.rs` or `service.rs`
- `service.rs`
  is the orchestration layer and may depend on `connect.rs`, `bindings.rs`,
  `body.rs`, and `protocol.rs`
- `tests.rs`
  may depend on all sibling modules
- `mod.rs`
  should not become an orchestration sink again after the split

This is the key guardrail that avoids recreating a single giant file across
multiple modules.

## Symbol Allocation Plan

### `connect.rs`

- `ConnectorStack`
- `ProxyConnectDeps`
- `ConnectTunnelParams`
- `ConnectBudget`
- `IoCompat`
- `connect_route_plan`
- `connect_direct`
- `record_fast_fallback_trace`
- `connect_via_http_forward_proxy`
- `connect_via_http_proxy`
- `connect_via_socks_proxy`
- `connect_target_tls_if_needed`
- `establish_connect_tunnel`
- `establish_socks5_tunnel`
- `ConnectTunnelOutcome`
- `ConnectResponseHead`
- `PrefetchedTunnelBytes`
- `ProxiedConnection`
- `send_connect_request`
- CONNECT parsing helpers
- SOCKS5 helpers
- timeout helpers shared only by connection-establishment code

### `bindings.rs`

- `ConnectionBindings`
- `ConnectionBinding`
- `Http1Binding`
- `Http2Binding`
- `AcquiredBinding`
- `BindingAcquireResult`
- `ConnectionTaskRegistry`
- `ConnectionTaskRegistryInner`
- `release_acquired_connection`

### `body.rs`

- `BoundResponse`
- `ResponseLease`
- `ResponseLeaseShared`
- `ResponseLeaseState`
- `ObservedIncomingBody`
- `BodyDeadlineSignal`
- `abandon_response_lease`
- `release_response_lease`
- `discard_response_lease`
- `abandon_response_lease_state`
- `spawn_body_deadline_signal`

### `protocol.rs`

- `bind_http1`
- `bind_http2`
- `determine_protocol`
- `prepare_bound_request`
- `http1_exchange_allows_reuse`
- `connection_header_requests_close`
- `connection_header_value_requests_close`
- HTTP token parsing helpers
- `map_hyper_error`
- `find_wire_error`
- `connection_info_from_connected`
- `coalescing_info_from_connected`

### `service.rs`

- `TransportService`
- `SelectedConnection`
- `attempt_span`
- `record_pool_lookup_trace`
- `cleanup_failed_request`
- `send_bound_request`

### `mod.rs`

- `pub(crate) use connect::ConnectorStack;`
- `pub(crate) use service::TransportService;`
- sibling `mod` declarations
- `#[cfg(test)] mod tests;`

## Current Rough Boundaries

- connection and tunnel setup, timeout helpers, and proxy IO wrappers:
  lines ~62-1316
  target: `connect.rs`
- binding registry:
  lines ~1318-1458
  target: `bindings.rs`
- task registry:
  lines ~1470-1516
  target: `bindings.rs`
- selected-connection pre-body ownership:
  lines ~1518-1592
  target: `service.rs`
- bound-response and response-lease types:
  lines ~1594-1679
  target: `body.rs`
- `TransportService` orchestration:
  lines ~1688-2141
  target: `service.rs`
- observed body lifecycle types and response-body release helpers:
  lines ~2142-2259 and ~2291-2490
  target: `body.rs`
- acquired-connection cleanup before body publication:
  lines ~2261-2289
  target: `bindings.rs`
- bound send path, protocol binding helpers, request preparation, and Hyper
  error mapping:
  lines ~2492-2777
  eventual target: split between `service.rs` and `protocol.rs`
- inline unit tests:
  lines ~2780-3700
  target: `tests.rs`

## Full Migration Sequence

### Phase 0 — Baseline Capture

- confirm the branch is transport-only
- capture current file size and rough section boundaries
- capture the current verification baseline before code movement

Expected code change:

- none

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`
- `cargo test -q -p openwire --test performance_baseline -- --nocapture`

### Phase 1 — Convert To Directory Module

- move `crates/openwire/src/transport.rs` to
  `crates/openwire/src/transport/mod.rs`
- keep file contents otherwise identical

Expected code change:

- file move only

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`

### Phase 2 — Extract Inline Tests

- move the current `#[cfg(test)] mod tests` block into
  `crates/openwire/src/transport/tests.rs`
- keep helper names and test coverage unchanged

Expected code change:

- production code unchanged
- test-only import and path adjustments

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`

### Phase 3 — Extract Connection Establishment Layer

- create `connect.rs`
- move `ConnectorStack`, route-plan dialing, proxy tunnel setup, CONNECT /
  SOCKS helpers, and timeout utilities
- keep `ConnectorStack` re-exported from `mod.rs` so `client.rs` does not
  change shape

Expected code change:

- large cohesive helper move out of `mod.rs`
- `tests.rs` import path updates expected for moved connect symbols

High-risk area:

- proxy auth retry and shared timeout budget paths

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`
- `cargo test -q -p openwire --test integration -- --nocapture`

### Phase 4 — Extract Bindings Layer

- create `bindings.rs`
- move `ConnectionBindings`, binding acquire state, and connection-task registry
- move `release_acquired_connection` alongside `AcquiredBinding` and
  `ConnectionBindings` because it is pre-body cleanup owned by
  `SelectedConnection::Drop`, not by response-body lifecycle
- update production code to import from `super::bindings`

Expected code change:

- type moves and imports only
- `tests.rs` import path updates expected for moved binding symbols

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`
- review diffs around binding acquire / release call sites

### Phase 5 — Extract Body Lifecycle Layer

- create `body.rs`
- move response lease state, observed body wrapper, deadline signal, and
  response-body lifecycle release helpers
- keep `ResponseBody::new(ObservedIncomingBody::wrap(...))` flow unchanged

Expected code change:

- type / helper moves only
- `tests.rs` import path updates expected for moved body symbols

High-risk area:

- RAII release semantics on body success, body failure, and body drop

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`
- `cargo test -q -p openwire --test integration -- --nocapture`

### Phase 6 — Extract Protocol Layer

- create `protocol.rs`
- move HTTP/1 / HTTP/2 bind helpers, request normalization for bound sends,
  Hyper error mapping, and connection metadata extraction
- leave `cleanup_failed_request` in `mod.rs` during this phase because it
  depends on `ConnectionBindings` and is only called from `send_bound_request`;
  move it into `service.rs` in Phase 7
- keep protocol selection logic unchanged

Expected code change:

- helper moves only
- `tests.rs` import path updates expected for moved protocol symbols

High-risk area:

- stale imports around `RequestBody`, `ConnectionInfo`, and Hyper sender types

Verification:

- `cargo test -q -p openwire transport --lib -- --nocapture`
- `cargo test -q -p openwire --test performance_baseline -- --nocapture`

### Phase 7 — Reduce Root To Wiring And Orchestration

- create `service.rs`
- move `TransportService`, `SelectedConnection`, attempt tracing, and
  `send_bound_request`
- keep `cleanup_failed_request` here because it couples send-path failure
  handling with `ConnectionBindings`, `ExchangeFinder`, and
  `ConnectionAvailability`
- reduce `mod.rs` to re-exports and module declarations

Expected code change:

- orchestration code move only
- `tests.rs` import path updates expected for moved service symbols

Done target for this phase:

- `mod.rs` should be short and stop owning transport implementation details

Verification:

- `cargo test -q -p openwire`

### Phase 8 — Docs Sync And Final Review

- update `docs/DESIGN.md` file references that still point at
  `crates/openwire/src/transport.rs`
- confirm no stale references remain in planning docs or redesign notes

Verification:

- `rg -n "crates/openwire/src/transport.rs|transport/mod.rs" docs`
- `cargo test -q -p openwire`

## Refactor Rules

- preserve existing private helper behavior during each move
- avoid semantic cleanup in the same patch that moves code
- keep imports explicit rather than creating a shared helper bag
- if a helper creates circular dependencies, move the smaller helper instead of
  widening visibility
- if a step causes behavior drift, stop and re-plan before continuing

## Review Strategy

- prefer one commit per phase, or at least one commit per extracted sibling
  module
- reviewers should be able to compare old and new symbol locations directly
- diff shape should be dominated by moves, imports, and path adjustments
- after Phase 2, expect `tests.rs` to keep changing in later phases as a
  mechanical continuation of the initial extraction; reviewers should treat
  those import-path edits separately from production logic moves
- semantic edits should be isolated and justified if they become unavoidable

## Risk Register

- `SelectedConnection`, `ResponseLease`, and `AcquiredBinding` currently span
  acquisition and body-lifecycle concerns; careless movement can create cyclic
  sibling dependencies
- during Phase 6, `cleanup_failed_request` must not be pulled into
  `protocol.rs`; reviewers should treat that as a dependency-direction
  regression because the function couples send-path failure handling to
  `ConnectionBindings`, `ExchangeFinder`, and `ConnectionAvailability`
- during Phases 4-5, `release_acquired_connection` must stay with the
  acquired-binding layer; reviewers should treat movement into `body.rs` as a
  boundary regression because it is pre-body cleanup, not response-body
  lifecycle cleanup
- body-drop and timeout behavior can regress silently if RAII helpers move with
  semantic edits
- proxy tunnel helpers have many tightly-coupled private parsing functions;
  partial extraction increases churn if not moved as a cohesive block
- test fixtures currently rely on direct `super::...` access and will need
  careful path updates when tests move out

## Stop Conditions

- if extraction requires broad `pub(crate)` visibility for many helpers, pause
  and re-evaluate the module boundaries
- if transport-local tests fail after a supposedly mechanical move, treat that
  as a behavior regression until proven otherwise
- if `mod.rs` starts accumulating shared helper logic again, stop and move that
  logic into the correct leaf module

## Verification Matrix

- every phase: `cargo test -q -p openwire transport --lib -- --nocapture`
- after phases touching HTTP/2 binding or sender reuse:
  `cargo test -q -p openwire --test performance_baseline -- --nocapture`
- after phases touching CONNECT / SOCKS / timeout code:
  `cargo test -q -p openwire --test integration -- --nocapture`
- final gate: `cargo test -q -p openwire`

## Definition Of Done

- `crates/openwire/src/transport/mod.rs` is only wiring and re-exports
- the transport implementation is split across cohesive child modules
- no transport behavior change is introduced
- the full `openwire` test suite passes
- docs are updated where file paths became stale
