use base64::Engine;
use sha1::{Digest, Sha1};

const HANDSHAKE_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub(crate) fn derive_accept(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(HANDSHAKE_MAGIC.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6455_section_1_3_example() {
        // RFC 6455 §1.3: Sec-WebSocket-Key dGhlIHNhbXBsZSBub25jZQ==
        // expected accept s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
        assert_eq!(
            derive_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
