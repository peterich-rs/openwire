use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openwire_core::ConnectionId;

use super::{
    Address, ConnectionAllocationState, ConnectionProtocol, ProtocolPolicy, RealConnection, Route,
    RouteKind, RoutePlan, UriScheme,
};
use crate::sync_util::lock_mutex;

/// Invoked after a connection is removed from pool metadata so transport can
/// abort the owned hyper task and drop bindings. Must not re-enter the pool.
pub(crate) type PoolEvictionHook = Arc<dyn Fn(ConnectionId) + Send + Sync>;

/// Number of address-keyed pool shards. Keeps independent hosts off the same
/// mutex while preserving exact-address reuse semantics within a shard.
const POOL_SHARDS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolSettings {
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) max_idle_per_address: usize,
    pub(crate) max_lifetime: Option<Duration>,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Some(Duration::from_secs(300)),
            max_idle_per_address: 5,
            max_lifetime: Some(Duration::from_secs(600)),
        }
    }
}

impl PoolSettings {
    pub(crate) fn needs_reaper(&self) -> bool {
        self.idle_timeout.is_some() || self.max_lifetime.is_some()
    }

    pub(crate) fn reaper_interval_hint(&self) -> Duration {
        match (self.idle_timeout, self.max_lifetime) {
            (Some(idle), Some(lifetime)) => idle.min(lifetime),
            (Some(idle), None) => idle,
            (None, Some(lifetime)) => lifetime,
            (None, None) => Duration::from_secs(60),
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
    shards: Arc<[Mutex<PoolState>]>,
    /// Coalescing index is shared: candidates may live on different address
    /// shards. Guarded separately from per-address shards.
    coalesced_by_target: Mutex<HashMap<SocketAddr, Vec<RealConnection>>>,
    /// Global id → address map for remove-by-id without scanning shards.
    by_id: Mutex<HashMap<ConnectionId, Address>>,
    eviction_hook: Mutex<Option<PoolEvictionHook>>,
}

#[derive(Debug, Default)]
struct PoolState {
    by_address: HashMap<Address, Vec<RealConnection>>,
}

impl ConnectionPool {
    pub(crate) fn new(settings: PoolSettings) -> Self {
        let shards = (0..POOL_SHARDS)
            .map(|_| Mutex::new(PoolState::default()))
            .collect::<Vec<_>>();
        Self {
            settings,
            shards: Arc::<[Mutex<PoolState>]>::from(shards),
            coalesced_by_target: Mutex::new(HashMap::new()),
            by_id: Mutex::new(HashMap::new()),
            eviction_hook: Mutex::new(None),
        }
    }

    /// Installs a hook that runs after a connection leaves the pool.
    ///
    /// Transport uses this to abort hyper connection tasks and clear bindings
    /// so idle eviction actually closes sockets.
    pub(crate) fn set_eviction_hook(&self, hook: PoolEvictionHook) {
        *lock_mutex(&self.eviction_hook) = Some(hook);
    }

    pub(crate) fn settings(&self) -> &PoolSettings {
        &self.settings
    }

    fn shard(&self, address: &Address) -> &Mutex<PoolState> {
        &self.shards[address_shard(address)]
    }

    pub(crate) fn insert(&self, connection: RealConnection) {
        let address = connection.address().clone();
        let mut evicted = Vec::new();
        {
            let mut state = lock_mutex(self.shard(&address));
            state
                .by_address
                .entry(address.clone())
                .or_default()
                .push(connection.clone());
            evicted.extend(prune_address(&self.settings, &mut state, &address));
        }
        lock_mutex(&self.by_id).insert(connection.id(), address);
        self.index_coalescing(&connection);
        self.finish_removals(evicted);
    }

    pub(crate) fn acquire(&self, address: &Address) -> Option<RealConnection> {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let result = state
            .by_address
            .get_mut(address)
            .and_then(|connections| connections.iter().find(|conn| conn.try_acquire()).cloned());
        drop(state);
        self.finish_removals(removed);
        result
    }

    pub(crate) fn acquire_with_in_use_hint(
        &self,
        address: &Address,
    ) -> (Option<RealConnection>, bool) {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let result = match state.by_address.get_mut(address) {
            Some(connections) => {
                let connection = connections.iter().find(|conn| conn.try_acquire()).cloned();
                let has_in_use = has_in_use_connection_unpruned(
                    connections,
                    connection.as_ref().map(RealConnection::id),
                );
                (connection, has_in_use)
            }
            None => (None, false),
        };
        drop(state);
        self.finish_removals(removed);
        result
    }

    pub(crate) fn has_in_use_connection(&self, address: &Address) -> bool {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let result = state
            .by_address
            .get(address)
            .is_some_and(|connections| has_in_use_connection_unpruned(connections, None));
        drop(state);
        self.finish_removals(removed);
        result
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

        let mut addresses_to_prune = HashSet::new();
        {
            let coalesced = lock_mutex(&self.coalesced_by_target);
            for target in &direct_targets {
                if let Some(bucket) = coalesced.get(target) {
                    addresses_to_prune
                        .extend(bucket.iter().map(|connection| connection.address().clone()));
                }
            }
        }

        let mut removed = Vec::new();
        for candidate_address in &addresses_to_prune {
            let mut state = lock_mutex(self.shard(candidate_address));
            removed.extend(prune_address(
                &self.settings,
                &mut state,
                candidate_address,
            ));
        }
        self.finish_removals(removed);

        let mut candidates = Vec::new();
        let mut seen_ids = HashSet::new();
        {
            let mut coalesced = lock_mutex(&self.coalesced_by_target);
            for target in direct_targets {
                prune_coalescing_bucket(&mut coalesced, target);
                let Some(bucket) = coalesced.get(&target) else {
                    continue;
                };

                for connection in bucket {
                    if seen_ids.insert(connection.id())
                        && can_coalesce(connection, address, route_plan)
                    {
                        candidates.push(connection.clone());
                    }
                }
            }
        }

        candidates
            .into_iter()
            .find(|connection| connection.try_acquire())
    }

    pub(crate) fn release(&self, connection: &RealConnection) -> bool {
        if !connection.release() {
            return false;
        }

        let address = connection.address();
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        drop(state);
        self.finish_removals(removed);
        true
    }

    pub(crate) fn get_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let result = if lock_mutex(&self.by_id).get(&connection_id) != Some(address) {
            None
        } else {
            state.by_address.get(address).and_then(|connections| {
                connections
                    .iter()
                    .find(|connection| connection.id() == connection_id)
                    .cloned()
            })
        };
        drop(state);
        self.finish_removals(removed);
        result
    }

    pub(crate) fn acquire_by_id(
        &self,
        address: &Address,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let result = if lock_mutex(&self.by_id).get(&connection_id) != Some(address) {
            None
        } else {
            state.by_address.get_mut(address).and_then(|connections| {
                connections.iter().find_map(|connection| {
                    (connection.id() == connection_id && connection.try_acquire())
                        .then(|| connection.clone())
                })
            })
        };
        drop(state);
        self.finish_removals(removed);
        result
    }

    pub(crate) fn remove(&self, connection_id: ConnectionId) -> Option<RealConnection> {
        let removed = self.remove_without_hook(connection_id);
        if removed.is_some() {
            self.notify_evictions(vec![connection_id]);
        }
        removed
    }

    /// Removes pool metadata without running the eviction hook.
    ///
    /// Used by transport teardown after it has already aborted the connection
    /// task, so the hook does not re-enter abort/remove.
    pub(crate) fn remove_without_hook(
        &self,
        connection_id: ConnectionId,
    ) -> Option<RealConnection> {
        let address = lock_mutex(&self.by_id).remove(&connection_id)?;
        let mut state = lock_mutex(self.shard(&address));
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
            remove_index_connection(&mut lock_mutex(&self.coalesced_by_target), connection);
        }

        removed
    }

