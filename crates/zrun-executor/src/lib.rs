//! A bounded single-thread executor with cross-thread injection and wake.
//!
//! Only block_on polls tasks. Spawner and task wakers can be used on other
//! threads. A runtime admits 65,536 active tasks and timer entries by default;
//! with_max_tasks sets both limits. Registries and queues grow lazily to that bound.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::Duration;

use zrun_task::{JoinHandle, Scheduler, Task, TaskRef};
use zrun_time::{Driver, Timer};

mod parking;
mod pool;
use parking::ParkingFlag;
pub use pool::{Pool, PoolSpawner};

pub const DEFAULT_MAX_TASKS: usize = 65_536;
const TASK_TURN_BUDGET: usize = 64;
const READY_BATCH: usize = 16;
const INLINE_REPOLLS: usize = 2;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Admission failure. Rejected futures are dropped by the calling thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    InvalidCapacity,
    AtCapacity { capacity: usize },
    Shutdown,
    IdExhausted,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity => f.write_str("task capacity must be positive"),
            Self::AtCapacity { capacity } => {
                write!(f, "runtime task capacity of {capacity} is exhausted")
            }
            Self::Shutdown => f.write_str("runtime is closed"),
            Self::IdExhausted => f.write_str("task identity counter is exhausted"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// A bounded instantaneous observation; concurrent spawn/wake can change it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeStats {
    pub active_tasks: usize,
    pub queued_tasks: usize,
    pub timer_entries: usize,
}

#[derive(Default)]
struct Parker {
    runner: Mutex<Option<Runner>>,
    parking: ParkingFlag,
}

#[derive(Clone)]
enum Runner {
    Thread(Thread),
    #[cfg(feature = "io")]
    Io(thread::ThreadId, zrun_io::PollNotifier),
}

impl Runner {
    fn wake(&self) {
        match self {
            Self::Thread(thread) => thread.unpark(),
            // A caller-driven poll batch dispatches after returning from the
            // OS wait. Wakers invoked on this thread need no second signal.
            #[cfg(feature = "io")]
            Self::Io(owner, notifier) if *owner != thread::current().id() => {
                let _ = notifier.wake();
            }
            #[cfg(feature = "io")]
            Self::Io(..) => {}
        }
    }
}

impl Parker {
    fn notify(&self) {
        self.notify_waiter();
    }

    fn notify_waiter(&self) -> bool {
        if !self.parking.notify() {
            return false;
        }
        let runner = { lock(&self.runner).clone() };
        if let Some(runner) = runner {
            runner.wake();
        }
        true
    }

    fn register(self: &Arc<Self>) -> RunnerGuard {
        self.register_runner(Runner::Thread(thread::current()))
    }

    fn register_runner(self: &Arc<Self>, target: Runner) -> RunnerGuard {
        let mut runner = lock(&self.runner);
        assert!(runner.is_none(), "runtime already has a driving thread");
        *runner = Some(target);
        RunnerGuard(self.clone())
    }
}

struct RunnerGuard(Arc<Parker>);

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        // This also runs when the root future unwinds. Spawned-task panics are
        // caught by Task, while a root panic belongs to the block_on caller.
        self.0.parking.disarm();
        *lock(&self.0.runner) = None;
    }
}

struct RootSignal {
    notified: AtomicBool,
    parker: Arc<Parker>,
}

impl RootSignal {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        self.parker.notify();
    }
}

