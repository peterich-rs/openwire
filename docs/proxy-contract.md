# OpenWire Proxy Contract

Date: 2026-03-21

This document defines the intended proxy contract for OpenWire.

It is informed by:

- OkHttp's split between route selection, proxy configuration, and proxy
  authentication
- reqwest's proxy rule model, including ordered matching and `no_proxy`
- the practical need to keep CONNECT and SOCKS mechanics below the normal
  request policy chain

## Goals

- provide a production-usable proxy model without leaking connector-specific
  details into ordinary request code
- keep proxy selection predictable and composable
- support HTTP proxy, HTTPS proxy, and SOCKS use cases without overloading one
  opaque API
- preserve a clear line between request policy and connector behavior

## Core Design Principle

Proxy support spans two different layers and should not be collapsed into one:

- config and selection belong to client-level policy/configuration
- CONNECT / SOCKS / proxy socket behavior belongs to the connector layer

This mirrors the operational reality better than treating proxies as just a
header mutation feature.

## Proposed Public Surface

### Basic Builder API

```rust
ClientBuilder::proxy(...)
ClientBuilder::proxy_selector(...)
ClientBuilder::proxy_authenticator(...)
ClientBuilder::use_system_proxy(...)
```

The exact final API can be simplified, but these are the concepts OpenWire
needs to represent.

### Proposed Types

```rust
pub enum ProxyMode {
    Http,
    Https,
    Socks5,
}

pub struct ProxyRule {
    pub mode: ProxyMode,
    pub target: url::Url,
    pub scope: ProxyScope,
    pub no_proxy: Option<NoProxy>,
}

pub enum ProxyScope {
    Http,
    Https,
    All,
    Custom,
}

pub trait ProxySelector: Send + Sync + 'static {
    fn select(&self, uri: &http::Uri) -> Option<ProxyRule>;
}
```

The final surface does not need to expose all fields publicly exactly as above,
but these are the concepts that must exist.

## Selection Semantics

### Ordered Matching

Proxy selection should be deterministic and ordered.

Reason:

- reqwest explicitly documents ordered proxy rule checking
- multiple overlapping rules are common in production
- "first match wins" is easier to reason about than implicit priority rules

### Explicit Constructors

OpenWire should provide convenience constructors for common cases:

- `ProxyRule::http(...)`
- `ProxyRule::https(...)`
- `ProxyRule::all(...)`
- `ProxyRule::socks5(...)`

These should cover the majority of user setups without requiring a custom
selector.

### `no_proxy`

OpenWire should model proxy exclusions explicitly instead of forcing users into
ad hoc selector closures for common direct-connect exceptions.

The first milestone should support:

- exact host matches
- domain suffix matches
- loopback / localhost exclusions
- wildcard exclusions
- IP / CIDR exclusions when parsing environment-driven `NO_PROXY`

## System Proxy Support

OpenWire should support "use the system/environment proxy configuration" as a
first-class option, but it should remain opt-in and explicit.

Baseline status on the current branch:

- `ClientBuilder::use_system_proxy(true)` loads common `http_proxy`,
  `https_proxy`, `all_proxy`, and `NO_PROXY` / `no_proxy` variables
- explicit `proxy(...)` rules still win because they are evaluated first
- `NO_PROXY` parsing currently covers host/domain exclusions, `localhost` /
  loopback, wildcard `*`, and IP CIDR ranges
- proxy CONNECT establishment is bounded by the configured `connect_timeout`
  instead of only TCP socket connect time

Reason:

- system proxy behavior is operationally important
- environment variable behavior around `http_proxy`, `https_proxy`, `all_proxy`,
  and `no_proxy` is messy and inconsistent across ecosystems
- a dedicated adapter layer is cleaner than baking environment parsing into core
  client code

Recommendation:

- provide a first-party system proxy selector module
- keep that logic outside the core request state machine
- treat crates focused on system proxy interpretation as implementation options,
  not public API

## Connector Boundary

The connector layer should own:

- direct socket connection to the proxy endpoint
- HTTP CONNECT handshake
- SOCKS handshake
- proxy tunnel lifecycle
- connector-level errors for proxy establishment

The policy layer should own:

- choosing whether a request is proxied
- mapping request URI to a proxy rule
- integrating `proxy_authenticator` for response-path `407` handling
- observability for proxy selection decisions

## Authentication Split

Proxy auth cannot be treated as one homogeneous flow.

### Response-Path Proxy Auth

For ordinary proxied HTTP requests, a `407` may be visible through the normal
request/response flow. This can participate in the follow-up coordinator.

### CONNECT-Time Proxy Auth

For HTTPS over an HTTP proxy, the `CONNECT` tunnel may require authentication
before the normal request pipeline exists.

That means:

- connector-level support is mandatory
- `proxy_authenticator` cannot be modeled only as a normal post-response retry
- OpenWire must leave room for preemptive or connector-mediated proxy auth

Baseline status on the current branch:

- response-path proxy `407` is handled in the follow-up coordinator
- `CONNECT`-time proxy `407` is handled in the connector path through the same
  builder-level `proxy_authenticator`

## DNS Policy

Proxy mode affects where name resolution should happen.

Open questions that must stay explicit in the design:

- HTTP proxy with absolute-form requests may not require client-side target DNS
- SOCKS5 can support remote DNS semantics
- some users will want local DNS resolution for auditing or policy reasons

Recommendation:

- do not hide this behind magic
- ensure the connector boundary can express whether the target is passed as a
  domain name or resolved IP

## Observability Requirements

Proxy support should emit stable observability around:

- whether a proxy was selected
- which proxy mode was used
- whether connection establishment failed before reaching the origin
- whether auth was origin auth or proxy auth

These fields belong in OpenWire's tracing/event schema, not in dependency
specific logs.

## First-Milestone Scope

In scope:

- explicit proxy rules
- ordered matching
- `no_proxy`
- HTTP proxy for HTTP requests
- HTTP proxy with CONNECT for HTTPS requests
- SOCKS5 support if the underlying adopted connector path is viable

Out of scope for the first milestone:

- PAC script support
- dynamic proxy pools
- automatic proxy failover between multiple upstreams
- broad system-specific proxy configuration beyond common environment semantics

## First-Milestone Acceptance

- the public API clearly separates proxy selection from connector mechanics
- HTTP and HTTPS proxy routing can be configured without user-written connector
  code
- proxy auth architecture supports both response-path `407` and future
  CONNECT-time auth
- proxy behavior does not distort the core follow-up coordinator
- the proxy implementation remains swappable behind OpenWire-owned boundaries
