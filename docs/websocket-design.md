# WebSocket Support — Design

Status: Proposed

This document specifies how WebSocket (RFC 6455) support is added to openwire. It
preserves the layering described in `AGENTS.md`: `Client::execute` → application
interceptors → follow-up coordinator → bridge → network interceptors →
`TransportService` → `ConnectorStack` → DNS / TCP / TLS / hyper binding. The
WebSocket entry point is **separate** from `Client::execute` but reuses the same
chain up to the protocol-binding step, where it diverges into a pluggable
`WebSocketEngine`.

```text
Client::new_websocket(request)
  -> CallContext + EventListener
  -> application interceptors
    -> follow-up coordinator           (3xx / 401 / 407 / 5xx for handshake reuse HTTP semantics)
      -> request validation
      -> cookie request application
      -> bridge normalization          (forces HTTP/1.1, injects WS headers, validates URI scheme)
        -> network interceptors
          -> TransportService          (acquires connection via ConnectionPermit; not via Pool)
            -> ConnectorStack          (DNS / TCP / TLS / proxy CONNECT) — unchanged
            -> bind_websocket_handshake (hyper::client::conn::http1 send + read 101)
            -> WebSocketEngine::upgrade(io, negotiated_config) -> WebSocketChannel
        <- handshake response + channel
      <- cookie response persistence
    <- redirect / auth follow-up decision (handshake only; never re-runs after 101)
  -> wrap channel into WebSocket { sender, receiver, handshake }
  -> emit websocket_open
  -> return to caller
```

## 1. Goals

- Provide first-class WebSocket client support that matches OkHttp's behavioral
  surface (`newWebSocket`, queue size, close handshake, automatic pong) while
  using Rust-idiomatic data-plane primitives (`Sink` / `Stream`).
- Reuse all existing infrastructure: DNS, TCP, TLS, proxy (forward and CONNECT),
  cookies, authentication, redirects (during handshake), `EventListener`,
  `CallOptions`, per-request overrides.
- Make the framing implementation **pluggable** through a `WebSocketEngine`
  trait so the same API can be backed by a built-in native codec, by
  `tokio-tungstenite`, or by `fastwebsockets` without code-level branches in
  the rest of the client.
- Keep WebSocket support behind `feature = "websocket"`; do not impose any new
  dependency on users who only need HTTP.
- Keep `ConnectionPool` invariants intact: HTTP and WebSocket share global /
  per-host connection budgets but do not share state machines.

## 2. Non-Goals (v1)

- `permessage-deflate` compression. The trait permits it; the default
  `NativeEngine` does not implement it. Users who need compression switch to
  the `tokio-tungstenite` adapter, or wait for v2.
- Server-side WebSocket. openwire is a client; only the client role is
  implemented.
- Reconnection / backoff / "subscriber" abstractions. Users own the retry loop.
- WebTransport, HTTP/2 WebSockets (RFC 8441), HTTP/3 WebSockets. v1 forces the
  handshake onto HTTP/1.1.
- Public exposure of the raw upgraded socket. The connection is owned by the
  engine after upgrade.

## 3. Background

### 3.1 Current state

`crates/openwire/src/transport/protocol.rs` selects HTTP/1.1 vs HTTP/2 from
URI scheme and ALPN, then calls `bind_http1` / `bind_http2`. There is no
runtime upgrade path: once bound, the protocol is fixed for the connection's
lifetime. Response bodies flow back through `ObservedIncomingBody`, which
assumes "consume body → release connection".

`Cargo.toml` does not pull in any WebSocket library. `EventListener` events
fire at HTTP-level phases (DNS / TCP / TLS / request / response). No upgrade
or 101-handling code exists.

### 3.2 Gaps for WebSocket

1. The HTTP/1.1 binding consumes the connection through hyper's `SendRequest`,
   but to take over after 101 we need the underlying IO back. hyper supports
   this through `hyper::upgrade::on(response)`, which yields `Upgraded` once
   the body of the 101 response is fully drained. The current code does not
   call `hyper::upgrade::on`.
