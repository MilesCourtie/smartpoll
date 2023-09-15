use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Returns a [`Future`] that yields control to the executor once before completing.
pub(crate) fn yield_once() -> impl Future<Output = ()> {
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
    YieldFuture(true)
}

pub(crate) use noop_waker::noop_waker;
mod noop_waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};

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

pub(crate) use sequencer::Sequencer;
mod sequencer {
    use core::{
        future::Future,
        pin::Pin,
        task::{Context, Waker},
    };
    extern crate alloc;
    use alloc::{boxed::Box, vec, vec::Vec};

    /// Used to run a collection of [`Future`]s multiple times, exhausting every possible sequence
    /// in which they can be polled. Does not wait for [`Waker`] invocation so must not be used as
    /// a general-purpose executor.
    pub(crate) struct Sequencer {
        num_futures: Option<usize>,
        noop_waker: Waker,
        next_sequence: Vec<usize>,
    }

    impl Sequencer {
        /// Create a new `Sequencer`.
        pub fn new() -> Self {
            Self {
                num_futures: None,
                noop_waker: super::noop_waker(),
                next_sequence: vec![0],
            }
        }

        /// Helper function that converts an `impl Future` to a `Pin<Box<dyn Future>>`.
        pub fn prepare(
            future: impl Future<Output = ()> + 'static,
        ) -> Pin<Box<dyn Future<Output = ()>>> {
            Box::pin(future) as Pin<Box<dyn Future<Output = ()>>>
        }

        /// Run the provided collection of [`Futures`] according to the next unexplored polling
        /// sequence. Will panic if a different number of futures are provided than were given
        /// in a previous call for the same `Sequencer`, or if any of the futures complete sooner
        /// than they did in an earlier sequence.
        pub fn run_next_sequence(
            &mut self,
            mut futures: Vec<Pin<Box<dyn Future<Output = ()>>>>,
        ) -> bool {
            let futures_len = futures.len();
            if let Some(num_futures) = self.num_futures {
                assert!(
                    num_futures == futures_len,
                    "the number of futures to run has changed"
                );
            }
            self.num_futures = Some(futures_len);
            let num_futures = futures_len;

            let mut cx = Context::from_waker(&self.noop_waker);

            let sequence = &mut self.next_sequence;
            let mut sequence_i = 0;

            let mut future_completed_at_index = futures.iter().map(|_| None).collect::<Vec<_>>();

            'next_future: loop {
                // poll the futures according to the current sequence
                while let Some(future_i) = sequence.get_mut(sequence_i) {
                    let future = futures.get_mut(*future_i).unwrap();
                    let completed = future_completed_at_index[*future_i].is_some();
                    assert!(
                    !completed,
                    "nondeterminism detected: a future completed sooner than it did in a previous \
                    sequence"
                );
                    if future.as_mut().poll(&mut cx).is_ready() {
                        future_completed_at_index[*future_i] = Some(sequence_i);
                    }
                    sequence_i += 1;
                }
                sequence_i = sequence.len() - 1;

                // once we've reached the end of the sequence
                if let Some(next_uncompleted_i) = future_completed_at_index
                    .iter()
                    .enumerate()
                    .find_map(|(i, &time_of_completion)| time_of_completion.is_none().then_some(i))
                {
                    // if there is a future that has not yet completed
                    // add it to the sequence and continue
                    sequence.push(next_uncompleted_i);
                    sequence_i += 1;
                    continue 'next_future;
                } else {
                    // once all futures have completed
                    while let Some(future_i) = sequence.get_mut(sequence_i) {
                        // increment the last item in the sequence until it reaches the number of
                        // futures, or points to a future which had not yet completed at that point
                        // in time
                        while {
                            *future_i += 1;
                            *future_i < num_futures
                                && future_completed_at_index[*future_i]
                                    .is_some_and(|t| t < sequence_i)
                        } {}
                        // if it reached the number of futures
                        if *future_i == num_futures {
                            // pop the last item from the sequence and repeat for the next last item
                            sequence.pop();
                            if !sequence.is_empty() {
                                sequence_i -= 1;
                            }
                        } else {
                            // otherwise, continue to the next sequence
                            return false;
                        }
                    }
                    // we have explored all possible sequences, so finish
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{noop_waker, yield_once, Sequencer};
    use core::{future::Future, pin::pin, task::Context};
    extern crate alloc;
    use alloc::vec;

    /// Check that `yield_once` yields control once and then completes.
    #[test]
    fn yield_test() {
        let waker = noop_waker::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(yield_once());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(future.as_mut().poll(&mut cx).is_ready());
    }

    /// types that can be used for logging runs of the sequencer
    mod sequence_logging {
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
            NewSequence(usize),
            Pending(usize),
            Ready(usize),
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
                    self.log.add(LogEntry::Pending(self.id));
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    self.log.add(LogEntry::Ready(self.id));
                    Poll::Ready(())
                }
            }
        }
    }

