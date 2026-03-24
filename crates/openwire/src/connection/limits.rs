use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_util::future::poll_fn;
use tokio::sync::{Notify, OwnedSemaphorePermit as TokioOwnedSemaphorePermit, Semaphore};

use openwire_core::WireError;

use super::Address;

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestAdmissionLimiter {
    inner: Option<Arc<RequestAdmissionLimiterInner>>,
}

#[derive(Debug)]
struct RequestAdmissionLimiterInner {
    global: Option<Arc<AsyncSemaphore>>,
    per_address: Option<AddressSemaphoreSet>,
}

#[derive(Debug)]
pub(crate) struct RequestAdmissionPermit {
    global: Option<RequestGlobalPermit>,
    per_address: Option<AddressSemaphorePermit>,
}

#[derive(Debug)]
struct RequestGlobalPermit {
    permit: Option<OwnedSemaphorePermit>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConnectionLimiter {
    inner: Option<Arc<ConnectionLimiterInner>>,
}

#[derive(Debug)]
struct ConnectionLimiterInner {
    global: Option<Arc<AsyncSemaphore>>,
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
    notify: Arc<Notify>,
}

#[derive(Clone, Debug)]
struct AddressSemaphoreSet {
    inner: Arc<AddressSemaphoreSetInner>,
}

#[derive(Debug)]
struct AddressSemaphoreSetInner {
    limit: usize,
    semaphores: Mutex<HashMap<Address, Arc<AsyncSemaphore>>>,
}

#[derive(Debug)]
struct AddressSemaphorePermit {
    key: Address,
    owner: AddressSemaphoreSet,
    semaphore: Arc<AsyncSemaphore>,
    permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug)]
struct AsyncSemaphore {
    limit: usize,
    semaphore: Arc<Semaphore>,
    waiters: Mutex<Vec<Waker>>,
}

#[derive(Debug)]
struct OwnedSemaphorePermit {
    semaphore: Arc<AsyncSemaphore>,
    permit: Option<TokioOwnedSemaphorePermit>,
}

impl RequestAdmissionLimiter {
    pub(crate) fn new(max_total: usize, max_per_address: usize) -> Self {
        let global = limit_semaphore(max_total);
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

        match (&inner.global, &inner.per_address) {
            (Some(global), Some(limiters)) => {
                let address_semaphore = limiters.semaphore_for(&address);
                let owner = limiters.clone();
                let key = address;
                poll_fn(move |cx| loop {
                    if global.poll_ready(cx).is_pending() {
                        return Poll::Pending;
                    }
                    if address_semaphore.poll_ready(cx).is_pending() {
                        return Poll::Pending;
                    }

                    let Some(global_permit) = global.try_acquire_owned() else {
                        continue;
                    };
                    let Some(per_address_permit) = address_semaphore.try_acquire_owned() else {
                        drop(global_permit);
                        continue;
                    };

                    return Poll::Ready(Ok(RequestAdmissionPermit {
                        global: Some(RequestGlobalPermit {
                            permit: Some(global_permit),
                        }),
                        per_address: Some(AddressSemaphorePermit {
                            key: key.clone(),
                            owner: owner.clone(),
                            semaphore: address_semaphore.clone(),
                            permit: Some(per_address_permit),
                        }),
                    }));
                })
                .await
            }
            (Some(global), None) => Ok(RequestAdmissionPermit {
                global: Some(RequestGlobalPermit {
                    permit: Some(global.acquire_owned().await),
                }),
                per_address: None,
            }),
            (None, Some(limiters)) => Ok(RequestAdmissionPermit {
                global: None,
                per_address: Some(limiters.acquire(address).await?),
            }),
            (None, None) => Ok(RequestAdmissionPermit {
                global: None,
                per_address: None,
            }),
        }
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), WireError>> {
        let Some(inner) = &self.inner else {
            return Poll::Ready(Ok(()));
        };
        let Some(global) = &inner.global else {
            return Poll::Ready(Ok(()));
        };

        global.poll_ready(cx).map(Ok)
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
            Some(semaphore) => match semaphore.try_acquire_owned() {
                Some(permit) => Some(permit),
                None => return None,
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

    pub(crate) fn can_acquire(&self, address: &Address) -> bool {
        let Some(inner) = &self.inner else {
            return true;
        };

        inner
            .global
            .as_ref()
            .map_or(true, |semaphore| semaphore.can_acquire())
            && inner
                .per_address
                .as_ref()
                .map_or(true, |limiters| limiters.can_acquire(address))
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
        let permit = semaphore.acquire_owned().await;
        Ok(AddressSemaphorePermit {
            key,
            owner: self.clone(),
            semaphore,
            permit: Some(permit),
        })
    }

    fn try_acquire(&self, key: Address) -> Option<AddressSemaphorePermit> {
        let semaphore = self.semaphore_for(&key);
        semaphore
            .try_acquire_owned()
            .map(|permit| AddressSemaphorePermit {
                key,
                owner: self.clone(),
                semaphore,
                permit: Some(permit),
            })
    }

    fn semaphore_for(&self, key: &Address) -> Arc<AsyncSemaphore> {
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

    fn can_acquire(&self, key: &Address) -> bool {
        self.inner
            .semaphores
            .lock()
            .expect("address semaphore lock")
            .get(key)
            .map_or(true, |semaphore| semaphore.can_acquire())
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

impl AsyncSemaphore {
    fn new(limit: usize) -> Self {
        let semaphore = Arc::new(Semaphore::new(limit));
        Self {
            limit,
            semaphore,
            waiters: Mutex::new(Vec::new()),
        }
    }

    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    fn can_acquire(&self) -> bool {
        self.available_permits() > 0
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.can_acquire() {
            return Poll::Ready(());
        }

        let mut waiters = self.waiters.lock().expect("semaphore waiters lock");
        if self.can_acquire() {
            return Poll::Ready(());
        }

        register_waker_locked(&mut waiters, cx.waker());
        if self.can_acquire() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn try_acquire_owned(self: &Arc<Self>) -> Option<OwnedSemaphorePermit> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| OwnedSemaphorePermit {
                semaphore: self.clone(),
                permit: Some(permit),
            })
    }

    async fn acquire_owned(self: &Arc<Self>) -> OwnedSemaphorePermit {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore should not close");
        OwnedSemaphorePermit {
            semaphore: self.clone(),
            permit: Some(permit),
        }
    }

    fn wake_waiters(&self) {
        let waiters = {
            let mut waiters = self.waiters.lock().expect("semaphore waiters lock");
            std::mem::take(&mut *waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            drop(permit);
            self.semaphore.wake_waiters();
        }
    }
}

fn limit_semaphore(limit: usize) -> Option<Arc<AsyncSemaphore>> {
    (limit != usize::MAX).then(|| Arc::new(AsyncSemaphore::new(limit)))
}

fn register_waker_locked(waiters: &mut Vec<Waker>, waker: &Waker) {
    if waiters.iter().any(|existing| existing.will_wake(waker)) {
        return;
    }
    waiters.push(waker.clone());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::future::poll_fn;
    use tokio::time::timeout;

    use super::{ConnectionAvailability, ConnectionLimiter, RequestAdmissionLimiter};
    use crate::connection::{Address, AuthorityKey, DnsPolicy, ProtocolPolicy, UriScheme};

    fn make_address(host: &str) -> Address {
        Address::new(
            UriScheme::Https,
            AuthorityKey::new(host, 443),
            None,
            Some(crate::connection::TlsIdentity::new(host)),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        )
    }

    #[tokio::test]
    async fn request_admission_waiter_completes_after_permit_drop() {
        let limiter = RequestAdmissionLimiter::new(1, 1);
        let first = limiter
            .acquire(make_address("example.com"))
            .await
            .expect("first permit");

        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                limiter
                    .acquire(make_address("example.com"))
                    .await
                    .expect("second permit")
            })
        };

        tokio::task::yield_now().await;
        drop(first);

        let second = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed")
            .expect("waiter join");
        drop(second);
    }

    #[tokio::test]
    async fn request_admission_multiple_waiters_complete_after_permit_drop() {
        let limiter = RequestAdmissionLimiter::new(1, 1);
        let first = limiter
            .acquire(make_address("example.com"))
            .await
            .expect("first permit");

        let waiters = (0..4)
            .map(|_| {
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    let permit = limiter
                        .acquire(make_address("example.com"))
                        .await
                        .expect("waiter permit");
                    tokio::task::yield_now().await;
                    drop(permit);
                })
            })
            .collect::<Vec<_>>();

        tokio::task::yield_now().await;
        drop(first);

        for waiter in waiters {
            timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter completed")
                .expect("waiter join");
        }
    }

