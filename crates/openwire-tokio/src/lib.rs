use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::rt::{Executor, Sleep, Timer};
use openwire_core::{
    next_connection_id, BoxConnection, BoxFuture, BoxTaskHandle, CallContext, Connected,
    Connection, ConnectionInfo, DnsResolver, TaskHandle, TcpConnector, WireError, WireExecutor,
};
use pin_project_lite::pin_project;
use tracing::instrument::WithSubscriber;

#[derive(Debug)]
struct TokioTaskHandle(tokio::task::JoinHandle<()>);

impl TaskHandle for TokioTaskHandle {
    fn abort(&self) {
        self.0.abort();
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

impl WireExecutor for TokioExecutor {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError> {
        Ok(Box::new(TokioTaskHandle(tokio::spawn(
            future.with_current_subscriber(),
        ))))
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

#[derive(Clone, Debug, Default)]
pub struct SystemDnsResolver;

impl DnsResolver for SystemDnsResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(addrs) => {
                    let addrs: Vec<_> = addrs.collect();
                    if addrs.is_empty() {
                        let error = WireError::dns(
                            "DNS resolution returned no socket addresses",
                            io::Error::new(io::ErrorKind::NotFound, "empty DNS result"),
                        );
                        ctx.listener().dns_failed(&ctx, &host, &error);
                        return Err(error);
                    }
                    ctx.listener().dns_end(&ctx, &host, &addrs);
                    Ok(addrs)
                }
                Err(error) => {
                    let error = WireError::dns("DNS resolution failed", error);
                    ctx.listener().dns_failed(&ctx, &host, &error);
                    Err(error)
                }
            }
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct TokioTcpConnector;

impl TcpConnector for TokioTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        Box::pin(async move {
            ctx.listener().connect_start(&ctx, addr);
            let connect = tokio::net::TcpStream::connect(addr);
            let stream = match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, connect).await {
                    Ok(result) => {
                        result.map_err(|error| WireError::tcp_connect("TCP connect failed", error))
                    }
                    Err(_error) => Err(WireError::connect_timeout(format!(
                        "connection timed out after {timeout:?}"
                    ))),
                },
                None => connect
                    .await
                    .map_err(|error| WireError::tcp_connect("TCP connect failed", error)),
            };
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    ctx.listener().connect_failed(&ctx, addr, &error);
                    return Err(error);
                }
            };

            stream
                .set_nodelay(true)
                .map_err(|error| WireError::tcp_connect("failed to configure TCP_NODELAY", error))
                .inspect_err(|error| {
                    ctx.listener().connect_failed(&ctx, addr, error);
                })?;

            let info = ConnectionInfo {
                id: next_connection_id(),
                remote_addr: stream.peer_addr().ok(),
                local_addr: stream.local_addr().ok(),
                tls: false,
            };

            ctx.mark_connection_established();
            ctx.listener().connect_end(&ctx, info.id, addr);

            Ok(Box::new(TcpConnection {
                inner: TokioIo::new(stream),
                info,
            }) as BoxConnection)
        })
    }
}

struct TcpConnection {
    inner: TokioIo<tokio::net::TcpStream>,
    info: ConnectionInfo,
}

impl Connection for TcpConnection {
    fn connected(&self) -> Connected {
        Connected::new().info(self.info.clone())
    }
}

impl hyper::rt::Read for TcpConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TcpConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
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
