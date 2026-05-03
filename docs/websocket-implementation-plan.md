# WebSocket Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land RFC 6455 WebSocket client support in openwire with a pluggable engine trait, native default codec, and OkHttp-aligned ergonomics, behind `feature = "websocket"`.

**Architecture:** Entry point `Client::new_websocket(request)` reuses the existing application-interceptor → bridge → network-interceptor → `TransportService` chain. The bridge tags the request as a WebSocket call, validates response headers, and after `101 Switching Protocols` calls `hyper::upgrade::on` to take the underlying IO and hand it to a `WebSocketEngine`. The engine produces a duplex `WebSocketChannel` (Sink + Stream of frames). openwire owns the writer task (mpsc), heartbeat scheduling, drop semantics, `EventListener` instrumentation, and `ConnectionPermit` accounting; the engine owns framing.

**Tech Stack:** Rust 2021, hyper 1.x, tower 0.5, tokio 1.x, http 1.x, bytes 1.x, base64 0.22, sha1 0.10 (new), thiserror 2.x.

**Implementation status update (2026-05-03):**
- The instrumentation wrapper and both adapter crates are now implemented in code.
- v1 does **not** currently follow the `TransportService`-based architecture above end-to-end; the shipping implementation uses the parallel `ConnectorStack` execution path documented in `docs/websocket-design.md`.
- Treat `docs/websocket-design.md` as the source of truth for current behavior. This plan remains useful as the original task breakdown, especially for the still-unfinished `TransportService` integration work.

Reference spec: `docs/websocket-design.md` (Status: Implemented, with the v1 transport-path divergence called out explicitly). Read it before starting.

---

## Phase 1 — Foundation

### Task 1: Add `websocket` feature flag and module skeletons

**Files:**
- Modify: `crates/openwire-core/Cargo.toml`
- Modify: `crates/openwire/Cargo.toml`
- Modify: `Cargo.toml` (workspace)
- Create: `crates/openwire-core/src/websocket/mod.rs`
- Create: `crates/openwire/src/websocket/mod.rs`
- Modify: `crates/openwire-core/src/lib.rs`
- Modify: `crates/openwire/src/lib.rs`

- [ ] **Step 1: Add `sha1` to workspace dependencies**

In `Cargo.toml` workspace.dependencies block (alphabetical order):

```toml
sha1 = "0.10"
```

- [ ] **Step 2: Add feature + sha1 to `crates/openwire-core/Cargo.toml`**

Under `[features]`:

```toml
[features]
default = []
websocket = []
```

(no new dep — engine trait + types only need things already present: bytes, http, futures, thiserror)

- [ ] **Step 3: Add feature + dependencies to `crates/openwire/Cargo.toml`**

```toml
[features]
default = []
websocket = ["openwire-core/websocket", "dep:sha1", "dep:base64"]

[dependencies]
sha1 = { workspace = true, optional = true }
base64 = { workspace = true, optional = true }
```

- [ ] **Step 4: Create skeleton `crates/openwire-core/src/websocket/mod.rs`**

```rust
pub mod engine;
pub mod error;
pub mod message;

pub use engine::*;
pub use error::*;
pub use message::*;
```

- [ ] **Step 5: Create skeleton `crates/openwire/src/websocket/mod.rs`**

```rust
pub(crate) mod handshake;
pub(crate) mod transport;
pub(crate) mod writer;
pub(crate) mod instrumented;
pub(crate) mod native;

mod public;

pub use public::{WebSocket, WebSocketHandshake, WebSocketReceiver, WebSocketSender};
```

(Submodules will be filled in by later tasks; for this task they can be `// placeholder` files that compile.)

- [ ] **Step 6: Conditionally include the modules**

In `crates/openwire-core/src/lib.rs`, append:

```rust
#[cfg(feature = "websocket")]
pub mod websocket;
```

In `crates/openwire/src/lib.rs`, append:

```rust
#[cfg(feature = "websocket")]
pub mod websocket;
```

- [ ] **Step 7: Verify it builds with and without the feature**

```bash
cargo check -p openwire
cargo check -p openwire --features websocket
```

Expected: both succeed, no warnings about unused imports.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/openwire/Cargo.toml crates/openwire-core/Cargo.toml \
        crates/openwire/src/lib.rs crates/openwire-core/src/lib.rs \
        crates/openwire/src/websocket/ crates/openwire-core/src/websocket/
git commit -m "feat(websocket): add feature flag and module skeletons"
```

---

### Task 2: Expose `ConnectionLimiter` to the websocket module

**Files:**
- Modify: `crates/openwire/src/connection/limits.rs`
- Modify: `crates/openwire/src/connection/mod.rs`

`ConnectionLimiter` is `pub(crate)` and has `try_acquire` / `acquire` / `availability` already. The websocket module lives inside `openwire`, so `pub(crate)` is sufficient. Verify and add a `can_acquire(&self, addr: &Address) -> bool` non-consuming probe if it does not already exist (the prior fix for "Tolerate DNS failure" introduced something similar).

- [ ] **Step 1: Read the current public surface**

Run: `grep -n "pub(crate) fn\|pub(crate) async fn" crates/openwire/src/connection/limits.rs`

Capture the list of exposed methods on `ConnectionLimiter` and `ConnectionPermit`. Confirm:
- `try_acquire(&Address) -> Option<ConnectionPermit>`
- `acquire(&Address) -> impl Future<Output = ConnectionPermit>` (or equivalent)
- `ConnectionPermit: Send + 'static` (for handing across tasks)

- [ ] **Step 2: Write a test that uses the limiter from outside `connection/`**

Create `crates/openwire/src/connection/limits_websocket_visibility_test.rs` (or add to `connection/limits.rs` test module):

```rust
#[cfg(test)]
#[cfg(feature = "websocket")]
mod websocket_visibility {
    use super::*;
    use crate::connection::Address;

    #[tokio::test]
    async fn limiter_can_be_used_from_websocket_module() {
        let limiter = ConnectionLimiter::new(2, 1);
        let addr = Address::new("example.com".into(), 443, false);
        let permit = limiter.acquire(&addr).await;
        assert!(limiter.try_acquire(&addr).is_none(), "second permit must be denied");
        drop(permit);
        assert!(limiter.try_acquire(&addr).is_some(), "permit returned after drop");
    }
}
```

Adjust `Address::new(...)` and `ConnectionLimiter::new(...)` signatures to whatever the actual constructors take (check by reading the file).

- [ ] **Step 3: Run the test**

```bash
cargo test -p openwire --features websocket --lib limits_websocket_visibility
```

Expected: PASS. If FAIL because `ConnectionLimiter::new` is private, change it to `pub(crate)` (it likely already is). If `acquire` is missing, add a wrapper that resolves a `Future<Output = ConnectionPermit>`.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire/src/connection/limits.rs
git commit -m "feat(websocket): expose ConnectionLimiter for websocket module"
```

---

### Task 3: Add `RoutePreference::Http1Only` flag

**Files:**
- Modify: `crates/openwire/src/connection/planning.rs`
- Test: same file's `#[cfg(test)] mod tests`

Bridge will set this on the request extension; the route planner will reject H2 routes when present.

- [ ] **Step 1: Inspect `planning.rs` to understand current route filtering**

```bash
grep -n "alpn\|http2\|protocol\|RoutePlan" crates/openwire/src/connection/planning.rs | head -20
```

- [ ] **Step 2: Add `RoutePreference` enum**

In `crates/openwire/src/connection/planning.rs`:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RoutePreference {
    #[default]
    Any,
    Http1Only,
}
```

Wire it into the existing route-selection function so a route with `negotiated_h2 = true` is filtered out when `pref == Http1Only`. (Most likely a one-line `if pref == Http1Only && route.expected_h2 { continue; }` inside the existing iteration.)

- [ ] **Step 3: Write a unit test**

```rust
#[test]
fn http1_only_filters_out_h2_routes() {
    let plan = RoutePlanner::new(/* … */).plan(...);
    let filtered = plan.with_preference(RoutePreference::Http1Only);
    assert!(filtered.routes().iter().all(|r| !r.is_http2()));
}
```

Mirror the actual constructor names and accessors used in the file.

- [ ] **Step 4: Run the test**

```bash
cargo test -p openwire --lib planning::tests::http1_only_filters_out_h2_routes
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/openwire/src/connection/planning.rs
git commit -m "feat(websocket): add RoutePreference::Http1Only flag"
```

---

## Phase 2 — Core Types

### Task 4: Define `Message` and ancillary types

**Files:**
- Create: `crates/openwire-core/src/websocket/message.rs`

- [ ] **Step 1: Write the file**

```rust
use bytes::Bytes;

#[derive(Clone, Debug)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind { Text, Binary }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseInitiator { Local, Remote }

impl Message {
    pub fn kind(&self) -> Option<MessageKind> {
        match self {
            Message::Text(_) => Some(MessageKind::Text),
            Message::Binary(_) => Some(MessageKind::Binary),
            _ => None,
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Message::Text(s) => s.len(),
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
            Message::Close { reason, .. } => 2 + reason.len(),
        }
    }
}
```

- [ ] **Step 2: Add a smoke test**

In a `#[cfg(test)] mod tests` block at the bottom:

```rust
#[test]
fn payload_len_includes_close_code_bytes() {
    let m = Message::Close { code: 1000, reason: "ok".into() };
    assert_eq!(m.payload_len(), 4);
}
```

- [ ] **Step 3: Run**

```bash
cargo test -p openwire-core --features websocket --lib websocket::message
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire-core/src/websocket/message.rs
git commit -m "feat(websocket): add Message and ancillary types"
```

---

### Task 5: Define error types

**Files:**
- Create: `crates/openwire-core/src/websocket/error.rs`

- [ ] **Step 1: Write the file**

```rust
use thiserror::Error;

use crate::WireError;

#[derive(Debug, Error)]
pub enum WebSocketError {
    #[error("handshake failed: {reason:?}")]
    Handshake {
        status: Option<http::StatusCode>,
        reason: HandshakeFailure,
    },

    #[error(transparent)]
    Engine(#[from] WebSocketEngineError),

    #[error("server closed the connection: {code} {reason}")]
    ClosedByPeer { code: u16, reason: String },

    #[error("websocket {0:?} timed out")]
    Timeout(TimeoutKind),

    #[error("transport io error: {0}")]
    Io(#[source] WireError),

    #[error("local cancellation")]
    LocalCancelled,
}

#[derive(Debug, Clone)]
pub enum HandshakeFailure {
    UnexpectedStatus,
    MissingUpgrade,
    MissingConnection,
    InvalidAccept,
    SubprotocolMismatch { offered: Vec<String>, returned: String },
    UnsupportedExtension(String),
    Other(String),
}

#[derive(Debug, Clone, Copy)]
pub enum TimeoutKind { Handshake, Close, Ping }

#[derive(Debug, Error)]
pub enum WebSocketEngineError {
    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    #[error("invalid utf-8 in text frame")]
    InvalidUtf8,

    #[error("invalid close code: {0}")]
    InvalidCloseCode(u16),

    #[error("payload too large: limit={limit} received={received}")]
    PayloadTooLarge { limit: usize, received: usize },

    #[error("unsupported extension: {0}")]
    UnsupportedExtension(String),

    #[error("io error: {0}")]
    Io(#[source] WireError),
}
```

- [ ] **Step 2: Verify build**

