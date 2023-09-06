//! Smartpoll provides a [`Task`] type that makes it easy to write your own executor for async Rust.
//!
//! A [`Task`] can be created from any [`Future`] that has no output and implements [`Send`].
//! To poll a task you just need to provide a closure that will schedule the task to be polled
//! again. This will be invoked if the task does not complete, but only once the task is ready to be
//! rescheduled.
//!
//! Because you don't have to deal with synchronisation, pinning or providing a [`Waker`], polling a
//! task is much simpler than polling a future directly. Only one copy of a task can exist at a
//! time, so each task has exclusive access to its underlying future. This is possible because tasks
//! cannot be cloned, and polling a task transfers ownership of it to the rescheduling code.
//!
//! Here is an example of a basic single-threaded executor that uses Smartpoll:
//!
//! ```rust
//! use smartpoll::Task;
//! use std::sync::{
//!     atomic::{AtomicUsize, Ordering},
//!     mpsc::channel,
//!     Arc,
//! };
//!
//! # fn yield_now() -> impl core::future::Future<Output = ()> {
//! #     use core::{
//! #         pin::Pin,
//! #         task::{Context, Poll},
//! #     };
//! #     struct YieldFut(bool);
//! #     impl core::future::Future for YieldFut {
//! #         type Output = ();
//! #         fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//! #             if self.0 {
//! #                 self.0 = false;
//! #                 cx.waker().wake_by_ref();
//! #                 Poll::Pending
//! #             } else {
//! #                 Poll::Ready(())
//! #             }
//! #         }
//! #     }
//! #     YieldFut(true)
//! # }
//! #
//! fn main() {
//!     // the executor has a work queue and an 'unfinished tasks' counter
//!     let (work_tx, work_rx) = channel();
//!     let num_unfinished_tasks = Arc::new(AtomicUsize::new(0));
//!
//!     // to reschedule a task, push it onto the work queue
//!     let reschedule_task = move |task| work_tx.send(task).unwrap();
//!
//!     let task_counter = num_unfinished_tasks.clone();
//!     let schedule_task = reschedule_task.clone();
//!
//!     // to spawn a task, add 1 to the counter and schedule the task
//!     let spawn_task = move |task| {
//!         task_counter.fetch_add(1, Ordering::SeqCst);
//!         schedule_task(task);
//!     };
//!
//!     // spawn some tasks
//!     spawn_task(Task::new(async {
//!         // async code...
//! #         println!("1");
//! #         yield_now().await;
//! #         println!("2");
//!     }));
//!     spawn_task(Task::new(async {
//!         // async code...
//! #         println!("A");
//! #         yield_now().await;
//! #         println!("B");
//!     }));
//!
//!     while let Ok(task) = work_rx.recv() {
//!         // poll the next task from the queue
//!         if task.poll(reschedule_task.clone()).is_ready() {
//!             // if the task has completed, subtract 1 from the counter
//!             if num_unfinished_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
//!                 // if the counter was 1, that was the last task
//!                 break;
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! For an implementation of a multithreaded executor, see this [example].
//!
//! For an explanation of how the library works, and a proof of its correctness, see the [source].
//!
//! [example]: https://github.com/MilesCourtie/smartpoll/blob/main/examples/executor.rs
//! [source]: https://github.com/MilesCourtie/smartpoll/blob/main/src/lib.rs

/*================================================================================================*/

/*  The aim of this library is to make it easy to poll futures, so that Rust programmers can create
    their own executors more easily.

    This library does not depend on std or any other libraries in order to make it as portable as
    possible, and minimise its impact on dependents' compile time and executable size.
*/

#![no_std]

use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    sync::atomic::AtomicUsize,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};
extern crate alloc;
use alloc::{boxed::Box, sync::Arc};

/*  The signature of `Future::poll()` shows the requirements that must be met in order to poll a
    future:

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;

    1) You must have an exclusive reference to the future.
    2) That reference must be pinned.
    3) You must provide a `Context`, which currently just means providing a `Waker`.

    In a single-threaded environment, requirement 1 is relatively straightforward as there is no
    need for synchronisation between threads. This library is intended for multi-threaded scenarios
    where the programmer needs to synchronise access to a future between multiple worker threads.

    A naive solution would be to guard each future with a mutex, but this becomes problematic once
    you consider requirement 2, as pinning does not propagate through a mutex. Instead, this library
    takes a different approach to synchronisation which makes use of Rust's ownership system.

    The future is stored in a `Task` object which cannot be cloned and does not give out references
    to the future. Polling the future is done by calling `Task::poll()`, which takes ownership of
    the `Task` object so that no other code on any thread has access to the future. This method
    calls `Future::poll()` and provides a waker that the future can use to signal that it is ready
    to be rescheduled, as per requirement 3.

    The key to the synchronisation is that `Task::poll()` and the waker communicate to ensure that
    the future is not rescheduled until after `Future::poll()` has returned. This 'smart polling'
    ensures that the future is only ever accessible to one thread at any given time.

    The rescheduling code is a closure provided by the user which is given ownership of the `Task`
    object when it eventually runs. Even if the waker is cloned, the communication between the
    wakers and `Task::poll()` ensures that the rescheduling code is called exactly once per poll
    until the future completes.
*/

