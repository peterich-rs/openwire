mod auth;
mod bridge;
mod client;
#[cfg(feature = "compression-core")]
mod compression;
mod connection;
#[cfg(feature = "cookies")]
mod cookie;
mod logging;
mod policy;
mod proxy;
mod sync_util;
mod trace;
mod transport;

#[cfg(feature = "websocket")]
pub mod websocket;

pub use client::{Call, CallHandle, CallOptions, Client, ClientBuilder, QueuedCall};
pub use connection::{
    Address, AuthorityKey, DefaultRoutePlanner, DnsPolicy, ProtocolPolicy, ProxyConfig,
    ProxyEndpoint, ProxyMode, ProxyScheme, RequestPriority, Route, RouteFamily, RoutePlan,
    RoutePlanner, TlsIdentity, UriScheme,
};
#[cfg(feature = "cookies")]
pub use cookie::Jar;
pub use logging::{HttpLogger, LogLevel, LoggerInterceptor, StderrLogger};
pub use openwire_core::{
    AuthChallenge, AuthChallengeParam, AuthContext, AuthKind, Authenticator, BoxFuture,
    BoxTaskHandle, CallContext, CallId, Connected, Connection, ConnectionId, ConnectionInfo,
    CookieJar, DnsResolver, EstablishmentStage, EventListener, EventListenerFactory, Exchange,
    HyperExecutor, Interceptor, Next, NoopEventListener, NoopEventListenerFactory, ProxyEvent,
    RedirectContext, RedirectDecision, RedirectPolicy, RequestBody, ResponseBody, RetryContext,
    RetryPolicy, SharedTimer, TaskHandle, TcpConnector, TlsAlpnPreference, TlsConnector,
    TlsHandshake, WireError, WireErrorKind, WireExecutor,
};
#[cfg(feature = "tls-rustls")]
pub use openwire_rustls::{RustlsTlsConnector, RustlsTlsConnectorBuilder};
pub use policy::{DefaultRedirectPolicy, DefaultRetryPolicy};
pub use proxy::{NoProxy, Proxy, ProxyChoice, ProxyRules, ProxySelection, ProxySelector};
pub use url::Url;
