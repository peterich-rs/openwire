mod auth;
mod body;
mod context;
mod cookie;
mod error;
mod event;
mod interceptor;
mod policy;
mod runtime;
mod transport;

#[cfg(feature = "websocket")]
pub mod websocket;

use std::future::Future;
use std::pin::Pin;

pub use auth::{AuthContext, AuthKind, Authenticator};
pub use body::{RequestBody, ResponseBody};
pub use context::{next_connection_id, CallContext, CallId, ConnectionId};
pub use cookie::CookieJar;
pub use error::{
    BoxError, EstablishmentStage, FailurePhase, WireError, WireErrorDiagnostics, WireErrorKind,
};
pub use event::{
    EventListener, EventListenerFactory, NoopEventListener, NoopEventListenerFactory,
    SharedEventListener, SharedEventListenerFactory,
};
pub use interceptor::{
    BoxWireService, Exchange, Interceptor, InterceptorLayer, Next, SharedInterceptor, WireResponse,
};
pub use policy::{RedirectContext, RedirectDecision, RedirectPolicy, RetryContext, RetryPolicy};
pub use runtime::{BoxTaskHandle, HyperExecutor, SharedTimer, TaskHandle, WireExecutor};
pub use transport::{
    BoxConnection, BoxDnsService, BoxTcpService, BoxTlsService, CoalescingInfo, Connected,
    Connection, ConnectionInfo, ConnectionIo, DnsRequest, DnsResolver, DnsResolverService,
    TcpConnectRequest, TcpConnector, TcpConnectorService, TlsConnectRequest, TlsConnector,
    TlsConnectorService, TowerDnsResolver, TowerTcpConnector, TowerTlsConnector,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
