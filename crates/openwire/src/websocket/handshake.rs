use base64::Engine;
use http::header::{CONNECTION, UPGRADE};
use http::{HeaderValue, Request, Uri, Version};
use sha1::{Digest, Sha1};

use openwire_core::{RequestBody, WireError};

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
    request.extensions_mut().insert(RoutePreference::Http1Only);

    Ok(())
}

fn request_must_be_get(request: &Request<RequestBody>) -> Result<(), WireError> {
    if request.method() != http::Method::GET {
        return Err(WireError::invalid_request(
            "WebSocket request must use the GET method",
        ));
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
}
