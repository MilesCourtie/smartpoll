/*  One purpose of the `Task` abstraction is to synchronise calls to `Future::poll` for each task.
    This means that while `Future::poll` is running on a particular thread, no other thread must
    call it on that same instance. The following describes how this invariant is enforced.

    When `Task::poll` is called it takes ownership of the `Task` object. Since the task cannot be
    cloned and does not give out references to its future, it is safe for `Task::poll` to assume
    it has exclusive access to the that future, which is stored on the heap in a `TaskInner` object.
    `Task::poll` calls `Future::poll` indirectly via `TaskInner::poll`, providing a waker which the
    future can use to reschedule the task.

    The waker (and any clones of it the future makes) share ownership of the `TaskInner` object with
    each other and the `Task` object. As well as the future itself, this also contains a counter
    which they use to communicate. They each also have a copy of the 'reschedule' closure.

    Once `Future::poll` has been called it may make arbitrarily many clones of the waker and arrange
    for each of them to be invoked at any time on any thread, and it will return either `Pending` or
    `Ready`. The purpose of the following algorithm is to ensure that:
      - If `Pending` is returned and a waker is invoked, the rescheduling code is called exactly
        once, but only once both conditions have been met.
      - If `Ready` is returned, the rescheduling code is not called.

    Calling the rescheduling closure requires constructing a new `Task` object using the existing
    `TaskInner` object. When `Pending` is returned and a waker is invoked exactly one of the
    participating threads will do this, effectively taking full ownership of the task before handing
    it to the rescheduling code. This requires that all of the other participating threads
    relinquish their ownership of the task.

    The design of the algorithm is explained below.

    The algorithm operates in rounds; one round corresponds to one poll of the task. The code for
    each round is split into two parts: one is run by `Task::poll` and the other is run by the waker
    that was provided to `Future::poll` whenever it is invoked, and is also run by any clones of
    that waker whenever they are invoked.

    The participants communicate by atomically increasing the counter stored in the `TaskInner`
    object. All of the atomic operations use sequentially consistent ordering, meaning that for a
    given run of the algorithm it will always be possible to determine some sequence in which the
    operations occurred, and all of the participants will have observed that same sequence. It also
    guarantees that the memory accesses will not be reordered within the same thread.

    Over the course of a round the shared counter increases from some initial value labelled `start
    to `start + 4`, which then becomes the value of `start` for the next round. The counter is
    occasionally reset to 0 before the start of a round to prevent it from wrapping, but only if
    there are no leftover wakers from previous rounds.

    At the start of the round, `Task::poll` reads the value of the counter and labels it `start`.
    It creates a new waker which has a copy of `start` and uses that waker to call `Future::poll`.
    It then acts according to the following pseudocode:

   1|   if `Future::poll` returned `Pending` {
   2|       let old_value = counter.fetch_add(2);
   3|       if the old value of the counter was `start+1` {
   4|           // a waker has been invoked and incremented the counter
   5|           // the counter now has a value of `start+3`
   6|           if counter.compare_exchange(start+3, start+4) is successful {
   7|               // we have taken full ownership of the task and must reschedule it
   8|               reschedule(Task);
   9|           }
  10|       }
  11|       return `Pending`;
  12|   } else {
  13|       // Future::poll returned `Ready`, so complete the round and return
  14|       counter.compare_exchange(start, start+4);
  15|       return `Ready`;
  16|   }

    At arbitrary times the waker and its clones each run the following:

   1|   if counter.compare_exchange(start, start+1) is unsuccessful because counter == start+2 {
   2|       // `Future::poll` has returned `Pending` and no other wakers have been invoked yet
   3|       if counter.compare_exchange(start+2, start+4) is successful {
   4|           // we have taken full ownership of the task and must reschedule it
   5|           reschedule(Task);
   6|       }
   7|   }

    You may wish to check for yourself that this algorithm meets the requirements described above
    for any valid sequence of memory accesses and for any number of wakers. An argument for its
    correctness is given in the module `src/tests/correctness.rs`, which includes automated tests
    that run all possible sequences of memory accesses for each test case.

    To aid with the development of these tests, each atomic access of the shared counter used by
    the algorithm has been isolated into its own function. These functions are shown below and are
    used both by the tests and the implementation itself. They are grouped into those used by
    `Task::poll` and those used by the wakers.
*/

