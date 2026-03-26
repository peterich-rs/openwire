# OpenWire Error Handling Review and Roadmap

Date: 2026-03-26
Status: Proposed

This document records the current error-handling state in OpenWire, the main
gaps found in review, and the long-term plan for turning the existing
`WireError` model into a production-grade failure contract.

The design constraint is to preserve the existing execution layering:

```text
User API
  -> Client::execute
    -> create CallContext
    -> create EventListener
    -> application interceptors
      -> follow-up coordinator
        -> request validation
        -> cookie request application
        -> bridge normalization
          -> network interceptors
            -> transport service
              -> connector stack
                -> proxy selection
                -> dns
                -> tcp
                -> tls
                -> hyper conn/client
        -> cookie response persistence
        -> auth / redirect follow-up decision
    -> response body wrapper
      -> connection release bookkeeping
```

No future error-handling work should bypass this chain or collapse policy and
transport concerns into one layer.

## 1. Why This Exists

OpenWire does not need to pretend that every failure is solvable. It needs a
failure system that does three things reliably:

- gives callers stable semantics
- gives retry / follow-up policy enough structure to make correct decisions
- gives operators enough context to diagnose production issues without parsing
  ad hoc strings

Today the codebase already has a useful baseline, but it still leans too hard
on `kind + message` in places where production behavior should be driven by
structured failure metadata.

## 2. Current State

OpenWire currently exposes a single public error type, `WireError`, with:

- a coarse `WireErrorKind`
- a message
- an optional source error
- optional establishment metadata used mainly for connect-stage retry logic

Current public kinds:

- `InvalidRequest`
- `Timeout`
- `Canceled`
- `Dns`
- `Connect`
- `Tls`
- `Protocol`
- `Redirect`
- `Body`
- `Interceptor`
- `Internal`

Current strengths:

- Request-construction and URI/proxy/config validation failures are clearly
  surfaced as `InvalidRequest`.
- Connection-establishment failures already distinguish retryable and
  non-retryable TLS / proxy / TCP paths well enough for the default retry
  policy.
- HTTP response status codes are not treated as transport exceptions.
- Response-body failures happen when the body is consumed, not from
  `Client::execute`, which is the correct ownership boundary.
- Event-listener hooks already separate `call_failed` from
  `response_body_failed`.

Current important implementation anchors:

- `crates/openwire-core/src/error.rs`
- `crates/openwire/src/client.rs`
- `crates/openwire/src/policy/follow_up.rs`
- `crates/openwire/src/policy/defaults.rs`
- `crates/openwire/src/transport/connect.rs`
- `crates/openwire/src/transport/service.rs`
- `crates/openwire/src/transport/body.rs`
- `crates/openwire/src/transport/protocol.rs`

## 3. Current Gaps

### Gap 1: Kinds are too coarse for production triage

`Timeout` currently covers more than one phase:

- whole-call timeout before a response is returned
- timeout while reading the response body
- connect timeout when establishing a socket or proxy tunnel

`Connect` also covers more than one class of failure:

- TCP refusal or reset
- proxy CONNECT rejection
- SOCKS authentication or tunnel failure
- route exhaustion

This is workable for unit classification but weak for production diagnosis.
Callers should not need to parse English messages to tell whether a timeout
happened in connect, response headers, or response body.

### Gap 2: Establishment metadata is not applied consistently

`EstablishmentStage::Dns` exists, but DNS errors are not currently tagged with
establishment metadata. The default retry policy compensates by checking
`WireErrorKind::Dns`, but that means the retry system is already split between
two classification axes instead of one consistent axis.

That weakens:

- custom retry policy authoring
- metrics bucketing
- future phase-aware observability

### Gap 3: Extension-boundary failures are not normalized

Interceptors and authenticators return `WireError` directly. That means OpenWire
cannot guarantee a stable distinction between:

- user extension failure
- policy-layer failure
- transport-layer failure

`Interceptor` exists as a kind, but the framework does not enforce wrapping at
the interceptor boundary. Authentication failures also do not currently have a
dedicated public kind.

### Gap 4: Retry context is too small for long-term policy quality

`RetryContext` currently carries:

- the error
- attempt count
- replayability

For production policy work, that is not enough. Policies will eventually need
stable access to information such as:

- request method or idempotency hint
- whether the request was committed to the wire
- which phase failed
- whether a connection had already been established

Without this, policy code tends to fall back to `kind()` and string matching.

### Gap 5: Error objects do not carry enough structured diagnostics

Today the error object does not expose operator-facing fields such as:

- target authority
- proxy mode / proxy endpoint
- retry count or attempt number
- response status when relevant
- exact failure phase

Tracing and event listeners already fill some of this gap, but the information
is outside the error object instead of attached to it.

### Gap 6: Observability hardening is not finished

The current branch already shows that retry/redirect trace expectations are not
fully stable yet. The integration test
`retry_and_redirect_events_follow_stable_order_and_trace_fields` currently fails
in filtered runs, which is a signal that the failure/diagnostic path is still
less trustworthy than the transport path itself.

That does not mean the execution chain is wrong. It means the observability
contract around failures still needs hardening.

## 4. Long-Term Goals

