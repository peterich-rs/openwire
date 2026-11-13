use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::TryStreamExt;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use pin_project_lite::pin_project;
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;
#[cfg(feature = "json")]
use serde::Serialize;

use crate::WireError;

const COMPOSITE_SKIP_BUDGET: usize = 16;
const EMPTY_FRAME_SKIP_BUDGET: usize = 16;

#[derive(Debug)]
pub struct RequestBody {
    inner: RequestBodyInner,
    content_length: Option<u64>,
    replayable: bool,
    presence: RequestBodyPresence,
    started: bool,
}

impl RequestBody {
    pub fn absent() -> Self {
        Self {
            inner: RequestBodyInner::Empty,
            content_length: Some(0),
            replayable: true,
            presence: RequestBodyPresence::Absent,
            started: false,
        }
    }

    pub fn explicit_empty() -> Self {
        Self {
            inner: RequestBodyInner::Empty,
            content_length: Some(0),
            replayable: true,
            presence: RequestBodyPresence::Present,
            started: false,
        }
    }

    pub fn empty() -> Self {
        Self::absent()
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        Self {
            content_length: Some(bytes.len() as u64),
            replayable: true,
            presence: RequestBodyPresence::Present,
            started: false,
            inner: RequestBodyInner::Replayable {
                bytes,
                emitted: false,
            },
        }
    }

    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self::from_bytes(Bytes::from_static(bytes))
    }

    #[cfg(feature = "json")]
    pub fn from_json<T>(value: &T) -> Result<Self, WireError>
    where
        T: Serialize,
    {
        serde_json::to_vec(value)
            .map(Bytes::from)
            .map(Self::from_bytes)
            .map_err(|error| WireError::body("failed to serialize request body as JSON", error))
    }

    pub fn from_stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static,
    {
        let stream = stream.map_ok(Frame::data).map_err(Into::into);
        Self {
            inner: RequestBodyInner::Streaming {
                inner: StreamBody::new(stream).boxed(),
            },
            content_length: None,
            replayable: false,
            presence: RequestBodyPresence::Present,
            started: false,
        }
    }

    /// DATA-only stream with a declared length. The body is not replayable.
    ///
    /// Reaching `length` bytes does not complete the body until the source
    /// yields EOF. An oversized chunk is rejected before it is forwarded.
    pub fn from_stream_with_length<S, E>(stream: S, length: u64) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Sync + 'static,
        E: Into<WireError> + 'static,
    {
        let stream = stream.map_ok(Frame::data).map_err(Into::into);
        Self {
            inner: RequestBodyInner::Strict {
                inner: StreamBody::new(stream).boxed(),
                remaining: length,
                eof_verified: false,
                errored: false,
            },
            content_length: Some(length),
            replayable: false,
            presence: RequestBodyPresence::Present,
            started: false,
        }
    }

    /// Complete length at construction, independent of replayability.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// DATA-only composition of fresh, unpolled children.
    ///
    /// Empty input yields a present explicit-empty body. Started children and
    /// length overflow are rejected before any polling.
    pub fn concat(bodies: Vec<RequestBody>) -> Result<Self, WireError> {
        if bodies.is_empty() {
            return Ok(Self::explicit_empty());
        }

        for body in &bodies {
            if body.started {
                return Err(WireError::invalid_request(
                    "cannot concatenate a request body that has already been polled",
                ));
            }
        }

        let mut total = Some(0u64);
        let mut replayable = true;
        for body in &bodies {
            replayable &= body.replayable;
            match (total, body.content_length) {
                (Some(acc), Some(len)) => {
                    total = Some(acc.checked_add(len).ok_or_else(|| {
                        WireError::invalid_request(
                            "concatenated request body length overflowed u64",
                        )
                    })?);
                }
                _ => total = None,
            }
        }

        Ok(Self {
            inner: RequestBodyInner::Composite {
                children: bodies,
                cursor: 0,
                errored: false,
            },
            content_length: total,
            replayable,
            presence: RequestBodyPresence::Present,
            started: false,
        })
    }

    pub fn try_clone(&self) -> Option<Self> {
        match &self.inner {
            RequestBodyInner::Empty => Some(match self.presence {
                RequestBodyPresence::Absent => Self::absent(),
                RequestBodyPresence::Present => Self::explicit_empty(),
            }),
            RequestBodyInner::Replayable { bytes, .. } => Some(Self::from_bytes(bytes.clone())),
            RequestBodyInner::Streaming { .. } | RequestBodyInner::Strict { .. } => None,
            RequestBodyInner::Composite { children, .. } => {
                let cloned = children
                    .iter()
                    .map(RequestBody::try_clone)
                    .collect::<Option<Vec<_>>>()?;
                Self::concat(cloned).ok()
            }
        }
    }

    pub fn replayable_len(&self) -> Option<u64> {
        if self.replayable {
            self.content_length
        } else {
            None
        }
    }

    pub fn is_absent(&self) -> bool {
        self.presence == RequestBodyPresence::Absent
    }

    /// Wraps an arbitrary body as a present, non-replayable request body.
    ///
    /// Used by transport observers that instrument an already-built `RequestBody`.
    /// DATA size hints are not treated as framing length because the inner body
    /// may still emit trailers.
    pub fn from_body<B>(body: B) -> Self
    where
        B: Body<Data = Bytes, Error = WireError> + Send + Sync + 'static,
    {
        Self {
            inner: RequestBodyInner::Streaming {
                inner: body.boxed(),
            },
            content_length: None,
            replayable: false,
            presence: RequestBodyPresence::Present,
            started: false,
        }
    }
}

