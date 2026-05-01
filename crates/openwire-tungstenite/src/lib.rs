//! `openwire-tungstenite` plugs `tokio-tungstenite` into openwire's
//! [`WebSocketEngine`] trait so a client can swap the bundled native codec
//! for tungstenite's framing.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use openwire::{Client, RequestBody};
//! use openwire_tungstenite::TungsteniteEngine;
//!
//! let request = http::Request::builder()
//!     .method(http::Method::GET)
//!     .uri("ws://127.0.0.1:9001/")
//!     .body(RequestBody::empty())?;
//! let client = Client::builder().build()?;
//! let websocket = client
//!     .new_websocket(request)
//!     .engine(TungsteniteEngine::shared())
//!     .execute()
//!     .await?;
//! # let _ = websocket;
//! # Ok(()) }
//! ```

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Role, WebSocketChannel, WebSocketEngine,
    WebSocketEngineConfig, WebSocketEngineError,
};
use openwire_core::{BoxConnection, BoxFuture, WireError};
use openwire_tokio::TokioIo;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::WebSocketStream;

/// `tokio-tungstenite`-backed [`WebSocketEngine`] implementation.
#[derive(Clone, Default)]
pub struct TungsteniteEngine;

impl TungsteniteEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl WebSocketEngine for TungsteniteEngine {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<Result<WebSocketChannel, WebSocketEngineError>> {
        Box::pin(async move {
            if config.role != Role::Client {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    "tungstenite engine only supports client role".into(),
                ));
            }
            if config.extensions.iter().any(|e| !e.is_empty()) {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    config.extensions.join(", "),
                ));
            }

            // BoxConnection (hyper::rt::Read+Write) → tokio AsyncRead+Write.
            let tokio_io = TokioIo::new(io);
            let stream = WebSocketStream::from_raw_socket(
                tokio_io,
                tokio_tungstenite::tungstenite::protocol::Role::Client,
                None,
            )
            .await;

            use futures_util::stream::StreamExt as _;
            let (sink, stream) = stream.split();
            let send: BoxEngineSink = Box::pin(EngineSinkAdapter { inner: sink });
            let recv: BoxEngineStream = Box::pin(EngineStreamAdapter { inner: stream });
            Ok(WebSocketChannel { send, recv })
        })
    }
}

struct EngineSinkAdapter<S> {
    inner: S,
}

impl<S> Sink<EngineFrame> for EngineSinkAdapter<S>
where
    S: Sink<TungMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    type Error = WebSocketEngineError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx).map_err(map_error)
    }

    fn start_send(mut self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
        let message = engine_to_tung(item);
        Pin::new(&mut self.inner)
            .start_send(message)
            .map_err(map_error)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(map_error)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(map_error)
    }
}

struct EngineStreamAdapter<S> {
    inner: S,
}

impl<S> Stream for EngineStreamAdapter<S>
where
    S: Stream<Item = Result<TungMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    type Item = Result<EngineFrame, WebSocketEngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(message))) => match tung_to_engine(message) {
                Some(frame) => Poll::Ready(Some(Ok(frame))),
                None => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(map_error(error)))),
        }
    }
}

fn engine_to_tung(frame: EngineFrame) -> TungMessage {
    match frame {
        EngineFrame::Text(text) => TungMessage::Text(text),
        EngineFrame::Binary(bytes) => TungMessage::Binary(bytes.to_vec()),
        EngineFrame::Ping(bytes) => TungMessage::Ping(bytes.to_vec()),
        EngineFrame::Pong(bytes) => TungMessage::Pong(bytes.to_vec()),
        EngineFrame::Close { code, reason } => TungMessage::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        })),
    }
}

fn tung_to_engine(message: TungMessage) -> Option<EngineFrame> {
    Some(match message {
        TungMessage::Text(text) => EngineFrame::Text(text),
        TungMessage::Binary(bytes) => EngineFrame::Binary(Bytes::from(bytes)),
        TungMessage::Ping(bytes) => EngineFrame::Ping(Bytes::from(bytes)),
        TungMessage::Pong(bytes) => EngineFrame::Pong(Bytes::from(bytes)),
        TungMessage::Close(None) => EngineFrame::Close {
            code: 1005,
            reason: String::new(),
        },
        TungMessage::Close(Some(close_frame)) => EngineFrame::Close {
            code: close_frame.code.into(),
            reason: close_frame.reason.into_owned(),
        },
        TungMessage::Frame(_) => return None,
    })
}

fn map_error(error: tokio_tungstenite::tungstenite::Error) -> WebSocketEngineError {
    use tokio_tungstenite::tungstenite::Error as TE;
    match error {
        TE::Io(io) => WebSocketEngineError::Io(WireError::with_source(
            openwire_core::WireErrorKind::Protocol,
            "tungstenite IO error",
            io,
        )),
        TE::Utf8 => WebSocketEngineError::InvalidUtf8,
        TE::Capacity(_) => WebSocketEngineError::PayloadTooLarge {
            limit: 0,
            received: 0,
        },
        TE::Protocol(error) => WebSocketEngineError::InvalidFrame(error.to_string()),
        other => WebSocketEngineError::InvalidFrame(other.to_string()),
    }
}
