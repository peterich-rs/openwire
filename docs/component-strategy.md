# OpenWire Component Strategy

This document records the default "build vs adopt" stance for major OpenWire
subsystems.

The current research snapshot for likely default dependencies and cautions lives
in [subsystem-survey.md](subsystem-survey.md).

It should stay practical:

- pick a default direction for each subsystem
- define the OpenWire-owned trait or orchestration boundary
- identify where research is still required
- avoid accidental re-implementation of commodity infrastructure

## Decision Rules

- adopt mature, widely used crates by default for commodity protocol and
  storage mechanics
- keep OpenWire-owned trait boundaries stable so defaults remain swappable
- build in-house only where behavior orchestration, API quality, observability,
  or extensibility are the differentiators

## Strategy Matrix

| Subsystem | Default Direction | OpenWire-Owned Boundary | Candidate Default | Current Status |
| --- | --- | --- | --- | --- |
| HTTP protocol / connection engine | Adopt | transport service + response lifecycle wrapping | `hyper`, `hyper-util` | active default |
| Middleware / pipeline composition | Adopt | interceptor and policy model | `tower` | active default |
| Runtime integration | OpenWire wraps | `Runtime` trait | Tokio by default | active default |
| DNS resolution | Adopt + wrap | `DnsResolver` trait | system resolver by default, alternate resolver later | active default, alternates can be researched |
| TCP connect | Adopt platform async I/O + wrap | `TcpConnector` trait | Tokio TCP by default | active default |
| TLS | Adopt + wrap | `TlsConnector` trait | `rustls` default | active default |
| Retry / redirect / follow-up semantics | Build in-house | policy core | OpenWire internal policy modules | active hardening |
| Request normalization | Build in-house | bridge / policy layer | OpenWire internal `BridgeInterceptor` | active default |
| Event / trace observability | Build in-house on ecosystem primitives | event listener + tracing schema | `tracing` for spans/events | active default |
| Cookies | Adopt storage/matching + own orchestration | `CookieJar` trait + policy wiring | `cookie_store` behind the default `Jar` | active default |
| Authentication follow-ups | Build in-house | `Authenticator` trait + follow-up coordinator | OpenWire internal policy modules | active default |
| Cache semantics | Build in-house, possibly adopt storage primitives | future cache traits / `openwire-cache` crate | OpenWire-owned policy crate | research before implementation |
| Proxy behavior | Mixed | `Proxy` / `NoProxy` surface + transport connector boundaries | OpenWire orchestration, no mandatory proxy crate yet | active baseline |
| Compression / content decoding | Adopt + wrap if added | response-body policy boundary | ecosystem codec crates if needed | deferred |
| OpenTelemetry / metrics export | Adopt | tracing bridge / exporter hooks | ecosystem exporters | deferred |

## Immediate Research Queue

The next research-and-planning pass should focus on:

1. Cache crate scope and storage abstraction
2. Whether SOCKS and broader system-proxy adapters belong in-tree or behind optional adapters
3. Whether alternate DNS/TLS defaults need feature-gated first-party adapters
4. Whether compression / content-decoding should arrive before broader transport adapters

## What OpenWire Must Own

Even when defaults come from external crates, OpenWire should keep ownership of:

- the public API surface
- the canonical request execution chain
- lifecycle semantics across retries, redirects, and future follow-ups
- event ordering and tracing field contracts
- extension points and swappable trait boundaries
- behavior documentation and production guarantees
