//! Multi-worker execution built on the same task ownership protocol as P0.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::thread::{self, JoinHandle as WorkerThread};

use zrun_task::{JoinHandle, Scheduler, Task, TaskRef};
use zrun_time::{Driver, Timer};

use crate::{
    DEFAULT_MAX_TASKS, INLINE_REPOLLS, Parker, Runtime, RuntimeStats, SpawnError, lock, remove_task,
};

const MAX_WORKERS: usize = 256;
const STEAL_LIMIT: usize = 8;
const SUBMISSION_CHUNK: usize = 8;
const RETAINED_LIMIT: usize = 4;
const RETAINED_TURNS: usize = 3;

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
    ready: Mutex<VecDeque<TaskRef>>,
    has_ready: AtomicBool,
    retained: AtomicUsize,
    parker: Arc<Parker>,
}

struct Active {
    next_id: u64,
    tasks: Vec<TaskRef>,
}

#[repr(align(128))]
struct RegistryShard {
    state: Mutex<Active>,
    capacity: usize,
}

struct SharedPool {
    closed: AtomicBool,
    worker_driven_io: bool,
    route: AtomicUsize,
    limit: usize,
    active: Box<[RegistryShard]>,
    workers: Box<[Worker]>,
    clock: Driver,
}

// An iterator or rejected future may panic after an earlier admission.
// Publish admitted tasks during unwinding as well as on normal return.
struct Submission<'a> {
    pool: &'a SharedPool,
    ready: &'a mut Vec<TaskRef>,
    owner: usize,
    local: bool,
}

impl Drop for Submission<'_> {
    fn drop(&mut self) {
        if !self.ready.is_empty() {
            self.pool
                .publish_tasks(self.ready.drain(..), self.owner, self.local, Arrival::Spawn);
        }
    }
}

impl SharedPool {
    fn spawn<F>(self: &Arc<Self>, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let caller = self.local_worker();
        let owner = caller
            .unwrap_or_else(|| self.route.fetch_add(1, Ordering::Relaxed) % self.workers.len());
        let preferred = owner % self.active.len();
        let (task, join) = self.admit(preferred, future)?;
        self.publish(task, owner, caller.is_some(), Arrival::Spawn);
        Ok(join)
    }

