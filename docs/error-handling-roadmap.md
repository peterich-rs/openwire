# OpenWire Error Handling Roadmap

Date: 2026-03-27
Status: In Progress

This document is the canonical roadmap for error handling in OpenWire. It
defines the target failure contract, the current gaps, and the implementation
phases needed to make the client production-ready without violating the
existing execution layering.

The design constraint is to preserve the execution chain:

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

No future error-handling work should bypass this chain or collapse policy,
extension, and transport concerns into one layer.

## 0. Implementation Snapshot

Implemented on 2026-03-27:

- `WireError` now exposes stable `phase` and limited diagnostic accessors
- `RetryContext` now carries `request_method`
- default retry policy now consumes `kind + phase + request_method`, while
  retaining establishment helpers as compatibility fallback
- high-risk request-path panic sites were replaced with structured
  `WireError::internal`
- source preservation and best-effort observability were improved across
  hyper, tokio, rustls, cookie, and fast-fallback paths
- `cargo test --workspace` is green on the implementation that matches this
  document

Still intentionally deferred:

- attempt-span and event-listener vocabulary are not yet fully aligned with the
  new phase/context model
- strict interceptor/authenticator runtime attribution still requires a separate
  API design if it remains a hard requirement

## 1. Why This Exists

OpenWire needs a failure system that does four things reliably:

- gives callers stable semantics without string parsing
- gives retry and follow-up policy enough structure to make correct decisions
- gives operators enough context to diagnose production failures from the
  surfaced error, not only from tracing backends
- keeps call-execution failures and response-body failures on their existing,
  correct ownership boundary

## 2. Current State

OpenWire currently exposes one public error type, `WireError`, with:

- a top-level `WireErrorKind`
- a stable `FailurePhase`
- a message
- an optional source error
- limited diagnostic accessors (`authority`, `proxy_addr`, `response_status`,
  `request_committed`)
- optional connection-establishment metadata kept mainly as a compatibility
  layer for retry helpers

Current public kinds:

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

Current strengths:

- request-construction and URI/proxy/config validation failures are clearly
  surfaced as `InvalidRequest`
- HTTP status codes are not treated as transport exceptions
- response-body failures surface during body consumption, not from
  `Client::execute`
- event-listener hooks expose both phase-local body failures and final
  call-level termination
- connect-stage retry behavior now uses phase-aware metadata in addition to
  establishment helpers
- response-body timeout, generic pre-response call timeout, and connect timeout
  can now be distinguished through stable accessors instead of string parsing

Current implementation anchors:

- `crates/openwire-core/src/error.rs`
- `crates/openwire-core/src/policy.rs`
- `crates/openwire-core/src/interceptor.rs`
- `crates/openwire-core/src/auth.rs`
- `crates/openwire-core/src/event.rs`
- `crates/openwire/src/client.rs`
- `crates/openwire/src/policy/follow_up.rs`
- `crates/openwire/src/policy/defaults.rs`
- `crates/openwire/src/transport/connect.rs`
- `crates/openwire/src/transport/service.rs`
- `crates/openwire/src/transport/body.rs`
- `crates/openwire/src/transport/protocol.rs`

## 3. Current Gaps

### Gap 1: Phase and diagnostic coverage is still incomplete

The core error model now has `phase`, but not every failure site populates the
full diagnostic surface yet. The main remaining work is consistency, not
whether the model should exist.

### Gap 2: Request commitment is only partially surfaced today

`request_method` is now available to retry policy, and `request_committed` now
exists on the error object, but request commitment is not yet populated at
every failure site where it could help future custom policies.

### Gap 3: Extension attribution is best-effort, not guaranteed

Interceptors and authenticators currently return `WireError` directly.
With the present trait shapes, OpenWire cannot guarantee strict runtime
distinction between:

- user extension failure
- policy-layer failure
- downstream transport failure propagated through an extension

The roadmap must not promise a guarantee that the current API cannot enforce.

### Gap 4: Production hardening is substantially improved, but not fully closed

High-risk request-path panic sites and source-chain inconsistencies were fixed,
but a small number of low-priority mathematical invariants still remain, most
notably the `Content-Length` construction path in `bridge.rs`.