```bash
cargo check -p openwire-core --features websocket
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/openwire-core/src/websocket/error.rs
git commit -m "feat(websocket): add error types"
```

---

### Task 6: Define `WebSocketEngine` trait + config + channel

**Files:**
- Create: `crates/openwire-core/src/websocket/engine.rs`

- [ ] **Step 1: Write the file**

```rust
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::sink::Sink;

use crate::transport::BoxConnection;
use crate::BoxFuture;

use super::error::WebSocketEngineError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    /// Reserved for v2; engines may reject Server with `UnsupportedExtension`.
    Server,
}

#[derive(Clone, Debug)]
pub struct WebSocketEngineConfig {
    pub role: Role,
    pub subprotocol: Option<String>,
    pub extensions: Vec<String>,
    pub max_frame_size: usize,
    pub max_message_size: usize,
}

#[derive(Clone, Debug)]
pub enum EngineFrame {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

pub type BoxEngineSink = Pin<Box<dyn Sink<EngineFrame, Error = WebSocketEngineError> + Send>>;
pub type BoxEngineStream =
    Pin<Box<dyn Stream<Item = Result<EngineFrame, WebSocketEngineError>> + Send>>;

pub struct WebSocketChannel {
    pub send: BoxEngineSink,
    pub recv: BoxEngineStream,
}

pub trait WebSocketEngine: Send + Sync + 'static {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<'static, Result<WebSocketChannel, WebSocketEngineError>>;
}

pub type SharedWebSocketEngine = Arc<dyn WebSocketEngine>;
```

- [ ] **Step 2: Verify build**

```bash
cargo check -p openwire-core --features websocket
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/openwire-core/src/websocket/engine.rs
git commit -m "feat(websocket): add engine trait and channel types"
```

---

## Phase 3 — Handshake Plumbing

### Task 7: `Sec-WebSocket-Accept` derivation

**Files:**
- Create: `crates/openwire/src/websocket/handshake.rs`

This is the RFC 6455 §1.3 derivation: `base64(sha1(client_key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`.

- [ ] **Step 1: Write the failing test (RFC test vector)**

```rust
// crates/openwire/src/websocket/handshake.rs
use base64::Engine;
use sha1::{Digest, Sha1};

const HANDSHAKE_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub(crate) fn derive_accept(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(HANDSHAKE_MAGIC.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6455_section_1_3_example() {
        // RFC 6455 §1.3: Sec-WebSocket-Key dGhlIHNhbXBsZSBub25jZQ==
        // expected accept s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
        assert_eq!(
            derive_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p openwire --features websocket --lib websocket::handshake::tests::rfc_6455_section_1_3_example
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/openwire/src/websocket/handshake.rs
git commit -m "feat(websocket): derive Sec-WebSocket-Accept (RFC 6455 §1.3)"
```

---

### Task 8: `Sec-WebSocket-Key` random generation

**Files:**
- Modify: `crates/openwire/src/websocket/handshake.rs`

- [ ] **Step 1: Add the function**

```rust
use base64::Engine;

pub(crate) fn generate_client_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
```

If `getrandom` is not in workspace deps, add it as a feature-gated dep on `openwire`:

```toml
# crates/openwire/Cargo.toml
[features]
websocket = [..., "dep:getrandom"]

[dependencies]
getrandom = { version = "0.2", optional = true }
```

- [ ] **Step 2: Test that it produces 24 chars (16 bytes base64)**

```rust
#[test]
fn client_key_is_24_base64_chars() {
    let k = generate_client_key();
    assert_eq!(k.len(), 24);
    assert!(k.ends_with('='), "16-byte base64 always ends with =");
}

#[test]
fn client_key_is_random() {
    let a = generate_client_key();
    let b = generate_client_key();
    assert_ne!(a, b);
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::handshake::tests::client_key
git add crates/openwire/Cargo.toml crates/openwire/src/websocket/handshake.rs
git commit -m "feat(websocket): generate Sec-WebSocket-Key"
```

---

### Task 9: WebSocket request marker + bridge handshake injection

**Files:**
- Modify: `crates/openwire/src/bridge.rs`
- Modify: `crates/openwire/src/websocket/handshake.rs`

The marker is a request extension: `WebSocketRequestMarker { subprotocols: Vec<String>, expected_accept: String }`. `Client::new_websocket` attaches it; `BridgeInterceptor` reads it.

- [ ] **Step 1: Define the marker in `handshake.rs`**

```rust
#[derive(Clone, Debug)]
pub(crate) struct WebSocketRequestMarker {
    pub subprotocols: Vec<String>,
    pub expected_accept: String,
}
```

- [ ] **Step 2: Write a failing bridge test**

In `crates/openwire/src/bridge.rs`'s test module, add (under `#[cfg(feature = "websocket")]`):

```rust
#[cfg(feature = "websocket")]
#[test]
fn injects_websocket_handshake_headers() {
    use crate::websocket::handshake::WebSocketRequestMarker;
    use http::Method;

    let mut req: Request<RequestBody> = Request::builder()
        .method(Method::GET)
        .uri("ws://example.com/socket")
        .body(RequestBody::empty())
        .unwrap();
    req.extensions_mut().insert(WebSocketRequestMarker {
        subprotocols: vec!["chat".into()],
        expected_accept: "expected".into(),
    });

    normalize_request(&mut req).unwrap();

    let h = req.headers();
    assert_eq!(h.get("upgrade").unwrap(), "websocket");
    assert!(h.get("connection").unwrap().to_str().unwrap().to_lowercase().contains("upgrade"));
    assert_eq!(h.get("sec-websocket-version").unwrap(), "13");
    assert!(h.get("sec-websocket-key").is_some());
    assert_eq!(h.get("sec-websocket-protocol").unwrap(), "chat");
    // Scheme rewritten ws -> http
    assert_eq!(req.uri().scheme_str(), Some("http"));
    // Forced HTTP/1.1
    assert_eq!(req.version(), Version::HTTP_11);
}
```

- [ ] **Step 3: Run to confirm it fails**

```bash
cargo test -p openwire --features websocket --lib bridge::tests::injects_websocket_handshake_headers
```

Expected: FAIL.

- [ ] **Step 4: Implement bridge changes**

In `bridge.rs`, modify `normalize_request`:

```rust
fn normalize_request(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    #[cfg(feature = "websocket")]
    if request.extensions().get::<crate::websocket::handshake::WebSocketRequestMarker>().is_some() {
        crate::websocket::handshake::inject_handshake(request)?;
    }
    normalize_host_header(request)?;
    normalize_user_agent_header(request);
    normalize_body_headers(request);
    Ok(())
}
```

In `handshake.rs`, add:

```rust
use http::header::{CONNECTION, UPGRADE};
use http::{HeaderValue, Request, Uri, Version};

use openwire_core::{RequestBody, WireError};

pub(crate) fn inject_handshake(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    request_must_be_get(request)?;
    rewrite_scheme(request)?;
    request.headers_mut().insert(UPGRADE, HeaderValue::from_static("websocket"));
    request.headers_mut().insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    request
        .headers_mut()
        .insert("sec-websocket-version", HeaderValue::from_static("13"));

    let key = generate_client_key();
    let accept = derive_accept(&key);
    request
        .headers_mut()
        .insert("sec-websocket-key", HeaderValue::from_str(&key).unwrap());

    let marker = request
        .extensions_mut()
        .get_mut::<WebSocketRequestMarker>()
        .expect("marker must be present");
    marker.expected_accept = accept;

    if !marker.subprotocols.is_empty() {
        let value = marker.subprotocols.join(", ");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&value).map_err(|e| WireError::invalid_request(e.to_string()))?,
        );
    }

    *request.version_mut() = Version::HTTP_11;
    request
        .extensions_mut()
        .insert(crate::connection::planning::RoutePreference::Http1Only);

    Ok(())
}

fn request_must_be_get(request: &Request<RequestBody>) -> Result<(), WireError> {
    if request.method() != http::Method::GET {
        return Err(WireError::invalid_request("WebSocket request must be GET"));
    }
    Ok(())
}

fn rewrite_scheme(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    let parts = request.uri().clone().into_parts();
    let scheme = match parts.scheme.as_ref().map(http::uri::Scheme::as_str) {
        Some("ws") => http::uri::Scheme::HTTP,
        Some("wss") => http::uri::Scheme::HTTPS,
        Some("http") | Some("https") => return Ok(()),
        _ => return Err(WireError::invalid_request("WebSocket URI must use ws/wss/http/https")),
    };
    let mut new_parts = parts;
    new_parts.scheme = Some(scheme);
    *request.uri_mut() = Uri::from_parts(new_parts).map_err(|e| WireError::invalid_request(e.to_string()))?;
    Ok(())
}
```

- [ ] **Step 5: Run test to confirm pass**

```bash
cargo test -p openwire --features websocket --lib bridge
```

Expected: all bridge tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/openwire/src/bridge.rs crates/openwire/src/websocket/handshake.rs
git commit -m "feat(websocket): bridge injects handshake headers and forces HTTP/1.1"
```

---

### Task 10: Response validation

**Files:**
- Modify: `crates/openwire/src/websocket/handshake.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod response_tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, StatusCode};

    fn ok_response(accept: &str) -> http::Response<()> {
        let mut r = http::Response::new(());
        *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        r.headers_mut().insert("upgrade", HeaderValue::from_static("websocket"));
        r.headers_mut().insert("connection", HeaderValue::from_static("Upgrade"));
        r.headers_mut().insert("sec-websocket-accept", HeaderValue::from_str(accept).unwrap());
        r
    }

    #[test]
    fn accepts_valid_response() {
        let r = ok_response("expected");
        assert!(validate_handshake_response(&r, "expected", &[]).is_ok());
    }

    #[test]
    fn rejects_non_101() {
        let mut r = ok_response("expected");
        *r.status_mut() = StatusCode::OK;
        assert!(validate_handshake_response(&r, "expected", &[]).is_err());
    }

    #[test]
    fn rejects_bad_accept() {
        let r = ok_response("wrong");
        let err = validate_handshake_response(&r, "expected", &[]).unwrap_err();
        matches!(err, HandshakeFailure::InvalidAccept);
    }

    #[test]
    fn rejects_missing_upgrade() {
        let mut r = ok_response("expected");
        r.headers_mut().remove("upgrade");
        let err = validate_handshake_response(&r, "expected", &[]).unwrap_err();
        matches!(err, HandshakeFailure::MissingUpgrade);
    }

    #[test]
    fn rejects_subprotocol_not_offered() {
        let mut r = ok_response("expected");
        r.headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("v2"));
        let err = validate_handshake_response(&r, "expected", &["v1".into()]).unwrap_err();
        matches!(err, HandshakeFailure::SubprotocolMismatch { .. });
    }

    #[test]
    fn accepts_no_subprotocol_returned_when_offered() {
        let r = ok_response("expected");
        assert!(validate_handshake_response(&r, "expected", &["v1".into()]).is_ok());
    }
}
```

- [ ] **Step 2: Implement `validate_handshake_response`**

```rust
use openwire_core::websocket::HandshakeFailure;

pub(crate) struct ValidatedHandshake {
    pub subprotocol: Option<String>,
    pub extensions: Vec<String>,
}

