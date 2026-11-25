# OpenWire Testing

Date: 2026-09-13

This is the testing-system reference. Command lists in `README.md` and
`docs/ARCHITECTURE.md` §7 stay short and point here.

Correctness is proven by deterministic local tests. Public-internet checks are
opt-in smoke only. Tests mock servers, never `Client`.

## Spec-first expected values

Unit and policy tests exist to review the implementation against a spec, not
to freeze whatever the current code does.

- Expected values come from RFC 9110/9112/6265, Fetch, the official Public
  Suffix List `tests.txt`, `EventListener` rustdoc, and `ARCHITECTURE.md`.
- If a test fails, the implementation is wrong or the spec citation in the
  test is wrong. Do not “fix” the test by copying the implementation.
- Applicable mandatory RFC requirements cannot be waived as compatibility
  exceptions. There is currently no cookie-store implicit-`*` carve-out:
  OpenWire applies RFC 6265 §5.3 with the PSL algorithm, including unknown TLDs.
- Do not mock `Client`. Use table-driven unit tests for policy and
  `MockWebServer` / `spawn_http1` for the wire. Mockall-style trait mocks are
  unnecessary for this crate: `StaticDnsResolver`, handler servers, and
  in-module `RequestSnapshot` tests already isolate seams.

## Protocol conformance evidence

