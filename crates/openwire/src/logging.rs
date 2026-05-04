use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE,
    PROXY_AUTHORIZATION, SET_COOKIE, TRANSFER_ENCODING, UPGRADE,
};
use http::{Request, Response, StatusCode, Uri};
use http_body::Body as _;
use http_body_util::BodyExt;
use openwire_core::{BoxFuture, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError};

const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const REDACTED_VALUE: &str = "██";
const STREAMING_BODY_MESSAGE: &str = "body omitted: streaming body";

pub trait HttpLogger: Send + Sync + 'static {
    fn log(&self, message: &str);
}

impl<F> HttpLogger for F
where
    F: Fn(&str) + Send + Sync + 'static,
{
    fn log(&self, message: &str) {
        (self)(message);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevel {
    None,
    #[default]
    Basic,
    Headers,
    Body,
}

#[derive(Clone, Debug, Default)]
pub struct StderrLogger;

impl HttpLogger for StderrLogger {
    fn log(&self, message: &str) {
        eprintln!("{message}");
    }
}

#[derive(Clone)]
pub struct LoggerInterceptor {
    config: Arc<LoggerConfig>,
}

#[derive(Clone)]
struct LoggerConfig {
    level: LogLevel,
    logger: Arc<dyn HttpLogger>,
    max_body_bytes: usize,
    redacted_headers: HashSet<HeaderName>,
}

struct CapturedResponse {
    response: Response<ResponseBody>,
    lines: Vec<String>,
}

enum BodyPreview {
    Text { text: String, len: usize },
    Omitted { reason: String },
}

enum ResponseBodyDecision {
    Skip,
    Omit(BodyPreview),
    Buffer,
}

impl LoggerInterceptor {
    pub fn new(level: LogLevel) -> Self {
        Self::with_logger(level, StderrLogger)
    }

    pub fn with_logger<L>(level: LogLevel, logger: L) -> Self
    where
        L: HttpLogger,
    {
        Self {
            config: Arc::new(LoggerConfig {
                level,
                logger: Arc::new(logger),
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                redacted_headers: default_redacted_headers(),
            }),
        }
    }

    pub fn level(&self) -> LogLevel {
        self.config.level
    }

    pub fn with_level(mut self, level: LogLevel) -> Self {
        Arc::make_mut(&mut self.config).level = level;
        self
    }

    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        Arc::make_mut(&mut self.config).max_body_bytes = max_body_bytes;
        self
    }

    pub fn redact_header(mut self, header: HeaderName) -> Self {
        Arc::make_mut(&mut self.config)
            .redacted_headers
            .insert(header);
        self
    }

    pub fn clear_redacted_headers(mut self) -> Self {
        Arc::make_mut(&mut self.config).redacted_headers.clear();
        self
    }
}

impl Default for LoggerInterceptor {
    fn default() -> Self {
        Self::new(LogLevel::Basic)
    }
}

impl Interceptor for LoggerInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<Response<ResponseBody>, WireError>> {
        if self.config.level == LogLevel::None {
            return next.run(exchange);
        }

        let config = self.config.clone();
        Box::pin(async move {
            let request_lines = capture_request(&config, exchange.request()).await?;
            emit_lines(&config, &request_lines);

            let request_uri = exchange.request().uri().clone();
            let started = Instant::now();
            match next.run(exchange).await {
                Ok(response) => {
                    let captured =
                        capture_response(&config, response, &request_uri, started.elapsed())
                            .await?;
                    emit_lines(&config, &captured.lines);
                    Ok(captured.response)
                }
                Err(error) => {
                    emit_lines(
                        &config,
                        &[format!("<-- HTTP FAILED {} ({})", request_uri, error)],
                    );
                    Err(error)
                }
            }
        })
    }
}

