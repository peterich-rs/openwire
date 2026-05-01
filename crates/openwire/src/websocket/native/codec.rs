use bytes::{Buf, BufMut, BytesMut};

use openwire_core::websocket::WebSocketEngineError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Opcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl Opcode {
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b & 0x0F {
            0x0 => Some(Self::Continuation),
            0x1 => Some(Self::Text),
            0x2 => Some(Self::Binary),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xA => Some(Self::Pong),
            _ => None,
        }
    }

    pub(crate) fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// RFC 6455 §7.4 close codes accepted on the wire. The 1004/1005/1006 codes
/// are reserved for in-process signaling and must not appear in close frames.
pub(crate) fn close_code_is_valid(code: u16) -> bool {
    matches!(
        code,
        1000 | 1001 | 1002 | 1003 | 1007 | 1008 | 1009 | 1010 | 1011 | 3000..=4999
    )
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameHeader {
    pub fin: bool,
    pub opcode: Opcode,
}

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: bytes::Bytes,
}

pub(crate) fn encode_frame(
    out: &mut BytesMut,
    header: FrameHeader,
    payload: &[u8],
    mask: Option<[u8; 4]>,
) {
    let b0 = if header.fin { 0x80 } else { 0x00 } | (header.opcode as u8);
    out.put_u8(b0);

    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
    let len = payload.len();
    if len <= 125 {
        out.put_u8(mask_bit | len as u8);
    } else if len <= u16::MAX as usize {
        out.put_u8(mask_bit | 126);
        out.put_u16(len as u16);
    } else {
        out.put_u8(mask_bit | 127);
        out.put_u64(len as u64);
    }

    if let Some(key) = mask {
        out.put_slice(&key);
        let start = out.len();
        out.put_slice(payload);
        super::mask::mask_in_place(&mut out[start..], key);
    } else {
        out.put_slice(payload);
    }
}

