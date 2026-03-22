use std::time::Duration;

use crate::{BoxFuture, WireError};

pub trait TaskHandle: Send + Sync + 'static {
    fn abort(&self);
}

pub type BoxTaskHandle = Box<dyn TaskHandle>;

pub trait Runtime: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>) -> Result<BoxTaskHandle, WireError>;
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}