async fn capture_request(
    config: &LoggerConfig,
    request: &Request<RequestBody>,
) -> Result<Vec<String>, WireError> {
    let mut lines = vec![format!("--> {} {}", request.method(), request.uri())];
    if config.level == LogLevel::Basic {
        return Ok(lines);
    }

    push_headers(&mut lines, config, request.headers());
    if config.level == LogLevel::Headers {
        lines.push(format!("--> END {}", request.method()));
        return Ok(lines);
    }

    match capture_request_body(config, request).await? {
        Some(preview) => {
            push_body_preview(&mut lines, &preview);
            lines.push(format!(
                "--> END {} ({})",
                request.method(),
                preview.end_summary()
            ));
        }
        None => lines.push(format!("--> END {}", request.method())),
    }

    Ok(lines)
}

async fn capture_response(
    config: &LoggerConfig,
    response: Response<ResponseBody>,
    request_uri: &Uri,
    elapsed: Duration,
) -> Result<CapturedResponse, WireError> {
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("");
    let mut lines = vec![if status_text.is_empty() {
        format!(
            "<-- {} {} ({} ms)",
            status.as_u16(),
            request_uri,
            elapsed.as_millis()
        )
    } else {
        format!(
            "<-- {} {} {} ({} ms)",
            status.as_u16(),
            status_text,
            request_uri,
            elapsed.as_millis()
        )
    }];
    if config.level == LogLevel::Basic {
        return Ok(CapturedResponse { response, lines });
    }

    push_headers(&mut lines, config, response.headers());
    if config.level == LogLevel::Headers {
        lines.push("<-- END HTTP".to_string());
        return Ok(CapturedResponse { response, lines });
    }

    match response_body_decision(config, &response) {
        ResponseBodyDecision::Skip => {
            lines.push("<-- END HTTP".to_string());
            Ok(CapturedResponse { response, lines })
        }
        ResponseBodyDecision::Omit(preview) => {
            push_body_preview(&mut lines, &preview);
            lines.push(format!("<-- END HTTP ({})", preview.end_summary()));
            Ok(CapturedResponse { response, lines })
        }
        ResponseBodyDecision::Buffer => {
            let (parts, body) = response.into_parts();
            let bytes = body.bytes().await?;
            let preview = capture_buffered_body(config, &parts.headers, bytes.as_ref());
            let response = Response::from_parts(parts, ResponseBody::from(bytes));
            push_body_preview(&mut lines, &preview);
            lines.push(format!("<-- END HTTP ({})", preview.end_summary()));
            Ok(CapturedResponse { response, lines })
        }
    }
}

async fn capture_request_body(
    config: &LoggerConfig,
    request: &Request<RequestBody>,
) -> Result<Option<BodyPreview>, WireError> {
    if request.body().is_absent() {
        return Ok(None);
    }

    let Some(body) = request.body().try_clone() else {
        return Ok(Some(BodyPreview::omitted(STREAMING_BODY_MESSAGE)));
    };

    let bytes = body.collect().await?.to_bytes();
    Ok(Some(capture_buffered_body(
        config,
        request.headers(),
        bytes.as_ref(),
    )))
}

fn response_body_decision(
    config: &LoggerConfig,
    response: &Response<ResponseBody>,
) -> ResponseBodyDecision {
    if !response_can_have_body(response.status()) {
        return ResponseBodyDecision::Skip;
    }

    if response.status() == StatusCode::SWITCHING_PROTOCOLS || is_upgraded(response.headers()) {
        return ResponseBodyDecision::Omit(BodyPreview::omitted("body omitted: upgraded protocol"));
    }

    if is_event_stream(response.headers()) || is_chunked(response.headers()) {
        return ResponseBodyDecision::Omit(BodyPreview::omitted(STREAMING_BODY_MESSAGE));
    }

    let content_length = header_content_length(response.headers())
        .or_else(|| response.body().size_hint().upper())
        .and_then(|len| usize::try_from(len).ok());

    if let Some(len) = content_length {
        if len > config.max_body_bytes {
            return ResponseBodyDecision::Omit(BodyPreview::omitted(format!(
                "body omitted: {}-byte body exceeds {}-byte limit",
                len, config.max_body_bytes
            )));
        }
        return ResponseBodyDecision::Buffer;
    } else {
        return ResponseBodyDecision::Omit(BodyPreview::omitted(STREAMING_BODY_MESSAGE));
    }
}

