# openwire

OpenWire is an OkHttp-inspired async HTTP client for Rust.

It uses `hyper` for HTTP protocol state, but owns the client-side semantics
around request policy, route planning, connection pooling, fast fallback, and
protocol binding. The default runtime and TLS integrations are Tokio and
Rustls.

## What It Provides

- `Client`, `ClientBuilder`, and one-shot `Call` over `http::Request<RequestBody>`
- application and network interceptors
- event listeners and stable request / connection observability
- retries, redirects, cookies, and origin / proxy authentication follow-ups
- HTTP forward proxy, HTTPS CONNECT proxy, and SOCKS5 proxy support
- custom DNS, TCP, TLS, and runtime hooks
- an owned connection core with route planning, pooling, and direct HTTP/1.1 /
  HTTP/2 protocol binding
- optional JSON helpers behind the `json` feature
- `openwire-cache` as a separate application-layer cache crate

## Workspace

- `crates/openwire`: public client API, policy layer, transport integration
- `crates/openwire-cache`: cache interceptor and in-memory cache store
- `crates/openwire-core`: shared body, error, event, runtime, and transport traits
- `crates/openwire-rustls`: default Rustls TLS connector
- `crates/openwire-test`: local test support

## Docs

- [docs/DESIGN.md](docs/DESIGN.md): canonical architecture and execution chain
- [docs/tasks.md](docs/tasks.md): active / deferred execution tracker

## Quick Start

```rust
use http::Request;
use openwire::{Client, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let request = Request::builder()
        .uri("http://example.com/")
        .body(RequestBody::empty())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
```

## Current Baseline

Today the repository includes:

- request execution through `Client::execute(...)` and `Call::execute()`
- application and network interceptor chains
- redirect, retry, cookie, and authenticator follow-up handling
- request normalization for `Host`, `User-Agent`, and body framing
- route planning plus direct-route fast fallback
- owned HTTP/1.1 and HTTP/2 bindings via `hyper::client::conn`
- connection pooling for HTTP/1.1 and HTTP/2, including conservative HTTPS
  HTTP/2 coalescing for verified authorities
- opt-in system proxy loading from standard environment variables
- local performance tests and Criterion benchmarks for warm and cold paths

## Verification

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo bench -p openwire --bench perf_baseline -- --noplot
```
