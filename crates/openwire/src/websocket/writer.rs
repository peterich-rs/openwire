use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Notify};

use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, CloseInitiator, EngineFrame, Message, TimeoutKind,
    WebSocketChannel, WebSocketEngineError, WebSocketError,
};
use openwire_core::{CallContext, SharedEventListener, WireError, WireErrorKind};

use crate::websocket::instrumented::instrument_channel;

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
    CloseAck {
        code: u16,
        reason: String,
    },
    PingTimeout,
    Cancel,
}

/// Drives the engine's outbound `Sink` from the `WriterCommand` channel.
/// On `Close`, sends the close frame, then waits for `Cancel` from the
/// reader (peer Close observed) or the timeout, whichever comes first.
async fn run_writer(
    mut sink: BoxEngineSink,
    mut commands: mpsc::Receiver<WriterCommand>,
    close_timeout: Duration,
    receiver_tx: mpsc::Sender<Result<Message, WebSocketError>>,
    ctx: Option<CallContext>,
    listener: Option<SharedEventListener>,
    session: SessionState,
) {
    while let Some(cmd) = commands.recv().await {
        match cmd {
            WriterCommand::Send(message) => {
                if session.remote_close_started() {
                    continue;
                }
                if let Err(error) = sink.send(message.into()).await {
                    let mapped = map_engine_error(error);
                    emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Ping(payload) => {
                if session.remote_close_started() {
                    continue;
                }
                if let Err(error) = sink.send(EngineFrame::Ping(payload)).await {
                    let mapped = map_engine_error(error);
                    emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Pong(payload) => {
                if session.remote_close_started() {
                    continue;
                }
                if let Err(error) = sink.send(EngineFrame::Pong(payload)).await {
                    let mapped = map_engine_error(error);
                    emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
                    let _ = receiver_tx.send(Err(mapped)).await;
                    return;
                }
            }
            WriterCommand::Close { code, reason, ack } => {
                if session.remote_close_started() {
                    let _ = ack.send(());
                    continue;
                }
                session.mark_local_close_started();
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
                if session.try_mark_closed() {
                    if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                        listener.websocket_closed(ctx, final_code, &final_reason);
                    }
                    emit_call_end(ctx.as_ref(), &session);
                }
                let _ = ack.send(());
                return;
            }
            WriterCommand::CloseAck { code, reason } => {
                let _ = sink.send(EngineFrame::Close { code, reason }).await;
                let _ = sink.flush().await;
                return;
            }
            WriterCommand::PingTimeout => {
                if session.remote_close_started() {
                    continue;
                }
                session.mark_local_close_started();
                let error = WebSocketError::Timeout(TimeoutKind::Ping);
                emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &error, &session);
                let _ = receiver_tx
                    .send(Err(WebSocketError::Timeout(TimeoutKind::Ping)))
                    .await;
                let _ = sink
                    .send(EngineFrame::Close {
                        code: 1011,
                        reason: "ping timeout".into(),
                    })
                    .await;
                let _ = sink.flush().await;
                return;
            }
            WriterCommand::Cancel => {
                let _ = sink.flush().await;
                let error = WebSocketError::LocalCancelled;
                emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &error, &session);
                return;
            }
        }
    }
}

struct ReaderRuntime {
    out: mpsc::Sender<Result<Message, WebSocketError>>,
    auto_pong: mpsc::Sender<WriterCommand>,
    pong_tracker: Option<PongTracker>,
    ctx: Option<CallContext>,
    listener: Option<SharedEventListener>,
    session: SessionState,
}

/// Drives the engine's inbound `Stream`, forwarding messages to the user's
/// `WebSocketReceiver` and auto-responding to `Ping` frames. On `Close`,
/// signals the writer task with `Cancel` so the close handshake can complete.
async fn run_reader(
    mut stream: BoxEngineStream,
    deliver_control_frames: bool,
    runtime: ReaderRuntime,
) {
    let ReaderRuntime {
        out,
        auto_pong,
        pong_tracker,
        ctx,
        listener,
        session,
    } = runtime;

    while let Some(item) = stream.next().await {
        match item {
            Ok(EngineFrame::Ping(payload)) => {
                let _ = auto_pong.send(WriterCommand::Pong(payload.clone())).await;
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Ping(payload))).await;
                }
            }
            Ok(EngineFrame::Pong(payload)) => {
                if let Some(tracker) = pong_tracker.as_ref() {
                    tracker.mark();
                }
                if deliver_control_frames {
                    let _ = out.send(Ok(Message::Pong(payload))).await;
                }
            }
            Ok(EngineFrame::Close { code, reason }) => {
                let local_close_started = session.local_close_started();
                session.mark_remote_close_started();
                if !local_close_started && session.try_mark_closed() {
                    if let (Some(ctx), Some(listener)) = (ctx.as_ref(), listener.as_ref()) {
                        listener.websocket_closed(ctx, code, &reason);
                    }
                    emit_call_end(ctx.as_ref(), &session);
                }
                let _ = out
                    .send(Err(WebSocketError::ClosedByPeer {
                        code,
                        reason: reason.clone(),
                    }))
                    .await;
                let command = if local_close_started {
                    WriterCommand::Cancel
                } else {
                    WriterCommand::CloseAck { code, reason }
                };
                let _ = auto_pong.send(command).await;
                return;
            }
            Ok(EngineFrame::Text(text)) => {
                let _ = out.send(Ok(Message::Text(text))).await;
            }
            Ok(EngineFrame::Binary(bytes)) => {
                let _ = out.send(Ok(Message::Binary(bytes))).await;
            }
            Err(WebSocketEngineError::Io(error)) => {
                let mapped = WebSocketError::Io(error);
                emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
                let _ = out.send(Err(mapped)).await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
            Err(other) => {
                let mapped = WebSocketError::Engine(other);
                emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
                let _ = out.send(Err(mapped)).await;
                let _ = auto_pong.send(WriterCommand::Cancel).await;
                return;
            }
        }
    }
    if session.local_close_started() {
        let _ = auto_pong.send(WriterCommand::Cancel).await;
        return;
    }

    // EOF before either side starts a Close handshake is an abnormal WebSocket
    // termination. Surface it as a protocol failure and wake the writer so it
    // can stop promptly.
    let mapped = WebSocketError::Engine(WebSocketEngineError::InvalidFrame(
        "websocket stream ended before close frame".into(),
    ));
    emit_terminal_failure(ctx.as_ref(), listener.as_ref(), &mapped, &session);
    let _ = out.send(Err(mapped)).await;
    let _ = auto_pong.send(WriterCommand::Cancel).await;
}

