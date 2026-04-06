use std::sync::Arc;

use http::Request;
use openwire_core::{ConnectionId, ConnectionInfo, RequestBody, WireError};

use crate::proxy::{
    ProxyChoice, ProxySelection, ProxySelector, SelectedProxy, SharedProxySelector,
};

use super::{
    Address, ConnectionAllocationState, ConnectionPool, ConnectionProtocol, RealConnection, Route,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedAddress {
    address: Address,
    selected_proxy: Option<SelectedProxy>,
}

impl ResolvedAddress {
    pub(crate) fn new(address: Address, selected_proxy: Option<SelectedProxy>) -> Self {
        Self {
            address,
            selected_proxy,
        }
    }

    pub(crate) fn address(&self) -> &Address {
        &self.address
    }

    pub(crate) fn selected_proxy(&self) -> Option<&SelectedProxy> {
        self.selected_proxy.as_ref()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CachedAddresses(pub(crate) Arc<[ResolvedAddress]>);

#[derive(Clone, Debug)]
pub(crate) struct PreparedExchange {
    addresses: Arc<[ResolvedAddress]>,
    outcome: PreparedExchangeOutcome,
}

impl PreparedExchange {
    pub(crate) fn addresses(&self) -> &[ResolvedAddress] {
        &self.addresses
    }

    pub(crate) fn pooled_address(&self) -> Option<&ResolvedAddress> {
        match &self.outcome {
            PreparedExchangeOutcome::PoolHit {
                address_index,
                connection: _,
            } => self.addresses.get(*address_index),
            PreparedExchangeOutcome::PoolMiss => None,
        }
    }

    pub(crate) fn pool_hit(&self) -> bool {
        matches!(self.outcome, PreparedExchangeOutcome::PoolHit { .. })
    }

    pub(crate) fn pool_connection_id(&self) -> Option<ConnectionId> {
        self.reserved_connection().map(|connection| connection.id())
    }

    pub(crate) fn outcome(&self) -> &PreparedExchangeOutcome {
        &self.outcome
    }

    pub(crate) fn reserved_connection(&self) -> Option<&RealConnection> {
        match &self.outcome {
            PreparedExchangeOutcome::PoolHit {
                address_index: _,
                connection,
            } => Some(connection),
            PreparedExchangeOutcome::PoolMiss => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PreparedExchangeOutcome {
    PoolHit {
        address_index: usize,
        connection: RealConnection,
    },
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

#[derive(Clone)]
pub(crate) struct ExchangeFinder {
    pool: Arc<ConnectionPool>,
    proxy_selector: SharedProxySelector,
}

impl ExchangeFinder {
    pub(crate) fn new(pool: Arc<ConnectionPool>, proxy_selector: SharedProxySelector) -> Self {
        Self {
            pool,
            proxy_selector,
        }
    }

    pub(crate) fn pool(&self) -> &Arc<ConnectionPool> {
        &self.pool
    }

    pub(crate) fn prepare(
        &self,
        request: &Request<RequestBody>,
    ) -> Result<PreparedExchange, WireError> {
        let addresses = if let Some(cached) = request.extensions().get::<CachedAddresses>() {
            cached.0.clone()
        } else {
            Arc::<[ResolvedAddress]>::from(self.resolve_addresses(request.uri())?)
        };
        let outcome = addresses
            .iter()
            .enumerate()
            .find_map(|(index, resolved)| {
                self.pool.acquire(resolved.address()).map(|connection| {
                    PreparedExchangeOutcome::PoolHit {
                        address_index: index,
                        connection,
                    }
                })
            })
            .unwrap_or(PreparedExchangeOutcome::PoolMiss);

        Ok(PreparedExchange { addresses, outcome })
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

        let observed_address = prepared
            .pooled_address()
            .unwrap_or_else(|| {
                prepared
                    .addresses()
                    .first()
                    .expect("prepared exchange should have at least one address")
            })
            .address();
        if let Some(existing) = self.pool.acquire_by_id(observed_address, info.id) {
            return ObservedConnection {
                connection: existing,
                reused: true,
            };
        }

        let route = Route::from_observed(observed_address.clone(), info.remote_addr);
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

    pub(crate) fn proxy_selection_for_uri(
        &self,
        uri: &http::Uri,
    ) -> Result<ProxySelection, WireError> {
        self.proxy_selector.select(uri)
    }

    fn resolve_addresses(&self, uri: &http::Uri) -> Result<Vec<ResolvedAddress>, WireError> {
        let selection = self.proxy_selector.select(uri)?;
        let mut addresses = Vec::new();

        for choice in selection.iter() {
            let selected_proxy = match choice {
                ProxyChoice::Direct => None,
                ProxyChoice::Proxy(proxy) => Some(SelectedProxy::from_proxy(proxy)),
            };
            let resolved = ResolvedAddress::new(
                Address::from_uri(uri, selected_proxy.as_ref())?,
                selected_proxy,
            );
            if !addresses.iter().any(|candidate: &ResolvedAddress| {
                candidate.address() == resolved.address()
                    && candidate.selected_proxy() == resolved.selected_proxy()
            }) {
                addresses.push(resolved);
            }
        }

        if addresses.is_empty() {
            addresses.push(ResolvedAddress::new(Address::from_uri(uri, None)?, None));
        }

        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use http::Request;
    use openwire_core::{ConnectionInfo, RequestBody};

    use super::{
        CachedAddresses, ExchangeFinder, PreparedExchange, PreparedExchangeOutcome, ResolvedAddress,
    };
    use crate::connection::{
        Address, AuthorityKey, ConnectionAllocationState, ConnectionProtocol, DnsPolicy,
        PoolSettings, ProtocolPolicy, RealConnection, Route, UriScheme,
    };
    use crate::proxy::{NoProxy, Proxy, ProxyRules};

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
        let finder = ExchangeFinder::new(pool, Arc::new(ProxyRules::new()));

        let request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");
        let prepared = finder.prepare(&request).expect("prepared exchange");

        match prepared.outcome() {
            PreparedExchangeOutcome::PoolHit {
                address_index: _,
                connection: pooled,
            } => {
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
        let finder = ExchangeFinder::new(pool, Arc::new(ProxyRules::new()));

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
        let finder = ExchangeFinder::new(pool, Arc::new(ProxyRules::new()));

        let prepared = PreparedExchange {
            addresses: Arc::from([ResolvedAddress::new(make_address(), None)]),
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

    #[test]
    fn exchange_finder_uses_shared_proxy_selection_order() {
        let pool = Arc::new(crate::connection::ConnectionPool::new(
            PoolSettings::default(),
        ));
        let primary = Proxy::all("http://first.test:8080")
            .expect("primary proxy")
            .no_proxy(NoProxy::new().domain("api.example.com"));
        let fallback = Proxy::all("http://second.test:8080").expect("fallback proxy");
        let finder = ExchangeFinder::new(
            pool,
            Arc::new(ProxyRules::new().proxy(primary).proxy(fallback)),
        );

        let direct_uri = "http://service.example.com/resource".parse().expect("uri");
        let direct_selection = finder
            .proxy_selection_for_uri(&direct_uri)
            .expect("proxy selection")
            .iter()
            .map(|choice| match choice {
                crate::proxy::ProxyChoice::Direct => "direct".to_owned(),
                crate::proxy::ProxyChoice::Proxy(proxy) => {
                    proxy.target().host_str().expect("proxy host").to_owned()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            direct_selection,
            vec!["first.test".to_owned(), "second.test".to_owned()]
        );

        let bypassed_uri = "http://api.example.com/resource".parse().expect("uri");
        let bypassed_selection = finder
            .proxy_selection_for_uri(&bypassed_uri)
            .expect("proxy selection")
            .iter()
            .map(|choice| match choice {
                crate::proxy::ProxyChoice::Direct => "direct".to_owned(),
                crate::proxy::ProxyChoice::Proxy(proxy) => {
                    proxy.target().host_str().expect("proxy host").to_owned()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(bypassed_selection, vec!["second.test".to_owned()]);
    }

    #[test]
    fn exchange_finder_uses_cached_address_when_present() {
        let cached_address = Address::new(
            UriScheme::Http,
            AuthorityKey::new("cached.test", 80),
            None,
            None,
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        );
        let route = Route::direct(
            cached_address.clone(),
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, 44), 80)),
        );
        let connection = RealConnection::new(route, ConnectionProtocol::Http1);
        assert!(connection.try_acquire());
        assert!(connection.release());

        let pool = Arc::new(crate::connection::ConnectionPool::new(
            PoolSettings::default(),
        ));
        pool.insert(connection.clone());
        let finder = ExchangeFinder::new(pool, Arc::new(ProxyRules::new()));

        let mut request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(CachedAddresses(Arc::from([ResolvedAddress::new(
                cached_address.clone(),
                None,
            )])));

        let prepared = finder.prepare(&request).expect("prepared exchange");

        assert_eq!(
            prepared.pooled_address().expect("pooled address").address(),
            &cached_address
        );
        assert_eq!(prepared.pool_connection_id(), Some(connection.id()));
    }

    #[test]
    fn exchange_finder_recomputes_address_when_cache_is_absent() {
        let finder = ExchangeFinder::new(
            Arc::new(crate::connection::ConnectionPool::new(
                PoolSettings::default(),
            )),
            Arc::new(ProxyRules::new()),
        );
        let request = Request::builder()
            .uri("http://example.com/resource")
            .body(RequestBody::empty())
            .expect("request");

        let prepared = finder.prepare(&request).expect("prepared exchange");

        assert_eq!(
            prepared
                .addresses()
                .first()
                .expect("first prepared address")
                .address(),
            &make_address()
        );
        assert!(matches!(
            prepared.outcome(),
            PreparedExchangeOutcome::PoolMiss
        ));
    }
}
