use http::header::CONNECTION;
use http::{HeaderMap, HeaderValue, Request, Response, Version};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::client::legacy::connect::Connected;
use openwire_core::{
    next_connection_id, BoxConnection, CoalescingInfo, ConnectionInfo, HyperExecutor, RequestBody,
    SharedTimer, WireError,
};

use crate::connection::{Address, ConnectionProtocol, RouteKind, UriScheme};

pub(super) async fn bind_http1(
    stream: BoxConnection,
) -> Result<
    (
        http1::SendRequest<RequestBody>,
        http1::Connection<BoxConnection, RequestBody>,
    ),
    WireError,
> {
    http1::Builder::new()
        .handshake(stream)
        .await
        .map_err(|error| WireError::protocol_binding("HTTP/1.1 client handshake failed", error))
}

pub(super) async fn bind_http2(
    stream: BoxConnection,
    config: &crate::client::TransportConfig,
    executor: HyperExecutor,
    timer: SharedTimer,
) -> Result<
    (
        http2::SendRequest<RequestBody>,
        http2::Connection<BoxConnection, RequestBody, HyperExecutor>,
    ),
    WireError,
> {
    let mut builder = http2::Builder::new(executor);
    builder.timer(timer);
    if let Some(interval) = config.http2_keep_alive_interval {
        builder.keep_alive_interval(interval);
        builder.keep_alive_while_idle(config.http2_keep_alive_while_idle);
    }
    builder
        .handshake(stream)
        .await
        .map_err(|error| WireError::protocol_binding("HTTP/2 client handshake failed", error))
}

pub(super) fn determine_protocol(address: &Address, connected: &Connected) -> ConnectionProtocol {
    if address.scheme() == UriScheme::Http {
        ConnectionProtocol::Http1
    } else if connected.is_negotiated_h2() {
        ConnectionProtocol::Http2
    } else {
        ConnectionProtocol::Http1
    }
}

pub(super) fn prepare_bound_request(
    mut request: Request<RequestBody>,
    protocol: ConnectionProtocol,
    route_kind: &RouteKind,
) -> Result<Request<RequestBody>, WireError> {
    if protocol != ConnectionProtocol::Http1
        || matches!(route_kind, RouteKind::HttpForwardProxy { .. })
    {
        return Ok(request);
    }

    let origin_form = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    *request.uri_mut() = origin_form.parse().map_err(|error| {
        WireError::internal(
            "failed to normalize request URI for direct HTTP/1.1 binding",
            error,
        )
    })?;
    Ok(request)
}

pub(super) fn http1_exchange_allows_reuse(
    request_requests_close: bool,
    response: &Response<Incoming>,
) -> bool {
    if request_requests_close {
        return false;
    }
    if response.version() == Version::HTTP_10 {
        return false;
    }

    !connection_header_requests_close(response.headers())
}

pub(super) fn connection_header_requests_close(headers: &HeaderMap) -> bool {
    headers
        .get_all(CONNECTION)
        .iter()
        .any(|value| connection_header_value_requests_close(value).unwrap_or(true))
}

fn connection_header_value_requests_close(value: &HeaderValue) -> Result<bool, ()> {
    let value = value.to_str().map_err(|_| ())?;
    let bytes = value.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        while index < bytes.len() && is_optional_whitespace(bytes[index]) {
            index += 1;
        }
        if index == bytes.len() {
            return Ok(false);
        }
        if bytes[index] == b',' {
            index += 1;
            continue;
        }

        let token_start = index;
        while index < bytes.len() && is_tchar(bytes[index]) {
            index += 1;
        }
        if token_start == index {
            return Err(());
        }

        let token = &value[token_start..index];
        while index < bytes.len() && is_optional_whitespace(bytes[index]) {
            index += 1;
        }

        match bytes.get(index).copied() {
            None => return Ok(token.eq_ignore_ascii_case("close")),
            Some(b',') => {
                if token.eq_ignore_ascii_case("close") {
                    return Ok(true);
                }
                index += 1;
            }
            Some(_) => return Err(()),
        }
    }

    Ok(false)
}

fn is_optional_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn is_tchar(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
}

pub(super) fn map_hyper_error(error: hyper::Error) -> WireError {
    if let Some(source) = find_wire_error(&error) {
        return source.clone();
    }

    if error.is_canceled() {
        return WireError::canceled("request canceled");
    }
    if error.is_timeout() {
        return WireError::timeout("request timed out");
    }

    WireError::protocol("transport request failed", error)
}

fn find_wire_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a WireError> {
    let mut current = Some(error);
    while let Some(source) = current {
        if let Some(wire_error) = source.downcast_ref::<WireError>() {
            return Some(wire_error);
        }
        current = source.source();
    }
    None
}

pub(super) fn connection_info_from_connected(connected: &Connected) -> ConnectionInfo {
    let mut extensions = http::Extensions::new();
    connected.get_extras(&mut extensions);
    extensions
        .remove::<ConnectionInfo>()
        .unwrap_or(ConnectionInfo {
            id: next_connection_id(),
            remote_addr: None,
            local_addr: None,
            tls: false,
        })
}

pub(super) fn coalescing_info_from_connected(connected: &Connected) -> CoalescingInfo {
    let mut extensions = http::Extensions::new();
    connected.get_extras(&mut extensions);
    extensions.remove::<CoalescingInfo>().unwrap_or_default()
}
