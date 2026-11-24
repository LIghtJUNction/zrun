use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

#[derive(Debug)]
pub(crate) struct Budget {
    limit: usize,
    next_waiter: AtomicUsize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    used: usize,
    generation: u64,
    waiters: HashMap<usize, Waker>,
}

impl Budget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            next_waiter: AtomicUsize::new(0),
            state: Mutex::default(),
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state
            .lock()
            .expect("output budget poisoned")
            .generation
    }

    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        assert!(
            bytes <= self.limit,
            "a single response exceeds the output budget"
        );
        let mut state = self.state.lock().expect("output budget poisoned");
        if bytes > self.limit - state.used {
            return None;
        }
        state.used += bytes;
        Some(Reservation {
            budget: self.clone(),
            bytes,
        })
    }

    pub(crate) async fn changed(&self, generation: u64) {
        let id = self.next_waiter.fetch_add(1, Ordering::Relaxed);
        let _waiter = Waiter { budget: self, id };
        poll_fn(|cx| {
            let mut state = self.state.lock().expect("output budget poisoned");
            if state.generation != generation {
                return Poll::Ready(());
            }
            state.waiters.insert(id, cx.waker().clone());
            Poll::Pending
        })
        .await;
    }

    fn notify(&self, released: usize) {
        let waiters = {
            let mut state = self.state.lock().expect("output budget poisoned");
            state.used -= released;
            state.generation = state.generation.wrapping_add(1);
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters.into_values() {
            waiter.wake();
        }
    }
}

struct Waiter<'a> {
    budget: &'a Budget,
    id: usize,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let removed = self
            .budget
            .state
            .lock()
            .expect("output budget poisoned")
            .waiters
            .remove(&self.id);
        drop(removed);
    }
}

#[derive(Debug)]
pub(crate) struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Reservation {
    pub(crate) fn published(&self) {
        self.budget.notify(0);
    }

    pub(crate) fn split(&mut self, bytes: usize) -> Self {
        self.bytes -= bytes;
        Self {
            budget: self.budget.clone(),
            bytes,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.budget.notify(self.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::task::Context;

    #[test]
    fn releasing_encoded_bytes_notifies_waiters_and_reopens_capacity() {
        let budget = Budget::new(10);
        let mut reservation = budget.try_reserve(8).unwrap();
        let first = reservation.split(5);
        let generation = budget.generation();
        assert!(budget.try_reserve(3).is_none());

        let mut waiter = pin!(budget.changed(generation));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        drop(first);
        assert!(waiter.as_mut().poll(&mut cx).is_ready());
        assert!(budget.try_reserve(7).is_some());
    }

    #[test]
    fn publishing_an_encoding_wakes_a_group_that_can_reuse_it() {
        let budget = Budget::new(10);
        let reservation = budget.try_reserve(10).unwrap();
        let mut waiter = pin!(budget.changed(budget.generation()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        reservation.published();
        assert!(waiter.as_mut().poll(&mut cx).is_ready());
        assert!(budget.try_reserve(1).is_none());
    }
}
