use http::header::{CONTENT_LENGTH, HOST, TRANSFER_ENCODING, USER_AGENT};
use http::{HeaderValue, Request, Uri, Version};
use openwire_core::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};

const DEFAULT_USER_AGENT: &str = concat!("openwire/", env!("CARGO_PKG_VERSION"));
const CHUNKED: HeaderValue = HeaderValue::from_static("chunked");
const DEFAULT_USER_AGENT_VALUE: HeaderValue = HeaderValue::from_static(DEFAULT_USER_AGENT);

#[derive(Clone, Debug)]
pub(crate) struct BridgeInterceptor {
    #[cfg(feature = "compression")]
    max_decompressed_body_bytes: usize,
    strict_host_header: bool,
}

impl BridgeInterceptor {
    pub(crate) fn new(
        #[cfg(feature = "compression")] max_decompressed_body_bytes: usize,
        strict_host_header: bool,
    ) -> Self {
        Self {
            #[cfg(feature = "compression")]
            max_decompressed_body_bytes,
            strict_host_header,
        }
    }
}

impl Interceptor for BridgeInterceptor {
    fn intercept(
        &self,
        mut exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<http::Response<ResponseBody>, WireError>> {
        #[cfg(feature = "compression")]
        let request_method = exchange.request().method().clone();
        #[cfg(feature = "compression")]
        let max_decompressed_body_bytes = self.max_decompressed_body_bytes;
        let strict_host_header = self.strict_host_header;
        let normalization = normalize_request(exchange.request_mut(), strict_host_header);
        #[cfg(feature = "compression")]
        let transparent_compression = normalization.as_ref().copied().unwrap_or(false);
        Box::pin(async move {
            normalization?;
            let response = next.run(exchange).await?;
            #[cfg(feature = "compression")]
            {
                if transparent_compression {
                    return Ok(crate::compression::decode_response(
                        response,
                        &request_method,
                        max_decompressed_body_bytes,
                    ));
                }
            }
            Ok(response)
        })
    }
}

pub(crate) fn normalize_request(
    request: &mut Request<RequestBody>,
    strict_host_header: bool,
) -> Result<bool, WireError> {
    #[cfg(feature = "websocket")]
    {
        if request
            .extensions()
            .get::<crate::websocket::handshake::WebSocketRequestMarker>()
            .is_some()
        {
            crate::websocket::handshake::inject_handshake(request)?;
        }
    }
    normalize_host_header(request, strict_host_header)?;
    normalize_user_agent_header(request);
    normalize_body_headers(request);
    #[cfg(feature = "compression")]
    let transparent_compression = crate::compression::normalize_request(request);
    #[cfg(not(feature = "compression"))]
    let transparent_compression = false;
    Ok(transparent_compression)
}

fn normalize_host_header(
    request: &mut Request<RequestBody>,
    strict_host_header: bool,
) -> Result<(), WireError> {
    let expected = synthesized_host_header(request.uri())?;
    if let Some(existing) = request.headers().get(HOST) {
        if strict_host_header {
            let existing = existing.to_str().map_err(|error| {
                WireError::invalid_request(format!("Host header is not valid ASCII: {error}"))
            })?;
            if !existing.eq_ignore_ascii_case(
                expected
                    .to_str()
                    .map_err(|error| {
                        WireError::invalid_request(format!(
                            "synthesized Host header is not valid ASCII: {error}"
                        ))
                    })?,
            ) {
                return Err(WireError::invalid_request(format!(
                    "Host header {existing:?} does not match request URI authority"
                )));
            }
        }
        return Ok(());
    }

    request.headers_mut().insert(HOST, expected);
    Ok(())
}

fn synthesized_host_header(uri: &Uri) -> Result<HeaderValue, WireError> {
    let authority = uri
        .authority()
        .ok_or_else(|| WireError::invalid_request("request URI is missing an authority"))?;
    let host = uri
        .host()
        .ok_or_else(|| WireError::invalid_request("request URI is missing a host"))?;

    let authority = match uri.port_u16() {
        Some(port) if !is_default_port(uri.scheme_str(), port) => authority.as_str(),
        _ => host,
    };

    HeaderValue::from_str(authority).map_err(|error| {
        WireError::invalid_request(format!(
            "request URI authority is not a valid Host header: {error}"
        ))
    })
}

fn is_default_port(scheme: Option<&str>, port: u16) -> bool {
    matches!(scheme, Some(scheme) if (scheme.eq_ignore_ascii_case("http") && port == 80)
        || (scheme.eq_ignore_ascii_case("https") && port == 443))
}

fn normalize_user_agent_header(request: &mut Request<RequestBody>) {
    if !request.headers().contains_key(USER_AGENT) {
        request
            .headers_mut()
            .insert(USER_AGENT, DEFAULT_USER_AGENT_VALUE.clone());
    }
}

fn normalize_body_headers(request: &mut Request<RequestBody>) {
    let version = request.version();
    let body_is_absent = request.body().is_absent();
    let replayable_len = request.body().replayable_len();
    let headers = request.headers_mut();

    if body_is_absent {
        headers.remove(CONTENT_LENGTH);
        headers.remove(TRANSFER_ENCODING);
        return;
    }

    match replayable_len {
        Some(len) => {
            let value = HeaderValue::from_str(&len.to_string())
                .expect("numeric content-length is always a valid header value");
            headers.insert(CONTENT_LENGTH, value);
            headers.remove(TRANSFER_ENCODING);
        }
        None => {
            headers.remove(CONTENT_LENGTH);
            if uses_chunked_transfer_encoding(version) {
                headers.insert(TRANSFER_ENCODING, CHUNKED.clone());
            } else {
                headers.remove(TRANSFER_ENCODING);
            }
        }
    }
}

fn uses_chunked_transfer_encoding(version: Version) -> bool {
    version == Version::HTTP_11
}

#[cfg(test)]
mod tests {
    use http::header::{CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
    use http::{Method, Request, Version};

    use super::{normalize_body_headers, normalize_request};
    use crate::RequestBody;

    #[test]
    fn synthesized_host_omits_default_http_port() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com:80/")
            .body(RequestBody::empty())
            .expect("request");

        normalize_request(&mut request, false).expect("normalize");

        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("example.com")
        );
    }