    pub(crate) fn stats(&self, address: &Address) -> PoolStats {
        let mut state = lock_mutex(self.shard(address));
        let removed = prune_address(&self.settings, &mut state, address);
        let stats = match state.by_address.get(address) {
            Some(connections) => connections.iter().fold(
                PoolStats::default(),
                |mut stats, connection| {
                    stats.total += 1;
                    match connection.snapshot().allocation {
                        ConnectionAllocationState::Idle => stats.idle += 1,
                        ConnectionAllocationState::InUse { .. } => stats.in_use += 1,
                        ConnectionAllocationState::Closed => {}
                    }
                    stats
                },
            ),
            None => PoolStats::default(),
        };
        drop(state);
        self.finish_removals(removed);
        stats
    }

    pub(crate) fn prune_all(&self) {
        let mut removed = Vec::new();
        for shard in self.shards.iter() {
            let mut state = lock_mutex(shard);
            let addresses = state.by_address.keys().cloned().collect::<Vec<_>>();
            for address in addresses {
                removed.extend(prune_address(&self.settings, &mut state, &address));
            }
        }
        self.finish_removals(removed);
    }

    fn index_coalescing(&self, connection: &RealConnection) {
        let Some(target) = coalescing_index_target(connection) else {
            return;
        };
        lock_mutex(&self.coalesced_by_target)
            .entry(target)
            .or_default()
            .push(connection.clone());
    }