    /// Check that the sequencer works correctly using a hand-written test case.
    #[test]
    fn sequencer() {
        use sequence_logging::*;

        let log = Log::new();
        let test_log = log.clone();

        let mut sequencer = Sequencer::new();
        let mut sequence_counter = 0;

        loop {
            log.add(LogEntry::NewSequence(sequence_counter));
            sequence_counter += 1;
            let done = sequencer.run_next_sequence(vec![
                Sequencer::prepare(LoggerFut::new(log.clone(), 1, 1)),
                Sequencer::prepare(LoggerFut::new(log.clone(), 2, 2)),
            ]);
            if done {
                break;
            }
        }

        assert!(test_log.entry_eq(0, LogEntry::NewSequence(0)));
        assert!(test_log.entry_eq(1, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(2, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(3, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(4, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(5, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(6, LogEntry::NewSequence(1)));
        assert!(test_log.entry_eq(7, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(8, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(9, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(10, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(11, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(12, LogEntry::NewSequence(2)));
        assert!(test_log.entry_eq(13, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(14, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(15, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(16, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(17, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(18, LogEntry::NewSequence(3)));
        assert!(test_log.entry_eq(19, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(20, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(21, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(22, LogEntry::Ready(2)));
        assert!(test_log.entry_eq(23, LogEntry::Ready(1)));

        assert!(test_log.entry_eq(24, LogEntry::NewSequence(4)));
        assert!(test_log.entry_eq(25, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(26, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(27, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(28, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(29, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(30, LogEntry::NewSequence(5)));
        assert!(test_log.entry_eq(31, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(32, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(33, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(34, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(35, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(36, LogEntry::NewSequence(6)));
        assert!(test_log.entry_eq(37, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(38, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(39, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(40, LogEntry::Ready(2)));
        assert!(test_log.entry_eq(41, LogEntry::Ready(1)));

        assert!(test_log.entry_eq(42, LogEntry::NewSequence(7)));
        assert!(test_log.entry_eq(43, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(44, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(45, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(46, LogEntry::Ready(1)));
        assert!(test_log.entry_eq(47, LogEntry::Ready(2)));

        assert!(test_log.entry_eq(48, LogEntry::NewSequence(8)));
        assert!(test_log.entry_eq(49, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(50, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(51, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(52, LogEntry::Ready(2)));
        assert!(test_log.entry_eq(53, LogEntry::Ready(1)));

        assert!(test_log.entry_eq(54, LogEntry::NewSequence(9)));
        assert!(test_log.entry_eq(55, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(56, LogEntry::Pending(2)));
        assert!(test_log.entry_eq(57, LogEntry::Ready(2)));
        assert!(test_log.entry_eq(58, LogEntry::Pending(1)));
        assert!(test_log.entry_eq(59, LogEntry::Ready(1)));

        assert!(test_log.len() == 60);
    }
}