2. `ConnectionPool` only accepts HTTP-shaped connections. A WebSocket
   connection must be removed from pool accounting at upgrade time and never
   re-enter idle eviction.
3. `EventListener` has no WebSocket-specific events; downstream observers
   would need a new contract to track message and close lifecycle.

## 4. Design Overview

### 4.1 Entry point

```rust
impl Client {
    pub async fn new_websocket(&self, request: Request<()>) -> Result<WebSocket, WebSocketError>;
}

impl<'a> Call<'a, ()> {
    pub fn websocket(self) -> WebSocketCall<'a>;
}
```

`Request<()>` enforces an empty body at the type level (handshake is GET with no
body). `Call::websocket` reuses the existing per-request override builder so
every `CallOptions` knob already supported on HTTP carries over (call timeout,
connect timeout, retry policy, redirect policy, etc.).

`WebSocketCall` adds WS-specific overrides:

```rust
pub struct WebSocketCall<'a> { /* … */ }

impl<'a> WebSocketCall<'a> {
    pub fn handshake_timeout(self, d: Duration) -> Self;          // default: from call_timeout
    pub fn close_timeout(self, d: Duration) -> Self;              // default: 1s
    pub fn max_frame_size(self, n: usize) -> Self;                // default: 1 MiB
    pub fn max_message_size(self, n: usize) -> Self;              // default: 16 MiB
    pub fn send_queue_size(self, n: usize) -> Self;               // default: 256 messages
    pub fn ping_interval(self, d: Duration) -> Self;              // default: None (disabled)
    pub fn pong_timeout(self, d: Duration) -> Self;               // default: 30s when ping is enabled
    pub fn subprotocols<I, S>(self, p: I) -> Self
        where I: IntoIterator<Item = S>, S: Into<String>;
    pub fn engine(self, e: Arc<dyn WebSocketEngine>) -> Self;
    pub async fn execute(self) -> Result<WebSocket, WebSocketError>;
}
```

`ClientBuilder` carries the same names (`websocket_handshake_timeout`,
`websocket_max_frame_size`, etc.) for client-wide defaults, plus
`websocket_engine(Arc<dyn WebSocketEngine>)` to switch the default engine.

### 4.2 Public types

```rust
pub struct WebSocket {
    sender:    WebSocketSender,
    receiver:  WebSocketReceiver,
    handshake: WebSocketHandshake,
}

impl WebSocket {
    pub fn handshake(&self) -> &WebSocketHandshake;
    pub fn sender(&self) -> WebSocketSender;             // Arc clone — multi-task send
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver);
}

#[derive(Clone)]
pub struct WebSocketSender { /* Arc<Inner>; Inner holds an mpsc to the writer task */ }

impl WebSocketSender {
    pub async fn send(&self, msg: Message) -> Result<(), WebSocketError>;
    pub async fn send_text(&self, t: impl Into<String>) -> Result<(), WebSocketError>;
    pub async fn send_binary(&self, b: impl Into<Bytes>) -> Result<(), WebSocketError>;
    pub async fn close(&self, code: u16, reason: impl Into<String>) -> Result<(), WebSocketError>;
    pub fn queue_size(&self) -> usize;                   // OkHttp parity
    pub fn is_closed(&self) -> bool;
}

pub struct WebSocketReceiver { /* impl Stream<Item = Result<Message, WebSocketError>> */ }

impl Stream for WebSocketReceiver { type Item = Result<Message, WebSocketError>; /* … */ }

#[derive(Clone, Debug)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),                 // surfaced for visibility; user should not send unless they know why
    Pong(Bytes),                 // surfaced; auto-pong is internal
    Close { code: u16, reason: String },
}

pub struct WebSocketHandshake {
    response_status:  StatusCode,                        // always 101 on success
    response_headers: HeaderMap,
    subprotocol:      Option<String>,                    // negotiated
    extensions:       Vec<String>,                       // raw token list
}
```

