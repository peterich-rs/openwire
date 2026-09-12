use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, Weak};

use hyper::client::conn::{http1, http2};
use openwire_core::{BoxTaskHandle, ConnectionId, ConnectionInfo, RequestBody};
use parking_lot::Mutex;

use crate::connection::{
    fx_hash_map, ConnectionAvailability, ExchangeFinder, FxHashMap, RealConnection,
};
use crate::sync_util::lock_mutex;

const CONNECTION_BINDING_SHARDS: usize = 32;

#[derive(Clone)]
pub(super) struct ConnectionBindings {
    shards: Arc<[Mutex<FxHashMap<ConnectionId, ConnectionBinding>>]>,
}

enum ConnectionBinding {
    Http1(Http1Binding),
    Http2(Http2Binding),
}

struct Http1Binding {
    info: ConnectionInfo,
    sender: Option<http1::SendRequest<RequestBody>>,
}

struct Http2Binding {
    info: ConnectionInfo,
    sender: http2::SendRequest<RequestBody>,
}

pub(super) enum AcquiredBinding {
    Http1 {
        info: ConnectionInfo,
        sender: http1::SendRequest<RequestBody>,
    },
    Http2 {
        info: ConnectionInfo,
        sender: http2::SendRequest<RequestBody>,
    },
}

pub(super) enum BindingAcquireResult {
    Acquired(AcquiredBinding),
    Busy,
    Stale,
}

impl ConnectionBindings {
    fn shard(
        &self,
        connection_id: ConnectionId,
    ) -> &Mutex<FxHashMap<ConnectionId, ConnectionBinding>> {
        &self.shards[(connection_id.as_u64() as usize) % self.shards.len()]
    }

    pub(super) fn insert_http1(
        &self,
        connection_id: ConnectionId,
        info: ConnectionInfo,
        sender: http1::SendRequest<RequestBody>,
    ) {
        self.shard(connection_id).lock().insert(
            connection_id,
            ConnectionBinding::Http1(Http1Binding {
                info,
                sender: Some(sender),
            }),
        );
    }

    pub(super) fn insert_http2(
        &self,
        connection_id: ConnectionId,
        info: ConnectionInfo,
        sender: http2::SendRequest<RequestBody>,
    ) {
        self.shard(connection_id).lock().insert(
            connection_id,
            ConnectionBinding::Http2(Http2Binding { info, sender }),
        );
    }

    pub(super) fn acquire(&self, connection_id: ConnectionId) -> BindingAcquireResult {
        let mut bindings = self.shard(connection_id).lock();
        let mut remove_stale = false;
        let acquired = match bindings.get_mut(&connection_id) {
            Some(ConnectionBinding::Http1(binding)) => {
                // Exclusive checkout: a second HTTP/1 exchange must wait or
                // open another connection. This is the protocol lock, not a
                // per-host quota.
                let Some(sender) = binding.sender.take() else {
                    return BindingAcquireResult::Busy;
                };
                if sender.is_closed() {
                    remove_stale = true;
                    BindingAcquireResult::Stale
                } else {
                    BindingAcquireResult::Acquired(AcquiredBinding::Http1 {
                        info: binding.info.clone(),
                        sender,
                    })
                }
            }
            Some(ConnectionBinding::Http2(binding)) => {
                if binding.sender.is_closed() {
                    remove_stale = true;
                    BindingAcquireResult::Stale
                } else {
                    BindingAcquireResult::Acquired(AcquiredBinding::Http2 {
                        info: binding.info.clone(),
                        sender: binding.sender.clone(),
                    })
                }
            }
            None => BindingAcquireResult::Stale,
        };
        if remove_stale {
            bindings.remove(&connection_id);
        }
        acquired
    }

    pub(super) fn release_http1(
        &self,
        connection_id: ConnectionId,
        sender: http1::SendRequest<RequestBody>,
    ) -> bool {
        if sender.is_closed() {
            self.remove(connection_id);
            return false;
        }

        let mut bindings = self.shard(connection_id).lock();
        let Some(ConnectionBinding::Http1(binding)) = bindings.get_mut(&connection_id) else {
            return false;
        };
        debug_assert!(
            binding.sender.is_none(),
            "HTTP/1 sender should be checked out"
        );
        binding.sender = Some(sender);
        true
    }

