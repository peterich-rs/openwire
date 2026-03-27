use std::io::{BufReader, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::Uri;
use openwire_core::{
    BoxConnection, BoxFuture, CallContext, CoalescingInfo, Connected, Connection, ConnectionInfo,
    ConnectionIo, TlsConnector, WireError,
};
use openwire_tokio::TokioIo;
use pin_project_lite::pin_project;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
#[cfg(feature = "platform-verifier")]
use rustls_platform_verifier::BuilderVerifierExt;
use tokio_rustls::client::TlsStream;
use webpki::EndEntityCert;

#[derive(Clone)]
pub struct RustlsTlsConnector {
    config: Arc<ClientConfig>,
}

impl RustlsTlsConnector {
    pub fn builder() -> RustlsTlsConnectorBuilder {
        RustlsTlsConnectorBuilder::new()
    }

    pub fn from_config(mut config: ClientConfig) -> Self {
        ensure_default_http_alpn(&mut config);
        Self {
            config: Arc::new(config),
        }
    }
}

#[derive(Default)]
pub struct RustlsTlsConnectorBuilder {
    config: Option<ClientConfig>,
    custom_roots: Vec<CertificateDer<'static>>,
    #[cfg(feature = "platform-verifier")]
    use_platform_verifier: bool,
}

impl RustlsTlsConnectorBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            custom_roots: Vec::new(),
            #[cfg(feature = "platform-verifier")]
            use_platform_verifier: true,
        }
    }

    pub fn with_client_config(mut self, config: ClientConfig) -> Self {
        self.config = Some(config);
        self
    }

    #[cfg(feature = "platform-verifier")]
    pub fn with_platform_verifier(mut self, enabled: bool) -> Self {
        self.use_platform_verifier = enabled;
        self
    }

    pub fn add_root_certificates_pem(mut self, pem: impl AsRef<[u8]>) -> Result<Self, WireError> {
        let mut reader = BufReader::new(Cursor::new(pem.as_ref()));
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert
                .map_err(|error| WireError::tls("failed to parse PEM root certificate", error))?;
            self.custom_roots.push(cert);
        }
        Ok(self)
    }

    pub fn build(self) -> Result<RustlsTlsConnector, WireError> {
        if let Some(config) = self.config {
            return Ok(RustlsTlsConnector::from_config(config));
        }

        #[cfg(feature = "platform-verifier")]
        if self.custom_roots.is_empty() && self.use_platform_verifier {
            let mut config = ClientConfig::builder()
                .with_platform_verifier()
                .map_err(|error| WireError::tls("failed to initialize platform verifier", error))?
                .with_no_client_auth();
            ensure_default_http_alpn(&mut config);
            return Ok(RustlsTlsConnector::from_config(config));
        }

        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for error in native.errors {
            tracing::debug!(?error, "native root load error");
        }
        let mut native_root_failures = 0usize;
        for cert in native.certs {
            if roots.add(cert).is_err() {
                native_root_failures += 1;
            }
        }
        if native_root_failures > 0 {
            tracing::warn!(
                failed = native_root_failures,
                "some native root certificates failed to load"
            );
        }
        for cert in self.custom_roots {
            roots
                .add(cert)
                .map_err(|error| WireError::tls("failed to add custom root certificate", error))?;
        }

        if roots.is_empty() {
            return Err(WireError::tls(
                "no root certificates were loaded",
                std::io::Error::new(std::io::ErrorKind::NotFound, "empty root store"),
            ));
        }

        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        ensure_default_http_alpn(&mut config);
        Ok(RustlsTlsConnector::from_config(config))
    }
}

fn ensure_default_http_alpn(config: &mut ClientConfig) {
    if config.alpn_protocols.is_empty() {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    }
}

impl TlsConnector for RustlsTlsConnector {
    fn connect(
        &self,
        ctx: CallContext,
        uri: Uri,
        stream: BoxConnection,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let config = self.config.clone();
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| WireError::invalid_request("HTTPS request is missing a host"))?
                .to_owned();

            ctx.listener().tls_start(&ctx, &host);

            let server_name = ServerName::try_from(host.clone()).map_err(|error| {
                WireError::with_source(
                    openwire_core::WireErrorKind::InvalidRequest,
                    format!("invalid TLS server name: {host}"),
                    error,
                )
                .with_authority_from_uri(&uri)
            })?;
            let connection_info = connection_info_from_stream(&*stream);
            let connector = tokio_rustls::TlsConnector::from(config);

            let tls_stream = match connector.connect(server_name, TokioIo::new(stream)).await {
                Ok(stream) => stream,
                Err(error) => {
                    let error = classify_tls_handshake_error(error).with_authority_from_uri(&uri);
                    ctx.listener().tls_failed(&ctx, &host, &error);
                    return Err(error);
                }
            };