    fn spawn_batch<F>(
        self: &Arc<Self>,
        futures: impl IntoIterator<Item = F>,
    ) -> Vec<Result<JoinHandle<F::Output>, SpawnError>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut futures = futures.into_iter();
        let caller = self.local_worker();
        let mut results = Vec::new();
        let mut ready = Vec::with_capacity(SUBMISSION_CHUNK);
        while let Some(first) = futures.next() {
            let owner = caller
                .unwrap_or_else(|| self.route.fetch_add(1, Ordering::Relaxed) % self.workers.len());
            let preferred = owner % self.active.len();
            let submission = Submission {
                pool: self,
                ready: &mut ready,
                owner,
                local: caller.is_some(),
            };
            for future in std::iter::once(first).chain(futures.by_ref().take(SUBMISSION_CHUNK - 1))
            {
                results.push(self.admit(preferred, future).map(|(task, join)| {
                    submission.ready.push(task);
                    join
                }));
            }
        }
        results
    }

    fn admit<F>(
        self: &Arc<Self>,
        preferred: usize,
        future: F,
    ) -> Result<(TaskRef, JoinHandle<F::Output>), SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Keep the future outside the guards until admission is certain. A
        // rejected future's destructor can reenter this pool.
        let mut future = Some(future);
        let (first, rest) = self.active.split_at(preferred);
        let shards = rest.iter().chain(first);
        for shard in shards {
            let mut active = lock(&shard.state);
            if self.closed.load(Ordering::Acquire) {
                return Err(SpawnError::Shutdown);
            }
            if active.tasks.len() < shard.capacity {
                return self.register(&mut active, &mut future);
            }
        }

        // A vacancy may have moved during the first scan. Confirm a genuinely
        // full pool under a consistent snapshot. Other paths lock one shard
        // at a time, and concurrent snapshots lock in this same order.
        let mut states: Vec<_> = self.active.iter().map(|shard| lock(&shard.state)).collect();
        if self.closed.load(Ordering::Acquire) {
            return Err(SpawnError::Shutdown);
        }
        let Some((active, _)) = states
            .iter_mut()
            .zip(self.active.iter())
            .find(|(active, shard)| active.tasks.len() < shard.capacity)
        else {
            return Err(SpawnError::AtCapacity {
                capacity: self.limit,
            });
        };
        self.register(active, &mut future)
    }

    fn register<F>(
        self: &Arc<Self>,
        active: &mut Active,
        future: &mut Option<F>,
    ) -> Result<(TaskRef, JoinHandle<F::Output>), SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let id = active.next_id;
        // The low byte identifies the immutable registration shard, even if
        // stealing or a remote wake later runs this task on another worker.
        active.next_id = id
            .checked_add(MAX_WORKERS as u64)
            .ok_or(SpawnError::IdExhausted)?;
        let scheduler = Arc::downgrade(self);
        let future = future.take().expect("admission consumes the future once");
        let (task, join) = Task::allocate(id, future, scheduler);
        task.set_registry_index(active.tasks.len());
        active.tasks.push(task.clone());
        Ok((task, join))
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
        retained: &mut VecDeque<TaskRef>,
        retained_turns: &mut usize,
        allow_steal: bool,
    ) -> Option<TaskRef> {
        let worker = &self.workers[own];
        let pop_ready = || {
            let mut ready = lock(&worker.ready);
            let task = ready.pop_front();
            if self.worker_driven_io && task.is_some() && ready.is_empty() {
                worker.has_ready.store(false, Ordering::Relaxed);
            }
            task
        };
        let mut pop_retained = || {
            let task = retained.pop_front();
            if task.is_some() {
                worker.retained.store(retained.len(), Ordering::Relaxed);
            }
            task
        };
        if *retained_turns != 0
            && let Some(task) = pop_retained()
        {
            *retained_turns -= 1;
            return Some(task);
        }
        *retained_turns = RETAINED_TURNS;
        pop_ready()
            .or_else(pop_retained)
            .or_else(|| allow_steal.then(|| self.steal(own)).flatten())
    }

    fn steal(&self, own: usize) -> Option<TaskRef> {
        let mut batch = std::array::from_fn::<_, STEAL_LIMIT, _>(|_| None);
        let (before, own_and_after) = self.workers.split_at(own);
        for peer in own_and_after[1..].iter().chain(before) {
            // This is only a steal hint. The owner still locks its queue and
            // publishes/consumes wake tokens under the existing handshake.
            if self.worker_driven_io && own != 0 && !peer.has_ready.load(Ordering::Relaxed) {
                continue;
            }
            let first = {
                let mut victim = match peer.ready.try_lock() {
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
                if self.worker_driven_io && victim.is_empty() {
                    peer.has_ready.store(false, Ordering::Relaxed);
                }
                first
            };
            // Never hold both worker queue locks. Running the front item
            // before publishing another batch avoids a zero-progress steal.
            if batch[0].is_some() && !self.closed.load(Ordering::Acquire) {
                let worker = &self.workers[own];
                let mut local = lock(&worker.ready);
                if !self.closed.load(Ordering::Acquire) {
                    let was_empty = local.is_empty();
                    local.extend(batch.into_iter().flatten().rev());
                    if self.worker_driven_io && was_empty {
                        worker.has_ready.store(true, Ordering::Relaxed);
                    }
                    drop(local);
                    self.notify_peer(own);
                }
            }
            return Some(first);
        }
        None
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // An admission already holding a shard is collected before draining
        // it. A later admission under the same lock observes closed.
        let tasks: Vec<_> = self
            .active
            .iter()
            .map(|shard| std::mem::take(&mut lock(&shard.state).tasks))
            .collect();
        self.clock.shutdown();
        for worker in &self.workers {
            worker.parker.notify();
        }
        // Task::shutdown leaves an in-progress poll in the worker's ownership.
        // That worker resolves the cancellation once its poll returns.
        for task in tasks.into_iter().flatten() {
            task.shutdown();
        }
        for worker in &self.workers {
            let pending = std::mem::take(&mut *lock(&worker.ready));
            drop(pending);
        }
    }

    fn work(
        self: &Arc<Self>,
        own: usize,
        speculative_steal: bool,
        #[cfg(feature = "io")] mut poller: Option<zrun_io::Poller>,
    ) {
        let identity = Arc::as_ptr(self) as usize;
        CURRENT_WORKER.set(Some((identity, own)));
        let worker = &self.workers[own];
        #[cfg(feature = "io")]
        let _driver = match &poller {
            Some(poller) => worker
                .parker
                .register_runner(crate::Runner::Io(thread::current().id(), poller.notifier())),
            None => worker.parker.register(),
        };
        #[cfg(not(feature = "io"))]
        let _driver = worker.parker.register();
        #[cfg(feature = "io")]
        let mut io_budget = 32;
        let mut retained = VecDeque::new();
        let mut retained_turns = RETAINED_TURNS;
        while !self.closed.load(Ordering::Acquire) {
            // The first worker owns timer turns. Other workers can register
            // timers concurrently and notify that worker's parker.
            if own == 0 {
                self.clock.turn();
            }
            #[cfg(feature = "io")]
            if let Some(poller) = &mut poller {
                if io_budget == 0 {
                    if poller.poll(Some(std::time::Duration::ZERO)).is_err() {
                        self.close();
                        break;
                    }
                    io_budget = 32;
                }
                io_budget -= 1;
            }
            let task =
                match self.next_task(own, &mut retained, &mut retained_turns, speculative_steal) {
                    Some(task) => task,
                    None => {
                        worker.parker.parking.arm();
                        if self.closed.load(Ordering::Acquire) {
                            break;
                        }
                        let task = self.next_task(own, &mut retained, &mut retained_turns, true);
                        if task.is_none() {
                            #[cfg(feature = "io")]
                            if let Some(poller) = &mut poller {
                                let result = poller.poll(self.clock.next_timeout());
                                worker.parker.parking.disarm();
                                if result.is_err() {
                                    self.close();
                                    break;
                                }
                                io_budget = 32;
                                continue;
                            }
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
            task.run_owned(INLINE_REPOLLS, |task| {
                // Only continuations may be retained. Fresh arrivals keep a
                // turn after each bounded set of local polls.
                if retained.len() < RETAINED_LIMIT {
                    retained.push_back(task);
                    worker.retained.store(retained.len(), Ordering::Relaxed);
                } else {
                    self.publish(task, own, true, Arrival::Wake);
                }
            });
        }
        worker.retained.store(0, Ordering::Relaxed);
    }
}

impl SharedPool {
    fn schedule(&self, task: TaskRef, arrival: Arrival) {
        let caller = self.local_worker();
        let owner = caller
            .unwrap_or_else(|| self.route.fetch_add(1, Ordering::Relaxed) % self.workers.len());
        self.publish(task, owner, caller.is_some(), arrival);
    }

    fn publish(&self, task: TaskRef, owner: usize, local: bool, arrival: Arrival) {
        self.publish_tasks(std::iter::once(task), owner, local, arrival);
    }

    fn publish_tasks(
        &self,
        tasks: impl IntoIterator<Item = TaskRef>,
        owner: usize,
        local: bool,
        arrival: Arrival,
    ) {
        let worker = &self.workers[owner];
        let backlog = {
            let mut ready = lock(&worker.ready);
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let was_empty = ready.is_empty();
            ready.extend(tasks);
            if self.worker_driven_io && was_empty && !ready.is_empty() {
                worker.has_ready.store(true, Ordering::Relaxed);
            }
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
    fn enqueue(&self, task: TaskRef) {
        self.schedule(task, Arrival::Wake);
    }

    fn finished(&self, task: &Task) {
        let shard = &self.active[task.id() as u8 as usize];
        let removed = { remove_task(&mut lock(&shard.state).tasks, task) };
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
        Self::start(
            workers,
            max_tasks,
            #[cfg(feature = "io")]
            None,
        )
    }

    /// Start a worker-driven poller with the default active-task limit.
    ///
    /// # Errors
    ///
    /// Returns an invalid worker count, poller creation or worker creation error.
    #[cfg(feature = "io")]
    pub fn new_with_io(workers: usize) -> io::Result<(Self, zrun_io::Handle)> {
        Self::with_max_tasks_and_io(workers, DEFAULT_MAX_TASKS)
    }

    /// Start the pool with a poller driven by its first worker.
    ///
    /// The returned handle registers descriptors on that worker's poller.
    ///
    /// # Errors
    ///
    /// Returns an invalid capacity, poller creation or worker creation error.
    #[cfg(feature = "io")]
    pub fn with_max_tasks_and_io(
        workers: usize,
        max_tasks: usize,
    ) -> io::Result<(Self, zrun_io::Handle)> {
        let poller = zrun_io::Poller::new()?;
        let handle = poller.handle().clone();
        Ok((Self::start(workers, max_tasks, Some(poller))?, handle))
    }

    fn start(
        workers: usize,
        max_tasks: usize,
        #[cfg(feature = "io")] mut poller: Option<zrun_io::Poller>,
    ) -> io::Result<Self> {
        if !(1..=MAX_WORKERS).contains(&workers) || max_tasks == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pool needs between one and 256 workers and positive task capacity",
            ));
        }
        let queues: Box<[_]> = (0..workers).map(|_| Worker::default()).collect();
        let clock_worker = queues[0].parker.clone();
        let shards = workers.min(max_tasks);
        let active = (0..shards)
            .map(|index| RegistryShard {
                state: Mutex::new(Active {
                    next_id: index as u64,
                    tasks: Vec::new(),
                }),
                capacity: max_tasks / shards + usize::from(index < max_tasks % shards),
            })
            .collect();
        #[cfg(feature = "io")]
        let worker_driven_io = poller.is_some();
        #[cfg(not(feature = "io"))]
        let worker_driven_io = false;
        let shared = Arc::new(SharedPool {
            closed: AtomicBool::new(false),
            worker_driven_io,
            route: AtomicUsize::new(0),
            limit: max_tasks,
            active,
            workers: queues,
            clock: Driver::new(max_tasks, Arc::new(move || clock_worker.notify())),
        });
        let mut pool = Self {
            shared,
            workers: Vec::with_capacity(workers),
            root: Runtime::new(),
        };
        // In worker-driven I/O pools, helpers avoid optimistic peer scans
        // to reduce contention on the I/O worker's newly published work.
        // Every helper still scans after arming its park handshake.
        for index in 0..workers {
            let shared = pool.shared.clone();
            #[cfg(feature = "io")]
            let worker_poller = poller.take();
            let worker = thread::Builder::new()
                .name(format!("zrun-worker-{index}"))
                .spawn(move || {
                    shared.work(
                        index,
                        !worker_driven_io || index == 0,
                        #[cfg(feature = "io")]
                        worker_poller,
                    );
                })?;
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

    /// Admit futures in bounded chunks, publishing each chunk together.
    ///
    /// Results preserve input order. Each future is admitted independently.
    /// Jobs earlier in the batch may run before this method returns.
    pub fn spawn_batch<F>(
        &self,
        futures: impl IntoIterator<Item = F>,
    ) -> Vec<Result<JoinHandle<F::Output>, SpawnError>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shared.spawn_batch(futures)
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
            active_tasks: self
                .shared
                .active
                .iter()
                .map(|shard| lock(&shard.state).tasks.len())
                .sum(),
            queued_tasks: self
                .shared
                .workers
                .iter()
                .map(|worker| lock(&worker.ready).len() + worker.retained.load(Ordering::Relaxed))
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

    /// Admit futures in bounded chunks, returning one result per input.
    ///
    /// A dropped pool yields a shutdown error for every input.
    pub fn spawn_batch<F>(
        &self,
        futures: impl IntoIterator<Item = F>,
    ) -> Vec<Result<JoinHandle<F::Output>, SpawnError>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.0.upgrade() {
            Some(pool) => pool.spawn_batch(futures),
            None => futures
                .into_iter()
                .map(|_| Err(SpawnError::Shutdown))
                .collect(),
        }
    }
}
