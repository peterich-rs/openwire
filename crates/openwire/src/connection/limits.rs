use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use openwire_core::WireError;

use super::Address;

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestAdmissionLimiter {
    inner: Option<Arc<RequestAdmissionLimiterInner>>,
}

#[derive(Debug)]
struct RequestAdmissionLimiterInner {
    global: Option<Arc<RequestGlobalState>>,
    per_address: Option<AddressSemaphoreSet>,
}

#[derive(Debug)]
struct RequestGlobalState {
    semaphore: Arc<Semaphore>,
    ready_waker: AtomicWaker,
}

#[derive(Debug)]
pub(crate) struct RequestAdmissionPermit {
    global: Option<RequestGlobalPermit>,
    per_address: Option<AddressSemaphorePermit>,
}

#[derive(Debug)]
struct RequestGlobalPermit {
    state: Arc<RequestGlobalState>,
    permit: Option<OwnedSemaphorePermit>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConnectionLimiter {
    inner: Option<Arc<ConnectionLimiterInner>>,
}

#[derive(Debug)]
struct ConnectionLimiterInner {
    global: Option<Arc<Semaphore>>,
    per_address: Option<AddressSemaphoreSet>,
    availability: ConnectionAvailability,
}

#[derive(Clone, Debug)]
pub(crate) struct ConnectionPermit {
    inner: Arc<ConnectionPermitInner>,
}

#[derive(Debug)]
struct ConnectionPermitInner {
    global: Option<OwnedSemaphorePermit>,
    per_address: Option<AddressSemaphorePermit>,
    availability: ConnectionAvailability,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConnectionAvailability {
    notify: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Debug)]
struct AddressSemaphoreSet {
    inner: Arc<AddressSemaphoreSetInner>,
}

#[derive(Debug)]
struct AddressSemaphoreSetInner {
    limit: usize,
    semaphores: Mutex<HashMap<Address, Arc<Semaphore>>>,
}

#[derive(Debug)]
struct AddressSemaphorePermit {
    key: Address,
    owner: AddressSemaphoreSet,
    semaphore: Arc<Semaphore>,
    permit: Option<OwnedSemaphorePermit>,
}

impl RequestAdmissionLimiter {
    pub(crate) fn new(max_total: usize, max_per_address: usize) -> Self {
        let global = limit_semaphore(max_total).map(|semaphore| {
            Arc::new(RequestGlobalState {
                semaphore,
                ready_waker: AtomicWaker::new(),
            })
        });
        let per_address = AddressSemaphoreSet::new(max_per_address);
        if global.is_none() && per_address.is_none() {
            return Self::default();
        }

        Self {
            inner: Some(Arc::new(RequestAdmissionLimiterInner {
                global,
                per_address,
            })),
        }
    }

    pub(crate) async fn acquire(
        &self,
        address: Address,
    ) -> Result<RequestAdmissionPermit, WireError> {
        let Some(inner) = &self.inner else {
            return Ok(RequestAdmissionPermit {
                global: None,
                per_address: None,
            });
        };

        let global = match &inner.global {
            Some(state) => Some(RequestGlobalPermit {
                permit: Some(
                    state
                        .semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| limiter_closed("request global admission"))?,
                ),
                state: state.clone(),
            }),
            None => None,
        };

        let per_address = match &inner.per_address {
            Some(limiters) => Some(limiters.acquire(address).await?),
            None => None,
        };

