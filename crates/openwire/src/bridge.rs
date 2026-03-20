use http::header::{CONTENT_LENGTH, HOST, TRANSFER_ENCODING, USER_AGENT};
use http::{HeaderValue, Request, Version};
use openwire_core::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};

const DEFAULT_USER_AGENT: &str = concat!("openwire/", env!("CARGO_PKG_VERSION"));
const CHUNKED: HeaderValue = HeaderValue::from_static("chunked");
const DEFAULT_USER_AGENT_VALUE: HeaderValue = HeaderValue::from_static(DEFAULT_USER_AGENT);

#[derive(Debug, Default)]
pub(crate) struct BridgeInterceptor;

impl Interceptor for BridgeInterceptor {
    fn intercept(
        &self,
        mut exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<http::Response<ResponseBody>, WireError>> {
        let normalization = normalize_request(exchange.request_mut());
        Box::pin(async move {
            normalization?;
            next.run(exchange).await
        })
    }
}

fn normalize_request(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    normalize_host_header(request)?;
    normalize_user_agent_header(request);
    normalize_body_headers(request);
    Ok(())
}

fn normalize_host_header(request: &mut Request<RequestBody>) -> Result<(), WireError> {
    if request.headers().contains_key(HOST) {
        return Ok(());
    }

    let authority = request
        .uri()
        .authority()
        .ok_or_else(|| WireError::invalid_request("request URI is missing an authority"))?;
    let host = HeaderValue::from_str(authority.as_str()).map_err(|error| {
        WireError::invalid_request(format!(
            "request URI authority is not a valid Host header: {error}"
        ))
    })?;
    request.headers_mut().insert(HOST, host);
    Ok(())
}

fn normalize_user_agent_header(request: &mut Request<RequestBody>) {
    if !request.headers().contains_key(USER_AGENT) {
        request
            .headers_mut()
            .insert(USER_AGENT, DEFAULT_USER_AGENT_VALUE.clone());
    }
}

fn normalize_body_headers(request: &mut Request<RequestBody>) {
    let version = request.version();
    let replayable_len = request.body().replayable_len();
    let headers = request.headers_mut();

    match replayable_len {
        Some(len) => {
            let value = HeaderValue::from_str(&len.to_string())
                .expect("numeric content-length is always a valid header value");
            headers.insert(CONTENT_LENGTH, value);
            headers.remove(TRANSFER_ENCODING);
        }
        None => {
            headers.remove(CONTENT_LENGTH);
            if uses_chunked_transfer_encoding(version) {
                headers.insert(TRANSFER_ENCODING, CHUNKED.clone());
            } else {
                headers.remove(TRANSFER_ENCODING);
            }
        }
    }
}

fn uses_chunked_transfer_encoding(version: Version) -> bool {
    version == Version::HTTP_11
}
