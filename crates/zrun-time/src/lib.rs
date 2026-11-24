//! A bounded, per-runtime timer driver without background timer threads.
//!
//! Each admitted sleep owns one removable BTreeMap entry. Dropping a sleep
//! removes it immediately, so cancelled long deadlines do not leave tombstones.
//! Driver and waiter locks are never held together, and waking happens off-lock.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const TURN_BUDGET: usize = 64;
type Key = (Instant, u64);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// A timer could not be admitted or its runtime closed before expiry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerError {
    Closed,
    CapacityExceeded { capacity: usize },
    DeadlineOverflow,
    IdExhausted,
}

impl fmt::Display for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("timer driver is closed"),
            Self::CapacityExceeded { capacity } => {
                write!(
                    f,
                    "timer capacity of {capacity} pending entries is exhausted"
                )
            }
            Self::DeadlineOverflow => f.write_str("timer deadline exceeds Instant range"),
            Self::IdExhausted => f.write_str("timer identity counter is exhausted"),
        }
    }
}

impl std::error::Error for TimerError {}

enum Status {
    Pending,
    Ready(Result<(), TimerError>),
    Cancelled,
}

struct WaitState {
    status: Status,
    waker: Option<Waker>,
}

struct Waiter {
    state: Mutex<WaitState>,
}

impl Waiter {
    fn pending() -> Self {
        Self {
            state: Mutex::new(WaitState {
                status: Status::Pending,
                waker: None,
            }),
        }
    }

    fn ready(value: Result<(), TimerError>) -> Self {
        Self {
            state: Mutex::new(WaitState {
                status: Status::Ready(value),
                waker: None,
            }),
        }
    }

    fn finish(&self, value: Result<(), TimerError>) {
        let waker = {
            let mut state = lock(&self.state);
            if !matches!(state.status, Status::Pending) {
                return;
            }
            state.status = Status::Ready(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            // A foreign waker panic must not strand the rest of a due batch or
            // a shutdown drain. The readiness value is already committed.
            let _ = catch_unwind(AssertUnwindSafe(|| waker.wake()));
        }
    }
}

struct TimerState {
    closed: bool,
    capacity: usize,
    next_id: u64,
    entries: BTreeMap<Key, Arc<Waiter>>,
}

struct DriverInner {
    state: Mutex<TimerState>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

/// A clonable timer handle. It does not keep a dropped runtime alive.
#[derive(Clone)]
pub struct Timer {
    inner: Weak<DriverInner>,
}

/// A single sleep. Admission happens when Timer::sleep is called.
///
/// A completed Sleep must not be polled again. Dropping it cancels and removes
/// any pending driver entry synchronously.
pub struct Sleep {
    inner: Weak<DriverInner>,
    key: Option<Key>,
    waiter: Arc<Waiter>,
    completed: bool,
}

// Sleep exposes no pinned references to its separately allocated waiter.
impl Unpin for Sleep {}

impl Sleep {
    fn ready(value: Result<(), TimerError>) -> Self {
        Self {
            inner: Weak::new(),
            key: None,
            waiter: Arc::new(Waiter::ready(value)),
            completed: false,
        }
    }

    fn deregister(&mut self) {
        if let Some(key) = self.key.take()
            && let Some(inner) = self.inner.upgrade()
        {
            let (removed, changed) = {
                let mut state = lock(&inner.state);
                let changed = state
                    .entries
                    .first_key_value()
                    .is_some_and(|(first, _)| *first == key);
                (state.entries.remove(&key), changed)
            };
            drop(removed);
            if changed {
                (inner.notify)();
            }
        }
    }
}

impl Future for Sleep {
    type Output = Result<(), TimerError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(!this.completed, "sleep polled after completion");
        if this
            .key
            .is_some_and(|(deadline, _)| deadline <= Instant::now())
        {
            this.waiter.finish(Ok(()));
        }