impl Default for RequestBody {
    fn default() -> Self {
        Self::absent()
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        this.started = true;
        match &mut this.inner {
            RequestBodyInner::Empty => Poll::Ready(None),
            RequestBodyInner::Replayable { bytes, emitted } => {
                if *emitted {
                    return Poll::Ready(None);
                }

                *emitted = true;
                if bytes.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Frame::data(bytes.clone()))))
                }
            }
            RequestBodyInner::Streaming { inner } => Pin::new(inner).poll_frame(cx),
            RequestBodyInner::Strict {
                inner,
                remaining,
                eof_verified,
                errored,
                ..
            } => poll_strict(inner, remaining, eof_verified, errored, cx),
            RequestBodyInner::Composite {
                children,
                cursor,
                errored,
            } => poll_composite(children, cursor, errored, cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.inner {
            RequestBodyInner::Empty => true,
            RequestBodyInner::Replayable { bytes, emitted } => *emitted || bytes.is_empty(),
            RequestBodyInner::Streaming { inner } => inner.is_end_stream(),
            RequestBodyInner::Strict {
                eof_verified,
                errored,
                ..
            } => *errored || *eof_verified,
            RequestBodyInner::Composite {
                children,
                cursor,
                errored,
            } => {
                *errored
                    || children
                        .get(*cursor..)
                        .map(|rest| rest.iter().all(Body::is_end_stream))
                        .unwrap_or(true)
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.inner {
            RequestBodyInner::Empty => SizeHint::with_exact(0),
            RequestBodyInner::Replayable { bytes, emitted } => {
                if *emitted {
                    SizeHint::with_exact(0)
                } else {
                    SizeHint::with_exact(bytes.len() as u64)
                }
            }
            RequestBodyInner::Streaming { inner } => {
                let inner = inner.size_hint();
                if self.content_length.is_some() {
                    inner
                } else {
                    // DATA size alone cannot establish framing length: the inner
                    // body may still emit trailers. Keep the lower bound so
                    // consumers can estimate remaining DATA without letting an
                    // exact hint suppress trailing frames.
                    let mut hint = SizeHint::new();
                    hint.set_lower(inner.lower());
                    hint
                }
            }
            RequestBodyInner::Strict {
                remaining,
                eof_verified,
                errored,
                ..
            } => {
                if *errored || *eof_verified {
                    SizeHint::with_exact(0)
                } else {
                    SizeHint::with_exact(*remaining)
                }
            }
            RequestBodyInner::Composite {
                children,
                cursor,
                errored,
            } => {
                if *errored {
                    SizeHint::with_exact(0)
                } else {
                    composite_size_hint(children.get(*cursor..).unwrap_or(&[]))
                }
            }
        }
    }
}

impl From<Bytes> for RequestBody {
    fn from(value: Bytes) -> Self {
        Self::from_bytes(value)
    }
}

impl From<Vec<u8>> for RequestBody {
    fn from(value: Vec<u8>) -> Self {
        Self::from_bytes(Bytes::from(value))
    }
}

impl From<String> for RequestBody {
    fn from(value: String) -> Self {
        Self::from_bytes(Bytes::from(value))
    }
}

impl From<&'static [u8]> for RequestBody {
    fn from(value: &'static [u8]) -> Self {
        Self::from_static(value)
    }
}