            let negotiated_h2 = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .map(|protocol| protocol == b"h2")
                .unwrap_or(false);
            let coalescing = coalescing_info_from_session(tls_stream.get_ref().1);

            ctx.listener().tls_end(&ctx, &host);

            Ok(Box::new(RustlsConnection {
                inner: TokioIo::new(tls_stream),
                info: ConnectionInfo {
                    tls: true,
                    ..connection_info
                },
                coalescing,
                negotiated_h2,
            }) as BoxConnection)
        })
    }
}

pin_project! {
    struct RustlsConnection {
        #[pin]
        inner: TokioIo<TlsStream<TokioIo<BoxConnection>>>,
        info: ConnectionInfo,
        coalescing: CoalescingInfo,
        negotiated_h2: bool,
    }
}

impl Connection for RustlsConnection {
    fn connected(&self) -> Connected {
        build_connected(
            self.info.clone(),
            self.coalescing.clone(),
            self.inner
                .inner()
                .get_ref()
                .0
                .inner()
                .connected()
                .is_proxied(),
            self.negotiated_h2,
        )
    }
}

impl hyper::rt::Read for RustlsConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for RustlsConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

fn classify_tls_handshake_error(error: std::io::Error) -> WireError {
    let non_retryable = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<rustls::Error>())
        .is_some_and(|error| {
            matches!(
                error,
                rustls::Error::InvalidCertificate(_) | rustls::Error::PeerIncompatible(_)
            )
        });

    if non_retryable {
        WireError::tls_non_retryable("TLS handshake failed", error)
    } else {
        WireError::tls("TLS handshake failed", error)
    }
}

fn build_connected(
    info: ConnectionInfo,
    coalescing: CoalescingInfo,
    inner_proxied: bool,
    negotiated_h2: bool,
) -> Connected {
    Connected::new()
        .info(info)
        .coalescing(coalescing)
        .proxy(inner_proxied)
        .negotiated_h2(negotiated_h2)
}

fn connection_info_from_stream(stream: &dyn ConnectionIo) -> ConnectionInfo {
    stream.connected().connection_info_or_default()
}

fn coalescing_info_from_session(session: &rustls::ClientConnection) -> CoalescingInfo {
    let Some(end_entity) = session.peer_certificates().and_then(|certs| certs.first()) else {
        return CoalescingInfo::default();
    };

    let parsed = match EndEntityCert::try_from(end_entity) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::debug!(
                ?error,
                "failed to parse peer certificate for coalescing metadata"
            );
            return CoalescingInfo::default();
        }
    };

    CoalescingInfo::new(
        parsed
            .valid_dns_names()
            .map(|name| name.to_ascii_lowercase())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_injects_default_http_alpn_when_missing() {
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoopVerifier))
            .with_no_client_auth();

        let connector = RustlsTlsConnector::from_config(config);
        assert_eq!(
            connector.config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn from_config_preserves_explicit_alpn_configuration() {
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoopVerifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let connector = RustlsTlsConnector::from_config(config);
        assert_eq!(connector.config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn build_connected_preserves_inner_proxy_flag() {
        let info = test_connection_info();
        let coalescing = CoalescingInfo::new(vec!["example.com".to_owned()]);

        let connected = build_connected(info.clone(), coalescing.clone(), true, false);

        assert!(connected.is_proxied());
        assert!(!connected.is_negotiated_h2());
        let connected_info = connected.connection_info().expect("connection info");
        assert_eq!(connected_info.id, info.id);
        assert_eq!(connected_info.remote_addr, info.remote_addr);
        assert_eq!(connected_info.local_addr, info.local_addr);
        assert_eq!(connected_info.tls, info.tls);
        assert_eq!(connected.coalescing_info(), &coalescing);
    }

    #[test]
    fn build_connected_marks_negotiated_h2_when_requested() {
        let connected = build_connected(
            test_connection_info(),
            CoalescingInfo::default(),
            false,
            true,
        );

        assert!(connected.is_negotiated_h2());
    }

    #[test]
    fn build_connected_leaves_negotiated_h2_false_without_h2() {
        let connected = build_connected(
            test_connection_info(),
            CoalescingInfo::default(),
            false,
            false,
        );

        assert!(!connected.is_negotiated_h2());
    }

    fn test_connection_info() -> ConnectionInfo {
        ConnectionInfo {
            id: openwire_core::next_connection_id(),
            remote_addr: Some(([192, 0, 2, 10], 443).into()),
            local_addr: Some(([192, 0, 2, 20], 50000).into()),
            tls: true,
        }
    }

    #[derive(Debug)]
    struct NoopVerifier;

    impl rustls::client::danger::ServerCertVerifier for NoopVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA256,
            ]
        }
    }
}
