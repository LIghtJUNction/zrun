use std::collections::HashSet;
use std::future::pending;
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use zrun::{JoinError, Pool, Runtime, SpawnError, yield_now};

#[test]
fn local_work_can_be_stolen_by_all_other_workers() {
    let mut pool = Pool::new(4).expect("start four workers");
    let spawner = pool.spawner();
    let (visited, receiver) = mpsc::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let wait = gate.clone();
    let producer = pool
        .spawn(async move {
            // The producer enqueues from a worker, so all jobs start in one
            // local queue. Keep that worker occupied while peers steal.
            let jobs: Vec<_> = (0..3)
                .map(|_| {
                    let report = visited.clone();
                    let wait = wait.clone();
                    spawner
                        .spawn(async move {
                            report
                                .send(thread::current().id())
                                .expect("report worker identity");
                            park_until_done(&wait);
                        })
                        .expect("admit a local task")
                })
                .collect();
            park_until_done(&wait);
            for job in jobs {
                job.await.expect("finish the local task");
            }
        })
        .expect("admit the producer");

    let observed: Vec<_> = (0..3)
        .map(|_| receiver.recv_timeout(Duration::from_secs(5)))
        .collect();
    let (release, changed) = &*gate;
    *release.lock().expect("gate should not be poisoned") = true;
    changed.notify_all();
    pool.block_on(producer)
        .expect("producer and all stolen jobs complete");
    let worker_ids: HashSet<_> = observed
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("all three peer workers must begin");
    assert_eq!(worker_ids.len(), 3);
}

fn park_until_done(gate: &(Mutex<bool>, Condvar)) {
    let (release, changed) = gate;
    drop(
        changed
            .wait_while(release.lock().expect("lock gate"), |done| !*done)
            .expect("wait for the fixture release"),
    );
}

#[test]
fn timer_capacity_cancellation_and_shutdown() {
    let mut pool = Pool::with_max_tasks(4, 8).expect("start bounded pool");
    let timer = pool.timer();
    let spawner = pool.spawner();
    let mut tasks: Vec<_> = (0..8)
        .map(|_| pool.spawn(pending::<()>()).expect("admit pending task"))
        .collect();
    assert!(matches!(
        spawner.spawn(async {}),
        Err(SpawnError::AtCapacity { capacity: 8 })
    ));
    let task = tasks.pop().expect("pending task available");
    task.abort();
    assert!(matches!(pool.block_on(task), Err(JoinError::Cancelled)));
    let before = Instant::now();
    pool.block_on(timer.sleep(Duration::from_millis(10)))
        .expect("timer driven while root waits");
    assert!(before.elapsed() >= Duration::from_millis(10));
    pool.shutdown();
    for task in tasks {
        assert!(matches!(pool.block_on(task), Err(JoinError::Shutdown)));
    }
    assert_eq!(pool.stats().active_tasks, 0);
    assert_eq!(pool.stats().queued_tasks, 0);
    assert_eq!(pool.stats().timer_entries, 0);
    assert!(matches!(spawner.spawn(async {}), Err(SpawnError::Shutdown)));
}