Strict RFC conformance is required throughout the supported protocol surface.
Use the applicable RFC version, its updates, and verified errata. Interpret
normative language using [BCP 14](https://www.rfc-editor.org/rfc/rfc8174.html);
do not silently promote optional behavior to a mandatory requirement.

For each protocol change, keep a requirement matrix in its design or topic
documentation. Every row records:

- RFC and section, sender/recipient role, applicability, and requirement level.
- Observable invariant and the implementation owner responsible for it.
- Independently derived positive, negative, wire, and failure-path cases.
- Explicit status: planned, implemented but unverified, or verified with the
  relevant test names and command results.

Applicable MUST/MUST NOT violations block delivery. Follow SHOULD/SHOULD NOT
by default; an RFC-permitted exception requires its specific rationale and
tests. Record MAY choices and deliberate feature limits separately from
protocol violations. Existing behavior, OkHttp/Fetch compatibility, and a
dependency accepting bytes do not replace RFC-derived expected results.

Test library-owned integration boundaries as well as pure encoders: framing,
actual negotiated protocol, redirects/retries, error propagation, and connection
reuse after incomplete exchanges. Dependency-owned parsers and codecs need
boundary/interop evidence, rather than a duplicate implementation of their
internal test suite. A feature's passing suite does not establish conformance
for the entire library. Discovered mandatory violations must be fixed or block
delivery of the affected work; do not relabel them as verified behavior.

The multipart encoder tracks its requirement-to-evidence matrix in
[multipart-design.md](multipart-design.md#rfc-conformance-contract).

## Layers

| Layer | What it proves | Where | Required CI |
| --- | --- | --- | --- |
| 1. Unit / table-driven | RFC tables, parsers, policy decisions, no sockets | crate `src/**` `#[cfg(test)]` modules | yes |
| 2. Raw bytes | Exact HTTP/1 bytes sent or malformed peer handling | `crates/openwire/tests/` raw helpers; later `testdata/http1/` | yes |
| 3. Local full stack | Redirect, cookie, auth, proxy, pool, interceptors, events | `crates/openwire/tests/*.rs` + `openwire-test` | yes |
| 4. Independent peer | Interop with a non-hyper HTTP stack | planned `tests/interop.rs` (nghttpx/Caddy) | skip if binary missing; not a merge gate until stable |
| 5. Spec corpora | WebSocket Autobahn, cache-tests.fyi as an oracle | planned nightly / `#[ignore]` | no |
| 6. Live internet | Real PKI and public-origin drift | `crates/openwire/tests/live_network.rs` | no; weekly workflow |

Do not use httpbingo, postman-echo, or similar as a source of truth in required
CI. Copy a real response into a local fixture when a public shape is useful.

Do not re-test HTTP/2 framing, HPACK, or TLS handshake vectors here. Those
belong to `h2` and `rustls`. OpenWire tests client policy and connection
lifecycle on top.

## Commands

Default features (`tls-rustls`, `platform-verifier`, `compression`, `cookies`):

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Optional features, including `websocket`, `json`, and `multipart`:

```bash
cargo test --workspace --all-targets --all-features
cargo test -p openwire --features websocket --test websocket
cargo test -p openwire --features multipart --test multipart --test redirect --test auth --test compression_bridge --test pool_http2
```

Required CI runs both default-feature tests and `--all-features`. Default
features stay a separate job so a build with websocket disabled still compiles
and tests.

Live-network smoke (not a merge gate):

```bash
cargo test -p openwire --test live_network -- --ignored --test-threads=1
```

`.github/workflows/live-network.yml` runs that suite on manual dispatch and a
weekly schedule. Deferred public-origin follow-ons stay in
[live-network-follow-ups.md](live-network-follow-ups.md).

## Local harness

`crates/openwire-test` is unpublished workspace support:

- `MockWebServer` — OkHttp-style `enqueue` / `take_request` for sequential
  responses. Empty queue returns HTTP 500 and increments `underflow_count`.
- `spawn_http1` / `spawn_https_http1` / `spawn_https_http2*` — handler-oriented
  servers for concurrent and protocol-specific cases
- `RecordingEventListenerFactory` — call-lifecycle event capture
- `StaticDnsResolver` — pin a host to a loopback address

One server per test. Bind port `0`. Prefer these helpers over third-party mock
HTTP crates.

Scoped tracing assertions live in `crates/openwire/tests/tracing.rs`. Each case
runs alone in a fresh copy of that test executable; its parent uses `--exact`
and checks both the exit status and a scenario completion marker. This isolates
tracing's process-wide dispatcher/callsite cache from unrelated test threads
while the normal test runner stays parallel. Keep future tracing-capture cases
in this binary and use `run_isolated_tracing_test`; the capture-only async lock
is insufficient. See [tracing-test-isolation.md](tracing-test-isolation.md) for
the failure evidence and regression checks.

```rust
let server = MockWebServer::http1().await;
server.enqueue(MockResponse::new().status(StatusCode::FOUND).header("Location", "/next"));
server.enqueue(MockResponse::new().body("ok"));
// client.execute(server.url("/start"))
let first = server.take_request().await;
```

Concurrent or dispatcher-style peers still use `spawn_http1` closures. A custom
`Dispatcher` trait, trailers, and HTTP/2 push are not in this harness yet.

## Adding tests

- Policy tables (redirect status, cookie Domain matching) go in unit tests with
  no sockets.
- Wire-visible behavior (Host synthesis, chunked bodies, malformed peers) goes
  through raw HTTP/1 helpers or `openwire-test`.
- New integration cases go in the matching topic file under
  `crates/openwire/tests/` (`redirect.rs`, `cookie.rs`, `proxy.rs`,
  `pool_http2.rs`, `timeout_cancel.rs`, `auth.rs`, `events.rs`,
  `compression_bridge.rs`, `scheduler_limits.rs`, `multipart.rs`). Shared helpers live in
  `tests/support/`. `integration.rs` is smoke only.
- New public-internet tests must be `#[ignore]` and documented as smoke.

## Corpora (current and planned)

| Corpus | Role | Status |
| --- | --- | --- |
| `crates/openwire/data/public_suffix_list.dat` | Runtime public suffix list for the cookie jar | vendored |
| `crates/openwire/data/psl_tests.txt` | Cookie Domain / eTLD+1 vectors from publicsuffix.org | vendored; `tests/psl_corpus.rs` asserts official expected values |
| RFC 9110 redirect method matrix | Follow vs rewrite vs stop | unit matrix plus `tests/redirect.rs` |
| Autobahn Testsuite | RFC 6455 WebSocket | planned, nightly |
| http-tests/cache-tests | RFC 9111 behavior oracle | planned, not a 100% gate |

When vendoring a corpus, record source URL, date, and license next to the file.
Do not fetch corpora at test time.
