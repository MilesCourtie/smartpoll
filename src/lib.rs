//! Smartpoll provides a [`Task`] type that makes it easy to write your own multithreaded executor
//! for async Rust.
//!
//! A [`Task`] can be created from any [`Future`] that has no output and implements [`Send`].
//! To poll a task you just need to provide a closure that will schedule the task to be polled
//! again. This will be invoked if the task does not complete, but only once the task is ready to be
//! rescheduled.
//!
//! Tasks can also store metadata of any type that implements [`Send`].
//!
//! Polling tasks is much simpler than polling their futures directly as you do not have to deal
//! with synchronisation, pinning or providing [`Waker`]s. To demonstrate this, here is an example
//! of a basic multithreaded executor that uses Smartpoll and the standard library:
//!
//! ```rust
//! use smartpoll::Task;
//! use std::{
//!     sync::{
//!         atomic::{AtomicUsize, Ordering},
//!         mpsc, Arc,
//!     },
//!     thread,
//!     time::Duration,
//! };
//!
//! // the executor has a work queue,
//! let (queue_tx, queue_rx) = mpsc::channel::<Task<()>>();
//! // a counter that tracks the number of unfinished tasks (which is shared with each worker),
//! let num_unfinished_tasks = Arc::new(AtomicUsize::new(0));
//! // and a local counter that tracks which worker to send the next task to
//! let mut next_worker = 0;
//!
//! // to spawn a new task:
//! let spawn_task = {
//!     let queue_tx = queue_tx.clone();
//!     let num_unfinished_tasks = num_unfinished_tasks.clone();
//!     move |task| {
//!         // increment the 'unfinished tasks' counter
//!         num_unfinished_tasks.fetch_add(1, Ordering::SeqCst);
//!         // and add the task onto the work queue
//!         queue_tx.send(task).unwrap();
//!     }
//! };
//!
//! // to reschedule a task, add it back onto the work queue
//! let reschedule_task = move |task| queue_tx.send(task).unwrap();
//!
//! // for each worker:
//! let num_workers = thread::available_parallelism().unwrap().into();
//! let workers = (0..num_workers)
//!     .map(|_| {
//!         let num_unfinished_tasks = num_unfinished_tasks.clone();
//!         let reschedule_task = reschedule_task.clone();
//!
//!         // create a channel for sending tasks to the worker
//!         let (work_tx, work_rx) = mpsc::sync_channel::<Task<()>>(1);
//!
//!         // on a new thread:
//!         let join_handle = thread::spawn(move || {
//!             // for each task that is sent to this worker, until the channel closes:
//!             while let Ok(task) = work_rx.recv() {
//!                 // poll the task
//!                 if task.poll(reschedule_task.clone()).is_ready() {
//!                     // and if it has completed then decrement the 'unfinished tasks' counter
//!                     num_unfinished_tasks.fetch_sub(1, Ordering::SeqCst);
//!                 }
//!             }
//!         });
//!         (work_tx, join_handle)
//!     })
//!     .collect::<Vec<_>>();
//!
//! // spawn some tasks
//! spawn_task(Task::new((), async {
//!     // async code...
//! }));
//! spawn_task(Task::new((), async {
//!     // async code...
//! }));
//!
//! // while there are unfinished tasks:
//! while num_unfinished_tasks.load(Ordering::SeqCst) > 0 {
//!     // wait until a task is available from the queue
//!     if let Ok(task) = queue_rx.recv_timeout(Duration::from_millis(100)) {
//!         // send the task to the next available worker
//!         let mut task = Some(task);
//!         while let Err(mpsc::TrySendError::Full(returned_task)) =
//!             workers[next_worker].0.try_send(task.take().unwrap())
//!         {
//!             // whenever a worker's channel is full, try the next worker
//!             task = Some(returned_task);
//!             next_worker += 1;
//!             if next_worker == workers.len() {
//!                 next_worker = 0;
//!             }
//!         }
//!     }
//! }
//!
//! // once all of the tasks have completed
//! for (work_tx, join_handle) in workers.into_iter() {
//!     // close each worker's channel
//!     drop(work_tx);
//!     // and wait for each worker's thread to finish
//!     join_handle.join().unwrap();
//! }
//! ```
//!
//! For an explanation of how the library works and its correctness, see the [source].
//!
//! [source]: https://github.com/MilesCourtie/smartpoll/blob/main/src/lib.rs

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

