use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use openwire_core::ConnectionId;

use super::{Address, ConnectionAllocationState, ConnectionProtocol, RealConnection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolSettings {
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) max_idle_per_address: usize,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Some(Duration::from_secs(90)),
            max_idle_per_address: usize::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PoolStats {
    pub(crate) total: usize,
    pub(crate) idle: usize,
    pub(crate) in_use: usize,
}

#[derive(Debug)]
pub(crate) struct ConnectionPool {
    settings: PoolSettings,
    connections: Mutex<HashMap<Address, Vec<RealConnection>>>,
}

impl ConnectionPool {
    pub(crate) fn new(settings: PoolSettings) -> Self {
        Self {
            settings,
            connections: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn settings(&self) -> &PoolSettings {
        &self.settings
    }

    pub(crate) fn insert(&self, connection: RealConnection) {
        let address = connection.address().clone();
        let mut pool = self.connections.lock().expect("connection pool lock");
        pool.entry(address.clone()).or_default().push(connection);
        prune_address(&self.settings, &mut pool, &address);
    }

    pub(crate) fn acquire(&self, address: &Address) -> Option<RealConnection> {
        let mut pool = self.connections.lock().expect("connection pool lock");
        if !prune_address(&self.settings, &mut pool, address) {
            pool.remove(address);
            return None;
        }

        let connections = pool.get_mut(address)?;
        connections.iter().find(|conn| conn.try_acquire()).cloned()
    }

    pub(crate) fn release(&self, connection: &RealConnection) -> bool {
        if !connection.release() {
            return false;
        }

        let address = connection.address().clone();
        let mut pool = self.connections.lock().expect("connection pool lock");
        if !prune_address(&self.settings, &mut pool, &address) {
            pool.remove(&address);
        }

        true
    }

    pub(crate) fn get_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let mut pool = self.connections.lock().expect("connection pool lock");
        if !prune_address(&self.settings, &mut pool, address) {
            pool.remove(address);
            return None;
        }

        pool.get(address).and_then(|connections| {
            connections
                .iter()
                .find(|connection| connection.id() == connection_id)
                .cloned()
        })
    }

    pub(crate) fn remove(&self, connection_id: ConnectionId) -> Option<RealConnection> {
        let mut pool = self.connections.lock().expect("connection pool lock");
        let keys = pool.keys().cloned().collect::<Vec<_>>();

        for key in keys {
            let mut removed = None;
            let mut should_remove_key = false;

            if let Some(connections) = pool.get_mut(&key) {
                if let Some(index) = connections
                    .iter()
                    .position(|conn| conn.id() == connection_id)
                {
                    let connection = connections.remove(index);
                    connection.close();
                    removed = Some(connection);
                }
                should_remove_key = connections.is_empty();
            }

            if should_remove_key {
                pool.remove(&key);
            }

            if removed.is_some() {
                return removed;
            }
        }

        None
    }

    pub(crate) fn stats(&self, address: &Address) -> PoolStats {
        let mut pool = self.connections.lock().expect("connection pool lock");
        if !prune_address(&self.settings, &mut pool, address) {
            pool.remove(address);
            return PoolStats::default();
        }
        let Some(connections) = pool.get(address) else {
            return PoolStats::default();
        };

        connections
            .iter()
            .fold(PoolStats::default(), |mut stats, connection| {
                stats.total += 1;
                match connection.snapshot().allocation {
                    ConnectionAllocationState::Idle => stats.idle += 1,
                    ConnectionAllocationState::InUse { .. } => stats.in_use += 1,
                    ConnectionAllocationState::Closed => {}
                }
                stats
            })
    }
}

fn prune_address(
    settings: &PoolSettings,
    pool: &mut HashMap<Address, Vec<RealConnection>>,
    address: &Address,
) -> bool {
    let Some(connections) = pool.get_mut(address) else {
        return false;
    };

    prune_connections(settings, connections);
    !connections.is_empty()
}

fn prune_connections(settings: &PoolSettings, connections: &mut Vec<RealConnection>) {
    connections.retain(|connection| {
        if connection.is_closed() {
            return false;
        }

        if settings
            .idle_timeout
            .is_some_and(|timeout| idle_http1_expired(connection, timeout))
        {
            connection.close();
            return false;
        }

        true
    });

    enforce_max_idle_http1(settings.max_idle_per_address, connections);
}

fn idle_http1_expired(connection: &RealConnection, timeout: Duration) -> bool {
    if connection.protocol() != ConnectionProtocol::Http1 {
        return false;
    }

    let snapshot = connection.snapshot();
    matches!(snapshot.allocation, ConnectionAllocationState::Idle)
        && snapshot
            .idle_since
            .is_some_and(|idle_since| idle_since.elapsed() >= timeout)
}

fn enforce_max_idle_http1(max_idle_per_address: usize, connections: &mut Vec<RealConnection>) {
    if max_idle_per_address == usize::MAX {
        return;
    }

    let mut idle_http1 = connections
        .iter()
        .filter_map(|connection| {
            let snapshot = connection.snapshot();
            if snapshot.protocol != ConnectionProtocol::Http1
                || !matches!(snapshot.allocation, ConnectionAllocationState::Idle)
            {
                return None;
            }

            snapshot
                .idle_since
                .map(|idle_since| (connection.id(), idle_since))
        })
        .collect::<Vec<_>>();

    if idle_http1.len() <= max_idle_per_address {
        return;
    }

    idle_http1.sort_by_key(|(_, idle_since)| *idle_since);
    let evict_count = idle_http1.len() - max_idle_per_address;
    let evicted_ids = idle_http1
        .into_iter()
        .take(evict_count)
        .map(|(id, _)| id)
        .collect::<Vec<_>>();

    connections.retain(|connection| {
        if evicted_ids.contains(&connection.id()) {
            connection.close();
            return false;
        }

        true
    });
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::thread;
    use std::time::Duration;

    use super::{ConnectionPool, PoolSettings, PoolStats};
    use crate::connection::{
        Address, AuthorityKey, ConnectionAllocationState, ConnectionProtocol, DnsPolicy,
        ProtocolPolicy, ProxyConfig, ProxyEndpoint, ProxyMode, ProxyScheme, RealConnection, Route,
        UriScheme,
    };

    fn address_with_proxy(proxy: Option<ProxyConfig>) -> Address {
        Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            proxy,
            Some(crate::connection::TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        )
    }

    fn make_connection(address: Address, last_octet: u8) -> RealConnection {
        make_connection_with_protocol(address, last_octet, ConnectionProtocol::Http1)
    }

    fn make_connection_with_protocol(
        address: Address,
        last_octet: u8,
        protocol: ConnectionProtocol,
    ) -> RealConnection {
        let route = Route::direct(
            address,
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, last_octet), 443)),
        );
        RealConnection::new(route, protocol)
    }

    #[test]
    fn pool_stores_settings_without_enabling_background_eviction() {
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(30)),
            max_idle_per_address: 2,
        });

        assert_eq!(
            pool.settings(),
            &PoolSettings {
                idle_timeout: Some(Duration::from_secs(30)),
                max_idle_per_address: 2,
            }
        );
    }

    #[test]
    fn pool_insert_acquire_release_and_remove_follow_address_keying() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = make_connection(address.clone(), 10);
        let connection_id = connection.id();
        pool.insert(connection.clone());

        assert_eq!(
            pool.stats(&address),
            PoolStats {
                total: 1,
                idle: 1,
                in_use: 0,
            }
        );

        let acquired = pool
            .acquire(&address)
            .expect("connection should be reusable");
        assert_eq!(acquired.id(), connection_id);
        assert_eq!(
            acquired.snapshot().allocation,
            ConnectionAllocationState::InUse { allocations: 1 }
        );
        assert_eq!(
            pool.stats(&address),
            PoolStats {
                total: 1,
                idle: 0,
                in_use: 1,
            }
        );

        assert!(pool.release(&acquired));
        assert_eq!(
            pool.stats(&address),
            PoolStats {
                total: 1,
                idle: 1,
                in_use: 0,
            }
        );

        let removed = pool.remove(connection_id).expect("connection should exist");
        assert_eq!(removed.id(), connection_id);
        assert_eq!(pool.stats(&address), PoolStats::default());
    }

    #[test]
    fn pool_only_reuses_exact_address_matches() {
        let direct = address_with_proxy(None);
        let proxied = address_with_proxy(Some(ProxyConfig::new(
            ProxyMode::Connect,
            ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
        )));
        let alt_dns = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            None,
            Some(crate::connection::TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("mobile".into()),
        );

        let pool = ConnectionPool::new(PoolSettings::default());
        pool.insert(make_connection(direct.clone(), 11));

        assert!(pool.acquire(&proxied).is_none());
        assert!(pool.acquire(&alt_dns).is_none());
        assert!(pool.acquire(&direct).is_some());
    }

    #[test]
    fn pool_evicts_idle_http1_connections_after_timeout() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::ZERO),
            max_idle_per_address: usize::MAX,
        });
        let connection = make_connection(address.clone(), 12);
        let connection_id = connection.id();
        pool.insert(connection);

        assert!(pool.acquire(&address).is_none());
        assert_eq!(pool.stats(&address), PoolStats::default());
        assert!(pool.remove(connection_id).is_none());
    }

    #[test]
    fn pool_keeps_only_newest_idle_http1_connections_within_limit() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: None,
            max_idle_per_address: 2,
        });

        let oldest = make_connection(address.clone(), 21);
        thread::sleep(Duration::from_millis(1));
        let middle = make_connection(address.clone(), 22);
        thread::sleep(Duration::from_millis(1));
        let newest = make_connection(address.clone(), 23);

        pool.insert(oldest.clone());
        pool.insert(middle.clone());
        pool.insert(newest.clone());

        assert_eq!(
            pool.stats(&address),
            PoolStats {
                total: 2,
                idle: 2,
                in_use: 0,
            }
        );
        assert!(pool.remove(oldest.id()).is_none());
        assert!(pool.remove(middle.id()).is_some());
        assert!(pool.remove(newest.id()).is_some());
    }

    #[test]
    fn pool_does_not_apply_http1_idle_eviction_rules_to_http2_connections() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::ZERO),
            max_idle_per_address: 0,
        });
        let connection =
            make_connection_with_protocol(address.clone(), 31, ConnectionProtocol::Http2);
        let connection_id = connection.id();
        pool.insert(connection);

        assert_eq!(
            pool.stats(&address),
            PoolStats {
                total: 1,
                idle: 1,
                in_use: 0,
            }
        );
        assert_eq!(
            pool.acquire(&address).map(|connection| connection.id()),
            Some(connection_id)
        );
    }

    #[test]
    fn pool_refuses_http2_reuse_after_conservative_stream_limit_is_reached() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection =
            make_connection_with_protocol(address.clone(), 32, ConnectionProtocol::Http2);
        pool.insert(connection.clone());

        for _ in 0..100 {
            assert!(connection.try_acquire());
        }

        assert!(pool.acquire(&address).is_none());
        assert!(connection.release());
        assert!(pool.acquire(&address).is_some());
    }
}
