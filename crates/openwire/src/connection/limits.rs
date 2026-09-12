use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio::sync::futures::OwnedNotified;
use tokio::sync::{Notify, OwnedSemaphorePermit as TokioOwnedSemaphorePermit, Semaphore};

use openwire_core::WireError;
use parking_lot::Mutex;

use super::scheduler::{RequestPriority, RequestScheduler};
use super::{
    address_shard, sip_hash_map, verified_server_name_matches, Address, ProtocolPolicy, SipHashMap,
    UriScheme, ADDRESS_SHARDS,
};

pub(crate) use super::scheduler::RequestAdmissionPermit;

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestAdmissionLimiter {
    scheduler: Option<Arc<RequestScheduler>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ConnectionLimiter {
    inner: Arc<ConnectionLimiterInner>,
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
    address: Address,
    global: Option<OwnedSemaphorePermit>,
    per_address: Option<AddressSemaphorePermit>,
    availability: ConnectionAvailability,
}

#[derive(Clone, Debug)]
pub(crate) struct ConnectionAvailability {
    shards: Arc<[AddressNotifyShard]>,
    /// Wakes waiters blocked on the global connection cap when any address
    /// releases a connection permit.
    global: Arc<Notify>,
}

#[derive(Debug, Default)]
struct AddressNotifyShard {
    by_address: Mutex<SipHashMap<Address, Arc<Notify>>>,
}

#[derive(Clone, Debug)]
struct AddressSemaphoreSet {
    inner: Arc<AddressSemaphoreSetInner>,
}

type AddressSemaphoreShards = Arc<[Mutex<SipHashMap<Address, Arc<AsyncSemaphore>>>]>;

#[derive(Debug)]
struct AddressSemaphoreSetInner {
    limit: usize,
    shards: AddressSemaphoreShards,
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
}

#[derive(Debug)]
struct OwnedSemaphorePermit {
    permit: Option<TokioOwnedSemaphorePermit>,
}

impl RequestAdmissionLimiter {
    pub(crate) fn new(max_total: usize, max_per_address: usize, max_queued: usize) -> Self {
        if max_total == usize::MAX && max_per_address == usize::MAX && max_queued == usize::MAX {
            return Self::default();
        }

        Self {
            scheduler: Some(RequestScheduler::new(
                max_total,
                max_per_address,
                max_queued,
            )),
        }
    }

    pub(crate) async fn acquire(
        &self,
        address: Address,
        priority: RequestPriority,
    ) -> Result<RequestAdmissionPermit, WireError> {
        let Some(scheduler) = &self.scheduler else {
            return Ok(RequestAdmissionPermit::unlimited());
        };
        scheduler.acquire(address, priority).await
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), WireError>> {
        let Some(scheduler) = &self.scheduler else {
            return Poll::Ready(Ok(()));
        };
        scheduler.poll_ready(cx).map(Ok)
    }
}

impl ConnectionLimiter {
    pub(crate) fn new(
        max_total: usize,
        max_per_address: usize,
        availability: ConnectionAvailability,
    ) -> Self {
        Self {
            inner: Arc::new(ConnectionLimiterInner {
                global: limit_semaphore(max_total),
                per_address: AddressSemaphoreSet::new(max_per_address),
                availability,
            }),
        }
    }

    pub(crate) fn try_acquire(&self, address: Address) -> Option<ConnectionPermit> {
        // Take the per-address slot first so a host at its cap never consumes
        // (and then silently drops) a global permit.
        let per_address = match &self.inner.per_address {
            Some(limiters) => Some(limiters.try_acquire(address.clone())?),
            None => None,
        };

        let global = match &self.inner.global {
            Some(semaphore) => Some(semaphore.try_acquire_owned()?),
            None => None,
        };

        Some(ConnectionPermit {
            inner: Arc::new(ConnectionPermitInner {
                address,
                global,
                per_address,
                availability: self.inner.availability.clone(),
            }),
        })
    }

    /// Non-consuming heuristic; result may be stale by the time the caller acts on it.
    pub(crate) fn can_acquire(&self, address: &Address) -> bool {
        self.inner
            .global
            .as_ref()
            .map_or(true, |semaphore| semaphore.can_acquire())
            && self
                .inner
                .per_address
                .as_ref()
                .map_or(true, |limiters| limiters.can_acquire(address))
    }
}

impl Default for ConnectionLimiter {
    fn default() -> Self {
        Self::new(usize::MAX, usize::MAX, ConnectionAvailability::default())
    }
}

impl ConnectionAvailability {
    fn shard(&self, address: &Address) -> &AddressNotifyShard {
        &self.shards[address_shard(address)]
    }

    fn notify_for(&self, address: &Address) -> Arc<Notify> {
        let mut map = self.shard(address).by_address.lock();
        if let Some(existing) = map.get(address) {
            return existing.clone();
        }
        let notify = Arc::new(Notify::new());
        map.insert(address.clone(), notify.clone());
        notify
    }

    fn existing_notify(&self, address: &Address) -> Option<Arc<Notify>> {
        self.shard(address).by_address.lock().get(address).cloned()
    }

    pub(crate) fn notify(&self, address: &Address) {
        if let Some(notify) = self.existing_notify(address) {
            notify.notify_one();
        }
    }

    /// Wake same-host stream waiters and any currently listening authority that
    /// can coalesce onto this HTTP/2 connection. Does not insert map entries:
    /// only live `listen` waiters are visible.
    pub(crate) fn notify_http2(&self, address: &Address, verified_server_names: &[String]) {
        self.notify(address);
        if verified_server_names.is_empty() {
            return;
        }

        let mut waiters = Vec::new();
        for shard in self.shards.iter() {
            let map = shard.by_address.lock();
            for (waiting, notify) in map.iter() {
                if waiting == address {
                    continue;
                }
                if coalescing_wait_eligible(address, waiting, verified_server_names) {
                    waiters.push(Arc::clone(notify));
                }
            }
        }
        for notify in waiters {
            notify.notify_one();
        }
    }

    pub(crate) fn notify_global(&self) {
        // Broadcast: a freed total-cap slot may be usable by any host that is
        // under its per-address cap. `notify_one` would hand the token to an
        // arbitrary waiter, including one still blocked on per-host.
        self.global.notify_waiters();
    }

    /// Subscribe before the caller probes capacity. `Notified` snapshots the
    /// `notify_waiters` generation at construction; creating it only when the
    /// future is first polled drops a global wake that arrives in between.
    /// Wait after pool/binding mutexes are released. Do not hold those mutexes
    /// across the wait. Per-address `notify_one` stores a permit if no waiter
    /// has registered yet. The global channel is `notify_waiters` so a
    /// connection closing on another host can free the total cap without
    /// waking only the wrong host.
    pub(crate) fn listen(&self, address: &Address) -> impl Future<Output = ()> {
        let notify = self.notify_for(address);
        ConnectionWait {
            local: notify.clone().notified_owned(),
            global: Arc::clone(&self.global).notified_owned(),
            _reclaim: NotifyReclaim {
                address: address.clone(),
                notify,
                availability: self.clone(),
            },
        }
    }

    #[cfg(test)]
    fn notify_entry_count(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.by_address.lock().len())
            .sum()
    }
}

