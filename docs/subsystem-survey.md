# OpenWire Subsystem Survey

Survey date: 2026-03-21

This document captures the initial research baseline for major OpenWire
subsystems.

The recommended public/internal trait shapes that follow from this survey live
in [policy-interface-design.md](policy-interface-design.md).

It is not a lock-in document. Its job is to reduce ambiguity before
implementation and to prevent accidental re-implementation of commodity
infrastructure.

## Evaluation Rules

For each subsystem:

- prefer a mature default from the Rust ecosystem when the problem is mostly
  commodity infrastructure
- keep OpenWire's public API and orchestration model independent from the
  concrete default crate
- reject defaults that are too opinionated around another client stack or too
  immature for a production default

## Initial Conclusions

### HTTP Engine

- Direction: adopt
- Default: `hyper` + `hyper-util`
- Why: this is already the correct default for protocol handling, connection
  management, and pooling; OpenWire's differentiation is above this layer
- OpenWire-owned layer: transport traits, connection lifecycle handling, event
  emission, body wrapping, and policy orchestration

### Middleware / Request Pipeline

- Direction: adopt
- Default: `tower`
- Why: it is already a strong fit for interceptor and policy composition, and
  it keeps the execution chain explicit
- OpenWire-owned layer: interceptor semantics, policy ordering, and lifecycle
  contracts

### Runtime Integration

- Direction: wrap, not replace
- Default: Tokio
- Why: it is the practical default for the surrounding ecosystem; OpenWire only
  needs to own the `Runtime` abstraction, not a runtime implementation
- OpenWire-owned layer: `Runtime` trait and compatibility surface

### DNS

- Direction: adopt + wrap
- Default: host system resolver remains the default
- Optional alternate: `hickory-resolver`
- Why:
  - the system resolver is the least surprising production default for general
    app compatibility
  - `hickory-resolver` is valuable as an optional in-process resolver for more
    advanced behavior, but its own docs explicitly state that it is a fully
    in-process resolver and not the host OS resolver
- Recommendation:
  - keep the current system resolver default
  - research a first-party optional Hickory adapter later
  - do not make Hickory the default just because it is feature-rich
- OpenWire-owned layer: `DnsResolver` trait, resolver selection, and call/event
  integration

### TLS

- Direction: adopt + wrap
- Default: `rustls` with `rustls-platform-verifier`, preserving the current
  native-roots fallback behavior
- Why:
  - `rustls` is a strong modern default
  - `rustls-platform-verifier` is specifically aimed at using OS certificate
    verification facilities and documents correctness/security advantages over
    loading native roots alone
  - this aligns especially well with OpenWire's cross-platform and mobile
    direction
- Recommendation:
  - keep `rustls` as the default TLS backend
  - keep TLS behind `TlsConnector`
  - do not chase multiple first-party TLS stacks unless a concrete platform gap
    appears
- OpenWire-owned layer: `TlsConnector` trait, TLS lifecycle events, and client
  configuration wiring

### Cookies

- Direction: adopt storage/matching + own orchestration
- Default candidate: `cookie_store`
- Avoid as direct dependency surface: `reqwest_cookie_store`
- Why:
  - `cookie_store` is specifically for cookie storage/retrieval and RFC6265
    path/domain matching
  - `reqwest_cookie_store` is a reqwest adapter layer, not the right product
    boundary for OpenWire
- Recommendation:
  - expose an OpenWire `CookieJar` trait
  - implement the default jar as an adapter over `cookie_store`
  - keep persistence optional instead of defaulting to a serialized store in the
    core crate
- OpenWire-owned layer: `CookieJar` trait, request/response policy wiring, and
  interaction with redirects/follow-ups

### Authentication Follow-Ups

- Direction: build in-house
- Default dependency: none
- Why:
  - the hard part is not header parsing; it is request replayability, retry and
    redirect interaction, loop control, and lifecycle semantics
  - this is part of OpenWire's policy core
- Recommendation:
  - own an `Authenticator` trait in OpenWire
  - integrate auth into the follow-up coordinator instead of into transport
- OpenWire-owned layer: the whole feature

### HTTP Cache Semantics

- Direction: adopt semantics engine + own cache orchestration
- Default candidate for policy logic: `http-cache-semantics`
- Avoid as direct architecture default: `http-cache`
- Why:
  - `http-cache-semantics` focuses on the hard RFC cacheability logic
  - `http-cache` is already a higher-level middleware with its own managers and
    stack assumptions; that is useful as reference material but too opinionated
    to become OpenWire's architecture
- Recommendation:
  - build `openwire-cache` as an OpenWire-owned crate
  - use `http-cache-semantics` as the likely default policy engine
  - keep storage backend swappable instead of adopting a full middleware stack
- OpenWire-owned layer: cache crate API, storage abstraction, conditional
  request wiring, and observability integration

### Cache Storage

- Direction: adopt primitives behind OpenWire storage traits
- In-memory candidate: `moka`
- On-disk candidate: `cacache`
- Why:
  - `moka` is a strong concurrent in-memory cache
  - `cacache` is a strong async-oriented disk cache
  - neither should define OpenWire's HTTP cache API by themselves
- Recommendation:
  - do not choose a mandatory backend yet
  - design `openwire-cache` storage traits first
  - treat `moka` and `cacache` as likely default adapters, not as architectural
    centers

### Proxy Support

- Direction: mixed, baseline implemented
- Current baseline:
  - ordered `Proxy` rules
  - `NoProxy` host / suffix / loopback exclusions
  - HTTP forward proxying
  - HTTPS over HTTP proxy via `CONNECT`
  - `proxy_authenticator` for both response-path `407` and `CONNECT`-time proxy auth
  - opt-in environment proxy loading via `use_system_proxy(true)`
- Possible future connector candidate: `proxied`
- Why no mandatory dependency yet:
  - proxy support reaches across URI modeling, connector behavior, auth,
    tunneling, and possibly DNS resolution strategy
  - the remaining gaps are mostly SOCKS and long-tail system proxy semantics
    rather than plain forwarding/tunneling
- Recommendation:
  - keep the current OpenWire-owned proxy surface and connector split
  - do not lock a default external proxy dependency yet
  - only evaluate a crate like `proxied` after the remaining auth and selector
    requirements are clearer

### Observability Export

- Direction: adopt primitives + own schema
- Default: `tracing`
- Why: OpenWire should own the event model and field schema, but exporter
  integration itself should remain ecosystem-facing
- Recommendation:
  - keep current tracing direction
  - postpone dedicated OpenTelemetry integration until after core semantics are
    stable

## Recommended Implementation Order

1. Finish `Core Semantics Hardening`
2. Harden the remaining proxy paths:
   SOCKS and broader selector parity
3. Start `openwire-cache` with OpenWire-owned orchestration plus adopted cache
   semantics/storage primitives
4. Revisit alternate DNS/TLS adapters after cache and proxy gaps are clearer

## Research Tasks Still Open

- decide whether SOCKS belongs in the core crate or a first-party adapter crate
- decide how far the built-in environment proxy parser should go beyond common
  `*_proxy` / `NO_PROXY` semantics
- decide whether alternate DNS defaults should get first-party adapters early
- decide the cache storage trait surface before picking a default backend
- decide what mobile-specific TLS and DNS integration tests are required before
  broadening defaults