impl From<&'static str> for RequestBody {
    fn from(value: &'static str) -> Self {
        Self::from_static(value.as_bytes())
    }
}

#[derive(Debug)]
enum RequestBodyInner {
    Empty,
    Replayable {
        bytes: Bytes,
        emitted: bool,
    },
    Streaming {
        inner: BoxBody<Bytes, WireError>,
    },
    Strict {
        inner: BoxBody<Bytes, WireError>,
        remaining: u64,
        eof_verified: bool,
        errored: bool,
    },
    Composite {
        children: Vec<RequestBody>,
        cursor: usize,
        errored: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestBodyPresence {
    Absent,
    Present,
}

fn poll_strict(
    inner: &mut BoxBody<Bytes, WireError>,
    remaining: &mut u64,
    eof_verified: &mut bool,
    errored: &mut bool,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<Frame<Bytes>, WireError>>> {
    if *errored || *eof_verified {
        return Poll::Ready(None);
    }

    let mut skips = 0usize;
    loop {
        match Pin::new(&mut *inner).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => {
                if *remaining > 0 {
                    *errored = true;
                    return Poll::Ready(Some(Err(body_length_error(
                        "request body ended before declared content length",
                    ))));
                }
                *eof_verified = true;
                return Poll::Ready(None);
            }
            Poll::Ready(Some(Err(error))) => {
                *errored = true;
                return Poll::Ready(Some(Err(error)));
            }
            Poll::Ready(Some(Ok(frame))) => {
                let Ok(data) = frame.into_data() else {
                    *errored = true;
                    return Poll::Ready(Some(Err(body_length_error(
                        "strict-length request body cannot include trailers",
                    ))));
                };
                if data.is_empty() {
                    skips += 1;
                    if skips >= EMPTY_FRAME_SKIP_BUDGET {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    continue;
                }
                let chunk_len = data.len() as u64;
                if chunk_len > *remaining {
                    *errored = true;
                    return Poll::Ready(Some(Err(body_length_error(
                        "request body exceeded declared content length",
                    ))));
                }
                *remaining -= chunk_len;
                return Poll::Ready(Some(Ok(Frame::data(data))));
            }
        }
    }
}

fn poll_composite(
    children: &mut [RequestBody],
    cursor: &mut usize,
    errored: &mut bool,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<Frame<Bytes>, WireError>>> {
    if *errored {
        return Poll::Ready(None);
    }

    let mut skips = 0usize;
    while *cursor < children.len() {
        match Pin::new(&mut children[*cursor]).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => {
                *cursor += 1;
                skips += 1;
                if skips >= COMPOSITE_SKIP_BUDGET && *cursor < children.len() {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            }
            Poll::Ready(Some(Err(error))) => {
                *errored = true;
                return Poll::Ready(Some(Err(error)));
            }
            Poll::Ready(Some(Ok(frame))) => {
                if !frame.is_data() {
                    *errored = true;
                    return Poll::Ready(Some(Err(WireError::body(
                        "concatenated request body cannot include trailers",
                        std::io::Error::other("non-DATA frame in concatenated body"),
                    ))));
                }
                let data = frame.into_data().expect("concatenated frame was DATA");
                if data.is_empty() {
                    skips += 1;
                    if skips >= COMPOSITE_SKIP_BUDGET {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    continue;
                }
                return Poll::Ready(Some(Ok(Frame::data(data))));
            }
        }
    }

    Poll::Ready(None)
}

fn composite_size_hint(children: &[RequestBody]) -> SizeHint {
    let mut exact = Some(0u64);
    let mut lower = 0u64;
    let mut upper = Some(0u64);
    for child in children {
        let hint = child.size_hint();
        lower = lower.saturating_add(hint.lower());
        match (upper, hint.upper()) {
            (Some(acc), Some(len)) => upper = acc.checked_add(len),
            _ => upper = None,
        }
        match (exact, hint.exact()) {
            (Some(acc), Some(len)) => exact = acc.checked_add(len),
            _ => exact = None,
        }
    }
    if let Some(exact) = exact {
        SizeHint::with_exact(exact)
    } else {
        let mut hint = SizeHint::new();
        hint.set_lower(lower);
        if let Some(upper) = upper {
            hint.set_upper(upper);
        }
        hint
    }
}

fn body_length_error(message: &'static str) -> WireError {
    WireError::body(message, std::io::Error::other(message))
}

pin_project! {
    #[derive(Debug)]
    pub struct ResponseBody {
        #[pin]
        inner: BoxBody<Bytes, WireError>,
    }
}

impl ResponseBody {
    pub fn empty() -> Self {
        Self::new(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed(),
        )
    }

    pub fn new(inner: BoxBody<Bytes, WireError>) -> Self {
        Self { inner }
    }

    pub fn from_incoming(body: hyper::body::Incoming) -> Self {
        Self::new(body.map_err(Into::into).boxed())
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        Self::new(Full::new(bytes).map_err(|never| match never {}).boxed())
    }

    pub async fn bytes(self) -> Result<Bytes, WireError> {
        let collected = self.inner.collect().await?;
        Ok(collected.to_bytes())
    }

    pub async fn text(self) -> Result<String, WireError> {
        let bytes = self.bytes().await?;
        // Prefer reclaiming the buffer when this is the unique owner of the
        // Bytes allocation (common after a full collect).
        String::from_utf8(bytes.into())
            .map_err(|error| WireError::body("response body is not valid UTF-8", error))
    }

    #[cfg(feature = "json")]
    pub async fn json<T>(self) -> Result<T, WireError>
    where
        T: DeserializeOwned,
    {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| WireError::body("response body is not valid JSON", error))
    }
}

impl Default for ResponseBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl From<Bytes> for ResponseBody {
    fn from(value: Bytes) -> Self {
        Self::from_bytes(value)
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, OnceLock};
    use std::task::{Context, Poll, Wake, Waker};

    use bytes::Bytes;
    use futures_util::{stream, Stream};
    use http::HeaderMap;
    use http_body::{Body, Frame, SizeHint};
    use http_body_util::BodyExt;

    use super::RequestBody;
    use crate::{WireError, WireErrorKind};

    #[cfg(feature = "json")]
    use super::ResponseBody;

    fn noop_cx() -> Context<'static> {
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        static WAKER: OnceLock<Waker> = OnceLock::new();
        let waker = WAKER.get_or_init(|| Waker::from(Arc::new(Noop)));
        Context::from_waker(waker)
    }

    async fn collect_data(body: RequestBody) -> Result<Bytes, WireError> {
        Ok(body.collect().await?.to_bytes())
    }

    struct ExactDataThenTrailers {
        data: Option<Bytes>,
        trailers: Option<HeaderMap>,
    }

    impl Body for ExactDataThenTrailers {
        type Data = Bytes;
        type Error = WireError;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            if let Some(data) = this.data.take() {
                if data.is_empty() {
                    // Fall through to trailers so zero DATA + trailers is expressible.
                } else {
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
            }
            if let Some(trailers) = this.trailers.take() {
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
            Poll::Ready(None)
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(
                self.data
                    .as_ref()
                    .map(|data| data.len() as u64)
                    .unwrap_or(0),
            )
        }
    }

    struct PendingThen<S> {
        pending: bool,
        inner: S,
    }

    impl<S: stream::Stream<Item = Result<Bytes, WireError>> + Unpin> stream::Stream for PendingThen<S> {
        type Item = Result<Bytes, WireError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.pending {
                this.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_next(cx)
        }
    }

    #[cfg(feature = "json")]
    #[tokio::test]
    async fn request_body_serializes_json_replayably() {
        let body =
            RequestBody::from_json(&serde_json::json!({ "hello": "openwire" })).expect("json body");
        let cloned = body.try_clone().expect("replayable clone");
        assert_eq!(
            body.collect().await.expect("body").to_bytes(),
            Bytes::from_static(br#"{"hello":"openwire"}"#)
        );
        assert_eq!(
            cloned.collect().await.expect("clone body").to_bytes(),
            Bytes::from_static(br#"{"hello":"openwire"}"#)
        );
    }

    #[cfg(feature = "json")]
    #[tokio::test]
    async fn response_body_deserializes_json() {
        let value: serde_json::Value =
            ResponseBody::from_bytes(Bytes::from_static(br#"{"ok":true}"#))
                .json()
                .await
                .expect("json");
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn replayable_bytes_report_remaining_size_after_emission() {
        let mut body = RequestBody::from_bytes(Bytes::from_static(b"hello"));
        assert_eq!(body.content_length(), Some(5));
        assert_eq!(body.replayable_len(), Some(5));
        assert_eq!(body.size_hint().exact(), Some(5));

        let mut cx = noop_cx();
        let frame = Pin::new(&mut body)
            .poll_frame(&mut cx)
            .ready()
            .expect("ready")
            .expect("data")
            .expect("ok");
        assert_eq!(
            frame.into_data().expect("data"),
            Bytes::from_static(b"hello")
        );
        assert_eq!(body.content_length(), Some(5));
        assert_eq!(body.replayable_len(), Some(5));
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(body.is_end_stream());
    }

    #[test]
    fn known_length_one_shot_is_not_replayable() {
        let body = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"abc"))]),
            3,
        );
        assert_eq!(body.content_length(), Some(3));
        assert_eq!(body.replayable_len(), None);
        assert!(body.try_clone().is_none());
        assert_eq!(body.size_hint().exact(), Some(3));
    }

    #[tokio::test]
    async fn concat_replays_after_partial_and_complete_consumption() {
        let mut body = RequestBody::concat(vec![
            RequestBody::from_bytes(Bytes::from_static(b"ab")),
            RequestBody::from_bytes(Bytes::from_static(b"cd")),
        ])
        .expect("concat");
        assert_eq!(body.content_length(), Some(4));
        assert_eq!(body.replayable_len(), Some(4));

        let mut cx = noop_cx();
        let first = Pin::new(&mut body)
            .poll_frame(&mut cx)
            .ready()
            .expect("ready")
            .expect("data")
            .expect("ok");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"ab"));

        let mid_clone = body.try_clone().expect("clone after partial");
        assert_eq!(
            collect_data(mid_clone).await.expect("mid"),
            Bytes::from_static(b"abcd")
        );

        let rest = collect_data(body).await.expect("rest");
        assert_eq!(rest, Bytes::from_static(b"cd"));

        let complete = RequestBody::concat(vec![
            RequestBody::from_bytes(Bytes::from_static(b"ab")),
            RequestBody::from_bytes(Bytes::from_static(b"cd")),
        ])
        .expect("concat");
        let after = complete.try_clone().expect("clone before");
        assert_eq!(
            collect_data(complete).await.expect("complete"),
            Bytes::from_static(b"abcd")
        );
        assert_eq!(
            collect_data(after).await.expect("after"),
            Bytes::from_static(b"abcd")
        );
    }

    #[tokio::test]
    async fn empty_concat_is_present_explicit_empty() {
        let body = RequestBody::concat(Vec::new()).expect("empty concat");
        assert!(!body.is_absent());
        assert_eq!(body.content_length(), Some(0));
        assert_eq!(body.replayable_len(), Some(0));
        assert!(body.is_end_stream());
        assert_eq!(collect_data(body).await.expect("bytes"), Bytes::new());
    }

    #[test]
    fn concat_rejects_length_overflow() {
        let error = RequestBody::concat(vec![
            RequestBody::from_stream_with_length(
                stream::empty::<Result<Bytes, WireError>>(),
                u64::MAX,
            ),
            RequestBody::from_stream_with_length(stream::empty::<Result<Bytes, WireError>>(), 1),
        ])
        .expect_err("overflow");
        assert_eq!(error.kind(), WireErrorKind::InvalidRequest);
        assert!(error.message().contains("overflowed"), "error = {error}");
    }

    #[tokio::test]
    async fn concat_rejects_trailers_and_does_not_emit_later_children() {
        let trailing = ExactDataThenTrailers {
            data: Some(Bytes::from_static(b"x")),
            trailers: Some(HeaderMap::new()),
        };
        let mut body = RequestBody::concat(vec![
            RequestBody::from_body(trailing),
            RequestBody::from_bytes(Bytes::from_static(b"later")),
        ])
        .expect("concat");
        assert_eq!(body.content_length(), None);

        let first = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("data")
            .expect("ok");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"x"));

        let error = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("error frame")
            .expect_err("trailers");
        assert_eq!(error.kind(), WireErrorKind::Body);

        assert!(std::pin::pin!(&mut body).frame().await.is_none());
    }

