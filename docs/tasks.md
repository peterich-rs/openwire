# OpenWire Execution Tasks

Date: 2026-03-21

This file tracks only the remaining deferred follow-on work for the
real-network-validation baseline described in `docs/ARCHITECTURE.md`.

Completed baseline:

- baseline live-network validation work landed and was verified
- the repository now has an opt-in live-network suite plus a separate manual /
  scheduled workflow outside the required CI gate
- detailed completed phase tables were removed to keep this file focused on
  outstanding follow-on work; use git history when the exact landing sequence
  matters

Scope of the remaining tracker:

- capture only live-validation themes that are still intentionally deferred
- preserve task IDs for future reactivation
- keep the required CI path unchanged unless a future task explicitly says
  otherwise

## Status Legend

| Status | Meaning |
|---|---|
| `DEFERRED` | Accepted but intentionally postponed |

## Current Baseline

- `crates/openwire/tests/live_network.rs` covers the current zero-config public
  smoke suite
- `.github/workflows/live-network.yml` runs that suite only by manual dispatch
  or schedule
- deterministic local fixtures remain the source of truth for proxy semantics,
  fast-fallback timing, and exact event-ordering behavior

Current zero-config live coverage includes:

- `httpbingo` GET / POST / redirect / cookies / timeout / stream / ETag / auth
- `badssl` certificate-failure classification
- `postman-echo` GET / POST interoperability
- `jsonplaceholder` GET / POST JSON REST smoke paths

## Deferred Follow-Ons

These items remain deferred because they are blocked either by external
credentials, stateful third-party setup, or public-network safety / stability
constraints that are outside the current live-suite contract.

| ID | Status | Theme | Notes |
|---|---|---|---|
| LNV-D01 | `DEFERRED` | GitHub REST API live validation | Public read-only repo metadata is available without a token, but broader API coverage still needs explicit rate-limit budgeting and any authenticated or mutating paths need external credentials |
| LNV-D02 | `DEFERRED` | Webhook.site validation | Zero-config temporary tokens can be created on demand, but the flow adds stateful request-log polling and temporary remote resources beyond the baseline smoke-suite contract |
| LNV-D03 | `DEFERRED` | ReqRes validation | ReqRes currently requires `x-api-key` from `app.reqres.in`; anonymous requests may fail with `missing_api_key` or Cloudflare challenge responses |
| LNV-D04 | `DEFERRED` | Public proxy / tunnel live validation | Proxy and tunnel semantics remain covered by deterministic local fixtures until a trusted external proxy contract exists; arbitrary public proxies are not an acceptable default |
| LNV-D05 | `DEFERRED` | Public fast-fallback timing validation | Public networks are not suitable for strict or even loosely budgeted race-timing assertions that would claim fast-fallback correctness |
| LNV-D06 | `DEFERRED` | TLS-version-policy live validation | `badssl` `tls-v1-*` endpoints exist, but expected outcomes vary with the active TLS backend, verifier, and platform policy defaults rather than only OpenWire behavior |