/// The synchronisation algorithm is explained in this module. Each step of the algorithm has been
/// moved into its own function for testing purposes.
mod algorithm;

#[cfg(test)]
mod tests;

/// A task wraps around a [`Future`] to provide a simple interface for polling it.
pub struct Task(Pin<Arc<dyn AnyTaskInner>>);

/// A dynamically-sized type which stores a task's future alongside associated data.
struct TaskInner<F: Future<Output = ()> + Send> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,
    /// a counter that is shared between the task and its wakers to coordinate rescheduling
    counter: AtomicUsize,
    /// the task's [`Future`]
    future: UnsafeCell<F>,
}

impl Task {
    /// Creates a [`Task`] from the provided [`Future`].
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Arc::pin(TaskInner::new(future)))
    }

    /// Polls the task, returning the output from [`Future::poll`].
    ///
    /// If the future returned [`Poll::Pending`] and arranged for a [`Waker`] to be invoked,
    /// `reschedule_fn` will be called at some point and will be given ownership of this [`Task`].
    /// In this scenario it is guaranteed that the callback will not be invoked until the waker has
    /// been invoked *and* [`Future::poll`] has returned, and will be called exactly once. No
    /// guarantees are made regarding when this callback will be invoked or on what thread. For
    /// example, it may be called by a waker on an arbitrary thread in 10 minutes' time, or it might
    /// have been already been called on the current thread as part of this function.
    ///
    /// Note that there is a very rare exception to the above which may occur under very unlikely
    /// conditions. If a copy of the waker that is provided to the task's future is kept without
    /// being invoked, the task is polled `2^(N-2)` times where N is the size of a pointer in bits,
    /// and then the stored waker is invoked, the `reschedule_fn` provided when that waker was
    /// created may be called even though it was already called `2^(N-2)` polls ago. This is due to
    /// the task's internal counter wrapping such that the waker is unable to detect that it is no
    /// longer valid.
    pub fn poll(mut self, reschedule_fn: impl Fn(Task) + Send + Clone) -> Poll<()> {
        // the synchronisation algorithm is explained fully in the `algorithm` module
        use algorithm::task as steps;

        // Store the value of the task's internal counter at the start of this round of the
        // algorithm.
        let mut start = steps::get_counter(&mut self.0);

        // Try to safely reset the counter every 2^16 polls (i.e. every 2^18 increments).
        // This will succeed if there are no leftover wakers from previous polls, in which case it
        // will prevent the counter from wrapping unexpectedly.
        let inner = if start & 0x3ffff == 0 {
            let (success, inner) = steps::try_reset_counter(self.0);
            if success {
                start = 0;
            }
            inner
        } else {
            self.0
        };

        // Use the value of the counter to create a new waker. Assuming the counter has not
        // wrapped, no other wakers for this task will have this 'start' value.
        let waker = SmartWaker::new_waker(inner.clone(), reschedule_fn.clone(), start);

        // get a reference to the task's counter
        let counter = inner.counter();

        // next, poll the task's future using the new waker

        /* SAFETY:
            This is the only place where `TaskInner::poll` is invoked, and since this method
            consumes the `Task` object, which cannot be cloned, it will not be invoked again until
            the task has been rescheduled.

            The following code communicates with all copies of the waker to ensure that:
             a) the task is not rescheduled until `TaskInner::poll` has returned and the provided
                waker (or a clone thereof) has been invoked,
             b) the task is rescheduled exactly once iff `TaskInner::poll` returns `Pending`, and
             c) the task is not rescheduled if `TaskInner::poll` returns `Ready`.

            The correctness of the algorithm used to achieve this is shown in 'src/tests/proof.rs'.
        */
        let result = unsafe { inner.as_ref().poll(&waker) };

        // if the future returned 'pending'
        if result.is_pending() {
            // and a waker has been invoked
            if steps::was_waker_invoked(start, counter) {
                // try to obtain permission to reschedule the task from the waker threads
                let should_reschedule = steps::attempt_reschedule(start, counter);
                // if this was successful then reschedule the task
                if should_reschedule {
                    reschedule_fn(Self(inner));
                }
            }
        } else {
            // else the future returned 'ready', so end the algorithm
            steps::task_complete(start, counter);
        }

        // return the output from the future
        result
    }
}

impl<F: Future<Output = ()> + Send> TaskInner<F> {
    /// Creates a new [`TaskInner`] that wraps around the provided [`Future`].
    fn new(future: F) -> Self {
        Self {
            _pin: PhantomPinned,
            counter: AtomicUsize::new(0),
            future: UnsafeCell::new(future),
        }
    }
}