fn map_engine_error(error: WebSocketEngineError) -> WebSocketError {
    match error {
        WebSocketEngineError::Io(io) => WebSocketError::Io(io),
        other => WebSocketError::Engine(other),
    }
}

pub(crate) fn websocket_error_as_wire_error(error: &WebSocketError) -> WireError {
    match error {
        WebSocketError::Io(error) => error.clone(),
        WebSocketError::Timeout(_) => WireError::timeout(error.to_string()),
        WebSocketError::LocalCancelled => {
            WireError::new(WireErrorKind::Canceled, "websocket call cancelled")
        }
        WebSocketError::Handshake { .. }
        | WebSocketError::Engine(_)
        | WebSocketError::ClosedByPeer { .. } => {
            WireError::new(WireErrorKind::Protocol, error.to_string())
        }
    }
}

fn emit_terminal_failure(
    ctx: Option<&CallContext>,
    listener: Option<&SharedEventListener>,
    error: &WebSocketError,
    session: &SessionState,
) {
    let Some(ctx) = ctx else {
        return;
    };
    if !session.try_mark_call_terminal() {
        return;
    }
    if let Some(listener) = listener {
        listener.websocket_failed(ctx, error);
    }
    let wire_error = websocket_error_as_wire_error(error);
    ctx.listener().call_failed(ctx, &wire_error);
}

