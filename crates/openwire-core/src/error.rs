use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;

use thiserror::Error;

pub type BoxError = Box<dyn StdError + Send + Sync>;

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

#[derive(Debug, Error)]
#[error("{kind}: {message}")]
pub struct WireError {
    kind: WireErrorKind,
    message: Cow<'static, str>,
    #[source]
    source: Option<BoxError>,
}

impl WireError {
    pub fn new(kind: WireErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source<E>(
        kind: WireErrorKind,
        message: impl Into<Cow<'static, str>>,
        source: E,
    ) -> Self
    where
        E: Into<BoxError>,
    {
        Self {
            kind,
            message: message.into(),
            source: Some(source.into()),
        }
    }

    pub fn kind(&self) -> WireErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        self.message.as_ref()
    }

    pub fn invalid_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::InvalidRequest, message)
    }

    pub fn timeout(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Timeout, message)
    }

    pub fn canceled(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Canceled, message)
    }

    pub fn dns<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Dns, message, source)
    }

    pub fn connect<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Connect, message, source)
    }

    pub fn tls<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Tls, message, source)
    }

    pub fn protocol<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Protocol, message, source)
    }

    pub fn redirect(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(WireErrorKind::Redirect, message)
    }

    pub fn body<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Body, message, source)
    }

    pub fn interceptor<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Interceptor, message, source)
    }

    pub fn internal<E>(message: impl Into<Cow<'static, str>>, source: E) -> Self
    where
        E: Into<BoxError>,
    {
        Self::with_source(WireErrorKind::Internal, message, source)
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
