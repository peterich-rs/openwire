# Remove `hyper_util` Dependency

Date: 2026-03-24

This document captures the executable plan for removing the workspace's direct
dependency on `hyper_util`.

## Breaking Change Notice

This is a semver-breaking change to the public connector surface.

`ConnectionIo` and `BoxConnection` are public abstractions in
`crates/openwire-core/src/transport.rs`. External crates implementing custom
`TcpConnector` or `TlsConnector` can be supplied through
`ClientBuilder::tcp_connector()` and `ClientBuilder::tls_connector()` in
`crates/openwire/src/client.rs`.

Today those implementations satisfy `ConnectionIo` through
`hyper_util::client::legacy::connect::Connection`. After this change, that
trait comes from `openwire-core` instead.

External implementors will need to:

- remove their direct `hyper_util` dependency
- implement `openwire_core::Connection`
- return `openwire_core::Connected` from `fn connected(&self) -> Connected`
- switch builder usage from `hyper_util::Connected` to the OpenWire-owned type

To reduce migration friction, re-export `Connected` and `Connection` from both
`openwire-core` and `openwire`.

## Feasibility Assessment

Fully feasible.

The workspace only uses one `hyper_util` module path:

- `hyper_util::client::legacy::connect::{Connection, Connected}`

No other `hyper_util` facilities are in use. `hyper::client::conn::{http1,
http2}` handshakes only require `hyper::rt::Read + Write + Unpin + Send`; they
do not require `hyper_util::Connection`.

OpenWire already owns protocol selection, route execution, pooling, and
follow-up behavior. `hyper_util` is currently only acting as a metadata
side-channel attached to transport I/O objects.

## Current `Connected` API Surface Used

Current calls against `hyper_util::client::legacy::connect::Connected`:

- `Connected::new()`
- `.extra(T)` for `ConnectionInfo` and `CoalescingInfo`
- `.proxy(true)`
- `.negotiated_h2()`
- `.is_proxied()`
- `.is_negotiated_h2()`
- `.get_extras(&mut http::Extensions)`

No other `Connected` behaviors are consumed in this repository.

## Design: Strongly Typed Replacement

Replace the opaque typed-extra bag with explicit fields owned by
`openwire-core`.

Key decisions:

- `Connected::new()` must be side-effect-free
- `ConnectionInfo` remains optional in `Connected`
- default `ConnectionInfo` synthesis stays at the consumer side
- `CoalescingInfo`, `proxied`, and `negotiated_h2` become first-class fields
- readers use direct accessors instead of `http::Extensions`

New types in `crates/openwire-core/src/transport.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Connected {
    info: Option<ConnectionInfo>,
    coalescing: CoalescingInfo,
    proxied: bool,
    negotiated_h2: bool,
}

impl Connected {
    pub fn new() -> Self {
        Self {
            info: None,
            coalescing: CoalescingInfo::default(),
            proxied: false,
            negotiated_h2: false,
        }
    }

    pub fn info(mut self, info: ConnectionInfo) -> Self {
        self.info = Some(info);
        self
    }

    pub fn coalescing(mut self, coalescing: CoalescingInfo) -> Self {
        self.coalescing = coalescing;
        self
    }

    pub fn proxy(mut self, proxied: bool) -> Self {
        self.proxied = proxied;
        self
    }

    pub fn negotiated_h2(mut self) -> Self {
        self.negotiated_h2 = true;
        self
    }

    pub fn is_proxied(&self) -> bool {
        self.proxied
    }

    pub fn is_negotiated_h2(&self) -> bool {
        self.negotiated_h2
    }

    pub fn connection_info(&self) -> Option<&ConnectionInfo> {
        self.info.as_ref()
    }

    pub fn coalescing_info(&self) -> &CoalescingInfo {
        &self.coalescing
    }

    pub fn into_parts(self) -> (Option<ConnectionInfo>, CoalescingInfo, bool, bool) {
        (self.info, self.coalescing, self.proxied, self.negotiated_h2)
    }
}

pub trait Connection {
    fn connected(&self) -> Connected;
}
```

