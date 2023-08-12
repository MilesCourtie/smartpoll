use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

extern crate alloc;
use alloc::vec::Vec;

mod waker {
    /// Returns a [`Waker`] that does nothing when invoked.
    pub(crate) fn noop_waker() -> Waker {
        // SAFETY: see below
        unsafe { Waker::from_raw(noop_raw_waker()) }
    }

    /* SAFETY:
        TODO explain why RawWaker contract is upheld
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

/// Performs a depth-first traversal of the tree of all possible interleavings of the futures
/// returned by the provided closure. The closure must produce the same list of futures each time it
/// is invoked.
pub(crate) fn interleave_futures<F>(init_fn: F)
where
    F: Fn() -> Vec<Pin<Box<dyn Future<Output = ()>>>>,
{
    let mut num_futures = None;

    let waker = waker::noop_waker();
    let mut cx = Context::from_waker(&waker);

    let mut history: Vec<usize> = vec![0];

    'next_execution: loop {
        let mut futures = init_fn();
        let futures_len = futures.len();
        assert!(futures_len != 0, "`init_fn` returned zero futures");
        if let Some(num_futures) = num_futures {
            assert!(
                futures_len == num_futures,
                "`init_fn` returned a different number of futures than it did in a previous \
                execution"
            );
        }
        num_futures = Some(futures_len);

        let mut future_has_completed = futures.iter().map(|_| false).collect::<Vec<_>>();
        let mut history_i = 0;

        'next_future: loop {
            // poll the futures in the order described by the history list
            while let Some(future_i) = history.get_mut(history_i) {
                let future = futures.get_mut(*future_i).unwrap();
                let completed = future_has_completed.get_mut(*future_i).unwrap();
                assert!(
                    !*completed,
                    "nondeterminism detected: a future completed sooner than it did in a previous \
                    execution"
                );
                *completed = future.as_mut().poll(&mut cx).is_ready();
                history_i += 1;
            }
            history_i = history.len() - 1;

            // once we've reached the end of the history list
            if let Some(next_uncompleted_i) = future_has_completed
                .iter()
                .enumerate()
                .find_map(|(i, &x)| (!x).then_some(i))
            {
                // if there is a future that has not yet completed
                // add it to the history list and continue
                history.push(next_uncompleted_i);
                history_i += 1;
                continue 'next_future;
            } else {
                // once all futures have completed
                while let Some(future_i) = history.get_mut(history_i) {
                    // increment the last item on the history list
                    *future_i += 1;
                    if *future_i == num_futures.unwrap() {
                        // if it overflows, pop it off the list and repeat for the next last item
                        history.pop();
                        history_i -= 1;
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
    //! tests that check that the utilities used in the tests behave correctly

    /// check that `yield_now()` yields control once and then completes
    #[test]
    fn yielding() {
        use super::{waker::noop_waker, yield_now};
        use core::{future::Future, pin::pin, task::Context};

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut future = pin!(yield_now());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(future.as_mut().poll(&mut cx).is_ready());
    }

    /// check that `interleave_futures()` works correctly using a hand-written test case
    #[test]
    fn interleave_futures_case1() {
        todo!()
    }
}
