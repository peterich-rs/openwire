#![allow(dead_code, unused_imports)]

mod exchange_finder;
mod fast_fallback;
mod limits;
mod planning;
mod pool;
mod real_connection;

pub(crate) use exchange_finder::{
    CachedAddresses, ExchangeFinder, ObservedConnection, PreparedExchange, PreparedExchangeOutcome,
    ResolvedAddress,
};
pub(crate) use fast_fallback::{
    DirectDialDeps, FastFallbackDialer, FastFallbackOutcome, FastFallbackRuntime,
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
    RealConnectionSnapshot,
};