Builder usage stays familiar:

```rust
Connected::new()
    .info(info)
    .coalescing(coalescing)
    .proxy(true)
    .negotiated_h2()
```

## Phase 1: Define New Types in `openwire-core`

File: `crates/openwire-core/src/transport.rs`

- add `Connected`
- add `Connection`
- remove `use hyper_util::client::legacy::connect::Connection`
- update `ConnectionIo` to use the new local `Connection` trait
- update `impl Connection for BoxConnection`

File: `crates/openwire-core/src/lib.rs`

- re-export `Connected`
- re-export `Connection`

File: `crates/openwire/src/lib.rs`

- re-export `Connected`
- re-export `Connection`

This keeps the public migration path obvious for downstream connector
implementors that depend on `openwire` rather than `openwire-core`.

## Phase 2: Update Metadata Producers

### TCP Layer

File: `crates/openwire-tokio/src/lib.rs`

- replace the `hyper_util` import with `openwire_core::{Connected, Connection}`
- update `impl Connection for TcpConnection` to return
  `Connected::new().info(self.info.clone())`

### TLS Layer

File: `crates/openwire-rustls/src/lib.rs`

- replace the `hyper_util` import with `openwire_core::{Connected, Connection}`
- update `impl Connection for RustlsConnection`
- preserve:
  - `ConnectionInfo`
  - `CoalescingInfo`
  - inherited `proxied` state from the wrapped transport
  - ALPN-derived `negotiated_h2`

Recommended implementation shape:

- extract a small private helper from `impl Connection for RustlsConnection`
- example shape:

```rust
fn build_connected(
    info: ConnectionInfo,
    coalescing: CoalescingInfo,
    inner_proxied: bool,
    negotiated_h2: bool,
) -> Connected
```

This keeps the `impl Connection` body simple and gives the test suite a direct,
constructible unit under `crates/openwire-rustls/src/lib.rs` without needing to
instantiate a full `TlsStream<TokioIo<BoxConnection>>`.

Also update `connection_info_from_stream()` to preserve the current fallback
semantics:

```rust
fn connection_info_from_stream(stream: &dyn ConnectionIo) -> ConnectionInfo {
    stream.connected().connection_info().cloned().unwrap_or_else(|| ConnectionInfo {
        id: next_connection_id(),
        remote_addr: None,
        local_addr: None,
        tls: false,
    })
}
```

### Proxy and Tunnel Wrappers

File: `crates/openwire/src/transport/connect.rs`

- replace the `hyper_util` import with `openwire_core::{Connected, Connection}`
- keep `PrefetchedTunnelBytes::connected()` as a pure passthrough
- keep `ProxiedConnection::connected()` as `self.inner.connected().proxy(true)`

## Phase 3: Update Metadata Consumers

File: `crates/openwire/src/transport/protocol.rs`

- replace the `hyper_util` import with `openwire_core::Connected`
- keep `determine_protocol()` logic unchanged
- rewrite `connection_info_from_connected()` to use
  `connected.connection_info()`
- preserve the current consumer-side fallback:

```rust
pub(super) fn connection_info_from_connected(connected: &Connected) -> ConnectionInfo {
    connected.connection_info().cloned().unwrap_or_else(|| ConnectionInfo {
        id: next_connection_id(),
        remote_addr: None,
        local_addr: None,
        tls: false,
    })
}
```

- rewrite `coalescing_info_from_connected()` to use
  `connected.coalescing_info().clone()`

File: `crates/openwire/src/transport/service.rs`

- replace the `hyper_util` import with the OpenWire-owned `Connection` trait
- keep the call site unchanged:
  - `stream.connected()`
  - `connection_info_from_connected(&connected)`
  - `coalescing_info_from_connected(&connected)`
  - `determine_protocol(..., &connected)`

## Phase 4: Update Test Fixtures

File: `crates/openwire-core/src/transport.rs` test module