**Concurrency model**: `WebSocketSender` is `&self + Clone`. A user can clone
the sender to multiple tasks; each `send_*` call enqueues onto an internal
mpsc consumed by a single writer task that owns the engine's `Sink` half.
This matches OkHttp's "send from any thread" ergonomics and gives a single
serialization point for the wire. `WebSocketReceiver` is single-consumer
(`Stream`, not `Clone`) because demultiplexing a single WebSocket to multiple
consumers has no defined semantic.

`Ping` / `Pong` are present in `Message` so that observers and the
`deliver_control_frames` opt-in path have a typed representation. By default
(`deliver_control_frames = false`) the receiver `Stream` only ever yields
`Text` / `Binary` / `Close`; auto-pong is handled internally and incoming
`Ping`/`Pong` frames are absorbed before they reach the user. `WebSocketSender`
intentionally does **not** expose `send(Message::Ping(_))` as a happy-path API
— users who need a custom-payload ping (rare) can configure `ping_interval`
or wait for v2; sending arbitrary control frames out-of-band is not supported
in v1.

### 4.3 Engine abstraction

```rust
// openwire-core/src/websocket/engine.rs

pub trait WebSocketEngine: Send + Sync + 'static {
    /// Called after the 101 response is fully parsed and `hyper::upgrade::on`
    /// has yielded the underlying connection. The engine takes ownership of
    /// `io` and produces a duplex `WebSocketChannel`. `config` carries the
    /// negotiated subprotocol, extensions, role (always `Client` in v1), and
    /// engine-relevant size limits.
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<'static, Result<WebSocketChannel, WebSocketEngineError>>;
}

pub struct WebSocketEngineConfig {
    pub role:              Role,           // Client (Server reserved)
    pub subprotocol:       Option<String>,
    pub extensions:        Vec<String>,
    pub max_frame_size:    usize,
    pub max_message_size:  usize,
}

pub struct WebSocketChannel {
    pub send: BoxSink<EngineFrame, WebSocketEngineError>,
    pub recv: BoxStream<Result<EngineFrame, WebSocketEngineError>>,
}

pub enum EngineFrame {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}
```

`EngineFrame` is the engine-facing analog of `Message`. The boundary between
the two exists so that user-facing types can evolve without forcing every
engine impl to change. In practice they are 1:1 in v1.

The engine is responsible for:

- Frame masking on send (client always masks; server never masks in v1).
- Fragmentation handling on receive (re-assembly into a single `EngineFrame`
  per logical message, capped by `max_message_size`).
- Control-frame validation (≤125 bytes, never fragmented).
- UTF-8 validation for `Text` frames (per RFC 6455 §8.1).
- Close-frame echo on receive of a Close (RFC 6455 §5.5.1).
- Mapping protocol violations to `WebSocketEngineError`.

The engine is **not** responsible for:

- Heartbeat scheduling (openwire owns the timer; engine just sees `Ping` frames).
- Pong replies (openwire owns auto-pong; the engine receives `Ping`, openwire
  enqueues a `Pong` back through the same `send` sink).
- `EventListener` events (openwire wraps the channel with an instrumenting
  layer before exposing it).
- Subprotocol negotiation (openwire builds the request headers and validates
  the response before calling `upgrade`).

### 4.4 Engine implementations

| Engine | Crate | Default | Dependencies |
| --- | --- | --- | --- |
| `NativeEngine` | `crates/openwire/src/websocket/native/` | yes | none beyond existing |
| `TungsteniteEngine` | new `crates/openwire-tungstenite/` | no | `tokio-tungstenite` + reverse `TokioIo` shim |
| `FastWebSocketsEngine` | new `crates/openwire-fastwebsockets/` | no | `fastwebsockets` |

Adapter crates follow the `openwire-rustls` / `openwire-tokio` pattern:
separate workspace crate, one trait impl, no public surface beyond the engine
type and a default constructor. Users who want them run
`cargo add openwire-tungstenite` and call
`ClientBuilder::websocket_engine(Arc::new(TungsteniteEngine::default()))`.

`NativeEngine` is the v1 in-house implementation, scoped per §4.6. It has zero
new third-party dependencies — all I/O goes through `BoxConnection`, framing
uses `bytes`, masking uses a single 32-bit XOR loop.