### Gap 6: Observability alignment still needs broader coverage

As of 2026-03-27, the integration test
`retry_and_redirect_events_follow_stable_order_and_trace_fields` is green. The
remaining observability gap is no longer "this exact scenario still fails";
it is broader coverage and consistency:

- stable kind/phase/context vocabulary across errors, events, and traces
- parity for retry, redirect, timeout, body, proxy-tunnel, and TLS failures
- explicit documentation of what is guaranteed on the surfaced error versus
  what remains tracing-only

## 4. Canonical Target Model

OpenWire should keep a single public `WireError` type, but evolve it into a
more structured contract.

Canonical target fields or stable accessors:

- `kind`: stable top-level semantic bucket for callers
- `phase`: stable failure location
- `source`: original lower-level cause
- `context`: a small set of operator-facing diagnostic facts

Retryability is not a standalone public error field. Retry remains a policy
decision derived from:

- the error's stable metadata
- request replayability
- request method
- whether the request was already committed
- attempt count and policy configuration

Recommended phase vocabulary:

- `RequestValidation`
- `Admission`
- `Dns`
- `Tcp`
- `ProxyTunnel`
- `Tls`
- `ProtocolBinding`
- `RequestExchange`
- `ResponseHeaders`
- `ResponseBody`
- `Policy`
- `Interceptor`
- `Internal`

Recommended diagnostic fields or accessors:

- target authority
- proxy endpoint when relevant
- response status when relevant
- whether the current request had already been committed to the wire

Explicit non-goals:

- do not turn every transport detail into a new public `WireErrorKind` variant
- do not store tracing-only identifiers such as call IDs or retry counters on
  the error object
- do not collapse response-body failures into `Client::execute` failures

## 5. Target Handling Rules

### 5.1 Return a response instead of raising an error

These should normally remain regular HTTP outcomes:

- origin `4xx` and `5xx`
- redirect responses when redirect policy says `Stop`
- origin `401` or forward-proxy `407` when no authenticator is configured or
  when the authenticator declines to follow up

These are application-visible protocol results, not transport exceptions.

### 5.2 Recover internally only at explicit recovery points

OpenWire should only recover internally when all of the following are true:

- the failure happened before request commitment
- the request body is replayable
- policy explicitly allows retry
- the failure location and kind are marked as recoverable by policy

That recovery window primarily applies to:

- DNS
- TCP establishment
- proxy tunnel establishment
- retryable TLS handshake failures
- protocol binding during connection setup

### 5.3 Surface as hard errors immediately

These should surface and not be silently downgraded:

- invalid request or invalid client/proxy configuration
- missing required TLS support for an HTTPS request
- non-retryable TLS policy or certificate failures
- extension failures
- policy decision failures
- internal invariants and runtime/executor failures
- exhausted retries after policy says no further recovery is allowed
- CONNECT tunnel failures where no end-to-end HTTP response can be returned

### 5.4 Surface later during body consumption

These should remain body-consumption failures:

- response-body timeout
- truncated or malformed response stream
- text decode failures
- JSON decode failures

Once a response object has been returned, the request execution contract should
not report these through `Client::execute`, even though event listeners may
still observe `response_body_failed` followed by terminal `call_failed`.

### 5.5 Keep side-effect-only facilities best-effort by default

If side channels later become fallible, they should default to best-effort
behavior unless strict mode is explicitly requested:

- cookie persistence
- telemetry export
- non-critical diagnostics sinks

This is a policy rule, not a reason to hide mainline transport failures.

## 6. Key Decisions

1. Keep a single public `WireError` type and keep `WireErrorKind` as the
   top-level semantic contract. Do not explode public kind variants to encode
   every transport location.
2. Model failure location separately through `FailurePhase`. `kind` answers
   "what class of error is this"; `phase` answers "where did it fail".
3. Keep diagnostics on the surfaced error small and stable. Store only
   authority, proxy endpoint, response status, and request commitment. Do not
   copy tracing-only counters into the error object.
4. Keep retryability as a policy decision, not as a public error enum. Policy
   should derive retry from error metadata, replayability, method, and request
   commitment.
