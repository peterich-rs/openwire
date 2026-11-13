use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::task::AtomicWaker;
use http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::rt::Timer;
use openwire_core::{
    CallContext, FailurePhase, RequestBody, ResponseBody, SharedTimer, WireError, WireExecutor,
};

use crate::connection::{
    ConnectionAvailability, ConnectionProtocol, ExchangeFinder, RealConnection,
};

use super::bindings::{teardown_pooled_connection, ConnectionBindings, ConnectionTaskRegistry};

pub(super) struct BoundResponse {
    pub(super) response: Response<Incoming>,
    pub(super) release: ResponseLease,
}

pub(super) struct ResponseLease {
    state: Option<ResponseLeaseState>,
}

pub(super) struct ResponseLeaseShared {
    exchange_finder: Arc<ExchangeFinder>,
    ctx: CallContext,
    _tasks: ConnectionTaskRegistry,
    availability: ConnectionAvailability,
}

enum ResponseLeaseState {
    Http1 {
        connection: RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
        shared: ResponseLeaseShared,
    },
    Http2 {
        connection: RealConnection,
        shared: ResponseLeaseShared,
    },
}

impl ResponseLeaseShared {
    pub(super) fn new(
        exchange_finder: Arc<ExchangeFinder>,
        ctx: CallContext,
        tasks: ConnectionTaskRegistry,
        availability: ConnectionAvailability,
    ) -> Self {
        Self {
            exchange_finder,
            ctx,
            _tasks: tasks,
            availability,
        }
    }
}

impl ResponseLease {
    pub(super) fn http1(
        connection: RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
        shared: ResponseLeaseShared,
    ) -> Self {
        Self {
            state: Some(ResponseLeaseState::Http1 {
                connection,
                bindings,
                sender,
                reusable,
                shared,
            }),
        }
    }

    pub(super) fn http2(connection: RealConnection, shared: ResponseLeaseShared) -> Self {
        Self {
            state: Some(ResponseLeaseState::Http2 { connection, shared }),
        }
    }

    fn release(mut self) {
        if let Some(state) = self.state.take() {
            release_response_lease(state);
        }
    }

    fn discard(mut self) {
        if let Some(state) = self.state.take() {
            discard_response_lease(state);
        }
    }

    #[cfg(test)]
    pub(super) fn discard_for_test(self) {
        self.discard();
    }
}

impl Drop for ResponseLease {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            abandon_response_lease_state(state);
        }
    }
}

pub(super) struct ObservedOutgoingBody {
    inner: RequestBody,
    ctx: CallContext,
    bytes_sent: u64,
    fixed_length: Option<u64>,
    started: bool,
    finished: bool,
}

impl ObservedOutgoingBody {
    fn wrap(body: RequestBody, ctx: CallContext, fixed_length: Option<u64>) -> RequestBody {
        RequestBody::from_body(Self {
            inner: body,
            ctx,
            bytes_sent: 0,
            fixed_length,
            started: false,
            finished: false,
        })
    }

    fn finish_successfully(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_started();
        self.ctx.mark_request_write_complete();
        self.ctx
            .listener()
            .request_body_end(&self.ctx, self.bytes_sent);
    }

    fn finish_with_error(&mut self, error: &WireError) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_started();
        self.ctx.mark_request_failed_emitted();
        self.ctx.listener().request_failed(&self.ctx, error);
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.ctx.listener().request_body_start(&self.ctx);
    }
}

impl Drop for ObservedOutgoingBody {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.started
            && (self.inner.is_end_stream() || self.fixed_length == Some(self.bytes_sent))
        {
            // Hyper may stop polling after the exact Content-Length payload
            // on HTTP/1 without a trailing `None` frame. This is wire framing
            // completion, independent of the source body's EOF validation.
            self.finish_successfully();
        } else {
            self.finished = true;
        }
    }
}

