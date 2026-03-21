mod body;
mod context;
mod error;
mod event;
mod interceptor;
mod runtime;
mod tokio_rt;
mod transport;

use std::future::Future;
use std::pin::Pin;

pub use body::{RequestBody, ResponseBody};
pub use context::{next_connection_id, CallContext, CallId, ConnectionId};
pub use error::{BoxError, WireError, WireErrorKind};
pub use event::{
    EventListener, EventListenerFactory, NoopEventListener, NoopEventListenerFactory,
    SharedEventListener, SharedEventListenerFactory,
};
pub use interceptor::{
    BoxWireService, Exchange, Interceptor, InterceptorLayer, Next, SharedInterceptor, WireResponse,
};
pub use runtime::Runtime;
pub use tokio_rt::{TokioExecutor, TokioIo, TokioRuntime, TokioTimer};
pub use transport::{
    BoxConnection, ConnectionInfo, ConnectionIo, DnsResolver, TcpConnector, TlsConnector,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
