# OpenWire Review Remediation Implementation Design

Date: 2026-03-22
Status: Draft implementation spec

This document is the code-level implementation plan for the review findings
raised on 2026-03-22. It is intentionally concrete: internal types, file-level
changes, state transitions, sequencing, and deterministic test coverage.

`docs/DESIGN.md` remains the source of truth for the code that exists today.
This file describes the target implementation needed to close the reviewed
gaps. The execution tracker for this design lives in
`docs/REVIEW_REMEDIATION_TASKS.md`.

## 1. Change Set

This implementation plan covers these findings:

1. CONNECT parsing assumes the whole buffer is UTF-8
2. Redirect follow-ups scrub the wrong headers
3. Abandoned HTTP/2 bodies poison the whole connection
4. HTTP/2 connections enter the pool before binding exists
5. `retry_canceled_requests` is a dead config knob
6. `call_timeout` hardcodes Tokio and stops at headers

## 2. Constraints

- do not bypass the canonical execution chain
- keep policy decisions in `FollowUpPolicyService`
- keep connection acquisition and protocol binding in `TransportService`
- do not add public API surface unless the existing API cannot express the fix
- preserve current event ordering unless a finding explicitly requires a change
- prefer deterministic local tests over behavior that depends on public networks

## 3. Affected Files

| File | Change |
|---|---|
| `crates/openwire/src/client.rs` | move canceled-retry config to retry policy, remove Tokio-specific timeout call, pass runtime into body wrapper |
| `crates/openwire/src/policy/follow_up.rs` | canonical origin comparison, proxy-aware redirect header policy |
| `crates/openwire/src/proxy.rs` | add reusable internal proxy selector |
| `crates/openwire/src/transport.rs` | CONNECT parser split, prefetched tunnel bytes, body deadline enforcement, h2 abandonment semantics, fresh-binding publish order |
| `crates/openwire/src/connection/pool.rs` | prune unhealthy connections on normal pool touch points |
| `crates/openwire/src/connection/real_connection.rs` | add reusable/healthy helpers used by pool and transport |
| `crates/openwire/tests/integration.rs` | new deterministic cases for each finding |
| `README.md` | doc link label only |
| `docs/DESIGN.md` | doc inventory only |

No code changes are planned in:

- `openwire-rustls`
- `openwire-cache`
- `openwire-core` public traits

The only exception is use of the existing `Runtime::sleep(Duration)` contract.

## 4. Internal Types To Add

## 4.1 Proxy selection

Add an internal selector in `crates/openwire/src/proxy.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProxyRuleId(pub(crate) usize);

#[derive(Clone, Debug)]
pub(crate) struct ProxySelector {
    rules: Arc<[Proxy]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProxySelection {
    pub(crate) id: ProxyRuleId,
}
```

Methods:

```rust
impl ProxySelector {
    pub(crate) fn new(rules: Vec<Proxy>) -> Self;
    pub(crate) fn first_match(&self, uri: &Uri) -> Option<(&Proxy, ProxySelection)>;
    pub(crate) fn selection_for(&self, uri: &Uri) -> Option<ProxySelection>;
}
```

Purpose:

- `ExchangeFinder` uses `first_match()` to build `Address`
- `FollowUpPolicyService` uses `selection_for()` to decide whether
  `Proxy-Authorization` survives a redirect

This removes duplicate “which proxy would be used for this URI” logic while
keeping actual transport routing inside `TransportService`.

## 4.2 Canonical origin key

Add to `crates/openwire/src/policy/follow_up.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
struct OriginKey {
    scheme: &'static str,
    host: String,
    port: u16,
}
```

Methods:

```rust
impl OriginKey {
    fn from_uri(uri: &Uri) -> Result<Self, WireError>;
}

fn same_origin(left: &Uri, right: &Uri) -> Result<bool, WireError>;
```

Rules:

- host normalized to lowercase
- scheme must be `http` or `https`
- port is explicit port or `80/443` default

This replaces raw `authority()` equality checks.