- replace `hyper_util` imports with crate-local `Connected` and `Connection`
- keep `NoopConnection::connected()` as `Connected::new()`

File: `crates/openwire/src/connection/fast_fallback.rs` test module

- replace `hyper_util` imports with `openwire_core::{Connected, Connection}`
- keep `TestConnection::connected()` as
  `Connected::new().info(self.info.clone())`

File: `crates/openwire/src/transport/tests.rs`

- replace `hyper_util` imports with `openwire_core::Connected`
- replace fully qualified
  `hyper_util::client::legacy::connect::Connection` impls with
  `openwire_core::Connection`
- keep `DuplexConnection::connected()` and `ScriptedConnection::connected()` as
  `Connected::new()`

## Phase 5: Add Regression Coverage

The original proposed test placement needs one adjustment: a single test cannot
directly cover `ProxiedConnection -> RustlsConnection` from
`crates/openwire/src/transport/tests.rs` because those are private types in two
different crates.

Use the following executable split instead.

### 5A. Proxy Marker Wrapper Test

File: `crates/openwire/src/transport/connect.rs`

Add a same-file unit test that verifies `ProxiedConnection::connected()`
returns `is_proxied() == true` even when the wrapped transport is not already
marked proxied.

This locally protects the OpenWire-side wrapper that currently does:

- `self.inner.connected().proxy(true)`

### 5B. TLS Metadata Composition Tests

File: `crates/openwire-rustls/src/lib.rs`

Add same-file unit tests around the extracted helper described in Phase 2.

Required cases:

- `build_connected_preserves_inner_proxy_flag`
- `build_connected_marks_negotiated_h2_when_requested`
- `build_connected_leaves_negotiated_h2_false_without_h2`

These tests directly protect the Rustls-side metadata composition logic without
requiring construction of a full `RustlsConnection` test fixture.

### 5C. End-to-End Backstop Checks

Keep existing integration tests as end-to-end backstops:

- `https_requests_can_tunnel_through_http_proxy` in
  `crates/openwire/tests/integration.rs`
- `shared_client_coalesces_https_http2_connections_across_verified_authorities`
  in `crates/openwire/tests/integration.rs`
- existing HTTPS success tests using `spawn_https_http1(...)` and
  `spawn_https_http2_with_hosts(...)`

These already exercise the proxy tunnel path, TLS connection path, and
HTTP/2-over-TLS behavior at the public API level.

## Phase 6: Remove Dependency Declarations

File: `Cargo.toml`

- remove `hyper-util` from `[workspace.dependencies]`

File: `crates/openwire/Cargo.toml`

- remove `hyper-util.workspace = true`

File: `crates/openwire-core/Cargo.toml`

- remove `hyper-util.workspace = true`

File: `crates/openwire-tokio/Cargo.toml`

- remove `hyper-util.workspace = true`

File: `crates/openwire-rustls/Cargo.toml`

- remove `hyper-util.workspace = true`

## Phase 7: Verify

Use the wider `--all-targets` matrix because benches also depend on the public
transport surface.

Compilation and tests:

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Dependency removal verification:

```bash
cargo tree | rg hyper-util
```

Expected result:

- no matches

Alternative verification:

```bash
cargo metadata --format-version 1 | rg '"name":"hyper-util"'
```

Expected result:

- no matches

`cargo tree -i hyper-util` is not the right final-state check because it errors
once the package no longer exists in the resolved graph.

## Architectural Notes

- This is a public API break. The trait shape stays the same, but its source
  crate changes from `hyper_util` to `openwire-core`.
- `hyper::client::conn::{http1, http2}` binding code does not need to change;
  it only requires `Read + Write + Unpin + Send`.
- `dyn ConnectionIo` remains viable because the new `Connection` trait is still
  object-safe.
- `Connected::new()` remains side-effect-free. Synthesized default
  `ConnectionInfo` stays in consumer fallback sites.
- Removing `http::Extensions` indirection simplifies the transport metadata
  path and keeps ownership of these semantics inside `openwire-core`.
