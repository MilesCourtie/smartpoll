//! TODO document this module

pub(crate) mod task {
    use crate::AnyTaskInner;
    use core::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
    };
    extern crate alloc;
    use alloc::sync::Arc;

    pub(crate) fn get_counter(task_inner: &mut Pin<Arc<dyn AnyTaskInner>>) -> usize {
        task_inner.counter().load(Ordering::SeqCst)
    }

    pub(crate) fn try_reset_counter(
        task_inner: Pin<Arc<dyn AnyTaskInner>>,
    ) -> Pin<Arc<dyn AnyTaskInner>> {
        /* SAFETY:
            We remove the Pin from the Arc and immediately put it back after attempting to
            reset the counter. Regardless of whether the counter is reset, no data is moved
            or dropped so the pinning contract is not violated.
        */
        let mut task_inner = unsafe { Pin::into_inner_unchecked(task_inner) };
        if let Some(inner) = Arc::get_mut(&mut task_inner) {
            *inner.counter_mut().get_mut() = 0;
        }
        unsafe { Pin::new_unchecked(task_inner) }
    }

    pub(crate) fn was_waker_invoked(start: usize, counter: &AtomicUsize) -> bool {
        match counter.fetch_add(2, Ordering::SeqCst) {
            n if n == start => false,
            n if n == start + 1 => true,
            n => panic!(
                "BUG: counter was unexpectedly {n} after poll \
                (expected {} or {})",
                start,
                start + 1,
            ),
        }
    }

    pub(crate) fn attempt_reschedule(start: usize, counter: &AtomicUsize) -> bool {
        match counter.compare_exchange(start + 3, start + 4, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => true,
            Err(n) if n == start + 4 => false,
            Err(n) => panic!(
                "BUG: counter was unexpectedly {n} when task tried to reschedule \
                (expected {})",
                start + 3
            ),
        }
    }

    pub(crate) fn completed(start: usize, counter: &AtomicUsize) {
        let result = counter.compare_exchange(start, start + 4, Ordering::SeqCst, Ordering::SeqCst);
        // TODO explain why it is okay to ignore the result
        let _ = result;
    }
}

pub(crate) mod waker {
    use crate::AtomicUsize;
    use core::sync::atomic::Ordering;

    pub(crate) fn on_wake(start: usize, counter: &AtomicUsize) -> (bool, bool) {
        let (first_waker, poll_completed) =
            match counter.compare_exchange(start, start + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => (true, false),
                Err(n) if n == start + 1 => (false, false),
                Err(n) if n == start + 2 => (true, true),
                Err(n) if n == start + 3 => (false, true),
                Err(_) => (false, false), // waker is outdated
            };
        (first_waker, poll_completed)
    }

    pub(crate) fn attempt_reschedule(start: usize, counter: &AtomicUsize) -> bool {
        match counter.compare_exchange(start + 2, start + 4, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => true,
            Err(n) if n == start + 4 => false,
            Err(n) => panic!(
                "BUG: counter was unexpectedly {n} when waker tried to reschedule \
                (expected {})",
                start + 3
            ),
        }
    }
}

// Explanation of the synchronisation algorithm:
// (TODO move this to somewhere more sensible)
//
// Each time the task goes through the cycle of being polled and rescheduled, the counter
// is incremented by 4. the process is as follows:
//  1. Initially the counter is equal to 4n, where n is a non-negative integer. A waker is
//     created that stores the number 4n so that it can check it is still valid when invoked.
//  2. The future is polled and does not complete, so arranges for the waker to be invoked.
//     It may create multiple copies of the waker and arrange for many of them to be invoked.
//  3. If a waker is invoked while the future is still being polled, it sets the counter to
//     4n+1 iff its value is 4n. If this fails, another waker was invoked so this one halts.
//  4. Once `task::poll` has finished polling the future, it increments the counter by 2,
//     noting the value it is replacing. Iff the value was 4n+1 it knows a waker has been
//     invoked, otherwise it halts as there is nothing left to do until a waker is invoked.
//  5. If a waker is invoked while the counter is 4n+2 or 4n+3, it will try to increment the
//     counter to 4n+1 but fail as the value isn't 4n. The waker will know from the counter's
//     value that (a) the task has finished being polled, and (b) whether another waker has
//     already been invoked.
//  6. The waker that incremented the counter and `task::poll` (if it didn't halt in step 4)
//     try to increase the counter from 4n+3 to 4n+4 using `compare_exchange`.
//     Whichever one succeeds then reschedules the task, and the counter is equal to 4(n+1).
//
// N.B.
//  *1 The counter is initially 0, and occasionally it is reset to 0 before step 1 if there
//     are no wakers. This is to prevent it from overflowing.
//  *2 If the future completes in step 2, the counter is set to 4n+4 by `task::poll` and is not
//     modified again as there won't be any new wakers after that point.
