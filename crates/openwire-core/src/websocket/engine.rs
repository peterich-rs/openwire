use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::sink::Sink;

use super::error::WebSocketEngineError;
use crate::transport::BoxConnection;
use crate::BoxFuture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    /// Reserved for v2; engines may reject `Server` with
    /// [`WebSocketEngineError::UnsupportedExtension`].
    Server,
}

#[derive(Clone, Debug)]
pub struct WebSocketEngineConfig {
    pub role: Role,
    pub subprotocol: Option<String>,
    pub extensions: Vec<String>,
    pub max_frame_size: usize,
    pub max_message_size: usize,
}

#[derive(Clone, Debug)]
pub enum EngineFrame {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

pub type BoxEngineSink = Pin<Box<dyn Sink<EngineFrame, Error = WebSocketEngineError> + Send>>;
pub type BoxEngineStream =
    Pin<Box<dyn Stream<Item = Result<EngineFrame, WebSocketEngineError>> + Send>>;

pub struct WebSocketChannel {
    pub send: BoxEngineSink,
    pub recv: BoxEngineStream,
}

pub trait WebSocketEngine: Send + Sync + 'static {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<Result<WebSocketChannel, WebSocketEngineError>>;
}

pub type SharedWebSocketEngine = Arc<dyn WebSocketEngine>;
