//! Client-side `multipart/form-data` encoder.
//!
//! Encoding happens before [`Client::execute`](crate::Client::execute). The
//! result is an ordinary [`RequestBody`] plus a generated `Content-Type`.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use http::{HeaderValue, Request};
use mime::Mime;
use openwire_core::{RequestBody, WireError};
use pin_project_lite::pin_project;

const BOUNDARY_ATTEMPTS: usize = 16;
const BOUNDARY_RANDOM_BYTES: usize = 24;
const EMPTY_CHUNK_SKIP_BUDGET: usize = 16;
const CRLF: &[u8] = b"\r\n";
const DASH_DASH: &[u8] = b"--";

pub struct Form {
    fields: Vec<FormField>,
}

struct FormField {
    name: String,
    part: Part,
}

pub struct Part {
    payload: PartPayload,
    file_name: Option<String>,
    mime: Option<ValidatedMime>,
}

enum PartPayload {
    Memory {
        bytes: Bytes,
        kind: MemoryKind,
    },
    Stream {
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, WireError>> + Send + Sync>>,
        length: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryKind {
    Text,
    Bytes,
}

#[derive(Clone, Debug)]
struct ValidatedMime {
    serialized: String,
}

impl Form {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn text(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.part(name, Part::text(value))
    }

    pub fn part(mut self, name: impl Into<String>, part: Part) -> Self {
        self.fields.push(FormField {
            name: name.into(),
            part,
        });
        self
    }

    pub fn into_parts(self) -> Result<(HeaderValue, RequestBody), WireError> {
        if self.fields.is_empty() {
            return Err(WireError::invalid_request(
                "multipart form must contain at least one part",
            ));
        }

        let mut encoded_fields = Vec::with_capacity(self.fields.len());
        for field in self.fields {
            encoded_fields.push(EncodedField::from_form_field(field)?);
        }

        let boundary = select_boundary(&encoded_fields)?;
        let mut segments = Vec::new();
        for field in encoded_fields {
            segments.push(RequestBody::from_bytes(part_preamble(&boundary, &field)));
            segments.push(field.into_payload_body(&boundary)?);
            segments.push(RequestBody::from_bytes(Bytes::from_static(CRLF)));
        }
        segments.push(RequestBody::from_bytes(closing_delimiter(&boundary)));

        let content_type = HeaderValue::from_str(&format!(
            "multipart/form-data; boundary={boundary}"
        ))
        .map_err(|error| {
            WireError::internal(
                "generated multipart Content-Type is not a valid header value",
                error,
            )
        })?;
        let body = RequestBody::concat(segments)?;
        Ok((content_type, body))
    }

    #[cfg(test)]
    fn into_parts_with_boundary(
        self,
        boundary: &str,
    ) -> Result<(HeaderValue, RequestBody), WireError> {
        let mut encoded_fields = Vec::with_capacity(self.fields.len());
        for field in self.fields {
            encoded_fields.push(EncodedField::from_form_field(field)?);
        }
        let mut segments = Vec::new();
        for field in encoded_fields {
            segments.push(RequestBody::from_bytes(part_preamble(boundary, &field)));
            segments.push(field.into_payload_body(boundary)?);
            segments.push(RequestBody::from_bytes(Bytes::from_static(CRLF)));
        }
        segments.push(RequestBody::from_bytes(closing_delimiter(boundary)));
        let content_type =
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}"))
                .expect("header");
        Ok((content_type, RequestBody::concat(segments)?))
    }

    pub fn into_request(
        self,
        builder: http::request::Builder,
    ) -> Result<Request<RequestBody>, WireError> {
        let (content_type, body) = self.into_parts()?;
        let mut request = builder.body(body)?;
        if request.headers().contains_key(CONTENT_ENCODING) {
            return Err(WireError::invalid_request(
                "multipart request cannot reuse a pre-existing Content-Encoding",
            ));
        }
        request.headers_mut().insert(CONTENT_TYPE, content_type);
        request.headers_mut().remove(CONTENT_LENGTH);
        request.headers_mut().remove(TRANSFER_ENCODING);
        Ok(request)
    }
}

impl Default for Form {
    fn default() -> Self {
        Self::new()
    }
}

impl Part {
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            payload: PartPayload::Memory {
                bytes: Bytes::from(value.into()),
                kind: MemoryKind::Text,
            },
            file_name: None,
            mime: None,
        }
    }

    pub fn bytes(value: impl Into<Bytes>) -> Self {
        Self {
            payload: PartPayload::Memory {
                bytes: value.into(),
                kind: MemoryKind::Bytes,
            },
            file_name: None,
            mime: None,
        }
    }

    pub fn stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static,
    {
        Self {
            payload: PartPayload::Stream {
                inner: Box::pin(map_err_stream(stream)),
                length: None,
            },
            file_name: None,
            mime: None,
        }
    }

    pub fn stream_with_length<S, E>(stream: S, length: u64) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static,
    {
        Self {
            payload: PartPayload::Stream {
                inner: Box::pin(map_err_stream(stream)),
                length: Some(length),
            },
            file_name: None,
            mime: None,
        }
    }

    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    pub fn mime_str(mut self, value: &str) -> Result<Self, WireError> {
        self.mime = Some(validate_mime(value, self.is_library_text())?);
        Ok(self)
    }

    fn is_library_text(&self) -> bool {
        matches!(
            self.payload,
            PartPayload::Memory {
                kind: MemoryKind::Text,
                ..
            }
        )
    }
}