#[test]
fn concurrent_admission_uses_all_slots_and_releases_cancelled_tasks() {
    const CAPACITY: usize = 19;
    const PRODUCERS: usize = 8;
    let mut pool = Pool::with_max_tasks(4, CAPACITY).expect("start bounded pool");
    let start = Barrier::new(PRODUCERS);
    let mut tasks = thread::scope(|scope| {
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let start = &start;
                let spawner = pool.spawner();
                scope.spawn(move || {
                    start.wait();
                    (0..CAPACITY)
                        .filter_map(|_| match spawner.spawn(pending::<()>()) {
                            Ok(task) => Some(task),
                            Err(SpawnError::AtCapacity { capacity: CAPACITY }) => None,
                            Err(error) => panic!("unexpected admission failure: {error}"),
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        producers
            .into_iter()
            .flat_map(|producer| producer.join().expect("producer finishes"))
            .collect::<Vec<_>>()
    });
    assert_eq!(tasks.len(), CAPACITY);
    assert_eq!(pool.stats().active_tasks, CAPACITY);

    let cancelled = tasks.pop().expect("one admitted task");
    cancelled.abort();
    assert_eq!(pool.block_on(cancelled), Err(JoinError::Cancelled));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match pool.spawn(pending::<()>()) {
            Ok(replacement) => {
                tasks.push(replacement);
                break;
            }
            Err(SpawnError::AtCapacity { .. }) if Instant::now() < deadline => thread::yield_now(),
            Err(error) => panic!("cancelled slot did not become reusable: {error}"),
        }
    }
    pool.shutdown();
    assert_eq!(pool.stats().active_tasks, 0);
    assert_eq!(pool.stats().queued_tasks, 0);
    for task in tasks {
        assert_eq!(pool.block_on(task), Err(JoinError::Shutdown));
    }
}

#[test]
fn shutdown_resolves_every_task_admitted_by_racing_producers() {
    const CAPACITY: usize = 67;
    const PRODUCERS: usize = 8;
    let mut pool = Pool::with_max_tasks(4, CAPACITY).expect("start bounded pool");
    let spawner = pool.spawner();
    let start = Barrier::new(PRODUCERS + 1);
    let tasks = thread::scope(|scope| {
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let spawner = spawner.clone();
                let start = &start;
                scope.spawn(move || {
                    let first = spawner
                        .spawn(pending::<()>())
                        .expect("admit an initial task");
                    let mut tasks = vec![first];
                    start.wait();
                    for _ in 0..CAPACITY {
                        match spawner.spawn(pending::<()>()) {
                            Ok(task) => tasks.push(task),
                            Err(SpawnError::Shutdown | SpawnError::AtCapacity { .. }) => break,
                            Err(error) => panic!("unexpected admission failure: {error}"),
                        }
                    }
                    tasks
                })
            })
            .collect();
        start.wait();
        pool.shutdown();
        producers
            .into_iter()
            .flat_map(|producer| producer.join().expect("producer finishes"))
            .collect::<Vec<_>>()
    });
    assert!((PRODUCERS..=CAPACITY).contains(&tasks.len()));
    assert_eq!(pool.stats().active_tasks, 0);
    assert_eq!(pool.stats().queued_tasks, 0);
    assert!(matches!(spawner.spawn(async {}), Err(SpawnError::Shutdown)));
    for task in tasks {
        assert!(
            task.is_finished(),
            "shutdown must resolve every admitted task"
        );
        assert_eq!(pool.block_on(task), Err(JoinError::Shutdown));
    }
}

#[test]
fn a_task_can_drop_its_pool_without_joining_its_own_thread() {
    let pool = Pool::new(2).expect("start workers");
    let spawner = pool.spawner();
    let done = spawner
        .spawn(async move {
            drop(pool);
            42
        })
        .expect("admit owner");
    assert_eq!(Runtime::new().block_on(done).expect("complete owner"), 42);
    assert!(matches!(spawner.spawn(async {}), Err(SpawnError::Shutdown)));
}

#[test]
fn multi_worker_wakes_and_joins_keep_identity() {
    for workers in [1, 2, 4, 8, 16] {
        let mut pool = Pool::new(workers).expect("start workers");
        let tasks: Vec<_> = (0..4_096)
            .map(|index| {
                pool.spawn(async move {
                    for _ in 0..8 {
                        yield_now().await;
                    }
                    index
                })
                .expect("admit yield task")
            })
            .collect();
        pool.block_on(async {
            for (index, task) in tasks.into_iter().enumerate() {
                assert_eq!(
                    task.await.expect("join yield task"),
                    index,
                    "workers={workers}"
                );
            }
        });
    }
}

#[test]
fn retained_runnable_stays_visible_and_can_be_aborted() {
    struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        fn new() -> Self {
            Self(Arc::new((Mutex::new(false), Condvar::new())))
        }

        fn release(&self) {
            let (released, changed) = &*self.0;
            *released.lock().expect("release the fixture") = true;
            changed.notify_all();
        }

        fn block_worker(&self, pool: &Pool) -> (zrun::JoinHandle<()>, mpsc::Receiver<()>) {
            let gate = self.0.clone();
            let (started, observer) = mpsc::channel();
            let task = pool
                .spawn(async move {
                    started.send(()).expect("report poll entry");
                    park_until_done(&gate);
                })
                .expect("admit a fixture task");
            (task, observer)
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            self.release();
        }
    }

    let mut pool = Pool::new(1).expect("start one worker");
    let initial = Gate::new();
    let (first, first_started) = initial.block_worker(&pool);
    first_started
        .recv_timeout(Duration::from_secs(5))
        .expect("occupy the sole worker so both submissions are queued");
    let pending = pool
        .spawn(async {
            loop {
                yield_now().await
            }
        })
        .expect("admit a repeatedly waking task");
    let blocked = Gate::new();
    let (second, second_started) = blocked.block_worker(&pool);
    initial.release();
    second_started
        .recv_timeout(Duration::from_secs(5))
        .expect("shared work gets a turn despite the retained runnable");
    assert_eq!(pool.stats().queued_tasks, 1);
    pending.abort();
    blocked.release();

    pool.block_on(first).expect("finish the first fixture");
    pool.block_on(second).expect("finish the second fixture");
    assert!(matches!(pool.block_on(pending), Err(JoinError::Cancelled)));
    pool.shutdown();
    assert_eq!(pool.stats().active_tasks, 0);
    assert_eq!(pool.stats().queued_tasks, 0);
}

