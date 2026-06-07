use std::collections::HashSet;

use base64::Engine;
use http::header::{CONNECTION, UPGRADE};
use http::{HeaderMap, HeaderValue, Request, Uri, Version};
use sha1::{Digest, Sha1};

use openwire_core::websocket::HandshakeFailure;
use openwire_core::{RequestBody, TlsAlpnPreference, WireError};

use crate::connection::RoutePreference;

const HANDSHAKE_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Marker installed on `Request::extensions` by `Client::new_websocket`. The
/// bridge reads it to inject handshake headers; the transport branch reads
/// `expected_accept` to validate the 101 response.
#[derive(Clone, Debug)]
pub(crate) struct WebSocketRequestMarker {
    pub subprotocols: Vec<String>,
    pub expected_accept: String,
}

impl WebSocketRequestMarker {
    pub(crate) fn new(subprotocols: Vec<String>) -> Self {
        Self {
            subprotocols,
            expected_accept: String::new(),
        }
    }
}

pub(crate) fn derive_accept(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(HANDSHAKE_MAGIC.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

pub(crate) fn generate_client_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn inject_handshake(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    let subprotocols = &request
        .extensions()
        .get::<WebSocketRequestMarker>()
        .expect("WebSocketRequestMarker must be present when inject_handshake runs")
        .subprotocols;
    validate_subprotocols(subprotocols)?;

    request_must_be_get(request)?;
    rewrite_scheme(request)?;
    request
        .headers_mut()
        .insert(UPGRADE, HeaderValue::from_static("websocket"));
    request
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    request
        .headers_mut()
        .insert("sec-websocket-version", HeaderValue::from_static("13"));

    let key = generate_client_key();
    let accept = derive_accept(&key);
    request.headers_mut().insert(
        "sec-websocket-key",
        HeaderValue::from_str(&key)
            .map_err(|error| WireError::invalid_request(error.to_string()))?,
    );

    let marker = request
        .extensions_mut()
        .get_mut::<WebSocketRequestMarker>()
        .expect("WebSocketRequestMarker must be present when inject_handshake runs");
    marker.expected_accept = accept;

    if !marker.subprotocols.is_empty() {
        let value = marker.subprotocols.join(", ");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&value)
                .map_err(|error| WireError::invalid_request(error.to_string()))?,
        );
    }

    *request.version_mut() = Version::HTTP_11;
    request
        .extensions_mut()
        .insert(TlsAlpnPreference::Http1Only);
    request.extensions_mut().insert(RoutePreference::Http1Only);

    Ok(())
}

fn validate_subprotocols(protocols: &[String]) -> Result<(), WireError> {
    let mut seen = HashSet::new();
    for protocol in protocols {
        if !is_subprotocol_token(protocol) {
            return Err(WireError::invalid_request(format!(
                "WebSocket subprotocol must be a non-empty RFC 6455 token: {protocol:?}"
            )));
        }
        if !seen.insert(protocol.as_str()) {
            return Err(WireError::invalid_request(format!(
                "WebSocket subprotocol appears more than once: {protocol:?}"
            )));
        }
    }
    Ok(())
}

fn is_subprotocol_token(protocol: &str) -> bool {
    !protocol.is_empty() && protocol.bytes().all(is_http_token_byte)
}

fn is_http_token_byte(byte: u8) -> bool {
    matches!(byte, 0x21..=0x7e)
        && !matches!(
            byte,
            b'(' | b')'
                | b'<'
                | b'>'
                | b'@'
                | b','
                | b';'
                | b':'
                | b'\\'
                | b'"'
                | b'/'
                | b'['
                | b']'
                | b'?'
                | b'='
                | b'{'
                | b'}'
        )
}

fn request_must_be_get(request: &Request<RequestBody>) -> Result<(), WireError> {
    if request.method() != http::Method::GET {
        return Err(WireError::invalid_request(
            "WebSocket request must use the GET method",
        ));
    }
    Ok(())
}

/// Validated artefacts extracted from a successful 101 Switching Protocols
/// response. The transport branch (Task 17) hands these to the engine config.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by transport branch in Task 17
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

    let headers = response.headers();

    if !upgrade_has_websocket(headers) {
        return Err(HandshakeFailure::MissingUpgrade);
    }

    if !connection_has_upgrade(headers) {
        return Err(HandshakeFailure::MissingConnection);
    }

    let accept = headers
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok())
        .ok_or(HandshakeFailure::InvalidAccept)?;
    if accept != expected_accept {
        return Err(HandshakeFailure::InvalidAccept);
    }

    let subprotocol = validate_selected_subprotocol(headers, offered_subprotocols)?;

    reject_unrequested_extensions(headers)?;

    Ok(ValidatedHandshake {
        subprotocol,
        extensions: Vec::new(),
    })
}

