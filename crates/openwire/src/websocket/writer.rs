use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Message, WebSocketChannel, WebSocketEngineError,
    WebSocketError,
};

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

/// Drives the engine's outbound `Sink` from the `WriterCommand` channel.
/// On `Close`, sends the close frame, then waits for `Cancel` from the
/// reader (peer Close observed) or the timeout, whichever comes first.
pub(crate) async fn run_writer(
    mut sink: BoxEngineSink,
    mut commands: mpsc::Receiver<WriterCommand>,
    close_timeout: Duration,
    receiver_tx: mpsc::Sender<Result<Message, WebSocketError>>,
) {
    while let Some(cmd) = commands.recv().await {
        match cmd {
            WriterCommand::Send(message) => {
                if let Err(error) = sink.send(message.into()).await {
                    let _ = receiver_tx.send(Err(map_engine_error(error))).await;
                    return;
                }
            }
            WriterCommand::Ping(payload) => {
                if let Err(error) = sink.send(EngineFrame::Ping(payload)).await {
                    let _ = receiver_tx.send(Err(map_engine_error(error))).await;
                    return;
                }
            }
            WriterCommand::Pong(payload) => {
                if let Err(error) = sink.send(EngineFrame::Pong(payload)).await {
                    let _ = receiver_tx.send(Err(map_engine_error(error))).await;
                    return;
                }
            }
            WriterCommand::Close { code, reason, ack } => {
                let _ = sink.send(EngineFrame::Close { code, reason }).await;
                let _ = sink.flush().await;
                let _ = tokio::time::timeout(close_timeout, async {
                    while let Some(other) = commands.recv().await {
                        if matches!(other, WriterCommand::Cancel) {
                            break;
                        }
                    }
                })
                .await;
                let _ = ack.send(());
                return;
            }
            WriterCommand::Cancel => {
                let _ = sink.flush().await;
                return;
            }
        }
    }
}

/// Drives the engine's inbound `Stream`, forwarding messages to the user's
/// `WebSocketReceiver` and auto-responding to `Ping` frames. On `Close`,
/// signals the writer task with `Cancel` so the close handshake can complete.
pub(crate) async fn run_reader(
    mut stream: BoxEngineStream,
    deliver_control_frames: bool,
    out: mpsc::Sender<Result<Message, WebSocketError>>,
    auto_pong: mpsc::Sender<WriterCommand>,
) {
    while let Some(item) = stream.next().await {
        match item {
            Ok(EngineFrame::Ping(payload)) => {
                let _ = auto_pong
                    .send(WriterCommand::Pong(payload.clone()))
                    .await;
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Ping(payload))).await;
                }
            }
            Ok(EngineFrame::Pong(payload)) => {
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Pong(payload))).await;
                }
            }
            Ok(EngineFrame::Close { code, reason }) => {
                let _ = out
                    .send(Err(WebSocketError::ClosedByPeer {
                        code,
                        reason: reason.clone(),
                    }))
                    .await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
            Ok(other) => {
                let _ = out.send(Ok(other.into())).await;
            }
            Err(WebSocketEngineError::Io(error)) => {
                let _ = out.send(Err(WebSocketError::Io(error))).await;
                return;
            }
            Err(other) => {
                let _ = out.send(Err(WebSocketError::Engine(other))).await;
                return;
            }
        }
    }
}

fn map_engine_error(error: WebSocketEngineError) -> WebSocketError {
    match error {
        WebSocketEngineError::Io(io) => WebSocketError::Io(io),
        other => WebSocketError::Engine(other),
    }
}

pub(crate) struct SessionHandles {
    pub sender_tx: mpsc::Sender<WriterCommand>,
    pub receiver_rx: mpsc::Receiver<Result<Message, WebSocketError>>,
    pub _drop_guard: DropGuard,
}

/// Best-effort cancel signal sent to the writer when the user's `WebSocket`
/// is dropped without explicitly calling `close`.
pub(crate) struct DropGuard {
    cancel_tx: mpsc::Sender<WriterCommand>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        let _ = self.cancel_tx.try_send(WriterCommand::Cancel);
    }
}

pub(crate) fn spawn_session(
    channel: WebSocketChannel,
    queue_size: usize,
    deliver_control_frames: bool,
    close_timeout: Duration,
) -> SessionHandles {
    let (sender_tx, sender_rx) = mpsc::channel::<WriterCommand>(queue_size);
    let (recv_tx, recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(queue_size);
    let auto_pong_tx = sender_tx.clone();

    tokio::spawn(run_writer(
        channel.send,
        sender_rx,
        close_timeout,
        recv_tx.clone(),
    ));
    tokio::spawn(run_reader(
        channel.recv,
        deliver_control_frames,
        recv_tx,
        auto_pong_tx,
    ));

    SessionHandles {
        sender_tx: sender_tx.clone(),
        receiver_rx: recv_rx,
        _drop_guard: DropGuard {
            cancel_tx: sender_tx,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::sink;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct CapturingSink {
        captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>>,
    }

    impl futures_util::Sink<EngineFrame> for CapturingSink {
        type Error = WebSocketEngineError;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
            self.captured.lock().unwrap().push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn writer_processes_send_then_cancel() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>> = Default::default();
        let sink: BoxEngineSink = Box::pin(CapturingSink {
            captured: captured.clone(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>(8);
        let (recv_tx, _recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(8);

        let writer = tokio::spawn(run_writer(sink, cmd_rx, Duration::from_millis(50), recv_tx));

        cmd_tx
            .send(WriterCommand::Send(Message::Text("hi".into())))
            .await
            .expect("send");
        cmd_tx.send(WriterCommand::Cancel).await.expect("cancel");
        writer.await.expect("writer joined");

        let captured = captured.lock().unwrap();
        assert!(matches!(captured.as_slice(), [EngineFrame::Text(t)] if t == "hi"));
    }

    #[tokio::test]
    async fn writer_close_completes_on_cancel() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>> = Default::default();
        let sink: BoxEngineSink = Box::pin(CapturingSink {
            captured: captured.clone(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>(8);
        let (recv_tx, _recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(8);
        let writer = tokio::spawn(run_writer(sink, cmd_rx, Duration::from_secs(1), recv_tx));

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(WriterCommand::Close {
                code: 1000,
                reason: "bye".into(),
                ack: ack_tx,
            })
            .await
            .expect("close");
        // Simulate reader observing peer close.
        cmd_tx.send(WriterCommand::Cancel).await.expect("cancel");

        ack_rx.await.expect("ack received");
        writer.await.expect("writer joined");

        let captured = captured.lock().unwrap();
        assert!(matches!(
            captured.as_slice(),
            [EngineFrame::Close { code: 1000, .. }]
        ));
    }

    #[tokio::test]
    async fn writer_close_completes_on_timeout() {
        // No reader; cancel never arrives. Close must complete via timeout.
        let _drain = sink::drain::<EngineFrame>();
        let captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>> = Default::default();
        let sink: BoxEngineSink = Box::pin(CapturingSink {
            captured: captured.clone(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>(4);
        let (recv_tx, _recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(4);
        let writer = tokio::spawn(run_writer(
            sink,
            cmd_rx,
            Duration::from_millis(50),
            recv_tx,
        ));
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(WriterCommand::Close {
                code: 1001,
                reason: String::new(),
                ack: ack_tx,
            })
            .await
            .expect("close");
        // Don't send Cancel — let timeout fire.
        ack_rx.await.expect("ack");
        writer.await.expect("writer joined");
    }
}
