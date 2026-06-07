use std::sync::Arc;

use http::header::{PROXY_AUTHENTICATE, WWW_AUTHENTICATE};
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

/// A parsed RFC 7235 authentication challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthChallenge {
    scheme: String,
    token68: Option<String>,
    parameters: Vec<AuthChallengeParam>,
}

impl AuthChallenge {
    /// Returns the case-preserving authentication scheme token.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Returns the token68 payload for schemes that use token68 syntax.
    pub fn token68(&self) -> Option<&str> {
        self.token68.as_deref()
    }

    /// Returns parsed auth parameters in header order.
    pub fn parameters(&self) -> &[AuthChallengeParam] {
        &self.parameters
    }

    /// Returns the first parameter value matching `name`, ignoring ASCII case.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|parameter| parameter.name.eq_ignore_ascii_case(name))
            .map(|parameter| parameter.value.as_str())
    }

    /// Returns the challenge realm parameter when present.
    pub fn realm(&self) -> Option<&str> {
        self.parameter("realm")
    }
}

/// A parsed auth-param from an RFC 7235 challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthChallengeParam {
    name: String,
    value: String,
}

impl AuthChallengeParam {
    /// Returns the case-preserving parameter name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the unquoted parameter value.
    pub fn value(&self) -> &str {
        &self.value
    }
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

    /// Parses the applicable authentication challenges from the response.
    ///
    /// Origin contexts read `WWW-Authenticate`; proxy contexts read
    /// `Proxy-Authenticate`. Invalid, non-UTF-8, or non-token challenge fields
    /// are skipped.
    pub fn challenges(&self) -> Vec<AuthChallenge> {
        let header = match self.kind {
            AuthKind::Origin => WWW_AUTHENTICATE,
            AuthKind::Proxy => PROXY_AUTHENTICATE,
        };
        parse_auth_challenges(
            self.response_headers
                .get_all(header)
                .iter()
                .filter_map(|value| value.to_str().ok()),
        )
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

fn parse_auth_challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<AuthChallenge> {
    let mut challenges = Vec::new();
    for value in values {
        challenges.extend(parse_auth_challenge_header(value));
    }
    challenges
}

fn parse_auth_challenge_header(value: &str) -> Vec<AuthChallenge> {
    let mut challenges = Vec::new();
    let mut current: Option<AuthChallenge> = None;

    for part in split_top_level_commas(value) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(parameter) = parse_auth_param(part) {
            if let Some(challenge) = current.as_mut() {
                challenge.parameters.push(parameter);
                continue;
            }
        }

        if let Some(challenge) = current.take() {
            challenges.push(challenge);
        }
        current = parse_challenge_start(part);
    }

    if let Some(challenge) = current {
        challenges.push(challenge);
    }

    challenges
}

fn parse_challenge_start(value: &str) -> Option<AuthChallenge> {
    let (scheme, rest) = parse_token(value)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }

    let rest = rest.trim();
    let mut challenge = AuthChallenge {
        scheme: scheme.to_owned(),
        token68: None,
        parameters: Vec::new(),
    };
    if rest.is_empty() {
        return Some(challenge);
    }

    if is_token68(rest) {
        challenge.token68 = Some(rest.to_owned());
    } else if rest.contains('=') {
        challenge.parameters.extend(parse_auth_params(rest));
    }

    Some(challenge)
}

fn parse_auth_params(value: &str) -> Vec<AuthChallengeParam> {
    split_top_level_commas(value)
        .into_iter()
        .filter_map(parse_auth_param)
        .collect()
}

fn parse_auth_param(value: &str) -> Option<AuthChallengeParam> {
    let (name, rest) = parse_token(value.trim())?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let (value, remaining) = parse_auth_param_value(rest)?;
    remaining.trim().is_empty().then_some(AuthChallengeParam {
        name: name.to_owned(),
        value,
    })
}

fn parse_auth_param_value(value: &str) -> Option<(String, &str)> {
    if value.starts_with('"') {
        return parse_quoted_string(value);
    }

    let (value, rest) = parse_token(value)?;
    Some((value.to_owned(), rest))
}

