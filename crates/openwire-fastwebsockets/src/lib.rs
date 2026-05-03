//! `openwire-fastwebsockets` plugs `fastwebsockets` into openwire's
//! [`WebSocketEngine`] trait so a client can swap the bundled native codec
//! for fastwebsockets' framing.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use openwire::{Client, RequestBody};
//! use openwire_fastwebsockets::FastWebSocketsEngine;
//!
//! let request = http::Request::builder()
//!     .method(http::Method::GET)
//!     .uri("ws://127.0.0.1:9001/")
//!     .body(RequestBody::empty())?;
//! let client = Client::builder().build()?;
//! let websocket = client
//!     .new_websocket(request)
//!     .engine(FastWebSocketsEngine::shared())
//!     .execute()
//!     .await?;
//! # let _ = websocket;
//! # Ok(()) }
//! ```

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use fastwebsockets::FragmentCollectorRead;
use fastwebsockets::Frame as FastFrame;
use fastwebsockets::OpCode as FastOpCode;
use fastwebsockets::Role as FastRole;
use fastwebsockets::WebSocketError as FastWebSocketError;
use fastwebsockets::WebSocketWrite as FastWriteHalf;
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Role, WebSocketChannel, WebSocketEngine,
    WebSocketEngineConfig, WebSocketEngineError,
};
use openwire_core::{BoxConnection, BoxFuture, WireError, WireErrorKind};
use openwire_tokio::TokioIo;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::Mutex;

/// `fastwebsockets`-backed [`WebSocketEngine`] implementation.
#[derive(Clone, Default)]
pub struct FastWebSocketsEngine;

impl FastWebSocketsEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl WebSocketEngine for FastWebSocketsEngine {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<Result<WebSocketChannel, WebSocketEngineError>> {
        Box::pin(async move {
            validate_config(&config)?;

            let websocket = fastwebsockets::WebSocket::after_handshake(TokioIo::new(io), FastRole::Client);
            let (mut read, write) = websocket.split(tokio::io::split);
            read.set_auto_close(false);
            read.set_auto_pong(false);
            read.set_max_message_size(config.max_message_size);

            let send: BoxEngineSink = Box::pin(FastEngineSink::new(write));
            let recv: BoxEngineStream = Box::pin(FastEngineStream::new(
                FragmentCollectorRead::new(read),
                config.max_message_size,
            ));
            Ok(WebSocketChannel { send, recv })
        })
    }
}

fn validate_config(config: &WebSocketEngineConfig) -> Result<(), WebSocketEngineError> {
    if config.role != Role::Client {
        return Err(WebSocketEngineError::UnsupportedExtension(
            "fastwebsockets engine only supports client role".into(),
        ));
    }
    if config.extensions.iter().any(|extension| !extension.is_empty()) {
        return Err(WebSocketEngineError::UnsupportedExtension(
            config.extensions.join(", "),
        ));
    }
    Ok(())
}

type BoxOpFuture = Pin<Box<dyn Future<Output = Result<(), WebSocketEngineError>> + Send>>;
type BoxReadFuture =
    Pin<Box<dyn Future<Output = Option<Result<EngineFrame, WebSocketEngineError>>> + Send>>;

struct FastEngineSink<W> {
    inner: Arc<Mutex<FastWriteHalf<W>>>,
    buffered: Option<EngineFrame>,
    write_fut: Option<BoxOpFuture>,
    flush_fut: Option<BoxOpFuture>,
}

impl<W> FastEngineSink<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn new(inner: FastWriteHalf<W>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            buffered: None,
            write_fut: None,
            flush_fut: None,
        }
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), WebSocketEngineError>> {
        if self.write_fut.is_none() {
            if let Some(frame) = self.buffered.take() {
                let inner = Arc::clone(&self.inner);
                self.write_fut = Some(Box::pin(async move {
                    let mut writer = inner.lock_owned().await;
                    writer.write_frame(engine_to_fast(frame)).await.map_err(map_error)
                }));
            }
        }

        if let Some(fut) = self.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    self.write_fut = None;
                    result?;
                }
            }
        }

        if let Some(fut) = self.flush_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    self.flush_fut = None;
                    result?;
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    fn start_flush(&mut self) {
        if self.flush_fut.is_some() {
            return;
        }

        let inner = Arc::clone(&self.inner);
        self.flush_fut = Some(Box::pin(async move {
            let mut writer = inner.lock_owned().await;
            writer.flush().await.map_err(map_error)
        }));
    }
}

impl<W> Sink<EngineFrame> for FastEngineSink<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = WebSocketEngineError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.as_mut().get_mut().poll_pending(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
        let me = self.as_mut().get_mut();
        if me.buffered.is_some() {
            return Err(closed_sink_error("write already buffered"));
        }
        me.buffered = Some(item);
        Ok(())
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        let me = self.as_mut().get_mut();
        match me.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                me.start_flush();
                me.poll_pending(cx)
            }
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.as_mut().poll_flush(cx)
    }
}