### 4.5 Connection limiter refactor

Currently `ConnectionPool` owns both pool state (idle list, allocations) and
the global / per-host connection counters. The WebSocket path needs the
counters but not the pool. We extract a `ConnectionLimiter` (shape sketched
below) that the existing `ConnectionPool` consumes internally and that the
WebSocket transport branch can call directly.

```rust
// crates/openwire/src/connection/limits.rs

pub struct ConnectionLimiter { /* per-host + global counters */ }

impl ConnectionLimiter {
    pub fn try_acquire(&self, addr: &Address) -> Option<ConnectionPermit>;
    pub async fn acquire(&self, addr: &Address) -> ConnectionPermit;
    pub fn can_acquire(&self, addr: &Address) -> bool;   // existing helper from prior fix
}

pub struct ConnectionPermit { /* on Drop: counters decremented */ }
```

`ConnectionPool` continues to wrap the limiter. The WebSocket branch in
`TransportService` takes a `ConnectionPermit` *before* calling `ConnectorStack`
(so handshake failure cleanly releases the budget) and stores the permit
inside the WebSocket connection state. When the WebSocket close handshake
completes (or the writer task aborts), the permit is dropped and the budget
returns to the limiter.

This refactor lifts logic that already lives in `connection/limits.rs` and
`pool.rs` (commit `8b11e31` introduced `can_acquire`, commit `1232441`
established a similar "abstract a piece out of pool" pattern with the proxy
selector). The rest of the pool is unchanged.

### 4.6 Native engine v1 scope

Implements RFC 6455 §5–§8 with the following surface:

- Frame opcodes: `Text(0x1)`, `Binary(0x2)`, `Close(0x8)`, `Ping(0x9)`,
  `Pong(0xA)`. Unknown opcodes are protocol violations.
- Client sends: every frame is masked with a freshly generated 32-bit key
  (`getrandom` already used elsewhere in the workspace for TLS; reuse).
- Client receives: any masked frame from the server is a protocol violation.
- Length encoding: 7-bit, 7+16-bit, 7+64-bit per RFC 6455 §5.2.
- Fragmentation: control frames must not be fragmented; data frames
  reassemble across continuation frames; reassembled message is rejected if
  it exceeds `max_message_size`.
- UTF-8: `Text` frames are validated per RFC 6455 §8.1 using `std::str::from_utf8`
  on the reassembled payload (we do not stream-validate fragments).
- Close: outgoing close sends `0x88` with status code + reason (truncated to
  fit ≤125 bytes); incoming close echoes once. After close exchange both
  halves of the channel return `Ok(None)` / `Poll::Ready(Ok(()))`.
- Close codes: validated against the reserved-range table in RFC 6455 §7.4.

Out of scope for native v1:

- `permessage-deflate` (RFC 7692). The engine config carries the negotiated
  extension list; `NativeEngine` rejects with `WebSocketEngineError::UnsupportedExtension`
  if any extension survives negotiation. (Negotiation prevents this in
  practice: §4.7.2.)
- Per-message interleaving across fragments (sending a control frame in the
  middle of a fragmented data message is **received** correctly per RFC 6455,
  but the native engine never **emits** interleaved fragments — every send is
  a single frame).

### 4.7 Handshake details

#### 4.7.1 Bridge changes

`BridgeInterceptor` (`crates/openwire/src/bridge.rs`) detects WebSocket-bound
requests by checking the `Upgrade` extension marker that
`Client::new_websocket` attaches to the request. When set, the bridge:

- Forces `HTTP/1.1` by attaching a `RoutePreference::Http1Only` flag the
  route planner reads (the planner already filters routes by ALPN; we add
  one branch to short-circuit H2-coalesced reuse for WS-flagged requests).
- Rewrites `ws` → `http` and `wss` → `https` for routing (the WebSocket scheme
  is conceptual; the wire is HTTP/1.1).
- Adds `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Version: 13`,
  generates a 16-byte random `Sec-WebSocket-Key` (base64-encoded).