fn parse_quoted_string(value: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut escaped = false;
    for (index, ch) in value.char_indices().skip(1) {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some((out, &value[index + ch.len_utf8()..])),
            _ => out.push(ch),
        }
    }
    None
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let mut escaped = false;

    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            ',' if !in_quote => {
                out.push(&value[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    out.push(&value[start..]);
    out
}

fn parse_token(value: &str) -> Option<(&str, &str)> {
    let end = value
        .char_indices()
        .take_while(|(_, ch)| is_token_char(*ch))
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    Some((&value[..end], &value[end..]))
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

fn is_token68(value: &str) -> bool {
    let mut seen_padding = false;
    let mut has_value = false;
    for ch in value.chars() {
        match ch {
            '=' => seen_padding = true,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' | '+' | '/' => {
                if seen_padding {
                    return false;
                }
                has_value = true;
            }
            _ => return false,
        }
    }
    has_value
}

#[cfg(test)]
mod tests {
    use http::header::{PROXY_AUTHENTICATE, WWW_AUTHENTICATE};
    use http::{HeaderMap, Method, Request, StatusCode, Version};

    use super::{is_token_char, parse_auth_challenge_header, AuthContext, AuthKind};
    use crate::RequestBody;

    #[test]
    fn parses_rfc7235_multi_challenge_example() {
        let challenges = parse_auth_challenge_header(
            r#"Newauth realm="apps", type=1, title="Login to \"apps\"", Basic realm="simple""#,
        );

        assert_eq!(challenges.len(), 2);
        assert_eq!(challenges[0].scheme(), "Newauth");
        assert_eq!(challenges[0].realm(), Some("apps"));
        assert_eq!(challenges[0].parameter("type"), Some("1"));
        assert_eq!(challenges[0].parameter("title"), Some("Login to \"apps\""));
        assert_eq!(challenges[1].scheme(), "Basic");
        assert_eq!(challenges[1].realm(), Some("simple"));
    }

    #[test]
    fn parses_token68_and_multiple_header_fields() {
        let mut headers = HeaderMap::new();
        headers.insert(WWW_AUTHENTICATE, "Bearer abcDEF123+/==".parse().unwrap());
        headers.append(WWW_AUTHENTICATE, r#"Basic realm="simple""#.parse().unwrap());
        let ctx = test_context(AuthKind::Origin, headers);

        let challenges = ctx.challenges();
        assert_eq!(challenges.len(), 2);
        assert_eq!(challenges[0].scheme(), "Bearer");
        assert_eq!(challenges[0].token68(), Some("abcDEF123+/=="));
        assert_eq!(challenges[1].scheme(), "Basic");
        assert_eq!(challenges[1].realm(), Some("simple"));
    }

    #[test]
    fn proxy_context_reads_proxy_authenticate_only() {
        let mut headers = HeaderMap::new();
        headers.insert(WWW_AUTHENTICATE, r#"Basic realm="origin""#.parse().unwrap());
        headers.insert(
            PROXY_AUTHENTICATE,
            r#"Digest realm="proxy", nonce="n""#.parse().unwrap(),
        );
        let ctx = test_context(AuthKind::Proxy, headers);

        let challenges = ctx.challenges();
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme(), "Digest");
        assert_eq!(challenges[0].realm(), Some("proxy"));
        assert_eq!(challenges[0].parameter("nonce"), Some("n"));
    }

    #[test]
    fn keeps_commas_inside_quoted_parameter_values() {
        let challenges =
            parse_auth_challenge_header(r#"Bearer realm="api, v1", scope="read,write""#);

        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme(), "Bearer");
        assert_eq!(challenges[0].realm(), Some("api, v1"));
        assert_eq!(challenges[0].parameter("scope"), Some("read,write"));
    }

    #[test]
    fn skips_malformed_and_non_utf8_challenge_fields() {
        let challenges = parse_auth_challenge_header(r#"=bad, Basic realm="simple""#);
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme(), "Basic");

        let mut headers = HeaderMap::new();
        headers.insert(
            WWW_AUTHENTICATE,
            http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        let ctx = test_context(AuthKind::Origin, headers);
        assert!(ctx.challenges().is_empty());
    }

    #[test]
    fn token_chars_match_http_tchar() {
        for ch in
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&'*+-.^_`|~".chars()
        {
            assert!(is_token_char(ch), "{ch:?} should be accepted");
        }

        for ch in "()<>@,;:\\\"/[]?={} \t\r\n".chars() {
            assert!(!is_token_char(ch), "{ch:?} should be rejected");
        }
    }

    #[test]
    fn rejects_invalid_token_chars_in_challenge_scheme_and_param_names() {
        let challenges = parse_auth_challenge_header(
            r#"Bad/Scheme realm="ignored", Basic realm="simple", bad/name="ignored""#,
        );

        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme(), "Basic");
        assert_eq!(challenges[0].realm(), Some("simple"));
        assert_eq!(challenges[0].parameter("bad/name"), None);
    }

    fn test_context(kind: AuthKind, response_headers: HeaderMap) -> AuthContext {
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .body(RequestBody::empty())
            .expect("request");
        AuthContext::new(
            kind,
            request.method().clone(),
            request.uri().clone(),
            Version::HTTP_11,
            request.headers().clone(),
            request.extensions().clone(),
            request.body().try_clone(),
            StatusCode::UNAUTHORIZED,
            response_headers,
            1,
            0,
            0,
            0,
        )
    }
}