impl Wake for RootSignal {
    fn wake(self: Arc<Self>) {
        self.notify();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

struct SchedulerState {
    closed: bool,
    next_id: u64,
    tasks: Vec<TaskRef>,
    ready: VecDeque<TaskRef>,
}

struct Shared {
    state: Mutex<SchedulerState>,
    max_tasks: usize,
    parker: Arc<Parker>,
}

impl Shared {
    fn spawn<F>(self: &Arc<Self>, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Admission, registration, and initial enqueue are one transaction.
        // On rejection, future remains an argument and drops only after this
        // scoped guard has been released; its Drop may itself call spawn.
        let accepted = {
            let mut state = lock(&self.state);
            if state.closed {
                Err(SpawnError::Shutdown)
            } else if state.tasks.len() >= self.max_tasks {
                Err(SpawnError::AtCapacity {
                    capacity: self.max_tasks,
                })
            } else if let Some(next_id) = state.next_id.checked_add(1) {
                let id = state.next_id;
                state.next_id = next_id;
                let scheduler = Arc::downgrade(self);
                let (task, join) = Task::allocate(id, future, scheduler);
                task.set_registry_index(state.tasks.len());
                state.tasks.push(task.clone());
                state.ready.push_back(task);
                Ok(join)
            } else {
                Err(SpawnError::IdExhausted)
            }
        };
        if accepted.is_ok() {
            self.parker.notify();
        }
        accepted
    }

    fn refill(&self, batch: &mut Vec<TaskRef>) -> Option<TaskRef> {
        let mut state = lock(&self.state);
        let first = state.ready.pop_front()?;
        let count = state.ready.len().min(READY_BATCH - 1);
        // Store the FIFO prefix in reverse so the driver can pop from the end.
        batch.extend(state.ready.drain(..count).rev());
        Some(first)
    }

    fn has_ready(&self) -> bool {
        !lock(&self.state).ready.is_empty()
    }
}

impl Scheduler for Shared {
    fn enqueue(&self, task: TaskRef) {
        let queued = {
            let mut state = lock(&self.state);
            if !state.closed
                && state
                    .tasks
                    .get(task.registry_index())
                    .is_some_and(|registered| TaskRef::ptr_eq(registered, &task))
            {
                // Task's state serialized Idle/Running -> Queued and gives
                // exactly one caller this insertion responsibility.
                state.ready.push_back(task);
                true
            } else {
                false
            }
        };
        if queued {
            self.parker.notify();
        }
    }

    fn finished(&self, task: &Task) {
        let removed = { remove_task(&mut lock(&self.state).tasks, task) };
        // A task's destructor may call back into the runtime.
        drop(removed);
    }
}

fn remove_task(tasks: &mut Vec<TaskRef>, task: &Task) -> Option<TaskRef> {
    let index = task.registry_index();
    if !tasks
        .get(index)
        .is_some_and(|registered| std::ptr::eq(registered.as_ref(), task))
    {
        // Shutdown may already have detached the registry. Stale callbacks
        // cannot remove another task that has since moved into this slot.
        return None;
    }
    let removed = tasks.swap_remove(index);
    removed.set_registry_index(usize::MAX);
    if let Some(moved) = tasks.get(index) {
        moved.set_registry_index(index);
    }
    Some(removed)
}

/// Cross-thread admission handle. Old handles cannot retain or revive a runtime.
#[derive(Clone)]
pub struct Spawner {
    shared: Weak<Shared>,
}

impl Spawner {
    pub fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.shared.upgrade() {
            Some(shared) => shared.spawn(future),
            None => Err(SpawnError::Shutdown),
        }
    }
}

/// The P0 driver. Futures run only on the thread currently calling block_on.
///
/// Dropping Runtime performs shutdown. Cancellation and shutdown cannot preempt
/// a poll that never returns. A detached task stays admitted until it finishes,
/// is explicitly aborted, or the runtime closes.
pub struct Runtime {
    // A returning or unwinding root leaves prefetched tasks for the next drive.
    ready_batch: Vec<TaskRef>,
    shared: Arc<Shared>,
    driver: Driver,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self::with_max_tasks(DEFAULT_MAX_TASKS).expect("the default capacity is nonzero")
    }

    /// Set both active-task and pending-timer capacities. Zero is rejected.
    pub fn with_max_tasks(max_tasks: usize) -> Result<Self, SpawnError> {
        if max_tasks == 0 {
            return Err(SpawnError::InvalidCapacity);
        }
        let parker = Arc::new(Parker::default());
        let timer_parker = parker.clone();
        Ok(Self {
            ready_batch: Vec::new(),
            shared: Arc::new(Shared {
                state: Mutex::new(SchedulerState {
                    closed: false,
                    next_id: 0,
                    tasks: Vec::new(),
                    ready: VecDeque::new(),
                }),
                max_tasks,
                parker,
            }),
            driver: Driver::new(max_tasks, Arc::new(move || timer_parker.notify())),
        })
    }

