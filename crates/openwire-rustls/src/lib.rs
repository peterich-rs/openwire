use std::io::{BufReader, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use openwire_core::{
    BoxConnection, BoxFuture, CallContext, ConnectionInfo, TlsConnector, WireError,
};
use pin_project_lite::pin_project;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
#[cfg(feature = "platform-verifier")]
use rustls_platform_verifier::BuilderVerifierExt;
use tokio_rustls::client::TlsStream;

#[derive(Clone)]
pub struct RustlsTlsConnector {
    config: Arc<ClientConfig>,
}

impl RustlsTlsConnector {
    pub fn builder() -> RustlsTlsConnectorBuilder {
        RustlsTlsConnectorBuilder::new()
    }

    pub fn from_config(config: ClientConfig) -> Self {
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
            let config = ClientConfig::builder()
                .with_platform_verifier()
                .map_err(|error| WireError::tls("failed to initialize platform verifier", error))?
                .with_no_client_auth();
            return Ok(RustlsTlsConnector::from_config(config));
        }

        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for error in native.errors {
            tracing::debug!(?error, "native root load error");
        }
        for cert in native.certs {
            let _ = roots.add(cert);
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

        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(RustlsTlsConnector::from_config(config))
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

            let server_name = ServerName::try_from(host.clone())
                .map_err(|_| WireError::invalid_request("invalid TLS server name"))?;
            let connection_info = connection_info_from_stream(&*stream);
            let connector = tokio_rustls::TlsConnector::from(config);

            let tls_stream = match connector.connect(server_name, TokioIo::new(stream)).await {
                Ok(stream) => stream,
                Err(error) => {
                    let error = WireError::tls("TLS handshake failed", error);
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

            ctx.listener().tls_end(&ctx, &host);

            Ok(Box::new(RustlsConnection {
                inner: TokioIo::new(tls_stream),
                info: ConnectionInfo {
                    tls: true,
                    ..connection_info
                },
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
        negotiated_h2: bool,
    }
}

impl Connection for RustlsConnection {
    fn connected(&self) -> Connected {
        let connected = Connected::new().extra(self.info.clone());
        if self.negotiated_h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
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

fn connection_info_from_stream(stream: &dyn openwire_core::ConnectionIo) -> ConnectionInfo {
    let mut extensions = http::Extensions::new();
    stream.connected().get_extras(&mut extensions);
    extensions
        .remove::<ConnectionInfo>()
        .unwrap_or(ConnectionInfo {
            id: openwire_core::next_connection_id(),
            remote_addr: None,
            local_addr: None,
            tls: false,
        })
}