The long-term error system should satisfy these goals:

- callers can program against stable error semantics without parsing messages
- retry and follow-up policy decisions are driven by structured metadata
- request validation, extension failure, transport failure, and internal bugs
  are clearly separated
- response-body failures remain body-consumption failures, not call-execution
  failures
- logs, metrics, traces, and final surfaced errors all agree on the same phase
  model
- adding new transports or runtime integrations does not require changing the
  public failure contract

## 5. Target Handling Rules

The following rules should guide both current maintenance and future refactors.

### 5.1 Return a response instead of raising an error

These should normally remain regular HTTP outcomes:

- origin `4xx` and `5xx`
- redirect responses when redirect policy says `Stop`
- origin `401` or forward-proxy `407` responses when no authenticator is
  configured or when the authenticator declines to follow up

These are application-visible protocol results, not transport exceptions.

### 5.2 Recover internally only at explicit recovery points

OpenWire should only recover internally when all of the following are true:

- the failure happened before the request was committed to the network
- the request body is replayable
- policy explicitly allows retry
- the failure class is marked as retryable

That recovery window applies to:

- DNS
- TCP establishment
- proxy tunnel establishment
- retryable TLS handshake failures
- protocol binding during connection setup

### 5.3 Surface as hard errors immediately

These should be surfaced and not silently downgraded:

- invalid request or invalid client/proxy configuration
- missing required TLS support for an HTTPS request
- non-retryable TLS policy or certificate failures
- extension failures from interceptors or authenticators
- internal invariants and runtime/executor failures
- exhausted retries after policy says no further recovery is allowed
- CONNECT tunnel failures where no end-to-end HTTP response can be returned to
  the caller

### 5.4 Surface later during body consumption

These should remain body-consumption failures:

- response-body timeout
- truncated or malformed response stream
- UTF-8 decode failures
- JSON decode failures

The request execution contract should not report these through `call_failed`
once a response object has already been returned.

### 5.5 Do not fail the main request for side-effect-only facilities by default

If side channels later become fallible, they should default to best-effort
behavior unless strict mode is explicitly requested:

- cookie persistence
- telemetry export
- non-critical diagnostics sinks

This is a policy rule, not a reason to hide mainline transport failures.

## 6. Target Error Model

OpenWire does not necessarily need multiple public error structs. It does need a
more structured public model.

Long-term target fields or equivalent accessors:

- `kind`: stable top-level contract for callers
- `phase`: where the failure happened
- `retryability`: whether policy may retry this class before request commitment
- `source`: original lower-level error
- `context`: structured diagnostic metadata

Recommended phase vocabulary:

- `RequestValidation`
- `Admission`
- `Dns`
- `Tcp`
- `ProxyTunnel`
- `Tls`
- `ProtocolBinding`
- `RequestExchange`
- `ResponseHeaders`
- `ResponseBody`
- `Internal`

Recommended top-level semantic buckets:

- request/configuration failure
- extension/policy failure
- transport establishment failure
- protocol exchange failure
- body-consumption failure
- internal failure

The exact final type shape can change, but message strings must stop carrying
the full semantic load.

## 7. Execution Plan

### Phase 1: Freeze and document current semantics

Goals:

- document the current behavior and known gaps
- preserve the call-vs-body failure split
- preserve existing retry and redirect behavior unless a bug requires change

Deliverables:

- this roadmap document
- explicit links from architecture docs
- an issue list or checklist derived from the gaps in this document

### Phase 2: Add phase metadata without breaking behavior

Goals:

- introduce structured failure phase metadata
- ensure DNS, TCP, proxy, TLS, and body failures all populate it consistently

Deliverables:

- `WireError` or equivalent API gains stable phase accessors
- tests cover every major failure source and expected phase

### Phase 3: Normalize extension and policy boundaries

Goals:

- wrap interceptor-originated failures consistently
- introduce a stable classification for authenticator/policy failures

Deliverables:

- framework-owned wrapping at extension boundaries
- tests that prove extension failures cannot masquerade as transport failures

### Phase 4: Expand retry-policy inputs

Goals:

- let retry policy inspect stable fields instead of inferring from messages

Deliverables:

- richer `RetryContext`
- idempotency- and phase-aware retry tests

### Phase 5: Harden observability

Goals:

- make traces, event listeners, and surfaced errors agree
- close the current retry/redirect trace gap

Deliverables:

- green integration coverage for retry/redirect trace events
- stable field naming for production dashboards and logs

## 8. Success Criteria

This work is successful when:

- callers can distinguish connect timeout, body timeout, TLS policy failure,
  proxy tunnel rejection, and internal failure without message parsing
- default retry decisions are based on stable metadata instead of best-effort
  heuristics
- extension failures are clearly attributable to extension code
- the transport stack can add new connectors or runtimes without rewriting the
  public failure contract
- traces and event listeners consistently reflect the same failure phase as the
  final surfaced error

## 9. Maintenance Rule

This document should be kept aligned with the code until the roadmap is
implemented. Once the target behavior is fully absorbed into the codebase and
architecture reference, the roadmap can be replaced by a shorter closure doc or
merged into the canonical architecture documentation.
