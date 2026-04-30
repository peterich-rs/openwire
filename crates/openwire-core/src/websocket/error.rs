use thiserror::Error;

use crate::WireError;

#[derive(Debug, Error)]
pub enum WebSocketError {
    #[error("handshake failed: {reason:?}")]
    Handshake {
        status: Option<http::StatusCode>,
        reason: HandshakeFailure,
    },

    #[error(transparent)]
    Engine(#[from] WebSocketEngineError),

    #[error("server closed the connection: {code} {reason}")]
    ClosedByPeer { code: u16, reason: String },

    #[error("websocket {0:?} timed out")]
    Timeout(TimeoutKind),

    #[error("transport io error: {0}")]
    Io(#[source] WireError),

    #[error("local cancellation")]
    LocalCancelled,
}

#[derive(Debug, Clone)]
pub enum HandshakeFailure {
    UnexpectedStatus,
    MissingUpgrade,
    MissingConnection,
    InvalidAccept,
    SubprotocolMismatch {
        offered: Vec<String>,
        returned: String,
    },
    UnsupportedExtension(String),
    Other(String),
}

#[derive(Debug, Clone, Copy)]
pub enum TimeoutKind {
    Handshake,
    Close,
    Ping,
}

#[derive(Debug, Error)]
pub enum WebSocketEngineError {
    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    #[error("invalid utf-8 in text frame")]
    InvalidUtf8,

    #[error("invalid close code: {0}")]
    InvalidCloseCode(u16),

    #[error("payload too large: limit={limit} received={received}")]
    PayloadTooLarge { limit: usize, received: usize },

    #[error("unsupported extension: {0}")]
    UnsupportedExtension(String),

    #[error("io error: {0}")]
    Io(#[source] WireError),
}

impl WebSocketError {
    /// Convenience constructor for the bridge / transport branch.
    pub fn handshake(reason: HandshakeFailure, status: Option<http::StatusCode>) -> Self {
        Self::Handshake { status, reason }
    }
}
