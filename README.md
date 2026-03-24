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

- `Client`, `ClientBuilder`, and one-shot `Call` over `http::Request<RequestBody>`
- application and network interceptors
- event listeners and stable request / connection observability
- retries, redirects, cookies, and origin / proxy authentication follow-ups
- HTTP forward proxy, HTTPS CONNECT proxy, and SOCKS5 proxy support,
  including `socks5://user:pass@host:port` credentials and proxy-endpoint
  fast fallback
- custom DNS, TCP, TLS, executor, and timer hooks
- an owned connection core with route planning, pooling, and direct HTTP/1.1 /
  HTTP/2 protocol binding
- `RequestBody::absent()` for typical no-body requests and
  `RequestBody::explicit_empty()` when zero-length framing must be explicit
- optional JSON helpers behind the `json` feature
- `openwire-cache` as a separate application-layer cache crate

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

## Current Status

Today the project includes:

- request execution through `Client::execute(...)` and `Call::execute()`
- application and network interceptors
- retry, redirect, cookie, and authenticator follow-up handling
- HTTP forward proxy, HTTPS CONNECT proxy, and SOCKS5 proxy support
- owned HTTP/1.1 and HTTP/2 bindings via `hyper::client::conn`
- connection pooling, fast fallback, and route planning
- optional cache integration in `openwire-cache`
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
