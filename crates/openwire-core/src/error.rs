use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use http::uri::Authority;
use http::{StatusCode, Uri};

pub type BoxError = Arc<dyn StdError + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireErrorKind {
    InvalidRequest,
    Timeout,
    Canceled,
    Dns,
    Connect,
    Tls,
    Protocol,
    Redirect,
    Body,
    Interceptor,
    Internal,
}

impl fmt::Display for WireErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::InvalidRequest => "invalid request",
            Self::Timeout => "timeout",
            Self::Canceled => "canceled",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Protocol => "protocol",
            Self::Redirect => "redirect",
            Self::Body => "body",
            Self::Interceptor => "interceptor",
            Self::Internal => "internal",
        };

        f.write_str(label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailurePhase {
    RequestValidation,
    Admission,
    Dns,
    Tcp,
    ProxyTunnel,
    Tls,
    ProtocolBinding,
    RequestExchange,
    ResponseHeaders,
    ResponseBody,
    Policy,
    Interceptor,
    Internal,
}

impl fmt::Display for FailurePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::RequestValidation => "request_validation",
            Self::Admission => "admission",
            Self::Dns => "dns",
            Self::Tcp => "tcp",
            Self::ProxyTunnel => "proxy_tunnel",
            Self::Tls => "tls",
            Self::ProtocolBinding => "protocol_binding",
            Self::RequestExchange => "request_exchange",
            Self::ResponseHeaders => "response_headers",
            Self::ResponseBody => "response_body",
            Self::Policy => "policy",
            Self::Interceptor => "interceptor",
            Self::Internal => "internal",
        };

        f.write_str(label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EstablishmentStage {
    Dns,
    Tcp,
    Tls,
    ProtocolBinding,
    ProxyTunnel,
    RouteExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EstablishmentContext {
    stage: EstablishmentStage,
    retryable: bool,
    connect_timeout: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WireErrorDiagnostics {
    authority: Option<Authority>,
    proxy_addr: Option<SocketAddr>,
    response_status: Option<StatusCode>,
    request_committed: bool,
}

impl WireErrorDiagnostics {
    pub fn authority(&self) -> Option<&Authority> {
        self.authority.as_ref()
    }

    pub fn proxy_addr(&self) -> Option<SocketAddr> {
        self.proxy_addr
    }

    pub fn response_status(&self) -> Option<StatusCode> {
        self.response_status
    }

    pub fn request_committed(&self) -> bool {
        self.request_committed
    }
}

#[derive(Debug, Clone)]
pub struct WireError {
    kind: WireErrorKind,
    phase: FailurePhase,
    message: Cow<'static, str>,
    diagnostics: WireErrorDiagnostics,
    establishment: Option<EstablishmentContext>,
    source: Option<BoxError>,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl StdError for WireError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

impl WireError {
    pub fn new(kind: WireErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            phase: default_phase(kind),
            message: message.into(),
            diagnostics: WireErrorDiagnostics::default(),
            establishment: None,
            source: None,
        }
    }

    pub fn with_source<E>(
        kind: WireErrorKind,
        message: impl Into<Cow<'static, str>>,
        source: E,
    ) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            kind,
            phase: default_phase(kind),
            message: message.into(),
            diagnostics: WireErrorDiagnostics::default(),
            establishment: None,
            source: Some(Arc::new(source)),
        }
    }

    pub fn kind(&self) -> WireErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        self.message.as_ref()
    }

    pub fn phase(&self) -> FailurePhase {
        self.phase
    }

    pub fn diagnostics(&self) -> &WireErrorDiagnostics {
        &self.diagnostics
    }

    pub fn authority(&self) -> Option<&Authority> {
        self.diagnostics.authority()
    }

    pub fn proxy_addr(&self) -> Option<SocketAddr> {
        self.diagnostics.proxy_addr()
    }

    pub fn response_status(&self) -> Option<StatusCode> {
        self.diagnostics.response_status()
    }

    pub fn request_committed(&self) -> bool {
        self.diagnostics.request_committed()
    }

    pub fn establishment_stage(&self) -> Option<EstablishmentStage> {
        self.establishment.map(|context| context.stage)
    }

    pub fn is_retryable_establishment(&self) -> bool {
        self.establishment.is_some_and(|context| context.retryable)
    }

    pub fn is_connect_timeout(&self) -> bool {
        self.establishment
            .is_some_and(|context| context.connect_timeout)
    }

    pub fn is_non_retryable_connect(&self) -> bool {
        self.establishment
            .is_some_and(|context| context.stage == EstablishmentStage::Tcp && !context.retryable)
    }

    pub fn invalid_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::InvalidRequest, message)
    }

    pub fn timeout(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Timeout, message)
    }

    pub fn body_timeout(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Timeout, message)
            .with_phase(FailurePhase::ResponseBody)
            .with_request_committed()
    }

    pub fn connect_timeout(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Timeout, message)
            .with_establishment(EstablishmentStage::Tcp, true)
            .with_connect_timeout()
    }

    pub fn canceled(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Canceled, message)
    }

    pub fn dns<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Dns, message, source)
            .with_establishment(EstablishmentStage::Dns, true)
    }

    pub fn connect<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Connect, message, source)
    }

    pub fn tcp_connect<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Connect, message, source)
            .with_establishment(EstablishmentStage::Tcp, true)
    }

    pub fn connect_non_retryable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Connect, message)
            .with_establishment(EstablishmentStage::Tcp, false)
    }

    pub fn tls<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Tls, message, source)
            .with_establishment(EstablishmentStage::Tls, true)
    }

    pub fn tls_non_retryable<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Tls, message, source)
            .with_establishment(EstablishmentStage::Tls, false)
    }

    pub fn protocol<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Protocol, message, source)
    }

    pub fn protocol_binding<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Protocol, message, source)
            .with_establishment(EstablishmentStage::ProtocolBinding, true)
    }

    pub fn proxy_tunnel<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Connect, message, source)
            .with_establishment(EstablishmentStage::ProxyTunnel, true)
    }

    pub fn proxy_tunnel_non_retryable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Connect, message)
            .with_establishment(EstablishmentStage::ProxyTunnel, false)
    }

    pub fn route_exhausted(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Connect, message)
            .with_establishment(EstablishmentStage::RouteExhausted, true)
    }

    pub fn redirect(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Redirect, message)
    }

    pub fn body<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Body, message, source).with_request_committed()
    }

    pub fn interceptor<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Interceptor, message, source)
    }

    pub fn internal<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::with_source(WireErrorKind::Internal, message, source)
    }

    pub fn with_phase(mut self, phase: FailurePhase) -> Self {
        self.phase = phase;
        self
    }

    pub fn with_authority(mut self, authority: Authority) -> Self {
        self.diagnostics.authority = Some(authority);
        self
    }

    pub fn with_authority_from_uri(mut self, uri: &Uri) -> Self {
        if let Some(authority) = uri.authority().cloned() {
            self.diagnostics.authority = Some(authority);
        }
        self
    }

    pub fn with_proxy_addr(mut self, proxy_addr: SocketAddr) -> Self {
        self.diagnostics.proxy_addr = Some(proxy_addr);
        self
    }

    pub fn with_response_status(mut self, response_status: StatusCode) -> Self {
        self.diagnostics.response_status = Some(response_status);
        self
    }

    pub fn with_request_committed(mut self) -> Self {
        self.diagnostics.request_committed = true;
        self
    }

    pub fn with_establishment(mut self, stage: EstablishmentStage, retryable: bool) -> Self {
        self.phase = phase_for_establishment(stage);
        self.establishment = Some(EstablishmentContext {
            stage,
            retryable,
            connect_timeout: false,
        });
        self
    }

    pub fn with_connect_timeout(mut self) -> Self {
        if let Some(establishment) = &mut self.establishment {
            establishment.connect_timeout = true;
        }
        self
    }
}

