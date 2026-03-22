use std::sync::Arc;

use http::{Extensions, HeaderMap, Method, Request, StatusCode, Uri, Version};

use crate::{BoxFuture, RequestBody, WireError};

/// Produces authenticated follow-up requests for authentication challenges.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>>;
}

impl<T> Authenticator for Arc<T>
where
    T: Authenticator + ?Sized,
{
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        (**self).authenticate(ctx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Origin,
    Proxy,
}

/// Owned authentication challenge context passed to an [`Authenticator`].
pub struct AuthContext {
    kind: AuthKind,
    request_method: Method,
    request_uri: Uri,
    request_version: Version,
    request_headers: HeaderMap,
    request_extensions: Extensions,
    request_body: Option<RequestBody>,
    response_status: StatusCode,
    response_headers: HeaderMap,
    total_attempt: u32,
    retry_count: u32,
    redirect_count: u32,
    auth_count: u32,
}

impl AuthContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: AuthKind,
        request_method: Method,
        request_uri: Uri,
        request_version: Version,
        request_headers: HeaderMap,
        request_extensions: Extensions,
        request_body: Option<RequestBody>,
        response_status: StatusCode,
        response_headers: HeaderMap,
        total_attempt: u32,
        retry_count: u32,
        redirect_count: u32,
        auth_count: u32,
    ) -> Self {
        Self {
            kind,
            request_method,
            request_uri,
            request_version,
            request_headers,
            request_extensions,
            request_body,
            response_status,
            response_headers,
            total_attempt,
            retry_count,
            redirect_count,
            auth_count,
        }
    }

    /// Returns whether this is an origin or proxy authentication challenge.
    pub fn kind(&self) -> AuthKind {
        self.kind
    }

    /// Returns the method of the challenged request.
    pub fn request_method(&self) -> &Method {
        &self.request_method
    }

    /// Returns the URI of the challenged request.
    pub fn request_uri(&self) -> &Uri {
        &self.request_uri
    }

    /// Returns the request headers of the challenged request.
    pub fn request_headers(&self) -> &HeaderMap {
        &self.request_headers
    }

    /// Returns the response status that triggered authentication.
    pub fn response_status(&self) -> StatusCode {
        self.response_status
    }

    /// Returns the response headers that triggered authentication.
    pub fn response_headers(&self) -> &HeaderMap {
        &self.response_headers
    }

    /// Returns the current total attempt number for the logical call.
    pub fn total_attempt(&self) -> u32 {
        self.total_attempt
    }

    /// Returns the retry count accumulated before this auth decision.
    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    /// Returns the redirect count accumulated before this auth decision.
    pub fn redirect_count(&self) -> u32 {
        self.redirect_count
    }

    /// Returns the completed auth follow-up count before this auth decision.
    pub fn auth_count(&self) -> u32 {
        self.auth_count
    }

    /// Returns whether the challenged request body can be replayed.
    pub fn is_replayable(&self) -> bool {
        self.request_body.is_some()
    }

    /// Clones the challenged request when its body is replayable.
    pub fn try_clone_request(&self) -> Option<Request<RequestBody>> {
        let body = self
            .request_body
            .as_ref()
            .and_then(RequestBody::try_clone)?;
        let mut request = Request::builder()
            .method(self.request_method.clone())
            .uri(self.request_uri.clone())
            .version(self.request_version)
            .body(body)
            .ok()?;
        *request.headers_mut() = self.request_headers.clone();
        *request.extensions_mut() = self.request_extensions.clone();
        Some(request)
    }
}
