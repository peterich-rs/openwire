use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::futures::bufread::{
    BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder,
};
use bytes::Bytes;
use futures_util::io::{AsyncBufRead, AsyncRead, BufReader};
use futures_util::TryStreamExt;
use http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, RANGE};
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use openwire_core::{RequestBody, ResponseBody, WireError};
use pin_project_lite::pin_project;

const ACCEPTED_ENCODINGS: HeaderValue = HeaderValue::from_static("br, gzip, deflate, zstd");
const DECODE_BUFFER_SIZE: usize = 8 * 1024;

/// Default cap on transparent response decompression output.
///
/// Prevents classic compression-bomb denial-of-service against clients that
/// automatically decode `Content-Encoding`.
pub const DEFAULT_MAX_DECOMPRESSED_BODY_BYTES: usize = 128 * 1024 * 1024;

type BoxAsyncBufRead = Pin<Box<dyn AsyncBufRead + Send + Sync>>;
type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Sync>>;

pub(crate) fn normalize_request(request: &mut http::Request<RequestBody>) -> bool {
    if should_skip_transparent_compression(request) {
        return false;
    }

    request
        .headers_mut()
        .insert(ACCEPT_ENCODING, ACCEPTED_ENCODINGS.clone());
    true
}

pub(crate) fn decode_response(
    response: Response<ResponseBody>,
    request_method: &Method,
    max_decompressed_body_bytes: usize,
) -> Response<ResponseBody> {
    if !response_can_have_body(request_method, response.status()) {
        return response;
    }

    let Some(encodings) = supported_content_encodings(response.headers()) else {
        return response;
    };
    if encodings.is_empty() {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.remove(CONTENT_LENGTH);
    let label = encodings
        .iter()
        .map(|encoding| encoding.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let body = DecodedResponseBody::new(body, encodings, label, max_decompressed_body_bytes);
    Response::from_parts(parts, ResponseBody::new(body.boxed()))
}

fn should_skip_transparent_compression(request: &http::Request<RequestBody>) -> bool {
    #[cfg(feature = "websocket")]
    {
        if request
            .extensions()
            .get::<crate::websocket::handshake::WebSocketRequestMarker>()
            .is_some()
        {
            return true;
        }
    }

    request.headers().contains_key(ACCEPT_ENCODING) || request.headers().contains_key(RANGE)
}

fn supported_content_encodings(headers: &HeaderMap) -> Option<Vec<ResponseEncoding>> {
    let mut encodings = Vec::new();
    for value in headers.get_all(CONTENT_ENCODING) {
        let value = value.to_str().ok()?;
        for part in value.split(',') {
            let normalized = part.trim();
            if normalized.is_empty() {
                continue;
            }
            if normalized.eq_ignore_ascii_case("identity") {
                continue;
            }
            encodings.push(ResponseEncoding::parse(normalized)?);
        }
    }
    Some(encodings)
}

fn response_can_have_body(method: &Method, status: StatusCode) -> bool {
    if *method == Method::HEAD {
        return false;
    }

    !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
        && status != StatusCode::SWITCHING_PROTOCOLS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseEncoding {
    Brotli,
    Gzip,
    Deflate,
    Zstd,
}

impl ResponseEncoding {
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("br") {
            Some(Self::Brotli)
        } else if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
            Some(Self::Gzip)
        } else if value.eq_ignore_ascii_case("deflate") {
            Some(Self::Deflate)
        } else if value.eq_ignore_ascii_case("zstd") {
            Some(Self::Zstd)
        } else {
            None
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Zstd => "zstd",
        }
    }
}

pin_project! {
    struct DecodedResponseBody {
        #[pin]
        reader: BoxAsyncBufRead,
        label: String,
        max_decompressed_body_bytes: usize,
        decoded_bytes: usize,
    }
}

impl DecodedResponseBody {
    fn new(
        body: ResponseBody,
        encodings: Vec<ResponseEncoding>,
        label: String,
        max_decompressed_body_bytes: usize,
    ) -> Self {
        let stream = body.into_data_stream().map_err(wire_error_to_io);
        let reader = stream.into_async_read();
        let mut reader: BoxAsyncBufRead = Box::pin(reader);

        for encoding in encodings.into_iter().rev() {
            reader = decode_layer(reader, encoding);
        }

        Self {
            reader,
            label,
            max_decompressed_body_bytes,
            decoded_bytes: 0,
        }
    }
}

impl Body for DecodedResponseBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let mut buffer = [0; DECODE_BUFFER_SIZE];
        match this.reader.poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(0)) => Poll::Ready(None),
            Poll::Ready(Ok(read)) => {
                let Some(total) = this.decoded_bytes.checked_add(read) else {
                    return Poll::Ready(Some(Err(WireError::body(
                        format!(
                            "decompressed {} response exceeded size limit {}",
                            this.label, this.max_decompressed_body_bytes
                        ),
                        io::Error::new(io::ErrorKind::InvalidData, "decompressed body too large"),
                    ))));
                };
                if total > *this.max_decompressed_body_bytes {
                    return Poll::Ready(Some(Err(WireError::body(
                        format!(
                            "decompressed {} response exceeded size limit {}",
                            this.label, this.max_decompressed_body_bytes
                        ),
                        io::Error::new(io::ErrorKind::InvalidData, "decompressed body too large"),
                    ))));
                }
                *this.decoded_bytes = total;
                Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(
                    &buffer[..read],
                )))))
            }
            Poll::Ready(Err(error)) => {
                Poll::Ready(Some(Err(io_error_to_wire(error, this.label.as_str()))))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

fn decode_layer(reader: BoxAsyncBufRead, encoding: ResponseEncoding) -> BoxAsyncBufRead {
    match encoding {
        ResponseEncoding::Brotli => {
            let decoded: BoxAsyncRead = Box::pin(BrotliDecoder::new(reader));
            Box::pin(BufReader::new(decoded))
        }
        ResponseEncoding::Gzip => {
            let decoded: BoxAsyncRead = Box::pin(GzipDecoder::new(reader));
            Box::pin(BufReader::new(decoded))
        }
        ResponseEncoding::Deflate => {
            // HTTP "deflate" is ambiguous (zlib-wrapped vs raw). Detect via the
            // first two bytes (RFC 1950 CMF/FLG check) before wrapping.
            Box::pin(BufReader::new(DeflateAutoDecoder::new(reader)))
        }
        ResponseEncoding::Zstd => {
            let decoded: BoxAsyncRead = Box::pin(ZstdDecoder::new(reader));
            Box::pin(BufReader::new(decoded))
        }
    }
}

/// Chooses zlib-wrapped vs raw DEFLATE after buffering a two-byte header peek.
struct DeflateAutoDecoder {
    state: DeflateAutoState,
}

enum DeflateAutoState {
    /// Accumulating the first two payload bytes.
    Peeking {
        reader: BoxAsyncBufRead,
        peeked: [u8; 2],
        peeked_len: usize,
    },
    /// Decoder selected; remaining stream is already wired with the prefix.
    Decoding {
        reader: BoxAsyncBufRead,
    },
}

impl DeflateAutoDecoder {
    fn new(reader: BoxAsyncBufRead) -> Self {
        Self {
            state: DeflateAutoState::Peeking {
                reader,
                peeked: [0; 2],
                peeked_len: 0,
            },
        }
    }

    fn finish_peek(
        reader: BoxAsyncBufRead,
        peeked: [u8; 2],
        peeked_len: usize,
    ) -> BoxAsyncBufRead {
        let use_zlib = looks_like_zlib_header(&peeked[..peeked_len]);
        let prefixed: BoxAsyncBufRead = Box::pin(PrefixedReader {
            prefix: peeked,
            prefix_len: peeked_len,
            prefix_pos: 0,
            inner: reader,
        });
        let decoded: BoxAsyncRead = if use_zlib {
            Box::pin(ZlibDecoder::new(prefixed))
        } else {
            Box::pin(DeflateDecoder::new(prefixed))
        };
        Box::pin(BufReader::new(decoded))
    }
}

impl AsyncRead for DeflateAutoDecoder {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.state {
                DeflateAutoState::Peeking {
                    reader,
                    peeked,
                    peeked_len,
                } => {
                    while *peeked_len < 2 {
                        let mut tmp = [0u8; 1];
                        match Pin::new(&mut *reader).poll_read(cx, &mut tmp) {
                            Poll::Ready(Ok(0)) => break,
                            Poll::Ready(Ok(1)) => {
                                peeked[*peeked_len] = tmp[0];
                                *peeked_len += 1;
                            }
                            Poll::Ready(Ok(_)) => unreachable!("tmp is 1 byte"),
                            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                            Poll::Pending => return Poll::Pending,
                        }
                    }

                    let DeflateAutoState::Peeking {
                        reader,
                        peeked,
                        peeked_len,
                    } = std::mem::replace(
                        &mut self.state,
                        DeflateAutoState::Decoding {
                            // Temporary placeholder replaced immediately below.
                            reader: Box::pin(BufReader::new(futures_util::io::empty())),
                        },
                    )
                    else {
                        unreachable!("just matched Peeking");
                    };
                    self.state = DeflateAutoState::Decoding {
                        reader: Self::finish_peek(reader, peeked, peeked_len),
                    };
                }
                DeflateAutoState::Decoding { reader } => {
                    return Pin::new(reader).poll_read(cx, buf);
                }
            }
        }
    }
}

