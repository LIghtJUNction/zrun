//! Multi-worker execution built on the same task ownership protocol as P0.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::thread::{self, JoinHandle as WorkerThread};

use zrun_task::{JoinHandle, Scheduler, Task};
use zrun_time::{Driver, Timer};

use crate::{
    DEFAULT_MAX_TASKS, INLINE_REPOLLS, Parker, Runtime, RuntimeStats, SpawnError, lock, remove_task,
};

const MAX_WORKERS: usize = 256;
const STEAL_LIMIT: usize = 8;

enum Arrival {
    Spawn,
    Wake,
}

thread_local! {
    static CURRENT_WORKER: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

// Each worker writes its queue mutex and indices frequently. Keep neighboring
// queues on separate cache lines, including on machines with adjacent-line prefetch.
#[repr(align(128))]
#[derive(Default)]
struct Worker {
    ready: Mutex<VecDeque<Arc<Task>>>,
    retained: AtomicBool,
    parker: Arc<Parker>,
}

#[derive(Default)]
struct Active {
    next_id: u64,
    tasks: Vec<Arc<Task>>,
}

struct SharedPool {
    closed: AtomicBool,
    route: AtomicUsize,
    limit: usize,
    active: Mutex<Active>,
    workers: Box<[Worker]>,
    clock: Driver,
}

impl SharedPool {
    fn spawn<F>(self: &Arc<Self>, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (task, join) = {
            let mut active = lock(&self.active);
            if self.closed.load(Ordering::Acquire) {
                return Err(SpawnError::Shutdown);
            }
            if active.tasks.len() >= self.limit {
                return Err(SpawnError::AtCapacity {
                    capacity: self.limit,
                });
            }
            let id = active.next_id;
            active.next_id = id.checked_add(1).ok_or(SpawnError::IdExhausted)?;
            let scheduler: Arc<dyn Scheduler> = self.clone();
            let (task, join) = Task::new(id, future, Arc::downgrade(&scheduler));
            task.set_registry_index(active.tasks.len());
            active.tasks.push(task.clone());
            (task, join)
        };
        self.schedule(task, Arrival::Spawn);
        Ok(join)
    }

    fn local_worker(&self) -> Option<usize> {
        let identity = std::ptr::from_ref(self) as usize;
        CURRENT_WORKER
            .try_with(|cell| cell.get())
            .ok()
            .flatten()
            .filter(|(pool, _)| *pool == identity)
            .map(|(_, worker)| worker)
    }

    fn next_task(
        &self,
        own: usize,
        retained: &mut Option<Arc<Task>>,
        prefer_retained: bool,
    ) -> Option<Arc<Task>> {
        let worker = &self.workers[own];
        let pop_ready = || lock(&worker.ready).pop_front();
        let mut pop_retained = || {
            let task = retained.take();
            if task.is_some() {
                worker.retained.store(false, Ordering::Relaxed);
            }
            task
        };
        let task = if prefer_retained {
            pop_retained().or_else(pop_ready)
        } else {
            pop_ready().or_else(pop_retained)
        };
        task.or_else(|| self.steal(own))
    }

    fn steal(&self, own: usize) -> Option<Arc<Task>> {
        let mut batch = std::array::from_fn::<_, STEAL_LIMIT, _>(|_| None);
        let len = self.workers.len();
        for offset in 1..len {
            let peer = (own + offset) % len;
            let first = {
                let mut victim = match self.workers[peer].ready.try_lock() {
                    Ok(victim) => victim,
                    Err(TryLockError::WouldBlock) => continue,
                    Err(TryLockError::Poisoned(error)) => error.into_inner(),
                };
                let Some(first) = victim.pop_back() else {
                    continue;
                };
                let keep = victim.len() / 2;
                for entry in &mut batch[..keep.min(STEAL_LIMIT)] {
                    *entry = victim.pop_back();
                }
                first
            };
            // Never hold both worker queue locks. Running the front item
            // before publishing another batch avoids a zero-progress steal.
            if batch[0].is_some() && !self.closed.load(Ordering::Acquire) {
                let mut local = lock(&self.workers[own].ready);
                if !self.closed.load(Ordering::Acquire) {
                    local.extend(batch.into_iter().flatten().rev());
                    drop(local);
                    self.notify_peer(own);
                }
            }
            return Some(first);
        }
        None
    }

    fn close(&self) {
        let tasks = {
            let mut active = lock(&self.active);
            if self.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut active.tasks)
        };
        self.clock.shutdown();
        for worker in &self.workers {
            worker.parker.notify();
        }
        // Task::shutdown leaves an in-progress poll in the worker's ownership.
        // That worker resolves the cancellation once its poll returns.
        for task in tasks {
            task.shutdown();
        }
        for worker in &self.workers {
            let pending = std::mem::take(&mut *lock(&worker.ready));
            drop(pending);
        }
    }

    fn work(self: &Arc<Self>, own: usize) {
        let identity = Arc::as_ptr(self) as usize;
        CURRENT_WORKER.set(Some((identity, own)));
        let worker = &self.workers[own];
        let _driver = worker.parker.register();
        let mut retained = None;
        let mut prefer_retained = true;
        while !self.closed.load(Ordering::Acquire) {
            // The first worker owns timer turns. Other workers can register
            // timers concurrently and notify that worker's parker.
            if own == 0 {
                self.clock.turn();
            }
            let task = match self.next_task(own, &mut retained, prefer_retained) {
                Some(task) => task,
                None => {
                    worker.parker.parking.arm();
                    if self.closed.load(Ordering::Acquire) {
                        break;
                    }
                    let task = self.next_task(own, &mut retained, prefer_retained);
                    if task.is_none() {
                        if own == 0 {
                            match self.clock.next_timeout() {
                                Some(timeout) if timeout.is_zero() => {}
                                Some(timeout) => thread::park_timeout(timeout),
                                None => thread::park(),
                            }
                        } else {
                            thread::park();
                        }
                    }
                    worker.parker.parking.disarm();
                    let Some(task) = task else { continue };
                    task
                }
            };
            prefer_retained = !prefer_retained;
            task.run_owned(INLINE_REPOLLS, |task| {
                // The worker keeps at most one runnable that was already in
                // its poll. Alternate with shared work to service arrivals.
                if retained.is_none() {
                    retained = Some(task);
                    worker.retained.store(true, Ordering::Relaxed);
                } else {
                    self.publish(task, own, true, Arrival::Wake);
                }
            });
        }
        worker.retained.store(false, Ordering::Relaxed);
    }
}