    fn finish_removals(&self, removed: Vec<RealConnection>) {
        if removed.is_empty() {
            return;
        }
        let ids = removed.iter().map(RealConnection::id).collect::<Vec<_>>();
        {
            let mut by_id = lock_mutex(&self.by_id);
            for connection in &removed {
                by_id.remove(&connection.id());
            }
        }
        {
            let mut coalesced = lock_mutex(&self.coalesced_by_target);
            for connection in &removed {
                remove_index_connection(&mut coalesced, connection);
            }
        }
        self.notify_evictions(ids);
    }

    fn notify_evictions(&self, connection_ids: Vec<ConnectionId>) {
        if connection_ids.is_empty() {
            return;
        }
        let hook = lock_mutex(&self.eviction_hook).clone();
        let Some(hook) = hook else {
            return;
        };
        for connection_id in connection_ids {
            hook(connection_id);
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

/// Returns connections closed and removed from the address bucket.
fn prune_address(
    settings: &PoolSettings,
    state: &mut PoolState,
    address: &Address,
) -> Vec<RealConnection> {
    let (removed, empty) = {
        let Some(connections) = state.by_address.get_mut(address) else {
            return Vec::new();
        };

        let removed = prune_connections(settings, connections);
        (removed, connections.is_empty())
    };

    if empty {
        state.by_address.remove(address);
    }

    removed
}

fn address_shard(address: &Address) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    address.hash(&mut hasher);
    (hasher.finish() as usize) % POOL_SHARDS
}

fn prune_connections(
    settings: &PoolSettings,
    connections: &mut Vec<RealConnection>,
) -> Vec<RealConnection> {
    let mut removed = Vec::new();
    connections.retain(|connection| {
        if !connection.is_healthy() {
            // Keep unhealthy connections that still have live allocations so
            // HTTP/2 multiplex bookkeeping can drain cleanly. They are not
            // re-acquired because try_acquire requires Healthy.
            if matches!(
                connection.snapshot().allocation,
                ConnectionAllocationState::InUse { .. }
            ) {
                return true;
            }
            connection.close();
            removed.push(connection.clone());
            return false;
        }

        if settings
            .max_lifetime
            .is_some_and(|lifetime| connection_lifetime_expired(connection, lifetime))
        {
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

fn has_in_use_connection_unpruned(
    connections: &[RealConnection],
    excluded_id: Option<ConnectionId>,
) -> bool {
    connections.iter().any(|connection| {
        excluded_id != Some(connection.id())
            && matches!(
                connection.snapshot().allocation,
                ConnectionAllocationState::InUse { .. }
            )
    })
}

fn idle_connection_expired(connection: &RealConnection, timeout: Duration) -> bool {
    let snapshot = connection.snapshot();
    matches!(snapshot.allocation, ConnectionAllocationState::Idle)
        && snapshot
            .idle_since
            .is_some_and(|idle_since| idle_since.elapsed() >= timeout)
}

fn connection_lifetime_expired(connection: &RealConnection, max_lifetime: Duration) -> bool {
    // Only reclaim idle connections for max-lifetime; in-use streams finish first.
    let snapshot = connection.snapshot();
    matches!(snapshot.allocation, ConnectionAllocationState::Idle)
        && connection.created_at().elapsed() >= max_lifetime
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

fn prune_coalescing_bucket(
    coalesced: &mut HashMap<SocketAddr, Vec<RealConnection>>,
    target: SocketAddr,
) {
    let should_remove = if let Some(bucket) = coalesced.get_mut(&target) {
        bucket.retain(|connection| {
            coalescing_index_target(connection) == Some(target) && !connection.is_closed()
        });
        bucket.is_empty()
    } else {
        false
    };
    if should_remove {
        coalesced.remove(&target);
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
            max_lifetime: Some(Duration::from_secs(600)),
        });

        assert_eq!(
            pool.settings(),
            &PoolSettings {
                idle_timeout: Some(Duration::from_secs(30)),
                max_idle_per_address: 2,
                max_lifetime: Some(Duration::from_secs(600)),
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
            lock_mutex(&pool.by_id).get(&connection_id),
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
        assert!(!lock_mutex(&pool.by_id).contains_key(&connection_id));
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
            max_lifetime: None,
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
            max_lifetime: None,
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
            max_lifetime: None,
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
        assert!(!lock_mutex(&pool.by_id).contains_key(&connection_id));
        assert_eq!(connection.snapshot().health, ConnectionHealth::Closed);
    }

    #[test]
    fn pool_evicts_idle_http2_connections_after_timeout() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
            max_lifetime: None,
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
            max_lifetime: None,
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

        assert!(lock_mutex(&pool.coalesced_by_target).contains_key(&target));
        assert!(pool.remove(connection_id).is_some());
        assert!(!lock_mutex(&pool.coalesced_by_target).contains_key(&target));
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
            max_lifetime: None,
        });
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        pool.prune_all();

        assert!(!lock_mutex(pool.shard(&address))
            .by_address
            .contains_key(&address));
        assert!(!lock_mutex(&pool.by_id).contains_key(&connection_id));
        assert!(!lock_mutex(&pool.coalesced_by_target).contains_key(&target));
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
            max_lifetime: None,
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
        assert!(!lock_mutex(&pool.by_id).contains_key(&stale_id));
        assert_eq!(
            lock_mutex(&pool.by_id).get(&live_id),
            Some(&live_address)
        );
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
                .shard(&address)
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
    fn pool_reuses_http2_connections_until_local_stream_cap() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection = RealConnection::with_id_permit_coalescing_and_stream_cap(
            openwire_core::next_connection_id(),
            Route::direct(
                address.clone(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 32), 443)),
            ),
            ConnectionProtocol::Http2,
            None,
            CoalescingInfo::default(),
            4,
        );
        pool.insert(connection.clone());

        for _ in 0..4 {
            assert!(connection.try_acquire());
        }
        assert!(pool.acquire(&address).is_none());
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
    fn pool_acquire_with_in_use_hint_reuses_the_same_pruned_snapshot() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let busy = make_connection(address.clone(), 62);
        let idle = make_connection(address.clone(), 63);
        pool.insert(busy.clone());
        pool.insert(idle.clone());

        assert!(busy.try_acquire());

        let (acquired, has_in_use) = pool.acquire_with_in_use_hint(&address);

        assert_eq!(acquired.map(|connection| connection.id()), Some(idle.id()));
        assert!(has_in_use);
    }

    #[test]
    fn pool_acquire_with_in_use_hint_excludes_the_selected_connection() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings::default());
        let connection =
            make_connection_with_protocol(address.clone(), 64, ConnectionProtocol::Http2);
        pool.insert(connection.clone());

        assert!(connection.try_acquire());

        let (acquired, has_in_use) = pool.acquire_with_in_use_hint(&address);

        assert_eq!(
            acquired.map(|selected| selected.id()),
            Some(connection.id())
        );
        assert!(!has_in_use);
    }

