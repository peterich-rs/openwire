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
    control: mpsc::Sender<WriterCommand>,
    data: mpsc::Sender<Message>,
    closed: AtomicBool,
    shutdown: Arc<tokio::sync::Notify>,
}

impl Drop for SenderInner {
    fn drop(&mut self) {
        // Control may be full of pings/pongs. Notify is a slot-independent
        // shutdown so Drop cannot be lost on the bounded control lane.
        let _ = self.control.try_send(WriterCommand::Cancel);
        self.shutdown.notify_waiters();
    }
}

impl WebSocketSender {
    pub(crate) fn new(
        control: mpsc::Sender<WriterCommand>,
        data: mpsc::Sender<Message>,
        shutdown: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            inner: Arc::new(SenderInner {
                control,
                data,
                closed: AtomicBool::new(false),
                shutdown,
            }),
        }
    }

    pub async fn send(&self, message: Message) -> Result<(), WebSocketError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(WebSocketError::LocalCancelled);
        }
        validate_outbound_message(&message)?;
        self.inner
            .data
            .send(message)
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
            .control
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
            .data
            .max_capacity()
            .saturating_sub(self.inner.data.capacity())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.control.is_closed()
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
    use std::sync::Arc;

    use bytes::Bytes;
    use openwire_core::websocket::{
        Message, WebSocketEngineError, WebSocketError, MAX_CLOSE_REASON_BYTES,
        MAX_CONTROL_FRAME_PAYLOAD_BYTES,
    };
    use tokio::sync::mpsc;

    use super::WebSocketSender;
    use crate::websocket::writer::WriterCommand;

    fn test_sender() -> (
        WebSocketSender,
        mpsc::Receiver<WriterCommand>,
        mpsc::Receiver<Message>,
    ) {
        let (control_tx, control_rx) = mpsc::channel::<WriterCommand>(4);
        let (data_tx, data_rx) = mpsc::channel::<Message>(4);
        (
            WebSocketSender::new(control_tx, data_tx, Arc::new(tokio::sync::Notify::new())),
            control_rx,
            data_rx,
        )
    }

    #[tokio::test]
    async fn close_accepts_maximum_sized_reason() {
        let (sender, mut rx, _data_rx) = test_sender();
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
        let (sender, mut rx, mut data_rx) = test_sender();
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
            data_rx.recv().await,
            Some(Message::Text(text)) if text == "still open"
        ));
    }

    #[tokio::test]
    async fn close_rejects_reserved_wire_codes_without_closing_sender() {
        for code in [1005, 1006, 1015] {
            let (sender, mut rx, _data_rx) = test_sender();

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
    async fn close_accepts_iana_registered_wire_codes() {
        for code in [1012u16, 1013, 1014] {
            let (sender, mut rx, _data_rx) = test_sender();

            let close = tokio::spawn({
                let sender = sender.clone();
                async move { sender.close(code, "").await }
            });

            match rx.recv().await.expect("close command") {
                WriterCommand::Close {
                    code: actual,
                    reason,
                    ack,
                } => {
                    assert_eq!(actual, code);
                    assert!(reason.is_empty());
                    let _ = ack.send(());
                }
                _ => panic!("expected close command"),
            }

            close.await.expect("close joined").expect("close succeeds");
            assert!(sender.is_closed());
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
            let (sender, _control_rx, mut data_rx) = test_sender();

            let error = sender
                .send(message)
                .await
                .expect_err("invalid control message should fail");

            assert!(matches!(error, WebSocketError::Engine(_)));
            assert!(data_rx.try_recv().is_err());
            assert!(!sender.is_closed());
        }
    }

    #[tokio::test]
    async fn close_is_not_blocked_by_a_full_data_queue() {
        let (control_tx, mut control_rx) = mpsc::channel::<WriterCommand>(4);
        let (data_tx, _data_rx) = mpsc::channel::<Message>(1);
        data_tx
            .try_send(Message::Text("full".into()))
            .expect("fill data lane");
        let sender =
            WebSocketSender::new(control_tx, data_tx, Arc::new(tokio::sync::Notify::new()));

        let close = tokio::spawn({
            let sender = sender.clone();
            async move { sender.close(1000, "done").await }
        });

        match control_rx.recv().await.expect("close command") {
            WriterCommand::Close { code, reason, ack } => {
                assert_eq!(code, 1000);
                assert_eq!(reason, "done");
                let _ = ack.send(());
            }
            other => panic!("expected close, got {other:?}"),
        }

        close.await.expect("close joined").expect("close succeeds");
    }
}
