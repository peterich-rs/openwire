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

## Roadmap

- Long-term planning and accepted next-stage work live in [docs/roadmap.md](docs/roadmap.md)
- The current near-term roadmap now starts with `RequestBuilder` ergonomics and observability stabilization

## Current API

```rust
use openwire::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let response = client.get("http://example.com/").send().await?;
    println!("status = {}", response.status());
    Ok(())
}
```

## Implemented in this repository

- `Client`, `ClientBuilder`, and one-shot `Call`
- `RequestBuilder` with `client.get(url)` / `client.post(url)` entry points
- application and network interceptors
- event listener factory and per-call event listeners
- built-in request normalization for `Host`, `User-Agent`, and request body framing
- minimal safe retries for replayable requests on connection-establishment failures
- custom DNS resolver / TCP connector / TLS connector hooks
- redirect handling with basic authority-sensitive header stripping
- call timeout and connect timeout
- per-request timeout override plus basic auth / bearer auth helpers
- connection pooling via `hyper-util`
- default Tokio runtime integration
- default rustls TLS connector with platform verifier or native roots fallback
- response body wrappers with body-end and connection-release events
- examples and integration tests for HTTP, redirect, custom DNS, interceptors, events, and TLS

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p openwire --tests`
