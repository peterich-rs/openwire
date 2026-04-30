use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, CloseInitiator, EngineFrame, Message, MessageKind,
    WebSocketChannel, WebSocketEngineError, WebSocketError,
};
use openwire_core::{CallContext, SharedEventListener};

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
    ctx: Option<CallContext>,
    listener: Option<SharedEventListener>,
) {
    while let Some(cmd) = commands.recv().await {
        match cmd {
            WriterCommand::Send(message) => {
                emit_message_sent(ctx.as_ref(), listener.as_ref(), &message);
                if let Err(error) = sink.send(message.into()).await {
                    let mapped = map_engine_error(error);
                    if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                        listener.websocket_failed(ctx, &mapped);
                    }
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Ping(payload) => {
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_ping_sent(ctx);
                }
                if let Err(error) = sink.send(EngineFrame::Ping(payload)).await {
                    let mapped = map_engine_error(error);
                    if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                        listener.websocket_failed(ctx, &mapped);
                    }
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Pong(payload) => {
                if let Err(error) = sink.send(EngineFrame::Pong(payload)).await {
                    let mapped = map_engine_error(error);
                    if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                        listener.websocket_failed(ctx, &mapped);
                    }
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Close { code, reason, ack } => {
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_closing(ctx, code, &reason, CloseInitiator::Local);
                }
                let final_code = code;
                let final_reason = reason.clone();
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
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_closed(ctx, final_code, &final_reason);
                }
                return;
            }
            WriterCommand::Cancel => {
                let _ = sink.flush().await;
                return;
            }
        }
    }
}

fn emit_message_sent(
    ctx: Option<&CallContext>,
    listener: Option<&SharedEventListener>,
    message: &Message,
) {
    if let (Some(ctx), Some(listener)) = (ctx, listener) {
        match message {
            Message::Text(text) => {
                listener.websocket_message_sent(ctx, MessageKind::Text, text.len())
            }
            Message::Binary(bytes) => {
                listener.websocket_message_sent(ctx, MessageKind::Binary, bytes.len())
            }
            _ => {}
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
    pong_tracker: Option<PongTracker>,
    ctx: Option<CallContext>,
    listener: Option<SharedEventListener>,
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
                if let Some(tracker) = pong_tracker.as_ref() {
                    tracker.mark();
                }
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_pong_received(ctx);
                }
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Pong(payload))).await;
                }
            }
            Ok(EngineFrame::Close { code, reason }) => {
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_closing(ctx, code, &reason, CloseInitiator::Remote);
                    listener.websocket_closed(ctx, code, &reason);
                }
                let _ = out
                    .send(Err(WebSocketError::ClosedByPeer {
                        code,
                        reason: reason.clone(),
                    }))
                    .await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
            Ok(EngineFrame::Text(text)) => {
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_message_received(ctx, MessageKind::Text, text.len());
                }
                let _ = out.send(Ok(Message::Text(text))).await;
            }
            Ok(EngineFrame::Binary(bytes)) => {
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_message_received(
                        ctx,
                        MessageKind::Binary,
                        bytes.len(),
                    );
                }
                let _ = out.send(Ok(Message::Binary(bytes))).await;
            }
            Err(WebSocketEngineError::Io(error)) => {
                let mapped = WebSocketError::Io(error);
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_failed(ctx, &mapped);
                }
                let _ = out.send(Err(mapped)).await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
            Err(other) => {
                let mapped = WebSocketError::Engine(other);
                if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                    listener.websocket_failed(ctx, &mapped);
                }
                let _ = out.send(Err(mapped)).await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
        }
    }
    // Stream ended (EOF). Wake the writer so any pending close handshake
    // doesn't wait the full close_timeout when the peer never sent Close.
    let _ = auto_pong.send(WriterCommand::Cancel).await;
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
}

pub(crate) struct SessionConfig {
    pub queue_size: usize,
    pub deliver_control_frames: bool,
    pub close_timeout: Duration,
    pub heartbeat: Option<HeartbeatConfig>,
    pub ctx: Option<CallContext>,
    pub listener: Option<SharedEventListener>,
}

