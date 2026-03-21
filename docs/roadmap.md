# OpenWire Roadmap

This document tracks the pending development stages for OpenWire after the current baseline.

It is intended to stay small and operational:

- keep priorities visible
- define what "done" means for the active stages
- prevent feature creep inside transport and policy work
- keep completed work out of the "next" section

## Current Baseline

OpenWire already has:

- `Client` / `ClientBuilder` / one-shot `Call`
- application and network interceptors
- `CallContext` and `EventListener`
- pluggable DNS / TCP / TLS traits
- shared `hyper-util` connection pooling
- built-in request normalization via the internal `BridgeInterceptor`
- minimal safe retries for replayable requests on connection-establishment failures
- redirect handling with retry counting tracked separately from redirect counting
- basic response body observability
- Rustls-based default TLS integration

The next work should build on this base instead of reopening transport internals unless required.

## Recently Completed

- 2026-03-20: internal `BridgeInterceptor` merged with integration coverage for empty, fixed-size, and streaming request bodies
- 2026-03-20: minimal safe retry policy merged with integration coverage for retried success, retry exhaustion, non-replayable bodies, and retry/redirect count separation
- 2026-03-21: request API boundary verified in the public examples and README around `http::Request<RequestBody>` plus `client.execute(request)`
- 2026-03-21: observability stabilization verified with stable attempt span fields, response-body failure handling, and event-ordering regression coverage

## Priority Rules

Priority is defined as:

- P1: foundational and blocks later API work
- P2: important API ergonomics and observability
- P3: feature expansion after core semantics stabilize

Features below are accepted into the roadmap.

## External API Boundary

OpenWire's public API should keep request-message construction separate from
client execution and transport policy.

### Rules

- `http::Request<RequestBody>` is the public request container
- request message construction uses the standard `http::Request::builder()` API
- `Client::execute(request)` and `Client::new_call(request)` are the only send entry points
- `ClientBuilder` owns execution defaults and transport/policy configuration:
  - interceptors and event listeners
  - runtime and connector hooks
  - DNS / TCP / TLS integration
  - pooling, retry, redirect, and default timeouts
- requests own only request-scoped data:
  - method, URI, version, headers, and body
  - typed request metadata via `http::Extensions` when callers or interceptors
    need correlation data
- do not introduce a parallel OpenWire request builder or client-bound
  `send()` path unless the standard `http` request model becomes insufficient
- do not expose request-scoped timeout/auth helper APIs until their semantics are
  explicitly designed across retries, redirects, and pooled connections

### Canonical User Flow

1. `ClientBuilder -> Client`
2. `http::Request::builder() -> Request<RequestBody>`
3. `client.execute(request)` or `client.new_call(request).execute()`
4. interceptor chain -> transport -> connector stack -> hyper

## P2: Request API Boundary

### Goal

Stabilize the public request API around the standard `http::Request<RequestBody>`
model before more user-facing features are added.

### Scope

- use `http::Request::builder()` as the primary request-construction path
- keep `ClientBuilder` as the place for default transport and policy settings
- document `http::Extensions` as the request-scoped metadata mechanism
- align README and examples with the standard request/execution flow

### Out of Scope

- a custom OpenWire `RequestBuilder`
- `client.get(...)`, `client.post(...)`, or request-bound `send()`
- request-scoped timeout/auth helper surface in this phase
- JSON serialization helpers in the same phase

### Execution Plan

1. Remove the custom OpenWire request-builder API from the public surface.
2. Restore examples and README to the standard `Request::builder()` +
   `client.execute(request)` flow.
3. Keep timeout, retry, redirect, connector, and pool configuration documented
   on `ClientBuilder`.
4. Preserve request-scoped metadata via standard `http::Extensions`.

### Acceptance Criteria

- public examples use `Request::builder()` and `client.execute(request)`
- no public OpenWire request builder remains
- docs clearly separate client-level configuration from request message data
- request-scoped metadata guidance uses `http::Extensions`

## P2: Observability Stabilization

### Goal

Make response-body failures, connection reuse, and request-attempt data consistently visible in both events and tracing.

### Current Gap

OpenWire already emits `response_body_failed`, exposes connection reuse in `connection_acquired`, and attaches attempt numbers to attempt spans. The remaining work is to stabilize those fields, tighten event ordering guarantees, and add explicit regression coverage.

### Scope

- keep `response_body_failed` as a first-class event
- expose reused-connection metadata consistently in tracing as well as events
- keep retry and redirect attempt metadata easy to correlate in spans
- make tracing field names stable and predictable

### Out of Scope

- full OpenTelemetry exporter integration
- metrics backend abstraction

### Execution Plan

