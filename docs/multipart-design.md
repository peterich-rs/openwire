# Add Multipart Form Uploads

Status: implemented behind the opt-in `multipart` feature. Strict conformance to
applicable RFC requirements remains a design and delivery constraint. Protocol
evidence for the rows below is recorded against named tests; this does not
certify unrelated features or the entire library.

## Feasibility Assessment

OpenWire can already transport multipart bytes: `RequestBody::from_stream` and
`RequestBody::from_body` accept incremental payloads
(`crates/openwire-core/src/body.rs`, lines 74–122), and `Client::execute` accepts
`http::Request<RequestBody>` (`crates/openwire/src/client.rs`, lines 755–769).
The implementation adds a form encoder, replay-preserving body composition, and
independent length metadata. No connector or protocol replacement is required.
Feasible with caveats: body composition must preserve replay semantics, boundary
validation must be deterministic, and touched HTTP framing/follow-up behavior
must pass independent RFC-derived acceptance tests.

## Current Surface Inventory

- `openwire_core::RequestBody::{from_bytes, from_stream, from_stream_with_length, from_body, concat, content_length}` — memory, stream, known-length one-shot, arbitrary HTTP body, and DATA-only composition (`crates/openwire-core/src/body.rs`).
- `openwire_core::RequestBody::{try_clone, replayable_len}` — replayability is cached; `replayable_len()` returns a length only when the body is replayable and its complete length is known.
- `http_body::Body for openwire_core::RequestBody` — DATA/frame polling, remaining `size_hint()`, and end-state, including composite and strict-length streams.
- `openwire::bridge::normalize_body_headers` — frames from `content_length()`; rejects present unknown-length HTTP/1.0 bodies.
- `openwire::multipart::{Form, Part}` — opt-in encoder above `Client::execute`.
- `openwire::Client::execute` and `ClientBuilder` pipeline assembly — unchanged public request type and canonical chain.
- `openwire::policy::follow_up::RequestSnapshot::into_redirect_request` — 301/302 POST-to-GET; PUT/PATCH preserved when replayable; 303 GET except HEAD; 307/308 preserve method.
- `openwire::policy::follow_up::authenticate_response` — authentication follow-ups still require replayability.
- `openwire::transport::body::ObservedOutgoingBody` — uses final HTTP/1 framing length for completion bookkeeping, independently of source EOF; HTTP/2 remains EOF-driven.
- `openwire::logging::capture_request_body` — unchanged; generic body contract feeds its existing behavior.

## Design

### RFC conformance contract

Scope: the client-side form encoder and its HTTP/1.0, HTTP/1.1, HTTP/2,
follow-up, and connection-lifecycle integration. RFCs define protocol
requirements; project choices below select behavior within those requirements.
Code is evidence of current behavior, not a substitute for a normative source.
Use applicable updates and verified errata when implementing each row.

