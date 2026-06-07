pub mod engine;
pub mod error;
pub mod handshake_info;
pub mod message;

pub use engine::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Role, SharedWebSocketEngine, WebSocketChannel,
    WebSocketEngine, WebSocketEngineConfig,
};
pub use error::{HandshakeFailure, TimeoutKind, WebSocketEngineError, WebSocketError};
pub use handshake_info::WebSocketHandshake;
pub use message::{
    close_code_is_valid, validate_close_frame, validate_outbound_engine_frame,
    validate_outbound_message, CloseInitiator, Message, MessageKind, MAX_CLOSE_REASON_BYTES,
    MAX_CONTROL_FRAME_PAYLOAD_BYTES,
};