pub(crate) fn validate_handshake_response<B>(
    response: &http::Response<B>,
    expected_accept: &str,
    offered_subprotocols: &[String],
) -> Result<ValidatedHandshake, HandshakeFailure> {
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(HandshakeFailure::UnexpectedStatus);
    }

    let h = response.headers();

    let upgrade = h.get("upgrade").ok_or(HandshakeFailure::MissingUpgrade)?;
    if !upgrade.as_bytes().eq_ignore_ascii_case(b"websocket") {
        return Err(HandshakeFailure::MissingUpgrade);
    }

    let connection = h.get("connection").ok_or(HandshakeFailure::MissingConnection)?;
    let conn_lc = connection.to_str().unwrap_or("").to_ascii_lowercase();
    if !conn_lc.split(',').any(|t| t.trim() == "upgrade") {
        return Err(HandshakeFailure::MissingConnection);
    }

    let accept = h
        .get("sec-websocket-accept")
        .and_then(|v| v.to_str().ok())
        .ok_or(HandshakeFailure::InvalidAccept)?;
    if accept != expected_accept {
        return Err(HandshakeFailure::InvalidAccept);
    }

    let subprotocol = match h.get("sec-websocket-protocol") {
        None => None,
        Some(v) => {
            let s = v.to_str().map_err(|_| HandshakeFailure::Other("bad protocol".into()))?;
            if s.contains(',') {
                return Err(HandshakeFailure::Other("multiple subprotocols returned".into()));
            }
            if !offered_subprotocols.iter().any(|o| o == s) {
                return Err(HandshakeFailure::SubprotocolMismatch {
                    offered: offered_subprotocols.to_vec(),
                    returned: s.to_string(),
                });
            }
            Some(s.to_string())
        }
    };

    let extensions: Vec<String> = h
        .get("sec-websocket-extensions")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect())
        .unwrap_or_default();

    Ok(ValidatedHandshake { subprotocol, extensions })
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::handshake
git add crates/openwire/src/websocket/handshake.rs
git commit -m "feat(websocket): validate 101 response headers and subprotocol"
```

---

## Phase 4 — Native Engine

### Task 11: Frame opcodes + close-code validation table

**Files:**
- Create: `crates/openwire/src/websocket/native/mod.rs`
- Create: `crates/openwire/src/websocket/native/codec.rs`

- [ ] **Step 1: Stub `native/mod.rs`**

```rust
pub(crate) mod codec;
pub(crate) mod mask;

mod engine;
pub use engine::NativeEngine;
```

(`engine.rs` is stubbed for now: `pub struct NativeEngine;` — fully wired in Task 16.)

- [ ] **Step 2: Define opcodes and frame header in `codec.rs`**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Opcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl Opcode {
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b & 0x0F {
            0x0 => Some(Self::Continuation),
            0x1 => Some(Self::Text),
            0x2 => Some(Self::Binary),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xA => Some(Self::Pong),
            _ => None,
        }
    }
    pub(crate) fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

pub(crate) fn close_code_is_valid(code: u16) -> bool {
    // RFC 6455 §7.4: 1000-1011, 1012-1014 reserved (we accept), 3000-4999 application
    matches!(code,
        1000 | 1001 | 1002 | 1003 | 1007 | 1008 | 1009 | 1010 | 1011
        | 3000..=4999)
}
```

- [ ] **Step 3: Add tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_round_trip() {
        for op in [Opcode::Text, Opcode::Binary, Opcode::Close, Opcode::Ping, Opcode::Pong] {
            assert_eq!(Opcode::from_byte(op as u8), Some(op));
        }
    }

    #[test]
    fn close_code_rejects_reserved() {
        assert!(close_code_is_valid(1000));
        assert!(!close_code_is_valid(1004)); // reserved
        assert!(!close_code_is_valid(2999)); // reserved IANA range
        assert!(close_code_is_valid(3000));
        assert!(!close_code_is_valid(5000));
    }
}
```

- [ ] **Step 4: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native::codec
git add crates/openwire/src/websocket/native/
git commit -m "feat(websocket-native): opcodes and close-code validation"
```

---

### Task 12: Masking helper with RFC test vector

**Files:**
- Create: `crates/openwire/src/websocket/native/mask.rs`

- [ ] **Step 1: Write test first**

```rust
pub(crate) fn mask_in_place(payload: &mut [u8], key: [u8; 4]) {
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= key[i % 4];
    }
}

pub(crate) fn random_mask_key() -> [u8; 4] {
    let mut k = [0u8; 4];
    getrandom::getrandom(&mut k).expect("getrandom failed");
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6455_section_5_3_example() {
        // RFC 6455 §5.3 example:
        // unmasked: 0x48, 0x65, 0x6c, 0x6c, 0x6f ("Hello")
        // mask:     0x37, 0xfa, 0x21, 0x3d
        // masked:   0x7f, 0x9f, 0x4d, 0x51, 0x58
        let mut buf = [0x48, 0x65, 0x6c, 0x6c, 0x6f];
        mask_in_place(&mut buf, [0x37, 0xfa, 0x21, 0x3d]);
        assert_eq!(buf, [0x7f, 0x9f, 0x4d, 0x51, 0x58]);
    }

    #[test]
    fn unmask_is_self_inverse() {
        let mut buf = b"openwire".to_vec();
        let key = [0xab, 0xcd, 0xef, 0x12];
        mask_in_place(&mut buf, key);
        mask_in_place(&mut buf, key);
        assert_eq!(buf, b"openwire");
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native::mask
git add crates/openwire/src/websocket/native/mask.rs
git commit -m "feat(websocket-native): masking with RFC 6455 §5.3 test vector"
```

---

### Task 13: Frame encoder

**Files:**
- Modify: `crates/openwire/src/websocket/native/codec.rs`

- [ ] **Step 1: Write tests for length boundary cases**

```rust
#[cfg(test)]
mod encode_tests {
    use super::*;
    use bytes::BytesMut;

    fn encode(opcode: Opcode, payload: &[u8], fin: bool, mask_key: [u8; 4]) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_frame(&mut out, FrameHeader { fin, opcode }, payload, Some(mask_key));
        out.to_vec()
    }

    #[test]
    fn encodes_short_text_frame() {
        // FIN=1, opcode=Text(0x1), MASK=1, len=5
        let bytes = encode(Opcode::Text, b"hello", true, [1, 2, 3, 4]);
        assert_eq!(bytes[0], 0x81);          // FIN=1, opcode=1
        assert_eq!(bytes[1], 0x80 | 5);      // MASK=1, len=5
        assert_eq!(&bytes[2..6], &[1, 2, 3, 4]);
        // payload masked
        let mut expected = b"hello".to_vec();
        super::super::mask::mask_in_place(&mut expected, [1, 2, 3, 4]);
        assert_eq!(&bytes[6..], &expected[..]);
    }

    #[test]
    fn uses_16bit_length_for_payload_126() {
        let payload = vec![0u8; 126];
        let bytes = encode(Opcode::Binary, &payload, true, [0; 4]);
        assert_eq!(bytes[1], 0x80 | 126);
        assert_eq!(&bytes[2..4], &[0x00, 0x7E]);
    }

    #[test]
    fn uses_64bit_length_for_payload_65536() {
        let payload = vec![0u8; 65536];
        let bytes = encode(Opcode::Binary, &payload, true, [0; 4]);
        assert_eq!(bytes[1], 0x80 | 127);
        assert_eq!(&bytes[2..10], &[0, 0, 0, 0, 0, 1, 0, 0]);
    }
}
```

- [ ] **Step 2: Implement encoder**

```rust
use bytes::{BufMut, BytesMut};

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameHeader {
    pub fin: bool,
    pub opcode: Opcode,
}

pub(crate) fn encode_frame(
    out: &mut BytesMut,
    header: FrameHeader,
    payload: &[u8],
    mask: Option<[u8; 4]>,
) {
    let b0 = if header.fin { 0x80 } else { 0x00 } | (header.opcode as u8);
    out.put_u8(b0);

    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
    let len = payload.len();
    if len <= 125 {
        out.put_u8(mask_bit | len as u8);
    } else if len <= u16::MAX as usize {
        out.put_u8(mask_bit | 126);
        out.put_u16(len as u16);
    } else {
        out.put_u8(mask_bit | 127);
        out.put_u64(len as u64);
    }

    if let Some(key) = mask {
        out.put_slice(&key);
        let start = out.len();
        out.put_slice(payload);
        super::mask::mask_in_place(&mut out[start..], key);
    } else {
        out.put_slice(payload);
    }
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native::codec::encode_tests
git add crates/openwire/src/websocket/native/codec.rs
git commit -m "feat(websocket-native): frame encoder"
```

---

### Task 14: Frame decoder

**Files:**
- Modify: `crates/openwire/src/websocket/native/codec.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod decode_tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn decodes_short_text_frame() {
        let mut buf = BytesMut::from(&[0x81u8, 0x05, b'h', b'e', b'l', b'l', b'o'][..]);
        let f = decode_frame(&mut buf, 1024).unwrap().expect("frame ready");
        assert!(f.fin);
        assert_eq!(f.opcode, Opcode::Text);
        assert_eq!(f.payload.as_ref(), b"hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn returns_none_when_partial() {
        let mut buf = BytesMut::from(&[0x81u8][..]); // only first byte
        assert!(decode_frame(&mut buf, 1024).unwrap().is_none());
    }

    #[test]
    fn rejects_masked_server_frame() {
        // bit 0x80 of len byte set -> server should not mask
        let mut buf = BytesMut::from(&[0x81u8, 0x85, 1, 2, 3, 4, b'h', b'e', b'l', b'l', b'o'][..]);
        let err = decode_frame(&mut buf, 1024).unwrap_err();
        matches!(err, WebSocketEngineError::InvalidFrame(_));
    }

    #[test]
    fn enforces_max_frame_size() {
        // 16-bit length=200, but limit=100
        let mut buf = BytesMut::from(&[0x82u8, 126, 0, 200][..]);
        buf.extend_from_slice(&vec![0u8; 200]);
        let err = decode_frame(&mut buf, 100).unwrap_err();
        matches!(err, WebSocketEngineError::PayloadTooLarge { .. });
    }
}
```

- [ ] **Step 2: Implement decoder**

```rust
use bytes::Buf;
use openwire_core::websocket::WebSocketEngineError;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: bytes::Bytes,
}

pub(crate) fn decode_frame(
    buf: &mut BytesMut,
    max_frame_size: usize,
) -> Result<Option<DecodedFrame>, WebSocketEngineError> {
    if buf.len() < 2 { return Ok(None); }

    let b0 = buf[0];
    let b1 = buf[1];
    let fin = (b0 & 0x80) != 0;
    let rsv = b0 & 0x70;
    if rsv != 0 {
        return Err(WebSocketEngineError::InvalidFrame("reserved bits set".into()));
    }
    let opcode = Opcode::from_byte(b0)
        .ok_or_else(|| WebSocketEngineError::InvalidFrame("unknown opcode".into()))?;

    let mask_bit = (b1 & 0x80) != 0;
    if mask_bit {
        return Err(WebSocketEngineError::InvalidFrame("server frames must not be masked".into()));
    }

    if opcode.is_control() && !fin {
        return Err(WebSocketEngineError::InvalidFrame("fragmented control frame".into()));
    }

    let len_field = b1 & 0x7F;
    let (header_len, payload_len) = match len_field {
        0..=125 => (2usize, len_field as usize),
        126 => {
            if buf.len() < 4 { return Ok(None); }
            (4, u16::from_be_bytes([buf[2], buf[3]]) as usize)
        }
        127 => {
            if buf.len() < 10 { return Ok(None); }
            let mut v = [0u8; 8];
            v.copy_from_slice(&buf[2..10]);
            (10, u64::from_be_bytes(v) as usize)
        }
        _ => unreachable!(),
    };

    if opcode.is_control() && payload_len > 125 {
        return Err(WebSocketEngineError::InvalidFrame("control frame > 125 bytes".into()));
    }
    if payload_len > max_frame_size {
        return Err(WebSocketEngineError::PayloadTooLarge {
            limit: max_frame_size,
            received: payload_len,
        });
    }

    if buf.len() < header_len + payload_len { return Ok(None); }
    buf.advance(header_len);
    let payload = buf.split_to(payload_len).freeze();

    Ok(Some(DecodedFrame { fin, opcode, payload }))
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native::codec::decode_tests
git add crates/openwire/src/websocket/native/codec.rs
git commit -m "feat(websocket-native): frame decoder"
```

