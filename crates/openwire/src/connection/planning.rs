use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use hyper::Uri;
use openwire_core::WireError;

use crate::proxy::{Proxy, ProxyCredentials};

const DEFAULT_FAST_FALLBACK_STAGGER: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UriScheme {
    Http,
    Https,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AuthorityKey {
    host: String,
    port: u16,
}

impl AuthorityKey {
    pub(crate) fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: normalize_host(host.into()),
            port,
        }
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TlsIdentity {
    server_name: String,
}

impl TlsIdentity {
    pub(crate) fn new(server_name: impl Into<String>) -> Self {
        Self {
            server_name: normalize_host(server_name.into()),
        }
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ProtocolPolicy {
    Http1Only,
    Http2Only,
    Http1OrHttp2,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DnsPolicy {
    System,
    Custom(String),
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ProxyScheme {
    Http,
    Socks5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ProxyMode {
    Forward,
    Connect,
    SocksTunnel,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProxyEndpoint {
    scheme: ProxyScheme,
    authority: AuthorityKey,
    credentials: Option<ProxyCredentials>,
}

impl ProxyEndpoint {
    pub(crate) fn new(scheme: ProxyScheme, host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme,
            authority: AuthorityKey::new(host, port),
            credentials: None,
        }
    }

    pub(crate) fn scheme(&self) -> ProxyScheme {
        self.scheme
    }

    pub(crate) fn authority(&self) -> &AuthorityKey {
        &self.authority
    }

    pub(crate) fn with_credentials(mut self, credentials: ProxyCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    pub(crate) fn credentials(&self) -> Option<&ProxyCredentials> {
        self.credentials.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProxyConfig {
    mode: ProxyMode,
    endpoint: ProxyEndpoint,
}

impl ProxyConfig {
    pub(crate) fn new(mode: ProxyMode, endpoint: ProxyEndpoint) -> Self {
        Self { mode, endpoint }
    }

    pub(crate) fn mode(&self) -> ProxyMode {
        self.mode
    }

    pub(crate) fn endpoint(&self) -> &ProxyEndpoint {
        &self.endpoint
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Address {
    scheme: UriScheme,
    authority: AuthorityKey,
    proxy: Option<ProxyConfig>,
    tls_identity: Option<TlsIdentity>,
    protocol_policy: ProtocolPolicy,
    dns_policy: DnsPolicy,
}

impl Address {
    pub(crate) fn new(
        scheme: UriScheme,
        authority: AuthorityKey,
        proxy: Option<ProxyConfig>,
        tls_identity: Option<TlsIdentity>,
        protocol_policy: ProtocolPolicy,
        dns_policy: DnsPolicy,
    ) -> Self {
        Self {
            scheme,
            authority,
            proxy,
            tls_identity,
            protocol_policy,
            dns_policy,
        }
    }

    pub(crate) fn from_uri(uri: &Uri, proxy: Option<&Proxy>) -> Result<Self, WireError> {
        let scheme = match uri.scheme_str() {
            Some(scheme) if scheme.eq_ignore_ascii_case("https") => UriScheme::Https,
            Some(scheme) if scheme.eq_ignore_ascii_case("http") => UriScheme::Http,
            Some(scheme) => {
                return Err(WireError::invalid_request(format!(
                    "unsupported URI scheme for address derivation: {scheme}"
                )));
            }
            None => {
                return Err(WireError::invalid_request(
                    "request URI is missing a scheme",
                ))
            }
        };

        let host = uri
            .host()
            .ok_or_else(|| WireError::invalid_request("request URI is missing a host"))?;
        let port = default_port(uri, scheme);
        let authority = AuthorityKey::new(host, port);
        let tls_identity = matches!(scheme, UriScheme::Https).then(|| TlsIdentity::new(host));
        let proxy = proxy
            .map(|proxy| ProxyConfig::from_runtime_proxy(scheme, proxy))
            .transpose()?;

        Ok(Self::new(
            scheme,
            authority,
            proxy,
            tls_identity,
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        ))
    }

    pub(crate) fn scheme(&self) -> UriScheme {
        self.scheme
    }

    pub(crate) fn authority(&self) -> &AuthorityKey {
        &self.authority
    }

    pub(crate) fn proxy(&self) -> Option<&ProxyConfig> {
        self.proxy.as_ref()
    }

    pub(crate) fn tls_identity(&self) -> Option<&TlsIdentity> {
        self.tls_identity.as_ref()
    }

    pub(crate) fn protocol_policy(&self) -> ProtocolPolicy {
        self.protocol_policy
    }

    pub(crate) fn dns_policy(&self) -> &DnsPolicy {
        &self.dns_policy
    }
}

impl ProxyConfig {
    fn from_runtime_proxy(scheme: UriScheme, proxy: &Proxy) -> Result<Self, WireError> {
        let endpoint_scheme = match proxy.target().scheme() {
            "http" => ProxyScheme::Http,
            "socks5" => ProxyScheme::Socks5,
            unsupported => {
                return Err(WireError::invalid_request(format!(
                    "unsupported proxy URL scheme: {unsupported}"
                )));
            }
        };
        let endpoint = ProxyEndpoint::new(
            endpoint_scheme,
            proxy
                .target()
                .host_str()
                .ok_or_else(|| WireError::invalid_request("proxy URL is missing a host"))?,
            proxy.target().port_or_known_default().ok_or_else(|| {
                WireError::invalid_request("proxy URL is missing a port and has no known default")
            })?,
        );
        let endpoint = match proxy.credentials() {
            Some(credentials) => endpoint.with_credentials(credentials.clone()),
            None => endpoint,
        };

        let mode = match (endpoint_scheme, scheme) {
            (ProxyScheme::Http, UriScheme::Http) if proxy.intercepts_http() => ProxyMode::Forward,
            (ProxyScheme::Http, UriScheme::Https) if proxy.intercepts_https() => ProxyMode::Connect,
            (ProxyScheme::Socks5, UriScheme::Http) if proxy.intercepts_http() => {
                ProxyMode::SocksTunnel
            }
            (ProxyScheme::Socks5, UriScheme::Https) if proxy.intercepts_https() => {
                ProxyMode::SocksTunnel
            }
            _ => {
                return Err(WireError::invalid_request(
                    "proxy does not apply to the request scheme",
                ));
            }
        };

        Ok(Self::new(mode, endpoint))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RouteFamily {
    Ipv4,
    Ipv6,
}

impl RouteFamily {
    fn from_socket_addr(addr: SocketAddr) -> Self {
        match addr.ip() {
            IpAddr::V4(_) => Self::Ipv4,
            IpAddr::V6(_) => Self::Ipv6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DnsResolution {
    Local(SocketAddr),
    Deferred { host: String, port: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RouteKind {
    Direct {
        target: SocketAddr,
    },
    HttpForwardProxy {
        proxy: SocketAddr,
        credentials: Option<ProxyCredentials>,
    },
    ConnectProxy {
        proxy: SocketAddr,
        credentials: Option<ProxyCredentials>,
    },
    SocksProxy {
        proxy: SocketAddr,
        credentials: Option<ProxyCredentials>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route {
    address: Address,
    family: RouteFamily,
    kind: RouteKind,
    target_dns: DnsResolution,
    proxy_dns: Option<DnsResolution>,
}

impl Route {
    pub(crate) fn direct(address: Address, target: SocketAddr) -> Self {
        Self {
            family: RouteFamily::from_socket_addr(target),
            kind: RouteKind::Direct { target },
            target_dns: DnsResolution::Local(target),
            proxy_dns: None,
            address,
        }
    }

    pub(crate) fn http_forward(address: Address, proxy: SocketAddr) -> Self {
        let credentials = address
            .proxy()
            .and_then(|proxy| proxy.endpoint().credentials())
            .cloned();
        Self::proxy_route(
            address,
            proxy,
            RouteKind::HttpForwardProxy { proxy, credentials },
        )
    }

    pub(crate) fn connect_proxy(address: Address, proxy: SocketAddr) -> Self {
        let credentials = address
            .proxy()
            .and_then(|proxy| proxy.endpoint().credentials())
            .cloned();
        Self::proxy_route(
            address,
            proxy,
            RouteKind::ConnectProxy { proxy, credentials },
        )
    }

    pub(crate) fn socks_proxy(address: Address, proxy: SocketAddr) -> Self {
        let credentials = address
            .proxy()
            .and_then(|proxy| proxy.endpoint().credentials())
            .cloned();
        Self::proxy_route(address, proxy, RouteKind::SocksProxy { proxy, credentials })
    }

    pub(crate) fn from_observed(address: Address, remote_addr: Option<SocketAddr>) -> Self {
        let proxy = address.proxy().cloned();
        let fallback_addr = remote_addr.unwrap_or_else(|| {
            let port = proxy
                .as_ref()
                .map(|proxy| proxy.endpoint().authority().port())
                .unwrap_or_else(|| address.authority().port());
            SocketAddr::from(([0, 0, 0, 0], port))
        });

        match proxy.map(|proxy| proxy.mode()) {
            Some(ProxyMode::Forward) => Self::http_forward(address, fallback_addr),
            Some(ProxyMode::Connect) => Self::connect_proxy(address, fallback_addr),
            Some(ProxyMode::SocksTunnel) => Self::socks_proxy(address, fallback_addr),
            None => Self::direct(address, fallback_addr),
        }
    }

    fn proxy_route(address: Address, proxy: SocketAddr, kind: RouteKind) -> Self {
        let authority = address.authority();
        Self {
            family: RouteFamily::from_socket_addr(proxy),
            kind,
            target_dns: DnsResolution::Deferred {
                host: authority.host().to_owned(),
                port: authority.port(),
            },
            proxy_dns: Some(DnsResolution::Local(proxy)),
            address,
        }
    }

    pub(crate) fn address(&self) -> &Address {
        &self.address
    }

    pub(crate) fn family(&self) -> RouteFamily {
        self.family
    }

    pub(crate) fn kind(&self) -> &RouteKind {
        &self.kind
    }

    pub(crate) fn target_dns(&self) -> &DnsResolution {
        &self.target_dns
    }

    pub(crate) fn proxy_dns(&self) -> Option<&DnsResolution> {
        self.proxy_dns.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutePlan {
    routes: Vec<Route>,
    fast_fallback_stagger: Duration,
}

impl RoutePlan {
    pub(crate) fn new(routes: Vec<Route>, fast_fallback_stagger: Duration) -> Self {
        Self {
            routes,
            fast_fallback_stagger,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.routes.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    pub(crate) fn route(&self, index: usize) -> Option<&Route> {
        self.routes.get(index)
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &Route> {
        self.routes.iter()
    }

    pub(crate) fn fast_fallback_stagger(&self) -> Duration {
        self.fast_fallback_stagger
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectFailureStage {
    Tcp,
    Tls,
    ProtocolBinding,
    ProxyTunnel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectFailure {
    stage: ConnectFailureStage,
    message: String,
}

impl ConnectFailure {
    pub(crate) fn new(stage: ConnectFailureStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }

    pub(crate) fn stage(&self) -> ConnectFailureStage {
        self.stage
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectAttemptState {
    Pending,
    Running,
    Won,
    Lost { winner_index: usize },
    Failed(ConnectFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectAttempt {
    route: Route,
    scheduled_after: Duration,
    state: ConnectAttemptState,
}

impl ConnectAttempt {
    pub(crate) fn route(&self) -> &Route {
        &self.route
    }

    pub(crate) fn scheduled_after(&self) -> Duration {
        self.scheduled_after
    }

    pub(crate) fn state(&self) -> &ConnectAttemptState {
        &self.state
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectPlan {
    attempts: Vec<ConnectAttempt>,
    fast_fallback_stagger: Duration,
}

impl ConnectPlan {
    pub(crate) fn from_route_plan(route_plan: &RoutePlan) -> Self {
        let attempts = route_plan
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, route)| ConnectAttempt {
                route,
                scheduled_after: route_plan.fast_fallback_stagger() * index as u32,
                state: ConnectAttemptState::Pending,
            })
            .collect();

        Self {
            attempts,
            fast_fallback_stagger: route_plan.fast_fallback_stagger(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.attempts.len()
    }

    pub(crate) fn attempt(&self, index: usize) -> Option<&ConnectAttempt> {
        self.attempts.get(index)
    }

    pub(crate) fn fast_fallback_stagger(&self) -> Duration {
        self.fast_fallback_stagger
    }

    pub(crate) fn mark_running(&mut self, index: usize) -> bool {
        self.transition(index, ConnectAttemptState::Running)
    }

    pub(crate) fn mark_failed(&mut self, index: usize, failure: ConnectFailure) -> bool {
        self.transition(index, ConnectAttemptState::Failed(failure))
    }

    pub(crate) fn mark_lost(&mut self, index: usize, winner_index: usize) -> bool {
        self.transition(index, ConnectAttemptState::Lost { winner_index })
    }

    pub(crate) fn promote_winner(&mut self, winner_index: usize) -> bool {
        if !self.transition(winner_index, ConnectAttemptState::Won) {
            return false;
        }

        for (index, attempt) in self.attempts.iter_mut().enumerate() {
            if index == winner_index {
                continue;
            }

            if matches!(
                attempt.state,
                ConnectAttemptState::Pending | ConnectAttemptState::Running
            ) {
                attempt.state = ConnectAttemptState::Lost { winner_index };
            }
        }

        true
    }

    fn transition(&mut self, index: usize, state: ConnectAttemptState) -> bool {
        let Some(attempt) = self.attempts.get_mut(index) else {
            return false;
        };
        attempt.state = state;
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RoutePlanner {
    fast_fallback_stagger: Duration,
}

impl Default for RoutePlanner {
    fn default() -> Self {
        Self::new(DEFAULT_FAST_FALLBACK_STAGGER)
    }
}

impl RoutePlanner {
    pub(crate) fn new(fast_fallback_stagger: Duration) -> Self {
        Self {
            fast_fallback_stagger,
        }
    }

    pub(crate) fn fast_fallback_stagger(&self) -> Duration {
        self.fast_fallback_stagger
    }

    pub(crate) fn dns_target<'a>(&self, address: &'a Address) -> (&'a str, u16) {
        if let Some(proxy) = address.proxy() {
            (
                proxy.endpoint().authority().host(),
                proxy.endpoint().authority().port(),
            )
        } else {
            (address.authority().host(), address.authority().port())
        }
    }

    pub(crate) fn plan(
        &self,
        address: Address,
        resolved_addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> RoutePlan {
        match address.proxy().map(ProxyConfig::mode) {
            Some(ProxyMode::Forward) => self.plan_http_forward(address, resolved_addrs),
            Some(ProxyMode::Connect) => self.plan_connect_proxy(address, resolved_addrs),
            Some(ProxyMode::SocksTunnel) => self.plan_socks_proxy(address, resolved_addrs),
            None => self.plan_direct(address, resolved_addrs),
        }
    }

    pub(crate) fn plan_direct(
        &self,
        address: Address,
        resolved_addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> RoutePlan {
        let ordered = order_for_fast_fallback(resolved_addrs);
        RoutePlan::new(
            ordered
                .into_iter()
                .map(|addr| Route::direct(address.clone(), addr))
                .collect(),
            self.fast_fallback_stagger,
        )
    }

    pub(crate) fn plan_http_forward(
        &self,
        address: Address,
        resolved_proxy_addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> RoutePlan {
        let ordered = order_for_fast_fallback(resolved_proxy_addrs);
        RoutePlan::new(
            ordered
                .into_iter()
                .map(|addr| Route::http_forward(address.clone(), addr))
                .collect(),
            self.fast_fallback_stagger,
        )
    }

    pub(crate) fn plan_connect_proxy(
        &self,
        address: Address,
        resolved_proxy_addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> RoutePlan {
        let ordered = order_for_fast_fallback(resolved_proxy_addrs);
        RoutePlan::new(
            ordered
                .into_iter()
                .map(|addr| Route::connect_proxy(address.clone(), addr))
                .collect(),
            self.fast_fallback_stagger,
        )
    }

    pub(crate) fn plan_socks_proxy(
        &self,
        address: Address,
        resolved_proxy_addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> RoutePlan {
        let ordered = order_for_fast_fallback(resolved_proxy_addrs);
        RoutePlan::new(
            ordered
                .into_iter()
                .map(|addr| Route::socks_proxy(address.clone(), addr))
                .collect(),
            self.fast_fallback_stagger,
        )
    }
}

fn order_for_fast_fallback(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let addrs = addrs.into_iter().collect::<Vec<_>>();
    let Some(first) = addrs.first().copied() else {
        return Vec::new();
    };

    let first_family = RouteFamily::from_socket_addr(first);
    let mut by_family = HashMap::<RouteFamily, VecDeque<SocketAddr>>::new();

    for addr in addrs {
        by_family
            .entry(RouteFamily::from_socket_addr(addr))
            .or_default()
            .push_back(addr);
    }

    let mut ordered = Vec::new();
    let mut next_family = first_family;

    loop {
        let primary = by_family
            .get_mut(&next_family)
            .and_then(VecDeque::pop_front);
        let secondary_family = match next_family {
            RouteFamily::Ipv4 => RouteFamily::Ipv6,
            RouteFamily::Ipv6 => RouteFamily::Ipv4,
        };
        let candidate = primary.or_else(|| {
            by_family
                .get_mut(&secondary_family)
                .and_then(VecDeque::pop_front)
        });

        let Some(addr) = candidate else {
            break;
        };

        ordered.push(addr);
        next_family = secondary_family;
    }

    ordered
}

fn default_port(uri: &Uri, scheme: UriScheme) -> u16 {
    uri.port_u16().unwrap_or(match scheme {
        UriScheme::Http => 80,
        UriScheme::Https => 443,
    })
}

fn normalize_host(host: impl Into<String>) -> String {
    host.into().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::time::Duration;

    use hyper::Uri;

    use super::{
        Address, AuthorityKey, ConnectAttemptState, ConnectFailure, ConnectFailureStage,
        ConnectPlan, DnsPolicy, DnsResolution, ProtocolPolicy, ProxyConfig, ProxyEndpoint,
        ProxyMode, ProxyScheme, RouteFamily, RouteKind, RoutePlanner, TlsIdentity, UriScheme,
    };
    use crate::Proxy;

    fn http_address() -> Address {
        Address::new(
            UriScheme::Http,
            AuthorityKey::new("example.com", 80),
            None,
            None,
            ProtocolPolicy::Http1Only,
            DnsPolicy::System,
        )
    }

    fn https_proxy_address(mode: ProxyMode) -> Address {
        Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            Some(ProxyConfig::new(
                mode,
                ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
            )),
            Some(TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("mobile".into()),
        )
    }

    fn socket_v4(last: u8) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(192, 0, 2, last), 443))
    }

    fn socket_v6(last: u16) -> SocketAddr {
        SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last), 443))
    }

    #[test]
    fn address_equality_and_hash_include_reuse_key_fields() {
        let base = Address::new(
            UriScheme::Https,
            AuthorityKey::new("EXAMPLE.COM", 443),
            Some(ProxyConfig::new(
                ProxyMode::Connect,
                ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
            )),
            Some(TlsIdentity::new("EXAMPLE.COM")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("cellular".into()),
        );

        let same = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            Some(ProxyConfig::new(
                ProxyMode::Connect,
                ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
            )),
            Some(TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::Custom("cellular".into()),
        );

        let different_dns = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            Some(ProxyConfig::new(
                ProxyMode::Connect,
                ProxyEndpoint::new(ProxyScheme::Http, "proxy.internal", 8080),
            )),
            Some(TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        );

        let mut set = HashSet::new();
        set.insert(base.clone());

        assert_eq!(base, same);
        assert!(set.contains(&same));
        assert_ne!(base, different_dns);
    }

    #[test]
    fn route_classification_covers_direct_and_proxy_shapes() {
        let direct = super::Route::direct(http_address(), socket_v4(10));
        assert_eq!(direct.family(), RouteFamily::Ipv4);
        assert!(matches!(direct.kind(), RouteKind::Direct { .. }));
        assert_eq!(direct.target_dns(), &DnsResolution::Local(socket_v4(10)));
        assert_eq!(direct.proxy_dns(), None);

        let forward =
            super::Route::http_forward(https_proxy_address(ProxyMode::Forward), socket_v4(20));
        assert!(matches!(forward.kind(), RouteKind::HttpForwardProxy { .. }));
        assert!(matches!(
            forward.target_dns(),
            DnsResolution::Deferred { host, port } if host == "example.com" && *port == 443
        ));
        assert_eq!(
            forward.proxy_dns(),
            Some(&DnsResolution::Local(socket_v4(20)))
        );

        let connect =
            super::Route::connect_proxy(https_proxy_address(ProxyMode::Connect), socket_v6(30));
        assert!(matches!(connect.kind(), RouteKind::ConnectProxy { .. }));
        assert_eq!(connect.family(), RouteFamily::Ipv6);

        let socks =
            super::Route::socks_proxy(https_proxy_address(ProxyMode::SocksTunnel), socket_v4(40));
        assert!(matches!(socks.kind(), RouteKind::SocksProxy { .. }));
    }

    #[test]
    fn socks_proxy_routes_preserve_proxy_credentials() {
        let proxy =
            Proxy::socks5("socks5://alice:secret@proxy.internal:1080").expect("proxy config");
        let address = Address::from_uri(
            &"http://example.com/".parse::<Uri>().expect("uri"),
            Some(&proxy),
        )
        .expect("address");
        let route = super::Route::socks_proxy(address, socket_v4(50));

        let RouteKind::SocksProxy {
            credentials: Some(credentials),
            ..
        } = route.kind()
        else {
            panic!("expected socks proxy credentials on route");
        };
        assert_eq!(credentials.username(), "alice");
        assert_eq!(credentials.password(), "secret");
    }

    #[test]
    fn planner_alternates_families_for_dual_stack_routes() {
        let planner = RoutePlanner::default();
        let plan = planner.plan_direct(
            http_address(),
            [
                socket_v6(1),
                socket_v6(2),
                socket_v4(3),
                socket_v4(4),
                socket_v6(5),
            ],
        );

        let ordered = plan
            .iter()
            .map(|route| match route.kind() {
                RouteKind::Direct { target } => *target,
                _ => unreachable!("expected direct route"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            ordered,
            vec![
                socket_v6(1),
                socket_v4(3),
                socket_v6(2),
                socket_v4(4),
                socket_v6(5)
            ]
        );
        assert_eq!(plan.fast_fallback_stagger(), Duration::from_millis(250));
    }

    #[test]
    fn planner_preserves_resolver_order_for_single_family_results() {
        let planner = RoutePlanner::default();
        let plan = planner.plan_direct(
            http_address(),
            [socket_v4(11), socket_v4(12), socket_v4(13)],
        );

        let ordered = plan
            .iter()
            .map(|route| match route.kind() {
                RouteKind::Direct { target } => *target,
                _ => unreachable!("expected direct route"),
            })
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec![socket_v4(11), socket_v4(12), socket_v4(13)]);
    }

    #[test]
    fn planner_builds_proxy_route_variants_with_deferred_target_dns() {
        let planner = RoutePlanner::default();
        let forward =
            planner.plan_http_forward(https_proxy_address(ProxyMode::Forward), [socket_v4(21)]);
        let connect =
            planner.plan_connect_proxy(https_proxy_address(ProxyMode::Connect), [socket_v4(22)]);
        let socks =
            planner.plan_socks_proxy(https_proxy_address(ProxyMode::SocksTunnel), [socket_v4(23)]);

        for plan in [forward, connect, socks] {
            assert_eq!(plan.len(), 1);
            assert!(matches!(
                plan.route(0).expect("route").target_dns(),
                DnsResolution::Deferred { host, port } if host == "example.com" && *port == 443
            ));
        }
    }

    #[test]
    fn planner_selects_dns_target_from_address() {
        let planner = RoutePlanner::default();

        assert_eq!(planner.dns_target(&http_address()), ("example.com", 80));
        assert_eq!(
            planner.dns_target(&https_proxy_address(ProxyMode::Connect)),
            ("proxy.internal", 8080)
        );
    }

    #[test]
    fn planner_selects_route_kind_from_address_proxy_mode() {
        let planner = RoutePlanner::default();

        let direct = planner.plan(http_address(), [socket_v4(51)]);
        assert!(matches!(
            direct.route(0).expect("direct route").kind(),
            RouteKind::Direct { .. }
        ));

        let forward = planner.plan(https_proxy_address(ProxyMode::Forward), [socket_v4(52)]);
        assert!(matches!(
            forward.route(0).expect("forward route").kind(),
            RouteKind::HttpForwardProxy { .. }
        ));

        let connect = planner.plan(https_proxy_address(ProxyMode::Connect), [socket_v4(53)]);
        assert!(matches!(
            connect.route(0).expect("connect route").kind(),
            RouteKind::ConnectProxy { .. }
        ));
    }

    #[test]
    fn connect_plan_tracks_attempt_state_transitions() {
        let planner = RoutePlanner::default();
        let route_plan = planner.plan_direct(http_address(), [socket_v6(1), socket_v4(2)]);
        let mut connect_plan = ConnectPlan::from_route_plan(&route_plan);

        assert_eq!(connect_plan.len(), 2);
        assert_eq!(
            connect_plan.attempt(1).expect("attempt").scheduled_after(),
            Duration::from_millis(250)
        );

        assert!(connect_plan.mark_running(0));
        assert!(matches!(
            connect_plan.attempt(0).expect("attempt").state(),
            ConnectAttemptState::Running
        ));

        assert!(connect_plan.mark_failed(
            0,
            ConnectFailure::new(ConnectFailureStage::Tcp, "tcp failed"),
        ));
        assert!(matches!(
            connect_plan.attempt(0).expect("attempt").state(),
            ConnectAttemptState::Failed(failure)
                if failure.stage() == ConnectFailureStage::Tcp && failure.message() == "tcp failed"
        ));

        assert!(connect_plan.mark_running(1));
        assert!(connect_plan.promote_winner(1));
        assert!(matches!(
            connect_plan.attempt(1).expect("attempt").state(),
            ConnectAttemptState::Won
        ));
    }
}
