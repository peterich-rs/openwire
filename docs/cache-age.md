# Correct Cached Response Age

Date: 2026-11-13

Status: implemented; verification evidence is recorded below.

## Feasibility Assessment

The transport already preserves response extensions through bridge and body
wrapping. Cache age can consume per-exchange timestamps without changing the
execution chain, connector traits or existing constructor signatures. Fully
feasible.

## Current Surface Inventory

- `openwire::transport::service::send_bound_request` captures the successful H1
  or H2 exchange and attaches per-exchange timing metadata.
- `openwire_core::transport` owns shared transport metadata and public re-exports.
- `openwire_cache::CacheInterceptor::intercept` buffers cacheable bodies before
  storage and merges 304 validation results.
- `openwire_cache::CachedResponse::{new_with_vary,current_age}` anchor corrected
  initial age at response receipt and retain monotonic residence.
- `openwire_cache::{response_current_age,merge_304_headers}` own the arithmetic
  and validation header handling.

## Design

1. Transport records each actual request send and final response header receipt,
   after sender readiness and before response callbacks. Logical call creation
   and application-interceptor duration are rejected as timing sources because
   redirects, authentication and retries can perform multiple exchanges.
2. A cloneable `ResponseTiming` response extension carries response delay,
   monotonic receipt time and wall-clock receipt time. Standard Instant supplies
   durations; wall time is used only against Date. Existing trait signatures
   and constructor signatures remain unchanged.
3. Cache stores corrected initial age anchored at header receipt. Body buffering,
   interceptor processing, storage and subsequent residence all age the response
   exactly once. Fractional durations are retained until serializing Age.
4. Validation uses the 304 exchange's timing and Age/Date. A recipient with a
   clock adds a missing Date at receipt before merging headers; old Age is
   removed. Missing timing is not guessed: synthetic responses without metadata
   are returned but not stored, and internal 304s still reconstruct the caller's
   response without publishing an unverified refreshed entry.

Public metadata API in `crates/openwire-core/src/transport.rs`:

```rust
#[derive(Clone, Copy, Debug)]
pub struct ResponseTiming {
    response_delay: std::time::Duration,
    received_at: std::time::Instant,
    received_at_wall: std::time::SystemTime,
}
```

Custom response sources can attach this extension using `ResponseTiming::new`
with the actual request send time, response receipt time and receipt wall time.
Body wrappers must preserve extensions; replacing a response source requires
replacing its timing metadata too. Existing connectors need no changes.

### RFC contract

Role: HTTP user agent with a private cache and a clock. Scope: supported 200 GET
storage, cache hits and 304 validation, including body buffering before storage.