impl SharedPool {
    fn schedule(&self, task: Arc<Task>, arrival: Arrival) {
        let caller = self.local_worker();
        let owner = caller
            .unwrap_or_else(|| self.route.fetch_add(1, Ordering::Relaxed) % self.workers.len());
        self.publish(task, owner, caller.is_some(), arrival);
    }

    fn publish(&self, task: Arc<Task>, owner: usize, local: bool, arrival: Arrival) {
        let worker = &self.workers[owner];
        let backlog = {
            let mut ready = lock(&worker.ready);
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            ready.push_back(task);
            ready.len()
        };
        // A worker publishing to its own queue cannot be parked. An external
        // producer still owes its owner the full arm/notify handshake.
        let woke_owner = !local && worker.parker.notify_waiter();
        if !woke_owner && (backlog > 1 || matches!(arrival, Arrival::Spawn)) {
            self.notify_peer(owner);
        }
    }

    fn notify_peer(&self, owner: usize) {
        let len = self.workers.len();
        for offset in 1..len {
            let peer = &self.workers[(owner + offset) % len].parker;
            // Peer stealing is opportunistic. Only claim an armed wait
            // rather than writing to running peers' parking cache lines.
            if peer.parking.is_armed() && peer.notify_waiter() {
                break;
            }
        }
    }
}

impl Scheduler for SharedPool {
    fn enqueue(&self, task: Arc<Task>) {
        self.schedule(task, Arrival::Wake);
    }

