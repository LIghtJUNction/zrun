//! Task ownership, join results, cancellation, and the task RawWaker.
//!
//! Atomic scheduling state grants the unique poll or shutdown reservation.
//! The future slot is locked only when ownership transfers. A poll owner
//! retains the future across inline repolls and restores it before publishing
//! Idle or Queued. User code and scheduler callbacks run outside the slot lock.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

mod schedule;

use schedule::{Begin, Pending, ScheduleState};

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
    result: Arc<JoinCell<T>>,
    task: Weak<Task>,
}

impl<T> JoinHandle<T> {
    /// Request cancellation. The driver processes it on its next task turn.
    ///
    /// Cancellation cannot interrupt a poll that does not return. If that poll
    /// completes, its already-produced result wins over the abort request.
    pub fn abort(&self) {
        if let Some(task) = self.task.upgrade() {
            task.request_abort();
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            *lock(&self.result.state),
            JoinState::Ready(_) | JoinState::Taken
        )
    }
}

// The result sits behind an Arc and a mutex. Pinning a handle never pins T.
impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Waker cloning and drops may call user code.
        let incoming = cx.waker().clone();
        let old = {
            let mut state = lock(&self.result.state);
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
            let mut state = lock(&self.result.state);
            std::mem::replace(&mut *state, JoinState::Detached)
        };
        drop(discarded);
    }
}

trait Completion: Send + Sync {
    fn fail(&self, error: JoinError);
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

impl<T: Send + 'static> Completion for JoinCell<T> {
    fn fail(&self, error: JoinError) {
        self.finish(Err(error));
    }
}

/// Internal executor integration. Implementations must not poll in enqueue.
#[doc(hidden)]
pub trait Scheduler: Send + Sync {
    fn enqueue(&self, task: Arc<Task>);
    fn finished(&self, task: &Task);
}

type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Internal task allocation. Only the executor drives run_once and shutdown.
#[doc(hidden)]
pub struct Task {
    id: u64,
    registry_index: AtomicUsize,
    schedule: ScheduleState,
    future: Mutex<Option<TaskFuture>>,
    scheduler: Weak<dyn Scheduler>,
    completion: Arc<dyn Completion>,
}

impl Task {
    /// Create an initially queued task. The caller must atomically register it
    /// and insert exactly one initial runnable before exposing the join handle.
    pub fn new<F>(
        id: u64,
        future: F,
        scheduler: Weak<dyn Scheduler>,
    ) -> (Arc<Self>, JoinHandle<F::Output>)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let completion = Arc::new(JoinCell {
            state: Mutex::new(JoinState::Waiting(None)),
        });
        let success = completion.clone();
        let wrapped = async move {
            let output = future.await;
            success.finish(Ok(output));
        };
        let task = Arc::new(Self {
            id,
            registry_index: AtomicUsize::new(usize::MAX),
            schedule: ScheduleState::new(),
            future: Mutex::new(Some(Box::pin(wrapped))),
            scheduler,
            completion: completion.clone(),
        });
        let handle = JoinHandle {
            result: completion,
            task: Arc::downgrade(&task),
        };
        (task, handle)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// A scheduler-owned lookup hint, not the task's identity. The scheduler
    /// reads and changes it under its registry lock and checks the allocation
    /// at that index before removal or enqueue. Moving a registry entry never
    /// moves this Task allocation or changes its monotonically assigned id.
    pub fn registry_index(&self) -> usize {
        self.registry_index.load(Ordering::Relaxed)
    }

    /// Update the hint while holding the owning scheduler's registry lock.
    /// That lock publishes entries and moves; this atomic only makes storage
    /// thread-safe and does not synchronize future ownership or scheduling.
    pub fn set_registry_index(&self, index: usize) {
        self.registry_index.store(index, Ordering::Relaxed);
    }

    fn schedule(self: &Arc<Self>) {
        if self.schedule.wake() {
            self.enqueue();
        }
    }

    fn enqueue(self: &Arc<Self>) {
        if let Some(scheduler) = self.scheduler.upgrade() {
            scheduler.enqueue(self.clone());
        }
    }

