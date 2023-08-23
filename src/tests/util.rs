//! A collection of utilities used in the crate's tests.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

extern crate alloc;
use alloc::{boxed::Box, vec, vec::Vec};

mod waker {
    /// Returns a [`Waker`] that does nothing when invoked.
    pub(crate) fn noop_waker() -> Waker {
        // SAFETY: see below
        unsafe { Waker::from_raw(noop_raw_waker()) }
    }

    /* SAFETY:
        It is safe to crate a Waker from the RawWaker defined below as the contract described in
        the documentation for RawWaker and RawWakerVTable is upheld:
          - All functions are thread-safe as no data is stored.
          - All resources are managed correctly, as there are no resources.
          - The wake() and wake_by_ref() methods *do not* wake the task as is is the intended
            behaviour of the 'no-op' waker. It is not used as an actual waker, just as a dummy for
            testing purposes.
    */
    use core::task::{RawWaker, RawWakerVTable, Waker};
    const fn noop_raw_waker() -> RawWaker {
        RawWaker::new(core::ptr::null(), &NOOP_WAKER_VTABLE)
    }
    const NOOP_WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(noop_waker_clone, noop, noop, noop);
    unsafe fn noop_waker_clone(_data: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    unsafe fn noop(_data: *const ()) {}
}

/// Returns a [`Future`] that yields control to the executor once before completing.
pub(crate) fn yield_now() -> YieldFuture {
    YieldFuture(true)
}
pub(crate) struct YieldFuture(bool);
impl Future for YieldFuture {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0 {
            self.0 = false;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

/// Runs all possible interleavings of the futures returned by the provided closure.
/// The closure must produce the same list of futures each time it is invoked.
/// The futures are provided with a dummy waker, and will be polled regardless of when or if the
/// provided wakers are invoked.
/// Note that this explores *all* possible interleavings so the complexity grows very rapidly with
/// both the number of futures and the number of polls required for each future to complete.
pub(crate) fn interleave_futures<F>(mut init_fn: F)
where
    F: FnMut() -> Vec<Pin<Box<dyn Future<Output = ()>>>>,
{
    let mut num_futures = None;

    let waker = waker::noop_waker();
    let mut cx = Context::from_waker(&waker);

    let mut history: Vec<usize> = vec![0];

    'next_execution: loop {
        let mut futures = init_fn();
        let futures_len = futures.len();
        assert!(futures_len != 0, "`init_fn` didn't return any futures");
        if let Some(num_futures) = num_futures {
            assert!(
                futures_len == num_futures,
                "`init_fn` returned a different number of futures than it did in a previous \
                execution"
            );
        }
        num_futures = Some(futures_len);
        let num_futures = futures_len;

        let mut future_completed_at_time = futures.iter().map(|_| None).collect::<Vec<_>>();
        let mut history_i = 0;

        'next_future: loop {
            // poll the futures in the order described by the history list
            while let Some(future_i) = history.get_mut(history_i) {
                let future = futures.get_mut(*future_i).unwrap();
                let completed = future_completed_at_time[*future_i].is_some();
                assert!(
                    !completed,
                    "nondeterminism detected: a future completed sooner than it did in a previous \
                    execution"
                );
                if future.as_mut().poll(&mut cx).is_ready() {
                    future_completed_at_time[*future_i] = Some(history_i);
                }
                history_i += 1;
            }
            history_i = history.len() - 1;

            // once we've reached the end of the history list
            if let Some(next_uncompleted_i) = future_completed_at_time
                .iter()
                .enumerate()
                .find_map(|(i, &time_of_completion)| time_of_completion.is_none().then_some(i))
            {
                // if there is a future that has not yet completed
                // add it to the history list and continue
                history.push(next_uncompleted_i);
                history_i += 1;
                continue 'next_future;
            } else {
                // once all futures have completed
                while let Some(future_i) = history.get_mut(history_i) {
                    // increment the last item on the history list until it reaches the number of
                    // futures, or points to a future which had not yet completed at that point in
                    // time
                    while {
                        *future_i += 1;
                        *future_i < num_futures
                            && future_completed_at_time[*future_i].is_some_and(|t| t < history_i)
                    } {}
                    // if it reached the number of futures
                    if *future_i == num_futures {
                        // pop the last item off the history list and repeat for the next last item
                        history.pop();
                        if !history.is_empty() {
                            history_i -= 1;
                        }
                    } else {
                        // otherwise, continue to the next execution
                        continue 'next_execution;
                    }
                }
                // we have explored all possible interleavings, so finish
                break 'next_execution;
            }
        }
    }
}

#[cfg(test)]
mod metatests {
    //! tests for the testing utilities

    use core::{
        future::Future,
        pin::{pin, Pin},
        task::Context,
    };
    extern crate alloc;
    use alloc::{boxed::Box, vec};

    /// check that `yield_now()` yields control once and then completes
    #[test]
    fn yield_now() {
        use super::{waker::noop_waker, yield_now};

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut future = pin!(yield_now());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(future.as_mut().poll(&mut cx).is_ready());
    }

    /// types that enable logging the interleaved execution of futures
    mod logging {
        use core::{
            cell::UnsafeCell,
            future::Future,
            pin::Pin,
            task::{Context, Poll},
        };
        extern crate alloc;
        use alloc::{rc::Rc, vec, vec::Vec};

