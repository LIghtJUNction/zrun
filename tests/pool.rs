use std::collections::HashSet;
use std::future::pending;
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