    fn finished(&self, task: &Task) {
        let removed = { remove_task(&mut lock(&self.active).tasks, task) };
        drop(removed);
    }
}

/// A bounded runtime with worker-local ready queues and bounded stealing.
///
/// Worker threads make progress independently of the caller. The root future
/// passed to [Self::block_on] is driven by the caller and may borrow its stack.
/// A worker has the usual cooperative Future contract: its poll must return.
pub struct Pool {
    shared: Arc<SharedPool>,
    workers: Vec<WorkerThread<()>>,
    root: Runtime,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("workers", &self.shared.workers.len())
            .field("capacity", &self.shared.limit)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Start worker threads with the default active-task limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid worker count or OS thread-creation failure.
    pub fn new(workers: usize) -> io::Result<Self> {
        Self::with_max_tasks(workers, DEFAULT_MAX_TASKS)
    }

    /// Start between one and 256 workers with a shared task and timer capacity.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid capacity or OS thread-creation failure.
    pub fn with_max_tasks(workers: usize, max_tasks: usize) -> io::Result<Self> {
        if !(1..=MAX_WORKERS).contains(&workers) || max_tasks == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pool needs between one and 256 workers and positive task capacity",
            ));
        }
        let queues: Box<[_]> = (0..workers).map(|_| Worker::default()).collect();
        let clock_worker = queues[0].parker.clone();
        let shared = Arc::new(SharedPool {
            closed: AtomicBool::new(false),
            route: AtomicUsize::new(0),
            limit: max_tasks,
            active: Mutex::new(Active::default()),
            workers: queues,
            clock: Driver::new(max_tasks, Arc::new(move || clock_worker.notify())),
        });
        let mut pool = Self {
            shared,
            workers: Vec::with_capacity(workers),
            root: Runtime::new(),
        };
        for index in 0..workers {
            let shared = pool.shared.clone();
            let worker = thread::Builder::new()
                .name(format!("zrun-worker-{index}"))
                .spawn(move || shared.work(index))?;
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    /// Admit a future to the shared worker pool.
    ///
    /// # Errors
    ///
    /// Returns an error when admission is closed or at capacity.
    pub fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shared.spawn(future)
    }

    /// Obtain a cross-thread handle for admission to this pool.
    pub fn spawner(&self) -> PoolSpawner {
        PoolSpawner(Arc::downgrade(&self.shared))
    }

    /// Obtain the timer driven by the pool.
    pub fn timer(&self) -> Timer {
        self.shared.clock.timer()
    }

    /// Drive a root future on the calling thread while pool workers run tasks.
    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        self.root.block_on(future)
    }

    /// Observe currently admitted tasks, queued tasks and outstanding timers.
    pub fn stats(&self) -> RuntimeStats {
        RuntimeStats {
            active_tasks: lock(&self.shared.active).tasks.len(),
            queued_tasks: self
                .shared
                .workers
                .iter()
                .map(|worker| {
                    lock(&worker.ready).len() + usize::from(worker.retained.load(Ordering::Relaxed))
                })
                .sum(),
            timer_entries: self.shared.clock.len(),
        }
    }

    /// Close admission and request termination of all admitted tasks.
    ///
    /// A caller outside the pool also joins the worker threads. When invoked
    /// by one of the pool's own workers, all workers exit after their current
    /// cooperative polls return.
    pub fn shutdown(&mut self) {
        self.shared.close();
        let current = thread::current().id();
        if self
            .workers
            .iter()
            .any(|thread| thread.thread().id() == current)
        {
            self.workers.clear();
        } else {
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A cross-thread admission handle that does not keep a closed pool alive.
#[derive(Clone, Debug)]
pub struct PoolSpawner(Weak<SharedPool>);

impl PoolSpawner {
    /// Admit a future to the pool.
    ///
    /// # Errors
    ///
    /// Returns an error when admission is closed or at capacity.
    pub fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.0.upgrade().ok_or(SpawnError::Shutdown)?.spawn(future)
    }
}