pin_project! {
    /// `OwnedNotified` fields drop before `_reclaim` so the map can observe
    /// `strong_count == 2` (reclaim + map) and remove an unused entry.
    struct ConnectionWait {
        #[pin]
        local: OwnedNotified,
        #[pin]
        global: OwnedNotified,
        _reclaim: NotifyReclaim,
    }
}

impl Future for ConnectionWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();
        if this.local.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        this.global.poll(cx)
    }
}

/// Drops the per-address `Notify` once no `listen` future holds it. Connection
/// releases must not retain an entry for every historical destination.
struct NotifyReclaim {
    address: Address,
    notify: Arc<Notify>,
    availability: ConnectionAvailability,
}

impl Drop for NotifyReclaim {
    fn drop(&mut self) {
        if Arc::strong_count(&self.notify) != 2 {
            return;
        }

        let mut map = self.availability.shard(&self.address).by_address.lock();
        let remove_entry = map
            .get(&self.address)
            .is_some_and(|current| Arc::ptr_eq(current, &self.notify))
            && Arc::strong_count(&self.notify) == 2;
        if remove_entry {
            map.remove(&self.address);
        }
    }
}

fn coalescing_wait_eligible(
    origin: &Address,
    waiting: &Address,
    verified_server_names: &[String],
) -> bool {
    origin.scheme() == UriScheme::Https
        && waiting.scheme() == UriScheme::Https
        && origin.proxy().is_none()
        && waiting.proxy().is_none()
        && origin.authority().port() == waiting.authority().port()
        && !matches!(waiting.protocol_policy(), ProtocolPolicy::Http1Only)
        && verified_server_names
            .iter()
            .any(|name| verified_server_name_matches(name, waiting.authority().host()))
}

