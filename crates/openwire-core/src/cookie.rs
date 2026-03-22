use std::sync::Arc;

use http::header::HeaderValue;

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