/// This trait provides dynamic dispatch over any `TaskInner` instance, i.e. over any [`Future`]
/// that is stored in a [`Task`].
trait AnyTaskInner: Send + Sync {
    /// Polls the task's future using the provided waker. Should only be called by `Task::poll`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this function will not be called on this instance again by
    /// any thread until it has returned, even if the waker is invoked before this function returns.
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()>;

    /// Returns a shared reference to the task's shared counter.
    fn counter(&self) -> &AtomicUsize;

    /// Returns a mutable reference to the task's shared counter.
    fn counter_mut(&mut self) -> &mut AtomicUsize;
}

/* SAFETY:
    It is safe to implement sync for `TaskInner` because the only non-sync type it contains is
    `UnsafeCell<F>` where `F` is the future type. Because the `UnsafeCell` is only accessed by
    `TaskInner::poll`, which is only invoked on a given instance by one thread at a time, this type
    can safely be shared between threads. Note that this type does not use any thread-local storage,
    and the future it contains must implement `Send` so cannot rely on TLS either.

    The future contained within the `TaskInner` does not need to be `Sync` because access to it is
    synchronised as described above.
*/
unsafe impl<F: Future<Output = ()> + Send> Sync for TaskInner<F> {}

impl<F: Future<Output = ()> + Send> AnyTaskInner for TaskInner<F> {
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()> {
        /* SAFETY:
            Because the caller has guaranteed that this function will not be called on this instance
            again by any thread until it has returned, and because no other code accesses the
            UnsafeCell, it is safe to assume that this function has exclusive access to the future
            in the UnsafeCell.
        */
        let future = unsafe { &mut *self.future.get() };

        /* SAFETY:
            Because the `TaskInner` type that contains the future is explicitly marked as !Unpin
            and is always accessed through a `Pin<Arc<_>>`, it is guaranteed that the future
            will not be moved. Therefore it is safe to create a pinned reference to it, as long as
            the original reference is shadowed.
        */
        let future = unsafe { Pin::new_unchecked(future) };

        future.poll(&mut Context::from_waker(waker))
    }

    fn counter(&self) -> &AtomicUsize {
        &self.counter
    }

    fn counter_mut(&mut self) -> &mut AtomicUsize {
        &mut self.counter
    }
}

/// A [`Waker`] that communicates with its [`Task`] to ensure that the task is not rescheduled
/// until the waker has been invoked and []`Future::poll`] has returned, and that it is only
/// rescheduled at most once per poll.
#[derive(Clone)]
struct SmartWaker<WakeFn: Fn(Task) + Send + Clone> {
    /// the inner storage of the task that created this waker
    task_inner: Pin<Arc<dyn AnyTaskInner>>,
    /// the value of the task's counter when this waker was created, i.e. at the start of this
    /// round of the synchronisation algorithm
    start: usize,
    /// the callback that reschedules the task
    reschedule_fn: WakeFn,
}

impl<RescheduleFn: Fn(Task) + Send + Clone> SmartWaker<RescheduleFn> {
    /// Creates a new [`SmartWaker`] and returns it in the form of a [`Waker`].
    fn new_waker(
        task_inner: Pin<Arc<dyn AnyTaskInner>>,
        reschedule_fn: RescheduleFn,
        start: usize,
    ) -> Waker {
        let this = Box::new(Self {
            task_inner,
            start,
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
            created by calling `Box::<SmartWaker<_>>::into_raw()`, this is guaranteed to simply
            convert the `data` pointer back into the `Box` it came from.

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
    /// Wakes the task and then consumes the waker.
    unsafe fn wake(data: *const ()) {
        Self::wake_by_ref(data);
        Self::drop(data);
    }

    /// The `wake_by_ref` function for this type's `RawWakerVTable`.
    /// Wakes the task without consuming the waker.
    unsafe fn wake_by_ref(data: *const ()) {
        // the synchronisation algorithm is explained fully in the `algorithm` module
        use algorithm::waker as steps;

        /* SAFETY:
            Since this function is only called through this type's vtable, it is guaranteed to only
            be called on the `data` pointer of `RawWaker`s that use this type's vtable.
            Since every `RawWaker` that uses this type's vtable has a `data` pointer that was
            created by calling `Box::<SmartWaker<_>>::into_raw()`, this is guaranteed to simply
            convert the `data` pointer back into the `Box` it came from.

            The pointer is not used again in this function, and the `Box` is converted back into
            a raw pointer before the function returns, so no double-free can occur.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };

        // get a reference to the task's counter
        let counter = this.task_inner.counter();

        // if this waker is still valid, is the first to be invoked for this round, and
        // `Future::poll()` has returned `Pending`
        if steps::on_wake(this.start, counter) {
            // try to obtain permission to reschedule the task from the `Task::poll()` thread
            let should_reschedule = steps::attempt_reschedule(this.start, counter);
            // if this was successful then reschedule the task
            if should_reschedule {
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
            created by calling `Box::<SmartWaker<_>>::into_raw()`, this is guaranteed to simply
            convert the `data` pointer back into the `Box` it came from.

            The raw pointer is not used again and the `Box` is dropped, so no double-free can occur.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };
        drop(this);
    }
}
