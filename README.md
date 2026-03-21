# openwire

OpenWire is an OkHttp-inspired async HTTP client for Rust.

This workspace uses:

- `hyper` as the protocol and connection core
- `tower` as the interceptor and policy composition layer
- `rustls` with `rustls-platform-verifier` as the default TLS stack

## Workspace

- `crates/openwire`: public client API
- `crates/openwire-core`: shared body, error, event, interceptor, and transport traits
- `crates/openwire-rustls`: default rustls TLS connector
- `crates/openwire-test`: local test server and observability test helpers

## Docs

- [docs/DESIGN.md](docs/DESIGN.md): canonical technical design
- [docs/tasks.md](docs/tasks.md): step-by-step execution tracker
- OpenWire now uses an owned connection core: it keeps `hyper` for HTTP protocol state while owning acquisition, route planning, pooling, fast fallback, and direct protocol binding

## Current API

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

## Implemented in this repository

- `Client`, `ClientBuilder`, and one-shot `Call`
- request construction via standard `http::Request<RequestBody>`
- application and network interceptors
- event listener factory and per-call event listeners
- built-in request normalization for `Host`, `User-Agent`, and request body framing
- minimal safe retries for replayable requests on connection-establishment failures
- configurable `CookieJar` support with a default in-memory `Jar`
- configurable `Authenticator` support for origin `401` follow-ups on replayable requests
- HTTP proxy forwarding and HTTPS-over-HTTP proxy tunneling via `Proxy::http(...)`, `Proxy::https(...)`, and `Proxy::all(...)`
- `proxy_authenticator(...)` support for both response-path `407` handling and HTTPS `CONNECT` tunnel authentication
- `NoProxy` exclusions for exact hosts, domain suffixes, and loopback/localhost addresses
- opt-in system proxy loading via `use_system_proxy(true)` from common `*_proxy` / `NO_PROXY` environment variables
- environment `NO_PROXY` parsing for wildcard `*`, host/domain exclusions, and IP CIDR ranges
- custom DNS resolver / TCP connector / TLS connector hooks
- redirect handling with basic authority-sensitive header stripping
- call timeout and connect timeout, including proxy CONNECT handshake reads
- typed request metadata via standard `http::Extensions`
- owned connection pooling with direct `hyper::client::conn::http1` and `hyper::client::conn::http2` bindings
- HTTP/2 over TLS via rustls ALPN negotiation
- default Tokio runtime integration
- default rustls TLS connector with platform verifier or native roots fallback
- response body wrappers with body-end and connection-release events
- local Criterion benchmarks for warm pooled HTTP/1.1 and HTTPS HTTP/2 request paths
- examples and integration tests for HTTP, cookies, auth, proxy, redirect, custom DNS, interceptors, events, and TLS

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p openwire --tests`
- `cargo test -p openwire --test performance_baseline`
- `cargo bench -p openwire --bench perf_baseline -- --noplot`
