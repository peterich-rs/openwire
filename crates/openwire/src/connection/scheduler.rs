use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use futures_util::future::poll_fn;
use openwire_core::WireError;
use parking_lot::Mutex;
use slab::Slab;

use super::{sip_hash_map, Address, SipHashMap};

/// After this many high-urgency (0..=2) promotions, the oldest remaining
/// eligible waiter is promoted so low-urgency traffic cannot starve.
pub(crate) const HIGH_URGENCY_PROMOTIONS_BEFORE_AGING: u32 = 8;
/// Waiters that have sat this long are eligible for aging promotion.
pub(crate) const AGING_WAIT: Duration = Duration::from_millis(250);
/// RFC 9218 urgency values in `0..=2` count as high urgency for aging.
const HIGH_URGENCY_MAX: u8 = 2;

/// RFC 9218 urgency: `0` is highest priority, `7` is lowest. Default is `3`.
///
/// Named constants are aliases, not extra scheduler lanes. Admission uses a
/// single ordered set keyed by `(urgency, seq)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestPriority {
    urgency: u8,
}

impl RequestPriority {
    pub const HIGHEST: Self = Self { urgency: 0 };
    pub const DEFAULT: Self = Self { urgency: 3 };
    pub const LOWEST: Self = Self { urgency: 7 };

    /// Alias for urgency `0`.
    pub const INTERACTIVE: Self = Self::HIGHEST;
    /// Alias for urgency `3` (RFC 9218 default).
    pub const NORMAL: Self = Self::DEFAULT;
    /// Alias for urgency `7`.
    pub const BULK: Self = Self::LOWEST;

    /// PascalCase aliases so `RequestPriority::Normal` keeps compiling.
    #[allow(non_upper_case_globals)]
    pub const Interactive: Self = Self::HIGHEST;
    #[allow(non_upper_case_globals)]
    pub const Normal: Self = Self::DEFAULT;
    #[allow(non_upper_case_globals)]
    pub const Bulk: Self = Self::LOWEST;

    /// Clamps `urgency` into `0..=7`.
    pub const fn from_urgency(urgency: u8) -> Self {
        Self {
            urgency: if urgency > 7 { 7 } else { urgency },
        }
    }

    pub const fn urgency(self) -> u8 {
        self.urgency
    }

    fn is_high_urgency(self) -> bool {
        self.urgency <= HIGH_URGENCY_MAX
    }
}

impl Default for RequestPriority {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug)]
pub(crate) struct RequestScheduler {
    max_total: usize,
    max_per_address: usize,
    max_queued: usize,
    global: Mutex<GlobalState>,
}

#[derive(Debug)]
struct GlobalState {
    global_running: usize,
    queued: usize,
    next_seq: u64,
    high_urgency_promotions: u32,
    waiters: Slab<Waiter>,
    /// Client-wide min-heap: RFC 9218 urgency then enqueue seq.
    ordered: BTreeSet<(u8, u64, usize)>,
    fifo: VecDeque<usize>,
    address_running: SipHashMap<Address, usize>,
    ready_waiters: VecDeque<Waker>,
}

#[derive(Debug)]
struct Waiter {
    waker: Option<Waker>,
    address: Address,
    priority: RequestPriority,
    seq: u64,
    enqueued_at: Instant,
    admitted: bool,
}

#[derive(Debug)]
pub(crate) struct RequestAdmissionPermit {
    scheduler: Option<Arc<RequestScheduler>>,
    address: Option<Address>,
}

impl RequestAdmissionPermit {
    pub(crate) fn unlimited() -> Self {
        Self {
            scheduler: None,
            address: None,
        }
    }
}

struct WaiterGuard {
    scheduler: Arc<RequestScheduler>,
    id: Option<usize>,
}

impl GlobalState {
    fn enqueue_waiter(&mut self, urgency: u8, seq: u64, id: usize) {
        self.ordered.insert((urgency, seq, id));
        self.fifo.push_back(id);
        self.queued += 1;
    }

    fn dequeue_waiter(&mut self, urgency: u8, seq: u64, id: usize) {
        self.ordered.remove(&(urgency, seq, id));
        if let Some(index) = self.fifo.iter().position(|queued| *queued == id) {
            self.fifo.remove(index);
        }
        self.queued = self.queued.saturating_sub(1);
    }

    fn bump_address_running(&mut self, address: &Address) {
        *self.address_running.entry(address.clone()).or_insert(0) += 1;
        self.global_running += 1;
    }