impl Body for ObservedOutgoingBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        this.ensure_started();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes_sent += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish_with_error(&error);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish_successfully();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn http1_fixed_length(request: &Request<RequestBody>, protocol: ConnectionProtocol) -> Option<u64> {
    if protocol != ConnectionProtocol::Http1 || request.headers().contains_key(TRANSFER_ENCODING) {
        return None;
    }
    request
        .headers()
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

pub(super) fn present_empty_outgoing_body(
    request: &Request<RequestBody>,
    protocol: ConnectionProtocol,
) -> bool {
    !request.body().is_absent()
        && (request.body().is_end_stream() || http1_fixed_length(request, protocol) == Some(0))
}

pub(super) fn emit_present_empty_body_events(ctx: &CallContext) {
    ctx.listener().request_body_start(ctx);
    ctx.listener().request_body_end(ctx, 0);
    ctx.mark_request_write_complete();
}

pub(super) fn instrument_outgoing_body(
    request: Request<RequestBody>,
    ctx: &CallContext,
    protocol: ConnectionProtocol,
) -> Request<RequestBody> {
    if request.body().is_absent() {
        ctx.mark_request_write_complete();
        return request;
    }
    if present_empty_outgoing_body(&request, protocol) {
        return request;
    }
    // Read final framing headers after protocol binding and network
    // interceptors. HTTP/2 completion always depends on EOF/END_STREAM.
    let fixed_length = http1_fixed_length(&request, protocol);
    let (parts, body) = request.into_parts();
    Request::from_parts(
        parts,
        ObservedOutgoingBody::wrap(body, ctx.clone(), fixed_length),
    )
}

pub(super) struct ObservedIncomingBody {
    inner: Incoming,
    ctx: CallContext,
    attempt: u32,
    bytes_read: u64,
    release: Option<ResponseLease>,
    deadline_signal: Option<Arc<BodyDeadlineSignal>>,
    span: tracing::Span,
    finished: bool,
    body_started: bool,
}

impl ObservedIncomingBody {
    pub(super) fn wrap(
        body: Incoming,
        ctx: CallContext,
        attempt: u32,
        release: Option<ResponseLease>,
        deadline_signal: Option<Arc<BodyDeadlineSignal>>,
        span: tracing::Span,
    ) -> ResponseBody {
        ResponseBody::new(
            Self {
                inner: body,
                ctx,
                attempt,
                bytes_read: 0,
                release,
                deadline_signal,
                span,
                finished: false,
                body_started: false,
            }
            .boxed(),
        )
    }

    fn ensure_body_started(&mut self) {
        if self.body_started {
            return;
        }
        self.body_started = true;
        self.ctx.listener().response_body_start(&self.ctx);
    }

    fn finish_successfully(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_body_started();
        self.ctx
            .listener()
            .response_body_end(&self.ctx, self.bytes_read);
        self.release_connection();
    }

    fn finish_with_error(&mut self, error: &WireError) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_body_started();
        self.span.in_scope(|| {
            tracing::debug!(
                call_id = self.ctx.call_id().as_u64(),
                attempt = self.attempt,
                error_kind = %error.kind(),
                error_phase = %error.phase(),
                error_message = %error.message(),
                bytes_read = self.bytes_read,
                "response body failed",
            );
        });
        self.ctx.listener().response_body_failed(&self.ctx, error);
        self.discard_connection();
    }

    fn finish_abandoned(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_body_started();
        if self.ctx.body_force_discard() {
            // Decode/body-layer errors set this flag so Drop discards the
            // connection instead of treating the body as a clean abandon.
            self.ctx.listener().response_body_failed(
                &self.ctx,
                &WireError::body(
                    "response body failed after intermediate processing error",
                    std::io::Error::other("body force discard"),
                ),
            );
            self.discard_connection();
            return;
        }
        self.ctx
            .listener()
            .response_body_end(&self.ctx, self.bytes_read);
        abandon_response_lease(self.release.take());
    }

