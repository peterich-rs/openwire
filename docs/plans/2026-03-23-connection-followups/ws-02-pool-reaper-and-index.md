# WS-02 — Pool Reaper And Reverse Index

Date: 2026-03-23
PR: `PR2`
Branch: `refactor/pool-reaper-and-by-id-index`
Status: implemented

## Scope

- add a `ConnectionId -> Address` reverse index to eliminate full-pool scans in
  `ConnectionPool::remove`
- add whole-pool pruning so idle addresses can be reaped without future traffic
- add a background idle reaper with explicit lifecycle ownership

## Owned Files

- `crates/openwire/src/connection/pool.rs`
- `crates/openwire/src/client.rs`
- tests under `crates/openwire/src/connection/pool.rs`
- `docs/DESIGN.md`

## Locked Decisions

- `PoolState` gains `by_id: HashMap<ConnectionId, Address>`.
- Reverse-index writes happen in `insert`.
- Reverse-index deletes happen in explicit `remove` and in the centralized
  removed-connection loop inside `prune_address`.
- Add `prune_all()` on `ConnectionPool` that collects address keys first, then
  calls `prune_address` per key.
- Background reaper holds `Weak<ConnectionPool>` and exits when upgrade fails.
- The client retains an abort-capable task handle for the reaper and aborts it
  on final client drop.
- Reaper start is lazy: the client installs a pool-insert hook, and the first
  successful pooled insert starts the background task only once.
- Reaper starts only when `idle_timeout.is_some()`.
- Reaper cadence is `(idle_timeout / 2).clamp(5s, 60s)`.
- Reaper uses the same `prune_address` / `prune_all` path as foreground pool
  accesses so coalescing-index cleanup remains centralized.

## Implementation Steps

1. Extend `PoolState` with the reverse index and update insert/remove paths.
2. Add `prune_all()` and keep all index cleanup in the shared prune path.
3. Add a small reaper task entrypoint that loops on the configured cadence,
   upgrades a `Weak<ConnectionPool>`, prunes, and exits cleanly when the pool is
   gone.
4. Attach the reaper lifecycle to pooled-insert start and final client drop.
5. Update docs to distinguish eager foreground prune from best-effort
   background idle reaping.

## Verification

- Add unit tests for reverse-index consistency across `insert`, `remove`, and
  prune-driven eviction.
- Add a test that `prune_all()` removes expired idle connections and keeps the
  coalescing index in sync.
- Add a test that an idle address can be reaped without any later traffic on
  that address.
- Add a lifecycle test that dropping the final client aborts the reaper task.

## Non-goals

- do not redesign pool bucket layout beyond the reverse index
- do not change HTTP/2 coalescing rules
- do not fold `WS-03` request-extension caching into this PR

## Docs To Update On Merge

- `docs/DESIGN.md`
