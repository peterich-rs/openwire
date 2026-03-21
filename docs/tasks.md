# OpenWire Execution Tasks

Date: 2026-03-21

This file tracks only active or intentionally deferred work.

Historical migration phases and completed task tables were removed after the
owned connection-core baseline landed on `main`. Use git history when the exact
implementation sequence matters.

## Current Status

- the previously tracked execution slice is complete on `main`
- there are no active `IN_PROGRESS` tasks in this file
- `DESIGN.md` and the codebase describe the current baseline

## Active Tasks

None.

## Deferred Themes

| ID | Status | Theme | Notes |
|---|---|---|---|
| FUT-001 | `DEFERRED` | Proxy-route fast fallback | Direct routes race resolved target addresses today; proxy routes still dial sequentially. |
| FUT-002 | `DEFERRED` | Background pool maintenance | HTTP/1.1 idle enforcement happens on pool touch points; there is no sweeper task yet. |
| FUT-003 | `DEFERRED` | Negotiated HTTP/2 reuse limits | Pool reuse still uses a conservative fixed stream cap instead of peer settings. |
| FUT-004 | `DEFERRED` | Broader cache semantics | `openwire-cache` exists, but current caching remains intentionally narrow and application-layer. |
| FUT-005 | `DEFERRED` | Additional mobile/runtime hardening | Keep mobile and cross-platform constraints explicit as runtime ownership grows. |

## Maintenance Rules

- add a row only for accepted work that is still active or intentionally deferred
- remove items that are no longer relevant instead of preserving stale history
- keep this file short enough to read in one pass
