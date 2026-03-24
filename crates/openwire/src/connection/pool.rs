use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use openwire_core::ConnectionId;

use super::{
    Address, ConnectionAllocationState, ConnectionProtocol, ProtocolPolicy, RealConnection, Route,
    RouteKind, RoutePlan, UriScheme,
};
use crate::sync_util::lock_mutex;

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

pub(crate) struct ConnectionPool {
    settings: PoolSettings,
    state: Mutex<PoolState>,
}

#[derive(Debug, Default)]
struct PoolState {
    by_address: HashMap<Address, Vec<RealConnection>>,
    by_id: HashMap<ConnectionId, Address>,
    coalesced_by_target: HashMap<SocketAddr, Vec<RealConnection>>,
}

impl ConnectionPool {
    pub(crate) fn new(settings: PoolSettings) -> Self {
        Self {
            settings,
            state: Mutex::new(PoolState::default()),
        }
    }

    pub(crate) fn settings(&self) -> &PoolSettings {
        &self.settings
    }

    pub(crate) fn insert(&self, connection: RealConnection) {
        let address = connection.address().clone();
        {
            let mut state = lock_mutex(&self.state);
            state
                .by_address
                .entry(address.clone())
                .or_default()
                .push(connection.clone());
            register_connection(&mut state, &connection);
            prune_address(&self.settings, &mut state, &address);
        }
    }

    pub(crate) fn acquire(&self, address: &Address) -> Option<RealConnection> {
        let mut state = lock_mutex(&self.state);
        if !prune_address(&self.settings, &mut state, address) {
            return None;
        }

        let connections = state.by_address.get_mut(address)?;
        connections.iter().find(|conn| conn.try_acquire()).cloned()
    }

    pub(crate) fn has_in_use_connection(&self, address: &Address) -> bool {
        let mut state = lock_mutex(&self.state);
        if !prune_address(&self.settings, &mut state, address) {
            return false;
        }

        state.by_address.get(address).is_some_and(|connections| {
            connections.iter().any(|connection| {
                matches!(
                    connection.snapshot().allocation,
                    ConnectionAllocationState::InUse { .. }
                )
            })
        })
    }

    pub(crate) fn acquire_coalesced(
        &self,
        address: &Address,
        route_plan: &RoutePlan,
    ) -> Option<RealConnection> {
        if !request_allows_h2_coalescing(address) {
            return None;
        }

        let direct_targets = direct_route_targets(route_plan);
        if direct_targets.is_empty() {
            return None;
        }

        let mut state = lock_mutex(&self.state);
        let mut addresses_to_prune = HashSet::new();
        for target in &direct_targets {
            if let Some(bucket) = state.coalesced_by_target.get(target) {
                addresses_to_prune
                    .extend(bucket.iter().map(|connection| connection.address().clone()));
            }
        }

        for candidate_address in addresses_to_prune {
            prune_address(&self.settings, &mut state, &candidate_address);
        }

        let mut candidates = Vec::new();
        let mut seen_ids = HashSet::new();
        for target in direct_targets {
            prune_coalescing_bucket(&mut state, target);
            let Some(bucket) = state.coalesced_by_target.get(&target) else {
                continue;
            };

            for connection in bucket {
                if seen_ids.insert(connection.id()) && can_coalesce(connection, address, route_plan)
                {
                    candidates.push(connection.clone());
                }
            }
        }

        drop(state);

        candidates
            .into_iter()
            .find(|connection| connection.try_acquire())
    }

    pub(crate) fn release(&self, connection: &RealConnection) -> bool {
        if !connection.release() {
            return false;
        }

        let address = connection.address().clone();
        let mut state = lock_mutex(&self.state);
        prune_address(&self.settings, &mut state, &address);

        true
    }

    pub(crate) fn get_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let mut state = lock_mutex(&self.state);
        if !prune_address(&self.settings, &mut state, address) {
            return None;
        }
        if state.by_id.get(&connection_id) != Some(address) {
            return None;
        }

