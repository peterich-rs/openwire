use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::rt::{Executor, Sleep, Timer};
use pin_project_lite::pin_project;
use tracing::instrument::WithSubscriber;

use crate::{BoxFuture, BoxTaskHandle, Runtime, TaskHandle, WireError};

#[derive(Clone, Debug, Default)]
pub struct TokioRuntime;

#[derive(Debug)]
struct TokioTaskHandle(tokio::task::JoinHandle<()>);

impl TaskHandle for TokioTaskHandle {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Runtime for TokioRuntime {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        Ok(Box::new(TokioTaskHandle(tokio::spawn(
            future.with_current_subscriber(),
        ))))
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct TokioExecutor;

impl TokioExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl<Fut> Executor<Fut> for TokioExecutor
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, future: Fut) {
        tokio::spawn(future.with_current_subscriber());
    }
}

pin_project! {
    #[derive(Debug)]
    pub struct TokioIo<T> {
        #[pin]
        inner: T,
    }
}

impl<T> TokioIo<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> hyper::rt::Read for TokioIo<T>
where
    T: tokio::io::AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let filled = unsafe {
            let mut read_buf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match tokio::io::AsyncRead::poll_read(self.project().inner, cx, &mut read_buf) {
                Poll::Ready(Ok(())) => read_buf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            buf.advance(filled);
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> hyper::rt::Write for TokioIo<T>
where
    T: tokio::io::AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_flush(self.project().inner, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        tokio::io::AsyncWrite::poll_shutdown(self.project().inner, cx)
    }

    fn is_write_vectored(&self) -> bool {
        tokio::io::AsyncWrite::is_write_vectored(&self.inner)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        tokio::io::AsyncWrite::poll_write_vectored(self.project().inner, cx, bufs)
    }
}

impl<T> tokio::io::AsyncRead for TokioIo<T>
where
    T: hyper::rt::Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read_buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let filled = read_buf.filled().len();
        let newly_filled = unsafe {
            let mut hyper_buf = hyper::rt::ReadBuf::uninit(read_buf.unfilled_mut());
            match hyper::rt::Read::poll_read(self.project().inner, cx, hyper_buf.unfilled()) {
                Poll::Ready(Ok(())) => hyper_buf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            read_buf.assume_init(newly_filled);
            read_buf.set_filled(filled + newly_filled);
        }

        Poll::Ready(Ok(()))
    }
}

impl<T> tokio::io::AsyncWrite for TokioIo<T>
where
    T: hyper::rt::Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_flush(self.project().inner, cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        hyper::rt::Write::poll_shutdown(self.project().inner, cx)
    }

    fn is_write_vectored(&self) -> bool {
        hyper::rt::Write::is_write_vectored(&self.inner)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        hyper::rt::Write::poll_write_vectored(self.project().inner, cx, bufs)
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct TokioTimer;

impl TokioTimer {
    pub fn new() -> Self {
        Self
    }
}

pin_project! {
    #[derive(Debug)]
    struct TokioSleep {
        #[pin]
        inner: tokio::time::Sleep,
    }
}

impl Timer for TokioTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        Box::pin(TokioSleep {
            inner: tokio::time::sleep(duration),
        })
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        Box::pin(TokioSleep {
            inner: tokio::time::sleep_until(deadline.into()),
        })
    }

    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
        if let Some(tokio_sleep) = sleep.as_mut().downcast_mut_pin::<TokioSleep>() {
            tokio_sleep.reset(new_deadline);
        }
    }

    fn now(&self) -> Instant {
        tokio::time::Instant::now().into()
    }
}

impl Future for TokioSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl Sleep for TokioSleep {}

impl TokioSleep {
    fn reset(self: Pin<&mut Self>, deadline: Instant) {
        self.project().inner.as_mut().reset(deadline.into());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hyper::rt::Executor;
    use hyper::rt::Timer;
    use tokio::sync::oneshot;

    use super::{TokioExecutor, TokioTimer};

    #[tokio::test]
    async fn tokio_executor_spawns_background_future() {
        let (tx, rx) = oneshot::channel();
        TokioExecutor::new().execute(async move {
            let _ = tx.send(());
        });
        rx.await.expect("executor future should complete");
    }

    #[tokio::test]
    async fn tokio_timer_reset_moves_sleep_deadline() {
        let timer = TokioTimer::new();
        let mut sleep = hyper::rt::Timer::sleep(&timer, Duration::from_secs(5));
        timer.reset(&mut sleep, timer.now() + Duration::from_millis(1));
        sleep.await;
    }
}
