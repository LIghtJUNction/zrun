//! The task references and join handle share one allocation.

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicUsize, Ordering, fence};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicUsize, Ordering, fence};

const JOIN: usize = 1;
const FINALIZER: usize = 2;
const TASK: usize = 4;

pub(super) struct References(AtomicUsize);

impl References {
    pub(super) fn new() -> Self {
        Self(AtomicUsize::new(TASK | JOIN | FINALIZER))
    }

    /// An existing runnable or waker keeps the count above zero.
    pub(super) fn retain_task(&self) {
        if self.0.fetch_add(TASK, Ordering::Relaxed) >= isize::MAX as usize {
            std::process::abort();
        }
    }

    /// The join handle anchors the allocation but cannot revive a dead task.
    pub(super) fn upgrade(&self) -> bool {
        let mut value = self.0.load(Ordering::Relaxed);
        while value >= TASK {
            if value >= isize::MAX as usize {
                std::process::abort();
            }
            match self.0.compare_exchange_weak(
                value,
                value + TASK,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => value = observed,
            }
        }
        false
    }

    /// The last task reference takes the finalizer reservation. The join
    /// handle may disappear concurrently, so this reservation keeps the
    /// allocation alive until the future and any shutdown result are settled.
    pub(super) fn release_task(&self) -> bool {
        if self.0.fetch_sub(TASK, Ordering::Release) >= 2 * TASK {
            return false;
        }
        fence(Ordering::Acquire);
        true
    }

    pub(super) fn release_finalizer(&self) -> bool {
        self.release_flag(FINALIZER)
    }

    pub(super) fn release_join(&self) -> bool {
        self.release_flag(JOIN)
    }

    fn release_flag(&self, bit: usize) -> bool {
        if self.0.fetch_sub(bit, Ordering::Release) != bit {
            return false;
        }
        fence(Ordering::Acquire);
        true
    }

    #[cfg(all(test, not(loom)))]
    pub(super) fn task_count(&self) -> usize {
        self.0.load(Ordering::Relaxed) / TASK
    }
}

#[cfg(all(test, loom))]
mod model {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::thread;

    use super::References;

    #[test]
    fn model_join_and_finalizer_free_once_after_drop() {
        loom::model(|| {
            let shared = Arc::new((References::new(), AtomicBool::new(false)));
            let join = shared.clone();
            let detached = thread::spawn(move || {
                let free = join.0.release_join();
                if free {
                    assert!(join.1.load(Ordering::Relaxed));
                }
                usize::from(free)
            });
            assert!(shared.0.release_task());
            shared.1.store(true, Ordering::Relaxed);
            let free = usize::from(shared.0.release_finalizer());
            assert_eq!(free + detached.join().unwrap(), 1);
        });
    }

    #[test]
    fn model_abort_upgrade_races_with_the_last_task_reference() {
        loom::model(|| {
            let shared = Arc::new((References::new(), AtomicUsize::new(0)));
            let join = shared.clone();
            let aborter = thread::spawn(move || {
                if join.0.upgrade() && join.0.release_task() {
                    join.1.fetch_add(1, Ordering::Relaxed);
                    assert!(!join.0.release_finalizer());
                }
            });
            if shared.0.release_task() {
                shared.1.fetch_add(1, Ordering::Relaxed);
                assert!(!shared.0.release_finalizer());
            }
            aborter.join().unwrap();
            assert_eq!(shared.1.load(Ordering::Relaxed), 1);
            assert!(!shared.0.upgrade());
            assert!(shared.0.release_join());
        });
    }
}