Sources checked 2026-11-13: [RFC 9111 info](https://www.rfc-editor.org/info/rfc9111/),
[RFC 9110 info](https://www.rfc-editor.org/info/rfc9110/), and
[RFC Editor errata export](https://www.rfc-editor.org/errata.json). Neither RFC
has a listed update; no verified RFC 9111 erratum changes this behavior. Verified
RFC 9110 errata do not affect §6.6.1. Reported errata are not applied as normative
corrections.

| Requirement | Level / applicability | Implementation owner | Independent expected evidence | Status |
| --- | --- | --- | --- | --- |
| [RFC 9111 §4.2.3](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.2.3): Age correction accounts for request initiation and response delay | MUST interpret corrected Age relative to request initiation; conservative max is a permitted choice | transport timing + cache arithmetic | request1000/response1002/Date900/Age200/now1005 yields205; Date-dominant and future-Date cases | verified by focused tests listed below |
| RFC 9111 §4.2.3: residence begins at receipt | §4 MUST on unvalidated cache hits; §4.2.3 algorithm | cache entry | entry age uses header receipt rather than construction time; subsecond delay/residence retained | verified by focused tests listed below |
| RFC 9111 §4 and §4.2: hits emit current Age and freshness is strict | MUST replace Age on unvalidated hits; freshness definition | cache hit/freshness decision | exact Age serialization and stale at equality; local peer request counts | verified by focused tests listed below |
| RFC 9111 §4.3.3–4 and §4.2.3: an identified response gets validation age metadata | 304 age/Date reconstruction only; validator eligibility remains blocked | 304 merge/cache | validation uses new exchange delay/Age, absent Age is zero; old representation body preserved | verified by focused tests listed below |
| RFC 9111 §§1.2.2 and 5.1: valid overflow never becomes zero; invalid Age is ignored; first list member selected by default | MUST saturation/invalid handling; SHOULD first-member choice | Age parser | legal oversized digits saturate; signed/fractional/invalid first member ignored | verified by focused tests listed below |
| [RFC 9110 §6.6.1](https://www.rfc-editor.org/rfc/rfc9110.html#section-6.6.1): recipient with clock records a missing Date when caching | MUST when a clocked recipient caches a response with absent Date | cache normalization | missing ordinary/304 Date becomes receipt wall time, future Date clamps apparent age to zero | verified by focused tests listed below |

Use the conservative RFC formula: maximum of apparent age and incoming Age plus
response delay, then add monotonic residence. This is an explicit permitted
choice even when corrected Age alone could be used.

## Phased Implementation

## Phase 1: Demonstrate the Failure (Completed)

**Files: `crates/openwire-cache/src/lib.rs`, `crates/openwire-cache/tests/integration.rs`**
- Added a fixed-vector regression: the old calculation produced 203s instead
  of the independently expected 205s. A second regression showed that a valid
  oversized Age became zero instead of saturating to 2^31 seconds. Both failed
  before the fix; their expected values were retained.

## Phase 2: Carry Timing and Correct Storage and Validation (Completed)

**Files: `crates/openwire-core/src/{transport,lib}.rs`, `crates/openwire/src/{lib,transport/service}.rs`**
- Added/re-exported ResponseTiming and attached metadata to each H1/H2 response.

**File: `crates/openwire-cache/src/lib.rs`**
- Consumes exchange metadata, normalizes missing Date, corrects age arithmetic
  and preserves receipt anchoring through normal storage and 304 freshening.
- Valid Age values that overflow saturate, invalid grammar is ignored, and the
  first list member is used (RFC 9111 §§1.2.2 and 5.1).
- Kept public CachedResponse::new as an immediately received manual entry with
  zero unknown transfer delay; automatic cache storage uses exchange metadata.

## Phase 3: Verify and Synchronize Documentation (Completed)

**Files: `crates/openwire-cache/tests/integration.rs`, `crates/openwire/tests/response_timing.rs`**
- Verified wire-produced timing, final-hop identity, cache freshness, emitted
  Age, validation, buffering and failure behavior without short timing sleeps.

**Files: `README.md`, `docs/cache-age.md`**
- Documented timing ownership, synthetic response behavior and exact evidence.
- Completed focused regressions, workspace default/all-feature tests, doctests,
  formatting and clippy; command results follow.

### Focused evidence

- `response_age_includes_request_delay_and_residence`: literal 205s, emitted
  Age=205 and stale exactly at lifetime=205.
- `response_age_vectors_choose_corrected_or_apparent_age`: Age-dominant,
  Date-dominant, future Date, absent fields and invalid Date.
- `buffering_before_storage_does_not_restart_response_age`: three seconds
  between receipt and storage remain in age, then later residence adds once.
- `response_age_keeps_fractional_delay_and_residence_until_serialization`:
  600ms delay + 600ms residence yields 1.2s internally and Age=1.
- `validation_age_uses_the_new_exchange_and_not_stored_date_or_age`: fresh
  validation Age/Date, absent Age/Date and an explicitly old validation Date;
  original body preserved. Scope is age reconstruction for an identified entry.
- `response_age_overflow_saturates_instead_of_becoming_zero` and
  `response_age_uses_first_list_member_and_rejects_invalid_values`: legal overflow,
  list ordering, invalid syntax and leading zeroes.
- Cache integrations verify emitted Age with an RFC-derived interval around
  its serialization instant, 304 refresh, no-timing fallback and no complete
  entry after a truncated wire body. A known delay is supplied at the network
  interceptor boundary; it is not a stopwatch assertion about network speed.
- `tests/response_timing.rs` separately proves actual H1 header receipt before
  gated body release, negotiated H2 metadata and final-hop redirect timing.

The original failing default workspace run exposed an unrelated existing trace
capture race: `tracing_attempt_spans_record_stable_connection_reuse_fields`
occasionally observes no spans under parallel tests. The same failure was
reproduced on detached origin/main; the isolated case passes. Final verification
also runs the complete default library/integration suite serially so this known
harness race cannot obscure Age results. It remains a separate test-support
issue, not an Age regression.

### Verification results

All results below are from macOS arm64 with Rust 1.99.1. Disabled incremental
compilation after clearing regenerable build caches when the local disk filled;
this changes build storage use, not the test assertions.

| Command | Result |
| --- | --- |
| `CARGO_INCREMENTAL=0 RUST_TEST_THREADS=1 cargo test --workspace --all-targets` | 579 passed, 0 failed, 16 ignored; benchmark/example targets completed |
| `CARGO_INCREMENTAL=0 RUST_TEST_THREADS=1 cargo test --workspace --all-targets --all-features` | 620 passed, 0 failed, 16 ignored; benchmark/example targets completed |
| `CARGO_INCREMENTAL=0 cargo test --workspace --all-features --doc` | 2 passed |
| `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo fmt --all --check` | passed |

New evidence comprises seven cache unit tests, five cache integrations and three
transport integrations. The parallel default run's existing tracing failure is
reported above rather than counted as a passing full-suite run. Desktop CI on
other operating systems remains separate evidence.

## Architectural Notes

- Additive response metadata; no changed connector/interceptor trait bounds.
- Network attempts own timestamps. Cache owns protocol arithmetic and policy.
- Metadata uses standard monotonic time consistently; custom runtime Timer
  clocks are not mixed with cache's std::Instant residence calculations.
- Missing trustworthy exchange metadata limits cache storage instead of silently
  assuming zero delay. This affects synthetic interceptor responses; attach
  ResponseTiming to opt into automatic storage.
- No new production dependency or global clock/test framework.
- This repair proves the age/Date cases listed above, not all cache conformance.
  In particular, 304 validator eligibility is outside this repair: the existing
  freshening path does not reject a mismatched strong ETag. That pre-existing
  §4.3.4 gap blocks a full validation-conformance claim and affected releases
  until separately fixed; passing age tests do not waive it.

## File Change Summary

- `crates/openwire-cache/src/lib.rs` -- receipt-based cache age and validation.
- `crates/openwire-cache/tests/integration.rs` -- cache and validation regressions.
- `crates/openwire-core/src/lib.rs` -- export timing metadata.
- `crates/openwire-core/src/transport.rs` -- exchange timing contract.
- `crates/openwire/src/lib.rs` -- re-export timing metadata.
- `crates/openwire/src/transport/service.rs` -- capture H1/H2 exchange timestamps.
- `crates/openwire/tests/response_timing.rs` -- transport timing evidence.
- `docs/cache-age.md` -- requirements and verification record.
- `README.md` -- cache timing and custom response guidance.