| ID | Authority / applicable obligation | Role and level | Owner | Required evidence | Status |
| --- | --- | --- | --- | --- | --- |
| MP-1 | [RFC 2046 §5.1.1](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.1), [RFC 7578 §4.1](https://www.rfc-editor.org/rfc/rfc7578.html#section-4.1): delimiter syntax, bounded boundary, collision exclusion | sender: required syntax / MUST NOT | encoder and payload guard | exact bytes; collisions across every split | verified: `multipart::tests::encodes_text_and_bytes_with_rfc_shape`, `stream_collision_is_rejected_before_complete_prefix`, `tests/multipart.rs::stream_collision_never_emits_forbidden_sequence`, `boundary_collisions_are_rejected_across_all_three_way_splits`, `safe_stream_payload_is_preserved_across_all_three_way_splits`, `boundary_guard_slices_large_safe_chunks_and_retains_only_candidate_bytes` |
| MP-2 | [RFC 7578 §§4.2–4.3](https://www.rfc-editor.org/rfc/rfc7578.html#section-4.2), [§5.2](https://www.rfc-editor.org/rfc/rfc7578.html#section-5.2): disposition, repeated fields, ordering | sender: MUST / SHOULD | form serializer | required name; duplicate/order fixtures | verified: `keeps_duplicate_fields_in_insertion_order`, `allows_empty_values_and_zero_byte_files` |
| MP-3 | [RFC 7578 §§2, 4.2, 4.5–4.8, 5.1](https://www.rfc-editor.org/rfc/rfc7578.html#section-4.2): filename, charset, permitted part fields | sender: MUST NOT / SHOULD NOT / MAY; choices below | metadata validator | escaping, Unicode, forbidden fields and charset conflicts | verified: `escapes_quotes_and_backslashes_and_encodes_non_ascii_filenames`, `rejects_control_characters_in_names`, `rejects_media_ranges_duplicates_nested_multipart_and_charset_conflicts` |
| MIME-1 | [RFC 2045 §5.1](https://www.rfc-editor.org/rfc/rfc2045.html#section-5.1): media-type/parameter syntax | sender: required syntax | metadata validator | concrete media types; malformed values; controls | verified: `rejects_media_ranges_duplicates_nested_multipart_and_charset_conflicts`, `mime_parameters_support_quoted_pairs_and_reject_incomplete_syntax` |
| HTTP-1 | [RFC 9112 §§6.1–6.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-6.1): valid request framing and unambiguous length | sender: MUST / MUST NOT | bridge | CL/TE exclusion; whole-body byte count; HTTP/1.0 rejection | verified: `http11_upload_matches_encoded_body_and_content_type`, `known_length_one_shot_form_uses_content_length`, `unknown_length_stream_form_uses_http11_chunked`, `http10_unknown_length_form_is_rejected_before_io`, `known_length_one_shot_stream_uses_content_length` |
| HTTP-2 | [RFC 9113 §§8.1–8.2.2](https://www.rfc-editor.org/rfc/rfc9113.html#section-8.1): content length, trailers, prohibited connection fields | sender: MUST / MUST NOT | protocol binding | actual H2 headers and DATA totals; TE restrictions | verified: `http2_upload_omits_transfer_encoding_and_matches_encoded_bytes` |
| POLICY-1 | [RFC 9110 §9.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2), [§15.4](https://www.rfc-editor.org/rfc/rfc9110.html#section-15.4): replay is distinct from retry permission; redirect semantics | client: MUST NOT / SHOULD / SHOULD NOT / MAY | follow-up coordinator | independent method/status table; committed POST cases | verified: `redirect_method_and_body_matrix_matches_rfc9110`, `put_301_second_hop_preserves_method_and_body`, `multipart_307_replays_identical_boundary_and_payload`, `replayable_multipart_form_is_authenticated_on_401` |
| LIFE-1 | [RFC 9112 §9.3](https://www.rfc-editor.org/rfc/rfc9112.html#section-9.3), [RFC 9113 §5.4](https://www.rfc-editor.org/rfc/rfc9113.html#section-5.4): safe failure handling and connection/stream disposition | client: required error handling; conservative reuse policy | transport lifecycle | no unsafe H1 reuse after incomplete upload; H2 stream failure | verified: `declared_length_mismatch_does_not_emit_suffix`, `source_read_error_does_not_read_later_parts`, `known_length_one_shot_form_is_not_retried_on_connection_failure`, `coalesced_http2_421_does_not_retry_one_shot_form`, `known_length_composites_emit_body_completion_once`, `http2_waits_for_eof_even_after_declared_data_is_read`, `fixed_length_does_not_turn_source_errors_into_success` |

Each row must gain named positive/negative tests and recorded command results
before it can become verified. Applicable mandatory violations block delivery.
Passing these rows does not certify unrelated features or the entire library.

Verified errata relevant to fixtures/terminology include
[RFC 7578 erratum 4676](https://www.rfc-editor.org/errata/eid4676), which corrects
the section 4.6 delimiters, and
[RFC 9113 erratum 7013](https://www.rfc-editor.org/errata/eid7013), which corrects
the DATA frame name in section 6.1. Do not copy the uncorrected examples or
treat merely reported errata as accepted corrections. Recheck the applicable
errata index when implementing each matrix row; this is not a completed audit
of all referenced RFCs.

### Key design decisions

1. **Start with `multipart/form-data` behind an opt-in `multipart` feature.**
   Support text, binary, and stream parts, including repeated names and explicit
   per-part media types. General MIME subtypes and response multipart parsing
   are separate work; they do not block uploads.
2. **Put `Form` and `Part` in `openwire::multipart`.** Encode before
   `Client::execute`, producing the existing `RequestBody`. A multipart
   interceptor or transport branch would put representation rules into the
   execution chain and complicate follow-ups.
3. **Add generic DATA-only composition to `openwire-core`.** Concatenate header
   bytes, payload bodies, separators, and the closing delimiter without
   collecting payloads. A multipart-specific core enum variant would couple
   shared body infrastructure to MIME; a general replay factory trait would
   introduce unnecessary lifecycle contracts for the first version.
4. **Separate total length from replayability.** A known-length one-shot stream
   can use Content-Length while remaining ineligible for replay. Retain the
   existing `try_clone()` protocol and `replayable_len()` compatibility API.
5. **Bind Content-Type and body in one request helper.** `Form::into_request`
   replaces Content-Type with the generated value; `Form::into_parts` is the
   lower-level escape hatch. Adding MIME metadata to every request body or a
   new client request-builder hierarchy is unnecessary for this feature.
6. **Accept app-owned streams rather than file paths.** File bytes can be read
   incrementally through `Part::stream` or `Part::stream_with_length`.
   Runtime-specific file opening, MIME guessing, and reopening mutable files
   are outside the encoder; adding filesystem I/O to core would weaken the
   runtime boundary.
7. **Validate boundaries rather than relying solely on randomness.** Scan
   in-memory payloads before construction and guard one-shot streams while
   polling. Keep the scanner in the multipart layer. The first version exposes
   text/Bytes/stream inputs, without `Part::body(RequestBody)`; a generic body
   transform that preserves replay would require a separate contract.

### Public interfaces

**File: `crates/openwire-core/src/body.rs`** — additive APIs; existing
constructors and method signatures remain available.

```rust
impl RequestBody {
    // Complete length at construction, independent of replayability.
    pub fn content_length(&self) -> Option<u64>;

    // Present, DATA-only composition of fresh, unpolled children.
    pub fn concat(bodies: Vec<RequestBody>) -> Result<Self, WireError>;

    // Counts bytes and reports length mismatches while the source is polled.
    pub fn from_stream_with_length<S, E>(stream: S, length: u64) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static;
}
```

- Store initial content length independently for DATA-only constructors.
  `from_body` keeps unknown framing length and remains non-replayable, even
  when its underlying `size_hint()` is exact: DATA size alone cannot establish
  that a body has no trailers. This preserves existing HTTP/1.1 chunked trailer
  behavior. Use the declared-length stream constructor for fixed-length
  one-shot DATA inputs; do not infer framing length from arbitrary bodies.
- `replayable_len()` keeps its existing meaning: return a length only when the
  body is replayable and its complete length is known. Cache replayability for
  composite bodies instead of cloning to answer a metadata query.
- A private composite representation retains the children and a polling cursor.
  `try_clone()` rewinds every child, including already-emitted children, and
  succeeds only if every child can replay. Preserve all encoded headers and the
  original boundary. An error does not change a one-shot body into a replayable
  body.
- Track whether a `RequestBody` has been polled; reject started children at
  composition time. Otherwise a consumed Bytes child can emit nothing while
  its clone emits the original payload. A fresh clone is a valid input.
- Sum known lengths with checked arithmetic; reject overflow. Unknown lengths
  produce an unknown total. Empty composition produces a present explicit-empty
  body, while an empty form is rejected by the form encoder.
- `Body::size_hint()` describes remaining bytes, while `content_length()` stays
  stable. Correct the existing Bytes branch, which currently reports the
  original length after emission (`body.rs`, lines 165–170).
- Strict-length streams remain non-replayable. Reaching the declared byte count
  does not imply source EOF: `Body::is_end_stream()` keeps the same EOF contract
  at the top level and inside any composition.
  Reject an oversized chunk before forwarding it and reject early EOF. In a
  multipart body this validation must finish before emitting the part suffix.
  Validation occurs when polled; standalone consumers can stop at their
  advertised length, whereas the multipart suffix requires advancing past the
  payload first.
- Report composite completion as soon as the final DATA segment is exhausted,
  without requiring another EOF poll. A strict-length child remains incomplete
  until source EOF is verified, even when its remaining size is zero.
- Transport observes framing separately from source EOF. After actual protocol
  binding, an HTTP/1 request with Content-Length and no Transfer-Encoding can
  complete outgoing byte bookkeeping once that length has been submitted,
  including zero-length and nested composite bodies. HTTP/2 still waits for
  body EOF/END_STREAM. Neither path turns a reported body error into success.
- Concatenation rejects trailers and other non-DATA frames, returns an error
  once, then terminates. It must never emit later separators after an error.
  A `from_body` child has unknown composition length, regardless of its raw
  size hint. Regular `from_body` retains its existing frame-forwarding and
  framing behavior outside composition.
- Bound work per poll when skipping empty children/frames. Do not prefetch
  subsequent payloads or spawn an upload task.

**File: `crates/openwire/src/multipart.rs`** — public API sketch, with private
fields and no public dependency on the MIME parser.

```rust
pub struct Form { /* ordered fields */ }
pub struct Part { /* payload and validated metadata */ }

impl Form {
    pub fn new() -> Self;
    pub fn text(self, name: impl Into<String>, value: impl Into<String>) -> Self;
    pub fn part(self, name: impl Into<String>, part: Part) -> Self;
    pub fn into_parts(self) -> Result<(HeaderValue, RequestBody), WireError>;
    pub fn into_request(
        self,
        builder: http::request::Builder,
    ) -> Result<http::Request<RequestBody>, WireError>;
}

impl Part {
    pub fn text(value: impl Into<String>) -> Self;
    pub fn bytes(value: impl Into<Bytes>) -> Self;
    pub fn stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static;
    pub fn stream_with_length<S, E>(stream: S, length: u64) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static;
    pub fn file_name(self, name: impl Into<String>) -> Self;
    pub fn mime_str(self, value: &str) -> Result<Self, WireError>;
}
```

`Form` also implements `Default`. `file_name` stores a display filename rather
than opening a path. Validate field names and filenames during form conversion;
`mime_str` validates immediately. Use optional `mime = "0.3"` internally for
media-type parsing, without exposing parser types in public signatures. Its
[`Mime` parser](https://docs.rs/mime/0.3.17/mime/struct.Mime.html#impl-FromStr-for-Mime)
validates the type/subtype. Parameters use a complete token/quoted-string parser
with quoted-pair decoding, closing-quote and separator validation, and duplicate
name rejection. This avoids a dependency parser rejecting valid escaped quotes
before the parameter parser runs. Serialization quotes/escapes decoded values
as required; parser acceptance alone is not proof of Content-Type validity.

**File: `crates/openwire/examples/post_multipart.rs`** — consumer
example, using the existing HTTP request builder.

```rust
use bytes::Bytes;
use http::Request;
use openwire::{multipart::{Form, Part}, Client};

let client = Client::builder().build()?;
let request = Form::new()
    .text("description", "upload from OpenWire")
    .part(
        "file",
        Part::bytes(Bytes::from_static(b"hello\n"))
            .file_name("hello.txt")
            .mime_str("text/plain; charset=utf-8")?,
    )
    .into_request(
        Request::builder()
            .method("POST")
            .uri("https://example.com/upload"),
    )?;
let response = client.execute(request).await?;
```

For large payloads substitute a stream part. The stream stays owned by the
request, follows downstream polling, and drops with the request. A known length
does not authorize reopening or replaying it.

### Encoding rules and failure behavior

Normative encoding obligations are tracked in MP-1 through MIME-1 above.
The following project choices implement that supported scope:

- Keep insertion order and repeated names; multiple files use separate parts
  with the same name. Each part has `Content-Disposition: form-data; name=...`.
- Use UTF-8 text and field names; add `text/plain; charset=utf-8` for text parts.
  Default binary/stream parts to `application/octet-stream` unless overridden.
- Escape DQUOTE and backslash in quoted parameters; reject CR, LF, NUL, and
  other control characters in names/metadata. Do not percent-encode field names
  as though they were URL query keys.
- Encode non-ASCII filename bytes with UTF-8 percent encoding; escape quoted
  parameters and encode literal percent signs unambiguously. This is the
  selected filename policy, not a claim that RFCs forbid other permitted
  encodings. Do not generate `filename*`.
- Do not expose arbitrary part headers or generate Content-Transfer-Encoding
  or per-part Content-Length.
- Require concrete media types rather than wildcard ranges. Reject duplicate
  parameter names and controls; reject nested `multipart/*` as an explicit
  first-version scope limit. Parser success is insufficient for these checks.
- Library-created text remains UTF-8: reject conflicting charset declarations
  and retain/add `charset=utf-8` when labeling it `text/plain`. Do not mechanically
  add charset to unrelated media types. Bytes/streams remain opaque app-owned
  content; validation does not prove an arbitrary file's format matches its label.

Additional implementation choices:

- Choose boundary candidates during conversion using 24 random bytes and a
  token-safe hexadecimal representation, reusing workspace `getrandom`.
  Keep the result within the 70-character limit. Scan all Bytes payloads before
  committing a candidate; retry collisions under a bounded attempt budget,
  then return a construction error if selection fails. Freeze the selected
  boundary and header bytes for every replay. No timestamp fallback.
- Store private payload variants for memory Bytes and erased one-shot streams
  with optional declared length. Attach a multipart-owned collision guard to
  each stream before converting it to `RequestBody::from_stream` or
  `from_stream_with_length`. This preserves known DATA length without wrapping
  the result in `from_body`; memory payloads remain replayable after scanning.
- The guard checks the start of each payload and every CRLF-started line for
  the full dash-boundary prefix. Retain candidate-prefix bytes across chunks
  and Pending polls, including arbitrarily many one-byte chunks. Scan retained
  bytes and the new chunk as one logical view without copying the new chunk;
  release only a proven-safe prefix and preserve a trailing CR. Use Bytes
  slices for the safe chunk region and O(boundary length) storage for unresolved
  bytes. Reject a collision before emitting its complete prefix, even
  when the following bytes are not CRLF or closing dashes. Return a body error,
  emit no later separator/closing delimiter, and do not mark encoder/body-write
  completion as successful.
  Do not pre-read an entire stream or silently change its bytes/boundary.
- Serialize each part as delimiter + headers + blank line + raw DATA + CRLF,
  followed by the closing delimiter. Reject zero-part forms; allow empty field
  values and zero-byte file payloads. Protocol lines use CRLF with no delimiter
  padding. Payload CR/LF bytes are preserved exactly.
- `into_request` uses header insertion to replace all existing Content-Type
  values, rather than appending a second value. Clear stale Content-Length and
  Transfer-Encoding; bridge computes framing from the resulting body. Reject
  a pre-existing Content-Encoding until an explicit encoding transform is
  supported, rather than labeling the raw form as already compressed.
- Invalid names, MIME values, started bodies, or length overflow fail before
  network I/O as `InvalidRequest`. Entropy failure is an internal construction
  error. Payload read/length/collision errors terminate the upload as body errors;
  preserve the original read error as the cause.

### Framing and follow-ups

| Encoded form | Length | Replay | Framing / follow-up behavior |
| --- | --- | --- | --- |
| Text and Bytes only | exact | yes | Content-Length; existing policy may replay |
| Includes a declared-length one-shot stream | exact | no | Content-Length; no body-preserving replay |
| Includes an unknown-length stream | unknown | no | HTTP/1.1 chunked; HTTP/2 DATA without Transfer-Encoding |

Change bridge to read `content_length()`. Reject present unknown-length bodies
for HTTP/1.0 before sending; neither connection-close request delimiting nor
HTTP/1.1 chunked coding is a valid substitute. Do not silently buffer or change
the requested protocol. Verify framing at the actual protocol-binding boundary.

No HTTP/1.1 request may carry both Content-Length and Transfer-Encoding. H2
must omit prohibited connection fields, with `TE` restricted to `trailers`.
An emitted Content-Length covers all encoded content bytes. Preserve legitimate
outer HTTP trailers for ordinary bodies while rejecting trailers inside parts.

Use `try_clone()` only to establish replay capability. Retry/redirect/auth
decisions still require the applicable policy and protocol conditions:

- 307/308 return the original response when a one-shot payload cannot replay.
- For 301/302 choose the permitted POST-to-GET behavior; preserve other methods
  and their body when replayable. For 303 choose GET retrieval (retain HEAD for
  HEAD). For 307/308 preserve the method. Remove content-specific fields when
  switching to retrieval, using a spec-derived list rather than copying the
  current implementation's list.
- Origin/proxy authentication follow-ups require replayable bodies.
- Replayable does not mean retryable: preserve the existing limits on retrying
  a committed non-idempotent POST.

The current 301/302 rewrite is broader than this policy. Correct the touched
coordinator path and its independent method/status tests; do not declare it
compliant solely because it predates multipart. Read errors, collisions, and
length failures leave the upload incomplete: close an unsafe H1 connection and
handle H2 failure at the appropriate stream scope without unsafe connection reuse.

## Phased Implementation

## Phase 1: Extend Generic Request Bodies

**File: `crates/openwire-core/src/body.rs`**

- Add independent total-length metadata, DATA-only `concat`, private composite
  state, started-input validation, and strict-length stream construction.
- Preserve existing absent/explicit-empty and clone behavior; correct Bytes
  remaining-size reporting.
- Add unit coverage for known-length one-shot bodies, replay after partial and
  complete consumption, empty composition, length overflow, trailers, terminal
  errors, Pending behavior, and length validation at actual EOF.

**File: `crates/openwire/src/bridge.rs`**

- Read `content_length()` for framing. Test a known-length non-replayable body
  and preserve normalization of absent, empty, and unknown-length bodies.
  Keep arbitrary `from_body` inputs on their existing framing path, including
  exact-size inputs that can still emit trailers.
- Reject present unknown-length HTTP/1.0 requests via the existing fallible
  normalization path. Do not serialize an undelimited request or chunked 1.0.

**Files: `crates/openwire/src/transport/body.rs`, `crates/openwire/src/transport/service.rs`**

- Pass the actual connection protocol to outgoing instrumentation and empty-body
  dispatch. Capture final Content-Length only for HTTP/1 without Transfer-Encoding.
- Complete HTTP/1 byte bookkeeping even when a composite source has not received
  an EOF poll. Keep HTTP/2 source errors, resets, and cancellation before EOF
  distinct from successful completion; preserve arbitrary-body trailers.

This phase compiles without any multipart feature and establishes the reusable
body behavior independently of MIME.

## Phase 2: Add the Form Encoder and Public Feature

**File: `crates/openwire/Cargo.toml`**

- Add `multipart = ["dep:getrandom", "dep:mime"]` and optional `mime = "0.3"`.
  Keep defaults unchanged and declare the multipart example's required feature.

**File: `Cargo.lock`**

- Resolve the new optional parser dependency with the repository toolchain;
  keep the declared Rust 1.80 minimum in view.

**File: `crates/openwire/src/lib.rs`**

- Export `pub mod multipart` under the feature; core remains unaware of MIME.

**File: `crates/openwire/src/multipart.rs`**

- Implement `Form`, `Part`, ordered encoding, validation, random boundary
  selection with memory scanning, incremental collision guards for one-shot
  streams, and the paired header/body conversion methods.
- Construct reusable Bytes header segments and concatenate them with payloads.
  Do not wrap the final form with `from_body`, which would discard replay.
- Add deterministic wire fixtures based on RFC examples, plus escaping,
  non-ASCII names/filenames, duplicate fields, empty values, and binary payloads.
- Test every boundary split, payload-start/end collisions, prefix-only matches,
  Pending polls, candidate regeneration, and terminal collision failure. Check
  protocol CRLF/padding separately from byte-for-byte payload preservation.
- Test media ranges, malformed/duplicate parameters, text charset conflicts,
  UTF-8 preservation, and rejection of unsupported nested multipart metadata.

## Phase 3: Correct Touched HTTP Follow-up Behavior

**File: `crates/openwire/src/policy/follow_up.rs`**

- Restrict the selected 301/302 POST-to-GET rewrite to POST. Preserve other
  methods and require body replay when preserving content; do not silently
  replace a one-shot preserved body with an absent body.
- Use the independent RFC 9110 method/status table for 303/307/308 as well.
- Complete content-specific header cleanup on retrieval rewrites, covering
  Content-Language, Content-Location, Digest, and Last-Modified in addition to
  framing and content-type/encoding fields. Account for newer applicable
  field specifications during implementation.
- Correct the in-module tests and comments that currently treat all non-GET/HEAD
  301/302 requests as GET. Expected values come from the RFC plus documented
  permitted policy choices, not the old implementation.

The rest of the existing retry/auth orchestration remains reusable, subject to
the acceptance evidence below. Any newly discovered mandatory violation on
this integration path blocks completion until resolved.

## Phase 4: Add Examples and Synchronize Documentation

**File: `crates/openwire/examples/post_multipart.rs`**

- Provide a complete runnable text/binary form example and show how to replace
  the payload with a one-shot stream. State the required feature.

**File: `README.md`**

- Document the opt-in feature, request helper, Content-Type ownership, and the
  distinction between length and replayability.

**File: `docs/ARCHITECTURE.md`**

- Place form construction above `Client::execute`; document generic body
  composition and independent length/replay metadata.

**File: `docs/testing.md`**

- Add multipart feature verification and deterministic encoding expectations.

**File: `docs/multipart-design.md`**

- Update proposal status and reconcile APIs with the implementation as it lands.

## Phase 5: Verify Wire Behavior and RFC Policy Integration

**File: `crates/openwire/tests/events.rs`**

- Cover known-length composites ending in Strict bodies, including zero length,
  nonempty prefixes, and nested composition. The local server collects the whole
  request before replying; assert matching payload and one start/end event pair.

**File: `crates/openwire/src/transport/body.rs` (outgoing unit tests)**

- Cover HTTP/1 framing completion without EOF polling, actual HTTP/2 waiting for
  EOF after all DATA, source errors, incomplete sends, zero-length framing, and
  Transfer-Encoding excluding fixed-length completion.

**File: `crates/openwire/tests/multipart.rs`**

- Exercise HTTP/1.1 and local HTTP/2 uploads using `openwire-test`, with the
  feature disabled cleanly gating this target's tests.
- Verify exact encoded body/header agreement, known-length one-shot framing,
  unknown-length framing, and application/network interceptor visibility.
- Assert CL/TE exclusion, actual H2 field constraints, exact encoded byte
  totals, and early rejection of unknown-length HTTP/1.0 with no upload I/O.
- Test a producer that returns Pending, source read errors, cancellation, and
  source drop. Assert no later part is read after failure/cancellation.
- Inject boundary collisions at every chunk split and assert the complete
  forbidden sequence and closing boundary are never emitted. Verify H1
  teardown after incomplete upload and appropriate H2 stream cleanup.
- Check too-short/too-long declarations, including declared zero with data and
  exactly N bytes followed by another chunk; suffixes must not hide errors.
- Verify request-body events count encoded bytes and complete exactly once,
  including the final frame path that receives no extra EOF poll from hyper.

**File: `crates/openwire/tests/redirect.rs`**

- Add feature-gated 307/308 replay with byte-identical boundary and payload,
  one-shot refusal, and GET rewrites with multipart headers removed.
- Include PUT/PATCH in 301/302 tests, retrieval header cleanup, and one-shot
  preserved-body refusal. These supplement feature-independent policy tests.

**File: `crates/openwire/tests/auth.rs`**

- Add feature-gated replayable and one-shot form authentication cases.

**File: `crates/openwire/tests/compression_bridge.rs`**

- Verify a declared-length DATA-only one-shot stream uses Content-Length
  independently of multipart; normalize a caller's stale framing headers.
- Verify an arbitrary body with exact DATA size retains chunked HTTP/1.1
  framing and its declared trailers, including zero DATA bytes. A raw size hint
  must not be mistaken for permission to suppress trailing frames.

**File: `crates/openwire/tests/pool_http2.rs`**

- Cover a known-length one-shot form failing without automatic body replay.

Run focused tests first, then the existing workspace gates:

```bash
cargo test -p openwire-core --lib
cargo test -p openwire --features multipart --test multipart --test redirect --test auth --test compression_bridge --test pool_http2
cargo check -p openwire --no-default-features --features multipart --lib
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --all-targets --all-features
```

Use local servers and fixed encoder fixtures, without public-internet tests.
HTTP/2 checks concern headers, payloads, and OpenWire policy rather than
retesting h2 framing. Audit dependency/MSRV compatibility when updating the lock.

Verification after the multipart review fixes (2026-11-12):

- `cargo test --workspace --all-targets --quiet` — passed.
- `cargo test --workspace --all-targets --all-features --quiet` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- `cargo check -p openwire --no-default-features --features multipart --lib` — passed.
- `cargo fmt --all --check` and `git diff --check` — passed.

The three reported failure scenarios were reproduced before applying the fixes. The current suite
also checks byte preservation and zero-copy output, rejected malformed MIME
parameters, and unsuccessful completion after source errors or H2 cancellation.

## Architectural Notes

- **Compatibility:** body/form APIs are additive and optional. HTTP/1.0
  undelimited uploads become explicit errors; 301/302 method preservation is a
  user-visible correction for non-POST methods. No signature migration is
  required, but release notes must describe these corrections. Ordinary
  arbitrary-body trailer framing remains preserved on applicable protocols.
- **Trait bounds:** retain the current `Send + Sync + 'static` body contract.
  No new object-safe replay trait or connector trait changes are needed.
- **Memory and side effects:** the encoder retains metadata and shared Bytes
  segments, with memory-payload scanning, bounded boundary-selection attempts,
  and one fixed boundary per built form. Stream collision detection uses
  O(boundary length) retained prefix bytes and incremental payload polling.
  Existing `LogLevel::Body` can still collect an entire replayable form for
  logging; the encoder's allocation guarantee does not cover that opt-in
  observer. One-shot bodies continue to be omitted from request-body logging.
- **Execution chain:** no bypass of application interceptors, follow-up
  coordination, bridge, network interceptors, transport, or connection release.
- **Audited without planned edits:** `crates/openwire/src/client.rs`,
  `crates/openwire/src/transport/protocol.rs`, and
  `crates/openwire/src/logging.rs`; the generic body contract feeds their
  existing behavior. Their existing integration still requires RFC acceptance
  evidence; adjust this inventory if that evidence exposes a violation.
- **Dependencies:** only `openwire` gains optional MIME parsing and reuses its
  optional randomness dependency. No new core or Tokio/filesystem dependency.
- **Deferred:** file-path convenience, arbitrary RequestBody parts with
  replay-preserving guards, replay factories for stable external sources,
  general MIME subtypes, and response multipart parsing. They need
  separate contracts and are unnecessary for streaming form uploads.
- **Documentation audit:** README describes the implemented feature and API.
  ARCHITECTURE also records transport completion versus source EOF and bounded
  multipart collision detection. Keep these aligned with the implementation.

## File Change Summary

Implementation has landed. Keep this document aligned with the code; RFC rows
above list the named tests that currently provide evidence.

- `AGENTS.md` -- persist project-wide strict RFC requirements.
- `Cargo.lock` -- resolve the optional MIME parser dependency.
- `crates/openwire-core/src/body.rs` -- length/replay separation, composition, strict-length streams, and unit tests.
- `crates/openwire/Cargo.toml` -- optional multipart feature, dependency, and example gate.
- `crates/openwire/examples/post_multipart.rs` -- text, binary, and stream usage.
- `crates/openwire/src/bridge.rs` -- length-based framing and HTTP/1.0 validation.
- `crates/openwire/src/lib.rs` -- feature-gated public multipart module.
- `crates/openwire/src/multipart.rs` -- form/part API, metadata validation, and collision-safe incremental encoding.
- `crates/openwire/src/policy/follow_up.rs` -- RFC-derived redirect method and header handling.
- `crates/openwire/src/transport/body.rs` -- protocol-specific outgoing completion and failure bookkeeping.
- `crates/openwire/src/transport/service.rs` -- pass the negotiated protocol to instrumentation and dispatch.
- `crates/openwire/tests/auth.rs` -- multipart authentication replay coverage.
- `crates/openwire/tests/compression_bridge.rs` -- known-length one-shot framing and arbitrary-body trailer preservation.
- `crates/openwire/tests/events.rs` -- completion events for zero-length and nested composite uploads.
- `crates/openwire/tests/multipart.rs` -- wire, backpressure, cancellation, and events.
- `crates/openwire/tests/pool_http2.rs` -- one-shot multipart retry constraints.
- `crates/openwire/tests/redirect.rs` -- replay, boundary stability, and GET rewrites.
- `README.md` -- feature and upload usage.
- `docs/ARCHITECTURE.md` -- strict RFC priority, encoding placement, and body capabilities.
- `docs/multipart-design.md` -- keep the design aligned with implementation.
- `docs/testing.md` -- RFC requirement-to-evidence rules and multipart verification entry points.