    fn request_abort(self: &Arc<Self>) {
        if self.schedule.abort() {
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
    pub fn run_once(self: &Arc<Self>) {
        self.run_budget(0);
    }

    /// Poll and immediately drive a bounded number of notifications observed
    /// during those polls. The driver keeps the future and its waker across
    /// the inline polls. When the budget expires, it publishes one runnable.
    pub fn run_budget(self: &Arc<Self>, inline_repolls: usize) {
        if self.poll_budget(inline_repolls) {
            self.enqueue();
        }
    }

    /// Drive an owned runnable taken from its scheduler. When another runnable
    /// is due, `requeue` must return it to that scheduler unless it has closed.
    /// The callback owns the only publication for this transition.
    pub fn run_owned(self: Arc<Self>, inline_repolls: usize, requeue: impl FnOnce(Arc<Self>)) {
        if self.poll_budget(inline_repolls) {
            requeue(self);
        }
    }

    fn poll_budget(self: &Arc<Self>, mut inline_repolls: usize) -> bool {
        let cancelled = match self.schedule.begin() {
            Begin::Skip => return false,
            Begin::Poll => None,
            Begin::Cancel(reason) => Some(reason),
        };
        let mut future = lock(&self.future)
            .take()
            .expect("a runnable owns one future");

        if let Some(reason) = cancelled {
            self.finish_failure(future, reason.error());
            return false;
        }

        let waker = task_waker(self.clone());
        let mut cx = Context::from_waker(&waker);
        loop {
            let result = catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx)));
            match result {
                Ok(Poll::Pending) => {
                    if inline_repolls != 0 && self.schedule.try_repoll() {
                        inline_repolls -= 1;
                        continue;
                    }
                    let mut slot = lock(&self.future);
                    *slot = Some(future);
                    match self.schedule.pending() {
                        Pending::Idle => return false,
                        Pending::Requeue => return true,
                        Pending::Cancel(reason) => {
                            let future = slot.take().expect("poll owner returned its future");
                            drop(slot);
                            self.finish_failure(future, reason.error());
                            return false;
                        }
                    }
                }
                Ok(Poll::Ready(())) => {
                    self.schedule.complete();
                    // Success was published by the wrapper. A destructor panic
                    // cannot replace that already delivered result.
                    let _ = drop_future(future);
                    self.finished();
                    return false;
                }
                Err(payload) => {
                    self.schedule.complete();
                    self.finish_failure(future, JoinError::Panicked(panic_message(payload)));
                    return false;
                }
            }
        }
    }

    fn finish_failure(&self, future: TaskFuture, error: JoinError) {
        // A panicking destructor is caught after the future has been removed
        // from task state. Cancellation/shutdown retain their original reason.
        let _ = drop_future(future);
        // A foreign join waker may panic. Lifecycle cleanup still has to retire
        // the task; delivery already consumed the channel's unique sender.
        let _ = catch_unwind(AssertUnwindSafe(|| self.completion.fail(error)));
        self.finished();
    }

    /// Terminate an unpolled/idle task on the driver thread. A concurrent poll
    /// keeps its ownership and observes Shutdown when it next returns Pending.
    pub fn shutdown(&self) {
        if let Some(reason) = self.schedule.shutdown() {
            let future = lock(&self.future).take().expect("shutdown owns one future");
            self.finish_failure(future, reason.error());
        }
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

fn drop_future(future: TaskFuture) -> Result<(), Box<dyn Any + Send>> {
    catch_unwind(AssertUnwindSafe(|| drop(future)))
}

// Every data pointer represents one live strong Arc<Task>. Task is Send + Sync
// through a mutex containing a Send future; scheduler/completion are Send +
// Sync too. RawWaker operations therefore work from any waking thread.
//
// into_raw creates the exact ownership consumed by wake/drop. clone creates a
// further count for its returned RawWaker; wake_by_ref does not consume one.
const TASK_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);

impl Drop for Task {
    fn drop(&mut self) {
        // Executors explicitly resolve registered tasks. This path also closes
        // a task whose initial runnable was dropped before registration.
        if !self.schedule.is_complete() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.completion.fail(JoinError::Shutdown)
            }));
        }
    }
}

fn task_waker(task: Arc<Task>) -> Waker {
    let raw = RawWaker::new(Arc::into_raw(task).cast(), &TASK_WAKER_VTABLE);
    // SAFETY: the pointer came from a live Arc<Task>; the vtable below preserves
    // its strong ownership, thread safety, and the required consuming/borrowing
    // contracts. No other code reconstructs this particular raw ownership.
    unsafe { Waker::from_raw(raw) }
}