fn connection_has_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get_all("connection")
        .iter()
        .any(|value| header_value_has_token(value, "upgrade"))
}

fn upgrade_has_websocket(headers: &HeaderMap) -> bool {
    headers
        .get_all("upgrade")
        .iter()
        .any(|value| header_value_has_token(value, "websocket"))
}

fn header_value_has_token(value: &HeaderValue, expected: &str) -> bool {
    value.to_str().is_ok_and(|raw| {
        raw.split(',')
            .any(|token| token.trim().eq_ignore_ascii_case(expected))
    })
}

fn validate_selected_subprotocol(
    headers: &HeaderMap,
    offered_subprotocols: &[String],
) -> Result<Option<String>, HandshakeFailure> {
    let mut selected = None;
    for value in headers.get_all("sec-websocket-protocol") {
        let token = value
            .to_str()
            .map_err(|_| HandshakeFailure::Other("invalid subprotocol header".into()))?;
        if token.contains(',') || selected.is_some() {
            return Err(HandshakeFailure::Other(
                "multiple subprotocols returned".into(),
            ));
        }
        selected = Some(token.to_string());
    }

    let Some(token) = selected else {
        return Ok(None);
    };

    if !offered_subprotocols.iter().any(|offered| offered == &token) {
        return Err(HandshakeFailure::SubprotocolMismatch {
            offered: offered_subprotocols.to_vec(),
            returned: token,
        });
    }

    Ok(Some(token))
}

fn reject_unrequested_extensions(headers: &HeaderMap) -> Result<(), HandshakeFailure> {
    for value in headers.get_all("sec-websocket-extensions") {
        let raw = value
            .to_str()
            .map_err(|_| HandshakeFailure::UnsupportedExtension("<invalid>".into()))?;
        if let Some(extension) = raw
            .split(',')
            .map(str::trim)
            .find(|extension| !extension.is_empty())
        {
            return Err(HandshakeFailure::UnsupportedExtension(
                extension.to_string(),
            ));
        }
    }
    Ok(())
}