    fn poll_deadline(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, WireError>>> {
        let Some(deadline_signal) = self.deadline_signal.as_ref() else {
            return Poll::Pending;
        };
        if !deadline_signal.expired.load(Ordering::Acquire) {
            deadline_signal.waker.register(cx.waker());
            if !deadline_signal.expired.load(Ordering::Acquire) {
                return Poll::Pending;
            }
        }

        let deadline = self.ctx.deadline().ok_or_else(|| {
            WireError::internal(
                "body deadline signal missing call deadline",
                std::io::Error::other("deadline invariant violated"),
            )
        })?;
        let timeout = deadline.saturating_duration_since(self.ctx.created_at());
        let error = WireError::body_timeout(format!("call timed out after {timeout:?}"));
        self.finish_with_error(&error);
        Poll::Ready(Some(Err(error)))
    }

    fn release_connection(&mut self) {
        if let Some(release) = self.release.take() {
            release.release();
        }
    }

    fn discard_connection(&mut self) {
        if let Some(release) = self.release.take() {
            release.discard();
        }
    }
}

pub(super) struct BodyDeadlineSignal {
    pub(super) expired: AtomicBool,
    waker: AtomicWaker,
}

pub(super) fn abandon_response_lease(release: Option<ResponseLease>) {
    drop(release);
}

fn release_response_lease(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            sender,
            reusable,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    _tasks,
                },
            ..
        } => {
            if reusable
                && bindings.release_http1(connection.id(), sender)
                && exchange_finder.release(&connection)
            {
                availability.notify(connection.address());
                ctx.listener().connection_released(&ctx, connection.id());
                return;
            }
            teardown_pooled_connection(&exchange_finder, &bindings, &_tasks, connection.id());
            availability.notify(connection.address());
            ctx.listener().connection_released(&ctx, connection.id());
        }
        ResponseLeaseState::Http2 {
            connection,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            let _ = exchange_finder.release(&connection);
            availability.notify_http2(
                connection.address(),
                &connection.coalescing().verified_server_names,
            );
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn discard_response_lease(state: ResponseLeaseState) {
    evict_response_lease_state(state, true);
}

fn abandon_response_lease_state(state: ResponseLeaseState) {
    evict_response_lease_state(state, false);
}

fn evict_response_lease_state(state: ResponseLeaseState, mark_unhealthy: bool) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    _tasks,
                },
            ..
        } => {
            teardown_pooled_connection(&exchange_finder, &bindings, &_tasks, connection.id());
            availability.notify(connection.address());
            ctx.listener().connection_released(&ctx, connection.id());
        }
        ResponseLeaseState::Http2 {
            connection,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            if mark_unhealthy {
                connection.mark_unhealthy();
            }
            let _ = exchange_finder.release(&connection);
            availability.notify_http2(
                connection.address(),
                &connection.coalescing().verified_server_names,
            );
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

pub(super) fn spawn_body_deadline_signal(
    executor: Arc<dyn WireExecutor>,
    timer: SharedTimer,
    ctx: &CallContext,
) -> Result<Option<Arc<BodyDeadlineSignal>>, WireError> {
    let Some(deadline) = ctx.deadline() else {
        return Ok(None);
    };

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let signal = Arc::new(BodyDeadlineSignal {
        expired: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    if remaining == Duration::ZERO {
        signal.expired.store(true, Ordering::Release);
        return Ok(Some(signal));
    }

    let signal_task = signal.clone();
    let _ = executor.spawn(Box::pin(async move {
        timer.sleep(remaining).await;
        signal_task.expired.store(true, Ordering::Release);
        signal_task.waker.wake();
    }))?;

    Ok(Some(signal))
}

impl Drop for ObservedIncomingBody {
    fn drop(&mut self) {
        self.finish_abandoned();
    }
}

impl Body for ObservedIncomingBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if let Poll::Ready(result) = this.poll_deadline(cx) {
            return Poll::Ready(result);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.ensure_body_started();
                    this.bytes_read += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                let error = WireError::from(error)
                    .with_phase(FailurePhase::ResponseBody)
                    .with_request_committed();
                this.finish_with_error(&error);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish_successfully();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    use futures_util::{stream, StreamExt};
    use openwire_core::NoopEventListener;

    fn instrument(
        body: RequestBody,
        length: u64,
        protocol: ConnectionProtocol,
    ) -> (RequestBody, CallContext) {
        let ctx = CallContext::new(Arc::new(NoopEventListener), None);
        // Deliberately leave the request version at its default. Negotiated
        // protocol, rather than this version, controls framing completion.
        let request = Request::builder()
            .header(CONTENT_LENGTH, length)
            .body(body)
            .expect("request");
        (
            instrument_outgoing_body(request, &ctx, protocol).into_body(),
            ctx,
        )
    }

    #[tokio::test]
    async fn http1_fixed_length_completes_when_sender_stops_polling() {
        let source =
            stream::iter([Ok::<_, WireError>(Bytes::from_static(b"abc"))]).chain(stream::pending());
        let composite = RequestBody::concat(vec![RequestBody::from_stream_with_length(source, 3)])
            .expect("concat");
        let (mut body, ctx) = instrument(composite, 3, ConnectionProtocol::Http1);
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "abc"
        );
        assert!(!ctx.request_write_complete());
        drop(body);
        assert!(ctx.request_write_complete());
    }

    #[tokio::test]
    async fn http2_waits_for_eof_even_after_declared_data_is_read() {
        let source =
            stream::iter([Ok::<_, WireError>(Bytes::from_static(b"abc"))]).chain(stream::pending());
        let (mut body, ctx) = instrument(
            RequestBody::from_stream_with_length(source, 3),
            3,
            ConnectionProtocol::Http2,
        );
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "abc"
        );
        assert!(!body.is_end_stream());
        drop(body);
        assert!(
            !ctx.request_write_complete(),
            "reset/cancel before EOF is incomplete on H2"
        );
    }

    #[tokio::test]
    async fn fixed_length_does_not_turn_source_errors_into_success() {
        let source = stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Err(WireError::body(
                "source failed",
                std::io::Error::other("read failed"),
            )),
        ]);
        let (mut body, ctx) = instrument(
            RequestBody::from_stream_with_length(source, 3),
            3,
            ConnectionProtocol::Http1,
        );
        assert!(body.frame().await.unwrap().is_ok());
        assert!(body.frame().await.unwrap().is_err());
        drop(body);
        assert!(!ctx.request_write_complete());
        assert!(ctx.request_failed_emitted());
    }

    #[tokio::test]
    async fn incomplete_fixed_length_does_not_complete_when_dropped() {
        let source =
            stream::iter([Ok::<_, WireError>(Bytes::from_static(b"a"))]).chain(stream::pending());
        let (mut body, ctx) = instrument(
            RequestBody::from_stream_with_length(source, 3),
            3,
            ConnectionProtocol::Http1,
        );
        assert!(body.frame().await.unwrap().is_ok());
        drop(body);
        assert!(!ctx.request_write_complete());
    }

    #[tokio::test]
    async fn transfer_encoding_and_arbitrary_trailers_require_eof() {
        let source =
            stream::iter([Ok::<_, WireError>(Bytes::from_static(b"abc"))]).chain(stream::pending());
        let arbitrary = RequestBody::from_body(RequestBody::from_stream_with_length(source, 3));
        let ctx = CallContext::new(Arc::new(NoopEventListener), None);
        let request = Request::builder()
            .header(CONTENT_LENGTH, 3)
            .header(TRANSFER_ENCODING, "chunked")
            .body(arbitrary)
            .unwrap();
        let mut body =
            instrument_outgoing_body(request, &ctx, ConnectionProtocol::Http1).into_body();
        assert!(body.frame().await.unwrap().is_ok());
        drop(body);
        assert!(!ctx.request_write_complete());
    }

    #[test]
    fn zero_length_is_empty_framing_only_for_http1() {
        for protocol in [ConnectionProtocol::Http1, ConnectionProtocol::Http2] {
            let request = Request::builder()
                .header(CONTENT_LENGTH, 0)
                .body(RequestBody::from_stream_with_length(
                    stream::pending::<Result<Bytes, WireError>>(),
                    0,
                ))
                .unwrap();
            assert_eq!(
                present_empty_outgoing_body(&request, protocol),
                protocol == ConnectionProtocol::Http1
            );
        }
    }
}