struct EncodedField {
    name: String,
    file_name: Option<String>,
    content_type: String,
    payload: EncodedPayload,
}

enum EncodedPayload {
    Memory(Bytes),
    Stream {
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, WireError>> + Send + Sync>>,
        length: Option<u64>,
    },
}

impl EncodedField {
    fn from_form_field(field: FormField) -> Result<Self, WireError> {
        validate_token_text(&field.name, "multipart field name")?;
        if let Some(file_name) = &field.part.file_name {
            validate_token_text(file_name, "multipart file name")?;
        }

        let content_type = match field.part.mime {
            Some(mime) => mime.serialized,
            None if field.part.is_library_text() => "text/plain; charset=utf-8".to_owned(),
            None => "application/octet-stream".to_owned(),
        };

        let payload = match field.part.payload {
            PartPayload::Memory { bytes, .. } => EncodedPayload::Memory(bytes),
            PartPayload::Stream { inner, length } => EncodedPayload::Stream { inner, length },
        };

        Ok(Self {
            name: field.name,
            file_name: field.part.file_name,
            content_type,
            payload,
        })
    }

    fn into_payload_body(self, boundary: &str) -> Result<RequestBody, WireError> {
        match self.payload {
            EncodedPayload::Memory(bytes) => Ok(RequestBody::from_bytes(bytes)),
            EncodedPayload::Stream { inner, length } => {
                let guarded = BoundaryGuardStream {
                    inner,
                    needle: dash_boundary(boundary),
                    ready: None,
                    hold: Vec::new(),
                    at_line_start: true,
                };
                Ok(match length {
                    Some(length) => RequestBody::from_stream_with_length(guarded, length),
                    None => RequestBody::from_stream(guarded),
                })
            }
        }
    }
}

fn wrap_payload_read_error(error: WireError) -> WireError {
    if error.kind() == openwire_core::WireErrorKind::Body {
        error
    } else {
        WireError::body(
            "multipart payload read failed",
            std::io::Error::other(error.to_string()),
        )
    }
}

fn map_err_stream<S, E>(stream: S) -> impl Stream<Item = Result<Bytes, WireError>> + Send + Sync
where
    S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
    E: Into<WireError> + 'static,
{
    MapErrStream { inner: stream }
}

pin_project! {
    struct MapErrStream<S> {
        #[pin]
        inner: S,
    }
}

impl<S, E> Stream for MapErrStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<WireError>,
{
    type Item = Result<Bytes, WireError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.project().inner.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(Some(Err(wrap_payload_read_error(error.into()))))
            }
        }
    }
}

pin_project! {
    struct BoundaryGuardStream<S> {
        #[pin]
        inner: S,
        needle: Vec<u8>,
        ready: Option<Bytes>,
        hold: Vec<u8>,
        at_line_start: bool,
    }
}

impl<S> Stream for BoundaryGuardStream<S>
where
    S: Stream<Item = Result<Bytes, WireError>>,
{
    type Item = Result<Bytes, WireError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let mut skips = 0usize;
        loop {
            if let Some(ready) = this.ready.take() {
                if !ready.is_empty() {
                    return Poll::Ready(Some(Ok(ready)));
                }
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if this.hold.is_empty() {
                        return Poll::Ready(None);
                    }
                    if collision_in_pending(this.hold, this.needle, *this.at_line_start) {
                        return Poll::Ready(Some(Err(boundary_collision_error())));
                    }
                    let rest = Bytes::from(std::mem::take(this.hold));
                    return Poll::Ready(Some(Ok(rest)));
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        skips += 1;
                        if skips >= EMPTY_CHUNK_SKIP_BUDGET {
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        continue;
                    }
                    match split_safe_chunk(
                        chunk,
                        this.hold,
                        this.ready,
                        this.needle,
                        this.at_line_start,
                    ) {
                        Ok(Some(safe)) => return Poll::Ready(Some(Ok(safe))),
                        Ok(None) => {
                            skips += 1;
                            if skips >= EMPTY_CHUNK_SKIP_BUDGET {
                                cx.waker().wake_by_ref();
                                return Poll::Pending;
                            }
                            continue;
                        }
                        Err(error) => return Poll::Ready(Some(Err(error))),
                    }
                }
            }
        }
    }
}

fn select_boundary(fields: &[EncodedField]) -> Result<String, WireError> {
    for _ in 0..BOUNDARY_ATTEMPTS {
        let boundary = random_boundary()?;
        if memory_payloads_collide(fields, &boundary) {
            continue;
        }
        return Ok(boundary);
    }
    Err(WireError::invalid_request(
        "failed to select a multipart boundary that does not collide with in-memory payloads",
    ))
}

