# Transport Core Connection Establishment Fixes

Date: 2026-03-24
Status: Implemented

This document records the implemented fixes for the three transport/core
connection-establishment gaps called out in review. The changes preserve the
existing layering:

```text
TransportService
  -> ConnectionPool / ConnectionLimiter reuse decision
  -> ConnectorStack route planning
  -> FastFallbackDialer TCP / tunnel / TLS establishment
  -> protocol binding
```

Issue 1 [P2]: Tolerate DNS failure when a busy pooled connection exists

Files:

- `crates/openwire/src/transport/service.rs`
- `crates/openwire/src/connection/limits.rs`
- `crates/openwire/src/connection/pool.rs`

Problem: `route_plan()` (DNS) currently runs before the connection-limiter
decision in `acquire_connection`. When a same-address pooled connection already
exists but is temporarily in use, and the global/per-host connection limit does
not allow opening another socket, a transient DNS failure aborts the request
even though the already-open keep-alive connection could satisfy it once
released.

Implementation summary — DNS failure is now recoverable only when a fresh
connection cannot be opened, while keeping coalesced-before-permit intact:

- `try_acquire_coalesced` must stay before permit consumption. It reuses an
  existing H2 connection and depends on `RoutePlan` for `route_overlap`
  validation, so moving it behind the permit would regress "limit full but a
  coalesced H2 connection is available".
- `pool.acquire()` only returns a connection when `try_acquire()` succeeds. A
  busy pooled connection therefore usually appears as `None`, not as
  `BindingAcquireResult::Busy`. Relying only on the binding path misses the
  main failure scenario.
- Add a non-consuming `ConnectionLimiter::can_acquire(&Address) -> bool`
  helper. Do not probe with `try_acquire(...).is_some()` just to inspect
  availability; that would briefly take and drop a permit and create avoidable
  wakeups. Document it as a heuristic because permit availability can change
  after the check.
- Add a lightweight pooled-state helper such as
  `ConnectionPool::has_in_use_connection(&Address) -> bool` that reuses the
  pool's pruning logic and returns whether the address currently has any
  healthy, in-use pooled connection.
- When the same loop iteration already touched the pool for same-address reuse,
  take the reusable connection candidate and the "has in-use connection" hint
  from the same pruned snapshot, excluding the selected candidate from the
  hint, so the address is not pruned twice before DNS and stale candidates do
  not self-report as waitable pooled work.
- When DNS failure is suppressed into the pooled-wait fallback, emit a
  `tracing::debug!` event with the call id, target host/port, and DNS error
  details so the degraded path is visible in production logs.

Restructured `acquire_connection` logic:

```rust
loop {
    let wait_for_availability = self.connection_availability.listen();
    let mut waitable_pooled_connection = false;
    let mut pooled_in_use_hint = None;

    // Step 1: exact reserved connection or same-address pooled reuse.
    let connection = match exact_candidate.take() {
        Some(connection) => Some(connection),
        None => {
            let (connection, has_in_use) = self
                .exchange_finder
                .pool()
                .acquire_with_in_use_hint(prepared.address());
            pooled_in_use_hint = connection.is_none().then_some(has_in_use);
            connection
        }
    };
    if let Some(connection) = connection {
        match self.bindings.acquire(connection.id()) {
            BindingAcquireResult::Acquired(binding) => return Ok(...),
            BindingAcquireResult::Busy => {
                let _ = self.exchange_finder.release(&connection);
                waitable_pooled_connection = true;
            }
            BindingAcquireResult::Stale => {
                let _ = self.exchange_finder.pool().remove(connection.id());
                self.connection_availability.notify();
            }
        }
    }

    if !waitable_pooled_connection {
        waitable_pooled_connection = pooled_in_use_hint.unwrap_or_else(|| {
            self.exchange_finder
                .pool()
                .has_in_use_connection(prepared.address())
        });
    }

    // Step 2: route planning stays before coalesced, but DNS failure is only
    // converted into a wait when pooled reuse is plausible and fresh connect is
    // currently blocked by the limiter.
    if route_plan.is_none() {
        match self.connector.route_plan(ctx.clone(), prepared.address()).await {
            Ok(plan) => route_plan = Some(plan),
            Err(error)
                if error.kind() == WireErrorKind::Dns
                    && waitable_pooled_connection
                    && !self.connection_limiter.can_acquire(prepared.address()) =>
            {
                tracing::debug!(...);
                wait_for_availability.await;
                continue;
            }
            Err(error) => return Err(error),
        }
    }

    // Step 3: coalesced stays before permit consumption.
    if let Some(selected) = self.try_acquire_coalesced(
        prepared.address(),
        route_plan.as_ref().expect("route plan"),
    ) {
        return Ok(selected);
    }

    // Step 4: only now do we need a fresh-connection permit.
    let Some(connection_permit) = self
        .connection_limiter
        .try_acquire(prepared.address().clone())
    else {
        wait_for_availability.await;
        continue;
    };

    // Step 5: bind a fresh connection.
    return self
        .bind_fresh_connection(
            prepared,
            request,
            ctx,
            span,
            route_plan.take().expect("route plan"),
            connection_permit,
        )
        .await;
}
```

