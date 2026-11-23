//! A one-value channel with explicit sender and receiver closure.
//!
//! All payload and waker drops happen after releasing the channel lock. In
//! particular, a waker is user code and may re-enter this channel.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

struct Slot<T> {
    value: Option<T>,
    sender_alive: bool,
    receiver_alive: bool,
    waker: Option<Waker>,
}

struct Shared<T> {
    slot: Mutex<Slot<T>>,
}

/// The unique sending end. Dropping it closes an empty channel.
pub struct Sender<T> {
    shared: Option<Arc<Shared<T>>>,
}

/// The unique receiving end. Dropping it discards a delivered value.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    completed: bool,
}

/// The sender disappeared without delivering a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the one-shot sender closed without a value")
    }
}

impl std::error::Error for RecvError {}

/// Create a channel with space for exactly one value.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        slot: Mutex::new(Slot {
            value: None,
            sender_alive: true,
            receiver_alive: true,
            waker: None,
        }),
    });
    (
        Sender {
            shared: Some(shared.clone()),
        },
        Receiver {
            shared,
            completed: false,
        },
    )
}

impl<T> Sender<T> {
    /// Deliver a value, returning ownership if the receiver is already gone.
    pub fn send(mut self, value: T) -> Result<(), T> {
        let shared = self.shared.take().expect("a sender is consumed once");
        let (result, waker) = {
            let mut slot = lock(&shared.slot);
            slot.sender_alive = false;
            let result = if slot.receiver_alive {
                slot.value = Some(value);
                Ok(())
            } else {
                Err(value)
            };
            (result, slot.waker.take())
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        result
    }

    pub fn is_closed(&self) -> bool {
        self.shared
            .as_ref()
            .is_none_or(|shared| !lock(&shared.slot).receiver_alive)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let waker = {
                let mut slot = lock(&shared.slot);
                slot.sender_alive = false;
                slot.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl<T> Receiver<T> {
    /// True when polling can produce a value or a closure error.
    pub fn is_ready(&self) -> bool {
        if self.completed {
            return true;
        }
        let slot = lock(&self.shared.slot);
        slot.value.is_some() || !slot.sender_alive
    }
}

// No T is pinned in Receiver: the payload lives behind a separate Arc and
// mutex, and Receiver never offers a pinned reference to it.
impl<T> Unpin for Receiver<T> {}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(!this.completed, "one-shot receiver polled after completion");
        // Cloning or dropping a raw waker may run arbitrary user code.
        let mut incoming = Some(cx.waker().clone());
        let (result, old_waker) = {
            let mut slot = lock(&this.shared.slot);
            if let Some(value) = slot.value.take() {
                (Poll::Ready(Ok(value)), slot.waker.take())
            } else if !slot.sender_alive {
                (Poll::Ready(Err(RecvError)), slot.waker.take())
            } else {
                (
                    Poll::Pending,
                    std::mem::replace(&mut slot.waker, incoming.take()),
                )
            }
        };
        if result.is_ready() {
            this.completed = true;
        }
        drop(old_waker);
        drop(incoming);
        result
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (value, waker) = {
            let mut slot = lock(&self.shared.slot);
            slot.receiver_alive = false;
            (slot.value.take(), slot.waker.take())
        };
        drop(value);
        drop(waker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn send_before_poll_and_close_are_distinct() {
        let (tx, mut rx) = channel();
        tx.send(17).unwrap();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Ok(17)));
        assert!(rx.is_ready());

        let (tx, mut rx) = channel::<usize>();
        drop(tx);
        assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Err(RecvError)));
    }

    #[test]
    fn replacing_waker_only_notifies_current_waiter() {
        let (tx, mut rx) = channel();
        let first = Arc::new(Counter::default());
        let second = Arc::new(Counter::default());
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());
        assert!(
            Pin::new(&mut rx)
                .poll(&mut Context::from_waker(&first_waker))
                .is_pending()
        );
        assert!(
            Pin::new(&mut rx)
                .poll(&mut Context::from_waker(&second_waker))
                .is_pending()
        );
        tx.send(3).unwrap();
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            Pin::new(&mut rx).poll(&mut Context::from_waker(&second_waker)),
            Poll::Ready(Ok(3))
        );
    }

    #[test]
    fn sender_drop_wakes_waiter() {
        let (tx, mut rx) = channel::<usize>();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        assert!(
            Pin::new(&mut rx)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(tx);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(rx.is_ready());
    }

    #[test]
    fn detached_receiver_drops_each_payload_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = channel();
        assert!(tx.send(DropCount(count.clone())).is_ok());
        drop(rx);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let (tx, rx) = channel();
        drop(rx);
        let value = tx.send(DropCount(count.clone())).err().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(value);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cross_thread_delivery_preserves_value() {
        let (tx, mut rx) = channel();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        assert!(
            Pin::new(&mut rx)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        std::thread::spawn(move || tx.send(31).unwrap())
            .join()
            .unwrap();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            Pin::new(&mut rx).poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(31))
        );
    }
}
