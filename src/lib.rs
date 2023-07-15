use core::{
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod sync;
use sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    cell::UnsafeCell,
    Arc,
};

#[cfg(test)]
mod tests;

/// Wrapper around a top-level [`Future`] that simplifies polling it.
pub struct Task(Pin<Arc<dyn AnyTaskInner>>);

/// Dynamically-sized type which contains the shared state needed to coordinate rescheduling the
/// task, as well as the task's [`Future`].
struct TaskInner<F: Future<Output = ()> + Send> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,

    /// state that is used to communicate between wakers and `Task::poll()`
    shared_state: SharedState,

    /* SAFETY:
        This field must only be accessed from within `TaskInner::poll()`.
    */
    future: UnsafeCell<F>,
}

/// State that is shared between `Task::poll` and the wakers, to ensure that whenever `Future::poll`
/// returns `Pending`, the task is not rescheduled until after `TaskInner::poll` has returned.
struct SharedState {
    /// monotonic counter for how many times this task has been rescheduled
    wake_counter: AtomicU64,

    /// flag to communicate whether the next waker should reschedule the task
    should_wake: AtomicBool,
}

impl Task {
    /// Converts the provided [`Future`] into a [`Task`].
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Self {
        #[cfg(not(loom))]
        let inner = Arc::pin(TaskInner::new(future));

        #[cfg(loom)]
        let inner = {
            let inner: sync::StdArc<dyn AnyTaskInner + 'static> =
                sync::StdArc::new(TaskInner::new(future));
            let inner = Arc::from_std(inner);

            /* SAFETY
                This is safe for the same reason that the implementation of loom::sync::Arc::pin()
                is safe.

                see:    https://docs.rs/loom/0.6.0/src/loom/sync/arc.rs.html#24
            */
            unsafe { Pin::new_unchecked(inner) }
        };

        Self(inner)
    }

    /// Polls the task. If the future does not complete, `reschedule_fn` will be invoked sometime
    /// after `Future::poll()` has returned and will be given a [`Task`] containing the future.
    pub fn poll(self, reschedule_fn: impl Fn(Task) + Send + Clone) {
        let shared_state = self.0.shared_state();

        // Read the task's wake counter and use it to create a new waker
        // This is guaranteed to be the only valid waker as any existing wakers were invalidated
        // when the task was last rescheduled (as the wake counter was incremented).
        let wake_counter = shared_state.wake_counter.load(Ordering::Acquire);
        let waker = SmartWaker::new_waker(self.0.clone(), reschedule_fn.clone(), wake_counter);

        /* SAFETY:
            This method is the only code that calls `TaskInner::poll`, and cannot be called again
            until the `reschedule_fn` callback is invoked as it consumes the `Task` object.
            The `Task` object cannot be cloned: the reschedule callback will have the only copy once
            it is invoked.
            The shared state is used by the wakers and the remainder of this function to ensure that
            the reschedule callback is not invoked until this `poll` invocation has returned, and
            is invoked no more than once per `poll` invocation.
        */
        let result = unsafe { self.0.as_ref().poll(&waker) };

        if result.is_pending() {
            let waker_invoked = shared_state.wake_counter.load(Ordering::SeqCst) != wake_counter;

            if waker_invoked {
                // A waker was invoked before poll() had finished. It did not reschedule the task,
                // but did increment the counter. The task must be rescheduled here instead.
                reschedule_fn(self);
            } else {
                // No wakers have been invoked yet for the most recent poll() invocation.
                // We must set 'should_wake' so that the next valid waker reschedules the task.
                shared_state
                    .should_wake
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .expect("BUG: should_wake was not reset");
            }
        } else {
            // poll() returned Poll::Ready
            // Even though there should be no valid wakers, increment the counter to invalidate them
            // just in case.
            let result = shared_state.wake_counter.compare_exchange(
                wake_counter,
                wake_counter + 1,
                Ordering::SeqCst,
                Ordering::Relaxed,
            );
            // If result is Ok(), the future did not use the waker we provided, which is expected as
            // it returned Poll::Ready.
            // If result is Err() then the future did use the waker, which it wasn't supposed to,
            // but all that the waker did was increment the counter which is what we were trying to
            // do anyway.
            let _ = result;
        }
    }
}

impl<F: Future<Output = ()> + Send> TaskInner<F> {
    fn new(future: F) -> Self {
        let shared_state = SharedState {
            wake_counter: AtomicU64::new(0),
            should_wake: AtomicBool::new(false),
        };
        Self {
            _pin: PhantomPinned,
            shared_state,
            future: UnsafeCell::new(future),
        }
    }
}

/// enables dynamic dispatch over any `TaskInner`
trait AnyTaskInner: Send + Sync {
    /// Polls the task's future using the provided waker. Should only be called by `Task::poll`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this function will not be called again by any thread until
    /// it has returned, even if the waker is invoked before this function returns.
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()>;

    /// Returns a shared reference to the task's shared state.
    fn shared_state(&self) -> &SharedState;
}

/* SAFETY:
    It is safe to implement sync for `TaskInner` because the only non-sync type it contains is
    `UnsafeCell<F>` where `F` is the future type. Because access to the `UnsafeCell` is
    synchronised manually by the implementations of `TaskInner` and `Task`, this type can safely
    be shared between threads.

    Furthermore, the future contained within the `TaskInner` does not need to be `Sync` because
    access to it is synchronised as described above.
*/
unsafe impl<F: Future<Output = ()> + Send> Sync for TaskInner<F> {}