impl From<http::Error> for WireError {
    fn from(source: http::Error) -> Self {
        Self::with_source(
            WireErrorKind::InvalidRequest,
            "failed to build HTTP request",
            source,
        )
    }
}

impl From<http::uri::InvalidUri> for WireError {
    fn from(source: http::uri::InvalidUri) -> Self {
        Self::with_source(WireErrorKind::InvalidRequest, "invalid URI", source)
    }
}

impl From<hyper::Error> for WireError {
    fn from(source: hyper::Error) -> Self {
        if source.is_canceled() {
            return Self::with_source(WireErrorKind::Canceled, "request canceled", source);
        }

        if source.is_timeout() {
            return Self::with_source(WireErrorKind::Timeout, "request timed out", source);
        }

        Self::with_source(WireErrorKind::Protocol, "HTTP protocol error", source)
    }
}

fn default_phase(kind: WireErrorKind) -> FailurePhase {
    match kind {
        WireErrorKind::InvalidRequest => FailurePhase::RequestValidation,
        WireErrorKind::Timeout => FailurePhase::RequestExchange,
        WireErrorKind::Canceled => FailurePhase::RequestExchange,
        WireErrorKind::Dns => FailurePhase::Dns,
        WireErrorKind::Connect => FailurePhase::Tcp,
        WireErrorKind::Tls => FailurePhase::Tls,
        WireErrorKind::Protocol => FailurePhase::RequestExchange,
        WireErrorKind::Redirect => FailurePhase::Policy,
        WireErrorKind::Body => FailurePhase::ResponseBody,
        WireErrorKind::Interceptor => FailurePhase::Interceptor,
        WireErrorKind::Internal => FailurePhase::Internal,
    }
}

fn phase_for_establishment(stage: EstablishmentStage) -> FailurePhase {
    match stage {
        EstablishmentStage::Dns => FailurePhase::Dns,
        EstablishmentStage::Tcp | EstablishmentStage::RouteExhausted => FailurePhase::Tcp,
        EstablishmentStage::Tls => FailurePhase::Tls,
        EstablishmentStage::ProtocolBinding => FailurePhase::ProtocolBinding,
        EstablishmentStage::ProxyTunnel => FailurePhase::ProxyTunnel,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use http::StatusCode;

    use super::{FailurePhase, WireError};

    #[test]
    fn display_includes_underlying_source_when_present() {
        let error = WireError::connect(
            "TCP connect failed",
            io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        );

        assert_eq!(
            error.to_string(),
            "connect: TCP connect failed: connection refused"
        );
    }

    #[test]
    fn dns_errors_are_retryable_establishment_failures_with_dns_phase() {
        let error = WireError::dns(
            "DNS resolution failed",
            io::Error::new(io::ErrorKind::NotFound, "not found"),
        );

        assert_eq!(error.phase(), FailurePhase::Dns);
        assert!(error.is_retryable_establishment());
    }

    #[test]
    fn body_timeout_marks_response_body_phase_and_committed_request() {
        let error = WireError::body_timeout("body timed out").with_response_status(StatusCode::OK);

        assert_eq!(error.phase(), FailurePhase::ResponseBody);
        assert!(error.request_committed());
        assert_eq!(error.response_status(), Some(StatusCode::OK));
    }
}