/// Replays a short peeked prefix before reading the remainder of the stream.
struct PrefixedReader {
    prefix: [u8; 2],
    prefix_len: usize,
    prefix_pos: usize,
    inner: BoxAsyncBufRead,
}

impl AsyncRead for PrefixedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.prefix_pos < self.prefix_len {
            let available = self.prefix_len - self.prefix_pos;
            let copy = available.min(buf.len());
            buf[..copy]
                .copy_from_slice(&self.prefix[self.prefix_pos..self.prefix_pos + copy]);
            self.prefix_pos += copy;
            return Poll::Ready(Ok(copy));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncBufRead for PrefixedReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.prefix_pos < this.prefix_len {
            return Poll::Ready(Ok(&this.prefix[this.prefix_pos..this.prefix_len]));
        }
        Pin::new(&mut this.inner).poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        if self.prefix_pos < self.prefix_len {
            let available = self.prefix_len - self.prefix_pos;
            let take = amt.min(available);
            self.prefix_pos += take;
            let remaining = amt - take;
            if remaining > 0 {
                Pin::new(&mut self.inner).consume(remaining);
            }
            return;
        }
        Pin::new(&mut self.inner).consume(amt);
    }
}

fn looks_like_zlib_header(header: &[u8]) -> bool {
    if header.len() < 2 {
        // Not enough data; zlib is the historical HTTP default.
        return true;
    }
    let cmf = header[0];
    let flg = header[1];
    // CM must be 8 (DEFLATE) and CMF/FLG must be multiple of 31 (RFC 1950).
    (cmf & 0x0f) == 8 && (u16::from(cmf) * 256 + u16::from(flg)) % 31 == 0
}