fn capture_buffered_body(
    config: &LoggerConfig,
    headers: &HeaderMap<HeaderValue>,
    bytes: &[u8],
) -> BodyPreview {
    if bytes.is_empty() {
        return BodyPreview::Text {
            text: String::new(),
            len: 0,
        };
    }

    if bytes.len() > config.max_body_bytes {
        return BodyPreview::omitted(format!(
            "body omitted: {}-byte body exceeds {}-byte limit",
            bytes.len(),
            config.max_body_bytes
        ));
    }

    if is_json(headers) {
        if let Some(text) = pretty_json(bytes) {
            return BodyPreview::Text {
                text,
                len: bytes.len(),
            };
        }
    }

    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => BodyPreview::Text {
            text,
            len: bytes.len(),
        },
        Err(_) => BodyPreview::omitted(format!("binary {}-byte body omitted", bytes.len())),
    }
}

fn push_headers(lines: &mut Vec<String>, config: &LoggerConfig, headers: &HeaderMap<HeaderValue>) {
    for (name, value) in headers {
        let rendered = if config.redacted_headers.contains(name) {
            REDACTED_VALUE.to_string()
        } else {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        };
        lines.push(format!("{}: {}", format_header_name(name), rendered));
    }
}

fn push_body_preview(lines: &mut Vec<String>, preview: &BodyPreview) {
    lines.push(String::new());
    match preview {
        BodyPreview::Text { text, .. } => {
            if !text.is_empty() {
                lines.extend(text.lines().map(ToOwned::to_owned));
            }
        }
        BodyPreview::Omitted { reason } => lines.push(format!("<{reason}>")),
    }
}

fn emit_lines(config: &LoggerConfig, lines: &[String]) {
    for line in lines {
        config.logger.log(line);
    }
}

fn pretty_json(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut rendered = Vec::new();
    serde_json::to_writer_pretty(&mut rendered, &value).ok()?;
    rendered.write_all(b"\n").ok()?;
    String::from_utf8(rendered).ok()
}

fn is_json(headers: &HeaderMap<HeaderValue>) -> bool {
    let Some(content_type) = content_type(headers) else {
        return false;
    };
    content_type == "application/json" || content_type.ends_with("+json")
}

fn is_event_stream(headers: &HeaderMap<HeaderValue>) -> bool {
    content_type(headers).as_deref() == Some("text/event-stream")
}

fn is_chunked(headers: &HeaderMap<HeaderValue>) -> bool {
    headers
        .get(TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
        })
        .unwrap_or(false)
}

fn is_upgraded(headers: &HeaderMap<HeaderValue>) -> bool {
    headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn content_type(headers: &HeaderMap<HeaderValue>) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
}

