use core::{
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

mod sync;
use sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, UnsafeCell,
};

#[cfg(test)]
mod tests;

/// Wrapper around a top-level [`Future`] that simplifies polling it.
pub struct Task(Pin<Arc<dyn AnyTaskInner>>);

/// Dynamically-sized type which wraps around a [`Future`] and contains shared state that is used to
/// coordinate rescheduling the task.
struct TaskInner<F: Future<Output = ()> + Send> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,

    /// state that is shared between the wakers and `Task::poll()` to coordinate rescheduling
    shared_state: SharedState,

    /* SAFETY:
        This field must only be accessed from within `TaskInner::poll()`.
    */
    /// the task's [`Future`]
    future: UnsafeCell<F>,
}

/// State that is shared between `Task::poll` and the wakers, used to ensure that the task is never
/// rescheduled while it is still being polled.
struct SharedState {
    /// A progress counter that is used to coordinate task rescheduling.
    /// Each time the task goes through the cycle of being polled and rescheduled, the counter
    /// is incremented by 4. The process is as follows:
    ///  1. Initially the counter is equal to 4N, where N is a non-negative integer. A waker is
    ///     created that stores the number 4N so that it can check it is still valid when invoked.
    ///  2. The future is polled and does not complete, so arranges for the waker to be invoked.
    ///     It may create multiple copies of the waker and arrange for many of them to be invoked.
    ///  3. If a waker is invoked while the future is still being polled, it sets the counter to
    ///     4N+1 iff its value is 4N. If this fails, another waker was invoked so this one halts.
    ///  4. Once `Task::poll` has finished polling the future, it increments the counter by 2,
    ///     noting the value it is replacing. Iff the value was 4N+1 it knows a waker has been
    ///     invoked, otherwise it halts as there is nothing left to do until a waker is invoked.
    ///  5. If a waker is invoked while the counter is 4N+2 or 4N+3, it will try to increment the
    ///     counter to 4N+1 but fail as the value isn't 4N. The waker will know from the counter's
    ///     value that (a) the task has finished being polled, and (b) whether another waker has
    ///     already been invoked.
    ///  6. The waker that incremented the counter and `Task::poll` (if it didn't halt in step 4)
    ///     try to increase the counter to 4N+4 using `compare_exchange`.
    ///     Whichever one succeeds then reschedules the task, and the counter is equal to 4(N+1).
    ///
    /// N.B.
    ///  *1 Initially the counter is 0, and occasionally it is reset to 0 before step 1, if there
    ///     are no wakers, to prevent it from overflowing.
    ///  *2 If the future completes in step 2, the counter is set to 4N+4 by `Task::poll` and is not
    ///     modified again as a result.
    progress: AtomicUsize,
}

impl Task {
    /// Converts the provided [`Future`] into a [`Task`].
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Self {
        #[cfg(not(loom))]
        let inner = Arc::pin(TaskInner::new(future));

        #[cfg(loom)]
        let inner = {
            extern crate alloc;
            use alloc::sync::Arc as StdArc;
            let inner: StdArc<dyn AnyTaskInner + 'static> = StdArc::new(TaskInner::new(future));
            let inner = Arc::from_std(inner);

            /* SAFETY
                I assume that it is safe to pin a new [`loom::sync::Arc`] because the same
                assumption is made in the implementation of [`loom::sync::Arc::pin()`].

                see: https://docs.rs/loom/0.6.1/src/loom/sync/arc.rs.html#24
            */
            unsafe { Pin::new_unchecked(inner) }
        };

        Self(inner)
    }

    /// Polls the task, returning `true` if and only if it completes.
    ///  If the task's future returns `Pending` and arranges for a waker to be invoked,
    /// `reschedule_fn` will be invoked sometime after `Future::poll()` has returned and will be
    /// given a [`Task`] containing the future.
    pub fn poll(self, reschedule_fn: impl Fn(Task) + Send + Clone) -> bool {
        let shared_state = self.0.shared_state();

        // Use the task's progress counter to create a new waker.
        // This is guaranteed to be the only waker with this `progress` value, as the counter was
        // incremented when the task was last rescheduled.
        let progress = shared_state.progress.load(Ordering::SeqCst);

        /*
        drop(shared_state);

        // every 2^16 polls (i.e. every 2^18 increments) reset `progress` to zero, if there are no
        // wakers, to stop it from overflowing
        self.0 = if progress & 0x3ffff == 0 {
            /* SAFETY
                It is safe to remove the Pin from the Arc as the pinning invariants are upheld
                until the Pin is added again. The shared_state is modified but nothing is moved.
            */
            let mut task_inner = unsafe { Pin::into_inner_unchecked(self.0) };
            if let Some(task_inner) = Arc::<_>::get_mut(&mut task_inner) {
                // if there are no wakers
                let progress = task_inner.shared_state_mut().progress.get_mut();
                *progress = 0;
            }
            /* SAFETY
                This is safe as we are simply re-pinning the Arc that was just unpinned.
            */
            unsafe { Pin::new_unchecked(task_inner) }
        } else {
            self.0
        };
        */

        // create a waker using the value of `shared_state.progress`
        let waker = SmartWaker::new_waker(self.0.clone(), reschedule_fn.clone(), progress);