    pub(super) fn remove(&self, connection_id: ConnectionId) {
        self.shard(connection_id).lock().remove(&connection_id);
    }
}

impl Default for ConnectionBindings {
    fn default() -> Self {
        let shards = (0..CONNECTION_BINDING_SHARDS)
            .map(|_| Mutex::new(fx_hash_map()))
            .collect::<Vec<_>>();
        Self {
            shards: Arc::<[Mutex<FxHashMap<ConnectionId, ConnectionBinding>>]>::from(shards),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct ConnectionTaskRegistry {
    inner: Arc<ConnectionTaskRegistryInner>,
}

#[derive(Default)]
pub(super) struct ConnectionTaskRegistryInner {
    handles_by_connection: StdMutex<HashMap<ConnectionId, Option<BoxTaskHandle>>>,
}

impl ConnectionTaskRegistry {
    pub(super) fn attach_connection(&self, connection_id: ConnectionId, handle: BoxTaskHandle) {
        let mut handles = lock_mutex(&self.inner.handles_by_connection);
        if let Some(Some(previous)) = handles.insert(connection_id, Some(handle)) {
            previous.abort();
        }
    }

    pub(super) fn abort_connection(&self, connection_id: ConnectionId) {
        if let Some(Some(handle)) =
            lock_mutex(&self.inner.handles_by_connection).remove(&connection_id)
        {
            handle.abort();
        }
    }

    pub(super) fn complete_connection_weak(
        inner: &Weak<ConnectionTaskRegistryInner>,
        connection_id: ConnectionId,
    ) {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        lock_mutex(&inner.handles_by_connection).remove(&connection_id);
    }

    pub(super) fn downgrade(&self) -> Weak<ConnectionTaskRegistryInner> {
        Arc::downgrade(&self.inner)
    }

    pub(super) fn teardown_connection(
        &self,
        bindings: &ConnectionBindings,
        connection_id: ConnectionId,
    ) {
        bindings.remove(connection_id);
        self.abort_connection(connection_id);
    }

    #[cfg(test)]
    pub(super) fn poison_handles_for_test(&self) {
        let _guard = self
            .inner
            .handles_by_connection
            .lock()
            .expect("poison connection task registry lock for test");
        panic!("poison connection task registry");
    }
}

impl Drop for ConnectionTaskRegistryInner {
    fn drop(&mut self) {
        let handles = lock_mutex(&self.handles_by_connection);
        for handle in handles.values().filter_map(Option::as_ref) {
            handle.abort();
        }
    }
}

pub(super) fn release_acquired_connection(
    exchange_finder: &Arc<ExchangeFinder>,
    bindings: &Arc<ConnectionBindings>,
    tasks: &ConnectionTaskRegistry,
    availability: &ConnectionAvailability,
    connection: RealConnection,
    binding: AcquiredBinding,
) {
    match binding {
        AcquiredBinding::Http1 { sender, .. } => {
            if bindings.release_http1(connection.id(), sender)
                && exchange_finder.release(&connection)
            {
                availability.notify(connection.address());
                return;
            }
            teardown_pooled_connection(exchange_finder, bindings, tasks, connection.id());
            availability.notify(connection.address());
        }
        AcquiredBinding::Http2 { .. } => {
            if exchange_finder.release(&connection) {
                availability.notify(connection.address());
                return;
            }
            teardown_pooled_connection(exchange_finder, bindings, tasks, connection.id());
            availability.notify(connection.address());
        }
    }
}

pub(super) fn teardown_pooled_connection(
    exchange_finder: &Arc<ExchangeFinder>,
    bindings: &ConnectionBindings,
    tasks: &ConnectionTaskRegistry,
    connection_id: ConnectionId,
) {
    tasks.teardown_connection(bindings, connection_id);
    let _ = exchange_finder.pool().remove_without_hook(connection_id);
}
