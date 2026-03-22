use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hyper::rt::{Sleep, Timer};

use crate::{BoxFuture, WireError};

pub trait TaskHandle: Send + Sync + 'static {
    fn abort(&self);
}

pub type BoxTaskHandle = Box<dyn TaskHandle>;

pub trait WireExecutor: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError>;
}

#[derive(Clone)]
pub struct HyperExecutor(pub Arc<dyn WireExecutor>);

impl<Fut> hyper::rt::Executor<Fut> for HyperExecutor
where
    Fut: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: Fut) {
        if let Err(error) = self.0.spawn(Box::pin(future)) {
            tracing::debug!(error = %error, "wire executor failed to spawn hyper future");
        }
    }
}

#[derive(Clone)]
pub struct SharedTimer(pub Arc<dyn Timer + Send + Sync>);

impl SharedTimer {
    pub fn new<T>(timer: T) -> Self
    where
        T: Timer + Send + Sync + 'static,
    {
        Self(Arc::new(timer))
    }
}

impl Timer for SharedTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        self.0.sleep(duration)
    }

    fn sleep_until(&self, deadline: std::time::Instant) -> Pin<Box<dyn Sleep>> {
        self.0.sleep_until(deadline)
    }

    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: std::time::Instant) {
        self.0.reset(sleep, new_deadline);
    }

    fn now(&self) -> std::time::Instant {
        self.0.now()
    }
}

pub trait Runtime: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError>;
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}