## 4.3 CONNECT parsing output

Add to `crates/openwire/src/transport.rs`:

```rust
struct ParsedConnectResponse {
    head: ConnectResponseHead,
    buffered_tail: Bytes,
}
```

`read_connect_response[_inner]()` returns `ParsedConnectResponse` instead of
just `ConnectResponseHead`.

## 4.4 Prefetched connection wrapper

Add to `crates/openwire/src/transport.rs`:

```rust
struct PrefetchedConnection {
    prefix: Bytes,
    offset: usize,
    inner: BoxConnection,
}
```

Behavior:

- `poll_read()` drains `prefix[offset..]` before delegating to `inner`
- `poll_write*()` and `connected()` delegate directly

Use:

- only on successful CONNECT where bytes after `\r\n\r\n` were already read

## 4.5 Runtime deadline helper

Add internal helper in `crates/openwire/src/client.rs` or a private module next
to it:

```rust
async fn with_call_deadline<T, F>(
    runtime: Arc<dyn Runtime>,
    deadline: Option<Instant>,
    future: F,
) -> Result<T, WireError>
where
    F: Future<Output = Result<T, WireError>>;
```

Implementation:

- no `tokio::time::timeout`
- use `runtime.sleep(remaining)` and `futures_util::future::select`

The helper is reused by `Call::execute()`. Body polling will use a pinned sleep
future directly inside `ObservedIncomingBody`.

## 5. Detailed Implementation

## 5.1 Redirect header policy

### 5.1.1 Constructor changes

`ClientBuilder::build()` currently creates:

- `ConnectionPool`
- `ExchangeFinder`
- `ConnectorStack`
- `FollowUpPolicyService`

Change build flow to create one shared `ProxySelector` first:

```rust
let proxy_selector = Arc::new(ProxySelector::new(proxies.clone()));
let exchange_finder = Arc::new(ExchangeFinder::new(pool, proxy_selector.clone()));
let service = build_service_chain(
    transport,
    application_interceptors,
    network_interceptors,
    policy,
    proxy_selector,
);
```

`build_service_chain()` and `FollowUpPolicyService::new(...)` gain the selector.

### 5.1.2 Redirect rebuild algorithm

Replace current redirect header stripping logic with:

```text
prev_origin = OriginKey::from_uri(snapshot.uri)
next_origin = OriginKey::from_uri(next_uri)
same_origin = prev_origin == next_origin

prev_proxy = proxy_selector.selection_for(snapshot.uri)
next_proxy = proxy_selector.selection_for(next_uri)
same_proxy = prev_proxy == next_proxy

headers.remove(HOST)

if !same_origin:
  headers.remove(AUTHORIZATION)
  headers.remove(COOKIE)

if !same_proxy:
  headers.remove(PROXY_AUTHORIZATION)

if method_switches_to_get:
  headers.remove(CONTENT_LENGTH)
  headers.remove(CONTENT_TYPE)
```

This is the only redirect carry-over logic. No other code path should
independently rewrite these headers.

### 5.1.3 File-level edits

`crates/openwire/src/policy/follow_up.rs`

- remove `same_authority()`
- add `OriginKey`
- update `RequestSnapshot::into_redirect_request(...)` signature to accept
  `proxy_selector: &ProxySelector`
- remove `Proxy-Authorization` from the cross-origin branch
- explicitly remove `Cookie` when `same_origin == false`

`crates/openwire/src/client.rs`

- thread `Arc<ProxySelector>` into `build_service_chain(...)`

### 5.1.4 Tests to add

Add to `crates/openwire/tests/integration.rs`:

- `redirect_same_origin_default_port_preserves_authorization`
- `redirect_cross_origin_drops_explicit_cookie_header`
- `redirect_same_proxy_preserves_proxy_authorization`
- `redirect_different_proxy_drops_proxy_authorization`

## 5.2 CONNECT parser and buffered bytes

### 5.2.1 Split header block before text parsing

Current problem:

- we stop reading after `\r\n\r\n`
- then pass the entire byte vector to `parse_connect_response_head()`

New flow:

```rust
fn split_connect_head(buf: &[u8]) -> Result<(&[u8], &[u8]), WireError>;
fn parse_connect_response_head(head_bytes: &[u8]) -> Result<ConnectResponseHead, WireError>;
```

`split_connect_head()`:

- finds first `\r\n\r\n`
- returns `(head_bytes_without_terminator, remaining_bytes_after_terminator)`
- errors if terminator is missing

`parse_connect_response_head()`:

- validates UTF-8 only for `head_bytes`
- parses status line and headers exactly as today

### 5.2.2 Preserve tail bytes on 200

`send_connect_request()` changes from:

```rust
let head = read_connect_response(...)?;
if head.status == StatusCode::OK { ... }
```

to:

```rust
let parsed = read_connect_response(...)?;
if parsed.head.status == StatusCode::OK {
    let stream = stream.into_inner();
    let stream = if parsed.buffered_tail.is_empty() {
        stream
    } else {
        Box::new(PrefetchedConnection::new(parsed.buffered_tail, stream)) as BoxConnection
    };
    return Ok(ConnectTunnelOutcome::Established(stream));
}
```

### 5.2.3 File-level edits

`crates/openwire/src/transport.rs`

- add `ParsedConnectResponse`
- add `split_connect_head`
- change `read_connect_response[_inner]` return type
- change `parse_connect_response_head` input type to `&[u8]`
- add `PrefetchedConnection`

### 5.2.4 Tests to add

- `connect_proxy_407_with_body_in_same_read_is_parsed`
- `connect_proxy_200_with_prefetched_tail_preserves_tunnel_bytes`

## 5.3 HTTP/2 response-body abandonment

### 5.3.1 Required behavior

We only need one semantic change for this finding:

- body drop without read error is a stream-local cancel for HTTP/2

Do not infer “whole session unhealthy” from drop alone.

### 5.3.2 `ObservedIncomingBody` state table

Keep `finish_successfully()` and `finish_with_error()` split as they are.
Change only `finish_abandoned()` behavior for the HTTP/2 branch.

Target behavior:

| Finish path | HTTP/1.1 | HTTP/2 |
|---|---|---|
| success | return sender, release connection | release one allocation |
| body error | remove binding, drop connection | mark unhealthy, release allocation |
| drop before EOF | remove binding, drop connection | release allocation only |

Implementation in `discard_connection()`:

- keep current HTTP/1.1 behavior
- split HTTP/2 branch into two helpers:
  - `release_http2_stream(connection)` for drop
  - `discard_http2_connection(connection)` for connection-level failure

Concretely:

- `finish_abandoned()` must not call `mark_unhealthy()` for HTTP/2
- `finish_with_error()` keeps the unhealthy path for HTTP/2

### 5.3.3 Pool pruning

The pool currently prunes only:

- `Closed`
- timed-out idle HTTP/1.1

Add helper on `RealConnection`:

```rust
impl RealConnection {
    pub(crate) fn is_healthy(&self) -> bool;
}
```

`prune_connections()` must drop entries where `!connection.is_healthy()`.

This is enough to prevent abandoned h2 follow-ups from leaving dead pool
entries around once any path still marks a connection unhealthy.

### 5.3.4 File-level edits

`crates/openwire/src/transport.rs`

- split HTTP/2 drop path from error path
- rename internal helpers if needed so “abandoned” and “errored” are distinct

`crates/openwire/src/connection/real_connection.rs`

- add `is_healthy()`

`crates/openwire/src/connection/pool.rs`

- prune non-healthy entries

### 5.3.5 Tests to add

- `http2_redirect_does_not_poison_connection`
- `http2_auth_followup_does_not_poison_connection`
- `dropping_http2_body_releases_stream_without_discarding_session`

## 5.4 Fresh HTTP/2 publish ordering

### 5.4.1 Required invariant

Pool visibility must happen strictly after binding visibility.