impl<F: Future<Output = ()> + Send> AnyTaskInner for TaskInner<F> {
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()> {
        // uses the API for loom::cell::UnsafeCell instead of core::cell::UnsafeCell
        self.future.with_mut(|future| {
            /* SAFETY:
                Because the caller has guaranteed that this function will not be called again by any
                thread until it has returned, and because no other code accesses the UnsafeCell, it
                is safe to assume that this function has exclusive access to the future in the
                UnsafeCell.
            */
            let future = unsafe { &mut *future };

            /* SAFETY:
                Because the type that contains the future is explicitly marked as !Unpin and is
                accessed here through a `Pin`, it is safe to assume that the future will not be
                moved. Hence it is safe to create a pinned reference to it.
            */
            let future = unsafe { Pin::new_unchecked(future) };

            future.poll(&mut Context::from_waker(waker))
        })
    }

    fn shared_state(&self) -> &SharedState {
        &self.shared_state
    }
}

#[derive(Clone)]
struct SmartWaker<WakeFn: Fn(Task) + Send + Clone> {
    task_inner: Pin<Arc<dyn AnyTaskInner>>,
    /// the value of the task's wake counter when this waker was created
    wake_counter: u64,
    /// the closure to invoke when the waker is invoked
    reschedule_fn: WakeFn,
}

impl<RescheduleFn: Fn(Task) + Send + Clone> SmartWaker<RescheduleFn> {
    fn new_waker(
        task_inner: Pin<Arc<dyn AnyTaskInner>>,
        reschedule_fn: RescheduleFn,
        wake_counter: u64,
    ) -> Waker {
        let this = Box::new(Self {
            task_inner,
            wake_counter,
            reschedule_fn,
        });

        // convert box into a raw pointer, preventing the object from being automatically dropped
        let data = Box::into_raw(this) as *const ();

        /* SAFETY:
            It is safe to create a `Waker` from a `RawWaker` constructed from `data` and `VTABLE`
            because the contracts described in `RawWaker`'s and `RawWakerVTable`'s documentation are
            upheld:
              - `clone` creates a clone of the `RawWaker` which wakes the same task, and retains all
                resources that are required for both `RawWaker`s and the task.
              - `wake` wakes the `RawWaker`'s task and releases the resources associated with the
                `RawWaker` and its task.
              - `wake_by_ref` wakes the `RawWaker`'s task without releasing any resources.
              - `drop` releases the resources associated with the `RawWaker` and its task.
              - All four of the functions listed above are thread-safe.
        */
        unsafe { Waker::from_raw(RawWaker::new(data, &Self::VTABLE)) }
    }

    const VTABLE: RawWakerVTable =
        RawWakerVTable::new(Self::clone, Self::wake, Self::wake_by_ref, Self::drop);

    /// The `clone` function for this type's `RawWakerVTable`.
    /// Creates a copy of the waker. Does not consume the waker.
    unsafe fn clone(data: *const ()) -> RawWaker {
        /* SAFETY:
            Since this function is only called through this type's vtable, it is guaranteed to only
            be called on the `data` pointer of `RawWaker`s that use this type's vtable.
            Since every `RawWaker` that uses this type's vtable has a `data` pointer that was
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, we can be sure that we
            are converting the `data` pointer back into the `Box` it came from.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };
        let copy = this.clone();
        let copy = Box::into_raw(copy) as *const ();

        /* SAFETY:
            ensures `this` doesn't get dropped when it goes out of scope
        */
        Box::into_raw(this);

        RawWaker::new(copy, &Self::VTABLE)
    }

    /// The `wake` function for this type's `RawWakerVTable`.
    /// Wakes the future and then consumes the waker.
    unsafe fn wake(data: *const ()) {
        Self::wake_by_ref(data);
        Self::drop(data);
    }

    /// The `wake_by_ref` function for this type's `RawWakerVTable`.
    /// Wakes the future without consuming the waker.
    unsafe fn wake_by_ref(data: *const ()) {
        /* SAFETY:
            Since this function is only called through this type's vtable, it is guaranteed to only
            be called on the `data` pointer of `RawWaker`s that use this type's vtable.
            Since every `RawWaker` that uses this type's vtable has a `data` pointer that was
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, we can be sure that we
            are converting the `data` pointer back into the `Box` it came from.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };

        let shared_state = this.task_inner.shared_state();

        // find out whether the next waker to run should reschedule the task
        let should_wake = shared_state
            .should_wake
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok();

        // find out if any other valid waker has already been invoked before this one
        let other_waker_invoked = shared_state
            .wake_counter
            .compare_exchange(
                this.wake_counter,
                this.wake_counter + 1,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_err();

        // if we successfully set should_wake to false and incremented wake_counter,
        // this is the waker that must reschedule the task
        if should_wake && !other_waker_invoked {
            (this.reschedule_fn)(Task(this.task_inner.clone()));
        }

        /* SAFETY:
            ensures `this` doesn't get dropped when it goes out of scope
        */
        Box::into_raw(this);
    }

    /// The `drop` function for this type's `RawWakerVTable`.
    /// Consumes the waker.
    unsafe fn drop(data: *const ()) {
        /* SAFETY:
            Since this function is only called through this type's vtable, it is guaranteed to only
            be called on the `data` pointer of `RawWaker`s that use this type's vtable.
            Since every `RawWaker` that uses this type's vtable has a `data` pointer that was
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, we can be sure that we
            are converting the `data` pointer back into the `Box` it came from.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };
        drop(this);
    }
}
