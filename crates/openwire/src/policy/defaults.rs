use std::sync::Arc;

use http::StatusCode;
use openwire_core::{
    EstablishmentStage, RedirectContext, RedirectDecision, RedirectPolicy, RetryContext,
    RetryPolicy, WireError, WireErrorKind,
};

#[derive(Clone, Default)]
pub(crate) struct RetryPolicyConfig {
    default: DefaultRetryPolicy,
    custom: Option<Arc<dyn RetryPolicy>>,
}

impl RetryPolicyConfig {
    pub(crate) fn policy(&self) -> &dyn RetryPolicy {
        self.custom
            .as_deref()
            .map(|policy| policy as &dyn RetryPolicy)
            .unwrap_or(&self.default)
    }

    pub(crate) fn default_mut(&mut self) -> &mut DefaultRetryPolicy {
        self.custom = None;
        &mut self.default
    }

    pub(crate) fn set_custom<P>(&mut self, policy: P)
    where
        P: RetryPolicy,
    {
        self.custom = Some(Arc::new(policy));
    }
}

#[derive(Clone, Debug)]
pub struct DefaultRetryPolicy {
    retry_on_connection_failure: bool,
    max_retries: usize,
    retry_canceled_requests: bool,
}

impl DefaultRetryPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retry_on_connection_failure(&self) -> bool {
        self.retry_on_connection_failure
    }

    pub fn set_retry_on_connection_failure(&mut self, enabled: bool) {
        self.retry_on_connection_failure = enabled;
    }

    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub fn set_max_retries(&mut self, max_retries: usize) {
        self.max_retries = max_retries;
    }

    pub fn retry_canceled_requests(&self) -> bool {
        self.retry_canceled_requests
    }

    pub fn set_retry_canceled_requests(&mut self, enabled: bool) {
        self.retry_canceled_requests = enabled;
    }
}

impl Default for DefaultRetryPolicy {
    fn default() -> Self {
        Self {
            retry_on_connection_failure: true,
            max_retries: 1,
            retry_canceled_requests: false,
        }
    }
}