    pub fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shared.spawn(future)
    }

    pub fn spawner(&self) -> Spawner {
        Spawner {
            shared: Arc::downgrade(&self.shared),
        }
    }

    pub fn timer(&self) -> Timer {
        self.driver.timer()
    }

    pub fn stats(&self) -> RuntimeStats {
        let (active_tasks, queued_tasks) = {
            let state = lock(&self.shared.state);
            (state.tasks.len(), state.ready.len())
        };
        RuntimeStats {
            active_tasks,
            queued_tasks: queued_tasks + self.ready_batch.len(),
            timer_entries: self.driver.len(),
        }
    }

    /// Drive a borrowed, possibly non-Send root future and all admitted tasks.
    ///
    /// Returning the root result leaves other tasks admitted for a later drive
    /// or shutdown. After shutdown this can still await already-resolved joins
    /// or an independent root future; spawn and timer admission remain closed.
    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        let _runner = self.shared.parker.register();
        self.drive(future, |timeout| {
            match timeout {
                Some(timeout) if timeout.is_zero() => {}
                Some(timeout) => thread::park_timeout(timeout),
                None => thread::park(),
            }
            Ok(())
        })
        .expect("thread parking is infallible")
    }

    /// Drive a root future, timers and sockets on the calling thread.
    ///
    /// The poller stays usable after this method returns. Sockets registered
    /// with it advance whenever the caller drives it again.
    ///
    /// # Errors
    ///
    /// Returns an OS error if polling the kernel fails.
    #[cfg(feature = "io")]
    pub fn block_on_with_io<F: Future>(
        &mut self,
        poller: &mut zrun_io::Poller,
        future: F,
    ) -> std::io::Result<F::Output> {
        let target = Runner::Io(thread::current().id(), poller.notifier());
        let _runner = self.shared.parker.register_runner(target);
        self.drive(future, |timeout| poller.poll(timeout))
    }

    fn drive<F: Future>(
        &mut self,
        future: F,
        mut wait: impl FnMut(Option<Duration>) -> std::io::Result<()>,
    ) -> std::io::Result<F::Output> {
        let signal = Arc::new(RootSignal {
            notified: AtomicBool::new(true),
            parker: self.shared.parker.clone(),
        });
        let waker = Waker::from(signal.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            self.driver.turn();
            if signal.notified.swap(false, Ordering::AcqRel)
                && let Poll::Ready(value) = future.as_mut().poll(&mut cx)
            {
                return Ok(value);
            }

            for _ in 0..TASK_TURN_BUDGET {
                let Some(task) = self
                    .ready_batch
                    .pop()
                    .or_else(|| self.shared.refill(&mut self.ready_batch))
                else {
                    break;
                };
                task.run_owned(INLINE_REPOLLS, |task| {
                    // This drive owns the runnable and cannot be asleep or
                    // shut down while inside its own poll loop.
                    lock(&self.shared.state).ready.push_back(task);
                });
                // Poll at least one available task even if the root wakes
                // itself on every poll; neither side can starve the other.
                if signal.notified.load(Ordering::Acquire) {
                    break;
                }
            }
            self.driver.turn();

            // Arm before inspecting every source of work. A notification that
            // precedes arm is acquired by the flag; one that follows it owes
            // this thread an unpark token, even if park has not started yet.
            self.shared.parker.parking.arm();
            if signal.notified.load(Ordering::Acquire)
                || !self.ready_batch.is_empty()
                || self.shared.has_ready()
            {
                self.shared.parker.parking.disarm();
                // During sustained runnable work, also collect any I/O that
                // has become ready, without blocking the next task turn.
                wait(Some(Duration::ZERO))?;
                continue;
            }
            let result = wait(self.driver.next_timeout());
            self.shared.parker.parking.disarm();
            result?;
            // Spurious returns and old unpark tokens are harmless: every turn
            // rechecks root readiness, task admission and timer deadlines.
        }
    }

    /// Stop admission, cancel live tasks, close timers, and release queue memory.
    pub fn shutdown(&mut self) {
        let (tasks, ready) = {
            let mut state = lock(&self.shared.state);
            if state.closed {
                return;
            }
            state.closed = true;
            (
                std::mem::take(&mut state.tasks),
                std::mem::take(&mut state.ready),
            )
        };
        // Do not hold scheduler state while completing timers or tasks:
        // their wakers and destructors can re-enter spawn and join code.
        self.driver.shutdown();
        for task in tasks {
            task.shutdown();
        }
        drop(ready);
        self.ready_batch = Vec::new();
        self.shared.parker.notify();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::future::{pending, poll_fn};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};
    use zrun_task::{JoinError, yield_now};
    use zrun_time::TimerError;

    async fn with_timeout<F: Future>(timer: Timer, future: F) -> F::Output {
        let mut future = pin!(future);
        // Miri interprets all 128 cross-thread handoffs. Keep the native
        // watchdog tight while allowing interpreter overhead, not fewer steps.
        let watchdog = if cfg!(miri) { 60 } else { 5 };
        let mut deadline = pin!(timer.sleep(Duration::from_secs(watchdog)));
        poll_fn(move |cx| {
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                return Poll::Ready(value);
            }
            assert!(
                deadline.as_mut().poll(cx).is_pending(),
                "test future did not finish before its watchdog"
            );
            Poll::Pending
        })
        .await
    }

    struct PendingDrop {
        drops: Arc<AtomicUsize>,
        drop_thread: Arc<Mutex<Option<ThreadId>>>,
        saved: Arc<Mutex<Option<Waker>>>,
    }

    impl Future for PendingDrop {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            *lock(&self.saved) = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl Drop for PendingDrop {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            *lock(&self.drop_thread) = Some(thread::current().id());
        }
    }

    type PendingDropFixture = (
        PendingDrop,
        Arc<AtomicUsize>,
        Arc<Mutex<Option<ThreadId>>>,
        Arc<Mutex<Option<Waker>>>,
    );

    fn pending_drop() -> PendingDropFixture {
        let drops = Arc::new(AtomicUsize::new(0));
        let drop_thread = Arc::new(Mutex::new(None));
        let saved = Arc::new(Mutex::new(None));
        (
            PendingDrop {
                drops: drops.clone(),
                drop_thread: drop_thread.clone(),
                saved: saved.clone(),
            },
            drops,
            drop_thread,
            saved,
        )
    }

    #[test]
    fn borrowed_non_send_root_and_spawn_join_yield() {
        let mut runtime = Runtime::new();
        let local = Rc::new(Cell::new(7));
        assert_eq!(
            runtime.block_on(async {
                yield_now().await;
                local.set(local.get() + 1);
                local.get()
            }),
            8
        );
        let joins: Vec<_> = (0..32)
            .map(|n| {
                runtime
                    .spawn(async move {
                        yield_now().await;
                        n
                    })
                    .unwrap()
            })
            .collect();
        let total = runtime.block_on(async move {
            let mut total = 0;
            for join in joins {
                total += join.await.unwrap();
            }
            total
        });
        assert_eq!(total, (0..32).sum::<usize>());
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn capacity_rejects_then_recovers_after_terminal_task() {
        assert!(matches!(
            Runtime::with_max_tasks(0),
            Err(SpawnError::InvalidCapacity)
        ));
        let mut runtime = Runtime::with_max_tasks(1).unwrap();
        let first = runtime.spawn(pending::<()>()).unwrap();
        assert!(matches!(
            runtime.spawn(async {}),
            Err(SpawnError::AtCapacity { capacity: 1 })
        ));
        first.abort();
        assert_eq!(runtime.block_on(first), Err(JoinError::Cancelled));
        let second = runtime.spawn(async { 23 }).unwrap();
        assert_eq!(runtime.block_on(second), Ok(23));
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn non_fifo_completion_keeps_moved_tasks_wakeable_and_capacity_reusable() {
        let mut runtime = Runtime::with_max_tasks(3).unwrap();
        let rounds = if cfg!(miri) { 4 } else { 64 };
        for _ in 0..rounds {
            let entered = Arc::new(AtomicUsize::new(0));
            let mut senders = Vec::new();
            let mut joins = Vec::new();
            for _ in 0..3 {
                let (sender, receiver) = zrun_sync::channel::<usize>();
                let entered = entered.clone();
                senders.push(Some(sender));
                joins.push(Some(
                    runtime
                        .spawn(async move {
                            entered.fetch_add(1, Ordering::Release);
                            receiver.await.unwrap()
                        })
                        .unwrap(),
                ));
            }
            runtime.block_on(with_timeout(
                runtime.timer(),
                poll_fn(|cx| {
                    if entered.load(Ordering::Acquire) == 3 {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }),
            ));

            // Retire a middle entry, then wake the task whose registration
            // moved. Its Future and result identity must remain unchanged.
            senders[1].take().unwrap().send(11).unwrap();
            assert_eq!(runtime.block_on(joins[1].take().unwrap()), Ok(11));
            let (replacement_sender, replacement_receiver) = zrun_sync::channel();
            let replacement = runtime
                .spawn(async move { replacement_receiver.await.unwrap() })
                .unwrap();
            assert!(matches!(
                runtime.spawn(async {}),
                Err(SpawnError::AtCapacity { capacity: 3 })
            ));

            senders[2].take().unwrap().send(12).unwrap();
            joins[0].as_ref().unwrap().abort();
            let (cancelled, completed) = runtime.block_on(async {
                (
                    joins[0].take().unwrap().await,
                    joins[2].take().unwrap().await,
                )
            });
            assert_eq!(cancelled, Err(JoinError::Cancelled));
            assert_eq!(completed, Ok(12));
            assert_eq!(senders[0].take().unwrap().send(10), Err(10));
            assert_eq!(runtime.stats().active_tasks, 1);

            // This pending replacement survives further non-last removals.
            replacement_sender.send(99).unwrap();
            assert_eq!(runtime.block_on(replacement), Ok(99));
            assert_eq!(runtime.stats(), RuntimeStats::default());
        }
    }

    #[test]
    fn old_wakers_do_not_wake_a_new_task_after_registry_reuse() {
        let mut runtime = Runtime::with_max_tasks(1).unwrap();
        let (first, first_drops, _, first_saved) = pending_drop();
        let first = runtime.spawn(first).unwrap();
        runtime.block_on(with_timeout(
            runtime.timer(),
            poll_fn(|cx| {
                if lock(&first_saved).is_some() {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }),
        ));
        let old_waker = lock(&first_saved).take().unwrap();
        first.abort();
        assert_eq!(runtime.block_on(first), Err(JoinError::Cancelled));
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);

        let (second, second_drops, _, second_saved) = pending_drop();
        let second = runtime.spawn(second).unwrap();
        runtime.block_on(with_timeout(
            runtime.timer(),
            poll_fn(|cx| {
                if lock(&second_saved).is_some() {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }),
        ));
        old_waker.wake_by_ref();
        old_waker.wake();
        assert_eq!(runtime.stats().queued_tasks, 0);
        assert_eq!(runtime.stats().active_tasks, 1);
        runtime.shutdown();
        assert_eq!(runtime.block_on(second), Err(JoinError::Shutdown));
        assert_eq!(second_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cross_thread_wake_runs_only_on_the_driving_thread() {
        struct External {
            ready: Arc<AtomicBool>,
            first: Option<mpsc::Sender<Waker>>,
        }
        impl Future for External {
            type Output = ThreadId;
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ThreadId> {
                if self.ready.load(Ordering::Acquire) {
                    Poll::Ready(thread::current().id())
                } else {
                    if let Some(first) = self.first.take() {
                        first.send(cx.waker().clone()).unwrap();
                    }
                    Poll::Pending
                }
            }
        }
        let mut runtime = Runtime::new();
        let ready = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let join = runtime
            .spawn(External {
                ready: ready.clone(),
                first: Some(tx),
            })
            .unwrap();
        let external = thread::spawn(move || {
            let waker = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            ready.store(true, Ordering::Release);
            for _ in 0..128 {
                waker.wake_by_ref();
            }
        });
        let timer = runtime.timer();
        let driven_on = runtime.block_on(with_timeout(timer, join)).unwrap();
        external.join().unwrap();
        assert_eq!(driven_on, thread::current().id());
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn repeated_cross_thread_injection_and_result_wake() {
        let mut runtime = Runtime::with_max_tasks(8).unwrap();
        let spawner = runtime.spawner();
        let (commands, receive) = mpsc::channel::<zrun_sync::Sender<JoinHandle<usize>>>();
        let worker = thread::spawn(move || {
            for (number, sender) in receive.into_iter().enumerate() {
                let join = spawner
                    .spawn(async move {
                        yield_now().await;
                        number
                    })
                    .unwrap();
                assert!(sender.send(join).is_ok());
            }
        });
        let timer = runtime.timer();
        runtime.block_on(with_timeout(timer, async {
            for number in 0..128 {
                let (tx, rx) = zrun_sync::channel();
                commands.send(tx).unwrap();
                assert_eq!(rx.await.unwrap().await.unwrap(), number);
            }
        }));
        drop(commands);
        worker.join().unwrap();
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn abort_from_other_thread_drops_future_on_driver_once() {
        for poll_before_abort in [false, true] {
            let mut runtime = Runtime::new();
            let (future, drops, drop_thread, saved) = pending_drop();
            let join = runtime.spawn(future).unwrap();
            if poll_before_abort {
                runtime.block_on(yield_now());
                assert!(lock(&saved).is_some());
            }
            let join = thread::spawn(move || {
                for _ in 0..128 {
                    join.abort();
                }
                join
            })
            .join()
            .unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.block_on(join), Err(JoinError::Cancelled));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(*lock(&drop_thread), Some(thread::current().id()));
            if let Some(waker) = lock(&saved).take() {
                waker.wake();
            }
            assert_eq!(runtime.stats(), RuntimeStats::default());
        }
    }

    #[test]
    fn panic_isolated_from_other_tasks_and_root_unwind_allows_next_drive() {
        let mut runtime = Runtime::new();
        let bad = runtime.spawn(async { panic!("isolated failure") }).unwrap();
        let good = runtime.spawn(async { 11 }).unwrap();
        assert!(runtime.block_on(bad).unwrap_err().is_panicked());
        assert_eq!(runtime.block_on(good), Ok(11));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on(async { panic!("root failure") })
            }))
            .is_err()
        );
        assert_eq!(runtime.block_on(async { 12 }), 12);
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn shutdown_clears_tasks_timers_and_stale_handles() {
        let mut runtime = Runtime::new();
        let spawner = runtime.spawner();
        let timer = runtime.timer();
        let (future, drops, _, saved) = pending_drop();
        let join = runtime.spawn(future).unwrap();
        let mut sleep = timer.sleep(Duration::from_secs(86_400));
        runtime.block_on(yield_now());
        assert_eq!(runtime.stats().active_tasks, 1);
        assert_eq!(runtime.stats().timer_entries, 1);
        runtime.shutdown();
        runtime.shutdown();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.block_on(join), Err(JoinError::Shutdown));
        assert_eq!(runtime.block_on(&mut sleep), Err(TimerError::Closed));
        assert!(matches!(spawner.spawn(async {}), Err(SpawnError::Shutdown)));
        assert_eq!(
            runtime.block_on(timer.sleep(Duration::ZERO)),
            Err(TimerError::Closed)
        );
        if let Some(waker) = lock(&saved).take() {
            waker.wake();
        }
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn runtime_drop_resolves_join_and_weak_spawner_does_not_keep_it_alive() {
        let mut runtime = Runtime::new();
        let spawner = runtime.spawner();
        let timer = runtime.timer();
        let (future, drops, _, saved) = pending_drop();
        let mut join = runtime.spawn(future).unwrap();
        runtime.block_on(yield_now());
        drop(runtime);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(join.is_finished());
        assert_eq!(
            Pin::new(&mut join).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(JoinError::Shutdown))
        );
        assert!(matches!(spawner.spawn(async {}), Err(SpawnError::Shutdown)));
        assert_eq!(
            Pin::new(&mut timer.sleep(Duration::ZERO))
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(TimerError::Closed))
        );
        if let Some(waker) = lock(&saved).take() {
            waker.wake();
        }
    }

    #[test]
    fn real_timer_progresses_amid_self_waking_tasks() {
        let mut runtime = Runtime::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let task_polls = polls.clone();
        let busy = runtime
            .spawn(poll_fn(move |cx| {
                task_polls.fetch_add(1, Ordering::Relaxed);
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            }))
            .unwrap();
        let timer = runtime.timer();
        runtime.block_on(yield_now());
        let duration = Duration::from_millis(2);
        let start = Instant::now();
        assert_eq!(runtime.block_on(timer.sleep(duration)), Ok(()));
        assert!(start.elapsed() >= duration);
        assert!(polls.load(Ordering::Relaxed) > 0);
        busy.abort();
        assert_eq!(runtime.block_on(busy), Err(JoinError::Cancelled));
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn self_waking_root_does_not_starve_a_spawned_task() {
        let mut runtime = Runtime::new();
        let ready = Arc::new(AtomicBool::new(false));
        let task_ready = ready.clone();
        let join = runtime
            .spawn(async move {
                task_ready.store(true, Ordering::Release);
            })
            .unwrap();
        let mut root_polls = 0;
        runtime.block_on(poll_fn(|cx| {
            root_polls += 1;
            assert!(root_polls < 100, "self-waking root starved a runnable task");
            if ready.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
        runtime.block_on(join).unwrap();
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn dropping_join_detaches_but_does_not_release_task_capacity_early() {
        let mut runtime = Runtime::with_max_tasks(1).unwrap();
        let complete = Arc::new(AtomicBool::new(false));
        let task_complete = complete.clone();
        let join = runtime
            .spawn(async move {
                yield_now().await;
                task_complete.store(true, Ordering::Release);
            })
            .unwrap();
        drop(join);
        assert_eq!(runtime.stats().active_tasks, 1);
        assert!(matches!(
            runtime.spawn(async {}),
            Err(SpawnError::AtCapacity { .. })
        ));
        runtime.block_on(async {
            while !complete.load(Ordering::Acquire) {
                yield_now().await;
            }
        });
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn public_handles_are_send_and_sync() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Spawner>();
        send_sync::<Timer>();
        send_sync::<JoinHandle<usize>>();
    }
    #[test]
    fn abort_arriving_inside_poll_cancels_after_unique_owner_returns() {
        struct InPoll {
            entered: Option<mpsc::Sender<()>>,
            resume: mpsc::Receiver<()>,
            drops: Arc<AtomicUsize>,
        }
        impl Future for InPoll {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                self.entered
                    .take()
                    .expect("only one poll")
                    .send(())
                    .unwrap();
                self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
                Poll::Pending
            }
        }
        impl Drop for InPoll {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut runtime = Runtime::new();
        let (entered, has_entered) = mpsc::channel();
        let (resume, can_resume) = mpsc::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let join = runtime
            .spawn(InPoll {
                entered: Some(entered),
                resume: can_resume,
                drops: drops.clone(),
            })
            .unwrap();
        let aborter = thread::spawn(move || {
            has_entered.recv_timeout(Duration::from_secs(5)).unwrap();
            join.abort();
            resume.send(()).unwrap();
            join
        });
        runtime.block_on(yield_now());
        let join = aborter.join().unwrap();
        assert_eq!(runtime.block_on(join), Err(JoinError::Cancelled));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.stats(), RuntimeStats::default());
    }

    #[test]
    fn rejected_and_shutdown_future_destructors_can_reenter_admission() {
        struct Reenter {
            spawner: Spawner,
            result: Arc<Mutex<Option<SpawnError>>>,
        }
        impl Future for Reenter {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                Poll::Pending
            }
        }
        impl Drop for Reenter {
            fn drop(&mut self) {
                let error = self
                    .spawner
                    .spawn(async {})
                    .err()
                    .expect("admission stays closed/full");
                *lock(&self.result) = Some(error);
            }
        }

        let mut runtime = Runtime::with_max_tasks(1).unwrap();
        let full = runtime.spawn(pending::<()>()).unwrap();
        let rejected_result = Arc::new(Mutex::new(None));
        assert!(matches!(
            runtime.spawn(Reenter {
                spawner: runtime.spawner(),
                result: rejected_result.clone(),
            }),
            Err(SpawnError::AtCapacity { capacity: 1 })
        ));
        assert_eq!(
            *lock(&rejected_result),
            Some(SpawnError::AtCapacity { capacity: 1 })
        );
        full.abort();
        assert_eq!(runtime.block_on(full), Err(JoinError::Cancelled));

        let shutdown_result = Arc::new(Mutex::new(None));
        let pending = runtime
            .spawn(Reenter {
                spawner: runtime.spawner(),
                result: shutdown_result.clone(),
            })
            .unwrap();
        runtime.shutdown();
        assert_eq!(*lock(&shutdown_result), Some(SpawnError::Shutdown));
        assert_eq!(runtime.block_on(pending), Err(JoinError::Shutdown));
    }
}