- Adds `Sec-WebSocket-Protocol: <comma-separated subprotocols>` if any.
- Disallows request bodies (caller-supplied `Request<()>` already enforces this).
- Records the expected `Sec-WebSocket-Accept` value in `CallContext` for
  validation when the response arrives.

#### 4.7.2 Subprotocol negotiation

The client offers candidates; the server picks zero or one. The bridge sends
`Sec-WebSocket-Protocol: a, b, c`. The transport branch reads the response
header and:

- If the server returns one value, it must be in the offered set; otherwise
  `WebSocketError::Handshake { reason: SubprotocolMismatch }`.
- If the server returns no `Sec-WebSocket-Protocol` header, negotiated
  subprotocol is `None`. This is allowed even when the client offered some.
- If multiple values are returned, this is a protocol violation.

#### 4.7.3 Response validation

After the network interceptors deliver a response, the WebSocket transport
branch verifies:

- `status == 101 Switching Protocols`
- `Connection` includes `upgrade` (case-insensitive, comma-list aware)
- `Upgrade` is `websocket` (case-insensitive)
- `Sec-WebSocket-Accept` equals the value recorded in `CallContext`
- No `Sec-WebSocket-Extensions` other than ones explicitly accepted by the
  selected engine

A non-101 response is fed back to the follow-up coordinator with normal HTTP
semantics: 3xx → redirect, 401 / 407 → re-auth, 5xx + retry policy → retry.
Once `101` is observed and validated, follow-up logic is bypassed for the
remainder of the call (a subsequent close / message error is **not** a retry
trigger; it is delivered to the user).

#### 4.7.4 `hyper::upgrade::on`

After the 101 headers are parsed, the transport branch:

1. Calls `hyper::upgrade::on(response)` to obtain the `Upgraded` IO.
2. Wraps `Upgraded` in the engine-facing `BoxConnection`.
3. Builds `WebSocketEngineConfig` from negotiated values + per-call limits.
4. Calls `engine.upgrade(io, config)` to obtain `WebSocketChannel`.
5. Wraps the channel with the instrumenting layer (§5).
6. Spawns the writer task and heartbeat task.
7. Returns `WebSocket { sender, receiver, handshake }` to the caller.

### 4.8 Writer task and queue

The writer task owns the engine's `BoxSink<EngineFrame>` and consumes from a
`tokio::sync::mpsc::Sender<WriterCommand>` whose receiver is held by the task.
`WriterCommand` is one of:

- `Send(Message)` — user data
- `Pong(Bytes)` — auto-reply to received `Ping`
- `Ping(Bytes)` — heartbeat
- `Close { code, reason }` — explicit close (from user `close()` or `Drop`)
- `Cancel` — abort without sending close

The writer task processes commands FIFO. `WebSocketSender::send_*` returns
`Ok(())` once the message is enqueued (engine-level write is asynchronous).
`queue_size()` returns the number of pending `WriterCommand`s in the mpsc.
This is approximate parity with OkHttp's `WebSocket.queueSize()` (which
reports buffered bytes); we surface message count rather than bytes because
the mpsc is message-shaped, not byte-shaped, and the precise "send queue
depth" semantics most users want from `queueSize()` is "how far behind am I"
— a unit that message count answers more directly. Because the mpsc is
bounded (per `send_queue_size`), `send_*` is `async fn`: it suspends when the
queue is full, providing natural backpressure.

When the writer task observes a `Close` command, it sends the close frame,
then waits up to `close_timeout` for the receiver task to surface a remote
close (the engine handles echo). On timeout, it forces socket teardown.

### 4.9 Heartbeat

When `ping_interval` is set, a background timer task periodically enqueues
`Ping(b"")` to the writer queue. If `pong_timeout` elapses without a matching
pong, the writer enqueues a `Close { code: 1011, reason: "ping timeout" }`
and the call fails with `WebSocketError::Timeout(PingTimeout)`. The receiver
task marks the most recent inbound pong timestamp; the heartbeat task
inspects it via an atomic.

### 4.10 Drop / cancel semantics

