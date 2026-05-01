use http::{HeaderMap, StatusCode};

/// Public handshake metadata returned to the user when a WebSocket call
/// completes. Captures the 101 response status, the response headers as
/// observed on the wire, and the negotiated subprotocol / extensions.
#[derive(Debug, Clone)]
pub struct WebSocketHandshake {
    status: StatusCode,
    headers: HeaderMap,
    subprotocol: Option<String>,
    extensions: Vec<String>,
}

impl WebSocketHandshake {
    /// Construct a handshake record from the validated response components.
    /// Used by the transport branch after `validate_handshake_response`.
    pub fn new(
        status: StatusCode,
        headers: HeaderMap,
        subprotocol: Option<String>,
        extensions: Vec<String>,
    ) -> Self {
        Self {
            status,
            headers,
            subprotocol,
            extensions,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn subprotocol(&self) -> Option<&str> {
        self.subprotocol.as_deref()
    }

    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }
}
