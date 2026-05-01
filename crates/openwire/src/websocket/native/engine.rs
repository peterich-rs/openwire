use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use openwire_core::websocket::{
    BoxEngineSink, BoxEngineStream, EngineFrame, Role, WebSocketChannel, WebSocketEngine,
    WebSocketEngineConfig, WebSocketEngineError,
};
use openwire_core::{BoxConnection, BoxFuture, WireError};
use openwire_tokio::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};

use super::codec::{decode_frame, encode_frame, FrameHeader, Opcode};
use super::mask::random_mask_key;
use super::session::ReassemblyState;

const READ_CHUNK_SIZE: usize = 8 * 1024;

#[derive(Default)]
pub struct NativeEngine;

impl NativeEngine {
    pub fn new() -> Self {
        Self
    }

    /// Convenience constructor returning an `Arc`-wrapped engine handle, the
    /// shape `WebSocketCall::engine` accepts. Useful when you want to share
    /// one engine across many calls.
    #[allow(dead_code)]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl WebSocketEngine for NativeEngine {
    fn upgrade(
        &self,
        io: BoxConnection,
        config: WebSocketEngineConfig,
    ) -> BoxFuture<Result<WebSocketChannel, WebSocketEngineError>> {
        Box::pin(async move {
            if config.role != Role::Client {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    "native engine only supports client role in v1".into(),
                ));
            }
            if config
                .extensions
                .iter()
                .any(|extension| !extension.is_empty())
            {
                return Err(WebSocketEngineError::UnsupportedExtension(
                    config.extensions.join(", "),
                ));
            }

            let tokio_io = TokioIo::new(io);
            let (read_half, write_half) = tokio::io::split(tokio_io);

            let send: BoxEngineSink = Box::pin(NativeSink::new(write_half));
            let recv: BoxEngineStream = Box::pin(NativeStream::new(
                read_half,
                config.max_frame_size,
                config.max_message_size,
            ));
            Ok(WebSocketChannel { send, recv })
        })
    }
}

type WriteIo = WriteHalf<TokioIo<BoxConnection>>;
type ReadIo = ReadHalf<TokioIo<BoxConnection>>;

struct NativeSink {
    write: WriteIo,
    buf: BytesMut,
}

impl NativeSink {
    fn new(write: WriteIo) -> Self {
        Self {
            write,
            buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), WebSocketEngineError>> {
        while !self.buf.is_empty() {
            match Pin::new(&mut self.write).poll_write(cx, &self.buf[..]) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(WebSocketEngineError::Io(WireError::internal(
                        "websocket peer closed write half",
                        std::io::Error::from(std::io::ErrorKind::WriteZero),
                    ))));
                }
                Poll::Ready(Ok(n)) => self.buf.advance(n),
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(WebSocketEngineError::Io(WireError::with_source(
                        openwire_core::WireErrorKind::Protocol,
                        "websocket write failed",
                        error,
                    ))));
                }
            }
        }
        match Pin::new(&mut self.write).poll_flush(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(error)) => {
                Poll::Ready(Err(WebSocketEngineError::Io(WireError::with_source(
                    openwire_core::WireErrorKind::Protocol,
                    "websocket flush failed",
                    error,
                ))))
            }
        }
    }
}

impl Sink<EngineFrame> for NativeSink {
    type Error = WebSocketEngineError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: EngineFrame) -> Result<(), Self::Error> {
        let me = self.get_mut();
        let key = random_mask_key();
        let header = match &item {
            EngineFrame::Text(_) => FrameHeader {
                fin: true,
                opcode: Opcode::Text,
            },
            EngineFrame::Binary(_) => FrameHeader {
                fin: true,
                opcode: Opcode::Binary,
            },
            EngineFrame::Ping(_) => FrameHeader {
                fin: true,
                opcode: Opcode::Ping,
            },
            EngineFrame::Pong(_) => FrameHeader {
                fin: true,
                opcode: Opcode::Pong,
            },
            EngineFrame::Close { .. } => FrameHeader {
                fin: true,
                opcode: Opcode::Close,
            },
        };
        let payload = match item {
            EngineFrame::Text(text) => Bytes::from(text.into_bytes()),
            EngineFrame::Binary(bytes) | EngineFrame::Ping(bytes) | EngineFrame::Pong(bytes) => {
                bytes
            }
            EngineFrame::Close { code, reason } => {
                let mut payload = BytesMut::with_capacity(2 + reason.len());
                payload.extend_from_slice(&code.to_be_bytes());
                payload.extend_from_slice(reason.as_bytes());
                payload.freeze()
            }
        };
        encode_frame(&mut me.buf, header, &payload, Some(key));
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().drain(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The close handshake is the user's responsibility — drain the buffer
        // but do not actively shut down the writer. Connection teardown
        // happens when the underlying BoxConnection is dropped.
        self.get_mut().drain(cx)
    }
}

struct NativeStream {
    read: ReadIo,
    buf: BytesMut,
    reassembly: ReassemblyState,
    max_frame_size: usize,
    done: bool,
}

impl NativeStream {
    fn new(read: ReadIo, max_frame_size: usize, max_message_size: usize) -> Self {
        Self {
            read,
            buf: BytesMut::with_capacity(8 * 1024),
            reassembly: ReassemblyState::new(max_message_size),
            max_frame_size,
            done: false,
        }
    }

    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<Result<bool, WebSocketEngineError>> {
        let mut chunk = [0u8; READ_CHUNK_SIZE];
        let mut read_buf = ReadBuf::new(&mut chunk);
        match Pin::new(&mut self.read).poll_read(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    Poll::Ready(Ok(true)) // eof
                } else {
                    self.buf.extend_from_slice(filled);
                    Poll::Ready(Ok(false))
                }
            }
            Poll::Ready(Err(error)) => {
                Poll::Ready(Err(WebSocketEngineError::Io(WireError::with_source(
                    openwire_core::WireErrorKind::Protocol,
                    "websocket read failed",
                    error,
                ))))
            }
        }
    }
}