Dropping a `WebSocketSender` clone other than the last has no effect (Arc
refcount). Dropping the last `WebSocketSender` while the receiver is still
held does **not** initiate close — the user may still want to receive
server-pushed messages. Dropping the `WebSocketReceiver` while sender remains
also does not initiate close.

When **both** halves are dropped (Arc refcount on a shared inner state hits
zero), a `Cancel`-equivalent signal fires:

1. Best-effort `Close { code: 1001, reason: "client cancelled" }` is enqueued.
2. The writer task waits up to `close_timeout` (default 1 s) for the close
   echo.
3. The TCP/TLS connection is torn down.
4. The `ConnectionPermit` is released.
5. `EventListener::websocket_closing(initiator = Local)` and
   `websocket_closed` fire.

Explicit `WebSocketSender::close(code, reason)` is the documented way to
trigger an orderly close with custom code/reason; the drop path exists only
to guarantee the connection does not leak.

## 5. Observability

### 5.1 EventListener additions

```rust
pub trait EventListener: Send + Sync {
    // … existing methods unchanged …

    fn websocket_open(&self, ctx: &CallContext, handshake: &WebSocketHandshake) {}
    fn websocket_message_sent(
        &self, ctx: &CallContext, kind: MessageKind, payload_len: usize,
    ) {}
    fn websocket_message_received(
        &self, ctx: &CallContext, kind: MessageKind, payload_len: usize,
    ) {}
    fn websocket_ping_sent(&self, ctx: &CallContext) {}
    fn websocket_pong_received(&self, ctx: &CallContext) {}
    fn websocket_closing(
        &self, ctx: &CallContext, code: u16, reason: &str, initiator: CloseInitiator,
    ) {}
    fn websocket_closed(&self, ctx: &CallContext, code: u16, reason: &str) {}
    fn websocket_failed(&self, ctx: &CallContext, error: &WebSocketError) {}
}

pub enum MessageKind { Text, Binary }
pub enum CloseInitiator { Local, Remote }
```

All methods have empty default impls (additive change; existing impls compile
unchanged). Existing HTTP events fire normally during the handshake itself —
`dns_start/end`, `connect_start/end`, `tls_start/end`, `request_headers_*`,
`response_headers_*` all fire as for any HTTP/1.1 GET. `response_body_end`
does **not** fire on a successful upgrade (the response has no body to
consume). `call_end` fires when the WebSocket close handshake completes
(both sides exchanged Close frames or the timeout elapsed); `call_failed`
fires for handshake failures and for protocol-level failures during the
session.

### 5.2 Tracing spans

Each WebSocket call enters a `websocket` tracing span attached to the existing
`call` span. The handshake portion is tagged `phase=handshake`; the session is
tagged `phase=session`. Frame-level events emit at `TRACE` so production users
can leave tracing on at `DEBUG` without flooding logs.

### 5.3 Instrumentation layer

`EventListener` events are emitted by an internal wrapper around
`WebSocketChannel`. The engine produces a raw `WebSocketChannel`; the
transport branch wraps it in an `InstrumentedChannel` whose `Sink::send` and
`Stream::poll_next` fire the corresponding events before forwarding to the
inner channel. Engines therefore never need to know about `EventListener`.

## 6. Configuration Defaults

| Knob | Client default | Per-call override | Rationale |
| --- | --- | --- | --- |
| `websocket_handshake_timeout` | inherits `call_timeout`, fall back to 30 s | yes | parity with HTTP |
| `websocket_close_timeout` | 1 s | yes | matches OkHttp's default cancel grace |
| `websocket_max_frame_size` | 1 MiB | yes | DoS / memory cap |
| `websocket_max_message_size` | 16 MiB | yes | DoS / memory cap on reassembled payload |
| `websocket_send_queue_size` | 256 messages | yes | bound writer task backlog |
| `websocket_ping_interval` | None | yes | OkHttp parity (off by default) |
| `websocket_pong_timeout` | 30 s | yes | only effective when ping enabled |
| `websocket_subprotocols` | empty | yes | offer list |
| `websocket_engine` | `NativeEngine` | yes (`engine(Arc::new(...))`) | pluggability |
| `websocket_deliver_control_frames` | false | yes | rare advanced use case |

