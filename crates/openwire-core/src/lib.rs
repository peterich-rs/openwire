mod body;
mod context;
mod error;
mod event;
mod interceptor;
mod runtime;
mod transport;

use std::future::Future;
use std::pin::Pin;

pub use body::{RequestBody, ResponseBody};
pub use context::{next_connection_id, CallContext, CallId, ConnectionId};
pub use error::{BoxError, EstablishmentStage, WireError, WireErrorKind};
pub use event::{
    EventListener, EventListenerFactory, NoopEventListener, NoopEventListenerFactory,
    SharedEventListener, SharedEventListenerFactory,
};
pub use interceptor::{
    BoxWireService, Exchange, Interceptor, InterceptorLayer, Next, SharedInterceptor, WireResponse,
};
pub use runtime::{BoxTaskHandle, Runtime, TaskHandle};
pub use transport::{
    BoxConnection, CoalescingInfo, ConnectionInfo, ConnectionIo, DnsResolver, TcpConnector,
    TlsConnector,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