    fn drop_address_running(&mut self, address: &Address) {
        let Some(running) = self.address_running.get_mut(address) else {
            self.global_running = self.global_running.saturating_sub(1);
            return;
        };
        *running = running.saturating_sub(1);
        if *running == 0 {
            self.address_running.remove(address);
        }
        self.global_running = self.global_running.saturating_sub(1);
    }

    fn address_running(&self, address: &Address) -> usize {
        self.address_running.get(address).copied().unwrap_or(0)
    }
}

impl RequestScheduler {
    pub(crate) fn new(max_total: usize, max_per_address: usize, max_queued: usize) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_per_address,
            max_queued,
            global: Mutex::new(GlobalState {
                global_running: 0,
                queued: 0,
                next_seq: 0,
                high_urgency_promotions: 0,
                waiters: Slab::new(),
                ordered: BTreeSet::new(),
                fifo: VecDeque::new(),
                address_running: sip_hash_map(),
                ready_waiters: VecDeque::new(),
            }),
        })
    }

    fn has_global_capacity(&self, running: usize) -> bool {
        running < self.max_total
    }

    fn has_address_capacity(&self, running: usize) -> bool {
        running < self.max_per_address
    }

    pub(crate) fn acquire(
        self: &Arc<Self>,
        address: Address,
        priority: RequestPriority,
    ) -> impl Future<Output = Result<RequestAdmissionPermit, WireError>> {
        let scheduler = Arc::clone(self);
        async move {
            let mut guard = WaiterGuard {
                scheduler: Arc::clone(&scheduler),
                id: None,
            };
            poll_fn(|cx| {
                let poll = scheduler.poll_acquire(cx, &address, priority, &mut guard.id);
                if matches!(poll, Poll::Ready(Ok(_))) {
                    guard.id = None;
                }
                poll
            })
            .await
        }
    }

    fn poll_acquire(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        address: &Address,
        priority: RequestPriority,
        waiter_id: &mut Option<usize>,
    ) -> Poll<Result<RequestAdmissionPermit, WireError>> {
        let mut global = self.global.lock();
        if let Some(id) = *waiter_id {
            if global.waiters.get(id).is_some_and(|waiter| waiter.admitted) {
                let waiter = global.waiters.remove(id);
                *waiter_id = None;
                return Poll::Ready(Ok(RequestAdmissionPermit {
                    scheduler: Some(Arc::clone(self)),
                    address: Some(waiter.address),
                }));
            }
            if let Some(waiter) = global.waiters.get_mut(id) {
                waiter.waker = Some(cx.waker().clone());
            }
            return Poll::Pending;
        }

        let can_run = self.has_global_capacity(global.global_running)
            && self.has_address_capacity(global.address_running(address));
        let blocked_by_queue = global
            .ordered
            .iter()
            .next()
            .is_some_and(|(urgency, _, _)| *urgency <= priority.urgency());

        if can_run && !blocked_by_queue {
            global.bump_address_running(address);
            return Poll::Ready(Ok(RequestAdmissionPermit {
                scheduler: Some(Arc::clone(self)),
                address: Some(address.clone()),
            }));
        }

        if global.queued >= self.max_queued {
            return Poll::Ready(Err(WireError::capacity("request admission queue is full")));
        }

        let seq = global.next_seq;
        global.next_seq += 1;
        let id = global.waiters.insert(Waiter {
            waker: Some(cx.waker().clone()),
            address: address.clone(),
            priority,
            seq,
            enqueued_at: Instant::now(),
            admitted: false,
        });
        global.enqueue_waiter(priority.urgency(), seq, id);
        *waiter_id = Some(id);
        let promoted = self.try_promote(&mut global);
        if global.waiters.get(id).is_some_and(|waiter| waiter.admitted) {
            let waiter = global.waiters.remove(id);
            *waiter_id = None;
            return Poll::Ready(Ok(RequestAdmissionPermit {
                scheduler: Some(Arc::clone(self)),
                address: Some(waiter.address),
            }));
        }
        drop(global);
        if let Some(waker) = promoted {
            waker.wake();
        }
        Poll::Pending
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut global = self.global.lock();
        if self.has_global_capacity(global.global_running) {
            return Poll::Ready(());
        }
        if !global
            .ready_waiters
            .iter()
            .any(|existing| existing.will_wake(cx.waker()))
        {
            global.ready_waiters.push_back(cx.waker().clone());
        }
        Poll::Pending
    }

    pub(crate) fn release(&self, address: Address) {
        let mut global = self.global.lock();
        global.drop_address_running(&address);
        let ready = self.wake_ready_waiters(&mut global);
        let promoted = self.try_promote(&mut global);
        drop(global);
        if let Some(waker) = ready {
            waker.wake();
        }
        if let Some(waker) = promoted {
            waker.wake();
        }
    }

    fn cancel_waiter(&self, id: usize) {
        let mut global = self.global.lock();
        let Some(waiter) = global.waiters.get(id) else {
            return;
        };
        let address = waiter.address.clone();
        let admitted = waiter.admitted;
        let urgency = waiter.priority.urgency();
        let seq = waiter.seq;
        if admitted {
            global.waiters.remove(id);
            global.drop_address_running(&address);
            let ready = self.wake_ready_waiters(&mut global);
            let promoted = self.try_promote(&mut global);
            drop(global);
            if let Some(waker) = ready {
                waker.wake();
            }
            if let Some(waker) = promoted {
                waker.wake();
            }
            return;
        }

        global.waiters.remove(id);
        global.dequeue_waiter(urgency, seq, id);
    }

    fn try_promote(&self, global: &mut GlobalState) -> Option<Waker> {
        if !self.has_global_capacity(global.global_running) {
            return None;
        }
        let id = select_waiter(global, self.max_per_address)?;
        let urgency = global.waiters[id].priority.urgency();
        let seq = global.waiters[id].seq;
        let address = global.waiters[id].address.clone();
        let high_urgency = global.waiters[id].priority.is_high_urgency();
        global.dequeue_waiter(urgency, seq, id);
        let waiter = &mut global.waiters[id];
        waiter.admitted = true;
        let waker = waiter.waker.take();
        if high_urgency {
            global.high_urgency_promotions = global.high_urgency_promotions.saturating_add(1);
        } else {
            global.high_urgency_promotions = 0;
        }
        global.bump_address_running(&address);
        waker
    }

    fn wake_ready_waiters(&self, global: &mut GlobalState) -> Option<Waker> {
        if !self.has_global_capacity(global.global_running) {
            return None;
        }
        global.ready_waiters.pop_front()
    }
}