## 7. Error Model

```rust
// openwire-core/src/websocket/error.rs

#[derive(Debug, thiserror::Error)]
pub enum WebSocketError {
    #[error("handshake failed: {reason}")]
    Handshake { status: Option<StatusCode>, reason: HandshakeFailure },

    #[error(transparent)]
    Engine(#[from] WebSocketEngineError),

    #[error("server closed the connection: {code} {reason}")]
    ClosedByPeer { code: u16, reason: String },

    #[error("websocket {kind} timed out")]
    Timeout(TimeoutKind),

    #[error("transport io error: {0}")]
    Io(#[source] WireError),

    #[error("local cancellation")]
    LocalCancelled,
}

pub enum HandshakeFailure {
    UnexpectedStatus,
    MissingUpgrade,
    MissingConnection,
    InvalidAccept,
    SubprotocolMismatch,
    UnsupportedExtension,
    Other(String),
}

pub enum TimeoutKind { Handshake, Close, Ping }

pub enum WebSocketEngineError {
    InvalidFrame(String),
    InvalidUtf8,
    InvalidCloseCode(u16),
    PayloadTooLarge { limit: usize, received: usize },
    UnsupportedExtension(String),
    Io(WireError),
}
```

`WebSocketError` is a leaf type; it does not implement `From<WebSocketError>
for WireError` because `WireError` is the HTTP-shape error and the conversion
is lossy. Callers of `Client::new_websocket` get `WebSocketError` directly.
Internal places that need to interoperate with `WireError` (e.g., emitting
`call_failed`) carry `WebSocketError` by reference.

## 8. Crate Layout & Feature Flags

```
crates/
  openwire-core/
    src/websocket/
      mod.rs            // re-exports
      engine.rs         // WebSocketEngine, WebSocketEngineConfig, WebSocketChannel, EngineFrame
      error.rs          // WebSocketError, WebSocketEngineError
      message.rs        // Message, MessageKind, CloseInitiator
  openwire/
    src/websocket/
      mod.rs            // public WebSocket, WebSocketSender, WebSocketReceiver, WebSocketHandshake
      handshake.rs      // bridge tweaks + response validation + Sec-WebSocket-Accept
      transport.rs      // TransportService branch: bind handshake, hyper::upgrade, engine.upgrade
      writer.rs         // writer task + heartbeat + drop semantics
      instrumented.rs   // EventListener wrapper around WebSocketChannel
      native/
        mod.rs          // NativeEngine
        codec.rs        // frame encode / decode
        mask.rs         // masking helper
  openwire-tungstenite/  // new crate
    src/lib.rs          // TungsteniteEngine
  openwire-fastwebsockets/  // new crate
    src/lib.rs          // FastWebSocketsEngine
```

Feature flags:

- `openwire/websocket` — adds the `websocket` module, depends on
  `openwire-core/websocket`. Default: off.
- `openwire-core/websocket` — exposes the engine trait + types. Default: off.
- `openwire-tungstenite` and `openwire-fastwebsockets` are separate
  crates; depending on either implies `openwire/websocket` (transitive).

`NativeEngine` lives inside `openwire` itself; the adapter crates pull in
their respective WebSocket dependencies and stand alone.

## 9. Testing Strategy

`crates/openwire-test` gains a WebSocket test server helper:

```rust
pub fn spawn_websocket_echo() -> TestServerHandle;
pub fn spawn_websocket_handler<F>(handler: F) -> TestServerHandle
    where F: Fn(WebSocketUpgrade) -> BoxFuture<'static, ()> + Send + Sync + 'static;
```

Backed by `tokio-tungstenite` on the **server** side (different from the
engine under test). This decouples test infra from the client engine so that
a bug in `NativeEngine` cannot mask itself by also being on the server side.

Integration tests (`crates/openwire/tests/websocket.rs`):

