use std::sync::{Arc, RwLock};

use bytes::Bytes;
use cookie as cookie_crate;
use cookie_store::CookieStore;
use http::header::HeaderValue;

pub(crate) type SharedCookieJar = Arc<dyn CookieJar>;

/// Cookie loading and persistence for automatic request handling.
pub trait CookieJar: Send + Sync + 'static {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url);

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue>;
}

impl<T> CookieJar for Arc<T>
where
    T: CookieJar + ?Sized,
{
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        (**self).set_cookies(cookie_headers, url);
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        (**self).cookies(url)
    }
}

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
        self.0
            .write()
            .expect("cookie store lock")
            .store_response_cookies(cookies, url);
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
        self.0
            .write()
            .expect("cookie store lock")
            .store_response_cookies(cookies, url);
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        let store = self.0.read().expect("cookie store lock");
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
