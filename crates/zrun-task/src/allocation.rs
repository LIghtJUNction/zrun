//! Thin references to a pinned, type-erased task allocation.

use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::references::References;
use crate::slot::FutureSlot;
use crate::{JoinCell, JoinError, JoinHandle, JoinState, Scheduler, Task};

// The raw bytes of an inline future remain reserved while stale wakers live.
// Keep large and unusually aligned futures separately allocated so completion
// promptly releases their storage even when stale references are retained.
const INLINE_FUTURE_BYTES: usize = 128;

#[repr(C)]
struct Allocation<F: Future> {
    header: Task,
    future: UnsafeCell<MaybeUninit<F>>,
    result: JoinCell<F::Output>,
}

pub(super) struct Operations {
    pub(super) poll: unsafe fn(NonNull<Task>, &mut Context<'_>) -> Poll<()>,
    pub(super) drop_future: unsafe fn(NonNull<Task>),
    pub(super) fail: unsafe fn(NonNull<Task>, JoinError),
    free: unsafe fn(NonNull<Task>),
}

impl<F: Future> Allocation<F> {
    const OPERATIONS: Operations = Operations {
        poll: Self::poll,
        drop_future: Self::drop_future,
        fail: Self::fail,
        free: Self::free,
    };

    unsafe fn poll(header: NonNull<Task>, cx: &mut Context<'_>) -> Poll<()> {
        let allocation = header.cast::<Self>().as_ptr();
        // SAFETY: the operations match F and the pointer was preserved from
        // Box::into_raw. Only the running owner may poll the live F, whose
        // storage has never moved since allocation.
        let future = unsafe { &mut *(*allocation).future.get().cast::<F>() };
        match unsafe { Pin::new_unchecked(future) }.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(value) => {
                // SAFETY: the poll returned Ready and still owns the future.
                // Match `.await`: a panicking destructor prevents the result
                // from being published. The outer unwind handler can retry
                // cleanup without dropping the future twice.
                unsafe { Self::drop_future(header) };
                // SAFETY: the caller owns an allocation reference and the
                // join cell is independently synchronized.
                unsafe { &(*allocation).result }.finish(Ok(value));
                Poll::Ready(())
            }
        }
    }

    unsafe fn drop_future(header: NonNull<Task>) {
        let allocation = header.cast::<Self>().as_ptr();
        // SAFETY: the running or terminal reservation excludes all other
        // callers. Mark the slot empty before entering the user destructor.
        let live = unsafe { &header.as_ref().future_live };
        if live.load(Ordering::Relaxed) {
            live.store(false, Ordering::Relaxed);
            // SAFETY: the future is still initialized and pinned in place.
            unsafe { ptr::drop_in_place((*allocation).future.get().cast::<F>()) };
        }
    }

    unsafe fn fail(header: NonNull<Task>, error: JoinError) {
        // SAFETY: the operations match F and the caller anchors the
        // allocation for the duration of this synchronized cell access.
        unsafe { &(*header.cast::<Self>().as_ptr()).result }.finish(Err(error));
    }

    unsafe fn free(header: NonNull<Task>) {
        // SAFETY: the finalizer and join owner have both finished. This is
        // the original allocation pointer and the only reclaim. The future
        // is in MaybeUninit because its terminal owner already dropped it.
        drop(unsafe { Box::from_raw(header.cast::<Self>().as_ptr()) });
    }
}

pub(super) fn new<F>(
    id: u64,
    future: F,
    scheduler: Weak<dyn Scheduler>,
) -> (TaskRef, JoinHandle<F::Output>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if size_of::<F>().max(align_of::<F>()) <= INLINE_FUTURE_BYTES {
        allocate(id, future, scheduler)
    } else {
        allocate(id, Box::pin(future), scheduler)
    }
}

fn allocate<F>(
    id: u64,
    future: F,
    scheduler: Weak<dyn Scheduler>,
) -> (TaskRef, JoinHandle<F::Output>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let allocation = Box::into_raw(Box::new(Allocation {
        header: Task {
            id,
            registry_index: AtomicUsize::new(usize::MAX),
            runnable: FutureSlot::new(()),
            future_live: AtomicBool::new(true),
            scheduler,
            references: References::new(),
            operations: &Allocation::<F>::OPERATIONS,
        },
        future: UnsafeCell::new(MaybeUninit::new(future)),
        result: JoinCell {
            state: Mutex::new(JoinState::Waiting(None)),
        },
    }));
    // SAFETY: Box::into_raw is non-null, and repr(C) puts the header first.
    // Project both pointers directly from the allocation pointer. Keep its
    // provenance in the task handle for casts back to the full allocation.
    let (header, result) = unsafe {
        (
            NonNull::new_unchecked(allocation.cast()),
            NonNull::new_unchecked(&raw mut (*allocation).result),
        )
    };
    (
        TaskRef(header),
        JoinHandle {
            owner: JoinOwner(header),
            result,
        },
    )
}

/// One execution reference held by a runnable, registry entry or waker.
#[doc(hidden)]
pub struct TaskRef(NonNull<Task>);

