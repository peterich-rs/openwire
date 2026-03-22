mod auth;
mod bridge;
mod client;
mod connection;
mod cookie;
mod policy;
mod proxy;
mod trace;
mod transport;

pub use auth::{AuthContext, AuthKind, Authenticator};
pub use client::{Call, Client, ClientBuilder};
pub use cookie::{CookieJar, Jar};
pub use openwire_core::{
    BoxFuture, BoxTaskHandle, CallContext, CallId, ConnectionId, ConnectionInfo, DnsResolver,
    EstablishmentStage, EventListener, EventListenerFactory, Exchange, Interceptor, Next,
    NoopEventListener, NoopEventListenerFactory, RequestBody, ResponseBody, Runtime, TaskHandle,
    TcpConnector, TlsConnector, TokioRuntime, WireError, WireErrorKind,
};
#[cfg(feature = "tls-rustls")]
pub use openwire_rustls::{RustlsTlsConnector, RustlsTlsConnectorBuilder};
pub use proxy::{NoProxy, Proxy};
pub use transport::{SystemDnsResolver, TokioTcpConnector};
pub use url::Url;
