use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::TryStreamExt;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use pin_project_lite::pin_project;

use crate::WireError;

pin_project! {
    #[derive(Debug)]
    pub struct RequestBody {
        #[pin]
        inner: RequestBodyInner,
        replayable_len: Option<u64>,
    }
}

impl RequestBody {
    pub fn empty() -> Self {
        Self {
            inner: RequestBodyInner::Empty,
            replayable_len: Some(0),
        }
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        Self {
            replayable_len: Some(bytes.len() as u64),
            inner: RequestBodyInner::Replayable {
                bytes,
                emitted: false,
            },
        }
    }

    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self::from_bytes(Bytes::from_static(bytes))
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
            replayable_len: None,
        }
    }

    pub fn try_clone(&self) -> Option<Self> {
        match &self.inner {
            RequestBodyInner::Empty => Some(Self::empty()),
            RequestBodyInner::Replayable { bytes, .. } => Some(Self::from_bytes(bytes.clone())),
            RequestBodyInner::Streaming { .. } => None,
        }
    }

    pub fn replayable_len(&self) -> Option<u64> {
        self.replayable_len
    }
}

impl Default for RequestBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = WireError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.project().inner.project() {
            RequestBodyInnerProj::Empty => Poll::Ready(None),
            RequestBodyInnerProj::Replayable { bytes, emitted } => {
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
            RequestBodyInnerProj::Streaming { inner } => inner.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.inner {
            RequestBodyInner::Empty => true,
            RequestBodyInner::Replayable { emitted, .. } => *emitted,
            RequestBodyInner::Streaming { inner } => inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.inner {
            RequestBodyInner::Empty => SizeHint::with_exact(0),
            RequestBodyInner::Replayable { bytes, .. } => SizeHint::with_exact(bytes.len() as u64),
            RequestBodyInner::Streaming { inner } => inner.size_hint(),
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

pin_project! {
    #[project = RequestBodyInnerProj]
    #[derive(Debug)]
    enum RequestBodyInner {
        Empty,
        Replayable {
            bytes: Bytes,
            emitted: bool,
        },
        Streaming {
            #[pin]
            inner: http_body_util::combinators::BoxBody<Bytes, WireError>,
        },
    }
}

pin_project! {
    #[derive(Debug)]
    pub struct ResponseBody {
        #[pin]
        inner: http_body_util::combinators::BoxBody<Bytes, WireError>,
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

    pub fn new(inner: http_body_util::combinators::BoxBody<Bytes, WireError>) -> Self {
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
        String::from_utf8(bytes.to_vec())
            .map_err(|error| WireError::body("response body is not valid UTF-8", error))
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
