use std::sync::Arc;

use http::{Method, StatusCode, Uri};

use crate::WireError;

pub trait RetryPolicy: Send + Sync + 'static {
    fn should_retry(&self, ctx: &RetryContext<'_>) -> Option<&'static str>;

    fn should_retry_response(&self, _ctx: &ResponseRetryContext<'_>) -> Option<&'static str> {
        None
    }
}

impl<T> RetryPolicy for Arc<T>
where
    T: RetryPolicy + ?Sized,
{
    fn should_retry(&self, ctx: &RetryContext<'_>) -> Option<&'static str> {
        (**self).should_retry(ctx)
    }

    fn should_retry_response(&self, ctx: &ResponseRetryContext<'_>) -> Option<&'static str> {
        (**self).should_retry_response(ctx)
    }
}

pub struct RetryContext<'a> {
    error: &'a WireError,
    attempt: u32,
    is_body_replayable: bool,
    request_method: &'a Method,
}

impl<'a> RetryContext<'a> {
    pub fn new(
        error: &'a WireError,
        attempt: u32,
        is_body_replayable: bool,
        request_method: &'a Method,
    ) -> Self {
        Self {
            error,
            attempt,
            is_body_replayable,
            request_method,
        }
    }

    pub fn error(&self) -> &'a WireError {
        self.error
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn is_body_replayable(&self) -> bool {
        self.is_body_replayable
    }

    pub fn request_method(&self) -> &'a Method {
        self.request_method
    }
}

pub struct ResponseRetryContext<'a> {
    response_status: StatusCode,
    retry_count: u32,
    is_body_replayable: bool,
    request_method: &'a Method,
    retry_after: Option<RetryAfter>,
}

impl<'a> ResponseRetryContext<'a> {
    pub fn new(
        response_status: StatusCode,
        retry_count: u32,
        is_body_replayable: bool,
        request_method: &'a Method,
        retry_after: Option<RetryAfter>,
    ) -> Self {
        Self {
            response_status,
            retry_count,
            is_body_replayable,
            request_method,
            retry_after,
        }
    }

    pub fn response_status(&self) -> StatusCode {
        self.response_status
    }

    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    pub fn is_body_replayable(&self) -> bool {
        self.is_body_replayable
    }

    pub fn request_method(&self) -> &'a Method {
        self.request_method
    }

    pub fn retry_after(&self) -> Option<RetryAfter> {
        self.retry_after
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryAfter {
    Immediate,
    Delayed,
    Invalid,
}

pub trait RedirectPolicy: Send + Sync + 'static {
    fn should_redirect(&self, ctx: &RedirectContext<'_>) -> RedirectDecision;
}

impl<T> RedirectPolicy for Arc<T>
where
    T: RedirectPolicy + ?Sized,
{
    fn should_redirect(&self, ctx: &RedirectContext<'_>) -> RedirectDecision {
        (**self).should_redirect(ctx)
    }
}

pub struct RedirectContext<'a> {
    request_method: &'a Method,
    request_uri: &'a Uri,
    response_status: StatusCode,
    location: &'a Uri,
    redirect_count: u32,
    is_body_replayable: bool,
}

impl<'a> RedirectContext<'a> {
    pub fn new(
        request_method: &'a Method,
        request_uri: &'a Uri,
        response_status: StatusCode,
        location: &'a Uri,
        redirect_count: u32,
        is_body_replayable: bool,
    ) -> Self {
        Self {
            request_method,
            request_uri,
            response_status,
            location,
            redirect_count,
            is_body_replayable,
        }
    }

    pub fn request_method(&self) -> &'a Method {
        self.request_method
    }

    pub fn request_uri(&self) -> &'a Uri {
        self.request_uri
    }

    pub fn response_status(&self) -> StatusCode {
        self.response_status
    }

    pub fn location(&self) -> &'a Uri {
        self.location
    }

    pub fn redirect_count(&self) -> u32 {
        self.redirect_count
    }

    pub fn is_body_replayable(&self) -> bool {
        self.is_body_replayable
    }
}

pub enum RedirectDecision {
    Follow,
    Stop,
    Error(WireError),
}