        let mut incoming = Some(cx.waker().clone());
        let (result, previous) = {
            let mut state = lock(&this.waiter.state);
            match state.status {
                Status::Ready(value) => (Poll::Ready(value), state.waker.take()),
                Status::Pending => (
                    Poll::Pending,
                    std::mem::replace(&mut state.waker, incoming.take()),
                ),
                Status::Cancelled => {
                    // Cancellation belongs to Sleep::drop; it cannot be
                    // reached by polling a live, safely owned Sleep.
                    unreachable!("a dropped sleep cannot be polled")
                }
            }
        };
        if result.is_ready() {
            this.completed = true;
            this.deregister();
        }
        drop(previous);
        drop(incoming);
        result
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.deregister();
        let waker = {
            let mut state = lock(&self.waiter.state);
            state.status = Status::Cancelled;
            state.waker.take()
        };
        drop(waker);
    }
}

impl Timer {
    pub fn sleep(&self, duration: Duration) -> Sleep {
        let Some(inner) = self.inner.upgrade() else {
            return Sleep::ready(Err(TimerError::Closed));
        };
        let deadline = Instant::now().checked_add(duration);
        let admitted = {
            let mut state = lock(&inner.state);
            if state.closed {
                Err(TimerError::Closed)
            } else if duration.is_zero() {
                return Sleep::ready(Ok(()));
            } else if let Some(deadline) = deadline {
                if state.entries.len() >= state.capacity {
                    Err(TimerError::CapacityExceeded {
                        capacity: state.capacity,
                    })
                } else if let Some(next_id) = state.next_id.checked_add(1) {
                    let key = (deadline, state.next_id);
                    state.next_id = next_id;
                    let waiter = Arc::new(Waiter::pending());
                    let changed = state
                        .entries
                        .first_key_value()
                        .is_none_or(|(first, _)| key < *first);
                    state.entries.insert(key, waiter.clone());
                    Ok((key, waiter, changed))
                } else {
                    Err(TimerError::IdExhausted)
                }
            } else {
                Err(TimerError::DeadlineOverflow)
            }
        };
        match admitted {
            Ok((key, waiter, changed)) => {
                // A driver already waiting for an earlier deadline will
                // inspect the queue again before it waits past this entry.
                if changed {
                    (inner.notify)();
                }
                Sleep {
                    inner: Arc::downgrade(&inner),
                    key: Some(key),
                    waiter,
                    completed: false,
                }
            }
            Err(error) => Sleep::ready(Err(error)),
        }
    }
}

/// Internal executor-owned driver. Calling turn never polls user futures.
#[doc(hidden)]
pub struct Driver {
    inner: Arc<DriverInner>,
}

impl Driver {
    /// Capacity bounds simultaneous retained entries. Runtime passes max_tasks.
    pub fn new(capacity: usize, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        assert!(capacity > 0, "timer capacity must be positive");
        Self {
            inner: Arc::new(DriverInner {
                state: Mutex::new(TimerState {
                    closed: false,
                    capacity,
                    next_id: 0,
                    entries: BTreeMap::new(),
                }),
                notify,
            }),
        }
    }