The correct sequence in `bind_fresh_connection()` is:

```text
connect socket
  -> derive info / protocol
  -> create RealConnection with allocations = 1
  -> bind protocol
  -> insert binding into ConnectionBindings
  -> acquire binding for the current request
  -> insert RealConnection into pool
  -> spawn background connection task
  -> return SelectedConnection
```

The current request must not reacquire from the pool. It already owns the
binding produced by the fresh bind path.

### 5.4.2 Concrete sequence

Pseudo-code:

```rust
let connection = RealConnection::with_id_and_coalescing(...);
assert!(connection.try_acquire());

match protocol {
    Http1 => {
        let (sender, task) = bind_http1(stream).await?;
        self.bindings.insert_http1(info.id, info.clone(), sender);
        let binding = self.bindings.acquire(info.id).expect("fresh http1 binding");
        self.exchange_finder.pool().insert(connection.clone());
        self.spawn_http1_task(connection.clone(), task, span)?;
        return Ok(SelectedConnection { connection, binding, reused: false });
    }
    Http2 => {
        let (sender, task) = bind_http2(stream, &self.config).await?;
        self.bindings.insert_http2(info.id, info.clone(), sender);
        let binding = self.bindings.acquire(info.id).expect("fresh http2 binding");
        self.exchange_finder.pool().insert(connection.clone());
        self.spawn_http2_task(connection.clone(), task, span)?;
        return Ok(SelectedConnection { connection, binding, reused: false });
    }
}
```

Failure properties:

- if `bind_http1/2()` fails, nothing has been inserted into pool/bindings
- if `spawn_http1/2_task()` fails, remove binding and do not publish pool entry

### 5.4.3 File-level edits

`crates/openwire/src/transport.rs`

- reorder `bind_fresh_connection()`
- remove the post-insert “freshly bound connection was not available” branch,
  because the current request should not round-trip through pool publication

### 5.4.4 Tests to add

- `fresh_http2_connection_is_not_visible_before_binding`
- `http2_bind_failure_leaves_no_pool_entry`

## 5.5 `retry_canceled_requests`

### 5.5.1 Config ownership

Move the field:

- from `TransportConfig`
- to `RetryPolicyConfig`

No public API rename:

```rust
pub fn retry_canceled_requests(mut self, enabled: bool) -> Self {
    self.policy.retry.retry_canceled_requests = enabled;
    self
}
```

### 5.5.2 Retry classification

Change:

```rust
fn retry_reason(error: &WireError) -> Option<&'static str>
```

to:

```rust
fn retry_reason(error: &WireError, config: &RetryPolicyConfig) -> Option<&'static str>
```

Add branch:

```rust
WireErrorKind::Canceled if config.retry_canceled_requests => Some("canceled")
```

Do not retry canceled requests when:

- the flag is false
- the request body is not replayable
- retry budget is exhausted

No transport-layer code should inspect this flag directly.

### 5.5.3 File-level edits

`crates/openwire/src/client.rs`

- remove field from `TransportConfig`
- add field to `RetryPolicyConfig`
- update defaults and builder setter

`crates/openwire/src/policy/follow_up.rs`

- pass retry config into `retry_reason`

### 5.5.4 Tests to add

- `canceled_requests_are_not_retried_by_default`
- `canceled_requests_are_retried_when_enabled`

## 5.6 Runtime-agnostic call timeout

### 5.6.1 Request future

Replace:

```rust
tokio::time::timeout(timeout, execute).await
```

with:

```rust
with_call_deadline(self.client.inner.runtime.clone(), ctx.deadline(), execute).await
```

This removes direct Tokio dependency from the request path.

### 5.6.2 Response body path

`ObservedIncomingBody` gains:

```rust
runtime: Arc<dyn Runtime>,
deadline_sleep: Option<BoxFuture<()>>,
deadline_error_emitted: bool,
```

Initialization:

- `TransportService::execute_exchange()` already has `self.runtime`
- pass `self.runtime.clone()` into `ObservedIncomingBody::wrap(...)`

