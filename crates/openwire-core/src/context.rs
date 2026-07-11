use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{SharedEventListener, SharedEventListenerFactory, TlsAlpnPreference};

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallId(u64);

impl CallId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub fn next_connection_id() -> ConnectionId {
    ConnectionId(NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone)]
pub struct CallContext {
    inner: Arc<CallContextInner>,
}

struct CallContextInner {
    call_id: CallId,
    listener: SharedEventListener,
    created_at: Instant,
    deadline: Option<Instant>,
    connection_established: AtomicBool,
    /// When set, response-body Drop discards the connection (decode/body errors).
    body_force_discard: AtomicBool,
    tls_alpn_preference: TlsAlpnPreference,
}

impl std::fmt::Debug for CallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallContext")
            .field("call_id", &self.call_id())
            .field("created_at", &self.created_at())
            .field("deadline", &self.deadline())
            .field("tls_alpn_preference", &self.tls_alpn_preference())
            .finish()
    }
}

impl CallContext {
    pub fn new(listener: SharedEventListener, deadline: Option<Duration>) -> Self {
        Self::with_tls_alpn_preference(listener, deadline, TlsAlpnPreference::Auto)
    }

    fn with_tls_alpn_preference(
        listener: SharedEventListener,
        deadline: Option<Duration>,
        tls_alpn_preference: TlsAlpnPreference,
    ) -> Self {
        let created_at = Instant::now();
        let deadline = deadline.map(|duration| created_at + duration);
        Self {
            inner: Arc::new(CallContextInner {
                call_id: CallId(NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)),
                listener,
                created_at,
                deadline,
                connection_established: AtomicBool::new(false),
                body_force_discard: AtomicBool::new(false),
                tls_alpn_preference,
            }),
        }
    }

    pub fn from_factory(
        factory: &SharedEventListenerFactory,
        request: &http::Request<crate::RequestBody>,
        deadline: Option<Duration>,
    ) -> Self {
        let listener = factory.create(request);
        let tls_alpn_preference = request
            .extensions()
            .get::<TlsAlpnPreference>()
            .copied()
            .unwrap_or_default();
        Self::with_tls_alpn_preference(listener, deadline, tls_alpn_preference)
    }

    pub fn call_id(&self) -> CallId {
        self.inner.call_id
    }

    pub fn listener(&self) -> &SharedEventListener {
        &self.inner.listener
    }

    pub fn mark_body_force_discard(&self) {
        self.inner
            .body_force_discard
            .store(true, Ordering::Release);
    }

    pub fn body_force_discard(&self) -> bool {
        self.inner.body_force_discard.load(Ordering::Acquire)
    }

    pub fn created_at(&self) -> Instant {
        self.inner.created_at
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.inner.deadline
    }

    pub fn tls_alpn_preference(&self) -> TlsAlpnPreference {
        self.inner.tls_alpn_preference
    }

    pub fn mark_connection_established(&self) {
        self.inner
            .connection_established
            .store(true, Ordering::Relaxed);
    }

    pub fn connection_established(&self) -> bool {
        self.inner.connection_established.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{NoopEventListenerFactory, RequestBody, SharedEventListenerFactory};

    #[test]
    fn from_factory_defaults_tls_alpn_preference_to_auto() {
        let factory = Arc::new(NoopEventListenerFactory) as SharedEventListenerFactory;
        let request = http::Request::builder()
            .uri("https://example.com/")
            .body(RequestBody::empty())
            .expect("request");

        let ctx = CallContext::from_factory(&factory, &request, None);

        assert_eq!(ctx.tls_alpn_preference(), TlsAlpnPreference::Auto);
    }

    #[test]
    fn from_factory_preserves_tls_alpn_preference_from_request_extensions() {
        let factory = Arc::new(NoopEventListenerFactory) as SharedEventListenerFactory;
        let mut request = http::Request::builder()
            .uri("https://example.com/")
            .body(RequestBody::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(TlsAlpnPreference::Http1Only);

        let ctx = CallContext::from_factory(&factory, &request, None);

        assert_eq!(ctx.tls_alpn_preference(), TlsAlpnPreference::Http1Only);
    }
}
