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
        let cookies = cookie_crate::Cookie::parse(cookie)
            .ok()
            .map(cookie_crate::Cookie::into_owned)
            .into_iter();
        write_rwlock(&self.0).store_response_cookies(cookies, url);
    }
}

impl CookieJar for Jar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        let cookies = cookie_headers.filter_map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| cookie_crate::Cookie::parse(value).ok())
                .map(cookie_crate::Cookie::into_owned)
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

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};

    use super::{CookieJar, Jar};

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
}