impl Default for ConnectionAvailability {
    fn default() -> Self {
        let shards = (0..ADDRESS_SHARDS)
            .map(|_| AddressNotifyShard::default())
            .collect::<Vec<_>>();
        Self {
            shards: Arc::<[AddressNotifyShard]>::from(shards),
            global: Arc::new(Notify::new()),
        }
    }
}

impl AddressSemaphoreSet {
    fn new(limit: usize) -> Option<Self> {
        (limit != usize::MAX).then(|| {
            let shards = (0..ADDRESS_SHARDS)
                .map(|_| Mutex::new(sip_hash_map()))
                .collect::<Vec<_>>();
            Self {
                inner: Arc::new(AddressSemaphoreSetInner {
                    limit,
                    shards: AddressSemaphoreShards::from(shards),
                }),
            }
        })
    }

    fn shard(&self, key: &Address) -> &Mutex<SipHashMap<Address, Arc<AsyncSemaphore>>> {
        &self.inner.shards[address_shard(key)]
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
        let mut semaphores = self.shard(key).lock();
        if let Some(existing) = semaphores.get(key) {
            return existing.clone();
        }
        let semaphore = limit_semaphore(self.inner.limit).unwrap_or_else(|| {
            debug_assert!(
                false,
                "address semaphore sets are only created with finite limits"
            );
            Arc::new(AsyncSemaphore::new(self.inner.limit))
        });
        semaphores.insert(key.clone(), semaphore.clone());
        semaphore
    }

    fn can_acquire(&self, key: &Address) -> bool {
        self.shard(key)
            .lock()
            .get(key)
            .map_or(true, |semaphore| semaphore.can_acquire())
    }
}

impl Drop for ConnectionPermitInner {
    fn drop(&mut self) {
        drop(self.per_address.take());
        drop(self.global.take());
        self.availability.notify(&self.address);
        self.availability.notify_global();
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

        let mut semaphores = self.owner.shard(&self.key).lock();
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
        Self {
            limit,
            semaphore: Arc::new(Semaphore::new(limit)),
        }
    }

    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    fn can_acquire(&self) -> bool {
        self.available_permits() > 0
    }

    fn try_acquire_owned(self: &Arc<Self>) -> Option<OwnedSemaphorePermit> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| OwnedSemaphorePermit {
                permit: Some(permit),
            })
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        drop(self.permit.take());
    }
}