unsafe fn raw_clone(data: *const ()) -> RawWaker {
    // SAFETY: the caller's live RawWaker owns a strong count for this allocation,
    // so the allocation stays alive during increment. The added count belongs
    // exclusively to the returned RawWaker and is consumed by its wake/drop.
    unsafe { Arc::<Task>::increment_strong_count(data.cast()) };
    RawWaker::new(data, &TASK_WAKER_VTABLE)
}

unsafe fn raw_wake(data: *const ()) {
    // SAFETY: wake consumes the incoming RawWaker's one count from into_raw or
    // raw_clone. This Arc's destructor releases that count once, after schedule.
    let task = unsafe { Arc::<Task>::from_raw(data.cast()) };
    task.schedule();
}

unsafe fn raw_wake_by_ref(data: *const ()) {
    // SAFETY: this is a temporary view of the caller's count, not a transfer.
    // ManuallyDrop keeps that count intact on return or unwinding. schedule can
    // clone a separate Arc for a new runnable while the caller keeps Task alive.
    let task = ManuallyDrop::new(unsafe { Arc::<Task>::from_raw(data.cast()) });
    task.schedule();
}

unsafe fn raw_drop(data: *const ()) {
    // SAFETY: drop consumes exactly the one count represented by this RawWaker.
    // Any other live waker, runnable, or registry entry owns its own Arc count.
    drop(unsafe { Arc::<Task>::from_raw(data.cast()) });
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
    use crate::schedule::CancelReason;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestScheduler {
        queue: Mutex<VecDeque<Arc<Task>>>,
        finished: AtomicUsize,
    }

    impl Scheduler for TestScheduler {
        fn enqueue(&self, task: Arc<Task>) {
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

    fn setup<F>(future: F) -> (Arc<TestScheduler>, Arc<Task>, JoinHandle<F::Output>)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let scheduler = Arc::new(TestScheduler::default());
        let erased: Arc<dyn Scheduler> = scheduler.clone();
        let (task, join) = Task::new(1, future, Arc::downgrade(&erased));
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
        let weak = Arc::downgrade(&task);
        let initial = Arc::strong_count(&task);
        let waker = task_waker(task.clone());
        assert_eq!(Arc::strong_count(&task), initial + 1);
        let cloned = waker.clone();
        assert_eq!(Arc::strong_count(&task), initial + 2);
        waker.wake_by_ref();
        assert_eq!(Arc::strong_count(&task), initial + 2);
        cloned.wake();
        assert_eq!(Arc::strong_count(&task), initial + 1);
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
        assert!(weak.upgrade().is_some());
        drop(waker); // The final owning reference is released by raw_drop.
        assert!(weak.upgrade().is_none());
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
        assert!(Arc::ptr_eq(
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
        let holder: Arc<Mutex<Option<Weak<Task>>>> = Arc::new(Mutex::new(None));
        let inner = holder.clone();
        let mut polls = 0;
        let (scheduler, task, mut join) = setup(std::future::poll_fn(move |cx| {
            polls += 1;
            if polls == 2 {
                let task = lock(&inner).as_ref().unwrap().upgrade().unwrap();
                task.shutdown();
            }
            assert!(polls <= 2);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }));
        *lock(&holder) = Some(Arc::downgrade(&task));
        assert!(Arc::ptr_eq(
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
        let holder: Arc<Mutex<Option<Weak<Task>>>> = Arc::new(Mutex::new(None));
        let inner = holder.clone();
        let (scheduler, task, mut join) = setup(async move {
            let task = lock(&inner).as_ref().unwrap().upgrade().unwrap();
            task.request_abort();
            19
        });
        *lock(&holder) = Some(Arc::downgrade(&task));
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
    fn dropping_an_unregistered_task_completes_its_join() {
        let scheduler: Arc<dyn Scheduler> = Arc::new(TestScheduler::default());
        let (task, mut join) =
            Task::new(1, std::future::pending::<()>(), Arc::downgrade(&scheduler));
        assert!(poll_join(&mut join).is_pending());
        drop(task);
        assert!(matches!(
            poll_join(&mut join),
            Poll::Ready(Err(JoinError::Shutdown))
        ));
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
