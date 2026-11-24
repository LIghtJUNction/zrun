//! A task's atomic scheduling state also grants exclusive access to its future.

#[cfg(all(test, loom))]
use loom::cell::UnsafeCell;
#[cfg(not(all(test, loom)))]
use std::cell::UnsafeCell;

use crate::schedule::{Begin, CancelReason, Pending, ScheduleState};

pub(super) struct FutureSlot<T> {
    state: ScheduleState,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: the slot is read or written only by the winner of an atomic claim
// (begin or shutdown), or while that claim remains in RUNNING. A pending
// owner writes the slot before releasing RUNNING; the next claim acquires
// that publication. A terminal transition leaves its winner as the last
// accessor. Wakes and abort requests only touch the atomic state.
unsafe impl<T: Send> Sync for FutureSlot<T> {}

pub(super) enum Claim<'a, T> {
    Skip,
    Poll(OwnedPoll<'a, T>),
    Cancel(T, CancelReason),
}

pub(super) enum Suspend<T> {
    Idle,
    Requeue,
    Cancel(T, CancelReason),
}

/// The only owner of the running reservation and of the detached future.
pub(super) struct OwnedPoll<'a, T> {
    slot: &'a FutureSlot<T>,
    value: T,
}

impl<T> FutureSlot<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            state: ScheduleState::new(),
            value: UnsafeCell::new(Some(value)),
        }
    }

    pub(super) fn wake(&self) -> bool {
        self.state.wake()
    }

    pub(super) fn abort(&self) -> bool {
        self.state.abort()
    }

    pub(super) fn begin(&self) -> Claim<'_, T> {
        match self.state.begin() {
            Begin::Skip => Claim::Skip,
            Begin::Poll => Claim::Poll(OwnedPoll {
                slot: self,
                // SAFETY: begin acquired the slot and set RUNNING.
                value: unsafe { self.take() },
            }),
            Begin::Cancel(reason) => {
                // SAFETY: begin claimed the slot and made it terminal.
                Claim::Cancel(unsafe { self.take() }, reason)
            }
        }
    }

    pub(super) fn shutdown(&self) -> Option<(T, CancelReason)> {
        self.state.shutdown().map(|reason| {
            // SAFETY: only a shutdown that claimed a non-running task returns
            // a reason. It acquires the published future and makes it terminal.
            (unsafe { self.take() }, reason)
        })
    }

    /// The caller exclusively owns a slot containing one value.
    unsafe fn take(&self) -> T {
        self.with_mut(|ptr| {
            // SAFETY: the caller's claim excludes every other slot accessor.
            unsafe { (*ptr).take() }.expect("a claimed task owns one future")
        })
    }

    fn with_mut<R>(&self, f: impl FnOnce(*mut Option<T>) -> R) -> R {
        #[cfg(all(test, loom))]
        return self.value.with_mut(f);
        #[cfg(not(all(test, loom)))]
        f(self.value.get())
    }
}

impl<T> OwnedPoll<'_, T> {
    #[cfg(all(test, loom))]
    pub(super) fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    pub(super) fn try_repoll(&self) -> bool {
        self.slot.state.try_repoll()
    }

    pub(super) fn pending(self) -> Suspend<T> {
        let Self { slot, value } = self;
        slot.with_mut(|ptr| {
            // SAFETY: this token owns RUNNING and took the slot's value. No
            // other thread can access the empty slot until pending publishes it.
            unsafe { ptr.write(Some(value)) };
        });
        match slot.state.pending() {
            Pending::Idle => Suspend::Idle,
            Pending::Requeue => Suspend::Requeue,
            Pending::Cancel(reason) => {
                // SAFETY: pending made the task terminal under our reservation.
                Suspend::Cancel(unsafe { slot.take() }, reason)
            }
        }
    }

    pub(super) fn complete(self) -> T {
        self.slot.state.complete();
        self.value
    }
}

#[cfg(all(test, loom))]
mod model {
    use loom::sync::Arc;
    use loom::thread;

    use super::*;

    #[test]
    fn model_slot_pending_publishes_to_the_next_driver() {
        loom::model(|| {
            let slot = Arc::new(FutureSlot::new(1));
            let Claim::Poll(mut first) = slot.begin() else {
                panic!("claim the initial runnable");
            };
            let peer = slot.clone();
            let next = thread::spawn(move || {
                peer.wake();
                match peer.begin() {
                    Claim::Poll(poll) => Some(poll.complete()),
                    Claim::Skip => None,
                    Claim::Cancel(..) => panic!("no cancellation was requested"),
                }
            });
            *first.value_mut() = 42;
            first.pending();
            if let Some(value) = next.join().unwrap() {
                assert_eq!(value, 42);
            } else if let Claim::Poll(poll) = slot.begin() {
                assert_eq!(poll.complete(), 42);
            } else {
                panic!("pending and wake must leave one runnable");
            }
            assert!(matches!(slot.begin(), Claim::Skip));
        });
    }

    #[test]
    fn model_slot_pending_and_shutdown_extract_once() {
        loom::model(|| {
            let slot = Arc::new(FutureSlot::new(1));
            let Claim::Poll(mut first) = slot.begin() else {
                panic!("claim the initial runnable");
            };
            let peer = slot.clone();
            let close = thread::spawn(move || peer.shutdown());
            *first.value_mut() = 42;
            let current = match first.pending() {
                Suspend::Cancel(value, reason) => Some((value, reason)),
                Suspend::Idle => None,
                Suspend::Requeue => panic!("no waking thread"),
            };
            let closed = close.join().unwrap();
            assert_ne!(current.is_some(), closed.is_some());
            assert_eq!(current.or(closed), Some((42, CancelReason::Shutdown)));
            assert!(slot.shutdown().is_none());
        });
    }

    #[test]
    fn model_slot_abort_and_shutdown_extract_once() {
        loom::model(|| {
            let slot = Arc::new(FutureSlot::new(42));
            let peer = slot.clone();
            let close = thread::spawn(move || peer.shutdown());
            slot.abort();
            let current = match slot.begin() {
                Claim::Cancel(value, reason) => Some((value, reason)),
                Claim::Skip => None,
                Claim::Poll(_) => panic!("abort must prevent the first poll"),
            };
            let closed = close.join().unwrap();
            assert_ne!(current.is_some(), closed.is_some());
            assert_eq!(current.or(closed).unwrap().0, 42);
        });
    }
}
