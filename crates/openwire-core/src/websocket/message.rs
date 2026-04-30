use bytes::Bytes;

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
        assert_eq!(
            Message::Binary(Bytes::from_static(b"abc")).payload_len(),
            3
        );
    }
}
