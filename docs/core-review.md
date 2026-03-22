# Core Review — Completed Summary

Date: 2026-03-22
Review baseline: `main` at `9abcfc1`
Closure status: merged into `main`

The core review of the request execution path is complete. Its findings have
already been absorbed into the implementation, tests, and current design docs.
This file remains only as a compact index of what landed and where the lasting
source of truth now lives.

## Review Scope

- `Client::execute` through `TransportService`
- connection acquisition, pooling, protocol binding, and background-task ownership
- retry / redirect / auth / cookie follow-up behavior
- body framing, timeouts, and Tower readiness / backpressure
- proxy routing, SOCKS / CONNECT tunnels, and fast fallback

## Landed Outcomes

- lifecycle ownership is now RAII-based across `SelectedConnection`,
  `ResponseLease`, and response-body release
- owned connection tasks are tracked and aborted on final client drop
- request admission and fresh-connection admission are both explicit transport
  concerns
- request body semantics now distinguish `absent` from `explicit empty`
- redirect downgrade rejection, readiness propagation, proxy fast fallback,
  SOCKS5 auth, pool coalescing indexing, poison-safe locking, and protocol
  correctness fixes are all part of the current baseline

## Where To Look Now

- Current architecture: [`docs/DESIGN.md`](DESIGN.md)
- Completed trait/crate-boundary redesign:
  [`docs/trait-oriented-redesign.md`](trait-oriented-redesign.md)
- Historical workstream split and closure map:
  [`docs/plans/core-review-plan-spec.md`](plans/core-review-plan-spec.md)
- Executable verification: `cargo test --workspace --all-targets`
