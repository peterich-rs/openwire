use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{SharedEventListener, SharedEventListenerFactory};

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
}

impl std::fmt::Debug for CallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallContext")
            .field("call_id", &self.call_id())
            .field("created_at", &self.created_at())
            .field("deadline", &self.deadline())
            .finish()
    }
}

impl CallContext {
    pub fn new(listener: SharedEventListener, deadline: Option<Duration>) -> Self {
        let created_at = Instant::now();
        let deadline = deadline.map(|duration| created_at + duration);
        Self {
            inner: Arc::new(CallContextInner {
                call_id: CallId(NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)),
                listener,
                created_at,
                deadline,
                connection_established: AtomicBool::new(false),
            }),
        }
    }

    pub fn from_factory(
        factory: &SharedEventListenerFactory,
        request: &http::Request<crate::RequestBody>,
        deadline: Option<Duration>,
    ) -> Self {
        let listener = factory.create(request);
        Self::new(listener, deadline)
    }

    pub fn call_id(&self) -> CallId {
        self.inner.call_id
    }

    pub fn listener(&self) -> &SharedEventListener {
        &self.inner.listener
    }

    pub fn created_at(&self) -> Instant {
        self.inner.created_at
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.inner.deadline
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
