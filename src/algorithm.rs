//! TODO document this module

pub(crate) mod task {
    use crate::AnyTaskInner;
    use core::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
    };
    extern crate alloc;
    use alloc::sync::Arc;

    /// The first step of the algorithm. Returns the value of the counter.
    pub(crate) fn get_counter(task_inner: &mut Pin<Arc<dyn AnyTaskInner>>) -> usize {
        task_inner.counter().load(Ordering::SeqCst)
    }

    /// The task thread occasionally calls this before polling the task to try and keep the counter
    /// from overflowing. Returns a bool that is true iff the counter was reset, and gives back
    /// ownership of the `TaskInner` instance, which must have no clones for this method to succeed.
    pub(crate) fn try_reset_counter(
        task_inner: Pin<Arc<dyn AnyTaskInner>>,
    ) -> (bool, Pin<Arc<dyn AnyTaskInner>>) {
        /* SAFETY:
            We remove the Pin from the Arc and immediately put it back after attempting to
            reset the counter. No data is moved or dropped so the pinning contract is not violated.
        */
        let mut task_inner = unsafe { Pin::into_inner_unchecked(task_inner) };
        let success;
        if let Some(inner) = Arc::get_mut(&mut task_inner) {
            *inner.counter_mut().get_mut() = 0;
            success = true;
        } else {
            success = false;
        }
        (success, unsafe { Pin::new_unchecked(task_inner) })
    }

    /// If the future returned 'pending' after being polled, the task thread calls this to see if a
    /// waker has been invoked yet. If this returns true, the task thread continues to the next step
    /// of the algorithm.
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

    /// The last step of the task thread's side of the algorithm, to be called if
    /// `was_waker_invoked` returns true. Returns true iff the task thread should reschedule the
    /// task.
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

    /// If the future returned 'ready' after being polled, the task thread calls this instead of
    /// 'was_waker_invoked'.
    pub(crate) fn task_complete(start: usize, counter: &AtomicUsize) {
        let result = counter.compare_exchange(start, start + 4, Ordering::SeqCst, Ordering::SeqCst);
        // TODO explain why it is okay to ignore the result
        let _ = result;
    }
}

pub(crate) mod waker {
    use crate::AtomicUsize;
    use core::sync::atomic::Ordering;

    /// The first step of a waker's side of the algorithm. Returns true iff the waker should proceed
    /// to the next step.
    pub(crate) fn on_wake(start: usize, counter: &AtomicUsize) -> bool {
        match counter.compare_exchange(start, start + 1, Ordering::SeqCst, Ordering::SeqCst) {
            // the poll has completed and this is is the first waker to be invoked
            Err(n) if n == start + 2 => true,
            /* all other cases return false:
              Ok(_) => this is the first waker to be invoked, but the poll hasn't finished
              Err(n) if n == start + 1 => this isn't the first waker, and the poll hasn't finished
              Err(n) if n == start + 3 => the poll has finished, but this isn't the first waker
              Err(_) => this waker is from a previous poll so should do nothing
            */
            _ => false,
        }
    }

    /// The second step of a waker's side of the algorithm. Returns true iff the waker should
    /// reschedule the task.
    pub(crate) fn attempt_reschedule(start: usize, counter: &AtomicUsize) -> bool {
        match counter.compare_exchange(start + 2, start + 4, Ordering::SeqCst, Ordering::SeqCst) {
            // this thread succeeded in setting the counter to `start + 4`, so has permission to
            // reschedule the task
            Ok(_) => true,
            // another thread has already set the counter to `start + 4`
            Err(n) if n == start + 4 => false,
            // something has gone wrong
            Err(n) => panic!(
                "BUG: counter was unexpectedly {n} when waker attempted to reschedule \
                (expected {} or {})",
                start + 2,
                start + 4,
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
