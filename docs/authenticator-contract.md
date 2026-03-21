# OpenWire Authenticator Contract

Date: 2026-03-21

This document defines the intended authenticator contract for OpenWire.

It is informed by:

- OkHttp's `Authenticator`, especially its separation of origin and proxy
  authenticators
- the need for async credential lookup and token refresh in real applications
- OpenWire's current follow-up coordinator direction

## Goals

- support production authentication flows without coupling them to transport
- align with OkHttp's "return a follow-up request or decline" mental model
- keep auth loop control and replayability rules explicit
- preserve a clear distinction between origin auth and proxy auth

## Builder-Level Surface

Recommended builder hooks:

```rust
ClientBuilder::authenticator(...)
ClientBuilder::proxy_authenticator(...)
```

These should be configured independently.

Reason:

- origin auth and proxy auth operate on different headers, lifecycles, and
  sometimes different runtime layers
- users often need only one of them

## Proposed Trait Direction

```rust
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> openwire_core::BoxFuture<
        Result<Option<http::Request<openwire::RequestBody>>, openwire::WireError>,
    >;
}
```

The authenticator returns:

- `Ok(Some(request))` to continue with an authenticated follow-up
- `Ok(None)` to decline and return the current response to the caller
- `Err(error)` if authentication logic itself fails and should fail the call

## Why Async

Authenticators routinely need to:

- refresh tokens
- call credential providers
- read secure local storage
- coordinate with other async application state

Forcing these flows into a synchronous trait would either block the runtime or
push too much awkward caching into user code.

## `AuthContext`

`AuthContext` should be an owned snapshot, not a borrowed view into internal
state.

It should contain at least:

- the current response head snapshot
- the request URI, method, and headers
- total attempt count
- auth attempt count
- retry count and redirect count
- whether the request body is replayable
- whether this is origin auth or proxy auth
- best-effort route / proxy metadata when available

It should also expose a helper equivalent to "clone the current request if
replayable" so common implementations do not have to rebuild requests from
scratch.

## Core Semantics

### Return A Follow-Up Request Or Decline

The authenticator should follow the OkHttp model:

- return a follow-up request with credentials when the challenge can be handled
- return `None` when it should give up

### Avoid Repeating The Same Failed Attempt

If the incoming request already carries the relevant auth header, the
authenticator should typically decline rather than looping.

Default guidance:

- for `401`, check `Authorization`
- for `407`, check `Proxy-Authorization`

### Replayability Is Mandatory

Automatic auth follow-ups require replayable request bodies.

If the request body cannot be replayed:

- OpenWire should not automatically authenticate and retry
- the authenticator context should make this visible directly

## Origin Auth vs Proxy Auth

### Origin Auth

Origin authentication is a pure follow-up policy concern.

Typical path:

1. receive `401`
2. authenticator inspects `AuthContext`
3. authenticator returns a new request with `Authorization`
4. follow-up coordinator schedules the new attempt

### Proxy Auth

Proxy authentication is split across policy and connector behavior.

There are two different cases:

- normal response-path proxy auth, where a proxied request receives `407`
- HTTPS-over-HTTP-proxy tunnel auth, where `CONNECT` must be authenticated
  before the normal request pipeline runs

For the second case, OpenWire should align with OkHttp's operational model:

- proxy authentication may need a preemptive hook before the tunnel request is
  sent
- this path is not equivalent to a normal origin-response follow-up
- connector-level support is required

Therefore:

- `proxy_authenticator` is a builder-level concept shared with the general auth
  model
- but not every proxy-auth invocation will occur inside the same runtime path as
  origin auth

## Attempt Accounting

Authentication must have its own counter, separate from:

- retry count
- redirect count
- total attempt count

This is necessary for:

- observability
- loop termination
- policy clarity when retries, redirects, and auth interact in one logical call

## Initial Scope Recommendation

The first authenticator milestone should support straightforward header-based
follow-ups.

In scope:

- Basic and Bearer-style credential injection
- async token refresh
- origin `401`
- proxy `407` where the response is visible in the normal request path

Explicitly not required in the first milestone:

- multi-round challenge state machines such as NTLM
- broad scheme-specific negotiation engines
- implicit body buffering to make non-replayable requests authenticate

## First-Milestone Acceptance

- origin auth can produce an authenticated follow-up for replayable requests
- auth loops terminate predictably
- auth counters remain distinct from retry and redirect counters
- proxy auth architecture does not force CONNECT-specific behavior into the
  normal origin-auth path
- the public trait remains independent from any one credential or token library