        state.by_address.get(address).and_then(|connections| {
            connections
                .iter()
                .find(|connection| connection.id() == connection_id)
                .cloned()
        })
    }

    pub(crate) fn acquire_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let mut state = lock_mutex(&self.state);
        if !prune_address(&self.settings, &mut state, address) {
            return None;
        }
        if state.by_id.get(&connection_id) != Some(address) {
            return None;
        }

        let connections = state.by_address.get_mut(address)?;
        connections.iter().find_map(|connection| {
            (connection.id() == connection_id && connection.try_acquire())
                .then(|| connection.clone())
        })
    }

    pub(crate) fn remove(&self, connection_id: ConnectionId) -> Option<RealConnection> {
        let mut state = lock_mutex(&self.state);
        let address = state.by_id.remove(&connection_id)?;
        let mut removed = None;
        let mut should_remove_key = false;

        if let Some(connections) = state.by_address.get_mut(&address) {
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
            state.by_address.remove(&address);
        }

        if let Some(ref connection) = removed {
            remove_index_connection(&mut state.coalesced_by_target, connection);
        }

        removed
    }

    pub(crate) fn stats(&self, address: &Address) -> PoolStats {
        let mut state = lock_mutex(&self.state);
        if !prune_address(&self.settings, &mut state, address) {
            return PoolStats::default();
        }
        let Some(connections) = state.by_address.get(address) else {
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

    pub(crate) fn prune_all(&self) {
        let mut state = lock_mutex(&self.state);
        let addresses = state.by_address.keys().cloned().collect::<Vec<_>>();
        for address in addresses {
            prune_address(&self.settings, &mut state, &address);
        }
    }
}

impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

fn prune_address(settings: &PoolSettings, state: &mut PoolState, address: &Address) -> bool {
    let (removed, empty) = {
        let Some(connections) = state.by_address.get_mut(address) else {
            return false;
        };

        let removed = prune_connections(settings, connections);
        (removed, connections.is_empty())
    };

    for connection in &removed {
        unregister_connection(state, connection);
    }

    if empty {
        state.by_address.remove(address);
        return false;
    }

    true
}

fn prune_connections(
    settings: &PoolSettings,
    connections: &mut Vec<RealConnection>,
) -> Vec<RealConnection> {
    let mut removed = Vec::new();
    connections.retain(|connection| {
        if !connection.is_healthy() {
            connection.close();
            removed.push(connection.clone());
            return false;
        }

        if settings
            .idle_timeout
            .is_some_and(|timeout| idle_connection_expired(connection, timeout))
        {
            connection.close();
            removed.push(connection.clone());
            return false;
        }

        true
    });

    removed.extend(enforce_max_idle_connections(
        settings.max_idle_per_address,
        connections,
    ));
    removed
}

fn idle_connection_expired(connection: &RealConnection, timeout: Duration) -> bool {
    let snapshot = connection.snapshot();
    matches!(snapshot.allocation, ConnectionAllocationState::Idle)
        && snapshot
            .idle_since
            .is_some_and(|idle_since| idle_since.elapsed() >= timeout)
}

fn can_coalesce(connection: &RealConnection, request: &Address, route_plan: &RoutePlan) -> bool {
    if connection.protocol() != ConnectionProtocol::Http2
        || !request_allows_h2_coalescing(request)
        || !connection_allows_h2_coalescing(connection)
    {
        return false;
    }

    let existing = connection.address();
    if existing.authority().port() != request.authority().port() {
        return false;
    }

    if existing.authority() == request.authority() {
        return false;
    }

    let host_matches = connection
        .coalescing()
        .verified_server_names
        .iter()
        .any(|name| verified_server_name_matches(name, request.authority().host()));
    if !host_matches {
        return false;
    }

    route_overlap(connection.route(), route_plan)
}

fn request_allows_h2_coalescing(address: &Address) -> bool {
    address.scheme() == UriScheme::Https
        && address.proxy().is_none()
        && !matches!(address.protocol_policy(), ProtocolPolicy::Http1Only)
}

fn connection_allows_h2_coalescing(connection: &RealConnection) -> bool {
    let address = connection.address();
    address.scheme() == UriScheme::Https && address.proxy().is_none()
}

fn route_overlap(connection_route: &Route, route_plan: &RoutePlan) -> bool {
    let RouteKind::Direct {
        target: existing_target,
    } = connection_route.kind()
    else {
        return false;
    };

    route_plan.iter().any(|route| {
        matches!(
            route.kind(),
            RouteKind::Direct { target } if target == existing_target
        )
    })
}

fn verified_server_name_matches(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }

    let Some(suffix) = pattern.strip_prefix("*.") else {
        return false;
    };
    let Some(prefix) = host.strip_suffix(suffix) else {
        return false;
    };

    !prefix.is_empty() && prefix.ends_with('.') && !prefix[..prefix.len() - 1].contains('.')
}

