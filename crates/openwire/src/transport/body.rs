use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::task::AtomicWaker;
use http::Response;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::rt::Timer;
use openwire_core::{CallContext, RequestBody, ResponseBody, SharedTimer, WireError, WireExecutor};

use crate::connection::{ConnectionAvailability, ExchangeFinder, RealConnection};

use super::bindings::{ConnectionBindings, ConnectionTaskRegistry};

pub(super) struct BoundResponse {
    pub(super) response: Response<Incoming>,
    pub(super) release: ResponseLease,
    pub(super) reused: bool,
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
}

impl Drop for ResponseLease {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            abandon_response_lease_state(state);
        }
    }
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
            }
            .boxed(),
        )
    }

    fn finish_successfully(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
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
        self.span.in_scope(|| {
            tracing::debug!(
                call_id = self.ctx.call_id().as_u64(),
                attempt = self.attempt,
                error_kind = %error.kind(),
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

        let deadline = self
            .ctx
            .deadline()
            .expect("deadline signal exists only when call timeout is configured");
        let timeout = deadline.saturating_duration_since(self.ctx.created_at());
        let error = WireError::timeout(format!("call timed out after {timeout:?}"));
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
                    ..
                },
            ..
        } => {
            if reusable
                && bindings.release_http1(connection.id(), sender)
                && exchange_finder.release(&connection)
            {
                availability.notify();
                ctx.listener().connection_released(&ctx, connection.id());
                return;
            }
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
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
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn discard_response_lease(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
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
            connection.mark_unhealthy();
            let _ = exchange_finder.release(&connection);
            availability.notify();
            ctx.listener().connection_released(&ctx, connection.id());
        }
    }
}

fn abandon_response_lease_state(state: ResponseLeaseState) {
    match state {
        ResponseLeaseState::Http1 {
            connection,
            bindings,
            shared:
                ResponseLeaseShared {
                    exchange_finder,
                    ctx,
                    availability,
                    ..
                },
            ..
        } => {
            bindings.remove(connection.id());
            let _ = exchange_finder.pool().remove(connection.id());
            availability.notify();
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
            availability.notify();
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
                    this.bytes_read += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                let error = WireError::from(error);
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
