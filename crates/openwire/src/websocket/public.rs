use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use openwire_core::websocket::{
    validate_close_frame, validate_outbound_message, Message, WebSocketError, WebSocketHandshake,
};
use tokio::sync::mpsc;

use crate::websocket::writer::WriterCommand;

#[derive(Clone)]
pub struct WebSocketSender {
    inner: Arc<SenderInner>,
}

struct SenderInner {
    tx: mpsc::Sender<WriterCommand>,
    closed: AtomicBool,
}

impl Drop for SenderInner {
    fn drop(&mut self) {
        // Best-effort cancel signal so the writer task wakes up and flushes
        // when the last sender clone goes out of scope. If the channel is
        // already closed (writer already returned) the send is a no-op.
        let _ = self.tx.try_send(WriterCommand::Cancel);
    }
}

impl WebSocketSender {
    pub(crate) fn new(tx: mpsc::Sender<WriterCommand>) -> Self {
        Self {
            inner: Arc::new(SenderInner {
                tx,
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub async fn send(&self, message: Message) -> Result<(), WebSocketError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(WebSocketError::LocalCancelled);
        }
        validate_outbound_message(&message)?;
        self.inner
            .tx
            .send(WriterCommand::Send(message))
            .await
            .map_err(|_| WebSocketError::LocalCancelled)
    }

    pub async fn send_text(&self, text: impl Into<String>) -> Result<(), WebSocketError> {
        self.send(Message::Text(text.into())).await
    }

    pub async fn send_binary(&self, bytes: impl Into<Bytes>) -> Result<(), WebSocketError> {
        self.send(Message::Binary(bytes.into())).await
    }

    /// Initiate a graceful close. Returns once the writer task has either
    /// observed the peer's close acknowledgement or the close timeout has
    /// fired. Subsequent calls are idempotent.
    pub async fn close(&self, code: u16, reason: impl Into<String>) -> Result<(), WebSocketError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }

        let reason = reason.into();
        validate_close_frame(code, &reason)?;

        if self
            .inner
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.inner
            .tx
            .send(WriterCommand::Close {
                code,
                reason,
                ack: ack_tx,
            })
            .await
            .map_err(|_| WebSocketError::LocalCancelled)?;
        let _ = ack_rx.await;
        Ok(())
    }

    pub fn queue_size(&self) -> usize {
        self.inner
            .tx
            .max_capacity()
            .saturating_sub(self.inner.tx.capacity())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.tx.is_closed()
    }
}

pub struct WebSocketReceiver {
    pub(crate) rx: mpsc::Receiver<Result<Message, WebSocketError>>,
}

impl Stream for WebSocketReceiver {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

pub struct WebSocket {
    pub(crate) sender: WebSocketSender,
    pub(crate) receiver: WebSocketReceiver,
    pub(crate) handshake: WebSocketHandshake,
}

impl WebSocket {
    pub fn handshake(&self) -> &WebSocketHandshake {
        &self.handshake
    }

    pub fn sender(&self) -> WebSocketSender {
        self.sender.clone()
    }

    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        (self.sender, self.receiver)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use openwire_core::websocket::{
        Message, WebSocketEngineError, WebSocketError, MAX_CLOSE_REASON_BYTES,
        MAX_CONTROL_FRAME_PAYLOAD_BYTES,
    };
    use tokio::sync::mpsc;

    use super::WebSocketSender;
    use crate::websocket::writer::WriterCommand;

    #[tokio::test]
    async fn close_accepts_maximum_sized_reason() {
        let (tx, mut rx) = mpsc::channel::<WriterCommand>(4);
        let sender = WebSocketSender::new(tx);
        let reason = "a".repeat(MAX_CLOSE_REASON_BYTES);

        let close = tokio::spawn({
            let sender = sender.clone();
            let reason = reason.clone();
            async move { sender.close(1000, reason).await }
        });

        let command = rx.recv().await.expect("close command");
        match command {
            WriterCommand::Close {
                code,
                reason: queued_reason,
                ack,
            } => {
                assert_eq!(code, 1000);
                assert_eq!(queued_reason, reason);
                let _ = ack.send(());
            }
            _ => panic!("expected close command"),
        }

        close.await.expect("close joined").expect("close succeeds");
        assert!(sender.is_closed());

        sender
            .close(1005, "")
            .await
            .expect("subsequent close remains idempotent");
    }

    #[tokio::test]
    async fn close_rejects_oversized_reason_without_closing_sender() {
        let (tx, mut rx) = mpsc::channel::<WriterCommand>(4);
        let sender = WebSocketSender::new(tx);
        let reason = "a".repeat(MAX_CLOSE_REASON_BYTES + 1);

        let error = sender
            .close(1000, reason)
            .await
            .expect_err("oversized reason should fail");

        assert!(matches!(
            error,
            WebSocketError::Engine(WebSocketEngineError::InvalidFrame(_))
        ));
        assert!(rx.try_recv().is_err());
        assert!(!sender.is_closed());

        sender
            .send_text("still open")
            .await
            .expect("sender remains usable");
        assert!(matches!(
            rx.recv().await,
            Some(WriterCommand::Send(Message::Text(text))) if text == "still open"
        ));
    }

    #[tokio::test]
    async fn close_rejects_reserved_wire_codes_without_closing_sender() {
        for code in [1005, 1006, 1015] {
            let (tx, mut rx) = mpsc::channel::<WriterCommand>(4);
            let sender = WebSocketSender::new(tx);

            let error = sender
                .close(code, "")
                .await
                .expect_err("reserved close code should fail");

            assert!(matches!(
                error,
                WebSocketError::Engine(WebSocketEngineError::InvalidCloseCode(actual))
                    if actual == code
            ));
            assert!(rx.try_recv().is_err());
            assert!(!sender.is_closed());
        }
    }

    #[tokio::test]
    async fn send_rejects_invalid_control_messages_without_enqueueing() {
        let invalid_messages = [
            Message::Close {
                code: 1005,
                reason: String::new(),
            },
            Message::Ping(Bytes::from(vec![0; MAX_CONTROL_FRAME_PAYLOAD_BYTES + 1])),
            Message::Pong(Bytes::from(vec![0; MAX_CONTROL_FRAME_PAYLOAD_BYTES + 1])),
        ];

        for message in invalid_messages {
            let (tx, mut rx) = mpsc::channel::<WriterCommand>(4);
            let sender = WebSocketSender::new(tx);

            let error = sender
                .send(message)
                .await
                .expect_err("invalid control message should fail");

            assert!(matches!(error, WebSocketError::Engine(_)));
            assert!(rx.try_recv().is_err());
            assert!(!sender.is_closed());
        }
    }
}