---

### Task 15: Reassembly + UTF-8 + close-frame parsing

**Files:**
- Create: `crates/openwire/src/websocket/native/session.rs`

The session is the running state machine that turns a stream of `DecodedFrame`s into `EngineFrame`s (reassembling fragments, validating UTF-8, parsing close payloads).

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn assembles_fragmented_text() {
        let mut s = ReassemblyState::new(/*max_message=*/ 1024);
        let f1 = s.feed(DecodedFrame {
            fin: false, opcode: Opcode::Text, payload: Bytes::from_static(b"He")
        }).unwrap();
        assert!(f1.is_none());
        let f2 = s.feed(DecodedFrame {
            fin: true, opcode: Opcode::Continuation, payload: Bytes::from_static(b"llo")
        }).unwrap().unwrap();
        match f2 {
            EngineFrame::Text(s) => assert_eq!(s, "Hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut s = ReassemblyState::new(1024);
        let err = s.feed(DecodedFrame {
            fin: true, opcode: Opcode::Text, payload: Bytes::from_static(&[0xff, 0xfe])
        }).unwrap_err();
        matches!(err, WebSocketEngineError::InvalidUtf8);
    }

    #[test]
    fn rejects_message_over_limit() {
        let mut s = ReassemblyState::new(2);
        let err = s.feed(DecodedFrame {
            fin: true, opcode: Opcode::Binary, payload: Bytes::from_static(&[1, 2, 3])
        }).unwrap_err();
        matches!(err, WebSocketEngineError::PayloadTooLarge { .. });
    }

    #[test]
    fn parses_close_payload() {
        let mut s = ReassemblyState::new(1024);
        let mut payload = vec![0x03, 0xe8]; // 1000
        payload.extend_from_slice(b"bye");
        let f = s.feed(DecodedFrame {
            fin: true, opcode: Opcode::Close, payload: Bytes::from(payload)
        }).unwrap().unwrap();
        match f {
            EngineFrame::Close { code, reason } => {
                assert_eq!(code, 1000);
                assert_eq!(reason, "bye");
            }
            _ => panic!(),
        }
    }
}
```

- [ ] **Step 2: Implement `ReassemblyState`**

```rust
use bytes::{Bytes, BytesMut};
use openwire_core::websocket::{EngineFrame, WebSocketEngineError};

use super::codec::{DecodedFrame, Opcode, close_code_is_valid};

pub(crate) struct ReassemblyState {
    max_message_size: usize,
    in_progress: Option<InProgress>,
}

struct InProgress {
    opcode: Opcode,
    buffer: BytesMut,
}

impl ReassemblyState {
    pub(crate) fn new(max_message_size: usize) -> Self {
        Self { max_message_size, in_progress: None }
    }

    pub(crate) fn feed(
        &mut self,
        frame: DecodedFrame,
    ) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        if frame.opcode.is_control() {
            return Ok(Some(self.parse_control(frame)?));
        }

        match (frame.opcode, &mut self.in_progress) {
            (Opcode::Continuation, None) => {
                Err(WebSocketEngineError::InvalidFrame("orphan continuation".into()))
            }
            (Opcode::Continuation, Some(state)) => {
                self.append(state.opcode, frame, true)
            }
            (op @ (Opcode::Text | Opcode::Binary), None) => {
                if frame.fin {
                    self.deliver_single(op, frame.payload)
                } else {
                    self.in_progress = Some(InProgress { opcode: op, buffer: BytesMut::from(&frame.payload[..]) });
                    Ok(None)
                }
            }
            (Opcode::Text | Opcode::Binary, Some(_)) => {
                Err(WebSocketEngineError::InvalidFrame("nested data frame".into()))
            }
            (Opcode::Close | Opcode::Ping | Opcode::Pong, _) => unreachable!("handled above"),
        }
    }

    fn append(
        &mut self,
        opcode: Opcode,
        frame: DecodedFrame,
        _is_continuation: bool,
    ) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        let state = self.in_progress.as_mut().expect("checked above");
        let new_len = state.buffer.len() + frame.payload.len();
        if new_len > self.max_message_size {
            return Err(WebSocketEngineError::PayloadTooLarge {
                limit: self.max_message_size, received: new_len,
            });
        }
        state.buffer.extend_from_slice(&frame.payload);

        if frame.fin {
            let buffer = std::mem::replace(&mut state.buffer, BytesMut::new()).freeze();
            self.in_progress = None;
            self.deliver_single(opcode, buffer)
        } else {
            Ok(None)
        }
    }

    fn deliver_single(&self, opcode: Opcode, payload: Bytes) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        match opcode {
            Opcode::Text => {
                let s = std::str::from_utf8(&payload)
                    .map_err(|_| WebSocketEngineError::InvalidUtf8)?
                    .to_string();
                Ok(Some(EngineFrame::Text(s)))
            }
            Opcode::Binary => Ok(Some(EngineFrame::Binary(payload))),
            _ => unreachable!(),
        }
    }

    fn parse_control(&self, frame: DecodedFrame) -> Result<EngineFrame, WebSocketEngineError> {
        match frame.opcode {
            Opcode::Ping => Ok(EngineFrame::Ping(frame.payload)),
            Opcode::Pong => Ok(EngineFrame::Pong(frame.payload)),
            Opcode::Close => {
                if frame.payload.is_empty() {
                    Ok(EngineFrame::Close { code: 1005, reason: String::new() })
                } else if frame.payload.len() == 1 {
                    Err(WebSocketEngineError::InvalidFrame("close payload of length 1".into()))
                } else {
                    let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                    if !close_code_is_valid(code) {
                        return Err(WebSocketEngineError::InvalidCloseCode(code));
                    }
                    let reason = std::str::from_utf8(&frame.payload[2..])
                        .map_err(|_| WebSocketEngineError::InvalidUtf8)?
                        .to_string();
                    Ok(EngineFrame::Close { code, reason })
                }
            }
            _ => unreachable!(),
        }
    }
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native::session
git add crates/openwire/src/websocket/native/session.rs crates/openwire/src/websocket/native/mod.rs
git commit -m "feat(websocket-native): reassembly + close payload parsing"
```

---

### Task 16: `NativeEngine` impl that wires codec + session over `BoxConnection`

**Files:**
- Create: `crates/openwire/src/websocket/native/engine.rs`

This is the longest single task — it implements the engine trait against the codec/session infrastructure already built. The engine spawns no tasks itself; it returns Sink/Stream that drive IO when polled.

- [ ] **Step 1: Engine struct**

```rust
use std::pin::Pin;
use std::task::{Context, Poll};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use futures_util::{ready, sink::Sink};
use hyper::rt::{Read, Write};
use openwire_core::transport::BoxConnection;
use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Role, WebSocketChannel, WebSocketEngine,
    WebSocketEngineConfig, WebSocketEngineError,
};
use openwire_core::{BoxFuture, WireError};
use tokio::sync::Mutex;

use super::codec::{DecodedFrame, FrameHeader, Opcode, decode_frame, encode_frame};
use super::mask::random_mask_key;
use super::session::ReassemblyState;

#[derive(Default)]
pub struct NativeEngine;

impl NativeEngine {
    pub fn new() -> Self { Self }
    pub fn shared() -> Arc<Self> { Arc::new(Self) }
}

impl WebSocketEngine for NativeEngine {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<'static, Result<WebSocketChannel, WebSocketEngineError>> {
        Box::pin(async move {
            if config.role != Role::Client {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    "native engine only supports client role in v1".into(),
                ));
            }
            if !config.extensions.iter().all(|e| e.is_empty()) {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    config.extensions.join(", "),
                ));
            }
            let shared = Arc::new(Mutex::new(io));
            let send: BoxEngineSink = Box::pin(NativeSink {
                io: shared.clone(),
                buf: BytesMut::with_capacity(8 * 1024),
                state: SinkState::Idle,
            });
            let recv: BoxEngineStream = Box::pin(NativeStream {
                io: shared,
                buf: BytesMut::with_capacity(8 * 1024),
                reassembly: ReassemblyState::new(config.max_message_size),
                max_frame_size: config.max_frame_size,
                done: false,
            });
            Ok(WebSocketChannel { send, recv })
        })
    }
}
```

- [ ] **Step 2: `NativeSink` (Sink<EngineFrame>)**

```rust
enum SinkState { Idle, Writing(usize) }

struct NativeSink {
    io: Arc<Mutex<BoxConnection>>,
    buf: BytesMut,
    state: SinkState,
}

impl Sink<EngineFrame> for NativeSink {
    type Error = WebSocketEngineError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
        let key = random_mask_key();
        let header = match &item {
            EngineFrame::Text(_) => FrameHeader { fin: true, opcode: Opcode::Text },
            EngineFrame::Binary(_) => FrameHeader { fin: true, opcode: Opcode::Binary },
            EngineFrame::Ping(_) => FrameHeader { fin: true, opcode: Opcode::Ping },
            EngineFrame::Pong(_) => FrameHeader { fin: true, opcode: Opcode::Pong },
            EngineFrame::Close { .. } => FrameHeader { fin: true, opcode: Opcode::Close },
        };
        let payload = match item {
            EngineFrame::Text(s) => Bytes::from(s.into_bytes()),
            EngineFrame::Binary(b) | EngineFrame::Ping(b) | EngineFrame::Pong(b) => b,
            EngineFrame::Close { code, reason } => {
                let mut p = BytesMut::with_capacity(2 + reason.len());
                p.extend_from_slice(&code.to_be_bytes());
                p.extend_from_slice(reason.as_bytes());
                p.freeze()
            }
        };
        encode_frame(&mut self.buf, header, &payload, Some(key));
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let me = self.get_mut();
        if me.buf.is_empty() { return Poll::Ready(Ok(())); }

        let mut io = match me.io.try_lock() {
            Ok(g) => g,
            Err(_) => { cx.waker().wake_by_ref(); return Poll::Pending; }
        };
        let pinned = Pin::new(&mut **io);
        // hyper::rt::Write::poll_write returns Poll<Result<usize, std::io::Error>>
        match pinned.poll_write(cx, me.buf.as_ref()) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(n)) => {
                me.buf.advance(n);
                if me.buf.is_empty() { Poll::Ready(Ok(())) } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(WebSocketEngineError::Io(WireError::from(e)))),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // After flushing, do not actively shutdown — close handshake is the user's job;
        // dropping the connection elsewhere triggers TCP teardown.
        Self::poll_flush(self, cx)
    }
}
```

(Note: `bytes::BytesMut::advance` requires `bytes::Buf`. Engineer adds the import.)

- [ ] **Step 3: `NativeStream` (Stream<Result<EngineFrame, _>>)**

```rust
struct NativeStream {
    io: Arc<Mutex<BoxConnection>>,
    buf: BytesMut,
    reassembly: ReassemblyState,
    max_frame_size: usize,
    done: bool,
}

