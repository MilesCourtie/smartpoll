//! Smartpoll provides a [`Task`] type that makes it easy to write your own executor for async Rust.
//!
//! A [`Task`] can be created from any [`Future`] that has no output and implements [`Send`].
//! To poll a task you just need to provide a closure that will schedule the task to be polled
//! again. This will be invoked if the task does not complete, but only once the task is ready to be
//! rescheduled.
//!
//! Because you don't have to deal with synchronisation, pinning or providing a [`Waker`], polling a
//! [`Task`] is much simpler than polling a [`Future`] directly. Only one copy of a task can exist
//! at a time, so each task has exclusive access to its underlying future. This is possible because
//! tasks cannot be cloned, and polling a task transfers ownership of it to the rescheduling code.
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

    The library does not depend on std or any other libraries in order to make it as portable as
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

/*  The signature of `Future::poll()` shows the obstacles that must be overcome in order to poll a
    future:

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;

    1) You must have an exclusive reference to the future.
    2) That reference must be pinned.
    3) You must provide a `Context`, which currently just means providing a `Waker`.

    In a single-threaded environment, requirement 1 is relatively straightforward as there is no
    need for synchronisation between threads. This library is intended for multi-threaded scenarios
    where the programmer needs to synchronise access to a future between multiple worker threads.

    A naive solution would be to guard each future with a mutex, but this becomes problematic once
    you consider requirement 2 as pinning does not propagate through a mutex. Instead, this library
    takes a different approach to synchronisation which makes use of Rust's ownership system.

    The future is stored in a `Task` object which cannot be cloned and does not give out references
    to the future. Polling the future is done by calling `Task::poll()`, which takes ownership of
    the `Task` object so that no other code on any thread has access to the future. This method
    calls `Future::poll()` and provides a waker that the future can use to signal that it is ready
    to be rescheduled, as per requirement 3.

    The key to the synchronisation is that `Task::poll()` and the waker communicate to ensure that
    the future is not rescheduled until after `Future::poll()` has returned. This is the 'smart
    polling' after which the library is named, and is what synchronises access to the future.

    The rescheduling code is a closure provided by the user which is given ownership of the `Task`
    object when it eventually runs. Even if the waker is cloned, the communication between the
    wakers and `Task::poll()` ensures that the rescheduling code is called exactly once per poll
    until the future completes.
*/

/// The synchronisation algorithm is explained in this module, which contains the individual steps.
/// Each step of the algorithm has been moved into its own function for testing purposes.
mod algorithm;

#[cfg(test)]
mod tests;

/// Wrapper around a [`Future`] that simplifies polling it.
pub struct Task(Pin<Arc<dyn AnyTaskInner>>);

/// Dynamically-sized type which wraps around a [`Future`] and contains shared state that is used to
/// coordinate rescheduling the task.
struct TaskInner<F: Future<Output = ()> + Send> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,
    /// counter that is shared between the task and its wakers to coordinate rescheduling
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
    /// If the future returned `Pending` and arranged for a waker to be invoked, `reschedule_fn`
    /// will be/will have been invoked sometime after `Future::poll()` returned. That callback will
    /// then have the only copy of this [`Task`].
    pub fn poll(mut self, reschedule_fn: impl Fn(Task) + Send + Clone) -> Poll<()> {
        use algorithm::task as steps;

        // Use the task's progress counter to create a new waker.
        // This is guaranteed to be the only waker with this `progress` value, as the counter was
        // incremented when the task was last rescheduled.
        let mut start = steps::get_counter(&mut self.0);

        // every 2^16 polls (i.e. every 2^18 increments) try to reset the counter to zero to stop
        // it from overflowing. This will only succeed if there aren't any wakers left over from
        // previous polls of this task.
        let inner = if start & 0x3ffff == 0 {
            let (success, inner) = steps::try_reset_counter(self.0);
            if success {
                start = 0;
            }
            inner
        } else {
            self.0
        };

        // create a waker that shares the atomic counter and stores the counter's current value
        let waker = SmartWaker::new_waker(inner.clone(), reschedule_fn.clone(), start);

        // get a reference to the shared counter
        let counter = inner.counter();

        /* SAFETY:
            This is the only place where `TaskInner::poll` is invoked, and since this method
            consumes the `Task` object, which cannot be cloned, it cannot be called again until the
            reschedule callback has been invoked as that will have the only copy of this task.

            The following code communicates with any wakers created by the future to ensure that
             a) the task is not rescheduled until `TaskInner::poll` has returned,
             b) the task is rescheduled exactly once if `TaskInner::poll` returns `pending`, and
             c) the task is not rescheduled if `TaskInner::poll` returns `ready`.

            The correctness of the algorithm used to achieve this is shown in 'src/tests/proof.rs'.
        */
        let result = unsafe { inner.as_ref().poll(&waker) };

        if result.is_pending() {
            if steps::was_waker_invoked(start, counter) {
                let should_reschedule = steps::attempt_reschedule(start, counter);
                if should_reschedule {
                    reschedule_fn(Self(inner));
                }
            }
        } else {
            steps::task_complete(start, counter);
        }

        result
    }
}

impl<F: Future<Output = ()> + Send> TaskInner<F> {
    fn new(future: F) -> Self {
        Self {
            _pin: PhantomPinned,
            counter: AtomicUsize::new(0),
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

    /// Returns a shared reference to the task's shared counter.
    fn counter(&self) -> &AtomicUsize;

    /// Returns a mutable reference to the task's shared counter.
    fn counter_mut(&mut self) -> &mut AtomicUsize;
}

/* SAFETY:
    It is safe to implement sync for `TaskInner` because the only non-sync type it contains is
    `UnsafeCell<F>` where `F` is the future type. Because the `UnsafeCell` is only accessed by
    `TaskInner::poll`, which is only invoked on a given instance by one thread at a time, this type
    can safely be shared between threads.

    Furthermore, the future contained within the `TaskInner` does not need to be `Sync` because
    access to it is synchronised as described above.
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
            we shadow the original reference.
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

/// A [`Waker`] that communicates with its [`Task`] to ensure that the task is never rescheduled
/// until after it has finished being polled.
#[derive(Clone)]
struct SmartWaker<WakeFn: Fn(Task) + Send + Clone> {
    /// the task that created this waker
    task_inner: Pin<Arc<dyn AnyTaskInner>>,
    /// the value of the shared counter when this waker was created
    start: usize,
    /// the closure to call when the waker is invoked
    reschedule_fn: WakeFn,
}

impl<RescheduleFn: Fn(Task) + Send + Clone> SmartWaker<RescheduleFn> {
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
        use algorithm::waker as steps;

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

        let counter = this.task_inner.counter();

        if steps::on_wake(this.start, counter) {
            let should_reschedule = steps::attempt_reschedule(this.start, counter);
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
            created by calling `Box::<SmartWaker<RescheduleFn>>::into_raw()`, this is guaranteed to
            simply convert the `data` pointer back into the `Box` it came from.

            The raw pointer is not used again and the `Box` is dropped, so no double-free can occur.
        */
        let this = unsafe { Box::from_raw(data as *mut Self) };
        drop(this);
    }
}
