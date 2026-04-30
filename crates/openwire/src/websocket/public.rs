use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use openwire_core::websocket::{Message, WebSocketError, WebSocketHandshake};
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
    pub async fn close(
        &self,
        code: u16,
        reason: impl Into<String>,
    ) -> Result<(), WebSocketError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.inner
            .tx
            .send(WriterCommand::Close {
                code,
                reason: reason.into(),
                ack: ack_tx,
            })
            .await
            .map_err(|_| WebSocketError::LocalCancelled)?;
        let _ = ack_rx.await;
        Ok(())
    }

    pub fn queue_size(&self) -> usize {
        self.inner.tx.max_capacity().saturating_sub(self.inner.tx.capacity())
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
    pub(crate) _drop_guard: crate::websocket::writer::DropGuard,
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
