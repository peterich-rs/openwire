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
- application and network interceptors
- event listener factory and per-call event listeners
- custom DNS resolver / TCP connector / TLS connector hooks
- redirect handling with basic authority-sensitive header stripping
- call timeout and connect timeout
- connection pooling via `hyper-util`
- default Tokio runtime integration
- default rustls TLS connector with platform verifier or native roots fallback
- response body wrappers with body-end and connection-release events
- examples and integration tests for HTTP, redirect, custom DNS, interceptors, events, and TLS

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p openwire --tests`
