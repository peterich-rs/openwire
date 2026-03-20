# OpenWire Roadmap

This document tracks the next development stages for OpenWire after the current v1 foundation.

It is intended to be a long-lived maintenance document:

- keep priorities visible
- define what "done" means for each stage
- prevent feature creep inside transport and policy work
- record which suggestions are explicitly accepted now and which are deferred

## Current Baseline

OpenWire already has:

- `Client` / `ClientBuilder` / one-shot `Call`
- application and network interceptors
- `CallContext` and `EventListener`
- pluggable DNS / TCP / TLS traits
- shared `hyper-util` connection pooling
- redirect handling
- basic response body observability
- Rustls-based default TLS integration

The next work should build on this base instead of reopening the transport core unless required.

## Priority Rules

Priority is defined as:

- P1: foundational and blocks later API work
- P2: important API ergonomics and observability
- P3: feature expansion after core semantics stabilize

Features below are accepted into the roadmap.

## P1: Built-in BridgeInterceptor

### Goal

Introduce a built-in interceptor that normalizes outgoing requests before they reach user-provided network interceptors and transport.

### Scope

- add `Host` when absent
- set `User-Agent: openwire/<version>` when absent
- set `Content-Length` for replayable fixed-size bodies
- set `Transfer-Encoding: chunked` for streaming bodies when appropriate
- keep the behavior internal and deterministic across all requests

### Out of Scope

- automatic gzip / deflate decompression
- cookie handling
- transparent content negotiation

### Design Notes

- place this interceptor inside the built-in stack, not as a public opt-in middleware
- user network interceptors should observe the normalized request
- avoid leaking hyper-specific rules into user API types

### Acceptance Criteria

- requests without `Host` gain a correct header derived from the URI
- fixed-size request bodies emit `Content-Length`
- streaming request bodies do not emit a wrong `Content-Length`
- user-supplied `User-Agent` is preserved
- integration tests cover empty, replayable, and streaming bodies

## P1: Minimal Safe Retry Policy

### Goal

Split retry behavior from redirect behavior and introduce a minimal retry policy focused only on safe transport retries.

### Scope

- retry connection-establishment failures only
- retry only replayable requests
- keep retry count small and explicit
- separate retry count from redirect count in policy state

### Out of Scope

- 408 / 503 retry policies
- backoff / jitter
- retry-after parsing
- application-level retry rules

### Design Notes

- model this as a dedicated policy layer instead of folding more logic into redirect handling
- the retry layer should run before redirect follow-up generation
- retries must emit clear attempt metadata for tracing and event listeners

### Acceptance Criteria

- non-replayable streaming request bodies are not retried
- replayable `GET` / fixed-body requests can retry on connection failure
- redirect counting and retry counting are independently tracked
- tests cover retried success, retry exhaustion, and non-retryable bodies

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

### Design Notes

- keep `http::Request<RequestBody>` as the canonical lower-level API
- `RequestBuilder` should compile down into that canonical form
- avoid hiding `http` types completely; advanced users should still have the low-level path

### Acceptance Criteria

- common `GET` / `POST` paths no longer require manual `Request::builder()`
- builder supports custom headers and request body conversion
- per-request timeout override works without rebuilding the whole client
- examples in `crates/openwire/examples` include the new builder style

## P2: Observability Improvements

### Goal

Make response-body failures, connection reuse, and request attempt data visible in both events and tracing.

### Scope

- keep `response_body_failed` as a first-class event
- expose whether a connection was reused in event callbacks
- include attempt metadata in tracing spans
- make tracing field names stable and predictable

### Out of Scope

- full OpenTelemetry exporter integration
- metrics backend abstraction

### Design Notes

- event ordering should remain internally consistent even if hyper-util limits exact OkHttp parity
- prefer adding explicit fields over encoding metadata in free-form log messages
- keep one terminal call outcome per call path

### Acceptance Criteria

- tracing spans include `call_id`, `attempt`, and reuse-relevant connection data where available
- body read failures do not duplicate call terminal events
- tests validate event ordering for success, response-body failure, and retry attempts

## P3: CookieJar, Authenticator, Cache

### Goal

Add higher-level HTTP client behavior only after BridgeInterceptor, retry, and builder ergonomics are stable.

### CookieJar

- define a `CookieJar` trait
- load cookies before request send
- save cookies from response headers after response receive
- wire into BridgeInterceptor instead of transport

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

- BridgeInterceptor is stable
- retry policy semantics are validated
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
- item removed from the "next" section or marked complete with date/reference

## Suggested Execution Order

1. BridgeInterceptor
2. Minimal safe retry layer
3. RequestBuilder
4. Observability polish
5. CookieJar
6. Authenticator
7. `openwire-cache`
