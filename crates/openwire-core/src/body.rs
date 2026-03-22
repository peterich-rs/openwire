use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::TryStreamExt;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use pin_project_lite::pin_project;
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;
#[cfg(feature = "json")]
use serde::Serialize;

use crate::WireError;

pin_project! {
    #[derive(Debug)]
    pub struct RequestBody {
        #[pin]
        inner: RequestBodyInner,
        replayable_len: Option<u64>,
        presence: RequestBodyPresence,
    }
}

impl RequestBody {
    pub fn absent() -> Self {
        Self {
            inner: RequestBodyInner::Empty,
            replayable_len: Some(0),
            presence: RequestBodyPresence::Absent,
        }
    }

    pub fn explicit_empty() -> Self {
        Self {
            inner: RequestBodyInner::Empty,
            replayable_len: Some(0),
            presence: RequestBodyPresence::Present,
        }
    }

    pub fn empty() -> Self {
        Self::absent()
    }

    pub fn from_bytes(bytes: Bytes) -> Self {
        Self {
            replayable_len: Some(bytes.len() as u64),
            presence: RequestBodyPresence::Present,
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
            replayable_len: None,
            presence: RequestBodyPresence::Present,
        }
    }

    pub fn try_clone(&self) -> Option<Self> {
        match &self.inner {
            RequestBodyInner::Empty => Some(match self.presence {
                RequestBodyPresence::Absent => Self::absent(),
                RequestBodyPresence::Present => Self::explicit_empty(),
            }),
            RequestBodyInner::Replayable { bytes, .. } => Some(Self::from_bytes(bytes.clone())),
            RequestBodyInner::Streaming { .. } => None,
        }
    }

    pub fn replayable_len(&self) -> Option<u64> {
        self.replayable_len
    }

    pub fn is_absent(&self) -> bool {
        self.presence == RequestBodyPresence::Absent
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestBodyPresence {
    Absent,
    Present,
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
    #[cfg(feature = "json")]
    use bytes::Bytes;
    #[cfg(feature = "json")]
    use http_body_util::BodyExt;

    #[cfg(feature = "json")]
    use super::{RequestBody, ResponseBody};

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
}