impl Stream for NativeStream {
    type Item = Result<EngineFrame, WebSocketEngineError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        loop {
            if me.done { return Poll::Ready(None); }

            // try decode first
            match decode_frame(&mut me.buf, me.max_frame_size) {
                Err(e) => return Poll::Ready(Some(Err(e))),
                Ok(Some(frame)) => {
                    let was_close = frame.opcode == Opcode::Close;
                    match me.reassembly.feed(frame) {
                        Err(e) => return Poll::Ready(Some(Err(e))),
                        Ok(Some(ef)) => {
                            if was_close { me.done = true; }
                            return Poll::Ready(Some(Ok(ef)));
                        }
                        Ok(None) => continue,
                    }
                }
                Ok(None) => {
                    // need more bytes
                    let mut io = match me.io.try_lock() {
                        Ok(g) => g,
                        Err(_) => { cx.waker().wake_by_ref(); return Poll::Pending; }
                    };
                    let mut chunk = [0u8; 8192];
                    let read_buf = hyper::rt::ReadBuf::new(&mut chunk);
                    let mut read_buf = read_buf;
                    let pinned = Pin::new(&mut **io);
                    match pinned.poll_read(cx, read_buf.unfilled()) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(())) => {
                            let filled = read_buf.filled();
                            if filled.is_empty() {
                                me.done = true;
                                return Poll::Ready(None);
                            }
                            me.buf.extend_from_slice(filled);
                            // loop again to try decode
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Some(Err(WebSocketEngineError::Io(
                                WireError::from(e),
                            ))));
                        }
                    }
                }
            }
        }
    }
}
```

(The exact `hyper::rt::ReadBuf` API may differ slightly; the engineer reads `crates/openwire-tokio/src/lib.rs` which already uses it for examples.)

- [ ] **Step 4: Add an integration-style test using a duplex pipe**

`crates/openwire/src/websocket/native/engine_tests.rs` (or `#[cfg(test)] mod tests`):

Use `tokio::io::duplex(1024)` and a `TokioIo` wrapper to satisfy `BoxConnection`. Server side echoes a single Text("hi"); client opens engine, reads one frame, asserts `Text("hi")`.

```rust
#[tokio::test]
async fn echoes_a_text_message() {
    use tokio::io::AsyncWriteExt;
    let (client_tcp, mut server_tcp) = tokio::io::duplex(1024);

    // Server writes a server-side WS Text frame: FIN=1 opcode=Text len=2 "hi", no mask.
    server_tcp.write_all(&[0x81, 0x02, b'h', b'i']).await.unwrap();

    let io: BoxConnection = Box::new(openwire_tokio::TokioIo::new(client_tcp));
    let engine = NativeEngine::new();
    let cfg = WebSocketEngineConfig {
        role: Role::Client,
        subprotocol: None,
        extensions: vec![],
        max_frame_size: 1024,
        max_message_size: 1024,
    };
    let mut ch = engine.upgrade(io, cfg).await.unwrap();
    let frame = futures_util::StreamExt::next(&mut ch.recv).await.unwrap().unwrap();
    match frame { EngineFrame::Text(s) => assert_eq!(s, "hi"), _ => panic!() }
}
```

- [ ] **Step 5: Run + commit**

```bash
cargo test -p openwire --features websocket --lib websocket::native
git add crates/openwire/src/websocket/native/engine.rs
git commit -m "feat(websocket-native): WebSocketEngine impl over BoxConnection"
```

---

## Phase 5 — Transport Branch & Public API

### Task 17: `bind_websocket_handshake` — drive a 1-shot HTTP/1.1 GET and capture upgraded IO

**Files:**
- Modify: `crates/openwire/src/transport/protocol.rs`
- Create: `crates/openwire/src/websocket/transport.rs`

The plan: the existing `bind_http1` returns `(SendRequest, Connection)` from hyper. We send the request, read the 101 response, then call `hyper::upgrade::on(response)` to get `Upgraded`. Implementation note: `Connection` is a future that drives the IO; we must spawn it (and abort it) carefully so that after upgrade we no longer have `Connection` running over the IO.

In hyper 1.x the idiomatic flow is:

1. `let (send_request, connection) = hyper::client::conn::http1::handshake(io).await?;`
2. Spawn `connection.with_upgrades()` (note: `.with_upgrades()` is essential — without it, `hyper::upgrade::on` does not receive the IO).
3. `let response = send_request.send_request(req).await?;`
4. `let upgraded = hyper::upgrade::on(response).await?;` — yields `hyper::upgrade::Upgraded`.
5. Wrap `upgraded` as a `BoxConnection`.

- [ ] **Step 1: Implement `bind_websocket_handshake` in `transport/protocol.rs`**

```rust
#[cfg(feature = "websocket")]
pub(crate) async fn bind_websocket_handshake(
    io: BoxConnection,
    request: http::Request<RequestBody>,
) -> Result<(http::Response<()>, hyper::upgrade::Upgraded), WireError> {
    let (mut send, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| WireError::from(e))?;

    let conn_handle = tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let response = send.send_request(request).await.map_err(WireError::from)?;
    let (parts, body) = response.into_parts();
    let body_response = http::Response::from_parts(parts.clone(), body);

    let upgraded = hyper::upgrade::on(body_response)
        .await
        .map_err(WireError::from)?;

    let _ = conn_handle; // drops the join handle; the task is done after upgrade.
    let resp_no_body = http::Response::from_parts(parts, ());
    Ok((resp_no_body, upgraded))
}
```

(Engineer verifies `with_upgrades()` exists in their hyper 1.8.1 — if not, the upgrade flow uses a slightly different shape; consult hyper's `client/conn/http1` module docs.)

- [ ] **Step 2: Wrap `Upgraded` as `BoxConnection`**

The `Upgraded` type implements `tokio::io::AsyncRead + AsyncWrite` (via `hyper-util` or directly). We need a `hyper::rt::Read + Write` variant. `openwire-tokio::TokioIo` already does the bridge in the other direction; ensure the reverse adapter exists or add one to `crates/openwire-tokio/src/lib.rs`.

```rust
pub fn upgraded_into_box_connection(u: hyper::upgrade::Upgraded) -> BoxConnection {
    Box::new(openwire_tokio::TokioIo::new(u))
}
```

If `TokioIo` already wraps any `tokio::io` type, this is a one-liner. If not, write a fresh adapter mirroring the existing one.

- [ ] **Step 3: Implement `crates/openwire/src/websocket/transport.rs`**

The branch in `TransportService` is the gluing logic:

```rust
use std::sync::Arc;

use openwire_core::websocket::{
    Role, SharedWebSocketEngine, WebSocketEngineConfig, WebSocketChannel,
};
use openwire_core::WireError;

use super::handshake::{validate_handshake_response, ValidatedHandshake};

pub(crate) async fn execute_websocket_handshake(
    io: openwire_core::transport::BoxConnection,
    request: http::Request<openwire_core::RequestBody>,
    expected_accept: &str,
    offered_subprotocols: &[String],
    engine: SharedWebSocketEngine,
    max_frame_size: usize,
    max_message_size: usize,
) -> Result<(http::Response<()>, WebSocketChannel, ValidatedHandshake), WireError> {
    let (response, upgraded) =
        crate::transport::protocol::bind_websocket_handshake(io, request).await?;

    let handshake = validate_handshake_response(&response, expected_accept, offered_subprotocols)
        .map_err(|f| WireError::websocket_handshake(f, response.status()))?;

    let cfg = WebSocketEngineConfig {
        role: Role::Client,
        subprotocol: handshake.subprotocol.clone(),
        extensions: handshake.extensions.clone(),
        max_frame_size,
        max_message_size,
    };
    let io_for_engine = crate::transport::protocol::upgraded_into_box_connection(upgraded);
    let channel = engine
        .upgrade(io_for_engine, cfg)
        .await
        .map_err(WireError::from)?;
    Ok((response, channel, handshake))
}
```

(`WireError::websocket_handshake` is a new constructor on `WireError`; engineer adds it analogous to existing variants like `WireError::invalid_request`.)

- [ ] **Step 4: Smoke test using `openwire-test`'s plain HTTP server**

Add an integration test under `crates/openwire/tests/websocket_handshake.rs` that spins a vanilla HTTP server which returns a hand-crafted 101 with valid headers and asserts the handshake completes. Full message exchange comes in later tasks.

- [ ] **Step 5: Run + commit**

```bash
cargo test -p openwire --features websocket --test websocket_handshake
git add crates/openwire/src/transport/protocol.rs crates/openwire/src/websocket/transport.rs
git add crates/openwire-tokio/src/lib.rs crates/openwire/tests/websocket_handshake.rs
git commit -m "feat(websocket): handshake transport branch via hyper::upgrade"
```

---

### Task 18: `WebSocketHandshake` + `Message` <-> `EngineFrame` glue

**Files:**
- Create: `crates/openwire/src/websocket/public.rs`

- [ ] **Step 1: Define the public types (handshake info only — sender/receiver come next)**

```rust
use http::{HeaderMap, StatusCode};

#[derive(Debug, Clone)]
pub struct WebSocketHandshake {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) subprotocol: Option<String>,
    pub(crate) extensions: Vec<String>,
}

impl WebSocketHandshake {
    pub fn status(&self) -> StatusCode { self.status }
    pub fn headers(&self) -> &HeaderMap { &self.headers }
    pub fn subprotocol(&self) -> Option<&str> { self.subprotocol.as_deref() }
    pub fn extensions(&self) -> &[String] { &self.extensions }
}
```

- [ ] **Step 2: Add `Message <-> EngineFrame` conversions**

```rust
use openwire_core::websocket::{EngineFrame, Message};

impl From<Message> for EngineFrame {
    fn from(m: Message) -> Self {
        match m {
            Message::Text(s) => EngineFrame::Text(s),
            Message::Binary(b) => EngineFrame::Binary(b),
            Message::Ping(b) => EngineFrame::Ping(b),
            Message::Pong(b) => EngineFrame::Pong(b),
            Message::Close { code, reason } => EngineFrame::Close { code, reason },
        }
    }
}

impl From<EngineFrame> for Message {
    fn from(f: EngineFrame) -> Self {
        match f {
            EngineFrame::Text(s) => Message::Text(s),
            EngineFrame::Binary(b) => Message::Binary(b),
            EngineFrame::Ping(b) => Message::Ping(b),
            EngineFrame::Pong(b) => Message::Pong(b),
            EngineFrame::Close { code, reason } => Message::Close { code, reason },
        }
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/openwire/src/websocket/public.rs
git commit -m "feat(websocket): public handshake type and message conversions"
```

---

### Task 19: `WebSocketSender` (Arc + mpsc), `WebSocketReceiver` (Stream)

**Files:**
- Modify: `crates/openwire/src/websocket/public.rs`
- Modify: `crates/openwire/src/websocket/writer.rs`

- [ ] **Step 1: Define `WriterCommand`**

In `writer.rs`:

```rust
use bytes::Bytes;
use openwire_core::websocket::{Message, WebSocketError};

pub(crate) enum WriterCommand {
    Send(Message),
    Pong(Bytes),
    Ping(Bytes),
    Close { code: u16, reason: String, ack: tokio::sync::oneshot::Sender<()> },
    Cancel,
}
```

- [ ] **Step 2: Define `WebSocketSender`**

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use tokio::sync::mpsc;

use openwire_core::websocket::{Message, WebSocketError};
use crate::websocket::writer::WriterCommand;

#[derive(Clone)]
pub struct WebSocketSender {
    inner: Arc<SenderInner>,
}

pub(crate) struct SenderInner {
    tx: mpsc::Sender<WriterCommand>,
    closed: AtomicBool,
}

impl WebSocketSender {
    pub(crate) fn new(tx: mpsc::Sender<WriterCommand>) -> Self {
        Self { inner: Arc::new(SenderInner { tx, closed: AtomicBool::new(false) }) }
    }

    pub async fn send(&self, msg: Message) -> Result<(), WebSocketError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(WebSocketError::LocalCancelled);
        }
        self.inner.tx.send(WriterCommand::Send(msg)).await
            .map_err(|_| WebSocketError::LocalCancelled)
    }

    pub async fn send_text(&self, t: impl Into<String>) -> Result<(), WebSocketError> {
        self.send(Message::Text(t.into())).await
    }

    pub async fn send_binary(&self, b: impl Into<Bytes>) -> Result<(), WebSocketError> {
        self.send(Message::Binary(b.into())).await
    }

    pub async fn close(&self, code: u16, reason: impl Into<String>) -> Result<(), WebSocketError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) { return Ok(()); }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.inner.tx.send(WriterCommand::Close { code, reason: reason.into(), ack: ack_tx }).await
            .map_err(|_| WebSocketError::LocalCancelled)?;
        let _ = ack_rx.await;
        Ok(())
    }

    pub fn queue_size(&self) -> usize {
        self.inner.tx.max_capacity() - self.inner.tx.capacity()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}
