use std::sync::Arc;

use http::{Extensions, HeaderMap, Method, StatusCode, Uri, Version};
use openwire_core::{AuthContext, AuthKind, Authenticator, RequestBody};

pub(crate) type SharedAuthenticator = Arc<dyn Authenticator>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthAttemptState {
    pub(crate) total_attempt: u32,
    pub(crate) retry_count: u32,
    pub(crate) redirect_count: u32,
    pub(crate) auth_count: u32,
}

pub(crate) struct AuthRequestState {
    pub(crate) method: Method,
    pub(crate) uri: Uri,
    pub(crate) version: Version,
    pub(crate) headers: HeaderMap,
    pub(crate) extensions: Extensions,
    pub(crate) body: Option<RequestBody>,
}

impl AuthRequestState {
    pub(crate) fn new(
        method: Method,
        uri: Uri,
        version: Version,
        headers: HeaderMap,
        extensions: Extensions,
        body: Option<RequestBody>,
    ) -> Self {
        Self {
            method,
            uri,
            version,
            headers,
            extensions,
            body,
        }
    }
}

pub(crate) struct AuthResponseState {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
}

impl AuthResponseState {
    pub(crate) fn new(status: StatusCode, headers: HeaderMap) -> Self {
        Self { status, headers }
    }
}

pub(crate) fn build_auth_context(
    kind: AuthKind,
    request: AuthRequestState,
    response: AuthResponseState,
    attempts: AuthAttemptState,
) -> AuthContext {
    AuthContext::new(
        kind,
        request.method,
        request.uri,
        request.version,
        request.headers,
        request.extensions,
        request.body,
        response.status,
        response.headers,
        attempts.total_attempt,
        attempts.retry_count,
        attempts.redirect_count,
        attempts.auth_count,
    )
}
