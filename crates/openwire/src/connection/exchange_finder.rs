use std::sync::Arc;

use http::Request;
use openwire_core::{ConnectionInfo, RequestBody, WireError};

use crate::proxy::Proxy;

use super::{
    Address, ConnectionAllocationState, ConnectionPool, ConnectionProtocol, RealConnection, Route,
};

#[derive(Clone, Debug)]
pub(crate) struct PreparedExchange {
    address: Address,
    outcome: PreparedExchangeOutcome,
}

impl PreparedExchange {
    pub(crate) fn address(&self) -> &Address {
        &self.address
    }

    pub(crate) fn outcome(&self) -> &PreparedExchangeOutcome {
        &self.outcome
    }

    pub(crate) fn reserved_connection(&self) -> Option<&RealConnection> {
        match &self.outcome {
            PreparedExchangeOutcome::PoolHit { connection } => Some(connection),
            PreparedExchangeOutcome::PoolMiss => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PreparedExchangeOutcome {
    PoolHit { connection: RealConnection },
    PoolMiss,
}

#[derive(Clone, Debug)]
pub(crate) struct ObservedConnection {
    connection: RealConnection,
    reused: bool,
}

impl ObservedConnection {
    pub(crate) fn connection(&self) -> &RealConnection {
        &self.connection
    }

    pub(crate) fn reused(&self) -> bool {
        self.reused
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExchangeFinder {
    pool: Arc<ConnectionPool>,
    proxies: Vec<Proxy>,
}

impl ExchangeFinder {
    pub(crate) fn new(pool: Arc<ConnectionPool>, proxies: Vec<Proxy>) -> Self {
        Self { pool, proxies }
    }

    pub(crate) fn pool(&self) -> &Arc<ConnectionPool> {
        &self.pool
    }

    pub(crate) fn prepare(
        &self,
        request: &Request<RequestBody>,
    ) -> Result<PreparedExchange, WireError> {
        let proxy = self.matching_proxy(request.uri());
        let address = Address::from_uri(request.uri(), proxy)?;
        let outcome = match self.pool.acquire(&address) {
            Some(connection) => PreparedExchangeOutcome::PoolHit { connection },
            None => PreparedExchangeOutcome::PoolMiss,
        };

        Ok(PreparedExchange { address, outcome })
    }

    pub(crate) fn observe_connection(
        &self,
        prepared: &PreparedExchange,
        info: &ConnectionInfo,
        protocol: ConnectionProtocol,
    ) -> ObservedConnection {
        if let Some(reserved) = prepared.reserved_connection() {
            if reserved.id() == info.id {
                return ObservedConnection {
                    connection: reserved.clone(),
                    reused: true,
                };
            }

            let _ = self.pool.remove(reserved.id());
        }

        if let Some(existing) = self.pool.get_by_id(prepared.address(), info.id) {
            let tracked = existing.try_acquire();
            debug_assert!(tracked, "observed reused connection should be acquirable");
            return ObservedConnection {
                connection: existing,
                reused: true,
            };
        }

        let route = Route::from_observed(prepared.address().clone(), info.remote_addr);
        let connection = RealConnection::with_id(info.id, route, protocol);
        let _ = connection.try_acquire();
        self.pool.insert(connection.clone());

        ObservedConnection {
            connection,
            reused: false,
        }
    }

    pub(crate) fn release(&self, connection: &RealConnection) -> bool {
        self.pool.release(connection)
    }

    pub(crate) fn discard_prepared(&self, prepared: &PreparedExchange) {
        if let Some(connection) = prepared.reserved_connection() {
            let _ = self.pool.remove(connection.id());
        }
    }

    pub(crate) fn matching_proxy_for_uri(&self, uri: &http::Uri) -> Option<&Proxy> {
        self.matching_proxy(uri)
    }

    fn matching_proxy(&self, uri: &http::Uri) -> Option<&Proxy> {
        self.proxies.iter().find(|proxy| proxy.matches(uri))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use http::Request;
    use openwire_core::{ConnectionInfo, RequestBody};

    use super::{ExchangeFinder, PreparedExchange, PreparedExchangeOutcome};
    use crate::connection::{
        Address, AuthorityKey, ConnectionAllocationState, ConnectionProtocol, DnsPolicy,
        PoolSettings, ProtocolPolicy, RealConnection, Route, UriScheme,
    };

    fn make_address() -> Address {
        Address::new(
            UriScheme::Http,
            AuthorityKey::new("example.com", 80),
            None,
            None,
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        )
    }

    fn connection_for_pool() -> RealConnection {
        let route = Route::direct(
            make_address(),
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 80)),
        );
        RealConnection::new(route, ConnectionProtocol::Http1)
    }

    #[test]
    fn exchange_finder_reports_pool_hit_before_new_connection_work() {
        let pool = Arc::new(crate::connection::ConnectionPool::new(
            PoolSettings::default(),
        ));
        let connection = connection_for_pool();
        assert!(connection.try_acquire());
        assert!(connection.release());
        pool.insert(connection.clone());
        let finder = ExchangeFinder::new(pool, Vec::new());

        let request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");
        let prepared = finder.prepare(&request).expect("prepared exchange");

        match prepared.outcome() {
            PreparedExchangeOutcome::PoolHit { connection: pooled } => {
                assert_eq!(pooled.id(), connection.id());
            }
            PreparedExchangeOutcome::PoolMiss => panic!("expected pool hit"),
        }
    }

    #[test]
    fn exchange_finder_observes_reused_connection_from_reserved_pool_entry() {
        let pool = Arc::new(crate::connection::ConnectionPool::new(
            PoolSettings::default(),
        ));
        let connection = connection_for_pool();
        assert!(connection.try_acquire());
        assert!(connection.release());
        pool.insert(connection.clone());
        let finder = ExchangeFinder::new(pool, Vec::new());

        let request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");
        let prepared = finder.prepare(&request).expect("prepared exchange");

        let observed = finder.observe_connection(
            &prepared,
            &ConnectionInfo {
                id: connection.id(),
                remote_addr: Some(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 80))),
                local_addr: None,
                tls: false,
            },
            ConnectionProtocol::Http1,
        );

        assert!(observed.reused());
        assert_eq!(observed.connection().id(), connection.id());
    }

    #[test]
    fn exchange_finder_tracks_parallel_http2_streams_on_existing_connection() {
        let pool = Arc::new(crate::connection::ConnectionPool::new(
            PoolSettings::default(),
        ));
        let route = Route::direct(
            make_address(),
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 80)),
        );
        let connection = RealConnection::new(route, ConnectionProtocol::Http2);
        assert!(connection.try_acquire());
        pool.insert(connection.clone());
        let finder = ExchangeFinder::new(pool, Vec::new());

        let prepared = PreparedExchange {
            address: make_address(),
            outcome: PreparedExchangeOutcome::PoolMiss,
        };
        let observed = finder.observe_connection(
            &prepared,
            &ConnectionInfo {
                id: connection.id(),
                remote_addr: Some(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 80))),
                local_addr: None,
                tls: false,
            },
            ConnectionProtocol::Http2,
        );

        assert!(observed.reused());
        assert_eq!(
            observed.connection().snapshot().allocation,
            ConnectionAllocationState::InUse { allocations: 2 }
        );
    }
}
