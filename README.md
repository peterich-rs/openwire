# openwire

OpenWire is an OkHttp-inspired async HTTP client for Rust.

It uses `hyper` for HTTP protocol state, but owns the client-side semantics
around request policy, route planning, connection pooling, fast fallback, and
protocol binding. The default executor/timer and TLS integrations are Tokio and
Rustls.

It is aimed at cases where a plain protocol client is not enough and the
networking layer needs clear policy behavior, reusable transport building
blocks, and stable observability hooks.

## What It Provides

- `Client`, `ClientBuilder`, and single-execution `Call` over `http::Request<RequestBody>`
- OkHttp-style `Call` handles for cancellation, execution state, replayable
  cloning, and executor-backed queued calls
- request-scoped timeout, retry, and redirect overrides through `Call`
- application and network interceptors
- built-in `LoggerInterceptor` with `LogLevel::{Basic, Headers, Body}`
- event listeners and stable request / connection observability
- retries, redirects, cookies, and origin / proxy authentication follow-ups
  with structured HTTP authentication challenge parsing
- transparent response decompression for `br`, `gzip`, `deflate`, and `zstd`
  through the default `compression` feature
- HTTP forward proxy, HTTPS CONNECT proxy, and SOCKS5 proxy support,
  including `socks5://user:pass@host:port` credentials and proxy-endpoint
  fast fallback
- dynamic per-request proxy selection via `ProxySelector`, including ordered
  proxy candidate fallback and `DIRECT`, with `ProxyRules` as the built-in
  rule-based implementation
- custom DNS, TCP, TLS, executor, and timer hooks
- an owned connection core with route planning, pooling, and direct HTTP/1.1 /
  HTTP/2 protocol binding
- `RequestBody::absent()` for typical no-body requests and
  `RequestBody::explicit_empty()` when zero-length framing must be explicit
- optional JSON helpers behind the `json` feature
- optional WebSocket (RFC 6455) client behind the `websocket` feature, with a
  pluggable `WebSocketEngine` trait and a built-in native codec
- `openwire-cache` as a separate application-layer cache crate with an
  RFC 9111-aligned freshness subset

## Workspace

- `crates/openwire`: public client API, policy layer, transport integration
- `crates/openwire-cache`: cache interceptor and in-memory cache store
- `crates/openwire-core`: shared body, error, event, executor/timer, transport, and policy traits
- `crates/openwire-tokio`: Tokio executor, timer, I/O, DNS, and TCP adapters
- `crates/openwire-rustls`: default Rustls TLS connector
- `crates/openwire-test`: local test support

Tokio-specific adapters are imported from `openwire-tokio` directly; `openwire`
keeps the client API and higher-level policy / planning surfaces.

## Quick Start

```rust
use http::Request;
use openwire::{Client, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let request = Request::builder()
        .uri("http://example.com/")
        .body(RequestBody::absent())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
```

Request-scoped overrides stay on the canonical execution path:

```rust
use std::time::Duration;

let response = client
    .new_call(request)
    .call_timeout(Duration::from_secs(2))
    .connect_timeout(Duration::from_millis(250))
    .follow_redirects(false)
    .execute()
    .await?;
```

These per-request retry and redirect overrides target the built-in scalar policy
knobs. Custom `RetryPolicy` and `RedirectPolicy` objects remain client-scoped.

Calls can also be controlled after they have been moved into async execution.
`Call::handle()` returns a cloneable cancellation and state handle, and
`Call::try_clone()` creates a fresh unexecuted call when the request body is
replayable:

```rust
let call = client
    .new_call(request)
    .call_timeout(Duration::from_secs(5));
let handle = call.handle();
let task = tokio::spawn(async move { call.execute().await });

handle.cancel();
let error = task.await?.expect_err("call canceled");
assert_eq!(error.kind(), openwire::WireErrorKind::Canceled);
```

For OkHttp-style asynchronous dispatch through the client's configured
executor, queue the call and await the returned handle:

```rust
let queued = client.new_call(request).enqueue()?;
let response = queued.await_response().await?;
```

Dropping `QueuedCall` does not request cancellation; call `cancel()` on the
queued call or its `CallHandle` when the in-flight work should stop.

## Authentication Challenges

`Authenticator` receives an `AuthContext` for origin `401 Unauthorized`,
forward-proxy `407 Proxy Authentication Required`, and HTTPS CONNECT proxy
authentication challenges. `AuthContext::challenges()` parses the applicable
RFC 9110 / RFC 7235 authentication header into `AuthChallenge` values:

- origin authentication reads `WWW-Authenticate`
- proxy authentication reads `Proxy-Authenticate`
- each challenge exposes its auth scheme, optional token68 value, parameters,
  and convenience access to `realm`

OpenWire does not choose credentials by itself. The caller's authenticator
selects a supported challenge and returns a replayable follow-up request with
`Authorization` or `Proxy-Authorization` as appropriate.

## HTTP Logging

OpenWire includes an OkHttp-style `LoggerInterceptor` that can be attached as an
application interceptor for logical-call logging or as a network interceptor for
post-normalization, per-attempt wire logging:

```rust
use http::Request;
use openwire::{Client, LogLevel, LoggerInterceptor, RequestBody};

let client = Client::builder()
  .application_interceptor(LoggerInterceptor::new(LogLevel::Body))
  .build()?;

let request = Request::builder()
  .method("POST")
  .uri("https://api.example.com/users")
  .header("content-type", "application/json")
  .header("authorization", "Bearer secret")
  .body(RequestBody::from_static(br#"{"name":"Alice","age":18}"#))?;

let response = client.execute(request).await?;
println!("status = {}", response.status());
```