fn limit_semaphore(limit: usize) -> Option<Arc<AsyncSemaphore>> {
    (limit != usize::MAX).then(|| Arc::new(AsyncSemaphore::new(limit)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::future::poll_fn;
    use tokio::time::timeout;

    use super::{ConnectionAvailability, ConnectionLimiter, RequestAdmissionLimiter};
    use crate::connection::scheduler::RequestPriority;
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
        let limiter = RequestAdmissionLimiter::new(1, 1, usize::MAX);
        let first = limiter
            .acquire(make_address("example.com"), RequestPriority::Normal)
            .await
            .expect("first permit");

        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                limiter
                    .acquire(make_address("example.com"), RequestPriority::Normal)
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
        let limiter = RequestAdmissionLimiter::new(1, 1, usize::MAX);
        let first = limiter
            .acquire(make_address("example.com"), RequestPriority::Normal)
            .await
            .expect("first permit");

        let waiters = (0..4)
            .map(|_| {
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    let permit = limiter
                        .acquire(make_address("example.com"), RequestPriority::Normal)
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
    async fn request_admission_waiters_are_admitted_in_fifo_order() {
        let limiter = RequestAdmissionLimiter::new(1, 1, usize::MAX);
        let first = limiter
            .acquire(make_address("example.com"), RequestPriority::Normal)
            .await
            .expect("held permit");

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for index in 0..3 {
            let limiter = limiter.clone();
            let order = order.clone();
            waiters.push(tokio::spawn(async move {
                let permit = limiter
                    .acquire(make_address("example.com"), RequestPriority::Normal)
                    .await
                    .expect("waiter permit");
                order.lock().expect("order").push(index);
                permit
            }));
            tokio::task::yield_now().await;
        }

        drop(first);
        let first_waiter = timeout(Duration::from_secs(1), waiters.remove(0))
            .await
            .expect("first waiter completed")
            .expect("first waiter join");
        assert_eq!(&*order.lock().expect("order"), &[0]);

        drop(first_waiter);
        let second_waiter = timeout(Duration::from_secs(1), waiters.remove(0))
            .await
            .expect("second waiter completed")
            .expect("second waiter join");
        assert_eq!(&*order.lock().expect("order"), &[0, 1]);

        drop(second_waiter);
        let third_waiter = timeout(Duration::from_secs(1), waiters.remove(0))
            .await
            .expect("third waiter completed")
            .expect("third waiter join");
        assert_eq!(&*order.lock().expect("order"), &[0, 1, 2]);
        drop(third_waiter);
    }

    #[tokio::test]
    async fn request_admission_poll_ready_wakes_next_waiter() {
        let limiter = RequestAdmissionLimiter::new(1, usize::MAX, usize::MAX);
        let permit = limiter
            .acquire(make_address("example.com"), RequestPriority::Normal)
            .await
            .expect("held permit");

        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                poll_fn(|cx| limiter.poll_ready(cx))
                    .await
                    .expect("limiter ready");
            })
        };

        tokio::task::yield_now().await;
        drop(permit);

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed")
            .expect("waiter join");
    }

    #[tokio::test]
    async fn connection_availability_wakes_one_waiter_per_notify() {
        let availability = ConnectionAvailability::default();
        let address = make_address("example.com");
        let waiters = (0..3)
            .map(|_| {
                let availability = availability.clone();
                let address = address.clone();
                tokio::spawn(async move {
                    availability.listen(&address).await;
                })
            })
            .collect::<Vec<_>>();

        tokio::task::yield_now().await;
        availability.notify(&address);
        availability.notify(&address);
        availability.notify(&address);

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
        let address = make_address("example.com");
        let waiter = availability.listen(&address);

        availability.notify(&address);

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed");
    }

    #[tokio::test]
    async fn connection_waiter_for_host_a_does_not_complete_when_host_b_releases() {
        let availability = ConnectionAvailability::default();
        let host_a = make_address("a.example");
        let host_b = make_address("b.example");
        let waiter = {
            let availability = availability.clone();
            let host_a = host_a.clone();
            tokio::spawn(async move {
                availability.listen(&host_a).await;
            })
        };

        tokio::task::yield_now().await;
        availability.notify(&host_b);

        timeout(Duration::from_millis(50), waiter)
            .await
            .expect_err("host A waiter should ignore host B notify");
    }

    #[tokio::test]
    async fn global_connection_cap_wakes_a_host_that_can_use_the_slot() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(2, 1, availability.clone());
        let host_a = make_address("a.example");
        let host_b = make_address("b.example");
        let host_c = make_address("c.example");
        let permit_a = limiter.try_acquire(host_a.clone()).expect("host A permit");
        let permit_c = limiter.try_acquire(host_c.clone()).expect("host C permit");
        assert!(
            limiter.try_acquire(host_a.clone()).is_none(),
            "host A is at its per-host cap"
        );
        assert!(
            limiter.try_acquire(host_b.clone()).is_none(),
            "global cap is exhausted"
        );

        let waiter_b = {
            let availability = availability.clone();
            let host_b = host_b.clone();
            tokio::spawn(async move {
                availability.listen(&host_b).await;
            })
        };
        tokio::task::yield_now().await;
        drop(permit_c);

        timeout(Duration::from_secs(1), waiter_b)
            .await
            .expect("host B waiter completed")
            .expect("host B waiter join");
        let permit_b = limiter
            .try_acquire(host_b)
            .expect("host B should take the freed global slot");
        assert!(
            limiter.try_acquire(host_a.clone()).is_none(),
            "host A remains at its per-host cap"
        );
        drop(permit_a);
        drop(permit_b);
    }

    #[test]
    fn try_acquire_does_not_consume_global_when_per_host_is_full() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(1, 1, availability);
        let host_a = make_address("a.example");
        let host_b = make_address("b.example");
        let permit_a = limiter.try_acquire(host_a.clone()).expect("host A permit");
        assert!(limiter.try_acquire(host_a.clone()).is_none());
        assert!(limiter.try_acquire(host_b.clone()).is_none());
        drop(permit_a);
        assert!(limiter.try_acquire(host_b).is_some());
    }

    #[tokio::test]
    async fn connection_permit_drop_notifies_availability_waiters() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(1, 1, availability.clone());
        let address = make_address("example.com");
        let permit = limiter
            .try_acquire(address.clone())
            .expect("connection permit");

        let waiter = {
            let availability = availability.clone();
            let address = address.clone();
            tokio::spawn(async move {
                availability.listen(&address).await;
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

    #[tokio::test]
    async fn connection_availability_listen_observes_global_notify_waiters_before_first_poll() {
        let availability = ConnectionAvailability::default();
        let address = make_address("example.com");
        let waiter = availability.listen(&address);

        availability.notify_global();

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("global notify_waiters must complete a listen created before the notify");
    }

    #[tokio::test]
    async fn http2_release_wakes_coalescable_waiter_on_another_host() {
        let availability = ConnectionAvailability::default();
        let host_a = make_address("a.test");
        let host_b = make_address("b.test");
        let waiter = {
            let availability = availability.clone();
            let host_b = host_b.clone();
            tokio::spawn(async move {
                availability.listen(&host_b).await;
            })
        };

        tokio::task::yield_now().await;
        availability.notify_http2(&host_a, &["a.test".to_owned(), "b.test".to_owned()]);

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("coalescable host B waiter completed")
            .expect("host B waiter join");
    }

    #[tokio::test]
    async fn http2_release_does_not_wake_unrelated_host() {
        let availability = ConnectionAvailability::default();
        let host_a = make_address("a.test");
        let host_c = make_address("c.test");
        let waiter = {
            let availability = availability.clone();
            let host_c = host_c.clone();
            tokio::spawn(async move {
                availability.listen(&host_c).await;
            })
        };

        tokio::task::yield_now().await;
        availability.notify_http2(&host_a, &["a.test".to_owned(), "b.test".to_owned()]);

        timeout(Duration::from_millis(50), waiter).await.expect_err(
            "host C waiter should ignore an HTTP/2 release that cannot coalesce onto it",
        );
    }

    #[test]
    fn connection_release_does_not_retain_notify_entries_without_waiters() {
        let availability = ConnectionAvailability::default();
        let limiter = ConnectionLimiter::new(1, 1, availability.clone());
        for index in 0..1_000 {
            let address = make_address(&format!("host-{index}.test"));
            let permit = limiter
                .try_acquire(address)
                .expect("connection permit under cap 1");
            drop(permit);
        }
        assert_eq!(
            availability.notify_entry_count(),
            0,
            "releases must not retain a Notify per historical destination"
        );
    }

    #[tokio::test]
    async fn listen_reclaims_notify_entry_when_no_waiters_remain() {
        let availability = ConnectionAvailability::default();
        let address = make_address("example.com");
        let waiter = availability.listen(&address);
        assert_eq!(availability.notify_entry_count(), 1);
        drop(waiter);
        assert_eq!(availability.notify_entry_count(), 0);
    }
}