```

- [ ] **Step 3: Define `WebSocketReceiver`**

```rust
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::mpsc;

use openwire_core::websocket::{Message, WebSocketError};

pub struct WebSocketReceiver {
    pub(crate) rx: mpsc::Receiver<Result<Message, WebSocketError>>,
}

impl Stream for WebSocketReceiver {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}
```

- [ ] **Step 4: Define `WebSocket`**

```rust
pub struct WebSocket {
    pub(crate) sender: WebSocketSender,
    pub(crate) receiver: WebSocketReceiver,
    pub(crate) handshake: WebSocketHandshake,
}

impl WebSocket {
    pub fn handshake(&self) -> &WebSocketHandshake { &self.handshake }
    pub fn sender(&self) -> WebSocketSender { self.sender.clone() }
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        (self.sender, self.receiver)
    }
}
```

- [ ] **Step 5: Compile + commit**

```bash
cargo check -p openwire --features websocket
git add crates/openwire/src/websocket/public.rs crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): WebSocketSender / Receiver / WebSocket types"
```

---

### Task 20: Writer task (drives engine sink, handles close handshake)

**Files:**
- Modify: `crates/openwire/src/websocket/writer.rs`

- [ ] **Step 1: Spawn-able writer**

```rust
use std::time::Duration;

use futures_util::SinkExt;
use openwire_core::websocket::{
    BoxEngineSink, EngineFrame, Message, WebSocketError, WebSocketEngineError,
};
use tokio::sync::mpsc;

pub(crate) async fn run_writer(
    mut sink: BoxEngineSink,
    mut commands: mpsc::Receiver<WriterCommand>,
    close_timeout: Duration,
    receiver_tx: mpsc::Sender<Result<Message, WebSocketError>>,
) {
    while let Some(cmd) = commands.recv().await {
        let frame_opt: Option<EngineFrame> = match cmd {
            WriterCommand::Send(m) => Some(m.into()),
            WriterCommand::Ping(p) => Some(EngineFrame::Ping(p)),
            WriterCommand::Pong(p) => Some(EngineFrame::Pong(p)),
            WriterCommand::Close { code, reason, ack } => {
                let _ = sink.send(EngineFrame::Close { code, reason }).await;
                let _ = sink.flush().await;
                let _ = tokio::time::timeout(close_timeout, async {
                    while let Some(c) = commands.recv().await {
                        if matches!(c, WriterCommand::Cancel) { break; }
                    }
                }).await;
                let _ = ack.send(());
                let _ = receiver_tx
                    .send(Err(WebSocketError::LocalCancelled))
                    .await;
                return;
            }
            WriterCommand::Cancel => {
                let _ = sink.flush().await;
                return;
            }
        };
        if let Some(f) = frame_opt {
            if let Err(WebSocketEngineError::Io(e)) = sink.send(f).await {
                let _ = receiver_tx.send(Err(WebSocketError::Io(e))).await;
                return;
            }
        }
    }
}
```

- [ ] **Step 2: Test the writer with a fake sink**

Use `futures_util::sink::drain()` to provide a no-op `BoxEngineSink`; feed `WriterCommand::Send(text)`, then `Cancel`; assert it returns.

- [ ] **Step 3: Commit**

```bash
git add crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): writer task draining WriterCommands to engine sink"
```

---

### Task 21: Reader task + drop semantics

**Files:**
- Modify: `crates/openwire/src/websocket/writer.rs` (add `run_reader` + glue function `spawn_session`)

- [ ] **Step 1: Reader task**

```rust
use futures_util::StreamExt;
use openwire_core::websocket::BoxEngineStream;

pub(crate) async fn run_reader(
    mut stream: BoxEngineStream,
    deliver_control_frames: bool,
    out: mpsc::Sender<Result<Message, WebSocketError>>,
    auto_pong: mpsc::Sender<WriterCommand>,
) {
    while let Some(item) = stream.next().await {
        match item {
            Ok(EngineFrame::Ping(p)) => {
                let _ = auto_pong.send(WriterCommand::Pong(p.clone())).await;
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Ping(p))).await;
                }
            }
            Ok(EngineFrame::Pong(p)) if deliver_control_frames => {
                let _ = out.send(Ok(Message::Pong(p))).await;
            }
            Ok(EngineFrame::Pong(_)) => {} // absorbed
            Ok(EngineFrame::Close { code, reason }) => {
                let _ = out.send(Err(WebSocketError::ClosedByPeer { code, reason })).await;
                return;
            }
            Ok(other) => { let _ = out.send(Ok(other.into())).await; }
            Err(WebSocketEngineError::Io(e)) => {
                let _ = out.send(Err(WebSocketError::Io(e))).await;
                return;
            }
            Err(other) => {
                let _ = out.send(Err(WebSocketError::Engine(other))).await;
                return;
            }
        }
    }
}
```

- [ ] **Step 2: `spawn_session` orchestration with drop guard**

```rust
use std::time::Duration;

pub(crate) struct SessionHandles {
    pub sender_tx: mpsc::Sender<WriterCommand>,
    pub receiver_rx: mpsc::Receiver<Result<Message, WebSocketError>>,
    pub _drop_guard: DropGuard,
}

pub(crate) struct DropGuard {
    cancel_tx: mpsc::Sender<WriterCommand>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Best-effort: enqueue Close { 1001, "client cancelled" } if room, else Cancel.
        let _ = self.cancel_tx.try_send(WriterCommand::Cancel);
    }
}

pub(crate) fn spawn_session(
    channel: openwire_core::websocket::WebSocketChannel,
    queue_size: usize,
    deliver_control_frames: bool,
    close_timeout: Duration,
) -> SessionHandles {
    let (sender_tx, sender_rx) = mpsc::channel::<WriterCommand>(queue_size);
    let (recv_tx, recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(queue_size);
    let auto_pong_tx = sender_tx.clone();

    tokio::spawn(run_writer(channel.send, sender_rx, close_timeout, recv_tx.clone()));
    tokio::spawn(run_reader(channel.recv, deliver_control_frames, recv_tx, auto_pong_tx));

    SessionHandles {
        sender_tx: sender_tx.clone(),
        receiver_rx: recv_rx,
        _drop_guard: DropGuard { cancel_tx: sender_tx },
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): reader task + drop guard"
```

---

### Task 22: `Client::new_websocket` and `Call::websocket()` builder

**Files:**
- Modify: `crates/openwire/src/client.rs`

- [ ] **Step 1: Add `WebSocketCall` builder**

```rust
#[cfg(feature = "websocket")]
pub struct WebSocketCall<'a> {
    client: &'a Client,
    request: Request<RequestBody>,
    handshake_timeout: Option<Duration>,
    close_timeout: Option<Duration>,
    max_frame_size: Option<usize>,
    max_message_size: Option<usize>,
    send_queue_size: Option<usize>,
    ping_interval: Option<Duration>,
    pong_timeout: Option<Duration>,
    subprotocols: Vec<String>,
    deliver_control_frames: bool,
    engine: Option<openwire_core::websocket::SharedWebSocketEngine>,
}

#[cfg(feature = "websocket")]
impl<'a> WebSocketCall<'a> {
    pub fn handshake_timeout(mut self, d: Duration) -> Self { self.handshake_timeout = Some(d); self }
    pub fn close_timeout(mut self, d: Duration) -> Self { self.close_timeout = Some(d); self }
    pub fn max_frame_size(mut self, n: usize) -> Self { self.max_frame_size = Some(n); self }
    pub fn max_message_size(mut self, n: usize) -> Self { self.max_message_size = Some(n); self }
    pub fn send_queue_size(mut self, n: usize) -> Self { self.send_queue_size = Some(n); self }
    pub fn ping_interval(mut self, d: Duration) -> Self { self.ping_interval = Some(d); self }
    pub fn pong_timeout(mut self, d: Duration) -> Self { self.pong_timeout = Some(d); self }
    pub fn subprotocols<I, S>(mut self, p: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.subprotocols = p.into_iter().map(Into::into).collect();
        self
    }
    pub fn deliver_control_frames(mut self, on: bool) -> Self { self.deliver_control_frames = on; self }
    pub fn engine(mut self, e: openwire_core::websocket::SharedWebSocketEngine) -> Self {
        self.engine = Some(e);
        self
    }

    pub async fn execute(self) -> Result<crate::websocket::WebSocket, openwire_core::websocket::WebSocketError> {
        crate::websocket::transport::execute(self).await
    }
}
```

- [ ] **Step 2: Add entry on `Client`**

```rust
#[cfg(feature = "websocket")]
impl Client {
    pub fn new_websocket(&self, request: Request<RequestBody>) -> WebSocketCall<'_> {
        WebSocketCall {
            client: self,
            request,
            handshake_timeout: None,
            close_timeout: None,
            max_frame_size: None,
            max_message_size: None,
            send_queue_size: None,
            ping_interval: None,
            pong_timeout: None,
            subprotocols: Vec::new(),
            deliver_control_frames: false,
            engine: None,
        }
    }
}
```

- [ ] **Step 3: Implement `crate::websocket::transport::execute`**

This is the integration point that:
1. Attaches `WebSocketRequestMarker` to the request.
2. Acquires a `ConnectionPermit` via the shared `ConnectionLimiter`.
3. Drives the existing interceptor chain to completion (returns `(Response, BoxConnection)` — needs a small extension to `TransportService` to return the upgraded IO when the marker is set; see Task 23).
4. Calls `execute_websocket_handshake`.
5. Spawns session, builds `WebSocket`, returns it.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire/src/client.rs crates/openwire/src/websocket/transport.rs
git commit -m "feat(websocket): Client::new_websocket entry and WebSocketCall builder"
```

