pub(crate) fn mask_in_place(payload: &mut [u8], key: [u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= key[index % 4];
    }
}

pub(crate) fn random_mask_key() -> [u8; 4] {
    let mut key = [0u8; 4];
    getrandom::getrandom(&mut key).expect("getrandom failed");
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6455_section_5_3_example() {
        // RFC 6455 §5.3 example:
        // unmasked: 0x48, 0x65, 0x6c, 0x6c, 0x6f ("Hello")
        // mask:     0x37, 0xfa, 0x21, 0x3d
        // masked:   0x7f, 0x9f, 0x4d, 0x51, 0x58
        let mut buf = [0x48, 0x65, 0x6c, 0x6c, 0x6f];
        mask_in_place(&mut buf, [0x37, 0xfa, 0x21, 0x3d]);
        assert_eq!(buf, [0x7f, 0x9f, 0x4d, 0x51, 0x58]);
    }

    #[test]
    fn unmask_is_self_inverse() {
        let mut buf = b"openwire".to_vec();
        let key = [0xab, 0xcd, 0xef, 0x12];
        mask_in_place(&mut buf, key);
        mask_in_place(&mut buf, key);
        assert_eq!(buf, b"openwire");
    }

    #[test]
    fn random_keys_differ_across_calls() {
        let a = random_mask_key();
        let b = random_mask_key();
        // 4 bytes of entropy — collisions theoretically possible but vanishingly rare.
        assert_ne!(a, b);
    }
}
