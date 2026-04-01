mod auth;
mod bridge;
mod client;
mod connection;
mod cookie;
mod policy;
mod proxy;
mod sync_util;
mod trace;
mod transport;

pub use client::{Call, CallOptions, Client, ClientBuilder};
pub use connection::{
    Address, AuthorityKey, DefaultRoutePlanner, DnsPolicy, ProtocolPolicy, ProxyConfig,
    ProxyEndpoint, ProxyMode, ProxyScheme, Route, RouteFamily, RoutePlan, RoutePlanner,
    TlsIdentity, UriScheme,
};
pub use cookie::Jar;
pub use openwire_core::{
    AuthContext, AuthKind, Authenticator, BoxFuture, BoxTaskHandle, CallContext, CallId, Connected,
    Connection, ConnectionId, ConnectionInfo, CookieJar, DnsResolver, EstablishmentStage,
    EventListener, EventListenerFactory, Exchange, HyperExecutor, Interceptor, Next,
    NoopEventListener, NoopEventListenerFactory, RedirectContext, RedirectDecision, RedirectPolicy,
    RequestBody, ResponseBody, RetryContext, RetryPolicy, SharedTimer, TaskHandle, TcpConnector,
    TlsConnector, WireError, WireErrorKind, WireExecutor,
};
#[cfg(feature = "tls-rustls")]
pub use openwire_rustls::{RustlsTlsConnector, RustlsTlsConnectorBuilder};
pub use policy::{DefaultRedirectPolicy, DefaultRetryPolicy};
pub use proxy::{NoProxy, Proxy};
pub use url::Url;