fn emit_call_end(ctx: Option<&CallContext>, session: &SessionState) {
    let Some(ctx) = ctx else {
        return;
    };
    if session.try_mark_call_terminal() {
        ctx.listener().call_end(ctx);
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
    let session = SessionState::default();

    let channel = match (ctx.clone(), listener.clone()) {
        (Some(ctx), Some(listener)) => instrument_channel(channel, ctx, listener),
        _ => channel,
    };

    let pong_tracker = heartbeat.as_ref().map(|_| PongTracker::new());
    let writer_span = tracing::info_span!("websocket_writer");
    let reader_span = tracing::info_span!("websocket_reader");

    tokio::spawn({
        let span = writer_span.clone();
        let ctx = ctx.clone();
        let listener = listener.clone();
        let recv_tx = recv_tx.clone();
        let session = session.clone();
        let send = channel.send;
        async move {
            let _enter = span.enter();
            run_writer(
                send,
                sender_rx,
                close_timeout,
                recv_tx,
                ctx,
                listener,
                session,
            )
            .await;
        }
    });

    tokio::spawn({
        let span = reader_span.clone();
        let ctx = ctx.clone();
        let listener = listener.clone();
        let recv = channel.recv;
        let session = session.clone();
        let pong_tracker = pong_tracker.clone();
        async move {
            let _enter = span.enter();
            run_reader(
                recv,
                deliver_control_frames,
                ReaderRuntime {
                    out: recv_tx,
                    auto_pong: auto_pong_tx,
                    pong_tracker,
                    ctx,
                    listener,
                    session,
                },
            )
            .await;
        }
    });

    if let Some(config) = heartbeat {
        let heartbeat_tx = sender_tx.clone();
        let span = tracing::info_span!("websocket_heartbeat");
        let pong_tracker = pong_tracker.expect("heartbeat tracker missing");
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

#[derive(Clone, Default)]
struct SessionState {
    local_close_started: Arc<AtomicBool>,
    remote_close_started: Arc<AtomicBool>,
    closed_emitted: Arc<AtomicBool>,
    call_terminal_emitted: Arc<AtomicBool>,
}

impl SessionState {
    fn mark_local_close_started(&self) {
        self.local_close_started.store(true, Ordering::Release);
    }

    fn local_close_started(&self) -> bool {
        self.local_close_started.load(Ordering::Acquire)
    }

    fn mark_remote_close_started(&self) {
        self.remote_close_started.store(true, Ordering::Release);
    }

    fn remote_close_started(&self) -> bool {
        self.remote_close_started.load(Ordering::Acquire)
    }

    fn try_mark_closed(&self) -> bool {
        !self.closed_emitted.swap(true, Ordering::AcqRel)
    }

    fn try_mark_call_terminal(&self) -> bool {
        !self.call_terminal_emitted.swap(true, Ordering::AcqRel)
    }
}

#[derive(Clone)]
pub(crate) struct PongTracker {
    generation: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl PongTracker {
    pub(crate) fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn mark(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    async fn wait_for_pong_after(&self, generation: u64) {
        loop {
            let notified = self.notify.notified();
            if self.generation() != generation {
                return;
            }
            notified.await;
        }
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
        let pong_generation = tracker.generation();
        if out.send(WriterCommand::Ping(Bytes::new())).await.is_err() {
            return;
        }

        let timeout = tokio::time::sleep(pong_timeout);
        tokio::pin!(timeout);
        tokio::select! {
            _ = tracker.wait_for_pong_after(pong_generation) => {}
            _ = &mut timeout => {
                if tracker.generation() == pong_generation {
                    let _ = out.send(WriterCommand::PingTimeout).await;
                    return;
                }
            }
            _ = out.closed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{sink, stream};
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
            SessionState::default(),
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
    async fn writer_skips_queued_data_after_remote_close_and_sends_ack() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>> = Default::default();
        let sink: BoxEngineSink = Box::pin(CapturingSink {
            captured: captured.clone(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>(8);
        let (recv_tx, _recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(8);
        let session = SessionState::default();
        session.mark_remote_close_started();

        let writer = tokio::spawn(run_writer(
            sink,
            cmd_rx,
            Duration::from_millis(50),
            recv_tx,
            None,
            None,
            session,
        ));

        cmd_tx
            .send(WriterCommand::Send(Message::Text("late".into())))
            .await
            .expect("late send");
        cmd_tx
            .send(WriterCommand::CloseAck {
                code: 1000,
                reason: "peer done".into(),
            })
            .await
            .expect("close ack");

        writer.await.expect("writer joined");

        let captured = captured.lock().unwrap();
        assert!(matches!(
            captured.as_slice(),
            [EngineFrame::Close { code: 1000, reason }] if reason == "peer done"
        ));
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
            SessionState::default(),
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
            SessionState::default(),
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

    #[tokio::test]
    async fn writer_reports_ping_timeout_and_sends_close() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<EngineFrame>>> = Default::default();
        let sink: BoxEngineSink = Box::pin(CapturingSink {
            captured: captured.clone(),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>(4);
        let (recv_tx, mut recv_rx) = mpsc::channel::<Result<Message, WebSocketError>>(4);
        let writer = tokio::spawn(run_writer(
            sink,
            cmd_rx,
            Duration::from_millis(50),
            recv_tx,
            None,
            None,
            SessionState::default(),
        ));

        cmd_tx
            .send(WriterCommand::PingTimeout)
            .await
            .expect("ping timeout");

        match recv_rx.recv().await.expect("timeout error") {
            Err(WebSocketError::Timeout(TimeoutKind::Ping)) => {}
            other => panic!("expected ping timeout, got {other:?}"),
        }

        writer.await.expect("writer joined");
        let captured = captured.lock().unwrap();
        assert!(matches!(
            captured.as_slice(),
            [EngineFrame::Close { code: 1011, reason }] if reason == "ping timeout"
        ));
    }

    #[tokio::test]
    async fn reader_sends_close_ack_for_remote_close() {
        let stream: BoxEngineStream = Box::pin(stream::iter([Ok(EngineFrame::Close {
            code: 1000,
            reason: "remote done".into(),
        })]));
        let (out_tx, mut out_rx) = mpsc::channel::<Result<Message, WebSocketError>>(4);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCommand>(4);

        run_reader(
            stream,
            false,
            ReaderRuntime {
                out: out_tx,
                auto_pong: cmd_tx,
                pong_tracker: None,
                ctx: None,
                listener: None,
                session: SessionState::default(),
            },
        )
        .await;

        match out_rx.recv().await.expect("closed by peer") {
            Err(WebSocketError::ClosedByPeer { code, reason }) => {
                assert_eq!(code, 1000);
                assert_eq!(reason, "remote done");
            }
            other => panic!("expected ClosedByPeer, got {other:?}"),
        }
        assert!(matches!(
            cmd_rx.recv().await.expect("close ack"),
            WriterCommand::CloseAck { code: 1000, .. }
        ));
    }

    #[tokio::test]
    async fn reader_cancels_local_close_when_remote_close_arrives() {
        let stream: BoxEngineStream = Box::pin(stream::iter([Ok(EngineFrame::Close {
            code: 1000,
            reason: "ack".into(),
        })]));
        let (out_tx, _out_rx) = mpsc::channel::<Result<Message, WebSocketError>>(4);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCommand>(4);
        let session = SessionState::default();
        session.mark_local_close_started();

        run_reader(
            stream,
            false,
            ReaderRuntime {
                out: out_tx,
                auto_pong: cmd_tx,
                pong_tracker: None,
                ctx: None,
                listener: None,
                session,
            },
        )
        .await;

        assert!(matches!(
            cmd_rx.recv().await.expect("cancel"),
            WriterCommand::Cancel
        ));
    }

    #[tokio::test]
    async fn heartbeat_timeout_window_starts_after_ping_send() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCommand>(4);
        let tracker = PongTracker::new();
        let heartbeat = tokio::spawn(run_heartbeat(
            Duration::from_millis(80),
            Duration::from_millis(40),
            tracker,
            cmd_tx,
        ));

        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(150), cmd_rx.recv())
                .await
                .expect("first ping should arrive"),
            Some(WriterCommand::Ping(payload)) if payload.is_empty()
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(15), cmd_rx.recv())
                .await
                .is_err(),
            "ping timeout must not be queued immediately after the first ping"
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
                .await
                .expect("ping timeout should arrive"),
            Some(WriterCommand::PingTimeout)
        ));

        heartbeat.await.expect("heartbeat joined");
    }

    #[tokio::test]
    async fn heartbeat_pong_allows_next_ping() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCommand>(4);
        let tracker = PongTracker::new();
        let heartbeat_tracker = tracker.clone();
        let heartbeat = tokio::spawn(run_heartbeat(
            Duration::from_millis(30),
            Duration::from_millis(100),
            heartbeat_tracker,
            cmd_tx,
        ));

        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
                .await
                .expect("first ping should arrive"),
            Some(WriterCommand::Ping(payload)) if payload.is_empty()
        ));

        tracker.mark();

        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
                .await
                .expect("second ping should arrive after pong"),
            Some(WriterCommand::Ping(payload)) if payload.is_empty()
        ));

        heartbeat.abort();
    }
}
