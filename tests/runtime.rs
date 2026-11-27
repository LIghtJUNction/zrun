use std::{
    cell::Cell,
    future::{Future, pending},
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};
use zrun::{Runtime, yield_now};

fn bounded(work: impl FnOnce() + Send + 'static) {
    let (send, receive) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        work();
        let _ = send.send(());
    });
    receive
        .recv_timeout(Duration::from_secs(15))
        .expect("runtime regression exceeded its deadline or panicked");
    worker.join().unwrap();
}

struct DropProbe(Arc<AtomicUsize>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn spawn_join_yield_capacity_and_slot_reuse() {
    bounded(|| {
        let mut runtime = Runtime::with_max_tasks(4096).unwrap();
        let mut joins = Vec::new();
        for value in 0_u64..4096 {
            joins.push(
                runtime
                    .spawn(async move {
                        yield_now().await;
                        value
                    })
                    .unwrap(),
            );
        }
        let sum = runtime.block_on(async {
            let mut sum = 0;
            for join in joins {
                sum += join.await.unwrap();
            }
            sum
        });
        assert_eq!(sum, 4095 * 4096 / 2);
        assert_eq!(runtime.stats().active_tasks, 0);
        let next = runtime.spawn(async { 7 }).unwrap();
        assert_eq!(runtime.block_on(next).unwrap(), 7);
    });
}

#[test]
fn rejected_spawn_and_shutdown_drop_each_future_once() {
    bounded(|| {
        let mut runtime = Runtime::with_max_tasks(1).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let first = DropProbe(dropped.clone());
        let join = runtime
            .spawn(async move {
                let _first = first;
                pending::<()>().await;
            })
            .unwrap();
        let second = DropProbe(dropped.clone());
        assert!(
            runtime
                .spawn(async move {
                    let _second = second;
                    pending::<()>().await;
                })
                .is_err()
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        runtime.shutdown();
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert!(Runtime::new().block_on(join).is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn explicit_abort_has_one_drop_and_one_terminal_result() {
    bounded(|| {
        let mut runtime = Runtime::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let probe = DropProbe(dropped.clone());
        let join = runtime
            .spawn(async move {
                let _probe = probe;
                pending::<()>().await;
            })
            .unwrap();
        join.abort();
        join.abort();
        assert!(runtime.block_on(join).is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.stats().active_tasks, 0);
    });
}

#[test]
fn task_panic_isolated_from_other_tasks() {
    bounded(|| {
        let mut runtime = Runtime::new();
        let failed = runtime
            .spawn(async { panic!("intentional P0 panic regression") })
            .unwrap();
        let healthy = runtime.spawn(async { 42 }).unwrap();
        let error = runtime.block_on(failed).unwrap_err();
        assert!(format!("{error:?}").to_lowercase().contains("panic"));
        assert_eq!(runtime.block_on(healthy).unwrap(), 42);
    });
}

struct ExternalWake {
    ready: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
    concurrent_polls: Arc<AtomicUsize>,
}
impl Future for ExternalWake {
    type Output = ();
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        assert_eq!(self.concurrent_polls.fetch_add(1, Ordering::SeqCst), 0);
        *self.waker.lock().unwrap() = Some(context.waker().clone());
        let ready = self.ready.load(Ordering::Acquire);
        assert_eq!(self.concurrent_polls.fetch_sub(1, Ordering::SeqCst), 1);
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[test]
fn cross_thread_wake_storm_and_stale_waker_are_safe() {
    bounded(|| {
        let mut runtime = Runtime::new();
        let ready = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None::<Waker>));
        let join = runtime
            .spawn(ExternalWake {
                ready: ready.clone(),
                waker: slot.clone(),
                concurrent_polls: Arc::new(AtomicUsize::new(0)),
            })
            .unwrap();
        let wake_thread = thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            let waker = loop {
                if let Some(waker) = slot.lock().unwrap().clone() {
                    break waker;
                }
                assert!(Instant::now() < until, "task never registered its waker");
                thread::yield_now();
            };
            ready.store(true, Ordering::Release);
            for _ in 0..10_000 {
                waker.wake_by_ref();
            }
            waker
        });
        runtime.block_on(join).unwrap();
        let stale = wake_thread.join().unwrap();
        runtime.shutdown();
        for _ in 0..1000 {
            stale.wake_by_ref();
        }
        drop(stale);
        assert_eq!(runtime.stats().active_tasks, 0);
        assert_eq!(runtime.stats().queued_tasks, 0);
    });
}

#[test]
fn closed_spawner_rejects_and_detached_tasks_are_shutdown() {
    bounded(|| {
        let mut runtime = Runtime::new();
        let spawner = runtime.spawner();
        let dropped = Arc::new(AtomicUsize::new(0));
        let probe = DropProbe(dropped.clone());
        let join = spawner
            .spawn(async move {
                let _probe = probe;
                pending::<()>().await;
            })
            .unwrap();
        drop(join);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        runtime.shutdown();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(spawner.spawn(async {}).is_err());
    });
}

#[test]
fn root_future_may_borrow_non_send_state() {
    let mut runtime = Runtime::new();
    let value = Rc::new(Cell::new(0));
    runtime.block_on(async {
        yield_now().await;
        value.set(42);
    });
    assert_eq!(value.get(), 42);
}

#[test]
fn timer_deadline_and_cancel_storm_do_not_accumulate_entries() {
    bounded(|| {
        let mut runtime = Runtime::with_max_tasks(512).unwrap();
        let timer = runtime.timer();
        let started = Instant::now();
        runtime
            .block_on(timer.sleep(Duration::from_millis(10)))
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(10));
        for _ in 0..10 {
            let mut joins = Vec::new();
            for _ in 0..256 {
                let timer = timer.clone();
                joins.push(
                    runtime
                        .spawn(async move { timer.sleep(Duration::from_secs(3600)).await })
                        .unwrap(),
                );
            }
            runtime
                .block_on(timer.sleep(Duration::from_millis(1)))
                .unwrap();
            for join in &joins {
                join.abort();
            }
            runtime.block_on(async {
                for join in joins {
                    assert!(join.await.is_err());
                }
            });
            assert_eq!(runtime.stats().active_tasks, 0);
            assert!(
                runtime.stats().timer_entries <= 257,
                "cancelled timers retained across waves"
            );
        }
        runtime.shutdown();
        assert_eq!(runtime.stats().timer_entries, 0);
    });
}

#[test]
#[ignore = "explicit million-task capacity and completion acceptance"]
fn one_million_tasks_have_no_loss() {
    let count = 1_000_000_u64;
    let mut runtime = Runtime::with_max_tasks(count as usize).unwrap();
    let joins: Vec<_> = (0..count)
        .map(|value| {
            runtime
                .spawn(async move {
                    yield_now().await;
                    value
                })
                .unwrap()
        })
        .collect();
    let sum = runtime.block_on(async {
        let mut sum = 0_u64;
        for join in joins {
            sum += join.await.unwrap();
        }
        sum
    });
    assert_eq!(sum, count * (count - 1) / 2);
    assert_eq!(runtime.stats().active_tasks, 0);
}

#[test]
fn queued_tasks_survive_early_roots_abort_and_shutdown() {
    bounded(|| {
        let mut runtime = Runtime::with_max_tasks(40).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        let first = runtime.spawn(async {}).unwrap();
        for index in 0..39 {
            let probe = DropProbe(drops.clone());
            tasks.push(
                runtime
                    .spawn(async move {
                        drop(probe);
                        index
                    })
                    .unwrap(),
            );
        }
        runtime.block_on(first).unwrap();
        assert_eq!(runtime.stats().queued_tasks, 39);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let cancelled = tasks.remove(0);
        cancelled.abort();
        assert_eq!(runtime.block_on(cancelled), Err(zrun::JoinError::Cancelled));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let next = tasks.remove(0);
        assert_eq!(runtime.block_on(next), Ok(1));
        assert_eq!(runtime.stats().queued_tasks, 37);

        runtime.shutdown();
        assert_eq!(runtime.stats().queued_tasks, 0);
        assert_eq!(runtime.stats().active_tasks, 0);
        for task in tasks {
            assert_eq!(runtime.block_on(task), Err(zrun::JoinError::Shutdown));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 39);
    });
}
