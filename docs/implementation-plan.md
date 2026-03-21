# OpenWire Implementation Plan

This document turns the accepted roadmap into an execution plan for the next
stage and records the architectural decisions that should constrain future
changes.

It is intentionally implementation-oriented:

- start from the code that already exists
- keep the canonical execution chain intact
- add higher-level HTTP behavior in policy code, not transport code
- make later crate splits deliberate instead of accidental

## Product Direction

OpenWire should be built as a production-oriented HTTP client library that:

- feels coherent and predictable to use, with an API direction aligned with
  OkHttp's mental model
- is observable, controllable, and debuggable in production
- is extensible and customizable without forcing forks
- prefers stable architecture and long-term operability over feature count

The main product value is not "re-implement every HTTP subsystem in-house".
The main product value is to assemble proven building blocks into a cleaner,
more controllable, and more extensible client architecture.

## Build vs Adopt

OpenWire should follow a strict "build the architecture, adopt the commodity
infrastructure" rule.

### Adopt By Default

When a widely used, production-proven crate already solves a subsystem well,
OpenWire should adopt it as the default implementation instead of rebuilding it.

Examples for the current architecture:

- protocol and connection engine: `hyper`
- middleware composition and service orchestration: `tower`
- default TLS implementation: `rustls`
- future cookie storage/matching default: an established cookie-store crate,
  wrapped behind OpenWire traits instead of leaked into the public API

### Build In-House

OpenWire should build and own the parts that define its product identity:

- public client API shape
- call lifecycle model
- policy orchestration for retry / redirect / auth / cookies / cache
- observability model and event ordering
- extension points, trait boundaries, and default component wiring
- production-focused ergonomics and predictable behavior contracts

### Rule For New Subsystems

For each new capability, the first question should be:

1. what part is commodity infrastructure that should be adopted?
2. what part is OpenWire-specific orchestration that should be owned?
3. what trait boundary keeps the default implementation swappable?

Do not re-implement commodity protocol mechanics unless one of these is true:

- the ecosystem option is not reliable enough
- the ecosystem option cannot meet OpenWire's portability or control needs
- the trait boundary would become worse by adopting it

## Core Definition

OpenWire should treat the "core network chain" as two linked layers, not just
as transport internals:

- `transport core`: DNS, TCP, TLS, hyper connection management, pooling,
  timeout behavior, and response-body lifecycle
- `policy core`: request validation, request normalization boundaries, retry,
  redirect, follow-up generation, request snapshot semantics, and tracing/event
  ordering

The implementation route should harden these core semantics first. Higher-level
HTTP features should be added only after both layers are clear enough that they
do not force transport or public API rewrites.

## Current State

The current repository already has the following foundations in place:

- `http::Request<RequestBody>` as the public request container
- `ClientBuilder` for runtime, connector, timeout, retry, redirect, and pool
  configuration
- application interceptors, network interceptors, and the internal
  `BridgeInterceptor`
- request-attempt tracing with stable `call_id`, `attempt`, `retry_count`,
  `redirect_count`, `connection_id`, and `connection_reused` fields
- response-body success/failure events with explicit regression coverage

Verification baseline:

- `cargo check --workspace --all-targets`
- `cargo test -p openwire --tests`

## Architecture Rules

### Canonical Execution Chain

The project should keep treating the following as the source-of-truth request
path:

1. User API
2. `Client::execute`
3. create `CallContext`
4. create `EventListener`
5. interceptor / policy chain
6. transport service
7. connector stack
8. DNS
9. TCP
10. TLS
11. hyper client / connection
12. response body wrapper

No next-stage feature should bypass this chain.

### Layering Boundaries

- `openwire-core` should stay focused on low-level shared primitives:
  request/response bodies, errors, events, runtime, interceptor abstraction,
  transport traits, and connection metadata.
- `openwire` should own the public client API plus built-in HTTP policy logic.
- `openwire-rustls` should remain an optional TLS implementation crate.
- `openwire-cache` should be the first dedicated higher-level behavior crate
  added on top of `openwire`.

Do not move cookies, auth, redirect policy, retry policy, or cache semantics
into transport connectors.

### Public API Constraints

