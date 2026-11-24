//! Task ownership, join results, cancellation, and the task RawWaker.
//!
//! Atomic scheduling state grants the unique poll or shutdown reservation.
//! A poll owner exclusively accesses the pinned future across inline repolls
//! and releases it before publishing Idle or Queued.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

mod allocation;
mod references;
mod schedule;
mod slot;

pub use allocation::TaskRef;
use allocation::{JoinOwner, Operations, poll_waker};
use references::References;
use slot::{Claim, FutureSlot, Suspend};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// A task ended without a successful output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JoinError {
    Cancelled,
    Panicked(String),
    Shutdown,
}

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
    pub fn is_panicked(&self) -> bool {
        matches!(self, Self::Panicked(_))
    }
    pub fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("task was cancelled"),
            Self::Panicked(message) => write!(f, "task panicked: {message}"),
            Self::Shutdown => f.write_str("runtime shut down before task completion"),
        }
    }
}

impl std::error::Error for JoinError {}

/// Await a task's single result. Dropping this handle detaches the wait.
pub struct JoinHandle<T> {
    owner: JoinOwner,
    result: NonNull<JoinCell<T>>,
}

// SAFETY: the join reservation keeps the cell live across transfers. The
// mutex synchronizes access to T, and cancellation upgrades atomically.
unsafe impl<T: Send> Send for JoinHandle<T> {}
// SAFETY: sharing a handle permits only synchronized cell reads and abort.
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    /// Request cancellation. The driver processes it on its next task turn.
    ///
    /// Cancellation cannot interrupt a poll that does not return. If that poll
    /// completes, its already-produced result wins over the abort request.
    pub fn abort(&self) {
        if let Some(task) = self.owner.upgrade() {
            task.request_abort();
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            *lock(&self.result().state),
            JoinState::Ready(_) | JoinState::Taken
        )
    }

    fn result(&self) -> &JoinCell<T> {
        // SAFETY: the join owner anchors the typed field for this borrow.
        unsafe { self.result.as_ref() }
    }
}

// The result sits in the task allocation. Pinning its handle never pins T.
impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Waker cloning and drops may call user code.
        let incoming = cx.waker().clone();
        let old = {
            let mut state = lock(&self.result().state);
            match &mut *state {
                JoinState::Ready(_) => {
                    let JoinState::Ready(result) = std::mem::replace(&mut *state, JoinState::Taken)
                    else {
                        unreachable!("matched a ready join result");
                    };
                    return Poll::Ready(result);
                }
                JoinState::Waiting(waker) => {
                    if waker.as_ref().is_some_and(|old| old.will_wake(&incoming)) {
                        None
                    } else {
                        waker.replace(incoming)
                    }
                }
                JoinState::Taken | JoinState::Detached => {
                    panic!("join handle polled after completion")
                }
            }
        };
        drop(old);
        Poll::Pending
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        let discarded = {
            let mut state = lock(&self.result().state);
            std::mem::replace(&mut *state, JoinState::Detached)
        };
        drop(discarded);
    }
}

enum JoinState<T> {
    Waiting(Option<Waker>),
    Ready(Result<T, JoinError>),
    Taken,
    Detached,
}

struct JoinCell<T> {
    state: Mutex<JoinState<T>>,
}

