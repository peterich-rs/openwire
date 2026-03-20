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

## Priority Rules

Priority is defined as:

- P1: foundational and blocks later API work
- P2: important API ergonomics and observability
- P3: feature expansion after core semantics stabilize

Features below are accepted into the roadmap.

## P2: RequestBuilder Ergonomics

### Goal

Add a user-facing builder API so callers do not need to construct `http::Request<RequestBody>` manually for common cases.

### Scope

- `client.get(url)`
- `client.post(url)`
- request builder methods for headers, body, timeout, and auth helpers
- `send().await` returning `Response<ResponseBody>`

### Out of Scope

- generated API surface for every HTTP verb on day one
- JSON serialization helpers in the same phase

### Execution Plan

1. Add a minimal `RequestBuilder` type that compiles down into `http::Request<RequestBody>`.
2. Land `client.get(url)` and `client.post(url)` as the first public entry points.
3. Support common body conversions already represented by `RequestBody` (`Bytes`, `Vec<u8>`, `String`, `&'static str`).
4. Add request-local overrides for timeout and basic auth / bearer auth helpers.
5. Update `crates/openwire/examples` and the README once the builder is merged.

### Acceptance Criteria

- common `GET` / `POST` paths no longer require manual `Request::builder()`
- builder supports custom headers and request body conversion
- per-request timeout override works without rebuilding the whole client
- examples in `crates/openwire/examples` include the new builder style

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

Add higher-level HTTP client behavior only after RequestBuilder and observability stabilization are done.

### CookieJar

- define a `CookieJar` trait
- load cookies before request send
- save cookies from response headers after response receive
- wire cookie application into the built-in request normalization/policy stack, not transport

### Authenticator

- define an `Authenticator` trait for follow-up requests
- handle `401` and `407` in policy code, not in transport
- restrict the first implementation to straightforward follow-up generation

### Cache

- defer to a dedicated `openwire-cache` crate
- implement as interceptor-layer behavior, not transport-layer behavior
- follow HTTP caching semantics instead of ad hoc memoization

### Acceptance Gate For Starting P3

Do not begin this phase until:

- RequestBuilder is merged
- observability fields are considered stable enough for downstream tooling

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

1. RequestBuilder
2. Observability stabilization
3. CookieJar
4. Authenticator
5. `openwire-cache`
