# OpenWire Cache Contract

Date: 2026-03-21

This document defines the intended cache contract for OpenWire and the future
`openwire-cache` crate.

It is informed by:

- OkHttp's cache model, where HTTP caching is a core client feature but not part
  of transport
- the Rust ecosystem's `http-cache-semantics` crate, which focuses on the RFC
  decision engine
- the observation that existing higher-level cache middleware crates are useful
  references but too opinionated to become OpenWire's architecture directly

## Goals

- provide production-usable HTTP caching without contaminating transport
- keep RFC cacheability logic distinct from storage backend decisions
- make cache behavior observable and predictable
- leave room for multiple storage backends without rewriting cache policy logic

## Crate Boundary

The recommended architecture is:

- `crates/openwire-cache`: OpenWire-owned policy/orchestration crate
- adopted semantics engine underneath: likely `http-cache-semantics`
- adopted storage adapters underneath: in-memory and/or on-disk backends

`openwire-cache` should be responsible for integrating cache behavior into the
OpenWire execution chain. It should not merely wrap another middleware crate.

## Core Separation

There are three different things that should remain separate:

1. HTTP cache semantics
2. cache storage backend
3. OpenWire request/response orchestration

### HTTP Cache Semantics

This layer decides:

- whether a response is storable
- whether a stored response is fresh
- whether revalidation is required
- how `Vary`, `ETag`, `Last-Modified`, and authenticated responses affect reuse

This is the part most likely worth adopting from a focused crate.

### Cache Storage

This layer decides:

- where entries live
- how bytes and metadata are persisted
- eviction and capacity management
- concurrency model

This should sit behind OpenWire-owned storage traits.

### OpenWire Orchestration

This layer decides:

- where cache lookup runs in the request chain
- how conditional requests are generated
- when cached bodies are returned directly
- how unsafe methods invalidate cache entries
- how cache behavior appears in events and traces

This layer is pure OpenWire product logic and should stay owned in-house.

## Proposed Request-Chain Placement

Long-term placement should be:

1. application interceptors
2. cache lookup / cache policy layer
3. follow-up coordinator
4. cookie handling
5. bridge normalization
6. network interceptors
7. transport

Reason:

- cache should be able to short-circuit before transport
- cache revalidation still needs to cooperate with follow-up and normal
  transport flow
- cache should not sit below bridge/transport as if it were a protocol detail

## Proposed Storage Trait Direction

```rust
pub trait CacheStorage: Send + Sync + 'static {
    fn lookup(
        &self,
        key: &CacheKey,
    ) -> openwire_core::BoxFuture<Result<Option<CacheEntry>, CacheError>>;

    fn store(
        &self,
        key: CacheKey,
        entry: CacheEntry,
    ) -> openwire_core::BoxFuture<Result<(), CacheError>>;

    fn remove(
        &self,
        key: &CacheKey,
    ) -> openwire_core::BoxFuture<Result<(), CacheError>>;
}
```

The exact supporting types can evolve, but the separation should remain:

- `CacheKey`: OpenWire-owned cache identity
- `CacheEntry`: metadata + stored response representation
- `CacheError`: cache-system failure, separate from origin request failure

## Entry Model

The first milestone should store complete responses only after the full body is
available.

Reason:

- streaming cache writes complicate failure semantics substantially
- the first milestone should optimize for RFC correctness and predictable
  integration, not advanced body streaming

That means initial cache entries should include:

- request matching metadata
- response status
- response headers
- response body bytes
- cache-policy metadata needed for reuse/revalidation

## Response Combination Rules

The cache orchestration layer must own:

- generating conditional requests from stored metadata
- merging `304 Not Modified` responses with cached metadata/body
- invalidating or updating entries after unsafe methods

These are OpenWire-level semantics and should not be delegated blindly to a
storage backend.

## Failure Model

Cache failures should not automatically become request failures.

Recommended default behavior:

- cache read/write/remove failures are traced
- origin requests still proceed unless the user explicitly opts into a stricter
  policy later

Reason:

- caches are auxiliary performance and efficiency systems by default
- production systems should degrade to origin fetches rather than fail closed

## Authenticated Responses

Cache handling for authenticated responses must be explicit and standards-aware.

This is one of the reasons to rely on a focused semantics engine underneath:

- authenticated responses interact with cacheability in non-obvious ways
- proxy revalidation and `Vary` handling are similarly tricky

OpenWire should not hand-roll this logic from scratch.

## Backend Strategy

OpenWire should not pick one mandatory backend in the cache API itself.

Likely default adapters later:

- in-memory adapter over a concurrent cache crate
- disk adapter over an async content-addressed or file-based cache crate

But these adapters should sit under `CacheStorage`, not define it.

## First-Milestone Scope

In scope:

- GET/HEAD only
- full-response caching after complete body read
- cache hit / miss / revalidate paths
- `Vary`, `ETag`, and `Last-Modified` correctness
- invalidation for unsafe methods at a practical baseline

Out of scope initially:

- partial content caching
- streaming write-through cache bodies
- stale-if-error / stale-while-revalidate optimization policies
- offline mode
- distributed/shared cache coordination

## Observability Requirements

OpenWire should expose stable observability for:

- cache lookup result: hit / miss / stale / revalidate
- whether a request was served from cache or origin
- whether a `304` response refreshed a stored entry
- cache backend errors when they occur

These should integrate with the existing tracing/event model rather than appear
as a separate subsystem with unrelated naming.

## First-Milestone Acceptance

- cache semantics, storage, and orchestration are clearly separated
- the public cache API is defined by OpenWire, not by an adopted middleware
  crate
- the first storage trait is async and backend-agnostic
- the first milestone prioritizes RFC correctness over advanced caching tricks
- cache errors degrade gracefully by default instead of breaking ordinary calls