    pub fn timer(&self) -> Timer {
        Timer {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn len(&self) -> usize {
        lock(&self.inner.state).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Expire a finite batch, so an already due timer flood cannot monopolize
    /// one turn before the executor revisits its root future.
    pub fn turn(&self) -> usize {
        self.turn_at(Instant::now())
    }

    fn turn_at(&self, now: Instant) -> usize {
        let due = {
            let mut state = lock(&self.inner.state);
            let mut due = Vec::new();
            while due.len() < TURN_BUDGET
                && state
                    .entries
                    .first_key_value()
                    .is_some_and(|(key, _)| key.0 <= now)
            {
                let (_, waiter) = state.entries.pop_first().unwrap();
                due.push(waiter);
            }
            due
        };
        let count = due.len();
        for waiter in due {
            waiter.finish(Ok(()));
        }
        count
    }

    pub fn next_timeout(&self) -> Option<Duration> {
        let deadline = {
            let state = lock(&self.inner.state);
            state.entries.first_key_value().map(|(key, _)| key.0)
        };
        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Close admission and resolve all outstanding sleeps with Closed.
    pub fn shutdown(&self) {
        let entries = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.entries)
        };
        for (_, waiter) in entries {
            waiter.finish(Err(TimerError::Closed));
        }
        (self.inner.notify)();
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn driver(capacity: usize) -> Driver {
        Driver::new(capacity, Arc::new(|| {}))
    }

    fn poll_sleep(sleep: &mut Sleep) -> Poll<Result<(), TimerError>> {
        Pin::new(sleep).poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn only_earliest_deadline_changes_notify_the_driver() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let notifications = Arc::new(AtomicUsize::new(0));
        let count = notifications.clone();
        let driver = Driver::new(
            8,
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let timer = driver.timer();
        let first = timer.sleep(Duration::from_secs(300));
        let later = timer.sleep(Duration::from_secs(600));
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        drop(later);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        let earliest = timer.sleep(Duration::from_secs(60));
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
        drop(earliest);
        assert_eq!(notifications.load(Ordering::Relaxed), 3);
        drop(first);
        assert_eq!(notifications.load(Ordering::Relaxed), 4);
        assert!(driver.is_empty());
    }

    #[test]
    fn zero_sleep_needs_no_entry() {
        let driver = driver(1);
        let timer = driver.timer();
        let _long = timer.sleep(Duration::from_secs(86_400));
        let mut zero = timer.sleep(Duration::ZERO);
        assert_eq!(poll_sleep(&mut zero), Poll::Ready(Ok(())));
        assert_eq!(driver.len(), 1);
    }

    #[test]
    fn cancelled_long_timers_leave_no_retained_entries() {
        let driver = driver(2);
        let timer = driver.timer();
        for _ in 0..512 {
            let sleep = timer.sleep(Duration::from_secs(86_400));
            assert_eq!(driver.len(), 1);
            drop(sleep);
            assert!(driver.is_empty());
        }
        assert_eq!(driver.next_timeout(), None);
    }

    #[test]
    fn capacity_failure_and_drop_recovery_are_explicit() {
        let driver = driver(1);
        let timer = driver.timer();
        let first = timer.sleep(Duration::from_secs(86_400));
        let mut rejected = timer.sleep(Duration::from_secs(86_400));
        assert_eq!(
            poll_sleep(&mut rejected),
            Poll::Ready(Err(TimerError::CapacityExceeded { capacity: 1 }))
        );
        drop(first);
        let _accepted = timer.sleep(Duration::from_secs(86_400));
        assert_eq!(driver.len(), 1);
    }

    #[test]
    fn expiry_wakes_latest_waker_and_removes_entry() {
        let driver = driver(1);
        let mut sleep = driver.timer().sleep(Duration::from_secs(86_400));
        let deadline = sleep.key.unwrap().0;
        let first = Arc::new(Counter::default());
        let second = Arc::new(Counter::default());
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());
        assert!(
            Pin::new(&mut sleep)
                .poll(&mut Context::from_waker(&first_waker))
                .is_pending()
        );
        assert!(
            Pin::new(&mut sleep)
                .poll(&mut Context::from_waker(&second_waker))
                .is_pending()
        );
        assert_eq!(driver.turn_at(deadline), 1);
        assert!(driver.is_empty());
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
        assert_eq!(poll_sleep(&mut sleep), Poll::Ready(Ok(())));
    }

    #[test]
    fn shutdown_wakes_waiters_and_old_handles_stay_closed() {
        let driver = driver(2);
        let timer = driver.timer();
        let mut sleep = timer.sleep(Duration::from_secs(86_400));
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        assert!(
            Pin::new(&mut sleep)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        driver.shutdown();
        driver.shutdown();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(poll_sleep(&mut sleep), Poll::Ready(Err(TimerError::Closed)));
        assert!(driver.is_empty());
        assert_eq!(
            poll_sleep(&mut timer.sleep(Duration::ZERO)),
            Poll::Ready(Err(TimerError::Closed))
        );
        drop(driver);
        assert_eq!(
            poll_sleep(&mut timer.sleep(Duration::ZERO)),
            Poll::Ready(Err(TimerError::Closed))
        );
    }

    #[test]
    fn expiry_and_cancellation_do_not_revive_each_other() {
        let driver = driver(2);
        let timer = driver.timer();
        let mut sleep = timer.sleep(Duration::from_secs(86_400));
        let deadline = sleep.key.unwrap().0;
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        assert!(
            Pin::new(&mut sleep)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(sleep);
        assert_eq!(driver.turn_at(deadline), 0);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn due_batches_are_bounded() {
        let driver = driver(TURN_BUDGET + 1);
        let timer = driver.timer();
        let sleeps: Vec<_> = (0..=TURN_BUDGET)
            .map(|_| timer.sleep(Duration::from_secs(86_400)))
            .collect();
        let after = sleeps.last().unwrap().key.unwrap().0;
        assert_eq!(driver.turn_at(after), TURN_BUDGET);
        assert_eq!(driver.len(), 1);
        assert_eq!(driver.turn_at(after), 1);
        assert!(driver.is_empty());
    }

    #[test]
    fn overflow_is_an_error_and_never_registers() {
        let driver = driver(1);
        let mut sleep = driver.timer().sleep(Duration::MAX);
        assert_eq!(
            poll_sleep(&mut sleep),
            Poll::Ready(Err(TimerError::DeadlineOverflow))
        );
        assert!(driver.is_empty());
    }
}