fn enforce_max_idle_connections(
    max_idle_per_address: usize,
    connections: &mut Vec<RealConnection>,
) -> Vec<RealConnection> {
    if max_idle_per_address == usize::MAX {
        return Vec::new();
    }

    let mut idle_connections = connections
        .iter()
        .filter_map(|connection| {
            let snapshot = connection.snapshot();
            if !matches!(snapshot.allocation, ConnectionAllocationState::Idle) {
                return None;
            }

            snapshot
                .idle_since
                .map(|idle_since| (connection.id(), idle_since))
        })
        .collect::<Vec<_>>();

    if idle_connections.len() <= max_idle_per_address {
        return Vec::new();
    }

    idle_connections.sort_by_key(|(_, idle_since)| *idle_since);
    let evict_count = idle_connections.len() - max_idle_per_address;
    let evicted_ids = idle_connections
        .into_iter()
        .take(evict_count)
        .map(|(id, _)| id)
        .collect::<HashSet<_>>();

    let mut removed = Vec::new();
    connections.retain(|connection| {
        if evicted_ids.contains(&connection.id()) {
            connection.close();
            removed.push(connection.clone());
            return false;
        }

        true
    });

    removed
}

fn direct_route_targets(route_plan: &RoutePlan) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for route in route_plan.iter() {
        let RouteKind::Direct { target } = route.kind() else {
            continue;
        };
        if seen.insert(*target) {
            targets.push(*target);
        }
    }
    targets
}

fn register_connection(state: &mut PoolState, connection: &RealConnection) {
    state
        .by_id
        .insert(connection.id(), connection.address().clone());
    index_connection(&mut state.coalesced_by_target, connection);
}

fn unregister_connection(state: &mut PoolState, connection: &RealConnection) {
    state.by_id.remove(&connection.id());
    remove_index_connection(&mut state.coalesced_by_target, connection);
}

fn index_connection(
    index: &mut HashMap<SocketAddr, Vec<RealConnection>>,
    connection: &RealConnection,
) {
    let Some(target) = coalescing_index_target(connection) else {
        return;
    };
    index.entry(target).or_default().push(connection.clone());
}

fn remove_index_connection(
    index: &mut HashMap<SocketAddr, Vec<RealConnection>>,
    connection: &RealConnection,
) {
    let Some(target) = coalescing_index_target(connection) else {
        return;
    };
    let should_remove = if let Some(bucket) = index.get_mut(&target) {
        bucket.retain(|existing| existing.id() != connection.id());
        bucket.is_empty()
    } else {
        false
    };
    if should_remove {
        index.remove(&target);
    }
}

fn prune_coalescing_bucket(state: &mut PoolState, target: SocketAddr) {
    let should_remove = if let Some(bucket) = state.coalesced_by_target.get_mut(&target) {
        bucket.retain(|connection| {
            coalescing_index_target(connection) == Some(target) && !connection.is_closed()
        });
        bucket.is_empty()
    } else {
        false
    };
    if should_remove {
        state.coalesced_by_target.remove(&target);
    }
}