    #[tokio::test]
    async fn request_admission_poll_ready_wakes_all_waiters() {
        let limiter = RequestAdmissionLimiter::new(1, usize::MAX);
        let permit = limiter
            .acquire(make_address("example.com"))
            .await
            .expect("held permit");

        let waiters = (0..8)
            .map(|_| {
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    poll_fn(|cx| limiter.poll_ready(cx))
                        .await
                        .expect("limiter ready");
                })
            })
            .collect::<Vec<_>>();

        tokio::task::yield_now().await;
        drop(permit);

        for waiter in waiters {
            timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter completed")
                .expect("waiter join");
        }
    }

    #[tokio::test]
    async fn connection_availability_broadcasts_to_all_waiters() {
        let availability = ConnectionAvailability::default();
        let waiters = (0..8)
            .map(|_| {
                let availability = availability.clone();
                tokio::spawn(async move {
                    availability.listen().await;
                })
            })
            .collect::<Vec<_>>();

        tokio::task::yield_now().await;
        availability.notify();

        for waiter in waiters {
            timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter completed")
                .expect("waiter join");
        }
    }

    #[tokio::test]
    async fn connection_availability_listen_observes_notify_before_first_poll() {
        let availability = ConnectionAvailability::default();
        let waiter = availability.listen();

        availability.notify();

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed");
    }

    #[tokio::test]
    async fn connection_permit_drop_notifies_availability_waiters() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(1, 1, availability.clone());
        let permit = limiter
            .try_acquire(make_address("example.com"))
            .expect("connection permit");

        let waiter = {
            let availability = availability.clone();
            tokio::spawn(async move {
                availability.listen().await;
            })
        };

        tokio::task::yield_now().await;
        drop(permit);

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed")
            .expect("waiter join");
    }

    #[test]
    fn connection_limiter_can_acquire_without_consuming_permits() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(1, 1, availability);
        let address = make_address("example.com");

        assert!(limiter.can_acquire(&address));

        let permit = limiter
            .try_acquire(address.clone())
            .expect("connection permit");
        assert!(!limiter.can_acquire(&address));

        drop(permit);
        assert!(limiter.can_acquire(&address));
    }
}