        /* SAFETY:
            This method is the only code that calls `TaskInner::poll`, and cannot be called again
            until the `reschedule_fn` callback is invoked as it consumes the `Task` object.
            The `Task` object cannot be cloned, so the reschedule callback will have the only copy.

            The shared state is used by the following code and all of the task's wakers to ensure
            that the task is not rescheduled until this function call has completed. Therefore it
            is guaranteed that the function will not be called on this `TaskInner` instance again by
            any thread until it has returned.
        */
        let result = unsafe { self.0.as_ref().poll(&waker) };

        if result.is_pending() {
            let _waker_invoked = match shared_state.progress.fetch_add(2, Ordering::SeqCst) {
                n if n == progress => false,
                n if n == progress + 1 => true,
                n => panic!("BUG: 'progress' was unexpectedly set to {n} during poll"),
            };

            println!("task: waker_invoked: {_waker_invoked}");

            let permission_to_reschedule = shared_state
                .progress
                .compare_exchange(
                    progress + 3,
                    progress + 4,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok();

            println!("task: permission_to_reschedule: {permission_to_reschedule}");

            if permission_to_reschedule {
                reschedule_fn(self);
            }
        }
        result.is_ready()
    }
}

impl<F: Future<Output = ()> + Send> TaskInner<F> {
    fn new(future: F) -> Self {
        let shared_state = SharedState {
            progress: AtomicUsize::new(0),
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
    /// The caller must guarantee that this function will not be called on this instance again by
    /// any thread until it has returned, even if the waker is invoked before this function returns.
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()>;

    /// Returns a shared reference to the task's shared state.
    fn shared_state(&self) -> &SharedState;

    /// Returns a mutable reference to the task's shared state.
    fn shared_state_mut(&mut self) -> &mut SharedState;
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
        // this is the API of `loom::cell::UnsafeCell`, rather than `core::cell::UnsafeCell`
        self.future.with_mut(|future| {
            /* SAFETY:
                Because the caller has guaranteed that this function will not be called again by any
                thread until it has returned, and because no other code accesses the UnsafeCell, it
                is safe to assume that this function has exclusive access to the future in the
                UnsafeCell.
            */
            let future = unsafe { &mut *future };

            /* SAFETY:
                Because the `TaskInner` type that contains the future is explicitly marked as !Unpin
                and is always accessed through a `Pin<Arc<_>>`, it is guaranteed that the future
                will not be moved. Therefore it is safe to create a pinned reference to it.
            */
            let future = unsafe { Pin::new_unchecked(future) };

            future.poll(&mut Context::from_waker(waker))
        })
    }

    fn shared_state(&self) -> &SharedState {
        &self.shared_state
    }

    fn shared_state_mut(&mut self) -> &mut SharedState {
        &mut self.shared_state
    }
}

/// A [`Waker`] that communicates with its [`Task`] to ensure that the task is never rescheduled
/// until after it has finished being polled.
#[derive(Clone)]
struct SmartWaker<WakeFn: Fn(Task) + Send + Clone> {
    task_inner: Pin<Arc<dyn AnyTaskInner>>,
    /// the value of the task's progress counter when this waker was created
    progress: usize,
    /// the closure to call when the waker is invoked
    reschedule_fn: WakeFn,
}

impl<RescheduleFn: Fn(Task) + Send + Clone> SmartWaker<RescheduleFn> {
    fn new_waker(
        task_inner: Pin<Arc<dyn AnyTaskInner>>,
        reschedule_fn: RescheduleFn,
        progress: usize,
    ) -> Waker {
        let this = Box::new(Self {
            task_inner,
            progress,
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
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, this is guaranteed to
            simply convert the `data` pointer back into the `Box` it came from.

            The pointer is not used again in this function, and the `Box` is converted back into
            a raw pointer before the function returns, so no double-free can occur.
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
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, this is guaranteed to
            simply convert the `data` pointer back into the `Box` it came from.

            The pointer is not used again in this function, and the `Box` is converted back into
            a raw pointer before the function returns, so no double-free can occur.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };

        let shared_state = this.task_inner.shared_state();

        // If we successfully increment the progress counter to this.progress+1, this is the "first"
        // valid waker to be invoked since the task was polled. From the old value of the counter we
        // can also learn whether the task has finished being polled.
        let (mut first_waker, poll_completed) = match shared_state.progress.compare_exchange(
            this.progress,
            this.progress + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(n) if n == this.progress => (true, false),
            Err(n) if n == this.progress + 1 => (false, false),
            Err(n) if n == this.progress + 2 => (true, true),
            Err(n) if n == this.progress + 3 => (false, true),
            Err(n) => panic!("BUG: shared_state.progress was unexpectedly {n}"),
            Ok(_) => unreachable!(),
        };

        println!("waker: first_waker: {first_waker}");
        println!("waker: poll_completed: {poll_completed}");

        if first_waker && poll_completed {
            first_waker = shared_state
                .progress
                .compare_exchange(
                    this.progress + 2,
                    this.progress + 3,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok();
        }

        println!("waker: first_waker: {first_waker}");

        if first_waker {
            let permission_to_reschedule = shared_state
                .progress
                .compare_exchange(
                    this.progress + 3,
                    this.progress + 4,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok();

            println!("waker: permission_to_reschedule: {permission_to_reschedule}");

            if permission_to_reschedule {
                (this.reschedule_fn)(Task(this.task_inner.clone()));
            }
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
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, this is guaranteed to
            simply convert the `data` pointer back into the `Box` it came from.

            The raw pointer is not used again and the `Box` is dropped, so no double-free can occur.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };
        drop(this);
    }
}
