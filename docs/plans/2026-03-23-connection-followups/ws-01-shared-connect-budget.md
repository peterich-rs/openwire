# WS-01 — Shared Connect Budget

Date: 2026-03-23
PR: `PR1`
Branch: `feature/shared-connect-budget`
Status: implemented

## Scope

- fix repeated use of full `connect_timeout` across proxy TCP dial, CONNECT
  handshake, CONNECT 407 retry re-dial, and SOCKS5 sub-steps
- preserve stage-specific timeout classification and error strings
- keep current no-timeout semantics when both `connect_timeout` and
  `CallContext::deadline()` are absent

## Owned Files

- `crates/openwire/src/transport/connect.rs`
- `crates/openwire/src/connection/fast_fallback.rs`
- `crates/openwire/tests/integration.rs`
- `docs/DESIGN.md`

## Locked Decisions

- Introduce `ConnectBudget { started_at, connect_timeout, call_deadline }`.
- `ConnectBudget::new(connect_timeout, call_deadline)` snapshots `started_at`
  once and stores the raw timeout plus optional call deadline.
- `ConnectBudget::remaining()` returns `Option<Duration>` by subtracting
  elapsed time from `connect_timeout`, comparing that with the remaining call
  deadline when present, and taking the earlier budget.
- `FastFallbackDialer::dial_route_plan` is generalized over an intermediate
  connect result type `I`.
- Direct and forward-proxy paths use `I = BoxConnection`.
- CONNECT and SOCKS proxy paths use `I = (BoxConnection, ConnectBudget)`.
- `ConnectTunnelParams` stores `budget: ConnectBudget` instead of raw
  `connect_timeout`.
- `ConnectBudget` itself does not manufacture errors. Callers pass
  `budget.remaining()` into existing timeout wrappers so the active stage still
  controls the timeout error wording.

## Implementation Steps

1. Add `ConnectBudget` near the transport timeout helpers.
2. Generalize the fast-fallback connect/finalize handoff so proxy paths can
   pass budget from connect to finalize without side channels.
3. Thread budget through `connect_via_http_proxy`, `connect_via_socks_proxy`,
   `establish_connect_tunnel`, and all SOCKS5 stage helpers.
4. Replace every reused raw `connect_timeout` in proxy tunnel code with
   `budget.remaining()`.
5. Verify that direct and forward-proxy behavior is unchanged.
6. Update design docs to say proxy connection establishment uses a single
   shared connect budget per route attempt.

## Verification

- Existing proxy and tunnel integration tests still pass.
- Add a CONNECT case where 407-triggered re-dial plus CONNECT response read must
  fail within one shared budget.
- Add a SOCKS5 case where multiple handshake stages cumulatively exhaust one
  shared budget.
- `ConnectBudget::new(None, None)` preserves the existing no-timeout behavior.
- `ConnectBudget::new(Some(Duration::MAX), None)` does not panic.

## Non-goals

- do not change TLS handshake timeout semantics outside the shared connect
  budget path
- do not refactor unrelated transport structure or split `transport.rs`

## Docs To Update On Merge

- `docs/DESIGN.md`
