//! Runnable ownership and notifications shared with the Loom model.

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicU8, Ordering};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicU8, Ordering};

use super::JoinError;

const NOTIFIED: u8 = 1;
const RUNNING: u8 = 2;
const COMPLETE: u8 = 4;
const ABORT: u8 = 8;
const SHUTDOWN: u8 = 16;
const PHASE: u8 = NOTIFIED | RUNNING | COMPLETE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CancelReason {
    Abort,
    Shutdown,
}

impl CancelReason {
    fn from_bits(bits: u8) -> Option<Self> {
        match bits & (ABORT | SHUTDOWN) {
            0 => None,
            ABORT => Some(Self::Abort),
            SHUTDOWN => Some(Self::Shutdown),
            _ => unreachable!("a task has at most one cancellation reason"),
        }
    }

    pub(super) fn error(self) -> JoinError {
        match self {
            Self::Abort => JoinError::Cancelled,
            Self::Shutdown => JoinError::Shutdown,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Begin {
    Skip,
    Poll,
    Cancel(CancelReason),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Pending {
    Idle,
    Requeue,
    Cancel(CancelReason),
}

/// Without RUNNING or COMPLETE, NOTIFIED is the unique runnable reservation.
/// During RUNNING, NOTIFIED requests another poll by the current driver.
pub(super) struct ScheduleState(AtomicU8);

impl ScheduleState {
    pub(super) fn new() -> Self {
        Self(AtomicU8::new(NOTIFIED))
    }

    /// Only a wake observing idle owns publication of a new runnable.
    pub(super) fn wake(&self) -> bool {
        // Even a repeated notification publishes the caller's preceding work.
        // Setting NOTIFIED after completion leaves the terminal bit intact.
        self.0.fetch_or(NOTIFIED, Ordering::AcqRel) & PHASE == 0
    }

    pub(super) fn abort(&self) -> bool {
        self.transition(|bits| {
            if bits & COMPLETE != 0 {
                return (bits, false);
            }
            let cancel = if CancelReason::from_bits(bits).is_none() {
                ABORT
            } else {
                0
            };
            (bits | cancel | NOTIFIED, bits & PHASE == 0)
        })
    }

    pub(super) fn begin(&self) -> Begin {
        self.transition(|bits| {
            if bits & PHASE != NOTIFIED {
                (bits, Begin::Skip)
            } else if let Some(cancel) = CancelReason::from_bits(bits) {
                (COMPLETE, Begin::Cancel(cancel))
            } else {
                (RUNNING, Begin::Poll)
            }
        })
    }

    /// Consume a notification and grant another poll while retaining ownership.
    pub(super) fn try_repoll(&self) -> bool {
        self.0
            .compare_exchange(
                RUNNING | NOTIFIED,
                RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Release the poll reservation after the future was restored to its slot.
    pub(super) fn pending(&self) -> Pending {
        self.transition(|bits| {
            debug_assert_ne!(bits & RUNNING, 0);
            if let Some(cancel) = CancelReason::from_bits(bits) {
                (COMPLETE, Pending::Cancel(cancel))
            } else if bits & NOTIFIED != 0 {
                (NOTIFIED, Pending::Requeue)
            } else {
                (0, Pending::Idle)
            }
        })
    }

    /// Return ownership to the shutdown caller only if a driver is absent.
    pub(super) fn shutdown(&self) -> Option<CancelReason> {
        self.transition(|bits| {
            if bits & COMPLETE != 0 {
                (bits, None)
            } else if bits & RUNNING != 0 {
                if CancelReason::from_bits(bits).is_none() {
                    (bits | SHUTDOWN, None)
                } else {
                    (bits, None)
                }
            } else {
                let cancel = CancelReason::from_bits(bits).unwrap_or(CancelReason::Shutdown);
                (COMPLETE, Some(cancel))
            }
        })
    }

    pub(super) fn complete(&self) {
        // Only the poll owner can publish Ready or a panic.
        self.0.store(COMPLETE, Ordering::Release);
    }

    pub(super) fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire) & COMPLETE != 0
    }

    fn transition<T>(&self, next: impl Fn(u8) -> (u8, T)) -> T {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            let (updated, result) = next(current);
            if updated == current {
                return result;
            }
            match self.0.compare_exchange_weak(
                current,
                updated,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return result,
                Err(observed) => current = observed,
            }
        }
    }
}

#[cfg(all(test, loom))]
mod model {
    use super::*;
    use loom::sync::atomic::AtomicBool;
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[test]
    fn model_wake_and_pending_publish_one_runnable() {
        loom::model(|| {
            let state = Arc::new(ScheduleState::new());
            assert_eq!(state.begin(), Begin::Poll);
            let producer = state.clone();
            let wake = thread::spawn(move || usize::from(producer.wake()));
            let producer = state.clone();
            let repeat = thread::spawn(move || usize::from(producer.wake()));

            let driver = usize::from(state.pending() == Pending::Requeue);
            assert_eq!(driver + wake.join().unwrap() + repeat.join().unwrap(), 1);
            assert_eq!(state.begin(), Begin::Poll);
            assert_eq!(state.begin(), Begin::Skip);
        });
    }

    #[test]
    fn model_inline_repoll_observes_the_wake_publication() {
        loom::model(|| {
            let state = Arc::new(ScheduleState::new());
            let payload = Arc::new(AtomicBool::new(false));
            assert_eq!(state.begin(), Begin::Poll);
            let producer = state.clone();
            let data = payload.clone();
            let wake = thread::spawn(move || {
                data.store(true, Ordering::Relaxed);
                assert!(!producer.wake());
            });
            if state.try_repoll() {
                assert!(payload.load(Ordering::Relaxed));
                assert_eq!(state.pending(), Pending::Idle);
            }
            wake.join().unwrap();
        });
    }

    #[test]
    fn model_abort_shutdown_and_pending_have_one_future_owner() {
        loom::model(|| {
            let state = Arc::new(ScheduleState::new());
            let slot = Arc::new(Mutex::new(Some(7)));
            assert_eq!(state.begin(), Begin::Poll);
            let future = slot.lock().unwrap().take().unwrap();

            let spawner = state.clone();
            let abort = thread::spawn(move || spawner.abort());
            let closing = state.clone();
            let storage = slot.clone();
            let shutdown = thread::spawn(move || {
                closing
                    .shutdown()
                    .map(|_| storage.lock().unwrap().take().unwrap())
            });
            let driver = {
                let mut slot = slot.lock().unwrap();
                *slot = Some(future);
                match state.pending() {
                    Pending::Cancel(_) => slot.take(),
                    Pending::Idle | Pending::Requeue => None,
                }
            };
            let _ = abort.join().unwrap();
            let terminal = shutdown.join().unwrap();
            assert_eq!(driver.into_iter().chain(terminal).collect::<Vec<_>>(), [7]);
            assert!(state.is_complete());
            assert!(slot.lock().unwrap().is_none());
            assert!(!state.wake());
            assert_eq!(state.begin(), Begin::Skip);
        });
    }
}