`LogLevel::Body` pretty-prints JSON with `serde_json::to_writer_pretty`, redacts
`Authorization`, `Proxy-Authorization`, `Cookie`, and `Set-Cookie` by default,
and only buffers bodies when they are replayable and bounded. Streaming request
bodies, chunked responses, SSE, upgraded protocols, and oversized bodies are
logged as omitted placeholders instead of being fully drained into memory.

The WebSocket path still bypasses the interceptor chain today, so
`LoggerInterceptor` covers HTTP calls made through `Client::execute(...)` /
`Call::execute()` rather than `Client::new_websocket(...)`.

Proxy routing is configured through a selector so the active proxy can change at
execution time. A selector can return multiple candidates for one request; the
transport tries them in order within the same logical attempt:

```rust
use openwire::{Client, Proxy, ProxySelection, ProxySelector};

#[derive(Clone)]
struct MobileSelector;

impl ProxySelector for MobileSelector {
    fn select(&self, _uri: &http::Uri) -> Result<ProxySelection, openwire::WireError> {
        Ok(ProxySelection::new()
            .push_proxy(Proxy::https("http://proxy-a.local:8080")?)
            .push_proxy(Proxy::https("http://proxy-b.local:8080")?)
            .push_direct())
    }
}

let client = Client::builder()
    .proxy_selector(MobileSelector)
    .build()?;
```

`ProxyRules` remains available when a simple ordered rule list is enough.
Once a proxied attempt succeeds, later auth and redirect follow-ups in the same
logical call prefer that proxy first so proxy-authorization state stays bound
to the proxy that actually handled the request.

## Transparent Compression

With the default `compression` feature enabled, the bridge injects
`Accept-Encoding: br, gzip, deflate, zstd` for normal HTTP requests that do not
already set `Accept-Encoding` and are not range requests. Responses using those
encodings are decoded as a stream before they reach application interceptors or
callers, and the decoded response omits the wire `Content-Encoding` and
compressed `Content-Length` headers.

If a caller sets `Accept-Encoding` explicitly, OpenWire leaves the response
body and headers untouched so the caller owns the wire encoding semantics.

## Application Cache

`openwire-cache` provides an application interceptor for private, in-process
response caching. It currently caches replayable `GET` responses with explicit
or conservative heuristic freshness metadata and honors the core RFC 9111
controls needed to avoid unsafe reuse:

- request `Cache-Control: no-cache`, `no-store`, `max-age=0`,
  `min-fresh`, and `only-if-cached`
- response `Cache-Control: max-age`, `no-cache`, and `no-store`
- `Expires` freshness when `max-age` is absent
- `Age` reducing remaining `max-age` freshness
- `Date` apparent age and Last-Modified heuristic freshness when explicit
  freshness is absent
- multiple stored variants for one URI through `Vary` matching against the
  original request headers, with `Vary: *` treated as not reusable
- stale stored responses with `ETag` or `Last-Modified` validators are
  revalidated with conditional GET requests, and `304 Not Modified` responses
  refresh stored metadata before returning the cached body as `200 OK`
- cached hits generate a current `Age` header

The cache intentionally remains conservative: it only stores `200 OK` `GET`
responses, skips responses with `Set-Cookie`, skips authenticated requests, and
does not yet implement stale serving.

## Default Transport Settings

`Client::builder()` currently defaults to:

- pooled idle connection eviction after 5 minutes
- at most 5 idle pooled connections per address
- at most 64 in-flight requests across the client
- at most 5 in-flight requests per address

These request and pool limits are bounded by address, not only origin host. If a
caller needs the previous unbounded request-admission or idle-pool behavior, set
the corresponding knobs explicitly, for example with `usize::MAX`.

## Current Status

Today the project includes:

- request execution through `Client::execute(...)` and `Call::execute()`
- cancellation, execution-state handles, replayable `Call::try_clone()`, and
  queued calls through `Call::enqueue()`
- application and network interceptors
- retry, redirect, cookie, and authenticator follow-up handling
- HTTP forward proxy, HTTPS CONNECT proxy, and SOCKS5 proxy support
- owned HTTP/1.1 and HTTP/2 bindings via `hyper::client::conn`
- connection pooling, fast fallback, and route planning
- optional RFC 9111-oriented cache integration in `openwire-cache`
- an opt-in live-network smoke suite outside the required CI path

## Development

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo bench -p openwire --bench perf_baseline -- --noplot
```

Optional live-network smoke suite:

```bash
cargo test -p openwire --test live_network -- --ignored --test-threads=1
```

This suite is opt-in, hits public internet endpoints, and is not part of the
required CI gate.

The repository also provides a separate GitHub Actions workflow at
`.github/workflows/live-network.yml` for manual dispatches and weekly scheduled
runs without affecting the required CI path.

Deferred public-origin follow-ons are intentionally kept out of this baseline
when they require external credentials, temporary remote resources, untrusted
public proxies, or timing-sensitive assertions that public networks cannot make
credible. Those follow-ons are tracked in
[docs/live-network-follow-ups.md](docs/live-network-follow-ups.md).

## Architecture

Detailed execution flow, transport layering, and extension boundaries are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

Error-handling review, current gaps, and the long-term failure-model roadmap
are tracked in [docs/error-handling-roadmap.md](docs/error-handling-roadmap.md).