1. **Handshake**: success path; rejects 200 / 400 / 5xx; redirects 301 →
   follow-up; 401 → auth; missing `Sec-WebSocket-Accept`; bad accept; bad
   upgrade header; subprotocol negotiated / rejected / mismatched.
2. **Message exchange**: text / binary roundtrip; large messages near and
   over `max_message_size`; fragmented receive; UTF-8 violation in text.
3. **Control frames**: server-initiated ping / client auto-pong; server
   pong absorption when ping interval is set; ping timeout.
4. **Close**: client-initiated close; server-initiated close; concurrent
   close; close timeout; abnormal disconnect (TCP reset).
5. **Drop semantics**: dropping both halves triggers close 1001 within
   close_timeout; dropping only sender or only receiver does not close.
6. **Concurrent send**: cloned `WebSocketSender` from multiple tasks; queue
   backpressure when `send_queue_size` is small.
7. **Proxy**: WSS through an HTTPS proxy via CONNECT tunnel.
8. **EventListener**: full event sequence captured and asserted.
9. **Engine pluggability**: same test suite parameterized over
   `NativeEngine` and `TungsteniteEngine` (skipped if feature off).
10. **Limiter**: WebSocket counts against `max_connections_per_host` /
    `max_connections_total`; closing the WebSocket releases the budget.

Unit tests (next to each module):

- `native/codec.rs`: round-trip every frame opcode at every length boundary
  (0 / 125 / 126 / 65535 / 65536 / large).
- `native/mask.rs`: known-vector tests against RFC 6455 §5.3 example.
- `handshake.rs`: `Sec-WebSocket-Accept` derivation against the RFC 6455
  §1.3 example (`dGhlIHNhbXBsZSBub25jZQ==` → `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`).
- `writer.rs`: queue ordering; close timeout; cancel.

Optional fuzzing: a frame parser fuzz target under `crates/openwire/fuzz/`
to exercise `native/codec.rs` against arbitrary bytes (out of scope for v1
beyond skeleton).

## 10. Phased Rollout

1. **Phase 1 — scaffolding**: extract `ConnectionLimiter` from
   `ConnectionPool` (no functional change to HTTP); add `feature = "websocket"`
   gates; create empty `websocket` modules.
2. **Phase 2 — handshake**: bridge changes, response validation,
   `hyper::upgrade::on` integration; reach the point of returning a raw
   `Upgraded` to a stub engine. Unit-test handshake derivation.
3. **Phase 3 — native engine**: implement codec, mask, frame fragmentation,
   close handshake, UTF-8 validation. Wire into `WebSocketEngine` trait.
4. **Phase 4 — public API**: `WebSocket`, `WebSocketSender` (Arc / mpsc),
   `WebSocketReceiver`, writer task, drop semantics, heartbeat.
5. **Phase 5 — observability & limits**: `EventListener` events,
   instrumentation wrapper, `ConnectionPermit` plumbing, tracing spans.
6. **Phase 6 — adapters**: `openwire-tungstenite`, `openwire-fastwebsockets`.
   Run the same integration test suite against each.
7. **Phase 7 — docs & examples**: `examples/websocket_echo.rs`,
   `examples/websocket_subprotocol.rs`, `docs/ARCHITECTURE.md` update,
   README mention.

Each phase ships independently; phases 1–5 are required to call this feature
"WebSocket support landed". Phase 6 makes pluggability real; phase 7 is the
DX polish.

## 11. Future Work (out of scope for v1)

- `permessage-deflate` in the native engine (RFC 7692). Trait already permits;
  flip a flag and ship.
- HTTP/2 WebSockets (RFC 8441). Requires extending the engine config to carry
  a `:protocol` pseudo-header path and reusing an H2 stream rather than
  taking over the connection.
- Server role (RFC 6455 §4.2). `Role::Server` is reserved in the engine
  config but not implemented.
- Built-in reconnection / backoff. We expose error variants that tell users
  what failed; the retry loop is theirs.
- Public exposure of the upgraded socket (escape hatch). Not planned; users
  who need raw socket control can write their own engine implementation.
