use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::sink::Sink;
use futures_util::stream::Stream;

use openwire_core::websocket::{
	BoxEngineSink, BoxEngineStream, CloseInitiator, EngineFrame, MessageKind, WebSocketChannel,
	WebSocketEngineError,
};
use openwire_core::{CallContext, SharedEventListener};

pub(crate) fn instrument_channel(
	channel: WebSocketChannel,
	ctx: CallContext,
	listener: SharedEventListener,
) -> WebSocketChannel {
	WebSocketChannel {
		send: Box::pin(InstrumentedSink {
			inner: channel.send,
			ctx: ctx.clone(),
			listener: listener.clone(),
		}),
		recv: Box::pin(InstrumentedStream {
			inner: channel.recv,
			ctx,
			listener,
		}),
	}
}

struct InstrumentedSink {
	inner: BoxEngineSink,
	ctx: CallContext,
	listener: SharedEventListener,
}

impl Sink<EngineFrame> for InstrumentedSink {
	type Error = WebSocketEngineError;

	fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.as_mut().poll_ready(cx)
	}

	fn start_send(mut self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
		match &item {
			EngineFrame::Text(text) => self.listener.websocket_message_sent(
				&self.ctx,
				MessageKind::Text,
				text.len(),
			),
			EngineFrame::Binary(bytes) => self.listener.websocket_message_sent(
				&self.ctx,
				MessageKind::Binary,
				bytes.len(),
			),
			EngineFrame::Ping(_) => self.listener.websocket_ping_sent(&self.ctx),
			EngineFrame::Pong(_) | EngineFrame::Close { .. } => {}
		}
		self.inner.as_mut().start_send(item)
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.as_mut().poll_flush(cx)
	}

	fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.as_mut().poll_close(cx)
	}
}

struct InstrumentedStream {
	inner: BoxEngineStream,
	ctx: CallContext,
	listener: SharedEventListener,
}

impl Stream for InstrumentedStream {
	type Item = Result<EngineFrame, WebSocketEngineError>;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		match self.inner.as_mut().poll_next(cx) {
			Poll::Pending => Poll::Pending,
			Poll::Ready(None) => Poll::Ready(None),
			Poll::Ready(Some(Ok(frame))) => {
				match &frame {
					EngineFrame::Text(text) => self.listener.websocket_message_received(
						&self.ctx,
						MessageKind::Text,
						text.len(),
					),
					EngineFrame::Binary(bytes) => self.listener.websocket_message_received(
						&self.ctx,
						MessageKind::Binary,
						bytes.len(),
					),
					EngineFrame::Pong(_) => self.listener.websocket_pong_received(&self.ctx),
					EngineFrame::Close { code, reason } => self.listener.websocket_closing(
						&self.ctx,
						*code,
						reason,
						CloseInitiator::Remote,
					),
					EngineFrame::Ping(_) => {}
				}
				Poll::Ready(Some(Ok(frame)))
			}
			Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
		}
	}
}