5. Do not pretend the current interceptor/authenticator API can provide strict
   runtime attribution. Treat extension attribution as best-effort until a
   separate API redesign exists.
6. Treat request-path panic removal and source preservation as production
   hardening work, not as reasons to redesign the entire error model.

## 7. Hardening Status

Completed on 2026-03-27:

- request-path panic sites in `service.rs`, `follow_up.rs`, `body.rs`,
  `client.rs`, `limits.rs`, `fast_fallback.rs`, and `connect.rs` were replaced
  with structured errors
- `map_hyper_error`, rustls ServerName handling, and tokio connect timeout now
  preserve source chains consistently
- best-effort silent drops in cookie handling, pool reaper startup, and
  fast-fallback event delivery now emit tracing

Remaining:

- `bridge.rs` still contains one low-priority mathematical-invariant `expect`
- production observability is not yet fully aligned across surfaced errors,
  event listeners, and attempt spans
- strict extension attribution remains a separate follow-up design problem

## 8. Execution Plan

### Phase 1: Freeze semantics and correct the docs

Status: Completed on 2026-03-27

Goals:

- align the roadmap, architecture design, and hardening plan
- document the current guarantees and current non-guarantees honestly
- keep the existing top-level public kind contract stable

Deliverables:

- corrected roadmap, architecture, and hardening docs
- explicit notes on extension attribution limitations
- updated verification notes for current observability status

### Phase 2: Harden production paths

Status: Mostly completed on 2026-03-27

Goals:

- remove request-path panics from production code
- eliminate source-chain inconsistencies in error mapping
- stop silently dropping operationally meaningful failures

Deliverables:

- no known request-path panic sites on non-test code paths
- consistent source preservation across hyper/TLS/runtime mappings
- explicit warn/debug tracing for retained best-effort drops

### Phase 3: Introduce stable phase and limited diagnostics

Status: Partially completed on 2026-03-27

Goals:

- add stable phase accessors without exploding public semantic kinds
- attach a small amount of diagnostic context to surfaced errors
- populate phase and diagnostics consistently across validation, transport,
  policy, and body paths

Deliverables:

- `WireError` phase accessors
- limited diagnostic accessors
- tests that cover major failure sources and expected phase semantics

### Phase 4: Expand retry inputs

Status: Partially completed on 2026-03-27

Goals:

- let retry policy inspect stable metadata instead of message strings
- add request method immediately
- add request-commitment signals only when transport code can assert them

Deliverables:

- richer `RetryContext`
- retry-policy tests covering idempotency and request commitment

### Phase 5: Improve extension attribution

Status: Not started beyond documentation and problem framing

Goals:

- make the current best-effort extension contract explicit
- avoid promising strict runtime guarantees that current trait shapes cannot
  provide
- define a separate, semver-conscious redesign only if strict attribution
  remains a hard requirement

Deliverables:

- documented current behavior for interceptor/authenticator failures
- tests for the supported best-effort contract
- follow-up ADR only if a stricter API redesign is still needed

### Phase 6: Harden observability

Status: Remaining

Goals:

- make surfaced errors, event listeners, and traces agree on kind, phase, and
  diagnostic vocabulary
- cover the main failure classes that operators will debug in production

Deliverables:

- green integration coverage for retry, redirect, timeout, body, proxy-tunnel,
  and TLS diagnostics
- stable field naming for dashboards and logs

## 9. Success Criteria

This work is successful when:

- callers can distinguish connect timeout, pre-response call timeout, and body
  timeout through stable accessors rather than message parsing
- default retry decisions are based on stable metadata and request context
- extension attribution is either explicit or clearly documented as
  best-effort; the docs do not over-promise
- surfaced errors plus tracing are sufficient to locate common production
  failures by authority, proxy, phase, and source
- new connectors or runtimes can plug into the same kind/phase/context model
  without redefining the public semantic contract

## 10. Maintenance Rule

This document is the source of truth for the error-handling direction until the
roadmap is implemented. If implementation diverges, update this document in the
same change. This file intentionally replaces the former separate architecture
and hardening planning docs.
