# OpenWire Policy Interface Design

Date: 2026-03-21

This document turns the current subsystem survey into concrete interface
recommendations for the next policy-facing capabilities.

It does not mean these features should be implemented immediately. The purpose
is to define the trait and boundary shape early, so later implementation work
does not reopen the core architecture.

## Design Goals

- align the user mental model with OkHttp where that improves predictability
- keep OpenWire's public API independent from any one adopted default crate
- make hot-path operations synchronous only when they should stay cheap and
  local
- allow expensive or externally coordinated work to remain asynchronous
- keep policy concerns above transport unless the problem is inherently a
  connector concern

## CookieJar

### Recommendation

Use a synchronous trait, closely aligned with the `reqwest::cookie::CookieStore`
shape, and adapt a mature cookie-store crate behind it.

Detailed contract: [cookiejar-contract.md](cookiejar-contract.md).

### Proposed Trait

```rust
pub trait CookieJar: Send + Sync + 'static {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &http::HeaderValue>,
        url: &url::Url,
    );

    fn cookies(&self, url: &url::Url) -> Option<http::HeaderValue>;
}
```

### Why Sync

- cookie lookup should stay on the request hot path and should normally be an
  in-memory operation
- the likely default adapter over a cookie-store crate is naturally synchronous
- persistence can be handled outside the hot path instead of forcing every
  request to await storage I/O

### Policy Rules

- OpenWire should ask the jar for cookies before each network attempt, not only
  before the first logical request
- OpenWire should store `Set-Cookie` values as soon as response headers are
  available
- if the user already set an explicit `Cookie` header, OpenWire should preserve
  it and skip automatic jar injection for that request
- cookie handling stays in the policy chain, not transport

### Default Implementation Direction

- public abstraction: `CookieJar`
- likely default adapter: `CookieStoreJar` over a mature cookie-store crate
- persistence: optional, not part of the core hot-path trait

## Authenticator

### Recommendation

Use an asynchronous trait. Unlike cookies, authenticators may need token
refresh, credential lookup, or other I/O and should not be forced into a
blocking model.

Detailed contract: [authenticator-contract.md](authenticator-contract.md).

### Proposed Builder Surface

```rust
ClientBuilder::authenticator(...)
ClientBuilder::proxy_authenticator(...)
```

Keeping origin and proxy auth separate matches the operational reality better
than trying to collapse both into a single opaque hook.

### Proposed Trait Direction

```rust
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> openwire_core::BoxFuture<Result<Option<http::Request<openwire::RequestBody>>, openwire::WireError>>;
}
```

### Proposed `AuthContext`

`AuthContext` should be an owned snapshot so the authenticator can be async
without borrowing from internal state machines.

It should expose at least:

- response head snapshot
- current request URI/method/headers
- total attempt count
- auth attempt count
- whether the body can be replayed
- whether the challenge is origin auth or proxy auth
- a helper to clone the current request when replayable

### Policy Rules

- automatic auth follow-ups require replayable bodies
- auth attempt limits must be independent from retry and redirect limits
- auth participates in the existing follow-up coordinator, not transport
- origin auth should live in the follow-up coordinator
- proxy auth needs a split design:
  reactive `407` responses for normal proxied HTTP requests can participate in
  follow-up coordination, but HTTPS proxy `CONNECT` authentication requires
  connector-level support before the normal request pipeline runs
- `401` and `407` should therefore share builder-level concepts without forcing
  all proxy auth behavior into the same runtime path as origin auth

## Proxy Configuration

### Recommendation

Treat proxy support as a connector concern with a policy-facing configuration
surface.

The selection rule is user-facing; the tunnel/connect mechanics are connector
behavior.

Detailed contract: [proxy-contract.md](proxy-contract.md).

### Proposed Public Shape

```rust
pub struct ProxyRule { /* intercept scope + proxy target + auth + exclusions */ }

pub trait ProxySelector: Send + Sync + 'static {
    fn select(&self, uri: &http::Uri) -> Option<ProxyRule>;
}
```

OpenWire should also provide convenience constructors in the style of:

- `ProxyRule::http(...)`
- `ProxyRule::https(...)`
- `ProxyRule::all(...)`
- `ProxyRule::socks5(...)`

### What Belongs Where

- user rule matching / exclusions / ordering: client config surface
- HTTP CONNECT / SOCKS tunnel mechanics: connector layer
- proxy credential selection: authenticator boundary
- `407` handling is split:
  normal response-path proxy auth can use follow-up policy, but `CONNECT`
  authentication must be supported in connector code

### Open Questions

- whether DNS resolution for proxied requests is local or remote for each proxy
  mode
- whether proxy auth headers are stored in the proxy rule or supplied only by
  `proxy_authenticator`
- how `no_proxy` should be modeled for IP ranges, suffixes, and exact hosts

## Cache Storage

### Recommendation

Keep HTTP cache semantics and storage separate.

- semantics and conditional request logic belong in `openwire-cache`
- storage should sit behind OpenWire-owned traits
- adopted crates can power semantics or storage, but should not define the
  public cache API

Detailed contract: [cache-contract.md](cache-contract.md).

### Proposed Crate Boundary

- `openwire-cache`: policy/orchestration crate
- adopted default semantics engine: an HTTP cache semantics crate
- adopted default storage adapters later: in-memory and on-disk backends

### Proposed First Storage Trait Direction

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

### Phase-1 Scope Recommendation

- limit the first cache milestone to GET/HEAD
- store complete response bodies before commit instead of trying to design a
  streaming cache body protocol immediately
- make `Vary`, `ETag`, and `Last-Modified` correctness higher priority than
  backend flexibility

### What OpenWire Must Own

- cache key strategy
- conditional request wiring
- invalidation rules for unsafe methods
- observability and trace fields around cache hit/miss/revalidate behavior

## Recommended Design Order

1. Finish proxy hardening at the existing boundary:
   `CONNECT`-time auth, selector parity, and optional SOCKS support
2. Finalize `openwire-cache` crate boundary and `CacheStorage` trait
3. Revisit whether broader system-proxy and alternate connector adapters belong
   in-tree behind the current public surfaces

This order keeps the current follow-up coordinator and connector boundaries
stable while focusing new work on the largest remaining production gaps.
