use bytes::Bytes;
use openwire_core::websocket::Message;

/// Command messages enqueued from `WebSocketSender` (and the heartbeat task)
/// to the writer task. `Close` carries an oneshot so the caller can await the
/// acknowledged close handshake or timeout.
pub(crate) enum WriterCommand {
    Send(Message),
    Pong(Bytes),
    Ping(Bytes),
    Close {
        code: u16,
        reason: String,
        ack: tokio::sync::oneshot::Sender<()>,
    },
    Cancel,
}
