# WS-04 — Tokio Sync Primitives

Date: 2026-03-23
PR: `PR4`
Branch: `refactor/tokio-sync-primitives`
Status: implemented

## Scope

- remove wakeup-list accumulation and full-vector `Waker` scans from limiter
  coordination
- replace custom async semaphore logic with Tokio primitives unless a hidden
  runtime-independence requirement is discovered

## Owned Files

- `crates/openwire/src/connection/limits.rs`
- limiter-related integration tests or focused unit tests
- any perf harness or benchmark added for contention checks
- `docs/DESIGN.md`

## Locked Decisions

- `ConnectionAvailability` is replaced with `tokio::sync::Notify`.
- Availability notifications use `notify_waiters()` to preserve broadcast
  semantics.
- Preferred path: replace custom `AsyncSemaphore` with `tokio::sync::Semaphore`.
- Fall back to a custom FIFO waiter queue only if implementation review uncovers
  a concrete incompatibility with Tokio semaphore semantics.
- The PR must preserve current limiter correctness before attempting perf
  tuning.

## Implementation Steps

1. Replace the custom availability waker list with `Notify`.
2. Prototype the `AsyncSemaphore -> tokio::sync::Semaphore` swap and adapt the
   owned-permit plumbing.
3. Re-run correctness tests covering request admission and connection admission.
4. Add a focused contention check or benchmark to confirm the new wakeup path
   eliminates the current wake-all + linear-scan debt.
5. Update design docs to say limiter waiting uses Tokio primitives.

## Verification

- Existing request-limit and connection-limit tests continue to pass.
- Add coverage that waiting tasks are released correctly after permits return.
- Add a contention-oriented benchmark or narrow perf harness that exercises many
  waiters and validates the removal of custom-waker overhead.

## Non-goals

- do not redesign limiter policy semantics
- do not mix in connection-pool or transport timeout work

## Docs To Update On Merge

- `docs/DESIGN.md`
