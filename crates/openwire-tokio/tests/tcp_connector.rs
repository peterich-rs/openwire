use openwire_core::TcpConnector;
use openwire_tokio::{ConfiguredTokioTcpConnector, TokioTcpConnector};

#[test]
fn released_unit_connector_can_be_constructed_and_destructured() {
    const CONNECTOR: TokioTcpConnector = TokioTcpConnector;
    let _connector: &dyn TcpConnector = &CONNECTOR;
    let TokioTcpConnector = CONNECTOR;
    assert!(CONNECTOR.config().nodelay);

    let configured: ConfiguredTokioTcpConnector = TokioTcpConnector.nodelay(false);
    assert!(!configured.config().nodelay);
}
