use bytes::Bytes;

use super::engine::EngineFrame;
use super::error::WebSocketEngineError;

pub const MAX_CONTROL_FRAME_PAYLOAD_BYTES: usize = 125;
pub const MAX_CLOSE_REASON_BYTES: usize = 123;
const CLOSE_NO_STATUS_CODE: u16 = 1005;

#[derive(Clone, Debug)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Text,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseInitiator {
    Local,
    Remote,
}

impl Message {
    pub fn kind(&self) -> Option<MessageKind> {
        match self {
            Message::Text(_) => Some(MessageKind::Text),
            Message::Binary(_) => Some(MessageKind::Binary),
            _ => None,
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Message::Text(s) => s.len(),
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
            Message::Close { reason, .. } => 2 + reason.len(),
        }
    }
}

impl From<Message> for EngineFrame {
    fn from(message: Message) -> Self {
        match message {
            Message::Text(text) => EngineFrame::Text(text),
            Message::Binary(bytes) => EngineFrame::Binary(bytes),
            Message::Ping(bytes) => EngineFrame::Ping(bytes),
            Message::Pong(bytes) => EngineFrame::Pong(bytes),
            Message::Close { code, reason } => EngineFrame::Close { code, reason },
        }
    }
}

impl From<EngineFrame> for Message {
    fn from(frame: EngineFrame) -> Self {
        match frame {
            EngineFrame::Text(text) => Message::Text(text),
            EngineFrame::Binary(bytes) => Message::Binary(bytes),
            EngineFrame::Ping(bytes) => Message::Ping(bytes),
            EngineFrame::Pong(bytes) => Message::Pong(bytes),
            EngineFrame::Close { code, reason } => Message::Close { code, reason },
        }
    }
}

/// WebSocket close codes accepted on the wire. The 1004/1005/1006 and 1015
/// codes are reserved for in-process signaling and must not appear in close
/// frames.
pub fn close_code_is_valid(code: u16) -> bool {
    matches!(
        code,
        1000..=1003 | 1007..=1014 | 3000..=4999
    )
}

pub fn validate_close_frame(code: u16, reason: &str) -> Result<(), WebSocketEngineError> {
    if !close_code_is_valid(code) {
        return Err(WebSocketEngineError::InvalidCloseCode(code));
    }

    let reason_len = reason.len();
    if reason_len > MAX_CLOSE_REASON_BYTES {
        return Err(WebSocketEngineError::InvalidFrame(format!(
            "close reason exceeds {MAX_CLOSE_REASON_BYTES} bytes"
        )));
    }

    Ok(())
}

pub fn validate_outbound_message(message: &Message) -> Result<(), WebSocketEngineError> {
    match message {
        Message::Ping(payload) => validate_control_payload_len("ping", payload.len()),
        Message::Pong(payload) => validate_control_payload_len("pong", payload.len()),
        Message::Close { code, reason } => validate_close_frame(*code, reason),
        Message::Text(_) | Message::Binary(_) => Ok(()),
    }
}

pub fn validate_outbound_engine_frame(frame: &EngineFrame) -> Result<(), WebSocketEngineError> {
    match frame {
        EngineFrame::Ping(payload) => validate_control_payload_len("ping", payload.len()),
        EngineFrame::Pong(payload) => validate_control_payload_len("pong", payload.len()),
        EngineFrame::Close { code, reason }
            if *code == CLOSE_NO_STATUS_CODE && reason.is_empty() =>
        {
            Ok(())
        }
        EngineFrame::Close { code, reason } => validate_close_frame(*code, reason),
        EngineFrame::Text(_) | EngineFrame::Binary(_) => Ok(()),
    }
}