fn random_boundary() -> Result<String, WireError> {
    let mut bytes = [0u8; BOUNDARY_RANDOM_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|error| {
        WireError::internal(
            "failed to generate multipart boundary",
            std::io::Error::other(error.to_string()),
        )
    })?;
    Ok(hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn memory_payloads_collide(fields: &[EncodedField], boundary: &str) -> bool {
    let needle = dash_boundary(boundary);
    fields.iter().any(|field| match &field.payload {
        EncodedPayload::Memory(bytes) => contains_dash_boundary(bytes, &needle),
        EncodedPayload::Stream { .. } => false,
    })
}

fn contains_dash_boundary(haystack: &[u8], needle: &[u8]) -> bool {
    contains_dash_boundary_from(haystack, needle, true)
}

fn contains_dash_boundary_from(haystack: &[u8], needle: &[u8], at_line_start: bool) -> bool {
    if needle.is_empty() {
        return false;
    }
    if at_line_start && haystack.starts_with(needle) {
        return true;
    }
    haystack
        .windows(2 + needle.len())
        .any(|window| window.starts_with(CRLF) && window[2..].starts_with(needle))
}

fn dash_boundary(boundary: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + boundary.len());
    out.extend_from_slice(DASH_DASH);
    out.extend_from_slice(boundary.as_bytes());
    out
}

fn closing_delimiter(boundary: &str) -> Bytes {
    let mut out = Vec::with_capacity(6 + boundary.len());
    out.extend_from_slice(DASH_DASH);
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(DASH_DASH);
    out.extend_from_slice(CRLF);
    Bytes::from(out)
}

fn part_preamble(boundary: &str, field: &EncodedField) -> Bytes {
    let mut out = String::new();
    out.push_str("--");
    out.push_str(boundary);
    out.push_str("\r\n");
    out.push_str("Content-Disposition: form-data; name=");
    out.push_str(&quoted_string(&field.name));
    if let Some(file_name) = &field.file_name {
        out.push_str("; filename=");
        out.push_str(&quoted_string(&encode_filename(file_name)));
    }
    out.push_str("\r\nContent-Type: ");
    out.push_str(&field.content_type);
    out.push_str("\r\n\r\n");
    Bytes::from(out)
}

fn quoted_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn encode_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'%' | 0x80..=0xff => {
                out.push('%');
                out.push(HEX_UPPER[(byte >> 4) as usize] as char);
                out.push(HEX_UPPER[(byte & 0x0f) as usize] as char);
            }
            _ => out.push(*byte as char),
        }
    }
    out
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn validate_token_text(value: &str, label: &str) -> Result<(), WireError> {
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(WireError::invalid_request(format!(
            "{label} contains a forbidden control character"
        )));
    }
    Ok(())
}

fn validate_mime(value: &str, library_text: bool) -> Result<ValidatedMime, WireError> {
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(WireError::invalid_request(
            "multipart media type contains a forbidden control character",
        ));
    }
    // mime 0.3 does not support every quoted-pair. Validate only the essence
    // there, and parse the complete parameter grammar below.
    let essence = value.split(';').next().unwrap_or(value).trim();
    let parsed: Mime = essence.parse().map_err(|error| {
        WireError::invalid_request(format!("multipart media type is invalid: {error}"))
    })?;
    if parsed.type_() == mime::STAR || parsed.subtype() == mime::STAR {
        return Err(WireError::invalid_request(
            "multipart media type must be a concrete type, not a range",
        ));
    }
    if parsed.type_() == mime::MULTIPART {
        return Err(WireError::invalid_request(
            "nested multipart media types are not supported",
        ));
    }

    let params = parse_mime_parameters(value)?;
    for (_name, param_value) in &params {
        if param_value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(WireError::invalid_request(
                "multipart media type parameter contains a forbidden control character",
            ));
        }
    }

    if library_text {
        if let Some((_, charset)) = params
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("charset"))
        {
            if !charset.eq_ignore_ascii_case("utf-8") {
                return Err(WireError::invalid_request(
                    "text part Content-Type charset must be utf-8",
                ));
            }
        }
    }

    let is_text_plain = parsed.type_() == mime::TEXT && parsed.subtype() == mime::PLAIN;
    if library_text && is_text_plain && !params.iter().any(|(name, _)| name == "charset") {
        return Ok(ValidatedMime {
            serialized: format_media_type_with_utf8(
                parsed.type_().as_str(),
                parsed.subtype().as_str(),
                &params,
            ),
        });
    }

    Ok(ValidatedMime {
        serialized: format_media_type(parsed.type_().as_str(), parsed.subtype().as_str(), &params),
    })
}