/*  The aim of this library is to make it easier to poll futures, so that Rust programmers can
    create their own executors more easily.

    The signature of `Future::poll()` shows the requirements that must be met in order to poll a
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

/// The synchronisation algorithm is explained in this module, which contains the steps of the
/// algorithm separated into separate functions for testing purposes. The tests use an exhaustive
/// search of all possible multithreaded executions to check that the algorithm works regardless of
/// how the threads are scheduled.
mod algorithm;

/// This module contains utilities that are used in the crate's tests.
#[cfg(test)]
mod util;

/// A [`Task`] wraps around a [`Future`] to provide a simple interface for polling it.
pub struct Task<M: Send + 'static>(Pin<Arc<dyn AnyTaskInner<Metadata = M>>>);

/*  When a future is polled it is given a waker that it can use to reschedule the task. To prevent
    the task from being polled again before the waker has been invoked, calling `Task::poll()` takes
    ownership of the task object. Because the waker may be invoked on an arbitrary thread long after
    `Task::poll()` has returned, the task object cannot store its contents directly as they have
    to outlive the task object. The contents of the task are stored on the heap in a `TaskInner`
    object and shared between the task and its wakers, so that when a waker is invoked it can create
    a new `Task` object with the same contents as the original.
*/

/// A dynamically-sized type which stores a task's future and associated data.
struct TaskInner<M: Send, F: Future<Output = ()> + Send> {
    /// marks this type as !Unpin
    _pin: PhantomPinned,
    /// a counter that is shared between the task and its wakers to coordinate rescheduling
    counter: AtomicUsize,
    /// the task's metadata
    metadata: M,
    /// the task's [`Future`]
    future: UnsafeCell<F>,
}

impl<M: Send + 'static> Task<M> {
    /// Converts the provided [`Future`] into a [`Task`] that contains the provided metadata.
    pub fn new(metadata: M, future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Arc::pin(TaskInner::new(metadata, future)))
    }

    /// Polls the task, returning the output from [`Future::poll`].
    ///
    /// If the future returned [`Poll::Pending`] and arranged for a [`Waker`] to be invoked,
    /// `reschedule_fn` will be called at some point and will be given ownership of this [`Task`].
    /// If this occurs it is guaranteed that the callback will not run until the waker has been
    /// invoked *and* [`Future::poll`] has returned, and will be called exactly once. No further
    /// guarantees are made regarding when this callback will be invoked or on what thread. For
    /// example it may be called by a waker on an arbitrary thread in 10 minutes' time, or it might
    /// have already been called on the current thread as part of this function.
    pub fn poll(mut self, reschedule_fn: impl Fn(Task<M>) + Send + Clone + 'static) -> Poll<()> {
        // the synchronisation algorithm is explained fully in the `algorithm` module
        use algorithm::task as steps;

        // Store the value of the task's internal counter at the start of this round of the
        // algorithm.
        let mut start = steps::get_counter(&mut self.0);

        // Try to safely reset the counter every 2^16 polls (i.e. every 2^18 increments).
        // This will succeed iff there are no leftover wakers from previous rounds, in which case it
        // will prevent the counter from wrapping. If the counter did wrap, a leftover waker may not
        // be unable to detect that it is outdated. Note that this could only occur if the advice in
        // `Future::poll`'s documentation is ignored:
        //
        //  "on multiple calls to poll, only the Waker from the Context passed to the most recent
        //  call should be scheduled to receive a wakeup"
        //
        // source: https://doc.rust-lang.org/std/future/trait.Future.html#tymethod.poll
        //
        // Note that even if the counter did wrap such that an outdated waker participates in a
        // round which it shouldn't, no safety invariants could be violated because the outdated
        // waker would still participate in the round properly, just as any other waker would. The
        // worst outcome that this could cause is the task being rescheduled due to the outdated
        // waker's invocation, before any of the wakers from the current round are invoked.
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

        // if `Future::poll` returned `Pending`
        if result.is_pending() {
            // and a waker has been invoked
            if steps::was_waker_invoked(start, counter) {
                // try to claim full ownership of the task
                if steps::claim_ownership(start, counter) {
                    // if successful then reschedule the task
                    reschedule_fn(Self(inner));
                }
            }
        } else {
            // else the future returned `Ready`, so end the algorithm
            steps::task_complete(start, counter);
        }

        // return the output from the future
        result
    }

    /// Returns a shared reference to the task's metadata.
    pub fn metadata(&self) -> &M {
        self.0.metadata()
    }
}