- Keep `http::Request<RequestBody>` as the request-construction model.
- Keep `Client::execute(request)` and `Client::new_call(request)` as the only
  send entry points.
- Do not add a parallel OpenWire request builder.
- Do not add convenience `get/post/send` APIs while policy semantics are still
  being finalized.

## Known Constraint

Current retry and redirect follow-up requests are rebuilt from the internal
`RequestSnapshot` logic in `crates/openwire/src/policy/follow_up.rs`.
Those snapshots preserve:

- method
- URI
- HTTP version
- headers
- replayable request bodies
- internal policy trace fields

They do not preserve arbitrary `http::Extensions`.

That means the next stage must not rely on implicit cloning of arbitrary
request-scoped metadata across retries, redirects, or future auth follow-ups.
If broader metadata survivability is required later, it needs an explicit
design instead of an accidental promise.

## Next Stage

The current branch has already landed the first wave of policy-surface work:

- core semantics hardening
- user-facing `CookieJar`
- user-facing `Authenticator`
- baseline proxy support, including ordered rules, `NoProxy`, HTTP forwarding,
  HTTPS `CONNECT`, `proxy_authenticator` for both response-path and tunnel auth,
  and opt-in environment proxy loading

The next stage is now:

1. harden the remaining proxy gaps:
   optional SOCKS and selector parity
2. begin `openwire-cache`

Before feature implementation starts for each major subsystem, OpenWire should
also do a short build-vs-adopt survey so the repository does not drift into
accidental re-implementation work.

## Target Policy Design

### Internal Module Layout

The built-in policy stack should move out of the monolithic `client.rs`
implementation and into internal modules with bounded responsibilities:

- `openwire/src/policy/mod.rs`
- `openwire/src/policy/follow_up.rs`
- `openwire/src/policy/retry.rs`
- `openwire/src/policy/redirect.rs`
- `openwire/src/policy/cookie.rs`
- `openwire/src/policy/auth.rs`

`client.rs` should keep client assembly and builder wiring, not the detailed
policy state machines.

### Single Follow-Up Coordinator

Retry, redirect, and authenticator behavior should share one internal
coordinator instead of growing as independent ad hoc loops.

The coordinator should own:

- total attempt numbering
- independent retry / redirect / auth counters
- replayability checks for request bodies
- request snapshot reuse
- stable tracing field propagation
- loop termination and max-attempt enforcement

Preferred internal decision model:

- `ReturnResponse`
- `Retry`
- `Redirect`
- `Authenticate`
- `Fail`

This keeps all automatic follow-up decisions in one place and avoids
duplicating control-flow rules across separate services.

### Preferred Stack Order

The long-term built-in stack should converge on this shape:

1. application interceptors
2. cache interceptor layer when `openwire-cache` exists
3. follow-up coordinator
4. cookie request application
5. bridge normalization
6. network interceptors
7. transport

Response handling should flow back through the same policy boundary:

- transport returns response headers/body wrapper
- cookie persistence runs on response headers
- redirect/auth decisions inspect response heads
- final response returns to application interceptors

## Capability Survey Before Expansion

Before implementing each major capability after core semantics hardening, keep
one short decision record with:

- target user-facing behavior
- default crate choice, if any
- trait boundary exposed by OpenWire
- why the dependency is adopted instead of re-implemented
- what remains OpenWire-owned above that dependency

The first survey set should cover at least:

- DNS resolver defaults and alternates
- TLS defaults and alternates
- cookie storage default
- authentication follow-up model
- cache storage and validation model
- proxy support strategy

The working survey table lives in [component-strategy.md](component-strategy.md).
The initial research snapshot lives in [subsystem-survey.md](subsystem-survey.md).
The proposed interface surfaces live in [policy-interface-design.md](policy-interface-design.md).
Detailed contracts live in:

- [cookiejar-contract.md](cookiejar-contract.md)
- [authenticator-contract.md](authenticator-contract.md)
- [proxy-contract.md](proxy-contract.md)
- [cache-contract.md](cache-contract.md)

## Milestone 1: Core Semantics Hardening

Status: implemented on the current branch.

### Goal

Finish clarifying the request execution semantics that sit between the public
API and transport so later HTTP features do not force architectural rewrites.