pub(crate) fn spawn_session(channel: WebSocketChannel, config: SessionConfig) -> SessionHandles {
    let SessionConfig {
        queue_size,
        deliver_control_frames,
        close_timeout,
        heartbeat,
        ctx,
        listener,
    } = config;

    let (sender_tx, sender_rx) = mpsc::channel::<WriterCommand>(queue_size);
    let (recv_tx, recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(queue_size);
    let auto_pong_tx = sender_tx.clone();

    let pong_tracker = heartbeat.as_ref().map(|_| PongTracker::new());
    let writer_span = tracing::info_span!("websocket_writer");
    let reader_span = tracing::info_span!("websocket_reader");

    tokio::spawn({
        let span = writer_span.clone();
        let ctx = ctx.clone();
        let listener = listener.clone();
        let recv_tx = recv_tx.clone();
        let send = channel.send;
        async move {
            let _enter = span.enter();
            run_writer(send, sender_rx, close_timeout, recv_tx, ctx, listener).await;
        }
    });

    tokio::spawn({
        let span = reader_span.clone();
        let ctx = ctx.clone();
        let listener = listener.clone();
        let recv = channel.recv;
        async move {
            let _enter = span.enter();
            run_reader(
                recv,
                deliver_control_frames,
                recv_tx,
                auto_pong_tx,
                pong_tracker,
                ctx,
                listener,
            )
            .await;
        }
    });

    if let Some(config) = heartbeat {
        let heartbeat_tx = sender_tx.clone();
        let span = tracing::info_span!("websocket_heartbeat");
        let pong_tracker = PongTracker::new();
        tokio::spawn(async move {
            let _enter = span.enter();
            run_heartbeat(
                config.interval,
                config.pong_timeout,
                pong_tracker,
                heartbeat_tx,
            )
            .await;
        });
    }

    SessionHandles {
        sender_tx,
        receiver_rx: recv_rx,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HeartbeatConfig {
    pub interval: Duration,
    pub pong_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct PongTracker {
    last_pong_ms: Arc<AtomicU64>,
    start: Arc<Instant>,
}

impl PongTracker {
    pub(crate) fn new() -> Self {
        Self {
            last_pong_ms: Arc::new(AtomicU64::new(0)),
            start: Arc::new(Instant::now()),
        }
    }

    pub(crate) fn mark(&self) {
        self.last_pong_ms
            .store(self.start.elapsed().as_millis() as u64, Ordering::Release);
    }

    pub(crate) fn since_last_pong(&self) -> Duration {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last_pong_ms.load(Ordering::Acquire);
        Duration::from_millis(now.saturating_sub(last))
    }
}

pub(crate) async fn run_heartbeat(
    interval: Duration,
    pong_timeout: Duration,
    tracker: PongTracker,
    out: mpsc::Sender<WriterCommand>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick so we don't ping before the user can send.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if out.send(WriterCommand::Ping(Bytes::new())).await.is_err() {
            return;
        }
        if tracker.since_last_pong() > pong_timeout {
            let (ack_tx, _) = tokio::sync::oneshot::channel();
            let _ = out
                .send(WriterCommand::Close {
                    code: 1011,
                    reason: "ping timeout".into(),
                    ack: ack_tx,
                })
                .await;
            return;
        }
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

        let writer = tokio::spawn(run_writer(
            sink,
            cmd_rx,
            Duration::from_millis(50),
            recv_tx,
            None,
            None,
        ));

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
        let writer = tokio::spawn(run_writer(
            sink,
            cmd_rx,
            Duration::from_secs(1),
            recv_tx,
            None,
            None,
        ));

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(WriterCommand::Close {
                code: 1000,
                reason: "bye".into(),
                ack: ack_tx,
            })
            .await
            .expect("close");
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
            None,
            None,
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
        ack_rx.await.expect("ack");
        writer.await.expect("writer joined");
    }
}