pub(crate) fn decode_frame(
    buf: &mut BytesMut,
    max_frame_size: usize,
) -> Result<Option<DecodedFrame>, WebSocketEngineError> {
    if buf.len() < 2 {
        return Ok(None);
    }

    let b0 = buf[0];
    let b1 = buf[1];
    let fin = (b0 & 0x80) != 0;
    let rsv = b0 & 0x70;
    if rsv != 0 {
        return Err(WebSocketEngineError::InvalidFrame(
            "reserved bits set".into(),
        ));
    }
    let opcode = Opcode::from_byte(b0)
        .ok_or_else(|| WebSocketEngineError::InvalidFrame("unknown opcode".into()))?;

    let mask_bit = (b1 & 0x80) != 0;
    if mask_bit {
        return Err(WebSocketEngineError::InvalidFrame(
            "server frames must not be masked".into(),
        ));
    }

    if opcode.is_control() && !fin {
        return Err(WebSocketEngineError::InvalidFrame(
            "fragmented control frame".into(),
        ));
    }

    let len_field = b1 & 0x7F;
    let (header_len, payload_len) = match len_field {
        0..=125 => (2usize, len_field as usize),
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (4, u16::from_be_bytes([buf[2], buf[3]]) as usize)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut v = [0u8; 8];
            v.copy_from_slice(&buf[2..10]);
            (10, u64::from_be_bytes(v) as usize)
        }
        _ => unreachable!("len_field is at most 7 bits"),
    };

    if opcode.is_control() && payload_len > 125 {
        return Err(WebSocketEngineError::InvalidFrame(
            "control frame > 125 bytes".into(),
        ));
    }
    if payload_len > max_frame_size {
        return Err(WebSocketEngineError::PayloadTooLarge {
            limit: max_frame_size,
            received: payload_len,
        });
    }

    if buf.len() < header_len + payload_len {
        return Ok(None);
    }
    buf.advance(header_len);
    let payload = buf.split_to(payload_len).freeze();

    Ok(Some(DecodedFrame {
        fin,
        opcode,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_round_trip() {
        for op in [
            Opcode::Continuation,
            Opcode::Text,
            Opcode::Binary,
            Opcode::Close,
            Opcode::Ping,
            Opcode::Pong,
        ] {
            assert_eq!(Opcode::from_byte(op as u8), Some(op));
        }
    }

    #[test]
    fn close_code_rejects_reserved_and_unknown() {
        assert!(close_code_is_valid(1000));
        assert!(!close_code_is_valid(1004));
        assert!(!close_code_is_valid(2999));
        assert!(close_code_is_valid(3000));
        assert!(!close_code_is_valid(5000));
    }
}

#[cfg(test)]
mod encode_tests {
    use super::*;
    use bytes::BytesMut;

    fn encode(opcode: Opcode, payload: &[u8], fin: bool, mask_key: [u8; 4]) -> Vec<u8> {
        let mut out = BytesMut::new();
        encode_frame(
            &mut out,
            FrameHeader { fin, opcode },
            payload,
            Some(mask_key),
        );
        out.to_vec()
    }

    #[test]
    fn encodes_short_text_frame() {
        let bytes = encode(Opcode::Text, b"hello", true, [1, 2, 3, 4]);
        assert_eq!(bytes[0], 0x81);
        assert_eq!(bytes[1], 0x80 | 5);
        assert_eq!(&bytes[2..6], &[1, 2, 3, 4]);

        let mut expected = b"hello".to_vec();
        super::super::mask::mask_in_place(&mut expected, [1, 2, 3, 4]);
        assert_eq!(&bytes[6..], &expected[..]);
    }

    #[test]
    fn uses_16bit_length_for_payload_126() {
        let payload = vec![0u8; 126];
        let bytes = encode(Opcode::Binary, &payload, true, [0; 4]);
        assert_eq!(bytes[1], 0x80 | 126);
        assert_eq!(&bytes[2..4], &[0x00, 0x7E]);
    }

    #[test]
    fn uses_64bit_length_for_payload_65536() {
        let payload = vec![0u8; 65536];
        let bytes = encode(Opcode::Binary, &payload, true, [0; 4]);
        assert_eq!(bytes[1], 0x80 | 127);
        assert_eq!(&bytes[2..10], &[0, 0, 0, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn unmasked_encode_omits_mask_key() {
        let mut out = BytesMut::new();
        encode_frame(
            &mut out,
            FrameHeader {
                fin: true,
                opcode: Opcode::Text,
            },
            b"hi",
            None,
        );
        assert_eq!(&out[..], &[0x81, 0x02, b'h', b'i']);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn decodes_short_text_frame() {
        let mut buf = BytesMut::from(&[0x81u8, 0x05, b'h', b'e', b'l', b'l', b'o'][..]);
        let frame = decode_frame(&mut buf, 1024).unwrap().expect("frame ready");
        assert!(frame.fin);
        assert_eq!(frame.opcode, Opcode::Text);
        assert_eq!(frame.payload.as_ref(), b"hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn returns_none_when_partial() {
        let mut buf = BytesMut::from(&[0x81u8][..]);
        assert!(decode_frame(&mut buf, 1024).unwrap().is_none());
    }

    #[test]
    fn rejects_masked_server_frame() {
        let mut buf = BytesMut::from(&[0x81u8, 0x85, 1, 2, 3, 4, b'h', b'e', b'l', b'l', b'o'][..]);
        let err = decode_frame(&mut buf, 1024).unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }

    #[test]
    fn enforces_max_frame_size() {
        let mut buf = BytesMut::from(&[0x82u8, 126, 0, 200][..]);
        buf.extend_from_slice(&[0u8; 200]);
        let err = decode_frame(&mut buf, 100).unwrap_err();
        assert!(matches!(err, WebSocketEngineError::PayloadTooLarge { .. }));
    }

    #[test]
    fn rejects_fragmented_control_frame() {
        let mut buf = BytesMut::from(&[0x09u8, 0x00][..]);
        let err = decode_frame(&mut buf, 1024).unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }

    #[test]
    fn rejects_oversized_control_frame() {
        let mut buf = BytesMut::from(&[0x89u8, 126, 0, 200][..]);
        buf.extend_from_slice(&[0u8; 200]);
        let err = decode_frame(&mut buf, 1024).unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }

    #[test]
    fn rejects_reserved_bits() {
        let mut buf = BytesMut::from(&[0x91u8, 0x00][..]);
        let err = decode_frame(&mut buf, 1024).unwrap_err();
        assert!(matches!(err, WebSocketEngineError::InvalidFrame(_)));
    }
}