fn parse_mime_parameters(value: &str) -> Result<Vec<(String, String)>, WireError> {
    let Some((_, rest)) = value.split_once(';') else {
        return Ok(Vec::new());
    };
    let bytes = rest.as_bytes();
    let mut index = 0usize;
    let mut params = Vec::new();
    while index < bytes.len() {
        while index < bytes.len()
            && (bytes[index] == b' ' || bytes[index] == b'\t' || bytes[index] == b';')
        {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len() && is_mime_token_byte(bytes[index]) {
            index += 1;
        }
        if index == name_start {
            return Err(WireError::invalid_request(
                "multipart media type parameter name is invalid",
            ));
        }
        let name = rest[name_start..index].to_ascii_lowercase();
        if params.iter().any(|(existing, _)| existing == &name) {
            return Err(WireError::invalid_request(
                "multipart media type has a duplicate parameter name",
            ));
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            return Err(WireError::invalid_request(
                "multipart media type parameter is missing a value",
            ));
        }
        index += 1;
        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut decoded = Vec::new();
            loop {
                if index == bytes.len() {
                    return Err(WireError::invalid_request(
                        "multipart media type quoted-string is not closed",
                    ));
                }
                match bytes[index] {
                    b'\\' => {
                        index += 1;
                        let Some(&escaped) = bytes.get(index) else {
                            return Err(WireError::invalid_request(
                                "multipart media type quoted-string is truncated",
                            ));
                        };
                        decoded.push(escaped);
                        index += 1;
                    }
                    b'"' => {
                        index += 1;
                        break;
                    }
                    byte => {
                        decoded.push(byte);
                        index += 1;
                    }
                }
            }
            String::from_utf8(decoded).map_err(|error| {
                WireError::invalid_request(format!(
                    "multipart media type parameter is not valid UTF-8: {error}"
                ))
            })?
        } else {
            let value_start = index;
            while index < bytes.len() && is_mime_token_byte(bytes[index]) {
                index += 1;
            }
            if index == value_start {
                return Err(WireError::invalid_request(
                    "multipart media type parameter value must be a token or quoted-string",
                ));
            }
            rest[value_start..index].to_owned()
        };
        while index < bytes.len() && bytes[index] == b' ' {
            index += 1;
        }
        if index < bytes.len() && bytes[index] != b';' {
            return Err(WireError::invalid_request(
                "multipart media type parameters must be separated by semicolons",
            ));
        }
        params.push((name, value));
    }
    Ok(params)
}

fn format_media_type(type_: &str, subtype: &str, params: &[(String, String)]) -> String {
    let mut out = format!("{type_}/{subtype}");
    for (name, value) in params {
        out.push_str("; ");
        out.push_str(name);
        out.push('=');
        out.push_str(&format_mime_parameter_value(value));
    }
    out
}

fn format_media_type_with_utf8(type_: &str, subtype: &str, params: &[(String, String)]) -> String {
    let mut out = format!("{type_}/{subtype}; charset=utf-8");
    for (name, value) in params {
        if name.eq_ignore_ascii_case("charset") {
            continue;
        }
        out.push_str("; ");
        out.push_str(name);
        out.push('=');
        out.push_str(&format_mime_parameter_value(value));
    }
    out
}

fn format_mime_parameter_value(value: &str) -> String {
    if is_mime_token(value) {
        value.to_owned()
    } else {
        quoted_string(value)
    }
}

fn is_mime_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_mime_token_byte)
}

fn is_mime_token_byte(byte: u8) -> bool {
    matches!(byte,
                b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'{'
                    | b'|'
                    | b'}'
                    | b'~'
    )
}

fn split_safe_chunk(
    chunk: Bytes,
    hold: &mut Vec<u8>,
    ready: &mut Option<Bytes>,
    needle: &[u8],
    at_line_start: &mut bool,
) -> Result<Option<Bytes>, WireError> {
    let mut previous = std::mem::take(hold);
    let previous_len = previous.len();
    let total = previous_len + chunk.len();
    // Treat the retained prefix and source chunk as one view. Only unresolved
    // bytes are copied; the large safe chunk region stays a Bytes slice.
    let byte_at = |index: usize| {
        if index < previous_len {
            previous[index]
        } else {
            chunk[index - previous_len]
        }
    };
    let mut safe_end = total;
    for start in 0..total {
        let is_line_start = (start == 0 && *at_line_start)
            || (start >= 2 && byte_at(start - 2) == b'\r' && byte_at(start - 1) == b'\n');
        if !is_line_start {
            continue;
        }
        let available = needle.len().min(total - start);
        if (0..available).all(|offset| byte_at(start + offset) == needle[offset]) {
            if available == needle.len() {
                return Err(boundary_collision_error());
            }
            safe_end = start;
            break;
        }
    }
    if safe_end == total && total > 0 && byte_at(total - 1) == b'\r' {
        safe_end -= 1;
    }
    if safe_end > 0 {
        *at_line_start =
            safe_end >= 2 && byte_at(safe_end - 2) == b'\r' && byte_at(safe_end - 1) == b'\n';
    }
    hold.extend((safe_end..total).map(byte_at));
    debug_assert!(hold.len() < needle.len());

    let safe_chunk = chunk.slice(..safe_end.saturating_sub(previous_len));
    previous.truncate(safe_end.min(previous_len));
    if previous.is_empty() {
        Ok((!safe_chunk.is_empty()).then_some(safe_chunk))
    } else {
        if !safe_chunk.is_empty() {
            *ready = Some(safe_chunk);
        }
        Ok(Some(Bytes::from(previous)))
    }
}

fn collision_in_pending(pending: &[u8], needle: &[u8], at_line_start: bool) -> bool {
    contains_dash_boundary_from(pending, needle, at_line_start)
}