    #[test]
    fn pool_does_not_report_pruned_connections_as_in_use() {
        let address = address_with_proxy(None);
        let pool = ConnectionPool::new(PoolSettings {
            idle_timeout: Some(Duration::from_secs(5)),
            max_idle_per_address: usize::MAX,
            max_lifetime: None,
        });
        let connection = make_connection(address.clone(), 62);
        pool.insert(connection.clone());
        connection.set_idle_since_for_test(Some(Instant::now() - Duration::from_secs(6)));

        assert!(!pool.has_in_use_connection(&address));
    }
    #[test]
    fn different_hosts_map_to_independent_shards() {
        let a = address_for_host("a.test", None, ProtocolPolicy::Http1OrHttp2);
        let b = address_for_host("b.test", None, ProtocolPolicy::Http1OrHttp2);
        assert_eq!(super::address_shard(&a), super::address_shard(&a));
        assert_eq!(super::address_shard(&b), super::address_shard(&b));
        let pool = ConnectionPool::new(PoolSettings::default());
        pool.insert(make_connection(a.clone(), 1));
        pool.insert(make_connection(b.clone(), 2));
        assert!(pool.acquire(&a).is_some());
        assert!(pool.acquire(&b).is_some());
        assert!(pool.acquire(&a).is_none());
        assert!(pool.acquire(&b).is_none());
    }

}
