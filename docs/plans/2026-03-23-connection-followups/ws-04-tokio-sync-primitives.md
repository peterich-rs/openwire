# WS-04 — Tokio Sync Primitives

Date: 2026-03-23
PR: `PR4`
Branch: `refactor/tokio-sync-primitives`
Status: implemented

## Scope

- replace custom permit accounting with Tokio primitives while preserving
  concurrent `poll_ready()` correctness
- replace the connection-availability waker list with `tokio::sync::Notify`

## Owned Files

- `crates/openwire/src/connection/limits.rs`
- limiter-related integration tests or focused unit tests
- any perf harness or benchmark added for contention checks
- `docs/DESIGN.md`

## Locked Decisions

- `ConnectionAvailability` is replaced with `tokio::sync::Notify`.
- Availability notifications use `notify_waiters()` to preserve broadcast
  semantics.
- `AsyncSemaphore` delegates owned permit acquisition to
  `tokio::sync::Semaphore`.
- Shared readiness polling does not use a shared `PollSemaphore`; it keeps a
  local deduplicated waiter list so concurrent callers do not lose wakeups.
- The PR must preserve current limiter correctness before attempting perf
  tuning.

## Implementation Steps

1. Replace the custom availability waker list with `Notify`.
2. Replace custom permit accounting with Tokio owned permits while keeping a
   local readiness waiter list for shared `poll_ready()` call sites.
3. Add regression coverage for:
   - concurrent `poll_ready()` waiters on the same limiter
   - multiple concurrent request-admission waiters
   - `listen()` notifications that happen after `listen()` but before first poll
4. Re-run correctness tests covering request admission and connection admission.
5. Update design docs to describe the hybrid implementation accurately.

## Verification

- Existing request-limit and connection-limit tests continue to pass.
- Add coverage that waiting tasks are released correctly after permits return.
- Add coverage that concurrent `poll_ready()` callers all wake after a permit is
  released.
- Add coverage that `ConnectionAvailability::listen()` observes notifications
  sent before first poll.

## Non-goals

- do not redesign limiter policy semantics
- do not mix in connection-pool or transport timeout work

## Docs To Update On Merge

- `docs/DESIGN.md`