### Scope

- keep the transport chain stable while tightening the policy core above it
- extract retry and redirect code into internal policy modules
- introduce a single follow-up coordinator for retry, redirect, and future auth
- introduce explicit internal types for request snapshot, response head, and
  follow-up decision
- keep current public API and current behavior unchanged
- keep existing tracing field names unchanged

### Acceptance

- no change to `Client` / `ClientBuilder` public send flow
- retry and redirect tests continue to pass unchanged
- `client.rs` becomes assembly-oriented instead of state-machine-heavy
- new policy modules are clearly separate from transport code
- transport core and policy core responsibilities are easier to explain and
  extend

## Milestone 2: CookieJar

Status: implemented on the current branch.

### Goal

Add cookie support as policy-layer behavior that runs before and after network
attempts.

### Public Surface

Add `ClientBuilder::cookie_jar(...)` and a public `CookieJar` trait in
`openwire`.

Preferred trait direction:

- async-friendly
- request URI based
- storage owned by user code
- OpenWire handles policy wiring, not persistence strategy

The first public API does not need to ship a fully featured built-in persistent
jar. An in-memory example implementation is enough for integration coverage.

### Semantics

- load cookies before each network attempt, not only before the first logical
  call
- save `Set-Cookie` headers as soon as response headers are available
- redirected follow-up requests should see cookies set by earlier responses in
  the same logical call
- cookie behavior should remain independent from bridge normalization and
  transport connector code

### Acceptance

- cookies can be loaded for the first request
- cookies set on a redirect response can affect the follow-up request
- retries do not duplicate cookie persistence incorrectly
- tests cover no-jar, basic jar, and redirect-cookie flows

## Milestone 3: Authenticator

Status: implemented on the current branch.

### Goal

Add policy-layer follow-up generation for authentication challenges without
mixing auth logic into transport.

### Public Surface

Add `ClientBuilder::authenticator(...)` and a public `Authenticator` trait in
`openwire`.

The first authenticator API should focus on straightforward follow-up request
generation:

- inspect the prior response head
- decide whether another request should be attempted
- return either a follow-up request or `None`

### Semantics

- automatic auth follow-ups require replayable request bodies
- auth attempt limits should be separate from retry and redirect limits
- auth should reuse the same coordinator and tracing context as redirect/retry
- origin auth is the first required end-to-end path; the coordinator should
  still reserve space for proxy auth instead of baking in origin-only control
  flow

### Acceptance

- `401` can trigger a replayable authenticated follow-up
- non-replayable request bodies do not silently retry with auth
- auth loops terminate predictably
- redirect, retry, and auth counters remain distinguishable in tests and traces

## Milestone 4: `openwire-cache`

### Goal

Add HTTP caching as a dedicated crate layered above `openwire`.

### Scope

- new crate: `crates/openwire-cache`
- interceptor-based cache lookup and conditional revalidation
- RFC-aligned GET/HEAD behavior first
- invalidation on unsafe methods

### Out of Scope For The First Cache Milestone

- a transport-layer cache
- ad hoc memoization APIs
- offline mode, stale-if-error, or cache persistence tuning knobs beyond the
  minimum needed to prove the architecture

### Acceptance

- cache hit / miss / revalidate flows are covered by integration tests
- `Vary`, `ETag`, and `Last-Modified` semantics are exercised
- the cache crate does not require changes to DNS/TCP/TLS transport internals

## Verification Strategy

Each milestone should prove behavior with integration tests, not only unit
tests.

Required verification themes:

- request normalization still happens before network interceptors
- retry / redirect / auth event ordering stays stable
- response-body failures remain non-duplicated after policy extraction
- cookie persistence interacts correctly with redirects
- cache revalidation preserves tracing and event visibility

The baseline command set should stay:

- `cargo check --workspace --all-targets`
- targeted crate tests for the changed behavior

## Deliberate Non-Goals

The next stage should not expand into:

- HTTP/3
- WebSocket support
- serde convenience helpers
- multipart helpers
- a general middleware crate split
- full OpenTelemetry exporter integration

Those can return after the policy layer is cleaner and the higher-level HTTP
semantics are stable.