---

### Task 23: `TransportService` branch — return `BoxConnection` when WS marker present

**Files:**
- Modify: `crates/openwire/src/transport/service.rs`

This is the riskiest integration. The current `TransportService` runs a request to completion through hyper and returns a `Response<ResponseBody>`. For WS we want to: acquire a connection (via `ConnectorStack`), but **not** call `bind_http1` — instead, return the raw `BoxConnection` to the WS branch, which then runs `bind_websocket_handshake`.

Two approaches:

a. Add a `TransportRequest` variant for WS that short-circuits before binding.
b. Run the full HTTP/1.1 binding, send the request, get back a 101 response, then call `hyper::upgrade::on(response)`.

(b) is what the spec describes (§4.7.4) and what mainline hyper users do. It also means we get to reuse the existing `bind_http1` and the connection lifecycle stays in the existing pipeline up to the response — only after 101 do we diverge. This is also less invasive.

- [ ] **Step 1: In `TransportService`, detect WS marker and divert post-response**

The cleanest place is right after the response headers are received. The bridge has set `RoutePreference::Http1Only` and the marker is present in extensions. After bind_http1 + send_request, the existing code returns a `Response<Incoming>`. We split the path:

```rust
#[cfg(feature = "websocket")]
let is_websocket = exchange.request().extensions().get::<WebSocketRequestMarker>().is_some();

// existing code drives to response_parts + body
let response = ...;

#[cfg(feature = "websocket")]
if is_websocket {
    // Don't deliver via ResponseBody; deliver as upgrade.
    let upgraded = hyper::upgrade::on(response).await.map_err(WireError::from)?;
    return Ok(TransportOutcome::WebSocket { response_parts, upgraded });
}
```

`TransportOutcome` becomes an enum:

```rust
pub(crate) enum TransportOutcome {
    Http(http::Response<ResponseBody>),
    #[cfg(feature = "websocket")]
    WebSocket { response_parts: http::response::Parts, upgraded: hyper::upgrade::Upgraded },
}
```

- [ ] **Step 2: Adapt `Client::execute` and the WS execute path to handle the enum**

`Client::execute` always expects `Http`; if it ever sees `WebSocket`, that's a bug (panic with assertion). The WS execute path expects `WebSocket`; if it sees `Http`, that's an "upgrade missing" error.

- [ ] **Step 3: Wire the WS branch end-to-end**

In `crate::websocket::transport::execute`, after running the chain:

1. Get `TransportOutcome::WebSocket { response_parts, upgraded }`.
2. Reconstruct `http::Response<()>` from `response_parts`.
3. Validate handshake.
4. Wrap upgraded as `BoxConnection`.
5. Build `WebSocketEngineConfig` from negotiated values + per-call config.
6. Call `engine.upgrade(...).await`.
7. Spawn writer/reader, build `WebSocket`.

- [ ] **Step 4: Integration test: full handshake with native engine over a duplex pipe**

```rust
#[tokio::test]
#[cfg(feature = "websocket")]
async fn websocket_echo_against_test_server() {
    let server = openwire_test::spawn_websocket_echo();
    let client = Client::builder().build().unwrap();
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("ws://{}/", server.addr()))
        .body(RequestBody::empty()).unwrap();
    let ws = client.new_websocket(req).execute().await.unwrap();

    let (tx, mut rx) = ws.split();
    tx.send_text("hello").await.unwrap();
    match rx.next().await.unwrap().unwrap() {
        Message::Text(s) => assert_eq!(s, "hello"),
        _ => panic!(),
    }
    tx.close(1000, "bye").await.unwrap();
}
```

The test server is added in Task 28.

- [ ] **Step 5: Commit**

```bash
git add crates/openwire/src/transport/service.rs crates/openwire/src/client.rs \
        crates/openwire/src/websocket/transport.rs
git commit -m "feat(websocket): TransportService branch for upgrade and end-to-end glue"
```

---

## Phase 6 — Heartbeat & Observability

### Task 24: Heartbeat timer task

**Files:**
- Modify: `crates/openwire/src/websocket/writer.rs`

- [ ] **Step 1: Pong tracker**

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct PongTracker {
    last_pong: Arc<AtomicU64>, // millis since session start
    start: Arc<Instant>,
}

impl PongTracker {
    pub(crate) fn new() -> Self {
        Self { last_pong: Arc::new(AtomicU64::new(0)), start: Arc::new(Instant::now()) }
    }
    pub(crate) fn mark(&self) {
        self.last_pong.store(self.start.elapsed().as_millis() as u64, Ordering::Release);
    }
    pub(crate) fn since_last_pong(&self) -> Duration {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last_pong.load(Ordering::Acquire);
        Duration::from_millis(now.saturating_sub(last))
    }
}
```

The reader calls `tracker.mark()` on every `Pong` frame.

- [ ] **Step 2: Timer task**

```rust
pub(crate) async fn run_heartbeat(
    interval: Duration,
    pong_timeout: Duration,
    tracker: PongTracker,
    out: mpsc::Sender<WriterCommand>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if out.send(WriterCommand::Ping(bytes::Bytes::new())).await.is_err() { return; }
        if tracker.since_last_pong() > pong_timeout {
            let (ack_tx, _) = tokio::sync::oneshot::channel();
            let _ = out.send(WriterCommand::Close {
                code: 1011, reason: "ping timeout".into(), ack: ack_tx,
            }).await;
            return;
        }
    }
}
```

- [ ] **Step 3: Wire from `spawn_session` when ping_interval is set; commit**

```bash
git add crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): heartbeat scheduler with pong timeout"
```

---

### Task 25: EventListener event additions (no-op defaults)

**Files:**
- Modify: `crates/openwire-core/src/event.rs`

- [ ] **Step 1: Add methods**

```rust
#[cfg(feature = "websocket")]
use crate::websocket::{Message, MessageKind, CloseInitiator, WebSocketError};
#[cfg(feature = "websocket")]
use crate::websocket::WebSocketHandshake; // forward-declare or re-import as needed

pub trait EventListener: Send + Sync {
    // … existing …

    #[cfg(feature = "websocket")]
    fn websocket_open(&self, _ctx: &CallContext, _handshake: &crate::websocket::WebSocketHandshake) {}
    #[cfg(feature = "websocket")]
    fn websocket_message_sent(&self, _ctx: &CallContext, _kind: MessageKind, _payload_len: usize) {}
    #[cfg(feature = "websocket")]
    fn websocket_message_received(&self, _ctx: &CallContext, _kind: MessageKind, _payload_len: usize) {}
    #[cfg(feature = "websocket")]
    fn websocket_ping_sent(&self, _ctx: &CallContext) {}
    #[cfg(feature = "websocket")]
    fn websocket_pong_received(&self, _ctx: &CallContext) {}
    #[cfg(feature = "websocket")]
    fn websocket_closing(&self, _ctx: &CallContext, _code: u16, _reason: &str, _initiator: CloseInitiator) {}
    #[cfg(feature = "websocket")]
    fn websocket_closed(&self, _ctx: &CallContext, _code: u16, _reason: &str) {}
    #[cfg(feature = "websocket")]
    fn websocket_failed(&self, _ctx: &CallContext, _error: &WebSocketError) {}
}
```

`WebSocketHandshake` is in `crates/openwire/...` not `openwire-core`. Either move the public type into `openwire-core/src/websocket/handshake_info.rs` or expose a minimal trait. Cleanest: move `WebSocketHandshake` into `openwire-core` (it's just data + accessors).

- [ ] **Step 2: Move `WebSocketHandshake` to openwire-core**

Move the struct from `crates/openwire/src/websocket/public.rs` to `crates/openwire-core/src/websocket/handshake_info.rs` and re-export from `openwire-core::websocket`. The internal handshake module in `openwire/src/websocket/handshake.rs` remains, only the user-facing type moves.

- [ ] **Step 3: Verify recording listener still compiles in `openwire-test`**

```bash
cargo check -p openwire-test --features websocket
```

If `openwire-test` records via match-all event names, add the new arms.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire-core/src/event.rs crates/openwire-core/src/websocket/ \
        crates/openwire/src/websocket/public.rs
git commit -m "feat(websocket): EventListener additions for ws lifecycle"
```

---

### Task 26: Instrumentation wrapper

**Files:**
- Create: `crates/openwire/src/websocket/instrumented.rs`

Instead of weaving event emission into `run_writer` and `run_reader` directly, wrap the engine's `WebSocketChannel` in an `InstrumentedChannel` that fires events around the underlying Sink/Stream.

- [ ] **Step 1: Implement `InstrumentedSink`**

```rust
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::sink::Sink;
use openwire_core::websocket::{BoxEngineSink, EngineFrame, MessageKind, WebSocketEngineError};
use openwire_core::{CallContext, SharedEventListener};

pub(crate) struct InstrumentedSink {
    inner: BoxEngineSink,
    ctx: CallContext,
    listener: SharedEventListener,
}

impl InstrumentedSink {
    pub(crate) fn new(inner: BoxEngineSink, ctx: CallContext, listener: SharedEventListener) -> Self {
        Self { inner, ctx, listener }
    }
}

impl Sink<EngineFrame> for InstrumentedSink {
    type Error = WebSocketEngineError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
        match &item {
            EngineFrame::Text(s) => self.listener.websocket_message_sent(&self.ctx, MessageKind::Text, s.len()),
            EngineFrame::Binary(b) => self.listener.websocket_message_sent(&self.ctx, MessageKind::Binary, b.len()),
            EngineFrame::Ping(_) => self.listener.websocket_ping_sent(&self.ctx),
            _ => {}
        }
        Pin::new(&mut self.inner).start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}
```

- [ ] **Step 2: `InstrumentedStream`**

Mirror the same shape on the receive side; emit `websocket_message_received` for Text/Binary, `websocket_pong_received` for Pong, `websocket_closing(initiator=Remote)` for inbound Close.

- [ ] **Step 3: Wire in `spawn_session`**