    #[tokio::test]
    async fn concat_terminal_error_then_none() {
        let mut body = RequestBody::concat(vec![
            RequestBody::from_stream(stream::iter(vec![Err(WireError::internal(
                "source read failed",
                std::io::Error::other("read"),
            ))])),
            RequestBody::from_bytes(Bytes::from_static(b"later")),
        ])
        .expect("concat");

        let error = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("error frame")
            .expect_err("read");
        assert_eq!(error.kind(), WireErrorKind::Internal);
        assert!(std::pin::pin!(&mut body).frame().await.is_none());
    }

    #[tokio::test]
    async fn concat_pending_then_data() {
        let pending = RequestBody::from_stream(PendingThen {
            pending: true,
            inner: stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"go"))]),
        });
        let body = RequestBody::concat(vec![
            pending,
            RequestBody::from_bytes(Bytes::from_static(b"!")),
        ])
        .expect("concat");
        assert_eq!(
            collect_data(body).await.expect("bytes"),
            Bytes::from_static(b"go!")
        );
    }

    #[tokio::test]
    async fn strict_length_rejects_early_eof() {
        let body = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"ab"))]),
            4,
        );
        let error = collect_data(body).await.expect_err("early eof");
        assert_eq!(error.kind(), WireErrorKind::Body);
        assert!(
            error.message().contains("before declared"),
            "error = {error}"
        );
    }

    #[tokio::test]
    async fn strict_length_rejects_oversized_chunk_before_forwarding() {
        let body = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"abcd"))]),
            2,
        );
        let error = collect_data(body).await.expect_err("too long");
        assert_eq!(error.kind(), WireErrorKind::Body);
        assert!(error.message().contains("exceeded"), "error = {error}");
    }

    #[tokio::test]
    async fn strict_length_rejects_declared_zero_with_data() {
        let body = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"x"))]),
            0,
        );
        let error = collect_data(body).await.expect_err("zero with data");
        assert_eq!(error.kind(), WireErrorKind::Body);
    }

    #[tokio::test]
    async fn strict_length_rejects_extra_chunk_after_declared_count() {
        let body = RequestBody::from_stream_with_length(
            stream::iter(vec![
                Ok::<Bytes, WireError>(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"c")),
            ]),
            2,
        );
        let error = collect_data(body).await.expect_err("extra chunk");
        assert_eq!(error.kind(), WireErrorKind::Body);
        assert!(error.message().contains("exceeded"), "error = {error}");
    }

    #[tokio::test]
    async fn strict_length_requires_eof_before_composite_suffix() {
        let payload = RequestBody::from_stream_with_length(
            stream::iter(vec![
                Ok::<Bytes, WireError>(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"c")),
            ]),
            2,
        );
        let mut body = RequestBody::concat(vec![
            payload,
            RequestBody::from_bytes(Bytes::from_static(b"SUFFIX")),
        ])
        .expect("concat");
        assert_eq!(body.content_length(), Some(8));
        assert_eq!(body.replayable_len(), None);

        let first = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("data")
            .expect("ok");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"ab"));
        let error = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("error frame")
            .expect_err("extra");
        assert_eq!(error.kind(), WireErrorKind::Body);
        assert!(std::pin::pin!(&mut body).frame().await.is_none());
    }

    #[tokio::test]
    async fn strict_length_valid_eof_then_suffix() {
        let payload = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"ab"))]),
            2,
        );
        let body = RequestBody::concat(vec![
            payload,
            RequestBody::from_bytes(Bytes::from_static(b"CD")),
        ])
        .expect("concat");
        assert_eq!(
            collect_data(body).await.expect("bytes"),
            Bytes::from_static(b"abCD")
        );
    }

    #[test]
    fn concat_rejects_started_child_but_accepts_fresh_clone() {
        let mut body = RequestBody::from_bytes(Bytes::from_static(b"abc"));
        let mut cx = noop_cx();
        let _ = Pin::new(&mut body).poll_frame(&mut cx);

        let error = RequestBody::concat(vec![body]).expect_err("started");
        assert_eq!(error.kind(), WireErrorKind::InvalidRequest);

        let mut original = RequestBody::from_bytes(Bytes::from_static(b"abc"));
        let _ = Pin::new(&mut original).poll_frame(&mut cx);
        let clone = original.try_clone().expect("clone");
        let concat = RequestBody::concat(vec![clone]).expect("fresh clone");
        assert_eq!(concat.content_length(), Some(3));
    }

    #[test]
    fn from_body_exact_hint_does_not_establish_content_length() {
        let body = RequestBody::from_body(ExactDataThenTrailers {
            data: Some(Bytes::from_static(b"xyz")),
            trailers: Some(HeaderMap::new()),
        });
        assert_eq!(body.size_hint().exact(), None);
        assert_eq!(body.size_hint().lower(), 3);
        assert_eq!(body.content_length(), None);
        assert_eq!(body.replayable_len(), None);
        assert!(body.try_clone().is_none());
    }

    #[tokio::test]
    async fn from_body_zero_data_still_forwards_trailers_outside_concat() {
        let mut body = RequestBody::from_body(ExactDataThenTrailers {
            data: Some(Bytes::new()),
            trailers: Some(HeaderMap::new()),
        });
        let frame = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("trailers")
            .expect("ok");
        assert!(frame.is_trailers());
    }

    #[test]
    fn composite_is_end_stream_after_final_replayable_data_without_extra_poll() {
        let mut body =
            RequestBody::concat(vec![RequestBody::from_bytes(Bytes::from_static(b"ab"))])
                .expect("concat");
        assert!(!body.is_end_stream());
        let mut cx = noop_cx();
        let _ = Pin::new(&mut body).poll_frame(&mut cx);
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert_eq!(body.content_length(), Some(2));
    }

    #[test]
    fn top_level_strict_length_waits_for_source_eof() {
        let mut body = RequestBody::from_stream_with_length(
            stream::iter(vec![Ok::<Bytes, WireError>(Bytes::from_static(b"ab"))]),
            2,
        );
        let mut cx = noop_cx();
        let _ = Pin::new(&mut body).poll_frame(&mut cx);
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(!body.is_end_stream());
        assert_eq!(body.content_length(), Some(2));
        assert!(matches!(
            Pin::new(&mut body).poll_frame(&mut cx),
            Poll::Ready(None)
        ));
        assert!(body.is_end_stream());
    }

    #[tokio::test]
    async fn concat_still_waits_for_strict_eof_before_suffix() {
        let payload = RequestBody::from_stream_with_length(
            stream::iter(vec![
                Ok::<Bytes, WireError>(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"c")),
            ]),
            2,
        );
        let mut body = RequestBody::concat(vec![
            payload,
            RequestBody::from_bytes(Bytes::from_static(b"SUFFIX")),
        ])
        .expect("concat");
        let first = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("data")
            .expect("ok");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"ab"));
        assert!(!body.is_end_stream());
        let error = std::pin::pin!(&mut body)
            .frame()
            .await
            .expect("error frame")
            .expect_err("extra");
        assert_eq!(error.kind(), WireErrorKind::Body);
    }

    struct EmptyForever;

    impl Stream for EmptyForever {
        type Item = Result<Bytes, WireError>;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(Some(Ok(Bytes::new())))
        }
    }

    #[test]
    fn strict_length_empty_frames_yield_pending_instead_of_spinning() {
        let mut body = RequestBody::from_stream_with_length(EmptyForever, 4);
        let mut cx = noop_cx();
        let mut polls = 0usize;
        for _ in 0..8 {
            polls += 1;
            match Pin::new(&mut body).poll_frame(&mut cx) {
                Poll::Pending => break,
                Poll::Ready(_) => {}
            }
        }
        assert!(polls < 8, "empty frames must not monopolize the executor");
    }

    trait PollExt<T> {
        fn ready(self) -> Option<T>;
    }

    impl<T> PollExt<T> for Poll<T> {
        fn ready(self) -> Option<T> {
            match self {
                Poll::Ready(value) => Some(value),
                Poll::Pending => None,
            }
        }
    }
}