Key details:

- DNS errors are only suppressed when both of these are true:
  - a same-address pooled connection is already in use and could become
    reusable
  - the limiter says a fresh connection cannot be opened right now
- Non-DNS `route_plan` failures still surface immediately. Route-planner
  misconfiguration and internal planning errors must not be converted into a
  pooled-wait fallback.
- If the limiter can admit a new connection, surface the DNS error. In that
  case the request was legitimately on the fresh-connect path.
- This preserves the current coalesced fast path and only changes the failure
  behavior for the "busy pooled reuse + no permit" corner case.

Test changes:

- Add an integration regression test next to
  `connection_limit_per_host_waits_for_existing_connection_to_become_reusable`.
- Use a scripted resolver that succeeds on the first lookup and fails on the
  second lookup for the same host.
- Open a first request and keep its response body unreleased so the pooled
  connection remains busy.
- Run a second request with `max_connections_per_host(1)`.
- Assert the second request does not fail on DNS, waits for the first response
  to release, then succeeds on the reused connection.
- Assert the TCP connector still records only one connect attempt.


Issue 2 [P1]: Parallelize finalize across fast-fallback routes

File: `crates/openwire/src/connection/fast_fallback.rs`

Problem: `finalize(...)` is currently awaited inline in the single controller
loop. While one route is performing expensive post-TCP establishment work
(`CONNECT`, `SOCKS`, target TLS), later routes that already completed TCP sit
queued in the channel instead of progressing in parallel. For proxy routes this
degrades fast-fallback into serialized failover.

Implementation summary — `finalize` now runs inside each spawned route task:

Each route already runs inside its own executor task. Extend that task to run
the full connect-plus-finalize pipeline and send the final
`Result<BoxConnection, WireError>` back to the controller:

```rust
let connect_result = connect(ctx.clone(), attempt.route().clone()).await;
let result = match connect_result {
    Ok(intermediate) => {
        finalize(
            ctx,
            uri,
            attempt.route().clone(),
            intermediate,
        )
        .await
    }
    Err(error) => Err(error),
};
let _ = tx.unbounded_send(FastFallbackMessage::Finished {
    route_index: index,
    result,
});
```

Structural changes:

- `FastFallbackMessage` loses its `<I>` generic. `Finished` carries
  `Result<BoxConnection, WireError>` directly.
- `cleanup_losers` loses its `<I>` generic and drops completed loser
  `BoxConnection` values instead of intermediate streams.
- Each spawned task captures `uri.clone()` and `finalize.clone()` and completes
  the full establishment path end-to-end.
- The controller loop no longer calls `finalize(...)` inline. `Finished(Ok())`
  means the route is fully established and can win immediately.
- Winner and fatal-error return paths no longer wait for loser task teardown.
  Loser cleanup happens asynchronously after cancellation so a stalled loser
  cannot delay the winner response or fatal error propagation.
- `Finished(Err(error))` must classify failures with `failure_stage(&error)`.
  After this refactor, the error may come from TCP, proxy tunnel, or TLS. Do
  not keep the current TCP-only handling in that branch.
- `connect_race_lost` now fires for every failed route outcome that reaches the
  controller, including TCP establishment failures, instead of only finalize
  failures. Downstream listeners and metrics must treat it as a generic
  fast-fallback loss signal across TCP, tunnel, and TLS stages.
- `dial_route_plan` remains generic over the connect intermediate type `I`,
  because `connect` still returns `I` and `finalize` still consumes it inside
  the route task.
- `dial_direct` and the existing callers in `connect.rs` keep the same closure
  shapes; only the internal control flow changes.

Test changes:

- Update
  `cleanup_drops_completed_loser_streams_after_winner_is_selected` so the TLS
  script vector has a second entry and the TLS call assertion allows either one
  or two TLS starts:

  ```rust
  // Loser may have started TLS before abort; 1 or 2 calls are both valid.
  assert!(tls.calls.load(Ordering::Relaxed) >= 1);
  assert!(tls.calls.load(Ordering::Relaxed) <= 2);
  ```

- Re-verify `non_retryable_tls_failure_stops_fast_fallback_continuation`. The
  controller must still stop on the non-retryable TLS failure, but route 2 may
  already have entered TLS on some schedulers before the failure is observed,
  so allow either one or two TLS starts.
- Keep the existing
  `connect_proxy_fast_fallback_continues_after_proxy_tunnel_failure`
  integration test.
- Add a second proxy-focused regression test that covers the actual reported
  stall:
  - route 1 uses `spawn_stalling_connect_proxy()` and never completes the
    CONNECT response
  - route 2 uses `spawn_connect_proxy()` and has a `200ms` TCP stagger
  - assert the overall request succeeds within a short timeout such as `2s`
  - this proves route 2's CONNECT/tunnel/TLS establishment progressed in
    parallel instead of waiting for route 1's stalled finalization


Issue 3 [P2]: Apply URL-embedded credentials on CONNECT proxy requests

Files:

- `Cargo.toml`
- `crates/openwire/Cargo.toml`
- `crates/openwire/src/proxy.rs`
- `crates/openwire/src/transport/connect.rs`

Problem: `connect_via_http_proxy` currently destructures
`RouteKind::ConnectProxy { proxy, .. }` and drops the parsed credentials.
`establish_connect_tunnel` starts with an empty CONNECT header map and only
consults `proxy_authenticator` after a `407`. As a result, URL-embedded
credentials such as `Proxy::https("http://user:pass@proxy...")` are not sent on
the initial CONNECT request.

Implementation summary — CONNECT now pre-seeds Basic auth from
`ProxyCredentials`, while leaving the challenge-response authenticator path
unchanged:

Add dependency:

```toml
# Cargo.toml
[workspace.dependencies]
base64 = "0.22"

# crates/openwire/Cargo.toml
[dependencies]
base64.workspace = true
```

Add a helper on `ProxyCredentials`:

```rust
pub(crate) fn basic_auth_header_value(&self) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", self.username, self.password));
    format!("Basic {encoded}")
}
```

Extend `ConnectTunnelParams`:

```rust
pub(super) initial_proxy_credentials: Option<ProxyCredentials>,
```

Seed CONNECT headers before the tunnel loop:

```rust
let mut connect_headers = HeaderMap::new();
if let Some(creds) = &params.initial_proxy_credentials {
    let value = HeaderValue::from_str(&creds.basic_auth_header_value())
        .expect("proxy basic auth header should be valid ASCII");
    connect_headers.insert(http::header::PROXY_AUTHORIZATION, value);
}
```

Extract and pass credentials from the route in `connect_via_http_proxy`:

```rust
let (proxy_addr, credentials) = match route.kind() {
    RouteKind::ConnectProxy { proxy, credentials } => (*proxy, credentials.clone()),
    other => { ... }
};

establish_connect_tunnel(ConnectTunnelParams {
    ctx: ctx.clone(),
    proxy_addr,
    target_uri: &target_uri,
    stream,
    tcp_connector: deps.tcp_connector.clone(),
    initial_proxy_credentials: credentials,
    proxy_authenticator: deps.proxy_authenticator.clone(),
    max_proxy_auth_attempts: deps.max_proxy_auth_attempts,
    budget,
    timer: deps.timer.clone(),
})
```

Key details:

- The initial CONNECT now uses preemptive proxy auth when credentials were
  embedded in the proxy URL.
- The `proxy_authenticator` path remains in place for subsequent `407`
  challenge/response retries.
- `sanitize_connect_headers(request.headers())` continues to replace the
  CONNECT header set on retry, so authenticated follow-up behavior remains
  consistent with the current challenge-response path.

Test changes:

- Add an integration test for URL-embedded CONNECT credentials:
  - configure
    `Proxy::https("http://user:pass@proxy.test:<port>")`
  - do not install a `proxy_authenticator`
  - use `spawn_connect_proxy_requiring_authorization(...)`
  - assert the request succeeds
  - assert the proxy only received one CONNECT request
  - assert that first CONNECT request already contains
    `proxy-authorization: Basic dXNlcjpwYXNz`