fn validate_control_payload_len(kind: &str, len: usize) -> Result<(), WebSocketEngineError> {
    if len > MAX_CONTROL_FRAME_PAYLOAD_BYTES {
        return Err(WebSocketEngineError::InvalidFrame(format!(
            "{kind} control frame exceeds {MAX_CONTROL_FRAME_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_len_includes_close_code_bytes() {
        let m = Message::Close {
            code: 1000,
            reason: "ok".into(),
        };
        assert_eq!(m.payload_len(), 4);
    }

    #[test]
    fn close_code_rejects_reserved_and_unknown_wire_values() {
        assert!(close_code_is_valid(1000));
        assert!(close_code_is_valid(1011));
        assert!(!close_code_is_valid(1004));
        assert!(!close_code_is_valid(1005));
        assert!(!close_code_is_valid(1006));
        assert!(close_code_is_valid(1012));
        assert!(close_code_is_valid(1013));
        assert!(close_code_is_valid(1014));
        assert!(!close_code_is_valid(1015));
        assert!(!close_code_is_valid(2999));
        assert!(close_code_is_valid(3000));
        assert!(close_code_is_valid(4999));
        assert!(!close_code_is_valid(5000));
    }

    #[test]
    fn close_reason_is_limited_to_control_frame_payload_budget() {
        let allowed = "a".repeat(MAX_CLOSE_REASON_BYTES);
        validate_close_frame(1000, &allowed).expect("123 byte reason");

        let too_long = "a".repeat(MAX_CLOSE_REASON_BYTES + 1);
        assert!(matches!(
            validate_close_frame(1000, &too_long),
            Err(WebSocketEngineError::InvalidFrame(_))
        ));
    }

    #[test]
    fn outbound_control_messages_are_limited_to_control_frame_payload_budget() {
        validate_outbound_message(&Message::Ping(Bytes::from(vec![
            0;
            MAX_CONTROL_FRAME_PAYLOAD_BYTES
        ])))
        .expect("125 byte ping");

        assert!(matches!(
            validate_outbound_message(&Message::Ping(Bytes::from(vec![
                0;
                MAX_CONTROL_FRAME_PAYLOAD_BYTES
                    + 1
            ]))),
            Err(WebSocketEngineError::InvalidFrame(_))
        ));
        assert!(matches!(
            validate_outbound_message(&Message::Pong(Bytes::from(vec![
                0;
                MAX_CONTROL_FRAME_PAYLOAD_BYTES
                    + 1
            ]))),
            Err(WebSocketEngineError::InvalidFrame(_))
        ));
    }

    #[test]
    fn outbound_engine_frames_share_control_validation() {
        assert!(matches!(
            validate_outbound_engine_frame(&EngineFrame::Close {
                code: 1006,
                reason: String::new(),
            }),
            Err(WebSocketEngineError::InvalidCloseCode(1006))
        ));
    }

    #[test]
    fn outbound_engine_close_can_represent_no_status_ack() {
        validate_outbound_engine_frame(&EngineFrame::Close {
            code: 1005,
            reason: String::new(),
        })
        .expect("1005 is the internal empty-close sentinel for engine acks");

        assert!(matches!(
            validate_outbound_engine_frame(&EngineFrame::Close {
                code: 1005,
                reason: "not allowed without a wire code".into(),
            }),
            Err(WebSocketEngineError::InvalidCloseCode(1005))
        ));

        assert!(matches!(
            validate_outbound_message(&Message::Close {
                code: 1005,
                reason: String::new(),
            }),
            Err(WebSocketEngineError::InvalidCloseCode(1005))
        ));
    }

    #[test]
    fn kind_only_text_and_binary() {
        assert_eq!(Message::Text("a".into()).kind(), Some(MessageKind::Text));
        assert_eq!(
            Message::Binary(Bytes::from_static(b"a")).kind(),
            Some(MessageKind::Binary)
        );
        assert!(Message::Ping(Bytes::new()).kind().is_none());
        assert!(Message::Pong(Bytes::new()).kind().is_none());
        assert!(Message::Close {
            code: 1000,
            reason: String::new()
        }
        .kind()
        .is_none());
    }

    #[test]
    fn payload_len_text_and_binary() {
        assert_eq!(Message::Text("hello".into()).payload_len(), 5);
        assert_eq!(Message::Binary(Bytes::from_static(b"abc")).payload_len(), 3);
    }
}
