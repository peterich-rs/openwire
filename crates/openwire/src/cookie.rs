use std::sync::{Arc, RwLock};

use bytes::Bytes;
use cookie as cookie_crate;
use cookie_store::CookieStore;
use http::header::HeaderValue;
use openwire_core::CookieJar;

use crate::sync_util::{read_rwlock, write_rwlock};

pub(crate) type SharedCookieJar = Arc<dyn CookieJar>;

/// Default in-memory cookie jar backed by `cookie_store`.
#[derive(Default)]
pub struct Jar(RwLock<CookieStore>);

impl Jar {
    /// Creates an empty in-memory cookie jar.
    pub fn new() -> Self {
        Self::default()
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
}