impl<T> JoinCell<T> {
    fn finish(&self, value: Result<T, JoinError>) {
        let mut value = Some(value);
        let waker = {
            let mut state = lock(&self.state);
            if let JoinState::Waiting(slot) = &mut *state {
                let waiter = slot.take();
                *state = JoinState::Ready(value.take().expect("completion has one result"));
                waiter
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        // Detached result values may execute user destructors.
        drop(value);
    }
}

/// Internal executor integration. Implementations must not poll in enqueue.
#[doc(hidden)]
pub trait Scheduler: Send + Sync {
    fn enqueue(&self, task: TaskRef);
    fn finished(&self, task: &Task);
}

/// The type-erased header of a task allocation.
#[doc(hidden)]
pub struct Task {
    id: u64,
    registry_index: AtomicUsize,
    runnable: FutureSlot<()>,
    future_live: AtomicBool,
    scheduler: Weak<dyn Scheduler>,
    references: References,
    operations: &'static Operations,
}

impl Task {
    /// Create an initially queued task. The caller must atomically register it
    /// and insert exactly one initial runnable before exposing the join handle.
    pub fn allocate<F>(
        id: u64,
        future: F,
        scheduler: Weak<dyn Scheduler>,
    ) -> (TaskRef, JoinHandle<F::Output>)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        allocation::new(id, future, scheduler)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// A scheduler-owned lookup hint, not the task's identity. The scheduler
    /// reads and changes it under its registry lock and checks the allocation
    /// at that index before removal or enqueue. Moving a registry entry never
    /// moves this Task allocation or changes its assigned id.
    pub fn registry_index(&self) -> usize {
        self.registry_index.load(Ordering::Relaxed)
    }

    /// Update the hint while holding the owning scheduler's registry lock.
    /// That lock publishes entries and moves; this atomic only makes storage
    /// thread-safe and does not synchronize future ownership or scheduling.
    pub fn set_registry_index(&self, index: usize) {
        self.registry_index.store(index, Ordering::Relaxed);
    }
}

impl TaskRef {
    fn schedule(&self) {
        if self.runnable.wake() {
            self.enqueue();
        }
    }

    fn enqueue(&self) {
        if let Some(scheduler) = self.scheduler.upgrade() {
            scheduler.enqueue(self.clone());
        }
    }

    fn request_abort(&self) {
        if self.runnable.abort() {
            self.enqueue();
        }
    }

    fn finished(&self) {
        if let Some(scheduler) = self.scheduler.upgrade() {
            scheduler.finished(self);
        }
    }

    /// Run at most one poll. Repeated or stale runnable entries are harmless;
    /// only the Queued -> Running transition can extract the future.
    pub fn run_once(&self) {
        self.run_budget(0);
    }

    /// Poll and immediately drive a bounded number of notifications observed
    /// during those polls. The driver keeps the future and its waker across
    /// the inline polls. When the budget expires, it publishes one runnable.
    pub fn run_budget(&self, inline_repolls: usize) {
        if self.poll_budget(inline_repolls) {
            self.enqueue();
        }
    }

    /// Drive an owned runnable taken from its scheduler. When another runnable
    /// is due, `requeue` must return it to that scheduler unless it has closed.
    /// The callback owns the only publication for this transition.
    pub fn run_owned(self, inline_repolls: usize, requeue: impl FnOnce(Self)) {
        if self.poll_budget(inline_repolls) {
            requeue(self);
        }
    }

    fn poll_budget(&self, mut inline_repolls: usize) -> bool {
        let owner = match self.runnable.begin() {
            Claim::Skip => return false,
            Claim::Poll(owner) => owner,
            Claim::Cancel((), reason) => {
                self.finish_failure(reason.error());
                return false;
            }
        };

        let waker = poll_waker(self);
        let mut cx = Context::from_waker(&waker);
        loop {
            let result = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: owner holds the exclusive running reservation,
                // and self keeps the allocation alive.
                unsafe { (self.operations.poll)(self.pointer(), &mut cx) }
            }));
            match result {
                Ok(Poll::Pending) => {
                    if inline_repolls != 0 && owner.try_repoll() {
                        inline_repolls -= 1;
                        continue;
                    }
                    match owner.pending() {
                        Suspend::Idle => return false,
                        Suspend::Requeue => return true,
                        Suspend::Cancel((), reason) => {
                            self.finish_failure(reason.error());
                            return false;
                        }
                    }
                }
                Ok(Poll::Ready(())) => {
                    // The poll destroyed the future before publishing success.
                    owner.complete();
                    self.finished();
                    return false;
                }
                Err(payload) => {
                    owner.complete();
                    self.finish_failure(JoinError::Panicked(panic_message(payload)));
                    return false;
                }
            }
        }
    }

    fn finish_failure(&self, error: JoinError) {
        // A panicking destructor is caught after the future has been removed
        // from task state. Cancellation/shutdown retain their original reason.
        // SAFETY: all callers won a terminal transition. A prior panic may
        // have already emptied the slot, which the allocation tracks.
        unsafe { self.discard_future() };
        // A foreign join waker may panic. Lifecycle cleanup still has to retire
        // the task; delivery already consumed the channel's unique sender.
        self.fail_join(error);
        self.finished();
    }

    /// Terminate an unpolled/idle task on the driver thread. A concurrent poll
    /// keeps its ownership and observes Shutdown when it next returns Pending.
    pub fn shutdown(&self) {
        if let Some(((), reason)) = self.runnable.shutdown() {
            self.finish_failure(reason.error());
        }
    }

    fn finalize_unregistered(&self) {
        if let Some(((), reason)) = self.runnable.shutdown() {
            self.fail_join(reason.error());
            // SAFETY: the last execution reference still holds the finalizer
            // and claimed this live future with the shutdown transition.
            unsafe { self.discard_future() };
        }
    }

    fn fail_join(&self, error: JoinError) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: an execution reference or the finalizer protects the
            // allocation, and the result cell uses a mutex.
            unsafe { (self.operations.fail)(self.pointer(), error) };
        }));
    }

    /// The caller must hold the exclusive reservation for the future slot.
    unsafe fn discard_future(&self) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: the caller owns the only terminal future reservation.
            unsafe { (self.operations.drop_future)(self.pointer()) };
        }));
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Cooperatively return to the executor once.
pub fn yield_now() -> impl Future<Output = ()> + Send {
    YieldNow(false)
}

struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::allocation::task_waker;
    use crate::schedule::{Begin, CancelReason, Pending, ScheduleState};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestScheduler {
        queue: Mutex<VecDeque<TaskRef>>,
        finished: AtomicUsize,
    }

    impl Scheduler for TestScheduler {
        fn enqueue(&self, task: TaskRef) {
            lock(&self.queue).push_back(task);
        }
        fn finished(&self, _task: &Task) {
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl TestScheduler {
        fn run(&self) -> bool {
            let task = { lock(&self.queue).pop_front() };
            if let Some(task) = task {
                task.run_once();
                true
            } else {
                false
            }
        }
        fn len(&self) -> usize {
            lock(&self.queue).len()
        }
    }

    fn setup<F>(future: F) -> (Arc<TestScheduler>, TaskRef, JoinHandle<F::Output>)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let scheduler = Arc::new(TestScheduler::default());
        let erased: Arc<dyn Scheduler> = scheduler.clone();
        let (task, join) = Task::allocate(1, future, Arc::downgrade(&erased));
        scheduler.enqueue(task.clone());
        (scheduler, task, join)
    }

    fn poll_join<T>(join: &mut JoinHandle<T>) -> Poll<Result<T, JoinError>> {
        Pin::new(join).poll(&mut Context::from_waker(Waker::noop()))
    }

    struct Tracked {
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        mode: u8,
        saved: Arc<Mutex<Option<Waker>>>,
    }

    impl Future for Tracked {
        type Output = usize;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
            let number = self.polls.fetch_add(1, Ordering::SeqCst);
            *lock(&self.saved) = Some(cx.waker().clone());
            match self.mode {
                0 => Poll::Pending,
                1 if number == 0 => {
                    cx.waker().wake_by_ref();
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                2 => panic!("task poll failure"),
                _ => Poll::Ready(42),
            }
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn tracked(mode: u8) -> (Tracked, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        (
            Tracked {
                polls: polls.clone(),
                drops: drops.clone(),
                mode,
                saved: Arc::new(Mutex::new(None)),
            },
            polls,
            drops,
        )
    }

    #[test]
    fn raw_waker_counts_and_last_drop_are_miri_compatible() {
        let (future, polls, drops) = tracked(0);
        let (scheduler, task, mut join) = setup(future);
        let initial = task.references.task_count();
        let waker = task_waker(task.clone());
        assert_eq!(task.references.task_count(), initial + 1);
        let cloned = waker.clone();
        assert_eq!(task.references.task_count(), initial + 2);
        waker.wake_by_ref();
        assert_eq!(task.references.task_count(), initial + 2);
        cloned.wake();
        assert_eq!(task.references.task_count(), initial + 1);
        assert_eq!(scheduler.len(), 1);
        task.shutdown();
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(poll_join(&mut join), Poll::Ready(Err(JoinError::Shutdown)));
        waker.wake_by_ref();
        assert_eq!(scheduler.len(), 1);
        assert!(scheduler.run()); // The already queued entry is now inert.
        assert!(!scheduler.run());
        drop(task);
        assert!(join.owner.upgrade().is_some());
        drop(waker); // The final owning reference is released by raw_drop.
        assert!(join.owner.upgrade().is_none());
    }

    #[test]
    fn repeated_wake_during_poll_requeues_once_and_stale_wake_is_inert() {
        let (future, polls, drops) = tracked(1);
        let saved = future.saved.clone();
        let (scheduler, task, mut join) = setup(future);
        assert!(scheduler.run());
        assert_eq!(scheduler.len(), 1);
        assert!(scheduler.run());
        assert_eq!(poll_join(&mut join), Poll::Ready(Ok(42)));
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let stale = lock(&saved).take().unwrap();
        for _ in 0..32 {
            stale.wake_by_ref();
        }
        assert_eq!(scheduler.len(), 0);
        task.run_once();
        task.shutdown();
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inline_budget_publishes_a_single_runnable_when_exhausted() {
        let mut polls = 0;
        let (scheduler, task, mut join) = setup(std::future::poll_fn(move |cx| {
            polls += 1;
            if polls == 4 {
                Poll::Ready(polls)
            } else {
                cx.waker().wake_by_ref();
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
        assert!(TaskRef::ptr_eq(
            &lock(&scheduler.queue).pop_front().unwrap(),
            &task
        ));
        task.run_budget(2);
        assert!(poll_join(&mut join).is_pending());
        assert_eq!(scheduler.len(), 1);
        assert!(scheduler.run());
        assert_eq!(poll_join(&mut join), Poll::Ready(Ok(4)));
        assert_eq!(scheduler.len(), 0);
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shutdown_during_an_inline_poll_stops_the_next_repoll() {
        let holder: Arc<Mutex<Option<TaskRef>>> = Arc::new(Mutex::new(None));
        let inner = holder.clone();
        let mut polls = 0;
        let (scheduler, task, mut join) = setup(std::future::poll_fn(move |cx| {
            polls += 1;
            if polls == 2 {
                let task = lock(&inner).take().unwrap();
                task.shutdown();
            }
            assert!(polls <= 2);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }));
        *lock(&holder) = Some(task.clone());
        assert!(TaskRef::ptr_eq(
            &lock(&scheduler.queue).pop_front().unwrap(),
            &task
        ));
        task.run_budget(8);
        assert_eq!(poll_join(&mut join), Poll::Ready(Err(JoinError::Shutdown)));
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.len(), 0);
    }

    #[test]
    fn abort_before_poll_and_abort_idle_drop_once() {
        for already_polled in [false, true] {
            let (future, polls, drops) = tracked(0);
            let saved = future.saved.clone();
            let (scheduler, _task, mut join) = setup(future);
            if already_polled {
                scheduler.run();
                assert_eq!(scheduler.len(), 0);
            }
            for _ in 0..32 {
                join.abort();
            }
            assert!(!join.is_finished());
            assert_eq!(scheduler.len(), 1);
            scheduler.run();
            assert!(join.is_finished());
            assert_eq!(poll_join(&mut join), Poll::Ready(Err(JoinError::Cancelled)));
            assert_eq!(polls.load(Ordering::SeqCst), usize::from(already_polled));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(lock(&saved).take());
            assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn ready_result_wins_abort_requested_during_its_poll() {
        let holder: Arc<Mutex<Option<TaskRef>>> = Arc::new(Mutex::new(None));
        let inner = holder.clone();
        let (scheduler, task, mut join) = setup(async move {
            let task = lock(&inner).take().unwrap();
            task.request_abort();
            19
        });
        *lock(&holder) = Some(task.clone());
        scheduler.run();
        assert_eq!(poll_join(&mut join), Poll::Ready(Ok(19)));
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.len(), 0);
    }

    #[test]
    fn panic_is_reported_and_future_dropped_once() {
        let (future, polls, drops) = tracked(2);
        let saved = future.saved.clone();
        let (scheduler, _task, mut join) = setup(future);
        scheduler.run();
        let result = poll_join(&mut join);
        assert!(matches!(
            result,
            Poll::Ready(Err(JoinError::Panicked(ref text))) if text == "task poll failure"
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
        drop(lock(&saved).take());
    }

    #[test]
    fn ready_future_with_panicking_drop_reports_error_once() {
        struct PanickingDrop(Arc<AtomicUsize>);

        impl Future for PanickingDrop {
            type Output = usize;

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<usize> {
                Poll::Ready(42)
            }
        }

        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("future destructor");
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let (scheduler, task, mut join) = setup(PanickingDrop(drops.clone()));
        assert!(scheduler.run());
        assert!(matches!(
            poll_join(&mut join),
            Poll::Ready(Err(JoinError::Panicked(message))) if message == "future destructor"
        ));
        task.shutdown();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_an_unregistered_task_completes_its_join() {
        let scheduler: Arc<dyn Scheduler> = Arc::new(TestScheduler::default());
        let (task, mut join) =
            Task::allocate(1, std::future::pending::<()>(), Arc::downgrade(&scheduler));
        assert!(poll_join(&mut join).is_pending());
        drop(task);
        assert!(matches!(
            poll_join(&mut join),
            Poll::Ready(Err(JoinError::Shutdown))
        ));
    }

    #[test]
    fn detach_can_race_with_the_last_unregistered_reference() {
        let scheduler: Arc<dyn Scheduler> = Arc::new(TestScheduler::default());
        let (future, polls, drops) = tracked(0);
        let (task, join) = Task::allocate(1, future, Arc::downgrade(&scheduler));
        std::thread::scope(|scope| {
            scope.spawn(move || drop(task));
            drop(join);
        });
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inline_and_boxed_futures_keep_their_pin_and_drop_with_a_stale_waker() {
        struct Pinned<const N: usize> {
            location: std::cell::Cell<Option<usize>>,
            drops: Arc<AtomicUsize>,
            saved: Arc<Mutex<Option<Waker>>>,
            payload: [u8; N],
            _pin: std::marker::PhantomPinned,
        }

        impl<const N: usize> Future for Pinned<N> {
            type Output = usize;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
                let this = self.as_ref().get_ref();
                let address = std::ptr::from_ref(this).addr();
                if let Some(previous) = this.location.replace(Some(address)) {
                    assert_eq!(previous, address);
                    Poll::Ready(this.payload.len())
                } else {
                    *lock(&this.saved) = Some(cx.waker().clone());
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        impl<const N: usize> Drop for Pinned<N> {
            fn drop(&mut self) {
                assert_eq!(self.location.get(), Some(std::ptr::from_ref(self).addr()));
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn check<const N: usize>() {
            let drops = Arc::new(AtomicUsize::new(0));
            let saved = Arc::new(Mutex::new(None));
            let (scheduler, task, mut join) = setup(Pinned {
                location: std::cell::Cell::new(None),
                drops: drops.clone(),
                saved: saved.clone(),
                payload: [0; N],
                _pin: std::marker::PhantomPinned,
            });
            assert!(scheduler.run());
            assert!(scheduler.run());
            assert_eq!(poll_join(&mut join), Poll::Ready(Ok(N)));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(task);
            drop(join);
            let stale = lock(&saved).take().unwrap();
            stale.wake();
            assert!(!scheduler.run());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }

        check::<0>();
        check::<1024>();
    }

    #[test]
    fn detaching_does_not_cancel_and_discards_one_result() {
        struct Value(Arc<AtomicUsize>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let output_drops = drops.clone();
        let (scheduler, _task, join) = setup(async move {
            yield_now().await;
            Value(output_drops)
        });
        drop(join);
        assert!(scheduler.run());
        assert!(scheduler.run());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actual_wake_pending_transitions_cover_both_orders() {
        // The atomic transitions serialize wake and return-Pending into
        // these two relevant orders. This is an exhaustive test of
        // the implementation's transition functions, not a claimed Loom run.
        for wake_first in [true, false] {
            let state = ScheduleState::new();
            assert_eq!(state.begin(), Begin::Poll);
            let mut enqueue_count = 0;
            if wake_first {
                enqueue_count += usize::from(state.wake());
                enqueue_count += usize::from(state.pending() == Pending::Requeue);
            } else {
                assert_eq!(state.pending(), Pending::Idle);
                enqueue_count += usize::from(state.wake());
            }
            assert_eq!(enqueue_count, 1);
            assert!(!state.wake());
            assert_eq!(state.begin(), Begin::Poll);
        }
    }

    #[test]
    fn actual_abort_pending_transitions_cover_both_orders() {
        for abort_first in [true, false] {
            let state = ScheduleState::new();
            assert_eq!(state.begin(), Begin::Poll);
            if abort_first {
                assert!(!state.abort());
                assert_eq!(state.pending(), Pending::Cancel(CancelReason::Abort));
            } else {
                assert_eq!(state.pending(), Pending::Idle);
                assert!(state.abort());
                assert_eq!(state.begin(), Begin::Cancel(CancelReason::Abort));
            }
            assert!(state.is_complete());
            assert!(!state.wake());
            assert!(!state.abort());
            assert_eq!(state.begin(), Begin::Skip);
        }
    }
}