pub(crate) mod task {
    use crate::AnyTaskInner;
    use core::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
    };
    extern crate alloc;
    use alloc::sync::Arc;

    /// The first step of the algorithm. Returns the value of the counter.
    pub(crate) fn get_counter<M: Send>(
        task_inner: &mut Pin<Arc<dyn AnyTaskInner<Metadata = M>>>,
    ) -> usize {
        task_inner.counter().load(Ordering::SeqCst)
    }

    /// The task thread occasionally calls this before polling the task to try and keep the counter
    /// from overflowing. Returns a bool that is true iff the counter was reset, and gives back
    /// ownership of the `TaskInner` instance, which must have no clones for this to succeed.
    pub(crate) fn try_reset_counter<M: Send>(
        task_inner: Pin<Arc<dyn AnyTaskInner<Metadata = M>>>,
    ) -> (bool, Pin<Arc<dyn AnyTaskInner<Metadata = M>>>) {
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
        debug_assert!(start % 4 == 0, "'start' is not a multiple of 4");
        match counter.fetch_add(2, Ordering::SeqCst) {
            n if n == start => false,
            n if n == start + 1 => true,
            n => panic!(
                "BUG: counter was unexpectedly {n} after poll returned \
                (expected {} or {})",
                start,
                start + 1,
            ),
        }
    }

    /// The last step of the task thread's side of the algorithm, called only if `was_waker_invoked`
    /// returned true. Returns true iff the task thread has successfully claimed full ownership of
    /// the task object and must now reschedule it.
    pub(crate) fn claim_ownership(start: usize, counter: &AtomicUsize) -> bool {
        debug_assert!(start % 4 == 0, "'start' is not a multiple of 4");
        match counter.compare_exchange(start + 3, start + 4, Ordering::SeqCst, Ordering::SeqCst) {
            // this thread succeeded in setting the counter to `start + 4`, so has claimed ownership
            Ok(_) => true,
            // another thread has already set the counter to `start + 4`
            Err(n) if n == start + 4 => false,
            Err(n) => panic!(
                "BUG: counter was unexpectedly {n} when task thread tried to claim ownership \
                (expected {})",
                start + 3
            ),
        }
    }

    /// If the future returned 'ready' after being polled, the task thread calls this instead of
    /// 'was_waker_invoked'.
    pub(crate) fn task_complete(start: usize, counter: &AtomicUsize) {
        debug_assert!(start % 4 == 0, "'start' is not a multiple of 4");
        let result = counter.compare_exchange(start, start + 4, Ordering::SeqCst, Ordering::SeqCst);
        /* The result of the compare_exchange can safely be ignored:
            - If the future behaved correctly and did not use the provided waker in its final poll,
              the compare_exchange will have succeeded. This information is not needed. Any
              remaining wakers will detect that they are outdated because the counter has increased.
            - If the future behaved improperly and used the provided waker in its final poll,
              the compare_exchange may fail if the waker has already been invoked. This information
              is not needed, and the 'incorrect' counter value does not matter because the task will
              not be polled again. This is because the waker will not have rescheduled the task, and
              any other wakers will detect that they are outdated because the counter has increased.
        */
        let _ = result;
    }
}

pub(crate) mod waker {
    use crate::AtomicUsize;
    use core::sync::atomic::Ordering;

    /// The first step of a waker's side of the algorithm. Returns true iff:
    ///   - the waker is from the current round,
    ///   - the waker is the first to be invoked for this round, and
    ///   - `Future::poll` has returned `Pending` this round.
    pub(crate) fn on_wake(start: usize, counter: &AtomicUsize) -> bool {
        debug_assert!(start % 4 == 0, "'start' is not a multiple of 4");
        match counter.compare_exchange(start, start + 1, Ordering::SeqCst, Ordering::SeqCst) {
            // the poll has returned `Pending` and this is is the first waker to be invoked
            Err(n) if n == start + 2 => true,
            /* all other cases return false:
              Ok(_) => this is the first waker to be invoked, but the poll hasn't finished
              Err(n) if n == start + 1 => this isn't the first waker, and the poll hasn't finished
              Err(n) if n == start + 3 => the poll has finished, but this isn't the first waker
              Err(_) => this waker is from a previous round so should do nothing
            */
            _ => false,
        }
    }