/*  Regarding unwind safety:
    None of this crate's internal invariants can be broken by a panic: if a thread suddenly stops
    participating at any point in the synchronisation algorithm it might stop that task from being
    rescheduled, but no logical or safety invariants will be broken.

    However, `Task` does not implement `UnwindSafe` because the future it contains is not required
    to be `UnwindSafe`. If a user of this crate wishes to catch any unwinding panics that come from
    within a `Task`, they can use the `AssertUnwindSafe` wrapper to do so if they are confident that
    it will not create any logic bugs in their program.
*/

impl<M: Send, F: Future<Output = ()> + Send> TaskInner<M, F> {
    /// Creates a new [`TaskInner`] that wraps around the provided [`Future`].
    fn new(metadata: M, future: F) -> Self {
        Self {
            _pin: PhantomPinned,
            metadata,
            counter: AtomicUsize::new(0),
            future: UnsafeCell::new(future),
        }
    }
}

/// This trait provides dynamic dispatch over any `TaskInner` instance, i.e. over any [`Future`]
/// that is stored in a [`Task`].
trait AnyTaskInner: Send + Sync {
    /// The type of the task's metadata
    type Metadata;

    /// Polls the task's future using the provided waker. Should only be called by `Task::poll`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this function will not be called on this instance again by
    /// any thread until it has returned, even if the waker is invoked before this function returns.
    unsafe fn poll(self: Pin<&Self>, waker: &Waker) -> Poll<()>;

    /// Returns a shared reference to the task's counter.
    fn counter(&self) -> &AtomicUsize;

    /// Returns a mutable reference to the task's counter.
    fn counter_mut(&mut self) -> &mut AtomicUsize;

    /// Returns a shared reference to the task's metadata.
    fn metadata(&self) -> &Self::Metadata;
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
unsafe impl<M: Send, F: Future<Output = ()> + Send> Sync for TaskInner<M, F> {}

impl<M: Send, F: Future<Output = ()> + Send> AnyTaskInner for TaskInner<M, F> {
    type Metadata = M;

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

    fn metadata(&self) -> &Self::Metadata {
        &self.metadata
    }
}

/// A [`Waker`] that communicates with its [`Task`] to ensure that the task is not rescheduled
/// until the waker has been invoked and [`Future::poll`] has returned, and that it is only
/// rescheduled at most once per poll.
struct SmartWaker<M: Send + 'static, WakeFn: Fn(Task<M>) + Send + Clone> {
    /// the inner storage of the task that created this waker
    task_inner: Pin<Arc<dyn AnyTaskInner<Metadata = M>>>,
    /// the value of the task's counter when this waker was created, i.e. at the start of this
    /// round of the synchronisation algorithm
    start: usize,
    /// the callback that reschedules the task
    reschedule_fn: WakeFn,
}

impl<M: Send, RescheduleFn: Fn(Task<M>) + Send + Clone> Clone for SmartWaker<M, RescheduleFn> {
    fn clone(&self) -> Self {
        Self {
            task_inner: self.task_inner.clone(),
            start: self.start,
            reschedule_fn: self.reschedule_fn.clone(),
        }
    }
}

impl<M, RescheduleFn> SmartWaker<M, RescheduleFn>
where
    M: Send + 'static,
    RescheduleFn: Fn(Task<M>) + Send + Clone + 'static,
{
    /// Creates a new [`SmartWaker`] and returns it in the form of a [`Waker`].
    fn new_waker(
        task_inner: Pin<Arc<dyn AnyTaskInner<Metadata = M>>>,
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
        // `Future::poll` has returned `Pending`
        if steps::on_wake(this.start, counter) {
            // try to claim full ownership of the task
            if steps::claim_ownership(this.start, counter) {
                // if successful then reschedule the task
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

#[cfg(test)]
mod tests {
    use crate::{util::yield_once, Task};
    extern crate alloc;
    use alloc::boxed::Box;

    #[test]
    fn metadata_is_preserved() {
        //! Check that the metadata of the task passed to the rescheduling callback is the same
        //! metadata that was passed to the original task.

        let task = Task::new(Box::new(()), yield_once());

        // the task's metadata is a Box that points to a unique address
        let addr = (&**task.metadata() as *const _) as usize;

        task.poll(move |task| {
            // Check that the task's metadata is the original Box by checking that it is a Box which
            // points to the same address as the original.
            assert!(addr == (&**task.metadata() as *const _) as usize)
        })
        .is_ready();
    }
}