fn rewrite_scheme(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    let parts = request.uri().clone().into_parts();
    let scheme = match parts.scheme.as_ref().map(http::uri::Scheme::as_str) {
        Some("ws") => http::uri::Scheme::HTTP,
        Some("wss") => http::uri::Scheme::HTTPS,
        Some("http") | Some("https") => return Ok(()),
        Some(other) => {
            return Err(WireError::invalid_request(format!(
                "WebSocket URI must use ws/wss/http/https, got {other}"
            )))
        }
        None => {
            return Err(WireError::invalid_request(
                "WebSocket URI must include a scheme (ws/wss)",
            ))
        }
    };
    let mut new_parts = parts;
    new_parts.scheme = Some(scheme);
    *request.uri_mut() = Uri::from_parts(new_parts)
        .map_err(|error| WireError::invalid_request(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use http::{HeaderValue, StatusCode};

    fn ok_response(accept: &str) -> http::Response<()> {
        let mut response = http::Response::new(());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        response
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("websocket"));
        response
            .headers_mut()
            .insert("connection", HeaderValue::from_static("Upgrade"));
        response.headers_mut().insert(
            "sec-websocket-accept",
            HeaderValue::from_str(accept).expect("accept header"),
        );
        response
    }

    #[test]
    fn accepts_valid_response() {
        let response = ok_response("expected");
        assert!(validate_handshake_response(&response, "expected", &[]).is_ok());
    }

    #[test]
    fn rejects_non_101() {
        let mut response = ok_response("expected");
        *response.status_mut() = StatusCode::OK;
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(err, HandshakeFailure::UnexpectedStatus));
    }

    #[test]
    fn rejects_bad_accept() {
        let response = ok_response("wrong");
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(err, HandshakeFailure::InvalidAccept));
    }

    #[test]
    fn rejects_missing_upgrade() {
        let mut response = ok_response("expected");
        response.headers_mut().remove("upgrade");
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(err, HandshakeFailure::MissingUpgrade));
    }

    #[test]
    fn accepts_websocket_token_across_multiple_upgrade_fields() {
        let mut response = ok_response("expected");
        response.headers_mut().remove("upgrade");
        response
            .headers_mut()
            .append("upgrade", HeaderValue::from_static("h2c"));
        response
            .headers_mut()
            .append("upgrade", HeaderValue::from_static("WebSocket"));

        assert!(validate_handshake_response(&response, "expected", &[]).is_ok());
    }

    #[test]
    fn accepts_websocket_token_inside_upgrade_comma_list() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("h2c, WebSocket"));

        assert!(validate_handshake_response(&response, "expected", &[]).is_ok());
    }

    #[test]
    fn rejects_upgrade_without_websocket_token() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("h2c"));

        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();

        assert!(matches!(err, HandshakeFailure::MissingUpgrade));
    }

    #[test]
    fn rejects_missing_connection() {
        let mut response = ok_response("expected");
        response.headers_mut().remove("connection");
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(err, HandshakeFailure::MissingConnection));
    }

    #[test]
    fn accepts_upgrade_token_across_multiple_connection_fields() {
        let mut response = ok_response("expected");
        response.headers_mut().remove("connection");
        response
            .headers_mut()
            .append("connection", HeaderValue::from_static("keep-alive"));
        response
            .headers_mut()
            .append("connection", HeaderValue::from_static("Upgrade"));

        assert!(validate_handshake_response(&response, "expected", &[]).is_ok());
    }

    #[test]
    fn accepts_upgrade_token_inside_connection_comma_list() {
        let mut response = ok_response("expected");
        response.headers_mut().insert(
            "connection",
            HeaderValue::from_static("keep-alive, UpGrAdE"),
        );

        assert!(validate_handshake_response(&response, "expected", &[]).is_ok());
    }

    #[test]
    fn rejects_subprotocol_not_offered() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("v2"));
        let err = validate_handshake_response(&response, "expected", &["v1".into()]).unwrap_err();
        assert!(matches!(err, HandshakeFailure::SubprotocolMismatch { .. }));
    }

    #[test]
    fn rejects_multiple_subprotocols_in_comma_list() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("v1, v2"));

        let err = validate_handshake_response(&response, "expected", &["v1".into(), "v2".into()])
            .unwrap_err();

        assert!(
            matches!(err, HandshakeFailure::Other(reason) if reason == "multiple subprotocols returned")
        );
    }

    #[test]
    fn rejects_multiple_subprotocol_header_fields() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .append("sec-websocket-protocol", HeaderValue::from_static("v1"));
        response
            .headers_mut()
            .append("sec-websocket-protocol", HeaderValue::from_static("v2"));

        let err = validate_handshake_response(&response, "expected", &["v1".into(), "v2".into()])
            .unwrap_err();

        assert!(
            matches!(err, HandshakeFailure::Other(reason) if reason == "multiple subprotocols returned")
        );
    }

    #[test]
    fn accepts_no_subprotocol_returned_when_offered() {
        let response = ok_response("expected");
        assert!(validate_handshake_response(&response, "expected", &["v1".into()]).is_ok());
    }

    #[test]
    fn rejects_unrequested_extensions() {
        let mut response = ok_response("expected");
        response.headers_mut().insert(
            "sec-websocket-extensions",
            HeaderValue::from_static("permessage-deflate, future"),
        );
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(
            err,
            HandshakeFailure::UnsupportedExtension(extension)
                if extension == "permessage-deflate"
        ));
    }

    #[test]
    fn rejects_unrequested_extensions_across_multiple_header_fields() {
        let mut response = ok_response("expected");
        response
            .headers_mut()
            .append("sec-websocket-extensions", HeaderValue::from_static(" "));
        response.headers_mut().append(
            "sec-websocket-extensions",
            HeaderValue::from_static("future"),
        );
        let err = validate_handshake_response(&response, "expected", &[]).unwrap_err();
        assert!(matches!(
            err,
            HandshakeFailure::UnsupportedExtension(extension) if extension == "future"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    fn request_with_subprotocols(protocols: Vec<&str>) -> Request<RequestBody> {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("ws://example.com/socket")
            .body(RequestBody::empty())
            .expect("request");
        request.extensions_mut().insert(WebSocketRequestMarker::new(
            protocols.into_iter().map(String::from).collect(),
        ));
        request
    }

    #[test]
    fn rfc_6455_section_1_3_example() {
        // RFC 6455 §1.3: Sec-WebSocket-Key dGhlIHNhbXBsZSBub25jZQ==
        // expected accept s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
        assert_eq!(
            derive_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

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

    #[test]
    fn inject_handshake_accepts_valid_subprotocol_tokens() {
        let mut request = request_with_subprotocols(vec!["chat", "superchat.v2"]);

        inject_handshake(&mut request).expect("valid subprotocols");

        assert_eq!(
            request
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("chat, superchat.v2")
        );
    }

    #[test]
    fn inject_handshake_rejects_invalid_subprotocol_tokens() {
        for protocol in ["", "chat room", "chat,evil", "chat/evil", "chät"] {
            let mut request = request_with_subprotocols(vec![protocol]);

            let error = inject_handshake(&mut request).expect_err("invalid token");

            assert_eq!(error.kind(), openwire_core::WireErrorKind::InvalidRequest);
        }
    }

    #[test]
    fn inject_handshake_rejects_duplicate_subprotocol_tokens() {
        let mut request = request_with_subprotocols(vec!["chat", "chat"]);

        let error = inject_handshake(&mut request).expect_err("duplicate token");

        assert_eq!(error.kind(), openwire_core::WireErrorKind::InvalidRequest);
    }
}