    /// The second step of a waker's side of the algorithm, which runs only if the first returns
    /// true. Returns true iff the waker has claimed full ownership of the task object and must now
    /// reschedule it.
    pub(crate) fn claim_ownership(start: usize, counter: &AtomicUsize) -> bool {
        debug_assert!(start % 4 == 0, "'start' is not a multiple of 4");
        match counter.compare_exchange(start + 2, start + 4, Ordering::SeqCst, Ordering::SeqCst) {
            // this thread succeeded in setting the counter to `start + 4`, so has claimed ownership
            Ok(_) => true,
            // another thread has already set the counter to `start + 4`
            Err(n) if n == start + 4 => false,
            Err(n) => panic!(
                "BUG: counter was unexpectedly {n} when waker attempted to claim ownership \
                (expected {} or {})",
                start + 2,
                start + 4,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::util::{yield_once, Sequencer};
    use core::sync::atomic::{AtomicUsize, Ordering};
    extern crate alloc;
    use alloc::{rc::Rc, vec, vec::Vec};

    /*  The purpose of these tests is to show that the algorithm works correctly regardless of
        thread scheduling and for any number of wakers.

        Exhaustively testing multithreaded code in this way would usually require using a tool such
        as loom to simulate all possible thread schedulings, but a much simpler approach is possible
        here. Because all inter-thread communication in the algorithm uses atomic operations on a
        single shared variable using `Ordering::SeqCst`, simulating every possible thread scheduling
        just requires running every possible sequence in which the atomic operations could occur.

        This can be done by writing an asynchronous version of the algorithm which yields between
        each atomic operation. Each thread participating in the algorithm is represented by one of
        these futures, and exploring every possible sequence in which these futures can be polled is
        equivalent to exploring every possible scheduling of the threads they represent.

        Running all of these sequences is done by the `Sequencer` type defined in the `util` module.
        When repeatedly given the same list of futures, it will poll them in a different sequence
        each time until all possible sequences have been exhausted. It does not respect waker
        invocation, instead it simply polls the next future in the sequence immediately without
        waiting, so it is not suitable as a general-purpose executor. The `yield_once` function in
        the same module allows defining points in which async code should yield control to its
        executor once.

        Of course, this method only shows that the algorithm is correct if the asynchronous
        implementation of the algorithm matches the actual implementation exposed by the crate. This
        is the reason why each 'step' (i.e. each atomic access) of the algorithm has been separated
        into its own function; it allows both implementations to use exactly the same code for each
        operation. You are encouraged to check for yourself that the rest of the algorithm matches
        between each implementation; it is just a couple of nested 'if' statements so this is simple
        to do.
    */

    /// async version of the task thread's side of the algorithm
    async fn new_task_thread(
        start: usize,
        counter: Rc<AtomicUsize>,
        reschedule: impl Fn(),
        is_pending: bool,
    ) {
        use crate::algorithm::task as steps;
        let counter = counter.as_ref();

        // see `Task::poll` in lib.rs for the equivalent synchronous implementation

        // if `Future::poll` returned `Pending`
        if is_pending {
            let waker_invoked = steps::was_waker_invoked(start, counter);
            yield_once().await; // yield to the sequencer to allow other "threads" to run

            // if a waker has been invoked
            if waker_invoked {
                // try to claim full ownership of the task
                if steps::claim_ownership(start, counter) {
                    // if successful then reschedule the task
                    reschedule();
                }
            }
        } else {
            // else the future returned `Ready`, so end the algorithm
            steps::task_complete(start, counter);
        }
    }

    /// async version of a waker's side of the algorithm
    async fn new_waker_thread(start: usize, counter: Rc<AtomicUsize>, reschedule: impl Fn()) {
        use crate::algorithm::waker as steps;
        let counter = counter.as_ref();

        // see `SmartWaker::wake_by_ref` in lib.rs for the equivalent synchronous implementation

        let should_proceed = steps::on_wake(start, counter);
        yield_once().await; // yield to the sequencer to allow other "threads" to run

        // if this waker is still valid, is the first to be invoked for this round, and
        // `Future::poll` has returned `Pending`
        if should_proceed {
            // try to claim full ownership of the task
            if steps::claim_ownership(start, counter) {
                // if successful then reschedule the task
                reschedule();
            }
        }
    }

    #[test]
    fn algorithm_test1() {
        //! Check that the algorithm always reschedules the task exactly once when:
        //!   - `Future::poll` returns `Pending`;
        //!   - N wakers were used this round, for each N in {1,2,3,4}; and
        //!   - there are no leftover wakers from previous rounds of the algorithm.

        for num_wakers in 1..=4 {
            let mut sequencer = Sequencer::new();

            loop {
                let start = 0;
                let counter = Rc::new(AtomicUsize::new(start));

                let times_rescheduled = Rc::new(AtomicUsize::new(0));
                let reschedule_counter = times_rescheduled.clone();
                let reschedule = move || {
                    reschedule_counter.fetch_add(1, Ordering::SeqCst);
                };

                let mut futures = Vec::with_capacity(num_wakers + 1);

                futures.push(Sequencer::prepare(new_task_thread(
                    start,
                    counter.clone(),
                    reschedule.clone(),
                    true,
                )));

                for _ in 0..num_wakers {
                    futures.push(Sequencer::prepare(new_waker_thread(
                        start,
                        counter.clone(),
                        reschedule.clone(),
                    )));
                }

                let done = sequencer.run_next_sequence(futures);

                assert!(times_rescheduled.load(Ordering::SeqCst) == 1);

                if done {
                    break;
                }
            }
        }
    }

    #[test]
    fn algorithm_test2() {
        //! Check that the algorithm never reschedules the task when:
        //!   - `Future::poll` returns `Pending`,
        //!   - no wakers were used this round, and
        //!   - there are no leftover wakers from previous rounds of the algorithm.

        let mut sequencer = Sequencer::new();

        loop {
            let start = 0;
            let counter = Rc::new(AtomicUsize::new(start));

            let times_rescheduled = Rc::new(AtomicUsize::new(0));
            let reschedule_counter = times_rescheduled.clone();
            let reschedule = move || {
                reschedule_counter.fetch_add(1, Ordering::SeqCst);
            };

            let done = sequencer.run_next_sequence(vec![Sequencer::prepare(new_task_thread(
                start,
                counter.clone(),
                reschedule.clone(),
                true,
            ))]);

            assert!(times_rescheduled.load(Ordering::SeqCst) == 0);

            if done {
                break;
            }
        }
    }

    #[test]
    fn algorithm_test3() {
        //! Check that the algorithm never reschedules the task when:
        //!   - `Future::poll` returns `Ready`;
        //!   - N wakers were used this round, for each N in {0, 1}; and
        //!   - there are no leftover wakers from previous rounds of the algorithm.

        for num_wakers in 0..=1 {
            let mut sequencer = Sequencer::new();

            loop {
                let start = 0;
                let counter = Rc::new(AtomicUsize::new(start));

                let times_rescheduled = Rc::new(AtomicUsize::new(0));
                let reschedule_counter = times_rescheduled.clone();
                let reschedule = move || {
                    reschedule_counter.fetch_add(1, Ordering::SeqCst);
                };

                let mut futures = Vec::with_capacity(num_wakers + 1);

                futures.push(Sequencer::prepare(new_task_thread(
                    start,
                    counter.clone(),
                    reschedule.clone(),
                    false,
                )));

                for _ in 0..num_wakers {
                    futures.push(Sequencer::prepare(new_waker_thread(
                        start,
                        counter.clone(),
                        reschedule.clone(),
                    )));
                }

                let done = sequencer.run_next_sequence(futures);

                assert!(times_rescheduled.load(Ordering::SeqCst) == 0);

                if done {
                    break;
                }
            }
        }
    }

    #[test]
    fn algorithm_test4() {
        //! Check that a waker whose 'start' value is less than that of the current round will
        //! not reschedule the task when invoked, when:
        //!   - there are N outdated wakers, for each N in {1, 2};
        //!   - each outdated waker's start value is N less than this round's, for each N in {4, 8};
        //!   - `Future::poll` returns either `Ready` or `Pending`; and
        //!   - N wakers were used in the current round, for each N in {0, 1, 2}.

        let old_wakers = [
            (4, None),
            (4, Some(4)),
            (8, None),
            (8, Some(4)),
            (8, Some(8)),
        ];

        for old_wakers in old_wakers {
            for is_pending in [true, false] {
                for num_wakers in 0..=2 {
                    let mut sequencer = Sequencer::new();

                    loop {
                        let start = 20;
                        let counter = Rc::new(AtomicUsize::new(start));

                        let times_rescheduled = Rc::new(AtomicUsize::new(0));
                        let reschedule_counter = times_rescheduled.clone();
                        let reschedule = move || {
                            reschedule_counter.fetch_add(1, Ordering::SeqCst);
                        };

                        let mut futures = Vec::with_capacity(num_wakers + 1);

                        futures.push(Sequencer::prepare(new_task_thread(
                            start,
                            counter.clone(),
                            reschedule.clone(),
                            is_pending,
                        )));

                        for _ in 0..num_wakers {
                            futures.push(Sequencer::prepare(new_waker_thread(
                                start,
                                counter.clone(),
                                reschedule.clone(),
                            )));
                        }

                        futures.push(Sequencer::prepare(new_waker_thread(
                            start - old_wakers.0,
                            counter.clone(),
                            || panic!("outdated waker called reschedule()"),
                        )));

                        if let Some(diff) = old_wakers.1 {
                            futures.push(Sequencer::prepare(new_waker_thread(
                                start - diff,
                                counter.clone(),
                                || panic!("outdated waker called reschedule()"),
                            )))
                        }

                        let done = sequencer.run_next_sequence(futures);

                        match times_rescheduled.load(Ordering::SeqCst) {
                            n if n == if is_pending && num_wakers > 0 { 1 } else { 0 } => (),
                            n => panic!(
                                "incorrectly rescheduled {n} times\n\
                                is_pending == {is_pending},\n\
                                num_wakers == {num_wakers}\n,
                                old_wakers == {:?}\n",
                                old_wakers
                            ),
                        };

                        if done {
                            break;
                        }
                    }
                }
            }
        }
    }
}