1. Audit current tracing spans and promote ad hoc fields into a stable schema.
2. Decide and document the canonical field set for `call_id`, `attempt`, `connection_id`, `connection_reused`, and retry/redirect context.
3. Add event-ordering tests for success, response-body failure, and retry paths.
4. Ensure body read failures never duplicate the terminal call outcome.

### Acceptance Criteria

- tracing spans include `call_id`, `attempt`, and reuse-relevant connection data where available
- body read failures do not duplicate call terminal events
- tests validate event ordering for success, response-body failure, and retry attempts

## P3: CookieJar, Authenticator, Cache

### Goal

Add higher-level HTTP client behavior only after the request API boundary and
observability stabilization are done.

Detailed implementation planning for this phase lives in
[docs/implementation-plan.md](implementation-plan.md).

### Active Start Point

The first work inside P3 should be `Core Semantics Hardening` before any new
public feature lands.

- treat the core request path as two linked foundations:
  transport core plus policy core
- prefer mature, widely used open-source crates as default subsystem
  implementations where possible, while keeping OpenWire's API and policy model
  owned in-house behind trait boundaries
- finish clarifying retry, redirect, follow-up, request snapshot, and response
  lifecycle semantics before adding higher-level features
- keep cookies, auth, and cache in policy/interceptor code instead of
  transport code

### Default Implementation Principle

OpenWire should not try to win by re-implementing commodity infrastructure.

- adopt proven defaults for protocol, middleware, TLS, and similar subsystems
- wrap those defaults behind OpenWire-owned traits and orchestration layers
- spend implementation effort on API quality, observability, control surfaces,
  extension points, and behavior contracts

For each future capability, do a short build-vs-adopt decision pass before
implementation starts.

### Current Design Constraint

Current retry and redirect follow-up requests are rebuilt from snapshots that
preserve method, URI, version, headers, and replayable bodies, but they do not
preserve arbitrary `http::Extensions`.

Implication:

- P3 should not assume automatic cloning of arbitrary request metadata across
  retries, redirects, or future auth follow-ups
- internal policy state for cookies/auth/cache should live in policy-owned
  state, not transport
- any broader request-metadata survivability promise must be designed
  explicitly instead of being implied by the current snapshot logic

### Phase Split

#### P3.0: Core Semantics Hardening

- extract retry and redirect services into `openwire::policy::*` internals
- establish a single follow-up coordinator for retry, redirect, and future
  authenticator flows
- add a subsystem decision matrix for later features so defaults can come from
  proven crates without leaking those choices into the public API
- define the internal request snapshot / response-head / follow-up decision
  model used by retry, redirect, and authenticator flows
- keep tracing field names stable while allowing new optional policy counters
- preserve the canonical execution chain:
  user API -> `Client::execute` -> call context -> event listener ->
  interceptor/policy chain -> transport -> connector stack -> hyper

#### P3.1: CookieJar

- define a public `CookieJar` trait
- load request cookies before each network attempt
- persist `Set-Cookie` headers after response headers are received
- keep cookie matching and storage out of transport and out of the bridge
  normalization code

#### P3.2: Authenticator

- define a public `Authenticator` trait for follow-up request generation
- handle origin auth in policy code and reserve the same coordinator for proxy
  auth when proxy support expands
- require replayable request bodies for automatic auth follow-ups
- keep auth loop limits independent from retry and redirect limits

#### P3.3: `openwire-cache`

- introduce a dedicated `openwire-cache` crate instead of growing cache logic
  inside `openwire-core` or transport
- implement cache lookup, conditional revalidation, and invalidation as
  interceptor behavior
- scope the first cache milestone to RFC-aligned GET/HEAD behavior before
  broader optimizations

### Acceptance Gate For Starting P3

Do not begin this phase until:

- request API boundary is considered stable
- observability fields are considered stable enough for downstream tooling

This gate is now considered satisfied by the current merged code and test
coverage. The next stage should start with `P3.0: Core Semantics Hardening`,
not with transport expansion and not with higher-level HTTP features first.

## Deferred For Now

These ideas are valuable but intentionally not accepted into the near-term roadmap yet:

- HTTP/3 transport crate
- WebSocket support
- serde convenience crate
- multipart crate
- general middleware crate split
- full OpenTelemetry integration

Reason: they expand the surface area faster than the current foundation needs.

## Maintenance Rules

When updating this document:

- update status only when code is merged, not when work starts
- keep accepted scope and out-of-scope sections explicit
- record blockers in the relevant phase instead of scattering them across issues
- prefer additive edits over rewriting the whole roadmap each time

Definition of done for a roadmap item:

- implementation merged
- tests added for the promised behavior
- docs/examples updated when the public API changed
- item removed from the "next" section or moved into "Recently Completed"

## Suggested Execution Order

1. Core semantics hardening
2. CookieJar
3. Authenticator
4. `openwire-cache`