Polling logic:

1. lazily create `deadline_sleep` from `ctx.deadline()`
2. on each `poll_frame()`, poll `deadline_sleep` first
3. if ready first:
   - synthesize `WireError::timeout(...)`
   - call `finish_with_error(&error)`
   - return `Poll::Ready(Some(Err(error)))`
4. else poll inner body exactly as today

Semantics:

- `call_end` still fires when headers are returned
- timeout while reading body is reported as body failure, not `call_failed`

### 5.6.3 Why no `Runtime` trait change

Existing `Runtime::sleep(Duration)` is sufficient for both:

- async request timeout race
- polled body deadline future

Do not add `timeout()` or `sleep_until()` to the public runtime trait in this
change set.

### 5.6.4 File-level edits

`crates/openwire/src/client.rs`

- add `with_call_deadline(...)`
- use runtime-based helper in `Call::execute()`

`crates/openwire/src/transport.rs`

- add runtime and deadline future to `ObservedIncomingBody`
- pass runtime into `wrap(...)`

### 5.6.5 Tests to add

- `call_timeout_uses_runtime_sleep_not_tokio_timeout`
- `call_timeout_can_fail_during_body_read`

## 6. Concurrency and Locking Notes

This change set does not redesign pool locking. It only fixes the reviewed
correctness issue that came from publication order.

Locking invariants after the fix:

- `ConnectionBindings` entry exists before pool publication
- pool lookup can never observe a fresh h2 connection without a binding
- unhealthy connections are pruned on pool touch points instead of becoming
  permanent dead entries

The broader single-global-pool-mutex optimization remains out of scope for this
document.

## 7. Test Matrix

All new tests stay in `crates/openwire/tests/integration.rs` unless a pure unit
test is enough.

| Finding | Test name |
|---|---|
| CONNECT parser | `connect_proxy_407_with_body_in_same_read_is_parsed` |
| CONNECT parser | `connect_proxy_200_with_prefetched_tail_preserves_tunnel_bytes` |
| Redirect headers | `redirect_same_origin_default_port_preserves_authorization` |
| Redirect headers | `redirect_cross_origin_drops_explicit_cookie_header` |
| Redirect headers | `redirect_same_proxy_preserves_proxy_authorization` |
| Redirect headers | `redirect_different_proxy_drops_proxy_authorization` |
| h2 abandonment | `http2_redirect_does_not_poison_connection` |
| h2 abandonment | `http2_auth_followup_does_not_poison_connection` |
| h2 abandonment | `dropping_http2_body_releases_stream_without_discarding_session` |
| fresh h2 publish | `fresh_http2_connection_is_not_visible_before_binding` |
| fresh h2 publish | `http2_bind_failure_leaves_no_pool_entry` |
| canceled retry | `canceled_requests_are_not_retried_by_default` |
| canceled retry | `canceled_requests_are_retried_when_enabled` |
| runtime timeout | `call_timeout_uses_runtime_sleep_not_tokio_timeout` |
| runtime timeout | `call_timeout_can_fail_during_body_read` |

## 8. Landing Order

1. proxy selector and canonical origin key
2. redirect header policy
3. CONNECT split parser and prefetched connection
4. fresh binding publish ordering
5. HTTP/2 abandonment semantics and unhealthy pruning
6. move and wire `retry_canceled_requests`
7. runtime-based request timeout helper
8. body deadline enforcement
9. doc sync in `docs/DESIGN.md`

This order keeps correctness fixes ahead of timeout plumbing and minimizes
partial states during review.

## 9. Acceptance Criteria

The implementation is complete when:

- all six findings are covered by deterministic tests
- no code path uses `tokio::time::timeout` for `call_timeout`
- redirect rebuild is both origin-aware and proxy-aware
- CONNECT 200 can preserve prefetched bytes into the tunnel stream
- HTTP/2 body drop no longer poisons the session
- no fresh connection is published to the pool before its binding exists
- `retry_canceled_requests(true)` changes observable behavior
