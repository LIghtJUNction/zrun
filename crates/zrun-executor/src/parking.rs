//! Notification handshake shared by the native executor and its Loom models.

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(super) struct ParkingFlag {
    waiting: AtomicBool,
}

impl ParkingFlag {
    /// Must precede the driver's root, ready-queue and timer checks.
    pub(super) fn arm(&self) {
        // All mutations are acquire/release RMWs. A notification published
        // while active is therefore acquired before the next work checks,
        // including when an intervening disarm writes the same false value.
        self.waiting.swap(true, Ordering::AcqRel);
    }

    /// Cancel the wait after finding work, waking, timing out or unwinding.
    pub(super) fn disarm(&self) {
        self.waiting.swap(false, Ordering::AcqRel);
    }

    /// The caller first publishes work, then must unpark iff this returns true.
    pub(super) fn notify(&self) -> bool {
        // Exactly one notifier claims an armed wait. Clearing the flag only
        // coalesces later notifications; the winner still owes an unpark token.
        self.waiting.swap(false, Ordering::AcqRel)
    }
}

#[cfg(all(test, loom))]
mod tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    fn model_handshake(producers: usize, cancelled_arm: bool) {
        loom::model(move || {
            let flag = Arc::new(ParkingFlag::default());
            let ready = Arc::new(AtomicBool::new(false));
            let token = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::new();
            for _ in 0..producers {
                let flag = flag.clone();
                let ready = ready.clone();
                let token = token.clone();
                handles.push(thread::spawn(move || {
                    // Deliberately weaker than the real root's Release store
                    // and the scheduler/timer mutexes: the flag must acquire
                    // even this publication when it precedes arm.
                    ready.store(true, Ordering::Relaxed);
                    if flag.notify() {
                        // Model std::Thread's durable unpark token. Joining
                        // below observes delivery, even if the winner paused
                        // between claiming notification and calling unpark.
                        token.store(true, Ordering::Release);
                    }
                }));
            }
            if cancelled_arm {
                flag.arm();
                flag.disarm();
            }
            flag.arm();
            let would_park = !ready.load(Ordering::Acquire);
            for handle in handles {
                handle.join().unwrap();
            }
            if would_park {
                assert!(
                    token.load(Ordering::Acquire),
                    "published work lost its wake"
                );
            }
            flag.disarm();
        });
    }

    #[test]
    fn model_publication_before_or_after_arming_cannot_lose_wake() {
        model_handshake(1, false);
    }

    #[test]
    fn model_concurrent_notifiers_preserve_one_durable_token() {
        model_handshake(2, false);
    }

    #[test]
    fn model_cancelled_arm_preserves_notification_publication() {
        model_handshake(1, true);
    }
}
