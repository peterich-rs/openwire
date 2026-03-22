use std::task::{Context, Poll};

use http::{Request, Response};
use tower::layer::Layer;
use tower::util::BoxCloneSyncService;
use tower::Service;

use crate::{BoxFuture, CallContext, RequestBody, ResponseBody, WireError};

pub type WireResponse = Response<ResponseBody>;
pub type BoxWireService = BoxCloneSyncService<Exchange, WireResponse, WireError>;
pub type SharedInterceptor = std::sync::Arc<dyn Interceptor>;

#[derive(Debug)]
pub struct Exchange {
    request: Request<RequestBody>,
    context: CallContext,
    attempt: u32,
}

impl Exchange {
    pub fn new(request: Request<RequestBody>, context: CallContext, attempt: u32) -> Self {
        Self {
            request,
            context,
            attempt,
        }
    }

    pub fn request(&self) -> &Request<RequestBody> {
        &self.request
    }

    pub fn request_mut(&mut self) -> &mut Request<RequestBody> {
        &mut self.request
    }

    pub fn into_request(self) -> Request<RequestBody> {
        self.request
    }

    pub fn context(&self) -> &CallContext {
        &self.context
    }

    pub fn into_parts(self) -> (Request<RequestBody>, CallContext, u32) {
        (self.request, self.context, self.attempt)
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[derive(Clone)]
pub struct Next {
    inner: BoxWireService,
}

impl Next {
    pub fn new(inner: BoxWireService) -> Self {
        Self { inner }
    }

    pub fn run(self, exchange: Exchange) -> BoxFuture<Result<WireResponse, WireError>> {
        Box::pin(async move {
            let mut inner = self.inner;
            inner.call(exchange).await
        })
    }
}

pub trait Interceptor: Send + Sync + 'static {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<WireResponse, WireError>>;
}

#[derive(Clone)]
pub struct InterceptorLayer {
    interceptor: SharedInterceptor,
}

impl InterceptorLayer {
    pub fn new(interceptor: SharedInterceptor) -> Self {
        Self { interceptor }
    }
}

impl<S> Layer<S> for InterceptorLayer
where
    S: Service<Exchange, Response = WireResponse, Error = WireError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    type Service = InterceptorService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InterceptorService {
            inner,
            interceptor: self.interceptor.clone(),
        }
    }
}

#[derive(Clone)]
pub struct InterceptorService<S> {
    inner: S,
    interceptor: SharedInterceptor,
}

impl<S> Service<Exchange> for InterceptorService<S>
where
    S: Service<Exchange, Response = WireResponse, Error = WireError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    type Response = WireResponse;
    type Error = WireError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, exchange: Exchange) -> Self::Future {
        let replacement = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, replacement);
        let next = Next::new(BoxCloneSyncService::new(inner));
        self.interceptor.intercept(exchange, next)
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use http::Request;
    use tower::{Service, ServiceExt};

    use super::{Exchange, Interceptor, InterceptorService, Next, WireResponse};
    use crate::{
        BoxFuture, CallContext, NoopEventListenerFactory, RequestBody, ResponseBody, WireError,
    };

    #[derive(Clone)]
    struct PassthroughInterceptor;

    impl Interceptor for PassthroughInterceptor {
        fn intercept(
            &self,
            exchange: Exchange,
            next: Next,
        ) -> BoxFuture<Result<WireResponse, WireError>> {
            next.run(exchange)
        }
    }

    struct ReadinessTrackingService {
        was_polled: bool,
        is_clone: bool,
    }

    impl Clone for ReadinessTrackingService {
        fn clone(&self) -> Self {
            Self {
                was_polled: false,
                is_clone: true,
            }
        }
    }

    impl Service<Exchange> for ReadinessTrackingService {
        type Response = WireResponse;
        type Error = WireError;
        type Future = BoxFuture<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            assert!(
                !self.is_clone,
                "poll_ready should not be re-run against a cloned inner service",
            );
            self.was_polled = true;
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _exchange: Exchange) -> Self::Future {
            let was_polled = std::mem::take(&mut self.was_polled);
            Box::pin(async move {
                assert!(
                    was_polled,
                    "call must use the exact inner service instance that was polled ready",
                );
                Ok(http::Response::new(ResponseBody::empty()))
            })
        }
    }

    fn test_exchange() -> Exchange {
        let request = Request::builder()
            .uri("http://example.com/")
            .body(RequestBody::absent())
            .expect("request");
        let factory =
            std::sync::Arc::new(NoopEventListenerFactory) as crate::SharedEventListenerFactory;
        let ctx = CallContext::from_factory(&factory, &request, None);
        Exchange::new(request, ctx, 1)
    }

    #[tokio::test]
    async fn interceptor_service_preserves_ready_inner_service() {
        let mut service = InterceptorService {
            inner: ReadinessTrackingService {
                was_polled: false,
                is_clone: false,
            },
            interceptor: std::sync::Arc::new(PassthroughInterceptor),
        };

        let response = service
            .ready()
            .await
            .expect("service ready")
            .call(test_exchange())
            .await
            .expect("response");

        assert_eq!(response.status(), http::StatusCode::OK);
    }
}