fn header_content_length(headers: &HeaderMap<HeaderValue>) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn format_header_name(name: &HeaderName) -> String {
    name.as_str()
        .split('-')
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => {
                    let mut rendered = String::new();
                    rendered.extend(first.to_uppercase());
                    rendered.push_str(chars.as_str());
                    rendered
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn response_can_have_body(status: StatusCode) -> bool {
    !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
}

fn default_redacted_headers() -> HashSet<HeaderName> {
    HashSet::from([AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE, SET_COOKIE])
}

impl BodyPreview {
    fn omitted(reason: impl Into<String>) -> Self {
        Self::Omitted {
            reason: reason.into(),
        }
    }

    fn end_summary(&self) -> String {
        match self {
            BodyPreview::Text { len, .. } => format!("{len}-byte body"),
            BodyPreview::Omitted { reason } => reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use futures_util::stream;
    use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
    use http::{Method, Request, Response, StatusCode};
    use http_body_util::BodyExt;
    use tower::service_fn;
    use tower::util::BoxCloneSyncService;

    use super::{LogLevel, LoggerInterceptor};
    use crate::{CallContext, Interceptor, Next, NoopEventListenerFactory, RequestBody, WireError};
    use openwire_core::{Exchange, ResponseBody, SharedEventListenerFactory};

    #[tokio::test]
    async fn logger_pretty_prints_json_and_rebuilds_response_body() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logger = {
            let lines = lines.clone();
            move |line: &str| lines.lock().expect("lines").push(line.to_string())
        };
        let interceptor = LoggerInterceptor::with_logger(LogLevel::Body, logger);

        let request = Request::builder()
            .method("POST")
            .uri("https://api.example.com/users")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(RequestBody::from_static(br#"{"name":"Alice","age":18}"#))
            .expect("request");
        let exchange = test_exchange(request);
        let next = Next::new(BoxCloneSyncService::new(service_fn(
            |_exchange| async move {
                Ok::<_, WireError>(
                    Response::builder()
                        .status(StatusCode::CREATED)
                        .header(CONTENT_TYPE, "application/json")
                        .header(CONTENT_LENGTH, "27")
                        .body(ResponseBody::from(Bytes::from_static(
                            br#"{"id":123,"name":"Alice"}"#,
                        )))
                        .expect("response"),
                )
            },
        )));

        let response = interceptor
            .intercept(exchange, next)
            .await
            .expect("response");
        let body = response.into_body().text().await.expect("body");
        assert_eq!(body, r#"{"id":123,"name":"Alice"}"#);

        let lines = lines.lock().expect("lines").join("\n");
        assert!(lines.contains("--> POST https://api.example.com/users"));
        assert!(lines.contains("Authorization: ██"));
        assert!(lines.contains("\"name\": \"Alice\""));
        assert!(lines.contains("<-- 201 Created https://api.example.com/users"));
        assert!(lines.contains("<-- END HTTP (25-byte body)"));
    }

    #[tokio::test]
    async fn logger_omits_streaming_bodies() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logger = {
            let lines = lines.clone();
            move |line: &str| lines.lock().expect("lines").push(line.to_string())
        };
        let interceptor = LoggerInterceptor::with_logger(LogLevel::Body, logger);

        let request = Request::builder()
            .method(Method::POST)
            .uri("http://example.com/stream")
            .body(RequestBody::from_stream(stream::iter([
                Ok::<_, WireError>(Bytes::from_static(b"hello")),
            ])))
            .expect("request");
        let exchange = test_exchange(request);
        let next = Next::new(BoxCloneSyncService::new(service_fn(
            |_exchange| async move {
                let body = http_body_util::StreamBody::new(stream::iter([
                    Ok::<_, WireError>(http_body::Frame::data(Bytes::from_static(b"chunk-1"))),
                    Ok::<_, WireError>(http_body::Frame::data(Bytes::from_static(b"chunk-2"))),
                ]));
                Ok::<_, WireError>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(TRANSFER_ENCODING, "chunked")
                        .body(ResponseBody::new(body.boxed()))
                        .expect("response"),
                )
            },
        )));

        let response = interceptor
            .intercept(exchange, next)
            .await
            .expect("response");
        let body = response.into_body().text().await.expect("body");
        assert_eq!(body, "chunk-1chunk-2");

        let lines = lines.lock().expect("lines").join("\n");
        assert!(lines.contains("<body omitted: streaming body>"));
        assert!(lines.contains("<-- END HTTP (body omitted: streaming body)"));
    }

    fn test_exchange(request: Request<RequestBody>) -> Exchange {
        let factory = Arc::new(NoopEventListenerFactory) as SharedEventListenerFactory;
        let context = CallContext::from_factory(&factory, &request, None);
        Exchange::new(request, context, 1)
    }
}
