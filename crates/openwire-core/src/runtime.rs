use std::time::Duration;

use crate::BoxFuture;

pub trait Runtime: Send + Sync + 'static {
    fn spawn(&self, future: BoxFuture<()>) -> Result<(), crate::WireError>;
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}