impl Stream for NativeStream {
    type Item = Result<EngineFrame, WebSocketEngineError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        loop {
            if me.done {
                return Poll::Ready(None);
            }

            match decode_frame(&mut me.buf, me.max_frame_size) {
                Err(error) => {
                    me.done = true;
                    return Poll::Ready(Some(Err(error)));
                }
                Ok(Some(frame)) => {
                    let was_close = frame.opcode == Opcode::Close;
                    match me.reassembly.feed(frame) {
                        Err(error) => {
                            me.done = true;
                            return Poll::Ready(Some(Err(error)));
                        }
                        Ok(Some(engine_frame)) => {
                            if was_close {
                                me.done = true;
                            }
                            return Poll::Ready(Some(Ok(engine_frame)));
                        }
                        Ok(None) => continue,
                    }
                }
                Ok(None) => match me.poll_fill(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        me.done = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Ready(Ok(true)) => {
                        me.done = true;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Ok(false)) => continue,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use futures_util::{SinkExt, StreamExt};
    use openwire_core::{BoxConnection, Connected, Connection};
    use openwire_tokio::TokioIo;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    use super::*;

    struct TestIo(TokioIo<DuplexStream>);

    impl Connection for TestIo {
        fn connected(&self) -> Connected {
            Connected::new()
        }
    }

    impl hyper::rt::Read for TestIo {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: hyper::rt::ReadBufCursor<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }

    impl hyper::rt::Write for TestIo {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, std::io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_flush(cx)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }

    fn box_connection(stream: DuplexStream) -> BoxConnection {
        Box::new(TestIo(TokioIo::new(stream)))
    }

    #[tokio::test]
    async fn engine_decodes_unmasked_text_from_server() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        // Server writes a Text frame: FIN=1, opcode=Text, len=2, no mask.
        server_io
            .write_all(&[0x81, 0x02, b'h', b'i'])
            .await
            .expect("server write");

        let engine = NativeEngine::new();
        let cfg = WebSocketEngineConfig {
            role: Role::Client,
            subprotocol: None,
            extensions: vec![],
            max_frame_size: 1024,
            max_message_size: 1024,
        };
        let mut channel = engine
            .upgrade(box_connection(client_io), cfg)
            .await
            .expect("upgrade");
        let frame = channel.recv.next().await.expect("frame").expect("ok");
        match frame {
            EngineFrame::Text(text) => assert_eq!(text, "hi"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_writes_masked_client_frame() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);

        let engine = NativeEngine::new();
        let cfg = WebSocketEngineConfig {
            role: Role::Client,
            subprotocol: None,
            extensions: vec![],
            max_frame_size: 1024,
            max_message_size: 1024,
        };
        let mut channel = engine
            .upgrade(box_connection(client_io), cfg)
            .await
            .expect("upgrade");
        channel
            .send
            .send(EngineFrame::Text("hi".into()))
            .await
            .expect("send");
        channel.send.flush().await.expect("flush");

        let mut header = [0u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut server_io, &mut header)
            .await
            .expect("server read header");
        assert_eq!(header[0], 0x81);
        assert_eq!(header[1], 0x80 | 2);
        // Then 2 masked bytes.
        let mut payload = [0u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut server_io, &mut payload)
            .await
            .expect("server read payload");
        let key = [header[2], header[3], header[4], header[5]];
        let unmasked = [payload[0] ^ key[0], payload[1] ^ key[1]];
        assert_eq!(&unmasked, b"hi");
    }

    #[tokio::test]
    async fn engine_rejects_server_role() {
        let (client_io, _server_io) = tokio::io::duplex(1024);
        let engine = NativeEngine::new();
        let cfg = WebSocketEngineConfig {
            role: Role::Server,
            subprotocol: None,
            extensions: vec![],
            max_frame_size: 1024,
            max_message_size: 1024,
        };
        match engine.upgrade(box_connection(client_io), cfg).await {
            Err(WebSocketEngineError::UnsupportedExtension(_)) => {}
            Ok(_) => panic!("server role must be rejected"),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_rejects_unknown_extension() {
        let (client_io, _server_io) = tokio::io::duplex(1024);
        let engine = NativeEngine::new();
        let cfg = WebSocketEngineConfig {
            role: Role::Client,
            subprotocol: None,
            extensions: vec!["permessage-deflate".into()],
            max_frame_size: 1024,
            max_message_size: 1024,
        };
        match engine.upgrade(box_connection(client_io), cfg).await {
            Err(WebSocketEngineError::UnsupportedExtension(_)) => {}
            Ok(_) => panic!("unknown extension must be rejected"),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_returns_eof_when_server_closes() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        drop(server_io); // server hangs up before sending anything

        let engine = NativeEngine::new();
        let cfg = WebSocketEngineConfig {
            role: Role::Client,
            subprotocol: None,
            extensions: vec![],
            max_frame_size: 1024,
            max_message_size: 1024,
        };
        let mut channel = engine
            .upgrade(box_connection(client_io), cfg)
            .await
            .expect("upgrade");
        assert!(channel.recv.next().await.is_none());
    }
}
