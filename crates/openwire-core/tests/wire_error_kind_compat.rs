use openwire_core::WireErrorKind;

#[test]
fn downstream_can_still_match_all_v0_1_2_error_kinds() {
    fn is_internal(kind: WireErrorKind) -> bool {
        match kind {
            WireErrorKind::Internal => true,
            WireErrorKind::InvalidRequest
            | WireErrorKind::Timeout
            | WireErrorKind::Canceled
            | WireErrorKind::Dns
            | WireErrorKind::Connect
            | WireErrorKind::Tls
            | WireErrorKind::Protocol
            | WireErrorKind::Redirect
            | WireErrorKind::Body
            | WireErrorKind::Interceptor => false,
        }
    }

    assert!(is_internal(WireErrorKind::Internal));
    assert!(!is_internal(WireErrorKind::Timeout));
}