impl Drop for RequestAdmissionPermit {
    fn drop(&mut self) {
        if let (Some(scheduler), Some(address)) = (self.scheduler.take(), self.address.take()) {
            scheduler.release(address);
        }
    }
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.scheduler.cancel_waiter(id);
        }
    }
}

fn address_has_capacity(global: &GlobalState, address: &Address, max_per_address: usize) -> bool {
    global.address_running(address) < max_per_address
}

fn select_waiter(global: &GlobalState, max_per_address: usize) -> Option<usize> {
    let eligible = |id: usize| {
        global
            .waiters
            .get(id)
            .is_some_and(|waiter| address_has_capacity(global, &waiter.address, max_per_address))
    };
    let best = global
        .ordered
        .iter()
        .map(|(_, _, id)| *id)
        .find(|id| eligible(*id))?;
    let oldest = global.fifo.iter().copied().find(|id| eligible(*id))?;
    if oldest == best {
        return Some(best);
    }

    let oldest_waiter = &global.waiters[oldest];
    let best_waiter = &global.waiters[best];
    let aged_by_time = oldest_waiter.enqueued_at.elapsed() >= AGING_WAIT;
    let aged_by_count = global.high_urgency_promotions >= HIGH_URGENCY_PROMOTIONS_BEFORE_AGING
        && oldest_waiter.priority.urgency() > best_waiter.priority.urgency();
    if aged_by_time || aged_by_count {
        return Some(oldest);
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::Poll;
    use std::time::Duration;

    use futures_util::future::poll_fn;
    use tokio::time::timeout;

    use super::{RequestPriority, RequestScheduler, AGING_WAIT};
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

    async fn poll_pending<F>(future: &mut std::pin::Pin<Box<F>>)
    where
        F: std::future::Future,
    {
        poll_fn(|cx| {
            if future.as_mut().poll(cx).is_pending() {
                Poll::Ready(())
            } else {
                panic!("expected pending acquire")
            }
        })
        .await;
    }

    #[test]
    fn urgency_clamps_to_rfc9218_range() {
        assert_eq!(RequestPriority::from_urgency(0).urgency(), 0);
        assert_eq!(RequestPriority::from_urgency(7).urgency(), 7);
        assert_eq!(RequestPriority::from_urgency(9).urgency(), 7);
        assert_eq!(RequestPriority::DEFAULT.urgency(), 3);
        assert_eq!(
            RequestPriority::INTERACTIVE,
            RequestPriority::from_urgency(0)
        );
        assert_eq!(RequestPriority::NORMAL, RequestPriority::from_urgency(3));
        assert_eq!(RequestPriority::BULK, RequestPriority::from_urgency(7));
    }

    #[tokio::test]
    async fn lower_urgency_number_is_admitted_first() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::from_urgency(3))
            .await
            .expect("held permit");

        let order = Arc::new(StdMutex::new(Vec::new()));
        let later = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(address, RequestPriority::from_urgency(5))
                    .await
                    .expect("u=5 permit");
                order.lock().expect("order").push(5);
                permit
            })
        };
        tokio::task::yield_now().await;

        let earlier = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(address, RequestPriority::from_urgency(1))
                    .await
                    .expect("u=1 permit");
                order.lock().expect("order").push(1);
                permit
            })
        };
        tokio::task::yield_now().await;

        drop(first);
        let first_waiter = timeout(Duration::from_secs(1), earlier)
            .await
            .expect("u=1 completed")
            .expect("u=1 join");
        assert_eq!(&*order.lock().expect("order"), &[1]);
        drop(first_waiter);
        let second_waiter = timeout(Duration::from_secs(1), later)
            .await
            .expect("u=5 completed")
            .expect("u=5 join");
        assert_eq!(&*order.lock().expect("order"), &[1, 5]);
        drop(second_waiter);
    }

    #[tokio::test]
    async fn interactive_waiter_is_admitted_before_later_bulk() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let order = Arc::new(StdMutex::new(Vec::new()));
        let bulk = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(address, RequestPriority::Bulk)
                    .await
                    .expect("bulk permit");
                order.lock().expect("order").push("bulk");
                permit
            })
        };
        tokio::task::yield_now().await;

        let interactive = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(address, RequestPriority::Interactive)
                    .await
                    .expect("interactive permit");
                order.lock().expect("order").push("interactive");
                permit
            })
        };
        tokio::task::yield_now().await;

        drop(first);
        let interactive_permit = timeout(Duration::from_secs(1), interactive)
            .await
            .expect("interactive completed")
            .expect("interactive join");
        assert_eq!(&*order.lock().expect("order"), &["interactive"]);

        drop(interactive_permit);
        let bulk_permit = timeout(Duration::from_secs(1), bulk)
            .await
            .expect("bulk completed")
            .expect("bulk join");
        assert_eq!(&*order.lock().expect("order"), &["interactive", "bulk"]);
        drop(bulk_permit);
    }

    #[tokio::test]
    async fn fifo_within_the_same_priority() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for index in 0..3 {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            let order = order.clone();
            waiters.push(tokio::spawn(async move {
                let permit = scheduler
                    .acquire(address, RequestPriority::Normal)
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
    async fn aged_bulk_still_completes_after_interactive_promotions() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let mut held = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let bulk = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            tokio::spawn(async move {
                scheduler
                    .acquire(address, RequestPriority::Bulk)
                    .await
                    .expect("bulk permit")
            })
        };
        tokio::task::yield_now().await;

        let mut interactive = Vec::new();
        for _ in 0..9 {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            interactive.push(tokio::spawn(async move {
                scheduler
                    .acquire(address, RequestPriority::Interactive)
                    .await
                    .expect("interactive permit")
            }));
            tokio::task::yield_now().await;
        }

        for _ in 0..8 {
            drop(held);
            held = timeout(Duration::from_secs(1), interactive.remove(0))
                .await
                .expect("interactive completed")
                .expect("interactive join");
        }

        drop(held);
        let bulk_permit = timeout(Duration::from_secs(1), bulk)
            .await
            .expect("aged bulk completed")
            .expect("bulk join");
        drop(bulk_permit);
        drop(interactive.remove(0));
    }

    #[tokio::test]
    async fn aged_bulk_completes_after_wait_threshold() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let bulk = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            tokio::spawn(async move {
                scheduler
                    .acquire(address, RequestPriority::Bulk)
                    .await
                    .expect("bulk permit")
            })
        };
        tokio::task::yield_now().await;

        let interactive = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            tokio::spawn(async move {
                scheduler
                    .acquire(address, RequestPriority::Interactive)
                    .await
                    .expect("interactive permit")
            })
        };
        tokio::task::yield_now().await;
        tokio::time::sleep(AGING_WAIT + Duration::from_millis(20)).await;

        drop(first);
        let bulk_permit = timeout(Duration::from_secs(1), bulk)
            .await
            .expect("aged bulk completed")
            .expect("bulk join");
        drop(bulk_permit);
        drop(interactive);
    }

    #[tokio::test]
    async fn queue_full_fails_before_taking_a_running_slot() {
        let scheduler = RequestScheduler::new(1, 1, 1);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let queued = {
            let scheduler = Arc::clone(&scheduler);
            let address = address.clone();
            tokio::spawn(async move {
                scheduler
                    .acquire(address, RequestPriority::Normal)
                    .await
                    .expect("queued permit")
            })
        };
        tokio::task::yield_now().await;

        let error = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect_err("queue is full");
        assert_eq!(error.kind(), openwire_core::WireErrorKind::Capacity);
        assert_eq!(error.phase(), openwire_core::FailurePhase::Admission);

        drop(first);
        drop(queued);
    }

    #[tokio::test]
    async fn cancel_of_queued_waiter_does_not_leak_running_counts() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let mut queued = Box::pin(scheduler.acquire(address.clone(), RequestPriority::Normal));
        poll_pending(&mut queued).await;
        drop(queued);

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            scheduler.acquire(address, RequestPriority::Normal),
        )
        .await
        .expect("second acquire completed")
        .expect("second permit");
        drop(second);
    }

    #[tokio::test]
    async fn cancel_of_ready_waiter_does_not_leak_running_counts() {
        let scheduler = RequestScheduler::new(1, 1, usize::MAX);
        let address = make_address("example.com");
        let first = scheduler
            .acquire(address.clone(), RequestPriority::Normal)
            .await
            .expect("held permit");

        let mut queued = Box::pin(scheduler.acquire(address.clone(), RequestPriority::Normal));
        poll_pending(&mut queued).await;
        drop(first);
        drop(queued);

        let second = timeout(
            Duration::from_secs(1),
            scheduler.acquire(address, RequestPriority::Normal),
        )
        .await
        .expect("second acquire completed")
        .expect("second permit");
        drop(second);
    }

    #[tokio::test]
    async fn higher_urgency_on_another_host_beats_queued_low_urgency() {
        let scheduler = RequestScheduler::new(1, 5, usize::MAX);
        let events = make_address("events.example");
        let api = make_address("api.example");
        let held = scheduler
            .acquire(events.clone(), RequestPriority::from_urgency(7))
            .await
            .expect("held event permit");

        let order = Arc::new(StdMutex::new(Vec::new()));
        let event_waiter = {
            let scheduler = Arc::clone(&scheduler);
            let events = events.clone();
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(events, RequestPriority::from_urgency(7))
                    .await
                    .expect("queued event");
                order.lock().expect("order").push("event");
                permit
            })
        };
        tokio::task::yield_now().await;

        let api_waiter = {
            let scheduler = Arc::clone(&scheduler);
            let order = order.clone();
            tokio::spawn(async move {
                let permit = scheduler
                    .acquire(api, RequestPriority::from_urgency(0))
                    .await
                    .expect("api permit");
                order.lock().expect("order").push("api");
                permit
            })
        };
        tokio::task::yield_now().await;

        drop(held);
        let api_permit = timeout(Duration::from_secs(1), api_waiter)
            .await
            .expect("api completed")
            .expect("api join");
        assert_eq!(&*order.lock().expect("order"), &["api"]);
        drop(api_permit);
        let event_permit = timeout(Duration::from_secs(1), event_waiter)
            .await
            .expect("event completed")
            .expect("event join");
        assert_eq!(&*order.lock().expect("order"), &["api", "event"]);
        drop(event_permit);
    }

    #[tokio::test]
    async fn waiting_on_one_host_does_not_consume_global_capacity() {
        let scheduler = RequestScheduler::new(2, 1, usize::MAX);
        let host_a = make_address("a.example");
        let host_b = make_address("b.example");
        let first = scheduler
            .acquire(host_a.clone(), RequestPriority::Normal)
            .await
            .expect("host a permit");

        let mut queued = Box::pin(scheduler.acquire(host_a.clone(), RequestPriority::Normal));
        poll_pending(&mut queued).await;

        let other = timeout(
            Duration::from_secs(1),
            scheduler.acquire(host_b, RequestPriority::Normal),
        )
        .await
        .expect("host b should not wait on host a")
        .expect("host b permit");
        drop(other);
        drop(first);
        drop(queued);
    }
}
