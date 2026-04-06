use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use http::Uri;
use url::Url;

use crate::WireError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proxy {
    target: Url,
    intercept: ProxyIntercept,
    no_proxy: Option<NoProxy>,
    credentials: Option<ProxyCredentials>,
}

/// Resolves the proxy configuration to use for a request URI.
pub trait ProxySelector: Send + Sync + 'static {
    fn select(&self, uri: &Uri) -> Result<ProxySelection, WireError>;
}

impl<T> ProxySelector for Arc<T>
where
    T: ProxySelector + ?Sized,
{
    fn select(&self, uri: &Uri) -> Result<ProxySelection, WireError> {
        (**self).select(uri)
    }
}

pub(crate) type SharedProxySelector = Arc<dyn ProxySelector>;

/// Built-in proxy selector that applies explicit proxy rules first and can
/// optionally fall back to standard environment variables.
#[derive(Clone, Debug, Default)]
pub struct ProxyRules {
    proxies: Vec<Proxy>,
    use_system_proxy: bool,
}

/// An ordered set of proxy candidates for a request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxySelection {
    choices: Vec<ProxyChoice>,
}

/// One proxy candidate in a selection. `Direct` means connect without a proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyChoice {
    Direct,
    Proxy(Box<Proxy>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectedProxy {
    target: Url,
    credentials: Option<ProxyCredentials>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoProxy {
    matches_all: bool,
    exact_hosts: Vec<String>,
    domain_suffixes: Vec<String>,
    cidr_blocks: Vec<CidrBlock>,
    bypass_loopback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyIntercept {
    Http,
    Https,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CidrBlock {
    network: IpAddr,
    prefix: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProxyCredentials {
    username: String,
    password: String,
}

impl Proxy {
    fn from_target(mut target: Url, intercept: ProxyIntercept) -> Result<Self, WireError> {
        let credentials = ProxyCredentials::from_url(&target)?;
        if credentials.is_some() {
            let _ = target.set_username("");
            let _ = target.set_password(None);
        }
        Ok(Self {
            target,
            intercept,
            no_proxy: None,
            credentials,
        })
    }

    /// Routes HTTP requests through the given HTTP proxy endpoint.
    pub fn http(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Self::from_target(target, ProxyIntercept::Http)
    }

    /// Routes HTTPS requests through the given HTTP proxy endpoint.
    pub fn https(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Self::from_target(target, ProxyIntercept::Https)
    }

    /// Routes both HTTP and HTTPS requests through the given HTTP proxy endpoint.
    pub fn all(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Self::from_target(target, ProxyIntercept::All)
    }

    /// Routes both HTTP and HTTPS requests through the given SOCKS5 proxy endpoint.
    pub fn socks5(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_socks5_proxy_target(target.as_ref())?;
        Self::from_target(target, ProxyIntercept::All)
    }

    /// Excludes requests matched by `no_proxy` from using this proxy rule.
    pub fn no_proxy(mut self, no_proxy: NoProxy) -> Self {
        self.no_proxy = Some(no_proxy);
        self
    }

    pub(crate) fn matches(&self, uri: &http::Uri) -> bool {
        if let Some(no_proxy) = &self.no_proxy {
            let Some(host) = uri.host() else {
                return false;
            };
            if no_proxy.matches(host) {
                return false;
            }
        }

        matches!(
            (self.intercept, uri.scheme_str()),
            (ProxyIntercept::Http, Some("http"))
                | (ProxyIntercept::Https, Some("https"))
                | (ProxyIntercept::All, Some("http" | "https"))
        )
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> &Url {
        &self.target
    }

    #[cfg(test)]
    pub(crate) fn credentials(&self) -> Option<&ProxyCredentials> {
        self.credentials.as_ref()
    }

    pub(crate) fn selected_proxy(&self) -> SelectedProxy {
        SelectedProxy {
            target: self.target.clone(),
            credentials: self.credentials.clone(),
        }
    }
}

impl ProxyRules {
    /// Creates an empty selector that always connects directly unless proxy
    /// rules or system proxy lookup are added.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a proxy rule to this selector. The first matching rule wins.
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxies.push(proxy);
        self
    }

    /// Opts into reading proxy rules from standard environment variables when
    /// no explicit rule matches.
    pub fn use_system_proxy(mut self, enabled: bool) -> Self {
        self.use_system_proxy = enabled;
        self
    }
}

impl ProxySelector for ProxyRules {
    fn select(&self, uri: &Uri) -> Result<ProxySelection, WireError> {
        let explicit = matching_proxies(&self.proxies, uri);
        if !explicit.is_empty() {
            return Ok(ProxySelection::from_proxies(explicit));
        }

        if self.use_system_proxy {
            let system_proxies = system_proxies_from_env()?;
            let system_matches = matching_proxies(&system_proxies, uri);
            if !system_matches.is_empty() {
                return Ok(ProxySelection::from_proxies(system_matches));
            }
        }

        Ok(ProxySelection::direct())
    }
}

impl SelectedProxy {
    pub(crate) fn from_proxy(proxy: &Proxy) -> Self {
        proxy.selected_proxy()
    }

    pub(crate) fn target(&self) -> &Url {
        &self.target
    }

    pub(crate) fn credentials(&self) -> Option<&ProxyCredentials> {
        self.credentials.as_ref()
    }
}

impl ProxySelection {
    /// Creates an empty selection. Empty selections are treated as direct.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a selection that connects directly.
    pub fn direct() -> Self {
        Self::new().push_direct()
    }

    /// Appends a direct-connection candidate.
    pub fn push_direct(mut self) -> Self {
        self.choices.push(ProxyChoice::Direct);
        self
    }

    /// Appends a proxied candidate.
    pub fn push_proxy(mut self, proxy: Proxy) -> Self {
        self.choices.push(ProxyChoice::Proxy(Box::new(proxy)));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.choices.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ProxyChoice> {
        self.choices.iter()
    }

    fn from_proxies(proxies: Vec<Proxy>) -> Self {
        Self {
            choices: proxies
                .into_iter()
                .map(|proxy| ProxyChoice::Proxy(Box::new(proxy)))
                .collect(),
        }
    }
}

impl ProxyCredentials {
    fn from_url(target: &Url) -> Result<Option<Self>, WireError> {
        let username = target.username();
        let password = target.password();
        if username.is_empty() && password.is_none() {
            return Ok(None);
        }

        if username.is_empty() {
            return Err(WireError::invalid_request(
                "proxy URL credentials must include a username",
            ));
        }

        Ok(Some(Self {
            username: username.to_owned(),
            password: password.unwrap_or_default().to_owned(),
        }))
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    pub(crate) fn basic_auth_header_value(&self) -> String {
        use base64::Engine;

        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.username, self.password));
        format!("Basic {encoded}")
    }
}

impl NoProxy {
    /// Creates an empty no-proxy rule set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Excludes an exact host from this proxy rule.
    pub fn host(mut self, host: impl AsRef<str>) -> Self {
        self.exact_hosts.push(normalize_host(host.as_ref()));
        self
    }

    /// Excludes a domain suffix from this proxy rule.
    pub fn domain(mut self, suffix: impl AsRef<str>) -> Self {
        self.domain_suffixes
            .push(normalize_domain_suffix(suffix.as_ref()));
        self
    }

    /// Excludes `localhost` and loopback IP addresses.
    pub fn localhost(mut self) -> Self {
        self.bypass_loopback = true;
        self
    }

    pub(crate) fn all(mut self) -> Self {
        self.matches_all = true;
        self
    }

    fn matches(&self, host: &str) -> bool {
        if self.matches_all {
            return true;
        }

        let host = normalize_host(host);
        let ip = host.parse::<IpAddr>().ok();

        if self.exact_hosts.iter().any(|candidate| candidate == &host) {
            return true;
        }

        if self
            .domain_suffixes
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        {
            return true;
        }

        if self.bypass_loopback && (host == "localhost" || ip.is_some_and(|ip| ip.is_loopback())) {
            return true;
        }

        ip.is_some_and(|ip| self.cidr_blocks.iter().any(|cidr| cidr.matches(ip)))
    }
}

fn parse_http_proxy_target(target: &str) -> Result<Url, WireError> {
    parse_proxy_target(target, &["http"], "only http proxy endpoints are supported")
}

pub(crate) fn resolved_proxy_candidates(
    selection: ProxySelection,
    sticky: Option<SelectedProxy>,
) -> Vec<Option<SelectedProxy>> {
    let mut candidates = Vec::new();

    if let Some(sticky) = sticky {
        candidates.push(Some(sticky));
    }

    for choice in selection.iter() {
        let candidate = match choice {
            ProxyChoice::Direct => None,
            ProxyChoice::Proxy(proxy) => Some(SelectedProxy::from_proxy(proxy.as_ref())),
        };
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    if candidates.is_empty() {
        candidates.push(None);
    }

    candidates
}

fn matching_proxies(proxies: &[Proxy], uri: &Uri) -> Vec<Proxy> {
    proxies
        .iter()
        .filter(|proxy| proxy.matches(uri))
        .cloned()
        .collect()
}

fn parse_socks5_proxy_target(target: &str) -> Result<Url, WireError> {
    parse_proxy_target(
        target,
        &["socks5"],
        "only socks5 proxy endpoints are supported",
    )
}

fn parse_proxy_target(
    target: &str,
    allowed_schemes: &[&str],
    unsupported_message: &'static str,
) -> Result<Url, WireError> {
    let target = Url::parse(target)
        .map_err(|error| WireError::invalid_request(format!("invalid proxy URL: {error}")))?;

    if !allowed_schemes
        .iter()
        .any(|scheme| target.scheme().eq_ignore_ascii_case(scheme))
    {
        return Err(WireError::invalid_request(unsupported_message));
    }

    if target.host_str().is_none() {
        return Err(WireError::invalid_request("proxy URL is missing a host"));
    }

    Ok(target)
}

pub(crate) fn system_proxies_from_env() -> Result<Vec<Proxy>, WireError> {
    system_proxies_from_iter(std::env::vars())
}

pub(crate) fn system_proxies_from_iter<I, K, V>(vars: I) -> Result<Vec<Proxy>, WireError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let vars = vars
        .into_iter()
        .filter_map(|(key, value)| {
            let value = value.as_ref();
            if value.is_empty() {
                None
            } else {
                Some((key.as_ref().to_owned(), value.to_owned()))
            }
        })
        .collect::<HashMap<_, _>>();

    let http_proxy = lookup_proxy_var(&vars, &["http_proxy", "HTTP_PROXY"]);
    let https_proxy = lookup_proxy_var(&vars, &["https_proxy", "HTTPS_PROXY"]);
    let all_proxy = lookup_proxy_var(&vars, &["all_proxy", "ALL_PROXY"]);
    let no_proxy = lookup_proxy_var(&vars, &["NO_PROXY", "no_proxy"]);

    let no_proxy = no_proxy.as_deref().map(parse_no_proxy).transpose()?;
    let mut proxies = Vec::new();

    if let Some(proxy) = http_proxy {
        proxies.push(apply_no_proxy(
            proxy_from_env(&proxy, ProxyIntercept::Http)?,
            no_proxy.clone(),
        ));
    }

    if let Some(proxy) = https_proxy {
        proxies.push(apply_no_proxy(
            proxy_from_env(&proxy, ProxyIntercept::Https)?,
            no_proxy.clone(),
        ));
    }

    if let Some(proxy) = all_proxy {
        proxies.push(apply_no_proxy(
            proxy_from_env(&proxy, ProxyIntercept::All)?,
            no_proxy,
        ));
    }

    Ok(proxies)
}

fn apply_no_proxy(proxy: Proxy, no_proxy: Option<NoProxy>) -> Proxy {
    match no_proxy {
        Some(no_proxy) => proxy.no_proxy(no_proxy),
        None => proxy,
    }
}

fn proxy_from_env(target: &str, intercept: ProxyIntercept) -> Result<Proxy, WireError> {
    let parsed = Url::parse(target)
        .map_err(|error| WireError::invalid_request(format!("invalid proxy URL: {error}")))?;
    if parsed.host_str().is_none() {
        return Err(WireError::invalid_request("proxy URL is missing a host"));
    }
    match parsed.scheme() {
        "http" | "socks5" => Proxy::from_target(parsed, intercept),
        "https" => Err(WireError::invalid_request(
            "https proxy endpoints are not supported",
        )),
        _ => Err(WireError::invalid_request(
            "only http and socks5 proxy endpoints are supported",
        )),
    }
}

fn lookup_proxy_var(vars: &HashMap<String, String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| vars.get(*name).cloned())
}

fn parse_no_proxy(value: &str) -> Result<NoProxy, WireError> {
    let mut no_proxy = NoProxy::new();
    for token in value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        if token == "*" {
            no_proxy = no_proxy.all();
            continue;
        }

        if token.eq_ignore_ascii_case("localhost") {
            no_proxy = no_proxy.localhost().host("localhost");
            continue;
        }

        if token.contains('/') {
            no_proxy.cidr_blocks.push(parse_cidr_block(token)?);
            continue;
        }

        if token.parse::<IpAddr>().is_ok() {
            no_proxy = no_proxy.host(token);
            continue;
        }

        if token.starts_with('.') {
            no_proxy = no_proxy.domain(token);
            continue;
        }

        no_proxy = no_proxy.domain(token);
    }

    Ok(no_proxy)
}

fn parse_cidr_block(token: &str) -> Result<CidrBlock, WireError> {
    let (address, prefix) = token.rsplit_once('/').ok_or_else(|| {
        WireError::invalid_request(format!("invalid no_proxy CIDR entry: {token}"))
    })?;

    let address = address.parse::<IpAddr>().map_err(|error| {
        WireError::invalid_request(format!("invalid no_proxy CIDR address {token}: {error}"))
    })?;
    let prefix = prefix.parse::<u8>().map_err(|error| {
        WireError::invalid_request(format!("invalid no_proxy CIDR prefix {token}: {error}"))
    })?;

    match address {
        IpAddr::V4(address) if prefix <= 32 => Ok(CidrBlock {
            network: IpAddr::V4(canonicalize_ipv4_network(address, prefix)),
            prefix,
        }),
        IpAddr::V6(address) if prefix <= 128 => Ok(CidrBlock {
            network: IpAddr::V6(canonicalize_ipv6_network(address, prefix)),
            prefix,
        }),
        IpAddr::V4(_) => Err(WireError::invalid_request(format!(
            "invalid no_proxy CIDR prefix {token}: IPv4 prefixes must be <= 32"
        ))),
        IpAddr::V6(_) => Err(WireError::invalid_request(format!(
            "invalid no_proxy CIDR prefix {token}: IPv6 prefixes must be <= 128"
        ))),
    }
}

fn canonicalize_ipv4_network(address: std::net::Ipv4Addr, prefix: u8) -> std::net::Ipv4Addr {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    std::net::Ipv4Addr::from(u32::from(address) & mask)
}

fn canonicalize_ipv6_network(address: std::net::Ipv6Addr, prefix: u8) -> std::net::Ipv6Addr {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    std::net::Ipv6Addr::from(u128::from(address) & mask)
}

impl CidrBlock {
    fn matches(&self, candidate: IpAddr) -> bool {
        match (self.network, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                canonicalize_ipv4_network(candidate, self.prefix) == network
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                canonicalize_ipv6_network(candidate, self.prefix) == network
            }
            _ => false,
        }
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn normalize_domain_suffix(suffix: &str) -> String {
    normalize_host(suffix.trim_start_matches('.'))
}

#[cfg(test)]
mod tests {
    use http::Request;

    use super::{
        parse_no_proxy, system_proxies_from_iter, Proxy, ProxyChoice, ProxyIntercept, ProxyRules,
        ProxySelection,
    };
    use crate::proxy::ProxySelector;

    #[test]
    fn system_proxy_parser_uses_stable_variable_precedence() {
        let proxies = system_proxies_from_iter([
            ("HTTP_PROXY", "http://uppercase-http.test:8080"),
            ("http_proxy", "http://lowercase-http.test:8080"),
            ("HTTPS_PROXY", "http://uppercase-https.test:8080"),
            ("https_proxy", "http://lowercase-https.test:8080"),
            ("ALL_PROXY", "http://uppercase-all.test:8080"),
            ("all_proxy", "http://lowercase-all.test:8080"),
            ("no_proxy", "ignored.example"),
            ("NO_PROXY", "preferred.example"),
        ])
        .expect("proxy config");

        assert_eq!(proxies.len(), 3);
        assert_eq!(proxies[0].target.host_str(), Some("lowercase-http.test"));
        assert_eq!(proxies[1].target.host_str(), Some("lowercase-https.test"));
        assert_eq!(proxies[2].target.host_str(), Some("lowercase-all.test"));
        assert_eq!(proxies[0].intercept, ProxyIntercept::Http);
        assert_eq!(proxies[1].intercept, ProxyIntercept::Https);
        assert_eq!(proxies[2].intercept, ProxyIntercept::All);
        assert!(proxies[0]
            .no_proxy
            .as_ref()
            .expect("no_proxy")
            .matches("api.preferred.example"));
        assert!(!proxies[0]
            .no_proxy
            .as_ref()
            .expect("no_proxy")
            .matches("api.ignored.example"));
    }

    #[test]
    fn no_proxy_parser_supports_common_cidr_and_wildcard_entries() {
        let wildcard = parse_no_proxy("*").expect("wildcard");
        assert!(wildcard.matches("example.com"));

        let ipv4 = parse_no_proxy("10.0.0.0/8").expect("ipv4 cidr");
        assert!(ipv4.matches("10.42.0.7"));
        assert!(!ipv4.matches("11.42.0.7"));

        let ipv6 = parse_no_proxy("fd00::/8").expect("ipv6 cidr");
        assert!(ipv6.matches("fd12::1"));
        assert!(!ipv6.matches("fe80::1"));
    }

    #[test]
    fn socks5_proxy_constructor_requires_socks_scheme() {
        let proxy = Proxy::socks5("socks5://proxy.test:1080").expect("socks proxy");
        assert_eq!(proxy.target.scheme(), "socks5");

        let error = Proxy::socks5("http://proxy.test:1080").expect_err("invalid socks proxy");
        assert!(error
            .to_string()
            .contains("only socks5 proxy endpoints are supported"));
    }

    #[test]
    fn proxy_constructor_extracts_credentials_from_userinfo() {
        let proxy = Proxy::socks5("socks5://alice:secret@proxy.test:1080").expect("socks proxy");

        let credentials = proxy.credentials().expect("proxy credentials");
        assert_eq!(credentials.username(), "alice");
        assert_eq!(credentials.password(), "secret");
        assert_eq!(
            credentials.basic_auth_header_value(),
            "Basic YWxpY2U6c2VjcmV0"
        );
        assert_eq!(proxy.target.host_str(), Some("proxy.test"));
        assert_eq!(proxy.target.username(), "");
        assert!(proxy.target.password().is_none());
    }

    #[test]
    fn system_proxy_parser_accepts_socks5_targets() {
        let proxies = system_proxies_from_iter([("all_proxy", "socks5://socks.test:1080")])
            .expect("proxy config");
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].target.scheme(), "socks5");
        assert_eq!(proxies[0].intercept, ProxyIntercept::All);
    }

    #[test]
    fn proxy_rules_prefer_first_matching_rule_and_honor_no_proxy() {
        let primary = Proxy::all("http://first.test:8080")
            .expect("first proxy")
            .no_proxy(parse_no_proxy("api.example.com").expect("no_proxy"));
        let fallback = Proxy::all("http://second.test:8080").expect("fallback proxy");
        let selector = ProxyRules::new().proxy(primary).proxy(fallback);

        let direct_uri = Request::builder()
            .uri("http://service.example.com/resource")
            .body(())
            .expect("request")
            .uri()
            .clone();
        assert_eq!(
            selector
                .select(&direct_uri)
                .expect("proxy selection")
                .iter()
                .map(|choice| match choice {
                    ProxyChoice::Direct => "direct".to_owned(),
                    ProxyChoice::Proxy(proxy) =>
                        proxy.target().host_str().expect("proxy host").to_owned(),
                })
                .collect::<Vec<_>>(),
            vec!["first.test".to_owned(), "second.test".to_owned()]
        );

        let bypassed_uri = Request::builder()
            .uri("http://api.example.com/resource")
            .body(())
            .expect("request")
            .uri()
            .clone();
        assert_eq!(
            selector
                .select(&bypassed_uri)
                .expect("proxy selection")
                .iter()
                .map(|choice| match choice {
                    ProxyChoice::Direct => "direct".to_owned(),
                    ProxyChoice::Proxy(proxy) =>
                        proxy.target().host_str().expect("proxy host").to_owned(),
                })
                .collect::<Vec<_>>(),
            vec!["second.test".to_owned()]
        );
    }

    #[test]
    fn empty_proxy_selection_defaults_to_direct_candidate() {
        assert!(ProxySelection::new().is_empty());
        assert_eq!(
            super::resolved_proxy_candidates(ProxySelection::new(), None),
            vec![None]
        );
    }
}