        #[derive(Debug, PartialEq, Eq)]
        pub(super) enum LogEntry {
            Execution(usize),
            Poll(usize),
            Complete(usize),
        }

        #[derive(Clone)]
        pub(super) struct Log(Rc<UnsafeCell<Vec<LogEntry>>>);
        impl Log {
            pub fn new() -> Self {
                Self(Rc::new(UnsafeCell::new(vec![])))
            }
            pub fn add(&self, entry: LogEntry) {
                /* SAFETY
                    Because the UnsafeCell is behind an Rc, which cannot be sent or shared between
                    threads, it is safe to assume that the UnsafeCell is only accessed by one thread
                    at a time.
                */
                let log = unsafe { &mut *self.0.get() };
                log.push(entry);
            }
            pub fn entry_eq(&self, index: usize, target: LogEntry) -> bool {
                /* SAFETY
                    Because the UnsafeCell is behind an Rc, which cannot be sent or shared between
                    threads, it is safe to assume that the UnsafeCell is only accessed by one thread
                    at a time.
                */
                let log = unsafe { &*self.0.get() };
                *log.get(index).expect("provided index is out of bounds") == target
            }
            pub fn len(&self) -> usize {
                /* SAFETY
                    Because the UnsafeCell is behind an Rc, which cannot be sent or shared between
                    threads, it is safe to assume that the UnsafeCell is only accessed by one thread
                    at a time.
                */
                unsafe { &*self.0.get() }.len()
            }
        }

        impl core::fmt::Debug for Log {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                /* SAFETY
                    Because the UnsafeCell is behind an Rc, which cannot be sent or shared between
                    threads, it is safe to assume that the UnsafeCell is only accessed by one thread
                    at a time.
                */
                write!(f, "Log({:?})", unsafe { &*self.0.get() })
            }
        }

        #[derive(Debug)]
        pub(super) struct LoggerFut {
            log: Log,
            id: usize,
            polls_remaining: usize,
        }

        impl LoggerFut {
            pub fn new(log: Log, id: usize, polls_remaining: usize) -> Self {
                Self {
                    log,
                    id,
                    polls_remaining,
                }
            }
        }

        impl Future for LoggerFut {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                if self.polls_remaining > 0 {
                    self.polls_remaining -= 1;
                    self.log.add(LogEntry::Poll(self.id));
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    self.log.add(LogEntry::Complete(self.id));
                    Poll::Ready(())
                }
            }
        }
    }

    /// check that `interleave_futures()` works correctly using a hand-written test case
    #[test]
    fn interleave_futures() {
        use logging::{Log, LogEntry, LoggerFut};
        let log = Log::new();
        let test_log = log.clone();

        let mut execution_counter = 0;
        super::interleave_futures(move || {
            log.add(LogEntry::Execution(execution_counter));
            execution_counter += 1;
            vec![
                Box::pin(LoggerFut::new(log.clone(), 1, 1)) as Pin<Box<dyn Future<Output = ()>>>,
                Box::pin(LoggerFut::new(log.clone(), 2, 2)) as Pin<Box<dyn Future<Output = ()>>>,
            ]
        });

        assert!(test_log.entry_eq(0, LogEntry::Execution(0)));
        assert!(test_log.entry_eq(1, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(2, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(3, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(4, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(5, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(6, LogEntry::Execution(1)));
        assert!(test_log.entry_eq(7, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(8, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(9, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(10, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(11, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(12, LogEntry::Execution(2)));
        assert!(test_log.entry_eq(13, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(14, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(15, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(16, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(17, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(18, LogEntry::Execution(3)));
        assert!(test_log.entry_eq(19, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(20, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(21, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(22, LogEntry::Complete(2)));
        assert!(test_log.entry_eq(23, LogEntry::Complete(1)));

        assert!(test_log.entry_eq(24, LogEntry::Execution(4)));
        assert!(test_log.entry_eq(25, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(26, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(27, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(28, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(29, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(30, LogEntry::Execution(5)));
        assert!(test_log.entry_eq(31, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(32, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(33, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(34, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(35, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(36, LogEntry::Execution(6)));
        assert!(test_log.entry_eq(37, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(38, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(39, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(40, LogEntry::Complete(2)));
        assert!(test_log.entry_eq(41, LogEntry::Complete(1)));

        assert!(test_log.entry_eq(42, LogEntry::Execution(7)));
        assert!(test_log.entry_eq(43, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(44, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(45, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(46, LogEntry::Complete(1)));
        assert!(test_log.entry_eq(47, LogEntry::Complete(2)));

        assert!(test_log.entry_eq(48, LogEntry::Execution(8)));
        assert!(test_log.entry_eq(49, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(50, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(51, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(52, LogEntry::Complete(2)));
        assert!(test_log.entry_eq(53, LogEntry::Complete(1)));

        assert!(test_log.entry_eq(54, LogEntry::Execution(9)));
        assert!(test_log.entry_eq(55, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(56, LogEntry::Poll(2)));
        assert!(test_log.entry_eq(57, LogEntry::Complete(2)));
        assert!(test_log.entry_eq(58, LogEntry::Poll(1)));
        assert!(test_log.entry_eq(59, LogEntry::Complete(1)));

        assert!(test_log.len() == 60);
    }
}
