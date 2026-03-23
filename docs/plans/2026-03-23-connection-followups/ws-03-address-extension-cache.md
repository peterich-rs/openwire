# WS-03 — Address Extension Cache

Date: 2026-03-23
PR: `PR3`
Branch: `perf/address-extension-cache`
Status: planned

## Scope

- avoid recomputing the same `Address` in request admission and transport
  preparation
- keep fallback recomputation so transport remains robust if the extension is
  absent

## Owned Files

- `crates/openwire/src/client.rs`
- `crates/openwire/src/connection/exchange_finder.rs`
- related unit or integration tests
- `docs/DESIGN.md`

## Locked Decisions

- Add a crate-private request extension newtype named `CachedAddress`.
- `CachedAddress` derives `Clone` and wraps `Address`.
- `RequestAdmissionService` computes the `Address` once, writes
  `CachedAddress` into request extensions, then uses it for admission.
- `ExchangeFinder::prepare` first checks for `CachedAddress`; if absent, it
  computes `Address` exactly as today.
- No changes are required in follow-up cloning logic because request extensions
  are already cloned by existing request reconstruction paths.

## Implementation Steps

1. Add the new extension type in the narrowest crate-private location that both
   admission and exchange-finder code can use.
2. Update request admission to populate the extension before waiting on the
   limiter.
3. Update exchange preparation to read the extension and fall back to
   recomputation only when missing.
4. Add focused tests for cache hit and fallback behavior.
5. Update design docs to mention that request admission and transport share the
   computed destination `Address`.

## Verification

- Add a test proving transport consumes the precomputed address when present.
- Add a test proving recomputation still works when the extension is absent.
- Ensure existing request-limiter and pool-hit tests still pass.

## Non-goals

- do not change `Exchange` shape or public APIs
- do not piggyback any pool or timeout work into this PR

## Docs To Update On Merge

- `docs/DESIGN.md`