#[test]
fn batch_admission_preserves_order_and_reports_capacity_per_task() {
    let mut pool = Pool::with_max_tasks(2, 32).unwrap();
    let jobs = pool.spawn_batch((0..20).map(std::future::ready));
    for (index, job) in jobs.into_iter().enumerate() {
        assert_eq!(pool.block_on(job.unwrap()), Ok(index));
    }

    let mut pool = Pool::with_max_tasks(2, 3).unwrap();
    let mut jobs = pool.spawner().spawn_batch((0..7).map(|index| async move {
        pending::<()>().await;
        index
    }));
    assert!(jobs[..3].iter().all(Result::is_ok));
    assert!(
        jobs[3..]
            .iter()
            .all(|job| matches!(job, Err(SpawnError::AtCapacity { capacity: 3 })))
    );
    for job in jobs.drain(..3).map(Result::unwrap) {
        job.abort();
        assert_eq!(pool.block_on(job), Err(JoinError::Cancelled));
    }
}

#[test]
fn batch_iterator_panic_still_publishes_admitted_tasks() {
    let pool = Pool::with_max_tasks(2, 3).unwrap();
    let (tx, rx) = mpsc::channel();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.spawn_batch((0..3).map(|index| {
            assert!(index < 2);
            let tx = tx.clone();
            async move { tx.send(index).unwrap() }
        }))
    }));
    assert!(result.is_err());
    let mut received = [
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
    ];
    received.sort_unstable();
    assert_eq!(received, [0, 1]);
}

#[test]
fn cancellation_storm_reclaims_armed_timers_and_preserves_live_deadlines() {
    const CANCELLED: usize = 4_096;
    const LIVE: usize = 32;
    for workers in [1, 4, 16] {
        let mut pool = Pool::with_max_tasks(workers, CANCELLED + LIVE).unwrap();
        let timer = pool.timer();
        for wave in 0..4 {
            let jobs: Vec<_> = pool
                .spawn_batch((0..CANCELLED).map(|_| {
                    let timer = timer.clone();
                    async move { timer.sleep(Duration::from_secs(600)).await }
                }))
                .into_iter()
                .map(Result::unwrap)
                .collect();

            let deadline = Instant::now() + Duration::from_secs(5);
            while pool.stats().timer_entries != CANCELLED {
                assert!(
                    Instant::now() < deadline,
                    "arming timers with {workers} workers, wave {wave}"
                );
                thread::yield_now();
            }

            let wait = Duration::from_millis(10);
            let live = pool.spawn_batch((0..LIVE).map(|index| {
                let timer = timer.clone();
                async move {
                    let started = Instant::now();
                    timer.sleep(wait).await.unwrap();
                    (index, started.elapsed())
                }
            }));
            thread::scope(|scope| {
                for shard in jobs.chunks(CANCELLED / 4) {
                    scope.spawn(move || {
                        for job in shard {
                            job.abort();
                        }
                    });
                }
            });

            pool.block_on(async {
                for job in jobs {
                    assert!(matches!(job.await, Err(JoinError::Cancelled)));
                }
                for (index, job) in live.into_iter().enumerate() {
                    let (actual, slept) = job.unwrap().await.unwrap();
                    assert_eq!(actual, index);
                    assert!(slept >= wait);
                }
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let stats = pool.stats();
                if stats.active_tasks == 0 && stats.queued_tasks == 0 && stats.timer_entries == 0 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "draining pool with {workers} workers, wave {wave}: {stats:?}"
                );
                thread::yield_now();
            }
        }
    }
}

#[test]
fn repeated_abort_drains_registered_timers_and_reuses_every_slot() {
    use std::future::{Future, poll_fn};
    use std::pin::pin;

    const CAPACITY: usize = 32;
    let mut pool = Pool::with_max_tasks(4, CAPACITY).unwrap();
    let timer = pool.timer();
    for _ in 0..32 {
        let (registered, ready) = mpsc::channel();
        let mut jobs = Vec::with_capacity(CAPACITY);
        for _ in 0..CAPACITY {
            let timer = timer.clone();
            let mut entered = Some(registered.clone());
            jobs.push(
                pool.spawn(async move {
                    let mut sleep = pin!(timer.sleep(Duration::from_secs(600)));
                    poll_fn(|cx| {
                        let result = sleep.as_mut().poll(cx);
                        if result.is_pending()
                            && let Some(entered) = entered.take()
                        {
                            entered.send(()).unwrap();
                        }
                        result
                    })
                    .await
                })
                .unwrap(),
            );
        }
        drop(registered);
        for _ in 0..CAPACITY {
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(pool.stats().timer_entries, CAPACITY);
        assert!(matches!(
            pool.spawn(async {}),
            Err(SpawnError::AtCapacity { .. })
        ));
        for job in &jobs {
            job.abort();
            job.abort();
        }
        pool.block_on(async {
            for job in jobs {
                assert_eq!(job.await, Err(JoinError::Cancelled));
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.stats().active_tasks != 0 {
            assert!(
                Instant::now() < deadline,
                "cancelled tasks kept admission slots"
            );
            thread::yield_now();
        }
        assert_eq!(pool.stats().timer_entries, 0);
        assert_eq!(pool.stats().queued_tasks, 0);
    }
}