fn boundary_collision_error() -> WireError {
    WireError::body(
        "multipart payload collided with the selected boundary",
        std::io::Error::other("multipart boundary collision"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http::Method;
    use http_body_util::BodyExt;
    use openwire_core::WireErrorKind;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::task::{Context, Poll, Wake, Waker};

    use http_body::Body;

    fn collect(body: RequestBody) -> Bytes {
        futures_executor_collect(body)
    }

    fn futures_executor_collect(body: RequestBody) -> Bytes {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async { body.collect().await.expect("body").to_bytes() })
    }

    fn collect_result(body: RequestBody) -> Result<Bytes, WireError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async { Ok(body.collect().await?.to_bytes()) })
    }

    fn parse_boundary(content_type: &HeaderValue) -> String {
        let value = content_type.to_str().expect("ascii");
        let prefix = "multipart/form-data; boundary=";
        assert!(value.starts_with(prefix), "content-type = {value}");
        value[prefix.len()..].to_owned()
    }

    fn expected_form(boundary: &str, parts: &[(&str, Option<&str>, &str, &[u8])]) -> Bytes {
        let mut out = Vec::new();
        for (name, file_name, content_type, payload) in parts {
            out.extend_from_slice(DASH_DASH);
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(CRLF);
            out.extend_from_slice(b"Content-Disposition: form-data; name=");
            out.extend_from_slice(quoted_string(name).as_bytes());
            if let Some(file_name) = file_name {
                out.extend_from_slice(b"; filename=");
                out.extend_from_slice(quoted_string(&encode_filename(file_name)).as_bytes());
            }
            out.extend_from_slice(b"\r\nContent-Type: ");
            out.extend_from_slice(content_type.as_bytes());
            out.extend_from_slice(b"\r\n\r\n");
            out.extend_from_slice(payload);
            out.extend_from_slice(CRLF);
        }
        out.extend_from_slice(&closing_delimiter(boundary));
        Bytes::from(out)
    }

    #[test]
    fn encodes_text_and_bytes_with_rfc_shape() {
        let (content_type, body) = Form::new()
            .text("description", "upload from OpenWire")
            .part(
                "file",
                Part::bytes(Bytes::from_static(b"hello\n"))
                    .file_name("hello.txt")
                    .mime_str("text/plain; charset=utf-8")
                    .expect("mime"),
            )
            .into_parts()
            .expect("form");
        let boundary = parse_boundary(&content_type);
        assert_eq!(boundary.len(), 48);
        assert!(body.try_clone().is_some());
        assert_eq!(body.content_length(), Some(body.replayable_len().unwrap()));
        let encoded = collect(body);
        assert_eq!(
            encoded,
            expected_form(
                &boundary,
                &[
                    (
                        "description",
                        None,
                        "text/plain; charset=utf-8",
                        b"upload from OpenWire",
                    ),
                    (
                        "file",
                        Some("hello.txt"),
                        "text/plain; charset=utf-8",
                        b"hello\n",
                    ),
                ],
            )
        );
        assert!(encoded.windows(2).any(|window| window == CRLF));
        let close = closing_delimiter(&boundary);
        assert!(
            encoded.ends_with(close.as_ref()),
            "closing delimiter must be --boundary--CRLF with no padding"
        );
        assert!(
            !encoded.windows(3).any(|window| window == b"-- "),
            "delimiter must not include padding spaces"
        );
    }

    #[test]
    fn keeps_duplicate_fields_in_insertion_order() {
        let (content_type, body) = Form::new()
            .text("tag", "one")
            .text("tag", "two")
            .into_parts()
            .expect("form");
        let boundary = parse_boundary(&content_type);
        assert_eq!(
            collect(body),
            expected_form(
                &boundary,
                &[
                    ("tag", None, "text/plain; charset=utf-8", b"one"),
                    ("tag", None, "text/plain; charset=utf-8", b"two"),
                ],
            )
        );
    }

    #[test]
    fn allows_empty_values_and_zero_byte_files() {
        let (content_type, body) = Form::new()
            .text("empty", "")
            .part("file", Part::bytes(Bytes::new()).file_name("empty.bin"))
            .into_parts()
            .expect("form");
        let boundary = parse_boundary(&content_type);
        assert_eq!(
            collect(body),
            expected_form(
                &boundary,
                &[
                    ("empty", None, "text/plain; charset=utf-8", b""),
                    ("file", Some("empty.bin"), "application/octet-stream", b"",),
                ],
            )
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes_and_encodes_non_ascii_filenames() {
        let (content_type, body) = Form::new()
            .part(
                r#"na"me\z"#,
                Part::bytes(Bytes::from_static(b"x")).file_name("caf%é.txt"),
            )
            .into_parts()
            .expect("form");
        let encoded = collect(body);
        let text = String::from_utf8(encoded.to_vec()).expect("utf8");
        assert!(text.contains(r#"name="na\"me\\z""#), "{text}");
        assert!(text.contains(r#"filename="caf%25%C3%A9.txt""#), "{text}");
        assert!(!text.contains("filename*="), "{text}");
        let _ = content_type;
    }

    #[test]
    fn rejects_control_characters_in_names() {
        let error = Form::new()
            .text("bad\nname", "x")
            .into_parts()
            .expect_err("control");
        assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    }

    #[test]
    fn rejects_empty_form() {
        let error = Form::new().into_parts().expect_err("empty");
        assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    }

    #[test]
    fn rejects_media_ranges_duplicates_nested_multipart_and_charset_conflicts() {
        assert_eq!(
            Part::bytes(Bytes::from_static(b"x"))
                .mime_str("text/*")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            WireErrorKind::InvalidRequest
        );
        assert_eq!(
            Part::bytes(Bytes::from_static(b"x"))
                .mime_str("text/plain; charset=utf-8; charset=utf-8")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            WireErrorKind::InvalidRequest
        );
        assert_eq!(
            Part::bytes(Bytes::from_static(b"x"))
                .mime_str("multipart/mixed; boundary=abc")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            WireErrorKind::InvalidRequest
        );
        assert_eq!(
            Part::text("hi")
                .mime_str("text/plain; charset=iso-8859-1")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            WireErrorKind::InvalidRequest
        );
        let labeled = Part::text("hi")
            .mime_str("text/plain")
            .expect("adds charset");
        let (_, body) = Form::new().part("a", labeled).into_parts().expect("form");
        let encoded = collect(body);
        let text = String::from_utf8(encoded.to_vec()).expect("utf8");
        assert!(
            text.contains("Content-Type: text/plain; charset=utf-8"),
            "{text}"
        );
        assert_eq!(
            Part::text("中文")
                .mime_str("text/html; charset=iso-8859-1")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            WireErrorKind::InvalidRequest
        );
        let quoted = Part::text("hi")
            .mime_str(r#"text/plain; note="a;b""#)
            .expect("quoted param");
        let (_, body) = Form::new().part("a", quoted).into_parts().expect("form");
        let encoded = collect(body);
        let text = String::from_utf8(encoded.to_vec()).expect("utf8");
        assert!(
            text.contains(r#"Content-Type: text/plain; charset=utf-8; note="a;b""#),
            "{text}"
        );
        let content_type = text
            .lines()
            .find(|line| line.starts_with("Content-Type: "))
            .expect("content-type")
            .trim_start_matches("Content-Type: ");
        content_type.parse::<Mime>().expect("round-trip");
        assert!(Part::bytes(Bytes::from_static(b"x"))
            .mime_str(r#"application/octet-stream; x="a;x=b""#)
            .map(|_| ())
            .is_ok());
    }

    #[test]
    fn inline_boundary_bytes_are_not_collisions() {
        let payload = Bytes::from(format!("{}--fixed-boundary", "a".repeat(100)));
        let (content_type, body) = Form {
            fields: vec![FormField {
                name: "file".into(),
                part: Part {
                    payload: PartPayload::Stream {
                        inner: Box::pin(stream::iter(vec![Ok(payload.clone())])),
                        length: None,
                    },
                    file_name: None,
                    mime: None,
                },
            }],
        }
        .into_parts_with_boundary("fixed-boundary")
        .expect("form");
        let encoded = collect(body);
        assert!(encoded
            .windows(payload.len())
            .any(|window| window == payload.as_ref()));
        let _ = content_type;
    }

    async fn encode_chunked_payload(
        chunks: Vec<Bytes>,
        boundary: &str,
    ) -> (Vec<u8>, Option<WireError>) {
        use futures_util::StreamExt;

        let source = stream::iter(chunks).then(|chunk| async move {
            tokio::task::yield_now().await;
            Ok::<_, WireError>(chunk)
        });
        let (_, mut body) = Form::new()
            .part("file", Part::stream(source))
            .into_parts_with_boundary(boundary)
            .expect("form");
        let mut output = Vec::new();
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => output.extend_from_slice(&frame.into_data().expect("DATA")),
                Err(error) => return (output, Some(error)),
            }
        }
        (output, None)
    }

    #[tokio::test]
    async fn boundary_collisions_are_rejected_across_all_three_way_splits() {
        // RFC 2046 §5.1.1: dash-boundary cannot prefix a payload line.
        let boundary = "review-boundary";
        let forbidden = format!("--{boundary}");
        for prefix in ["", "safe\r\n"] {
            let payload = Bytes::from(format!("{prefix}{forbidden}"));
            for first in 1..payload.len() {
                for second in first + 1..payload.len() {
                    let chunks = vec![
                        payload.slice(..first),
                        Bytes::new(),
                        payload.slice(first..second),
                        payload.slice(second..),
                    ];
                    let (wire, error) = encode_chunked_payload(chunks, boundary).await;
                    assert!(error.is_some(), "accepted collision at {first}/{second}");
                    let payload_start = wire
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .expect("part headers")
                        + 4;
                    assert!(
                        !wire[payload_start..]
                            .windows(forbidden.len())
                            .any(|window| window == forbidden.as_bytes()),
                        "emitted complete collision at {first}/{second}"
                    );
                    assert!(!wire.ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()));
                }
            }
            let chunks = payload
                .iter()
                .map(|byte| Bytes::copy_from_slice(&[*byte]))
                .collect();
            assert!(encode_chunked_payload(chunks, boundary).await.1.is_some());
        }
    }

    #[tokio::test]
    async fn safe_stream_payload_is_preserved_across_all_three_way_splits() {
        let boundary = "review-boundary";
        for payload in [
            Bytes::from_static(b"inline--review-boundary"),
            Bytes::from_static(b"safe\r\n--review-boundarX\r"),
            Bytes::from_static(b"\0a\r\nb\rc\nd\xff"),
        ] {
            for first in 1..payload.len() {
                for second in first + 1..payload.len() {
                    let chunks = vec![
                        payload.slice(..first),
                        payload.slice(first..second),
                        payload.slice(second..),
                    ];
                    let (wire, error) = encode_chunked_payload(chunks, boundary).await;
                    assert!(error.is_none(), "rejected safe payload at {first}/{second}");
                    let mut expected = format!(
                        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                    ).into_bytes();
                    expected.extend_from_slice(&payload);
                    expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                    assert_eq!(wire, expected, "changed payload at {first}/{second}");
                }
            }
        }
    }

    #[test]
    fn mime_parameters_support_quoted_pairs_and_reject_incomplete_syntax() {
        // RFC 2045 §5.1 and RFC 9110 §5.6.4: quoted-pair is part of quoted-string.
        for (input, expected) in [
            (
                r#"text/plain; note="a\"b""#,
                r#"text/plain; charset=utf-8; note="a\"b""#,
            ),
            (
                r#"text/plain; note="a\\b""#,
                r#"text/plain; charset=utf-8; note="a\\b""#,
            ),
            (
                r#"text/plain; note="""#,
                r#"text/plain; charset=utf-8; note="""#,
            ),
            (
                r#"text/plain; note="a\;b""#,
                r#"text/plain; charset=utf-8; note="a;b""#,
            ),
        ] {
            let part = Part::text("hello")
                .mime_str(input)
                .expect("valid MIME parameter");
            assert_eq!(part.mime.expect("media type").serialized, expected);
        }
        for input in [
            "text/plain; note=",
            "text/plain; note=\"unterminated",
            "text/plain; note=\"trailing\\",
            "text/plain; note=\"ok\"extra=x",
            "text/plain; note=\"ok\"other=\"value\"",
            "text/plain; note =value",
            "text/plain; note= value",
            "text/plain; Note=one; note=two",
        ] {
            assert!(
                Part::text("hello").mime_str(input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn boundary_guard_slices_large_safe_chunks_and_retains_only_candidate_bytes() {
        let needle = dash_boundary("review-boundary");
        let mut hold = b"--".to_vec();
        let mut ready = None;
        let mut at_line_start = true;
        let chunk = Bytes::from(vec![b'a'; 4 * 1024 * 1024]);
        let original_pointer = chunk.as_ptr();
        let released = split_safe_chunk(chunk, &mut hold, &mut ready, &needle, &mut at_line_start)
            .expect("safe payload")
            .expect("released prefix");
        assert_eq!(released.as_ref(), b"--");
        let safe_chunk = ready.expect("zero-copy safe chunk");
        assert_eq!(safe_chunk.as_ptr(), original_pointer);
        assert_eq!(safe_chunk.len(), 4 * 1024 * 1024);
        assert!(hold.is_empty());
        assert!(hold.capacity() < needle.len());
    }

    #[test]
    fn into_request_replaces_content_type_and_clears_framing() {
        let request = Form::new()
            .text("a", "b")
            .into_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("https://example.com/upload")
                    .header(CONTENT_TYPE, "application/json")
                    .header(CONTENT_LENGTH, "999")
                    .header(TRANSFER_ENCODING, "chunked"),
            )
            .expect("request");
        let values: Vec<_> = request
            .headers()
            .get_all(CONTENT_TYPE)
            .iter()
            .cloned()
            .collect();
        assert_eq!(values.len(), 1);
        assert!(values[0]
            .to_str()
            .expect("ascii")
            .starts_with("multipart/form-data; boundary="));
        assert!(request.headers().get(CONTENT_LENGTH).is_none());
        assert!(request.headers().get(TRANSFER_ENCODING).is_none());
    }

    #[test]
    fn into_request_rejects_content_encoding() {
        let error = Form::new()
            .text("a", "b")
            .into_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("https://example.com/upload")
                    .header(CONTENT_ENCODING, "gzip"),
            )
            .expect_err("encoding");
        assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
    }

    #[test]
    fn stream_part_is_not_replayable_and_can_declare_length() {
        let (_, body) = Form::new()
            .part(
                "file",
                Part::stream_with_length(
                    stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"xyz"))]),
                    3,
                ),
            )
            .into_parts()
            .expect("form");
        assert!(body.try_clone().is_none());
        assert!(body.content_length().is_some());
        assert_eq!(body.replayable_len(), None);
        let encoded = collect(body);
        assert!(encoded.windows(3).any(|window| window == b"xyz"));
        assert!(encoded.ends_with(b"--\r\n") || encoded.windows(4).any(|w| w == b"--\r\n"));
    }

    #[test]
    fn stream_collision_is_rejected_before_complete_prefix() {
        let collision = b"--collide-boundary";
        let (_, body) = Form {
            fields: vec![FormField {
                name: "file".into(),
                part: Part {
                    payload: PartPayload::Stream {
                        inner: Box::pin(stream::iter(vec![
                            Ok(Bytes::from_static(b"ok\r\n")),
                            Ok(Bytes::from_static(collision)),
                        ])),
                        length: None,
                    },
                    file_name: None,
                    mime: None,
                },
            }],
        }
        .into_parts_with_boundary("collide-boundary")
        .expect("form");
        let error = collect_result(body).expect_err("collision");
        assert_eq!(error.kind(), WireErrorKind::Body);
    }

    #[test]
    fn payload_crlf_bytes_are_preserved() {
        let (content_type, body) = Form::new()
            .part("file", Part::bytes(Bytes::from_static(b"a\r\nb\nc")))
            .into_parts()
            .expect("form");
        let boundary = parse_boundary(&content_type);
        let encoded = collect(body);
        assert_eq!(
            encoded,
            expected_form(
                &boundary,
                &[("file", None, "application/octet-stream", b"a\r\nb\nc",)],
            )
        );
    }

    #[test]
    fn memory_payload_matching_boundary_selects_another_candidate() {
        let payload = Bytes::from_static(b"placeholder");
        let (content_type, body) = Form::new()
            .part("file", Part::bytes(payload.clone()))
            .into_parts()
            .expect("form");
        let boundary = parse_boundary(&content_type);
        let encoded = collect(body);
        assert!(!contains_dash_boundary(
            payload.as_ref(),
            &dash_boundary(&boundary)
        ));
        assert!(encoded
            .windows(payload.len())
            .any(|window| window == payload.as_ref()));
    }

    #[test]
    fn prefix_only_match_is_held_across_chunks() {
        let reads = Arc::new(Mutex::new(0usize));
        let stream_reads = reads.clone();
        let stream = futures_util::stream::unfold(0u8, move |step| {
            let stream_reads = stream_reads.clone();
            async move {
                *stream_reads.lock().expect("reads") += 1;
                match step {
                    0 => Some((Ok(Bytes::from_static(b"--coll")), 1)),
                    1 => Some((Ok(Bytes::from_static(b"ide-boundary")), 2)),
                    _ => None,
                }
            }
        });
        let (_, body) = Form {
            fields: vec![FormField {
                name: "file".into(),
                part: Part {
                    payload: PartPayload::Stream {
                        inner: Box::pin(stream),
                        length: None,
                    },
                    file_name: None,
                    mime: None,
                },
            }],
        }
        .into_parts_with_boundary("collide-boundary")
        .expect("form");
        let error = collect_result(body).expect_err("collision");
        assert_eq!(error.kind(), WireErrorKind::Body);
        assert!(*reads.lock().expect("reads") >= 2);
    }

    struct PendingOnce<S> {
        pending: bool,
        inner: S,
    }

    impl<S: Stream<Item = Result<Bytes, WireError>> + Unpin> Stream for PendingOnce<S> {
        type Item = Result<Bytes, WireError>;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.pending {
                this.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_next(cx)
        }
    }

    #[test]
    fn pending_poll_does_not_drop_prefix_state() {
        let stream = PendingOnce {
            pending: true,
            inner: stream::iter(vec![
                Ok(Bytes::from_static(b"--x")),
                Ok(Bytes::from_static(b"safe")),
            ]),
        };
        let (_, body) = Form {
            fields: vec![FormField {
                name: "file".into(),
                part: Part {
                    payload: PartPayload::Stream {
                        inner: Box::pin(stream),
                        length: None,
                    },
                    file_name: None,
                    mime: None,
                },
            }],
        }
        .into_parts_with_boundary("zzzzzzzz")
        .expect("form");
        let encoded = collect(body);
        assert!(
            encoded.windows(6).any(|window| window == b"--xsafe")
                || encoded.windows(4).any(|window| window == b"safe")
        );
    }

    struct EmptyForever;

    impl Stream for EmptyForever {
        type Item = Result<Bytes, WireError>;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(Some(Ok(Bytes::new())))
        }
    }

    #[test]
    fn empty_chunks_yield_pending_instead_of_spinning() {
        let (_, mut body) = Form {
            fields: vec![FormField {
                name: "file".into(),
                part: Part {
                    payload: PartPayload::Stream {
                        inner: Box::pin(EmptyForever),
                        length: None,
                    },
                    file_name: None,
                    mime: None,
                },
            }],
        }
        .into_parts_with_boundary("zzzzzzzz")
        .expect("form");
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        static WAKER: OnceLock<Waker> = OnceLock::new();
        let waker = WAKER.get_or_init(|| Waker::from(Arc::new(Noop)));
        let mut cx = Context::from_waker(waker);
        let mut polls = 0usize;
        for _ in 0..32 {
            polls += 1;
            match Pin::new(&mut body).poll_frame(&mut cx) {
                Poll::Pending => break,
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(_) => break,
            }
        }
        assert!(polls < 32, "empty chunks must not monopolize the executor");
    }
}