struct FastEngineStream<R> {
    inner: Arc<Mutex<FragmentCollectorRead<R>>>,
    read_fut: Option<BoxReadFuture>,
    max_message_size: usize,
}

impl<R> FastEngineStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    fn new(inner: FragmentCollectorRead<R>, max_message_size: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            read_fut: None,
            max_message_size,
        }
    }

    fn start_read(&mut self) {
        if self.read_fut.is_some() {
            return;
        }

        let inner = Arc::clone(&self.inner);
        let max_message_size = self.max_message_size;
        self.read_fut = Some(Box::pin(async move {
            let mut reader = inner.lock_owned().await;
            let mut noop_send = |_| async { Ok::<(), Infallible>(()) };
            match reader.read_frame::<_, Infallible>(&mut noop_send).await {
                Ok(frame) => Some(fast_to_engine(frame)),
                Err(FastWebSocketError::ConnectionClosed) => None,
                Err(error) => Some(Err(map_error_with_limit(error, max_message_size))),
            }
        }));
    }
}

impl<R> Stream for FastEngineStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    type Item = Result<EngineFrame, WebSocketEngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.as_mut().get_mut();
        me.start_read();

        let Some(fut) = me.read_fut.as_mut() else {
            return Poll::Ready(None);
        };

        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                me.read_fut = None;
                Poll::Ready(result)
            }
        }
    }
}

fn engine_to_fast(frame: EngineFrame) -> FastFrame<'static> {
    match frame {
        EngineFrame::Text(text) => FastFrame::text(text.into_bytes().into()),
        EngineFrame::Binary(bytes) => FastFrame::binary(bytes.to_vec().into()),
        EngineFrame::Ping(bytes) => {
            FastFrame::new(true, FastOpCode::Ping, None, bytes.to_vec().into())
        }
        EngineFrame::Pong(bytes) => FastFrame::pong(bytes.to_vec().into()),
        EngineFrame::Close { code, reason } => FastFrame::close(code, reason.as_bytes()),
    }
}

fn fast_to_engine(frame: FastFrame<'_>) -> Result<EngineFrame, WebSocketEngineError> {
    match frame.opcode {
        FastOpCode::Text => {
            let text = String::from_utf8(frame.payload.to_vec())
                .map_err(|_| WebSocketEngineError::InvalidUtf8)?;
            Ok(EngineFrame::Text(text))
        }
        FastOpCode::Binary => Ok(EngineFrame::Binary(Bytes::from(frame.payload.to_vec()))),
        FastOpCode::Ping => Ok(EngineFrame::Ping(Bytes::from(frame.payload.to_vec()))),
        FastOpCode::Pong => Ok(EngineFrame::Pong(Bytes::from(frame.payload.to_vec()))),
        FastOpCode::Close => {
            let (code, reason) = parse_close_payload(&frame.payload)?;
            Ok(EngineFrame::Close { code, reason })
        }
        FastOpCode::Continuation => Err(WebSocketEngineError::InvalidFrame(
            "fragment collector returned continuation frame".into(),
        )),
    }
}

fn parse_close_payload(payload: &[u8]) -> Result<(u16, String), WebSocketEngineError> {
    if payload.is_empty() {
        return Ok((1005, String::new()));
    }
    if payload.len() == 1 {
        return Err(WebSocketEngineError::InvalidFrame(
            "close payload of length 1".into(),
        ));
    }

    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !fastwebsockets::CloseCode::from(code).is_allowed() {
        return Err(WebSocketEngineError::InvalidCloseCode(code));
    }

    let reason = std::str::from_utf8(&payload[2..])
        .map_err(|_| WebSocketEngineError::InvalidUtf8)?
        .to_string();
    Ok((code, reason))
}

fn map_error(error: FastWebSocketError) -> WebSocketEngineError {
    map_error_with_limit(error, 0)
}

fn map_error_with_limit(error: FastWebSocketError, max_message_size: usize) -> WebSocketEngineError {
    match error {
        FastWebSocketError::IoError(io) => protocol_io_error("fastwebsockets IO error", io),
        FastWebSocketError::InvalidUTF8 => WebSocketEngineError::InvalidUtf8,
        FastWebSocketError::PingFrameTooLarge => WebSocketEngineError::PayloadTooLarge {
            limit: 125,
            received: 126,
        },
        FastWebSocketError::FrameTooLarge => WebSocketEngineError::PayloadTooLarge {
            limit: max_message_size,
            received: max_message_size.saturating_add(1),
        },
        other => WebSocketEngineError::InvalidFrame(other.to_string()),
    }
}

fn protocol_io_error(
    message: &'static str,
    error: std::io::Error,
) -> WebSocketEngineError {
    WebSocketEngineError::Io(WireError::with_source(WireErrorKind::Protocol, message, error))
}

fn closed_sink_error(message: &'static str) -> WebSocketEngineError {
    WebSocketEngineError::Io(WireError::new(WireErrorKind::Protocol, message))
}