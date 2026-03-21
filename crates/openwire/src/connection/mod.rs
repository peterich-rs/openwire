#![allow(dead_code, unused_imports)]

mod planning;
mod pool;
mod real_connection;

pub(crate) use planning::{
    Address, AuthorityKey, ConnectAttempt, ConnectAttemptState, ConnectFailure,
    ConnectFailureStage, ConnectPlan, DnsPolicy, DnsResolution, ProtocolPolicy, ProxyConfig,
    ProxyEndpoint, ProxyMode, ProxyScheme, Route, RouteFamily, RouteKind, RoutePlan, RoutePlanner,
    TlsIdentity, UriScheme,
};
pub(crate) use pool::{ConnectionPool, PoolSettings, PoolStats};
pub(crate) use real_connection::{
    ConnectionAllocationState, ConnectionHealth, ConnectionProtocol, RealConnection,
    RealConnectionSnapshot,
};