fn coalescing_index_target(connection: &RealConnection) -> Option<SocketAddr> {
    if connection.protocol() != ConnectionProtocol::Http2
        || !connection_allows_h2_coalescing(connection)
    {
        return None;
    }

    match connection.route().kind() {
        RouteKind::Direct { target } => Some(*target),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::panic::{self, AssertUnwindSafe};
    use std::time::{Duration, Instant};

    use openwire_core::CoalescingInfo;

    use super::{ConnectionPool, PoolSettings, PoolStats};
    use crate::connection::{
        Address, AuthorityKey, ConnectionAllocationState, ConnectionHealth, ConnectionProtocol,
        DnsPolicy, ProtocolPolicy, ProxyConfig, ProxyEndpoint, ProxyMode, ProxyScheme,
        RealConnection, Route, RoutePlan, UriScheme,
    };
    use crate::sync_util::lock_mutex;

    fn address_for_host(
        host: &str,
        proxy: Option<ProxyConfig>,
        protocol_policy: ProtocolPolicy,
    ) -> Address {
        Address::new(
            UriScheme::Https,
            AuthorityKey::new(host, 443),
            proxy,
            Some(crate::connection::TlsIdentity::new(host)),
            protocol_policy,
            DnsPolicy::System,
        )
    }

    fn address_with_proxy(proxy: Option<ProxyConfig>) -> Address {
        address_for_host("example.com", proxy, ProtocolPolicy::Http1OrHttp2)
    }

    fn make_connection(address: Address, last_octet: u8) -> RealConnection {
        make_connection_with_protocol(address, last_octet, ConnectionProtocol::Http1)
    }

    fn make_connection_with_protocol(
        address: Address,
        last_octet: u8,
        protocol: ConnectionProtocol,
    ) -> RealConnection {
        make_connection_with_protocol_and_coalescing(address, last_octet, protocol, &[])
    }

    fn make_connection_with_protocol_and_coalescing(
        address: Address,
        last_octet: u8,
        protocol: ConnectionProtocol,
        verified_server_names: &[&str],
    ) -> RealConnection {
        let route = Route::direct(
            address,
            SocketAddr::from((Ipv4Addr::new(192, 0, 2, last_octet), 443)),
        );
        RealConnection::with_id_and_coalescing(
            openwire_core::next_connection_id(),
            route,
            protocol,
            CoalescingInfo::new(
                verified_server_names
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
            ),
        )
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
            lock_mutex(&pool.state).by_id.get(&connection_id),
            Some(&address)
        );

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
        assert!(!lock_mutex(&pool.state).by_id.contains_key(&connection_id));
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
    fn pool_does_not_coalesce_same_authority_across_different_address_buckets() {
        let pooled = address_for_host("example.com", None, ProtocolPolicy::Http1OrHttp2);
        let alt_dns = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            None,
            Some(crate::connection::TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("mobile".into()),
        );
        let pool = ConnectionPool::new(PoolSettings::default());
        pool.insert(make_connection_with_protocol_and_coalescing(
            pooled,
            42,
            ConnectionProtocol::Http2,
            &["example.com"],
        ));

        let route_plan = RoutePlan::new(
            vec![Route::direct(
                alt_dns.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 42), 443)),
            )],
            Duration::from_millis(250),
        );

        assert!(pool.acquire(&alt_dns).is_none());
        assert!(pool.acquire_coalesced(&alt_dns, &route_plan).is_none());
    }

    #[test]
    fn pool_coalesces_direct_https_http2_connections_for_verified_authorities() {
        let first = address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2);
        let second = address_for_host("b.test", None, ProtocolPolicy::Http1OrHttp2);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = make_connection_with_protocol_and_coalescing(
            first,
            41,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        );
        let connection_id = connection.id();
        pool.insert(connection);

        let route_plan = RoutePlan::new(
            vec![Route::direct(
                second.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 41), 443)),
            )],
            Duration::from_millis(250),
        );

        assert_eq!(
            pool.acquire_coalesced(&second, &route_plan)
                .map(|connection| connection.id()),
            Some(connection_id)
        );
    }

    #[test]
    fn pool_rejects_coalescing_without_verified_origin_route_overlap_or_direct_h2() {
        let second = address_for_host("b.test", None, ProtocolPolicy::Http1OrHttp2);
        let matching_route_plan = RoutePlan::new(
            vec![Route::direct(
                second.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 42), 443)),
            )],
            Duration::from_millis(250),
        );
        let mismatched_route_plan = RoutePlan::new(
            vec![Route::direct(
                second.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 99), 443)),
            )],
            Duration::from_millis(250),
        );

        let unverified_pool = ConnectionPool::new(PoolSettings::default());
        unverified_pool.insert(make_connection_with_protocol_and_coalescing(
            address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2),
            42,
            ConnectionProtocol::Http2,
            &["a.test"],
        ));
        assert!(unverified_pool
            .acquire_coalesced(&second, &matching_route_plan)
            .is_none());

        let route_miss_pool = ConnectionPool::new(PoolSettings::default());
        route_miss_pool.insert(make_connection_with_protocol_and_coalescing(
            address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2),
            42,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        ));
        assert!(route_miss_pool
            .acquire_coalesced(&second, &mismatched_route_plan)
            .is_none());

        let proxy_pool = ConnectionPool::new(PoolSettings::default());
        proxy_pool.insert(make_connection_with_protocol_and_coalescing(
            address_for_host(
                "a.test",
                Some(ProxyConfig::new(
                    ProxyMode::Connect,
                    ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
                )),
                ProtocolPolicy::Http1OrHttp2,
            ),
            42,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        ));
        assert!(proxy_pool
            .acquire_coalesced(&second, &matching_route_plan)
            .is_none());

        let http1_only = address_for_host("b.test", None, ProtocolPolicy::Http1Only);
        let policy_pool = ConnectionPool::new(PoolSettings::default());
        policy_pool.insert(make_connection_with_protocol_and_coalescing(
            address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2),
            42,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        ));
        assert!(policy_pool
            .acquire_coalesced(&http1_only, &matching_route_plan)
            .is_none());
    }

    #[test]
    fn pool_evicts_idle_http1_connections_after_timeout() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        let connection = make_connection(address.clone(), 12);
        let connection_id = connection.id();
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        assert!(pool.acquire(&address).is_none());
        assert_eq!(pool.stats(&address), PoolStats::default());
        assert!(pool.remove(connection_id).is_none());
    }

    #[test]
    fn pool_keeps_idle_http1_connections_before_timeout() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        let connection = make_connection(address.clone(), 13);
        let connection_id = connection.id();
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(2)));

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
    fn pool_keeps_only_newest_idle_http1_connections_within_limit() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: None,
            max_idle_per_address: 2,
        });
        let now = Instant::now();

        let oldest = make_connection(address.clone(), 21);
        let middle = make_connection(address.clone(), 22);
        let newest = make_connection(address.clone(), 23);
        oldest.set_idle_since_for_test(Some(now - Duration::from_secs(3)));
        middle.set_idle_since_for_test(Some(now - Duration::from_secs(2)));
        newest.set_idle_since_for_test(Some(now - Duration::from_secs(1)));

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
    fn pool_prunes_unhealthy_connections_on_touch() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = make_connection(address.clone(), 24);
        let connection_id = connection.id();
        pool.insert(connection.clone());

        connection.mark_unhealthy();

        assert_eq!(pool.stats(&address), PoolStats::default());
        assert!(pool.acquire(&address).is_none());
        assert!(pool.get_by_id(&address, connection_id).is_none());
        assert!(pool.remove(connection_id).is_none());
        assert!(!lock_mutex(&pool.state).by_id.contains_key(&connection_id));
        assert_eq!(connection.snapshot().health, ConnectionHealth::Closed);
    }

    #[test]
    fn pool_evicts_idle_http2_connections_after_timeout() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        let connection =
            make_connection_with_protocol(address.clone(), 31, ConnectionProtocol::Http2);
        let connection_id = connection.id();
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        assert!(pool.acquire(&address).is_none());
        assert_eq!(pool.stats(&address), PoolStats::default());
        assert!(pool.remove(connection_id).is_none());
    }

    #[test]
    fn pool_max_idle_limit_applies_to_http2_connections() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: None,
            max_idle_per_address: 0,
        });
        let connection =
            make_connection_with_protocol(address.clone(), 31, ConnectionProtocol::Http2);
        pool.insert(connection);

        assert_eq!(pool.stats(&address), PoolStats::default());
    }

    #[test]
    fn pool_removes_connections_from_coalescing_index_on_remove() {
        let first = address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2);
        let connection = make_connection_with_protocol_and_coalescing(
            first,
            41,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        );
        let connection_id = connection.id();
        let target = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 41), 443));
        let pool = ConnectionPool::new(PoolSettings::default());
        pool.insert(connection);

        assert!(lock_mutex(&pool.state)
            .coalesced_by_target
            .contains_key(&target));
        assert!(pool.remove(connection_id).is_some());
        assert!(!lock_mutex(&pool.state)
            .coalesced_by_target
            .contains_key(&target));
    }

    #[test]
    fn pool_prune_all_removes_expired_idle_connections_and_syncs_indices() {
        let address = address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2);
        let connection = make_connection_with_protocol_and_coalescing(
            address.clone(),
            41,
            ConnectionProtocol::Http2,
            &["a.test", "b.test"],
        );
        let connection_id = connection.id();
        let target = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 41), 443));
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        pool.prune_all();

        let state = lock_mutex(&pool.state);
        assert!(!state.by_address.contains_key(&address));
        assert!(!state.by_id.contains_key(&connection_id));
        assert!(!state.coalesced_by_target.contains_key(&target));
    }

    #[test]
    fn pool_prune_all_reaps_idle_addresses_without_future_traffic() {
        let stale_address = address_for_host("stale.test", None, ProtocolPolicy::Http1OrHttp2);
        let live_address = address_for_host("live.test", None, ProtocolPolicy::Http1OrHttp2);
        let stale = make_connection(stale_address.clone(), 51);
        let live = make_connection(live_address.clone(), 52);
        let stale_id = stale.id();
        let live_id = live.id();
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        pool.insert(stale.clone());
        pool.insert(live.clone());
        stale.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));
        live.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(2)));

        pool.prune_all();

        assert_eq!(pool.stats(&stale_address), PoolStats::default());
        assert_eq!(
            pool.stats(&live_address),
            PoolStats {
                total: 1,
                idle: 1,
                in_use: 0,
            }
        );
        let state = lock_mutex(&pool.state);
        assert!(!state.by_id.contains_key(&stale_id));
        assert_eq!(state.by_id.get(&live_id), Some(&live_address));
    }

    #[test]
    fn pool_recovers_after_mutex_poisoning() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = make_connection(address.clone(), 33);
        let connection_id = connection.id();
        pool.insert(connection);

        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = pool
                .state
                .lock()
                .expect("poison connection pool lock for test");
            panic!("poison connection pool");
        }));

        assert_eq!(
            pool.acquire(&address).map(|connection| connection.id()),
            Some(connection_id)
        );
    }

    #[test]
    fn pool_does_not_coalesce_same_authority_across_address_policy_boundaries() {
        let direct = address_with_proxy(None);
        let alt_dns = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            None,
            Some(crate::connection::TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("mobile".into()),
        );
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection =
            make_connection_with_protocol(direct.clone(), 32, ConnectionProtocol::Http2);
        pool.insert(connection);

        let route_plan = RoutePlan::new(
            vec![Route::direct(
                alt_dns.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 32), 443)),
            )],
            Duration::from_millis(250),
        );

        assert!(pool.acquire_coalesced(&alt_dns, &route_plan).is_none());
    }

    #[test]
    fn pool_reuses_http2_connections_without_a_local_stream_cap() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection =
            make_connection_with_protocol(address.clone(), 32, ConnectionProtocol::Http2);
        pool.insert(connection.clone());

        for _ in 0..128 {
            assert!(connection.try_acquire());
        }

        assert!(pool.acquire(&address).is_some());
        assert!(connection.release());
        assert!(pool.acquire(&address).is_some());
    }

    #[test]
    fn pool_reports_in_use_connections_after_pruning() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = make_connection(address.clone(), 61);
        pool.insert(connection.clone());

        assert!(!pool.has_in_use_connection(&address));

        assert!(connection.try_acquire());
        assert!(pool.has_in_use_connection(&address));

        assert!(connection.release());
        assert!(!pool.has_in_use_connection(&address));
    }

    #[test]
    fn pool_does_not_report_pruned_connections_as_in_use() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
        });
        let connection = make_connection(address.clone(), 62);
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        assert!(!pool.has_in_use_connection(&address));
    }
}