// SAFETY: allocation construction checks F and its output are Send. The
// schedule grants exclusive access to F, the result uses a mutex, and the
// remaining shared fields are immutable or atomic. A TaskRef anchors them.
unsafe impl Send for TaskRef {}
// SAFETY: shared references use the same atomic scheduling and mutex rules.
unsafe impl Sync for TaskRef {}

impl TaskRef {
    pub(super) fn pointer(&self) -> NonNull<Task> {
        self.0
    }

    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        left.0 == right.0
    }
}

impl Deref for TaskRef {
    type Target = Task;

    fn deref(&self) -> &Task {
        // SAFETY: this execution reference keeps the header alive.
        unsafe { self.0.as_ref() }
    }
}

impl AsRef<Task> for TaskRef {
    fn as_ref(&self) -> &Task {
        self
    }
}

impl Clone for TaskRef {
    fn clone(&self) -> Self {
        self.references.retain_task();
        Self(self.0)
    }
}

impl Drop for TaskRef {
    fn drop(&mut self) {
        let header = self.0;
        if self.references.release_task() {
            let finalizer = Finalizer(header);
            // There are no other execution references. The finalizer keeps
            // the allocation live if dropping the future wakes or drops a join.
            self.finalize_unregistered();
            drop(finalizer);
        }
    }
}

struct Finalizer(NonNull<Task>);

impl Drop for Finalizer {
    fn drop(&mut self) {
        // SAFETY: the last execution reference transferred its live finalizer
        // bit. If this was the last owner, future destruction has finished.
        let header = unsafe { self.0.as_ref() };
        if header.references.release_finalizer() {
            unsafe { (header.operations.free)(self.0) };
        }
    }
}

pub(super) struct JoinOwner(NonNull<Task>);

impl JoinOwner {
    pub(super) fn upgrade(&self) -> Option<TaskRef> {
        // SAFETY: the join bit keeps the header live; a successful upgrade
        // reserves a separate execution reference without resurrecting it.
        unsafe { self.0.as_ref() }
            .references
            .upgrade()
            .then(|| TaskRef(self.0))
    }
}

impl Drop for JoinOwner {
    fn drop(&mut self) {
        // SAFETY: this guard owns the join bit, and access to the result has
        // ended. If this was the last owner there is no execution or finalizer.
        let header = unsafe { self.0.as_ref() };
        if header.references.release_join() {
            unsafe { (header.operations.free)(self.0) };
        }
    }
}

// Owned wakers store the original thin pointer and hold one execution count.
// A poll waker borrows the count from its TaskRef until polling returns.
const WAKER: RawWakerVTable = RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);

pub(super) struct PollWaker<'task> {
    value: ManuallyDrop<Waker>,
    owner: PhantomData<&'task TaskRef>,
}

impl Deref for PollWaker<'_> {
    type Target = Waker;

    fn deref(&self) -> &Waker {
        &self.value
    }
}

pub(super) fn poll_waker(task: &TaskRef) -> PollWaker<'_> {
    let raw = RawWaker::new(task.0.as_ptr().cast(), &WAKER);
    // SAFETY: the borrow anchors a live execution reference. Only &Waker is
    // exposed and ManuallyDrop suppresses consumption of the borrowed count.
    // A Waker cloned by the future gets its own count through the vtable.
    let value = ManuallyDrop::new(unsafe { Waker::from_raw(raw) });
    PollWaker {
        value,
        owner: PhantomData,
    }
}

#[cfg(all(test, not(loom)))]
pub(super) fn task_waker(task: TaskRef) -> Waker {
    let raw = RawWaker::new(task.0.as_ptr().cast(), &WAKER);
    std::mem::forget(task);
    // SAFETY: the raw pointer and its count were transferred from task. WAKER
    // preserves the reference-count and thread-safety contracts.
    unsafe { Waker::from_raw(raw) }
}

unsafe fn raw_task(data: *const ()) -> TaskRef {
    // SAFETY: every callback receives the non-null allocation pointer that
    // task_waker put into the waker, plus its live execution reference.
    TaskRef(unsafe { NonNull::new_unchecked(data.cast_mut().cast()) })
}

unsafe fn raw_clone(data: *const ()) -> RawWaker {
    // SAFETY: borrow the incoming count without consuming it. The increment
    // belongs exclusively to the returned RawWaker.
    let task = ManuallyDrop::new(unsafe { raw_task(data) });
    task.references.retain_task();
    RawWaker::new(data, &WAKER)
}

unsafe fn raw_wake(data: *const ()) {
    // SAFETY: wake consumes the incoming count through TaskRef's destructor.
    unsafe { raw_task(data) }.schedule();
}

unsafe fn raw_wake_by_ref(data: *const ()) {
    // SAFETY: borrow the live count and leave it intact even on unwind.
    ManuallyDrop::new(unsafe { raw_task(data) }).schedule();
}

unsafe fn raw_drop(data: *const ()) {
    // SAFETY: drop consumes exactly the incoming raw count.
    drop(unsafe { raw_task(data) });
}
