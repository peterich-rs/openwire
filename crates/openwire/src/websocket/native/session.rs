use bytes::{Bytes, BytesMut};

use openwire_core::websocket::{EngineFrame, WebSocketEngineError};

use super::codec::{close_code_is_valid, DecodedFrame, Opcode};

pub(crate) struct ReassemblyState {
    max_message_size: usize,
    in_progress: Option<InProgress>,
}

struct InProgress {
    opcode: Opcode,
    buffer: BytesMut,
}

impl ReassemblyState {
    pub(crate) fn new(max_message_size: usize) -> Self {
        Self {
            max_message_size,
            in_progress: None,
        }
    }

    pub(crate) fn feed(
        &mut self,
        frame: DecodedFrame,
    ) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        if frame.opcode.is_control() {
            return Ok(Some(self.parse_control(frame)?));
        }

        match (frame.opcode, self.in_progress.is_some()) {
            (Opcode::Continuation, false) => Err(WebSocketEngineError::InvalidFrame(
                "orphan continuation frame".into(),
            )),
            (Opcode::Continuation, true) => self.append_continuation(frame),
            (Opcode::Text | Opcode::Binary, false) => {
                if frame.fin {
                    self.deliver_single(frame.opcode, frame.payload)
                } else {
                    self.in_progress = Some(InProgress {
                        opcode: frame.opcode,
                        buffer: BytesMut::from(&frame.payload[..]),
                    });
                    Ok(None)
                }
            }
            (Opcode::Text | Opcode::Binary, true) => Err(WebSocketEngineError::InvalidFrame(
                "nested data frame while reassembling".into(),
            )),
            (Opcode::Close | Opcode::Ping | Opcode::Pong, _) => {
                unreachable!("control frames handled above")
            }
        }
    }

    fn append_continuation(
        &mut self,
        frame: DecodedFrame,
    ) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        let state = self
            .in_progress
            .as_mut()
            .expect("checked by caller before calling append");
        let new_len = state.buffer.len() + frame.payload.len();
        if new_len > self.max_message_size {
            return Err(WebSocketEngineError::PayloadTooLarge {
                limit: self.max_message_size,
                received: new_len,
            });
        }
        state.buffer.extend_from_slice(&frame.payload);

        if frame.fin {
            let opcode = state.opcode;
            let buffer = std::mem::take(&mut state.buffer).freeze();
            self.in_progress = None;
            self.deliver_single(opcode, buffer)
        } else {
            Ok(None)
        }
    }

    fn deliver_single(
        &self,
        opcode: Opcode,
        payload: Bytes,
    ) -> Result<Option<EngineFrame>, WebSocketEngineError> {
        if payload.len() > self.max_message_size {
            return Err(WebSocketEngineError::PayloadTooLarge {
                limit: self.max_message_size,
                received: payload.len(),
            });
        }
        match opcode {
            Opcode::Text => {
                let text = std::str::from_utf8(&payload)
                    .map_err(|_| WebSocketEngineError::InvalidUtf8)?
                    .to_string();
                Ok(Some(EngineFrame::Text(text)))
            }
            Opcode::Binary => Ok(Some(EngineFrame::Binary(payload))),
            _ => unreachable!("deliver_single only handles Text/Binary"),
        }
    }

    fn parse_control(
        &self,
        frame: DecodedFrame,
    ) -> Result<EngineFrame, WebSocketEngineError> {
        match frame.opcode {
            Opcode::Ping => Ok(EngineFrame::Ping(frame.payload)),
            Opcode::Pong => Ok(EngineFrame::Pong(frame.payload)),
            Opcode::Close => {
                if frame.payload.is_empty() {
                    Ok(EngineFrame::Close {
                        code: 1005,
                        reason: String::new(),
                    })
                } else if frame.payload.len() == 1 {
                    Err(WebSocketEngineError::InvalidFrame(
                        "close payload of length 1".into(),
                    ))
                } else {
                    let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                    if !close_code_is_valid(code) {
                        return Err(WebSocketEngineError::InvalidCloseCode(code));
                    }
                    let reason = std::str::from_utf8(&frame.payload[2..])
                        .map_err(|_| WebSocketEngineError::InvalidUtf8)?
                        .to_string();
                    Ok(EngineFrame::Close { code, reason })
                }
            }
            _ => unreachable!("parse_control only handles control opcodes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(fin: bool, opcode: Opcode, payload: &[u8]) -> DecodedFrame {
        DecodedFrame {
            fin,
            opcode,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn assembles_fragmented_text() {
        let mut session = ReassemblyState::new(1024);
        let first = session.feed(frame(false, Opcode::Text, b"He")).unwrap();
        assert!(first.is_none());
        let second = session
            .feed(frame(true, Opcode::Continuation, b"llo"))
            .unwrap()
            .unwrap();
        match second {
            EngineFrame::Text(text) => assert_eq!(text, "Hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut session = ReassemblyState::new(1024);
        let err = session
            .feed(frame(true, Opcode::Text, &[0xff, 0xfe]))
            .unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidUtf8));
    }

    #[test]
    fn rejects_message_over_limit_in_single_frame() {
        let mut session = ReassemblyState::new(2);
        let err = session
            .feed(frame(true, Opcode::Binary, &[1, 2, 3]))
            .unwrap_err();
        assert!(matches!(
            err,
            WebSocketEngineError::PayloadTooLarge { .. }
        ));
    }

    #[test]
    fn rejects_message_over_limit_during_reassembly() {
        let mut session = ReassemblyState::new(4);
        session.feed(frame(false, Opcode::Binary, &[1, 2, 3])).unwrap();
        let err = session
            .feed(frame(true, Opcode::Continuation, &[4, 5]))
            .unwrap_err();
        assert!(matches!(
            err,
            WebSocketEngineError::PayloadTooLarge { .. }
        ));
    }

    #[test]
    fn parses_close_payload() {
        let mut session = ReassemblyState::new(1024);
        let mut payload = vec![0x03, 0xe8];
        payload.extend_from_slice(b"bye");
        let result = session
            .feed(frame(true, Opcode::Close, &payload))
            .unwrap()
            .unwrap();
        match result {
            EngineFrame::Close { code, reason } => {
                assert_eq!(code, 1000);
                assert_eq!(reason, "bye");
            }
            other => panic!("expected close, got {other:?}"),
        }
    }

    #[test]
    fn empty_close_payload_yields_1005() {
        let mut session = ReassemblyState::new(1024);
        let result = session
            .feed(frame(true, Opcode::Close, &[]))
            .unwrap()
            .unwrap();
        match result {
            EngineFrame::Close { code, .. } => assert_eq!(code, 1005),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_orphan_continuation() {
        let mut session = ReassemblyState::new(1024);
        let err = session
            .feed(frame(true, Opcode::Continuation, b"x"))
            .unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }

    #[test]
    fn rejects_nested_data_frame() {
        let mut session = ReassemblyState::new(1024);
        session.feed(frame(false, Opcode::Text, b"He")).unwrap();
        let err = session
            .feed(frame(true, Opcode::Binary, b"!"))
            .unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }

    #[test]
    fn rejects_invalid_close_code() {
        let mut session = ReassemblyState::new(1024);
        // 1004 is reserved
        let err = session
            .feed(frame(true, Opcode::Close, &[0x03, 0xec]))
            .unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidCloseCode(_)));
    }
}
