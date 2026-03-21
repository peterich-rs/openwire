use std::collections::HashMap;
use std::net::IpAddr;

use url::Url;

use crate::WireError;

#[derive(Clone, Debug)]
pub struct Proxy {
    target: Url,
    intercept: ProxyIntercept,
    no_proxy: Option<NoProxy>,
}

#[derive(Clone, Debug, Default)]
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

impl Proxy {
    fn from_target(target: Url, intercept: ProxyIntercept) -> Self {
        Self {
            target,
            intercept,
            no_proxy: None,
        }
    }

    /// Routes HTTP requests through the given HTTP proxy endpoint.
    pub fn http(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Ok(Self::from_target(target, ProxyIntercept::Http))
    }

    /// Routes HTTPS requests through the given HTTP proxy endpoint.
    pub fn https(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Ok(Self::from_target(target, ProxyIntercept::Https))
    }

    /// Routes both HTTP and HTTPS requests through the given HTTP proxy endpoint.
    pub fn all(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_http_proxy_target(target.as_ref())?;
        Ok(Self::from_target(target, ProxyIntercept::All))
    }

    /// Routes both HTTP and HTTPS requests through the given SOCKS5 proxy endpoint.
    pub fn socks5(target: impl AsRef<str>) -> Result<Self, WireError> {
        let target = parse_socks5_proxy_target(target.as_ref())?;
        Ok(Self::from_target(target, ProxyIntercept::All))
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

    pub(crate) fn intercepts_http(&self) -> bool {
        matches!(self.intercept, ProxyIntercept::Http | ProxyIntercept::All)
    }

    pub(crate) fn intercepts_https(&self) -> bool {
        matches!(self.intercept, ProxyIntercept::Https | ProxyIntercept::All)
    }

    pub(crate) fn target(&self) -> &Url {
        &self.target
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
        "http" | "socks5" => Ok(Proxy::from_target(parsed, intercept)),
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
    use super::{parse_no_proxy, system_proxies_from_iter, Proxy, ProxyIntercept};

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
    fn system_proxy_parser_accepts_socks5_targets() {
        let proxies = system_proxies_from_iter([("all_proxy", "socks5://socks.test:1080")])
            .expect("proxy config");
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].target.scheme(), "socks5");
        assert_eq!(proxies[0].intercept, ProxyIntercept::All);
    }
}