    #[test]
    fn synthesized_host_omits_default_https_port() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com:443/")
            .body(RequestBody::empty())
            .expect("request");

        normalize_request(&mut request, false).expect("normalize");

        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("example.com")
        );
    }

    #[test]
    fn synthesized_host_preserves_non_default_port() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com:8443/")
            .body(RequestBody::empty())
            .expect("request");

        normalize_request(&mut request, false).expect("normalize");

        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("example.com:8443")
        );
    }

    #[test]
    fn synthesized_host_preserves_ipv6_brackets_while_omitting_default_port() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://[::1]:80/")
            .body(RequestBody::empty())
            .expect("request");

        normalize_request(&mut request, false).expect("normalize");

        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("[::1]")
        );
    }

    #[test]
    fn explicit_host_header_is_preserved() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com:443/")
            .header(HOST, "custom.example")
            .body(RequestBody::empty())
            .expect("request");

        normalize_request(&mut request, false).expect("normalize");

        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("custom.example")
        );
    }

    #[test]
    fn absent_body_omits_content_length() {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .body(RequestBody::empty())
            .expect("request");

        normalize_body_headers(&mut request);

        assert!(request.headers().get(CONTENT_LENGTH).is_none());
        assert!(request.headers().get(TRANSFER_ENCODING).is_none());
    }

    #[test]
    fn explicit_empty_body_sets_content_length_zero() {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("http://example.com/")
            .body(RequestBody::explicit_empty())
            .expect("request");

        normalize_body_headers(&mut request);

        assert_eq!(
            request
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some("0")
        );
        assert!(request.headers().get(TRANSFER_ENCODING).is_none());
    }

    #[cfg(feature = "websocket")]
    #[test]
    fn injects_websocket_handshake_headers() {
        use crate::websocket::handshake::WebSocketRequestMarker;
        use http::Method;
        use openwire_core::TlsAlpnPreference;

        let mut request: Request<crate::RequestBody> = Request::builder()
            .method(Method::GET)
            .uri("ws://example.com/socket")
            .body(crate::RequestBody::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(WebSocketRequestMarker::new(vec!["chat".into()]));

        super::normalize_request(&mut request, false).expect("normalize");

        let headers = request.headers();
        assert_eq!(headers.get("upgrade").unwrap(), "websocket");
        assert!(headers
            .get("connection")
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("upgrade"));
        assert_eq!(headers.get("sec-websocket-version").unwrap(), "13");
        assert!(headers.get("sec-websocket-key").is_some());
        assert_eq!(headers.get("sec-websocket-protocol").unwrap(), "chat");
        assert_eq!(request.uri().scheme_str(), Some("http"));
        assert_eq!(request.version(), Version::HTTP_11);
        let marker = request
            .extensions()
            .get::<WebSocketRequestMarker>()
            .expect("marker preserved");
        assert!(!marker.expected_accept.is_empty());
        assert_eq!(
            request.extensions().get::<TlsAlpnPreference>(),
            Some(&TlsAlpnPreference::Http1Only)
        );
    }

    #[cfg(feature = "websocket")]
    #[test]
    fn rejects_websocket_request_with_non_get_method() {
        use crate::websocket::handshake::WebSocketRequestMarker;
        use http::Method;

        let mut request: Request<crate::RequestBody> = Request::builder()
            .method(Method::POST)
            .uri("ws://example.com/socket")
            .body(crate::RequestBody::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(WebSocketRequestMarker::new(Vec::new()));

        let err = super::normalize_request(&mut request, false).expect_err("must reject");
        assert_eq!(err.kind(), crate::WireErrorKind::InvalidRequest);
    }

    #[test]
    fn http11_streaming_body_uses_chunked_transfer_encoding() {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("http://example.com/")
            .version(Version::HTTP_11)
            .body(RequestBody::from_stream(futures_util::stream::empty::<
                Result<bytes::Bytes, crate::WireError>,
            >()))
            .expect("request");

        normalize_body_headers(&mut request);

        assert!(request.headers().get(CONTENT_LENGTH).is_none());
        assert_eq!(
            request
                .headers()
                .get(TRANSFER_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("chunked")
        );
    }
}
