use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::futures::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
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
    let body = DecodedResponseBody::new(body, encodings, label);
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
    }
}

impl DecodedResponseBody {
    fn new(body: ResponseBody, encodings: Vec<ResponseEncoding>, label: String) -> Self {
        let stream = body.into_data_stream().map_err(wire_error_to_io);
        let reader = stream.into_async_read();
        let mut reader: BoxAsyncBufRead = Box::pin(reader);

        for encoding in encodings.into_iter().rev() {
            reader = decode_layer(reader, encoding);
        }

        Self { reader, label }
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
            Poll::Ready(Ok(read)) => Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(
                &buffer[..read],
            ))))),
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
            let decoded: BoxAsyncRead = Box::pin(ZlibDecoder::new(reader));
            Box::pin(BufReader::new(decoded))
        }
        ResponseEncoding::Zstd => {
            let decoded: BoxAsyncRead = Box::pin(ZstdDecoder::new(reader));
            Box::pin(BufReader::new(decoded))
        }
    }
}

fn wire_error_to_io(error: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error)
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

    use super::{decode_response, normalize_request, ACCEPTED_ENCODINGS};
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

        let response = decode_response(response, &Method::GET);

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

        let response = decode_response(response, &Method::GET);

        assert_eq!(response.headers().get(CONTENT_ENCODING).unwrap(), "made-up");
        assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "20");
    }
}