fn wire_error_to_io(error: WireError) -> io::Error {
    io::Error::other(error)
}

fn io_error_to_wire(error: io::Error, label: &str) -> WireError {
    if let Some(wire_error) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<WireError>())
    {
        return wire_error.clone();
    }

    WireError::body(format!("failed to decode {label} response body"), error)
}

#[cfg(test)]
mod tests {
    use http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, RANGE};
    use http::{Method, Request, Response};

    use super::{
        decode_response, looks_like_zlib_header, normalize_request, ACCEPTED_ENCODINGS,
        DEFAULT_MAX_DECOMPRESSED_BODY_BYTES,
    };
    use crate::{RequestBody, ResponseBody};

    #[test]
    fn normalize_request_injects_default_accept_encoding() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .body(RequestBody::empty())
            .expect("request");

        assert!(normalize_request(&mut request));
        assert_eq!(
            request.headers().get(ACCEPT_ENCODING),
            Some(&ACCEPTED_ENCODINGS)
        );
    }

    #[test]
    fn normalize_request_preserves_explicit_accept_encoding() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .header(ACCEPT_ENCODING, "identity")
            .body(RequestBody::empty())
            .expect("request");

        assert!(!normalize_request(&mut request));
        assert_eq!(request.headers().get(ACCEPT_ENCODING).unwrap(), "identity");
    }

    #[test]
    fn normalize_request_skips_ranges() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .header(RANGE, "bytes=0-99")
            .body(RequestBody::empty())
            .expect("request");

        assert!(!normalize_request(&mut request));
        assert!(request.headers().get(ACCEPT_ENCODING).is_none());
    }

    #[test]
    fn decode_response_cleans_transparent_encoding_headers() {
        let response = Response::builder()
            .header(CONTENT_ENCODING, "gzip")
            .header(CONTENT_LENGTH, "20")
            .body(ResponseBody::empty())
            .expect("response");

        let response =
            decode_response(response, &Method::GET, DEFAULT_MAX_DECOMPRESSED_BODY_BYTES);

        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        assert!(response.headers().get(CONTENT_LENGTH).is_none());
    }

    #[test]
    fn decode_response_leaves_unknown_encoding_untouched() {
        let response = Response::builder()
            .header(CONTENT_ENCODING, "made-up")
            .header(CONTENT_LENGTH, "20")
            .body(ResponseBody::empty())
            .expect("response");

        let response =
            decode_response(response, &Method::GET, DEFAULT_MAX_DECOMPRESSED_BODY_BYTES);

        assert_eq!(response.headers().get(CONTENT_ENCODING).unwrap(), "made-up");
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "20");
    }

    #[test]
    fn zlib_header_detection_accepts_rfc1950_header() {
        // CMF=0x78, FLG=0x9c is the common default zlib header.
        assert!(looks_like_zlib_header(&[0x78, 0x9c]));
        assert!(!looks_like_zlib_header(&[0x01, 0x02]));
    }
}