impl RetryPolicy for DefaultRetryPolicy {
    fn should_retry(&self, ctx: &RetryContext<'_>) -> Option<&'static str> {
        if !ctx.is_body_replayable() || ctx.attempt() as usize >= self.max_retries {
            return None;
        }

        let error = ctx.error();
        match error.establishment_stage() {
            Some(EstablishmentStage::Dns) if error.is_retryable_establishment() => {
                return self.retry_on_connection_failure.then_some("dns");
            }
            Some(EstablishmentStage::Tcp) if error.is_connect_timeout() => {
                return self
                    .retry_on_connection_failure
                    .then_some("connect_timeout");
            }
            Some(EstablishmentStage::Tcp | EstablishmentStage::ProtocolBinding)
                if error.is_retryable_establishment() =>
            {
                return self.retry_on_connection_failure.then_some("connect");
            }
            Some(EstablishmentStage::Tls) if error.is_retryable_establishment() => {
                return self.retry_on_connection_failure.then_some("tls");
            }
            Some(EstablishmentStage::RouteExhausted | EstablishmentStage::ProxyTunnel)
                if error.is_retryable_establishment() =>
            {
                return self.retry_on_connection_failure.then_some("connect");
            }
            Some(_) => return None,
            None => {}
        }

        match error.kind() {
            WireErrorKind::Canceled if self.retry_canceled_requests => Some("canceled"),
            WireErrorKind::Dns if self.retry_on_connection_failure => Some("dns"),
            WireErrorKind::Connect
                if self.retry_on_connection_failure && !error.is_non_retryable_connect() =>
            {
                Some("connect")
            }
            WireErrorKind::Tls if self.retry_on_connection_failure => Some("tls"),
            WireErrorKind::Timeout
                if self.retry_on_connection_failure && error.is_connect_timeout() =>
            {
                Some("connect_timeout")
            }
            _ => None,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct RedirectPolicyConfig {
    default: DefaultRedirectPolicy,
    custom: Option<Arc<dyn RedirectPolicy>>,
}

impl RedirectPolicyConfig {
    pub(crate) fn policy(&self) -> &dyn RedirectPolicy {
        self.custom
            .as_deref()
            .map(|policy| policy as &dyn RedirectPolicy)
            .unwrap_or(&self.default)
    }

    pub(crate) fn default_policy(&self) -> Option<&DefaultRedirectPolicy> {
        self.custom.is_none().then_some(&self.default)
    }

    pub(crate) fn default_mut(&mut self) -> &mut DefaultRedirectPolicy {
        self.custom = None;
        &mut self.default
    }

    pub(crate) fn set_custom<P>(&mut self, policy: P)
    where
        P: RedirectPolicy,
    {
        self.custom = Some(Arc::new(policy));
    }
}

#[derive(Clone, Debug)]
pub struct DefaultRedirectPolicy {
    follow_redirects: bool,
    max_redirects: usize,
    allow_insecure_redirects: bool,
}

impl DefaultRedirectPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn follow_redirects(&self) -> bool {
        self.follow_redirects
    }

    pub fn set_follow_redirects(&mut self, enabled: bool) {
        self.follow_redirects = enabled;
    }

    pub fn max_redirects(&self) -> usize {
        self.max_redirects
    }

    pub fn set_max_redirects(&mut self, max_redirects: usize) {
        self.max_redirects = max_redirects;
    }

    pub fn allow_insecure_redirects(&self) -> bool {
        self.allow_insecure_redirects
    }

    pub fn set_allow_insecure_redirects(&mut self, enabled: bool) {
        self.allow_insecure_redirects = enabled;
    }
}

impl Default for DefaultRedirectPolicy {
    fn default() -> Self {
        Self {
            follow_redirects: true,
            max_redirects: 10,
            allow_insecure_redirects: false,
        }
    }
}

impl RedirectPolicy for DefaultRedirectPolicy {
    fn should_redirect(&self, ctx: &RedirectContext<'_>) -> RedirectDecision {
        if !self.follow_redirects || !is_redirect_status(ctx.response_status()) {
            return RedirectDecision::Stop;
        }

        if ctx.redirect_count() as usize >= self.max_redirects {
            return RedirectDecision::Error(WireError::redirect(format!(
                "too many redirects (max {})",
                self.max_redirects
            )));
        }

        if !self.allow_insecure_redirects
            && ctx.request_uri().scheme_str() == Some("https")
            && ctx.location().scheme_str() == Some("http")
        {
            return RedirectDecision::Error(WireError::redirect(format!(
                "refusing insecure redirect from {} to {}",
                ctx.request_uri(),
                ctx.location()
            )));
        }

        RedirectDecision::Follow
    }
}

fn is_redirect_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(test)]
mod tests {
    use http::Method;

    use super::{DefaultRedirectPolicy, DefaultRetryPolicy};
    use openwire_core::{
        RedirectContext, RedirectDecision, RedirectPolicy, RetryContext, RetryPolicy, WireError,
        WireErrorKind,
    };

    #[test]
    fn default_retry_policy_respects_max_retries_and_cancel_toggle() {
        let mut policy = DefaultRetryPolicy::default();
        policy.set_retry_canceled_requests(true);

        let canceled = WireError::new(WireErrorKind::Canceled, "canceled");
        let first = RetryContext::new(&canceled, 0, true);
        let second = RetryContext::new(&canceled, 1, true);

        assert_eq!(policy.should_retry(&first), Some("canceled"));
        assert_eq!(policy.should_retry(&second), None);
    }

    #[test]
    fn default_redirect_policy_rejects_https_to_http_downgrade() {
        let policy = DefaultRedirectPolicy::default();
        let method = Method::GET;
        let current = "https://secure.test/start".parse().expect("current uri");
        let location = "http://secure.test/next".parse().expect("location");
        let ctx = RedirectContext::new(
            &method,
            &current,
            http::StatusCode::FOUND,
            &location,
            0,
            true,
        );

        match policy.should_redirect(&ctx) {
            RedirectDecision::Error(error) => {
                assert_eq!(error.kind(), WireErrorKind::Redirect);
                assert!(error.to_string().contains(
                    "refusing insecure redirect from https://secure.test/start to http://secure.test/next"
                ));
            }
            other => panic!(
                "expected redirect error, got {:?}",
                describe_decision(other)
            ),
        }
    }

    fn describe_decision(decision: RedirectDecision) -> &'static str {
        match decision {
            RedirectDecision::Follow => "follow",
            RedirectDecision::Stop => "stop",
            RedirectDecision::Error(_) => "error",
        }
    }
}
