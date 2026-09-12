#![allow(dead_code, unused_imports)]

mod exchange_finder;
mod fast_fallback;
mod limits;
mod planning;
mod pool;
mod real_connection;
mod scheduler;

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use rustc_hash::{FxBuildHasher, FxHasher};

pub(crate) use exchange_finder::{
    CachedAddresses, ExchangeFinder, ObservedConnection, PreparedExchange, PreparedExchangeOutcome,
    ResolvedAddress,
};
pub(crate) use fast_fallback::{
    with_connect_timeout, DirectDialDeps, FastFallbackDialer, FastFallbackOutcome,
    FastFallbackRuntime,
};
pub(crate) use limits::{
    ConnectionAvailability, ConnectionLimiter, ConnectionPermit, RequestAdmissionLimiter,
    RequestAdmissionPermit,
};
pub use planning::{
    Address, AuthorityKey, DefaultRoutePlanner, DnsPolicy, ProtocolPolicy, ProxyConfig,
    ProxyEndpoint, ProxyMode, ProxyScheme, Route, RouteFamily, RoutePlan, RoutePlanner,
    TlsIdentity, UriScheme,
};
pub(crate) use planning::{
    ConnectAttempt, ConnectAttemptState, ConnectFailure, ConnectFailureStage, ConnectPlan,
    DnsResolution, RouteKind, RoutePreference,
};
pub(crate) use pool::{ConnectionPool, PoolSettings, PoolStats};
pub(crate) use real_connection::{
    ConnectionAllocationState, ConnectionHealth, ConnectionProtocol, RealConnection,
    RealConnectionSnapshot, DEFAULT_HTTP2_MAX_LOCAL_STREAMS,
};
pub use scheduler::RequestPriority;

/// Number of address-keyed shards for the pool, limiter, and connection-wait
/// indexes. Independent hosts should not share a mutex. The request scheduler
/// is client-wide so priority can rank waiters across addresses.
pub(crate) const ADDRESS_SHARDS: usize = 32;

pub(crate) type FxHashMap<K, V> = hashbrown::HashMap<K, V, FxBuildHasher>;
pub(crate) type SipHashMap<K, V> =
    hashbrown::HashMap<K, V, std::collections::hash_map::RandomState>;

pub(crate) fn fx_hash_map<K, V>() -> FxHashMap<K, V> {
    hashbrown::HashMap::with_hasher(FxBuildHasher)
}

pub(crate) fn sip_hash_map<K, V>() -> SipHashMap<K, V> {
    hashbrown::HashMap::with_hasher(std::collections::hash_map::RandomState::new())
}

/// Cheap address hash folded with a process-local seed. Address-keyed maps
/// still use SipHash; this is only the shard selector.
pub(crate) fn address_shard(address: &Address) -> usize {
    static SEED: OnceLock<u64> = OnceLock::new();
    let seed = *SEED.get_or_init(|| {
        let addr = std::ptr::from_ref(&SEED) as u64;
        addr ^ 0x9E3779B97F4A7C15
    });
    let mut hasher = FxHasher::default();
    hasher.write_u64(seed);
    address.hash(&mut hasher);
    (hasher.finish() as usize) % ADDRESS_SHARDS
}