Wrap `channel.send` and `channel.recv` before passing to `run_writer` / `run_reader`.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire/src/websocket/instrumented.rs crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): instrumentation wrapper for EventListener events"
```

---

### Task 27: Tracing spans + permit lifecycle

**Files:**
- Modify: `crates/openwire/src/websocket/transport.rs`
- Modify: `crates/openwire/src/websocket/writer.rs`

- [ ] **Step 1: Wrap `execute` in a `websocket_call` span**

```rust
#[tracing::instrument(skip_all, name = "websocket_call", fields(uri = %ws_call.request.uri()))]
pub(crate) async fn execute(ws_call: WebSocketCall<'_>) -> Result<WebSocket, WebSocketError> { ... }
```

Subspan handshake + session phases.

- [ ] **Step 2: Hold `ConnectionPermit` for the lifetime of the session**

The permit is moved into the writer task via `WriterContext` (a small struct holding the permit). When the writer task exits (close handshake / cancel / error), the permit is dropped and `EventListener::connection_released` fires.

- [ ] **Step 3: Commit**

```bash
git add crates/openwire/src/websocket/transport.rs crates/openwire/src/websocket/writer.rs
git commit -m "feat(websocket): tracing spans and permit lifecycle"
```

---

### Task 28: WebSocket test server in `openwire-test`

**Files:**
- Modify: `crates/openwire-test/Cargo.toml`
- Modify: `crates/openwire-test/src/lib.rs`

- [ ] **Step 1: Add tokio-tungstenite (test only)**

```toml
[dev-dependencies]
tokio-tungstenite = "0.24"

[features]
websocket = []
```

Note: `tokio-tungstenite` is **only** used by tests (server side); production code does not depend on it.

- [ ] **Step 2: Add server helpers**

```rust
#[cfg(feature = "websocket")]
pub fn spawn_websocket_echo() -> TestServerHandle { /* listens; on connect, accept upgrade, echo loop */ }

#[cfg(feature = "websocket")]
pub fn spawn_websocket_handler<F, Fut>(handler: F) -> TestServerHandle
where
    F: Fn(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
{ ... }
```

- [ ] **Step 3: Smoke test — echo**

A standalone test in `crates/openwire-test/tests/echo.rs` that runs the echo server and a tokio-tungstenite client and verifies a roundtrip — independent of openwire's client.

- [ ] **Step 4: Commit**

```bash
git add crates/openwire-test/Cargo.toml crates/openwire-test/src/lib.rs \
        crates/openwire-test/tests/echo.rs
git commit -m "test(websocket): test server helpers (tokio-tungstenite)"
```

---

## Phase 7 — Integration Tests

### Task 29: Integration test suite

**Files:**
- Create: `crates/openwire/tests/websocket.rs`

Cover the cases from spec §9. Not every case needs a full TDD ritual — write one test, run, fix until green, then move to next. Group by section.

- [ ] **Step 1: Handshake suite**

Tests: success; reject 200; reject 400; reject 5xx; reject missing accept; reject bad accept; reject bad upgrade header; subprotocol negotiated; subprotocol rejected (server returns one not offered); subprotocol absent when offered (allowed).

- [ ] **Step 2: Message exchange**

Text roundtrip; binary roundtrip; large message at `max_message_size`; oversize message rejected (server sends large; client errors with `PayloadTooLarge`); fragmented receive (server sends in 2 frames); UTF-8 violation (server sends bad bytes in Text).

- [ ] **Step 3: Control + close**

Server-initiated ping triggers auto-pong; client ping interval works; pong timeout fires; client-initiated close; server-initiated close; abnormal disconnect (server drops TCP).

- [ ] **Step 4: Drop, concurrent send, proxy**

Drop both halves → close 1001 within close_timeout; concurrent `send_text` from 4 cloned senders preserves total order at the server; WSS through HTTPS CONNECT proxy (reuse existing proxy test helpers).

- [ ] **Step 5: EventListener**

Capture full event sequence on a successful session; assert it matches expected.

- [ ] **Step 6: Limiter**

Set `max_connections_per_host=1`; open WS; assert `Client::new_call` to same host queues; close WS; assert HTTP call proceeds.

- [ ] **Step 7: Commit**

```bash
git add crates/openwire/tests/websocket.rs
git commit -m "test(websocket): integration suite covering spec §9"
```

---

## Phase 8 — Adapter Crates

### Task 30: `openwire-tungstenite` crate

**Files:**
- Create: `crates/openwire-tungstenite/Cargo.toml`
- Create: `crates/openwire-tungstenite/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Cargo.toml**

```toml
[package]
name = "openwire-tungstenite"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
openwire-core = { path = "../openwire-core", features = ["websocket"] }
openwire-tokio = { path = "../openwire-tokio" }
tokio-tungstenite = "0.24"
futures-util.workspace = true
bytes.workspace = true
```

Add `"crates/openwire-tungstenite"` to workspace `members`.

- [ ] **Step 2: Implement `TungsteniteEngine`**

```rust
use std::sync::Arc;

use openwire_core::transport::BoxConnection;
use openwire_core::websocket::*;
use openwire_core::BoxFuture;

#[derive(Clone, Default)]
pub struct TungsteniteEngine;

impl TungsteniteEngine {
    pub fn new() -> Self { Self }
    pub fn shared() -> Arc<Self> { Arc::new(Self) }
}

impl WebSocketEngine for TungsteniteEngine {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<'static, Result<WebSocketChannel, WebSocketEngineError>> {
        Box::pin(async move {
            // 1. Adapt BoxConnection (hyper::rt::Read + Write) to tokio::io::AsyncRead + Write
            //    via openwire-tokio's reverse adapter (added in Task 17 Step 2 if missing).
            // 2. tokio_tungstenite::WebSocketStream::from_raw_socket(adapted, Role::Client, None)
            // 3. .split() into Sink + Stream of tungstenite::Message
            // 4. Map tungstenite::Message <-> EngineFrame
            // 5. Box and return WebSocketChannel
            todo!("see openwire-fastwebsockets for shape; tungstenite version")
        })
    }
}
```

The `todo!()` is **not allowed** in the final commit — the engineer fully writes it. The mapping table:

| `tungstenite::Message` | `EngineFrame` |
| --- | --- |
| `Text(s)` | `Text(s)` |
| `Binary(b)` | `Binary(Bytes::from(b))` |
| `Ping(b)` | `Ping(Bytes::from(b))` |
| `Pong(b)` | `Pong(Bytes::from(b))` |
| `Close(Some(cf))` | `Close { code: cf.code.into(), reason: cf.reason.into_owned() }` |
| `Close(None)` | `Close { code: 1005, reason: String::new() }` |
| `Frame(_)` | unreachable in `WebSocketStream` API |

Errors from `tokio_tungstenite::Error` map: `Io(e)` → `WebSocketEngineError::Io(WireError::from(e))`, `Protocol(e)` → `InvalidFrame(e.to_string())`, `Utf8` → `InvalidUtf8`, `Capacity(c)` → `PayloadTooLarge { ... }` (best-effort with limits), others → `InvalidFrame(format!(...))`.

- [ ] **Step 3: Run the integration test suite against `TungsteniteEngine`**

Add a parameterized variant of the integration tests. Skip if feature off:

```rust
#[cfg(feature = "websocket")]
#[tokio::test]
async fn handshake_succeeds_with_tungstenite_engine() {
    let engine = openwire_tungstenite::TungsteniteEngine::shared();
    let client = Client::builder().websocket_engine(engine).build().unwrap();
    // … same body as native test …
}
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/openwire-tungstenite/
git commit -m "feat(openwire-tungstenite): WebSocketEngine adapter"
```

---

### Task 31: `openwire-fastwebsockets` crate

**Files:** mirror Task 30 with `fastwebsockets` instead of `tokio-tungstenite`.

Implementation note: the current adapter keeps the uniform `BoxConnection` engine boundary by adapting `BoxConnection -> TokioIo -> fastwebsockets::WebSocket::after_handshake(...).split(tokio::io::split)`. The read half is wrapped in `FragmentCollectorRead`, while `auto_close` and `auto_pong` stay disabled so openwire's own session layer remains responsible for control-frame behavior.

The `BoxConnection` we receive is what was returned from `upgraded_into_box_connection` in Task 17 — but `fastwebsockets` wants `Upgraded` directly. Two options:

a. Define the engine to accept `Upgraded` directly via a downcast + extension trait. Messy.
b. Have fastwebsockets re-wrap `BoxConnection` (since it implements `tokio::io::AsyncRead + Write` through the existing adapter). Clean but suboptimal for fastwebsockets' SIMD path which prefers raw `Upgraded`.

Choose (b). Performance matters less than API uniformity; users who want maximum perf can also opt into compiler features.

- [ ] **Step 1: Cargo.toml**

```toml
[dependencies]
openwire-core = { path = "../openwire-core", features = ["websocket"] }
openwire-tokio = { path = "../openwire-tokio" }
fastwebsockets = { version = "0.10", features = ["unstable-split"] }
futures-util.workspace = true
bytes.workspace = true
```

- [ ] **Step 2: Implement `FastWebSocketsEngine`** — full code, no `todo!()`. The reading path uses `FragmentCollectorRead` over the split read half to deliver one logical message per `Stream::poll_next`. Map control frames the same way as in tungstenite.

- [ ] **Step 3: Run integration tests against this engine; commit**

```bash
git add Cargo.toml crates/openwire-fastwebsockets/
git commit -m "feat(openwire-fastwebsockets): WebSocketEngine adapter"
```

---

## Phase 9 — Docs & Examples

### Task 32: Examples

**Files:**
- Create: `crates/openwire/examples/websocket_echo.rs`
- Create: `crates/openwire/examples/websocket_subprotocol.rs`

- [ ] **Step 1: `websocket_echo.rs`** — connect to `wss://echo.websocket.org` (or local), send text, print received messages until interrupt.

- [ ] **Step 2: `websocket_subprotocol.rs`** — show subprotocol negotiation against a local test server.

- [ ] **Step 3: Make sure `cargo build --examples --features websocket` is clean**

- [ ] **Step 4: Commit**

```bash
git add crates/openwire/examples/
git commit -m "docs(websocket): example clients"
```

---

### Task 33: Update top-level docs

**Files:**
- Modify: `docs/ARCHITECTURE.md`
- Modify: `README.md`
- Modify: `docs/websocket-design.md` (status update)

- [ ] **Step 1: Add a WebSocket section to `ARCHITECTURE.md`** describing how the upgrade fits the existing flow chart, with a forward reference to `docs/websocket-design.md`.

- [ ] **Step 2: One-line mention in README** under features.

- [ ] **Step 3: Flip `Status: Proposed` → `Status: Implemented` in `docs/websocket-design.md`**, add a date stamp.

- [ ] **Step 4: Commit**

```bash
git add docs/ARCHITECTURE.md README.md docs/websocket-design.md
git commit -m "docs(websocket): architecture + readme + status"
```

---

## Verification Checklist Before Calling The Feature Done

- [ ] `cargo test --workspace --all-features` is green.
- [ ] `cargo clippy --workspace --all-features -- -D warnings` is clean.
- [ ] `cargo build --examples --features websocket` builds.
- [ ] `cargo doc --no-deps --features websocket` builds without broken intra-doc links.
- [ ] All integration tests in `crates/openwire/tests/websocket.rs` pass twice in a row (no flakes).
- [ ] Adapter crate test suites pass (with respective features on).
- [ ] `docs/websocket-design.md` Status is `Implemented`.

## Notes for the Implementer

- **You do not need to follow the task numbering rigidly** — Tasks 11–16 (native engine internals) can be developed in parallel branches and merged before Task 17 starts. Tasks 18–22 are tightly coupled and should run sequentially.
- **When a step shows code, prefer it over inventing your own** — the spec types and method shapes are load-bearing for downstream tasks.
- **`hyper::upgrade::on` interactions are subtle** — read hyper 1.x's `client::conn::http1` source if `with_upgrades()` doesn't exist exactly as named; the conceptual flow is what matters (drive the connection past 101, drain the response body, take the underlying IO).
- **The `BoxConnection` ↔ `tokio::io` adapter** must exist in both directions. Verify in `crates/openwire-tokio/src/lib.rs` whether the reverse direction is present; if not, add it before Task 17.
- **Run the spec self-review (`docs/websocket-design.md`) once at the end** — if anything you implemented diverged from the spec, update the spec to match reality.