        Ok(RequestAdmissionPermit {
            global,
            per_address,
        })
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), WireError>> {
        let Some(inner) = &self.inner else {
            return Poll::Ready(Ok(()));
        };
        let Some(global) = &inner.global else {
            return Poll::Ready(Ok(()));
        };

        if global.semaphore.available_permits() > 0 {
            return Poll::Ready(Ok(()));
        }

        global.ready_waker.register(cx.waker());
        if global.semaphore.available_permits() > 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl ConnectionLimiter {
    pub(crate) fn new(
        max_total: usize,
        max_per_address: usize,
        availability: ConnectionAvailability,
    ) -> Self {
        let global = limit_semaphore(max_total);
        let per_address = AddressSemaphoreSet::new(max_per_address);
        if global.is_none() && per_address.is_none() {
            return Self::default();
        }

        Self {
            inner: Some(Arc::new(ConnectionLimiterInner {
                global,
                per_address,
                availability,
            })),
        }
    }

    pub(crate) fn try_acquire(&self, address: Address) -> Option<ConnectionPermit> {
        let Some(inner) = &self.inner else {
            return Some(ConnectionPermit {
                inner: Arc::new(ConnectionPermitInner {
                    global: None,
                    per_address: None,
                    availability: ConnectionAvailability::default(),
                }),
            });
        };

        let global = match &inner.global {
            Some(semaphore) => match semaphore.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(TryAcquireError::NoPermits) => return None,
                Err(TryAcquireError::Closed) => return None,
            },
            None => None,
        };

        let per_address = match &inner.per_address {
            Some(limiters) => match limiters.try_acquire(address) {
                Some(permit) => Some(permit),
                None => return None,
            },
            None => None,
        };

        Some(ConnectionPermit {
            inner: Arc::new(ConnectionPermitInner {
                global,
                per_address,
                availability: inner.availability.clone(),
            }),
        })
    }
}

impl ConnectionAvailability {
    pub(crate) fn notify(&self) {
        self.notify.notify_waiters();
    }

    pub(crate) fn listen(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }
}

impl AddressSemaphoreSet {
    fn new(limit: usize) -> Option<Self> {
        limit_semaphore(limit).map(|_| Self {
            inner: Arc::new(AddressSemaphoreSetInner {
                limit,
                semaphores: Mutex::new(HashMap::new()),
            }),
        })
    }

    async fn acquire(&self, key: Address) -> Result<AddressSemaphorePermit, WireError> {
        let semaphore = self.semaphore_for(&key);
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| limiter_closed("address admission"))?;
        Ok(AddressSemaphorePermit {
            key,
            owner: self.clone(),
            semaphore,
            permit: Some(permit),
        })
    }

    fn try_acquire(&self, key: Address) -> Option<AddressSemaphorePermit> {
        let semaphore = self.semaphore_for(&key);
        match semaphore.clone().try_acquire_owned() {
            Ok(permit) => Some(AddressSemaphorePermit {
                key,
                owner: self.clone(),
                semaphore,
                permit: Some(permit),
            }),
            Err(TryAcquireError::NoPermits | TryAcquireError::Closed) => None,
        }
    }

    fn semaphore_for(&self, key: &Address) -> Arc<Semaphore> {
        let mut semaphores = self
            .inner
            .semaphores
            .lock()
            .expect("address semaphore lock");
        semaphores
            .entry(key.clone())
            .or_insert_with(|| {
                limit_semaphore(self.inner.limit)
                    .expect("address semaphore sets are only created with finite limits")
            })
            .clone()
    }
}

impl Drop for RequestGlobalPermit {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.state.ready_waker.wake();
    }
}

impl Drop for ConnectionPermitInner {
    fn drop(&mut self) {
        drop(self.per_address.take());
        drop(self.global.take());
        self.availability.notify();
    }
}

impl Drop for AddressSemaphorePermit {
    fn drop(&mut self) {
        drop(self.permit.take());

        if self.semaphore.available_permits() != self.owner.inner.limit
            || Arc::strong_count(&self.semaphore) != 2
        {
            return;
        }

        let mut semaphores = self
            .owner
            .inner
            .semaphores
            .lock()
            .expect("address semaphore lock");
        let remove_entry = semaphores
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.semaphore))
            && self.semaphore.available_permits() == self.owner.inner.limit
            && Arc::strong_count(&self.semaphore) == 2;
        if remove_entry {
            semaphores.remove(&self.key);
        }
    }
}

fn limit_semaphore(limit: usize) -> Option<Arc<Semaphore>> {
    (limit != usize::MAX).then(|| Arc::new(Semaphore::new(limit)))
}

fn limiter_closed(scope: &str) -> WireError {
    WireError::internal(
        format!("{scope} limiter was unexpectedly closed"),
        io::Error::other("limiter closed"),
    )
}
