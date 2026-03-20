use std::time::Duration;

use base64::Engine;
use http::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use http::{Method, Request, Response, Uri, Version};
use openwire_core::{RequestBody, ResponseBody, WireError};

use crate::client::{Client, RequestTimeout};

pub struct RequestBuilder {
    client: Client,
    state: Result<RequestBuilderParts, WireError>,
}

struct RequestBuilderParts {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    body: RequestBody,
    timeout: Option<Duration>,
}

impl RequestBuilder {
    pub(crate) fn new<U>(client: Client, method: Method, uri: U) -> Self
    where
        U: TryInto<Uri>,
    {
        let state = uri
            .try_into()
            .map_err(|_| WireError::invalid_request("invalid URI"))
            .map(|uri| RequestBuilderParts {
                method,
                uri,
                version: Version::HTTP_11,
                headers: HeaderMap::new(),
                body: RequestBody::empty(),
                timeout: None,
            });

        Self { client, state }
    }

    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        self.state = self.state.and_then(|mut request| {
            let name = name
                .try_into()
                .map_err(|_| WireError::invalid_request("invalid header name"))?;
            let value = value
                .try_into()
                .map_err(|_| WireError::invalid_request("invalid header value"))?;
            request.headers.append(name, value);
            Ok(request)
        });
        self
    }

    pub fn body<B>(mut self, body: B) -> Self
    where
        B: Into<RequestBody>,
    {
        if let Ok(request) = &mut self.state {
            request.body = body.into();
        }
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        if let Ok(request) = &mut self.state {
            request.timeout = Some(timeout);
        }
        self
    }

    pub fn basic_auth<U, P>(self, username: U, password: P) -> Self
    where
        U: AsRef<str>,
        P: AsRef<str>,
    {
        let credentials = format!("{}:{}", username.as_ref(), password.as_ref());
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        self.authorization_header(format!("Basic {encoded}"))
    }

    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: AsRef<str>,
    {
        self.authorization_header(format!("Bearer {}", token.as_ref()))
    }

    pub async fn send(self) -> Result<Response<ResponseBody>, WireError> {
        let RequestBuilder { client, state } = self;
        let request = Self::build_request(state)?;
        client.execute(request).await
    }

    fn authorization_header(self, value: String) -> Self {
        self.with_header_value(AUTHORIZATION, value)
    }

    fn with_header_value<V>(mut self, name: HeaderName, value: V) -> Self
    where
        V: TryInto<HeaderValue>,
    {
        self.state = self.state.and_then(|mut request| {
            let value = value
                .try_into()
                .map_err(|_| WireError::invalid_request("invalid header value"))?;
            request.headers.insert(name, value);
            Ok(request)
        });
        self
    }

    fn build_request(
        state: Result<RequestBuilderParts, WireError>,
    ) -> Result<Request<RequestBody>, WireError> {
        let RequestBuilderParts {
            method,
            uri,
            version,
            headers,
            body,
            timeout,
        } = state?;

        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .version(version)
            .body(body)?;
        *request.headers_mut() = headers;
        if let Some(timeout) = timeout {
            request.extensions_mut().insert(RequestTimeout(timeout));
        }
        Ok(request)
    }
}
