use std::sync::{Arc, Mutex};
use std::time::Instant;

use openwire_core::{next_connection_id, ConnectionId};

use super::{Address, Route};

const DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionProtocol {
    Http1,
    Http2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionHealth {
    Healthy,
    Unhealthy,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionAllocationState {
    Idle,
    InUse { allocations: usize },
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) struct RealConnectionSnapshot {
    pub(crate) id: ConnectionId,
    pub(crate) protocol: ConnectionProtocol,
    pub(crate) health: ConnectionHealth,
    pub(crate) allocation: ConnectionAllocationState,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) completed_exchanges: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct RealConnection {
    inner: Arc<RealConnectionInner>,
}

#[derive(Debug)]
struct RealConnectionInner {
    id: ConnectionId,
    address: Address,
    route: Route,
    protocol: ConnectionProtocol,
    state: Mutex<RealConnectionState>,
}

#[derive(Debug)]
struct RealConnectionState {
    health: ConnectionHealth,
    allocations: usize,
    idle_since: Option<Instant>,
    completed_exchanges: usize,
}

impl RealConnection {
    pub(crate) fn new(route: Route, protocol: ConnectionProtocol) -> Self {
        Self::with_id(next_connection_id(), route, protocol)
    }

    pub(crate) fn with_id(id: ConnectionId, route: Route, protocol: ConnectionProtocol) -> Self {
        Self {
            inner: Arc::new(RealConnectionInner {
                id,
                address: route.address().clone(),
                route,
                protocol,
                state: Mutex::new(RealConnectionState {
                    health: ConnectionHealth::Healthy,
                    allocations: 0,
                    idle_since: Some(Instant::now()),
                    completed_exchanges: 0,
                }),
            }),
        }
    }

    pub(crate) fn id(&self) -> ConnectionId {
        self.inner.id
    }

    pub(crate) fn address(&self) -> &Address {
        &self.inner.address
    }

    pub(crate) fn route(&self) -> &Route {
        &self.inner.route
    }

    pub(crate) fn protocol(&self) -> ConnectionProtocol {
        self.inner.protocol
    }

    pub(crate) fn snapshot(&self) -> RealConnectionSnapshot {
        let state = self.inner.state.lock().expect("real connection lock");
        RealConnectionSnapshot {
            id: self.inner.id,
            protocol: self.inner.protocol,
            health: state.health,
            allocation: allocation_state(&state),
            idle_since: state.idle_since,
            completed_exchanges: state.completed_exchanges,
        }
    }

    pub(crate) fn try_acquire(&self) -> bool {
        let mut state = self.inner.state.lock().expect("real connection lock");
        if state.health != ConnectionHealth::Healthy {
            return false;
        }

        match self.inner.protocol {
            ConnectionProtocol::Http1 if state.allocations > 0 => return false,
            ConnectionProtocol::Http2
                if state.allocations >= DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS =>
            {
                return false;
            }
            ConnectionProtocol::Http1 | ConnectionProtocol::Http2 => {}
        }

        state.allocations += 1;
        state.idle_since = None;
        true
    }

    pub(crate) fn release(&self) -> bool {
        let mut state = self.inner.state.lock().expect("real connection lock");
        if state.health == ConnectionHealth::Closed || state.allocations == 0 {
            return false;
        }

        state.allocations -= 1;
        state.completed_exchanges += 1;
        if state.allocations == 0 {
            state.idle_since = Some(Instant::now());
        }
        true
    }

    pub(crate) fn mark_unhealthy(&self) {
        let mut state = self.inner.state.lock().expect("real connection lock");
        if state.health != ConnectionHealth::Closed {
            state.health = ConnectionHealth::Unhealthy;
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.inner.state.lock().expect("real connection lock");
        state.health = ConnectionHealth::Closed;
        state.allocations = 0;
        state.idle_since = None;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.snapshot().health == ConnectionHealth::Closed
    }
}

fn allocation_state(state: &RealConnectionState) -> ConnectionAllocationState {
    if state.health == ConnectionHealth::Closed {
        ConnectionAllocationState::Closed
    } else if state.allocations == 0 {
        ConnectionAllocationState::Idle
    } else {
        ConnectionAllocationState::InUse {
            allocations: state.allocations,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::{
        ConnectionAllocationState, ConnectionHealth, ConnectionProtocol, RealConnection,
        DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS,
    };
    use crate::connection::{Address, AuthorityKey, DnsPolicy, ProtocolPolicy, Route, UriScheme};

    fn test_connection(protocol: ConnectionProtocol) -> RealConnection {
        let address = Address::new(
            UriScheme::Http,
            AuthorityKey::new("example.com", 80),
            None,
            None,
            ProtocolPolicy::Http1Only,
            DnsPolicy::System,
        );
        let route = Route::direct(
            address,
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 80)),
        );
        RealConnection::new(route, protocol)
    }

    #[test]
    fn real_connection_transitions_between_idle_in_use_and_closed() {
        let connection = test_connection(ConnectionProtocol::Http1);

        let snapshot = connection.snapshot();
        assert_eq!(snapshot.protocol, ConnectionProtocol::Http1);
        assert_eq!(snapshot.health, ConnectionHealth::Healthy);
        assert_eq!(snapshot.allocation, ConnectionAllocationState::Idle);
        assert!(snapshot.idle_since.is_some());

        assert!(connection.try_acquire());
        let snapshot = connection.snapshot();
        assert_eq!(
            snapshot.allocation,
            ConnectionAllocationState::InUse { allocations: 1 }
        );
        assert!(snapshot.idle_since.is_none());

        assert!(connection.release());
        let snapshot = connection.snapshot();
        assert_eq!(snapshot.allocation, ConnectionAllocationState::Idle);
        assert_eq!(snapshot.completed_exchanges, 1);
        assert!(snapshot.idle_since.is_some());

        connection.mark_unhealthy();
        assert_eq!(connection.snapshot().health, ConnectionHealth::Unhealthy);

        connection.close();
        let snapshot = connection.snapshot();
        assert_eq!(snapshot.health, ConnectionHealth::Closed);
        assert_eq!(snapshot.allocation, ConnectionAllocationState::Closed);
        assert!(snapshot.idle_since.is_none());
    }

    #[test]
    fn http2_connection_tracks_parallel_allocations() {
        let connection = test_connection(ConnectionProtocol::Http2);

        assert!(connection.try_acquire());
        assert!(connection.try_acquire());
        assert_eq!(
            connection.snapshot().allocation,
            ConnectionAllocationState::InUse { allocations: 2 }
        );

        assert!(connection.release());
        let snapshot = connection.snapshot();
        assert_eq!(
            snapshot.allocation,
            ConnectionAllocationState::InUse { allocations: 1 }
        );
        assert_eq!(snapshot.completed_exchanges, 1);
        assert!(snapshot.idle_since.is_none());

        assert!(connection.release());
        let snapshot = connection.snapshot();
        assert_eq!(snapshot.allocation, ConnectionAllocationState::Idle);
        assert_eq!(snapshot.completed_exchanges, 2);
        assert!(snapshot.idle_since.is_some());
    }

    #[test]
    fn http2_connection_enforces_conservative_stream_limit() {
        let connection = test_connection(ConnectionProtocol::Http2);

        for _ in 0..DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS {
            assert!(connection.try_acquire());
        }
        assert!(!connection.try_acquire());

        assert!(connection.release());
        assert!(connection.try_acquire());
    }
}
