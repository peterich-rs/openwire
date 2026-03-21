# OpenWire CookieJar Contract

Date: 2026-03-21

This document defines the intended `CookieJar` contract for OpenWire.

It is informed by:

- OkHttp's `CookieJar`, which treats the jar as both cookie policy and
  persistence
- reqwest's synchronous `CookieStore` interface
- the Rust ecosystem direction of adapting a mature cookie-store crate instead
  of rebuilding RFC6265 storage logic

## Goals

- keep cookie handling predictable and cheap on the request hot path
- align with OkHttp's mental model where the jar owns both acceptance policy and
  storage
- keep OpenWire independent from any one concrete cookie implementation
- ensure redirect and follow-up flows can observe newly stored cookies within
  the same logical call

## Proposed Public Trait

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

## Why This Shape

- it is close to reqwest's existing interface, which already maps well to a
  cookie-store-backed implementation
- it avoids leaking concrete cookie types from any adopted crate into OpenWire's
  public API
- it keeps lookup synchronous, which matches the expected hot-path behavior

## Default Semantics

### No Jar Configured

If no `CookieJar` is configured, OpenWire performs no automatic cookie loading
or persistence.

The effective behavior should match an explicit `NO_COOKIES` policy rather than
silently storing cookies anywhere.

### Request Path

OpenWire should consult the jar:

1. after the request URL is finalized for the next network attempt
2. after retry/redirect/auth follow-up decisions are made
3. before bridge normalization and before network interceptors run

This order ensures:

- redirect targets see cookies set by earlier responses
- auth follow-up requests can pick up new cookies
- network interceptors observe the actual cookie header that will be sent

### Response Path

OpenWire should pass `Set-Cookie` headers to the jar as soon as response headers
are available.

Future-proofing rule:

- the jar contract must tolerate multiple `set_cookies()` calls for one logical
  response, because trailer cookies may be supported later

OpenWire does not need trailer cookie support in the first milestone, but the
contract should not preclude it.

## Precedence Rules

### Explicit `Cookie` Header Wins

If the user or an application interceptor already set a `Cookie` header on the
request, OpenWire should not append or rewrite it from the jar.

Reason:

- explicit request-scoped intent should beat automatic jar behavior
- mixed automatic/manual composition is too error-prone to make implicit

### Jar Filtering Is Jar-Owned

OpenWire should not duplicate domain/path/expiry/security filtering logic in
its policy layer.

The jar is responsible for deciding:

- what cookies are accepted from responses
- which cookies match a request URL
- when cookies expire or are removed

## Error Model

The trait should stay infallible.

Implications:

- malformed `Set-Cookie` values should be ignored by the jar implementation
- OpenWire may trace cookie parsing or persistence failures for debugging, but
  should not fail the HTTP call because a cookie could not be stored

This mirrors the practical expectation that cookie handling is auxiliary to the
main request unless the application explicitly builds stronger guarantees on top.

## Concurrency And Persistence

The trait does not prescribe a storage model.

Acceptable implementations include:

- in-memory jars
- persistent jars backed by a file or database
- policy-only jars that accept or reject without durable storage

However, the default implementation should assume fast local state, typically a
lock-protected in-memory cookie store.

## Default Adapter Direction

Recommended default implementation:

- `CookieStoreJar`
- internally wraps a mature cookie-store crate behind `RwLock` or `Mutex`

OpenWire should own:

- the trait
- the no-cookies default
- request/response policy wiring
- documentation of cookie interaction with retries, redirects, and auth

OpenWire should not own:

- RFC6265 path/domain matching logic
- public suffix handling logic
- serialization formats for persisted cookies as part of the core trait

## First-Milestone Acceptance

- request cookies are loaded before each network attempt
- redirect follow-ups can observe cookies stored from prior redirect responses
- explicit user-provided `Cookie` headers are preserved
- malformed cookie storage does not fail the HTTP call
- the default jar implementation remains replaceable through the public trait
