use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use openwire_core::ConnectionId;

use super::{Address, ConnectionAllocationState, RealConnection};

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
        self.connections
            .lock()
            .expect("connection pool lock")
            .entry(connection.address().clone())
            .or_default()
            .push(connection);
    }

    pub(crate) fn acquire(&self, address: &Address) -> Option<RealConnection> {
        let mut pool = self.connections.lock().expect("connection pool lock");
        let connections = pool.get_mut(address)?;
        connections.retain(|connection| !connection.is_closed());
        if connections.is_empty() {
            pool.remove(address);
            return None;
        }

        connections.iter().find(|conn| conn.try_acquire()).cloned()
    }

    pub(crate) fn release(&self, connection: &RealConnection) -> bool {
        connection.release()
    }

    pub(crate) fn get_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        self.connections
            .lock()
            .expect("connection pool lock")
            .get(address)
            .and_then(|connections| {
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
        let pool = self.connections.lock().expect("connection pool lock");
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
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
        let route = Route::direct(
            address,
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, last_octet), 443)),
        );
        RealConnection::new(route, ConnectionProtocol::Http1)
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
}
