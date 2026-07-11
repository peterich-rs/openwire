use std::sync::{Arc, LazyLock, RwLock};

use bytes::Bytes;
use cookie as cookie_crate;
use cookie_store::CookieStore;
use http::header::HeaderValue;
use openwire_core::CookieJar;
use publicsuffix::List;

use crate::sync_util::{read_rwlock, write_rwlock};

pub(crate) type SharedCookieJar = Arc<dyn CookieJar>;

/// Embedded Mozilla Public Suffix List used by the default jar.
///
/// Loaded once so `Domain=.com` style cookies are rejected per RFC 6265 §5.3.
static PUBLIC_SUFFIX_LIST: LazyLock<List> = LazyLock::new(|| {
    List::from_bytes(include_bytes!("../data/public_suffix_list.dat"))
        .expect("embedded public suffix list must parse")
});

/// Default in-memory cookie jar backed by `cookie_store` with public-suffix
/// rejection enabled.
pub struct Jar(RwLock<CookieStore>);

impl Default for Jar {
    fn default() -> Self {
        Self::new()
    }
}

impl Jar {
    /// Creates an empty in-memory cookie jar with the embedded public suffix list.
    pub fn new() -> Self {
        Self(RwLock::new(CookieStore::new(Some(
            PUBLIC_SUFFIX_LIST.clone(),
        ))))
    }

    /// Creates a jar with an explicit public suffix list.
    ///
    /// Pass `None` only for tests that intentionally disable public-suffix
    /// rejection.
    pub fn with_public_suffix_list(list: Option<List>) -> Self {
        Self(RwLock::new(CookieStore::new(list)))
    }

    /// Adds a single cookie string for the given URL.
    pub fn add_cookie_str(&self, cookie: &str, url: &url::Url) {
        let cookies = match cookie_crate::Cookie::parse(cookie) {
            Ok(cookie) => Some(cookie.into_owned()).into_iter(),
            Err(error) => {
                tracing::debug!(
                    %error,
                    cookie_name = cookie_name_hint(cookie).unwrap_or("<unknown>"),
                    cookie_len = cookie.len(),
                    "dropping invalid cookie string"
                );
                None.into_iter()
            }
        };
        write_rwlock(&self.0).store_response_cookies(cookies, url);
    }
}

impl CookieJar for Jar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        let cookies = cookie_headers.filter_map(|value| {
            let value = match value.to_str() {
                Ok(value) => value,
                Err(error) => {
                    tracing::debug!(%error, "dropping non-UTF8 Set-Cookie header");
                    return None;
                }
            };
            match cookie_crate::Cookie::parse(value) {
                Ok(cookie) => Some(cookie.into_owned()),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        cookie_name = cookie_name_hint(value).unwrap_or("<unknown>"),
                        header_len = value.len(),
                        "dropping invalid Set-Cookie header"
                    );
                    None
                }
            }
        });
        write_rwlock(&self.0).store_response_cookies(cookies, url);
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        let store = read_rwlock(&self.0);
        let mut values = store.get_request_values(url).peekable();
        values.peek()?;

        let mut cookies = String::new();
        let mut first = true;
        for (name, value) in values {
            if !first {
                cookies.push_str("; ");
            }
            first = false;
            cookies.push_str(name);
            cookies.push('=');
            cookies.push_str(value);
        }

        HeaderValue::from_maybe_shared(Bytes::from(cookies)).ok()
    }
}

fn cookie_name_hint(value: &str) -> Option<&str> {
    let name = value.split_once('=')?.0.trim();
    if name.is_empty() || !is_safe_cookie_name(name) {
        return None;
    }
    Some(name)
}

fn is_safe_cookie_name(name: &str) -> bool {
    name.bytes().all(is_cookie_name_byte)
}

fn is_cookie_name_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'^'
            | b'_'
            | b'`'
            | b'a'..=b'z'
            | b'|'
            | b'~'
    )
}

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};

    use super::{cookie_name_hint, CookieJar, Jar};

    #[test]
    fn jar_recovers_after_rwlock_poisoning() {
        let jar = Jar::new();
        let url = url::Url::parse("https://example.com/").expect("url");

        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = jar.0.write().expect("poison cookie store lock for test");
            panic!("poison cookie store");
        }));

        jar.add_cookie_str("session=abc; Path=/", &url);
        let cookies = jar.cookies(&url).expect("cookies");
        assert_eq!(cookies.to_str().ok(), Some("session=abc"));
    }

    #[test]
    fn cookie_name_hint_extracts_safe_cookie_names() {
        assert_eq!(cookie_name_hint("session=abc; Path=/"), Some("session"));
        assert_eq!(cookie_name_hint("theme=light"), Some("theme"));
        assert_eq!(cookie_name_hint("Path=/"), Some("Path"));
    }

    #[test]
    fn cookie_name_hint_rejects_missing_or_unsafe_names() {
        assert_eq!(cookie_name_hint(""), None);
        assert_eq!(cookie_name_hint("session id=abc"), None);
        assert_eq!(cookie_name_hint(" =abc"), None);
    }

    #[test]
    fn default_jar_rejects_public_suffix_domain_cookies() {
        let jar = Jar::new();
        let url = url::Url::parse("https://evil.example.com/").expect("url");
        jar.add_cookie_str("session=bad; Domain=com; Path=/", &url);
        jar.add_cookie_str("ok=1; Domain=example.com; Path=/", &url);

        let other = url::Url::parse("https://victim.com/").expect("url");
        assert!(
            jar.cookies(&other).is_none(),
            "public-suffix Domain=com cookie must not leak across hosts"
        );

        let same_registrable = url::Url::parse("https://other.example.com/").expect("url");
        let cookies = jar
            .cookies(&same_registrable)
            .expect("same eTLD+1 should receive Domain=example.com cookie");
        assert_eq!(cookies.to_str().ok(), Some("ok=1"));
    }

    #[test]
    fn default_jar_honors_secure_attribute() {
        let jar = Jar::new();
        let https = url::Url::parse("https://example.com/").expect("url");
        jar.add_cookie_str("session=secret; Secure; Path=/", &https);

        let http = url::Url::parse("http://example.com/").expect("url");
        assert!(jar.cookies(&http).is_none());
        assert_eq!(
            jar.cookies(&https)
                .and_then(|value| value.to_str().ok().map(str::to_owned)),
            Some("session=secret".to_owned())
        );
    }
}
