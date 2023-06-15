use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};
use parking_lot::Mutex;
use std::sync::Arc;

#[cfg(test)]
mod tests;

/// Wrapper around a top-level [`Future`] that simplifies polling it.
pub struct Task(Pin<Arc<dyn AnyTaskInner>>);

/// Dynamically-sized type which contains the shared state needed to coordinate rescheduling the
/// task, as well as the task's [`Future`].
struct TaskInner<F: Future<Output = ()>> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,

    /// state that is used to communicate between wakers and `Task::poll()`
    shared_state: Mutex<SharedState>,

    /* SAFETY:
        This field must only be accessed from within `TaskInner::poll()`.
    */
    future: UnsafeCell<F>,
}

/// State that is shared between `Task::poll` and the wakers, to ensure that whenever `Future::poll`
/// returns `Pending`, the task is not rescheduled until after `TaskInner::poll` has returned.
struct SharedState {
    /// monotonic counter for how many times this task has been rescheduled
    wake_counter: u64,

    /// flag to communicate whether a waker has been invoked for the most recent poll
    waker_invoked: bool,

    /// flag to communicate whether the most recent poll has completed
    poll_finished: bool,
}

impl Task {
    /// Converts the provided [`Future`] into a [`Task`].
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Arc::<_>::pin(TaskInner::new(future)))
    }

    /// Polls the task. If the future does not complete, the 'reschedule' callback will be invoked
    /// sometime after `Future::poll()` has returned, and will be given a [`Task`] containing the
    /// future.
    pub fn poll(self, reschedule_fn: impl Fn(Task) + Send + Clone) {
        // Read the task's wake counter and use it to create a new waker
        // This is guaranteed to be the only valid waker as any existing wakers were invalidated
        // when the task was last rescheduled (as the wake counter was incremented).
        let wake_counter = {
            let shared_state = self.0.shared_state().lock();
            shared_state.wake_counter
        };
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
            // the future has not yet completed, so it must be rescheduled either here or by a waker
            let reschedule_now: bool;
            {
                let mut shared_state = self.0.shared_state().lock();
                // if one or more wakers were invoked while the future was being polled
                if shared_state.waker_invoked {
                    // we must reschedule the task here as the wakers will not have done so
                    reschedule_now = true;
                    // and increment the wake counter to invalidate any remaining wakers
                    shared_state.wake_counter += 1;
                    // lastly we reset the shared state ready for the next poll
                    shared_state.poll_finished = false;
                    shared_state.waker_invoked = false;
                } else {
                    // if no wakers have been invoked yet we must communicate that the task has
                    // finished being polled, so that the first waker to be invoked knows it must
                    // reschedule the task.
                    shared_state.poll_finished = true;

                    // we mustn't reschedule the future yet as no wakers have been invoked yet
                    reschedule_now = false;
                }
            }
            // to keep the critical section short the reschedule callback is invoked outside of it
            if reschedule_now {
                reschedule_fn(self);
            }
        } else {
            // the future has completed, so does not need to be rescheduled
            {
                let mut shared_state = self.0.shared_state().lock();
                // There *should* be no valid wakers as `Future::poll()` shouldn't have used the one
                // we provided. In case the future did use the waker we provided, we increment the
                // wake counter to invalidate it.
                shared_state.wake_counter += 1;
                // lastly we reset the shared state ready for the next poll
                shared_state.poll_finished = false;
                shared_state.waker_invoked = false;
            }
        }
    }
}

impl<F: Future<Output = ()>> TaskInner<F> {
    fn new(future: F) -> Self {
        let shared_state = Mutex::new(SharedState {
            wake_counter: 0,
            waker_invoked: false,
            poll_finished: false,
        });
        Self {
            _pin: PhantomPinned,
            shared_state,
            future: UnsafeCell::new(future),
        }
    }
}

/// enables dynamic dispatch over any `TaskInner`
trait AnyTaskInner {
    /// Polls the task's future using the provided waker. Should only be called by `Task::poll`.
    ///
    /// # SAFETY
    ///
    /// The caller must guarantee that this function will not be called again by any thread until
    /// it has returned, even if the waker is invoked before this function returns.
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()>;

    /// Returns a shared reference to the mutex guarding the task's shared state.
    fn shared_state(&self) -> &Mutex<SharedState>;
}

impl<F: Future<Output = ()>> AnyTaskInner for TaskInner<F> {
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()> {
        /* SAFETY:
            Because the caller has guaranteed that this function will not be called again by any
            thread until it has returned, and because no other code accesses the UnsafeCell, it is
            safe to assume that this function has exclusive access to the UnsafeCell.
        */
        let future = unsafe { &mut *self.future.get() };

        /* SAFETY:
            Because the type that contains the future is explicitly marked as !Unpin and is accessed
            here through a `Pin`, it is safe to assume that the future will not be moved.
            Hence it is safe to create a pinned reference to it.
        */
        let future = unsafe { Pin::new_unchecked(future) };

        future.poll(&mut Context::from_waker(waker))
    }

    fn shared_state(&self) -> &Mutex<SharedState> {
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
            because the contracts described in `RawWaker`'s and `RawWakerVTable`'s documentation is
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

        // determine whether this particular waker instance should reschedule the future
        let mut reschedule_now = false;
        {
            let mut shared_state = this.task_inner.shared_state().lock();
            // if the wake counter doesn't match then the future has already been rescheduled
            if shared_state.wake_counter == this.wake_counter {
                // if the future has finished being polled then this waker should reschedule it
                if shared_state.poll_finished {
                    shared_state.wake_counter += 1;
                    reschedule_now = true;
                } else {
                    // otherwise we must communicate that a waker has been invoked so that the
                    // future can be rescheduled as soon as `poll` returns
                    shared_state.waker_invoked = true;
                }
            }
        }
        // to keep the critical section short the reschedule callback is invoked outside of it
        if reschedule_now {
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
