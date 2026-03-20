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
            tower::ServiceExt::ready(&mut inner)
                .await
                .map_err(|error| WireError::interceptor("interceptor chain is not ready", error))?
                .call(exchange)
                .await
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
        let next = Next::new(BoxCloneSyncService::new(self.inner.clone()));
        self.interceptor.intercept(exchange, next)
    }
}
