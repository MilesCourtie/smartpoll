use crate::{
    algorithm,
    tests::util::{yield_once, Sequencer},
};
use core::sync::atomic::AtomicUsize;
extern crate alloc;
use alloc::{rc::Rc, vec};

#[test]
fn correctness() {
    async fn new_task_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::task as steps;
        let counter = counter.as_ref();

        let waker_invoked = steps::was_waker_invoked(start, counter);
        yield_once().await;

        if waker_invoked {
            let permission_to_reschedule = steps::claim_ownership(start, counter);
            if permission_to_reschedule {
                /* TODO
                println!("  task thread reschedules the task");
                */
            }
        }
    }

    async fn new_waker_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::waker as steps;
        let counter = counter.as_ref();

        let should_proceed = steps::on_wake(start, counter);
        yield_once().await;

        if should_proceed {
            let should_reschedule = steps::claim_ownership(start, counter);
            if should_reschedule {
                /* TODO
                println!("  waker thread reschedules the task");
                */
            }
        }
    }

    let mut sequencer = Sequencer::new();

    loop {
        let start = 0;
        let counter = Rc::new(AtomicUsize::new(start));

        let done = sequencer.run_next_sequence(vec![
            Sequencer::prepare(new_task_thread(start, counter.clone())),
            Sequencer::prepare(new_waker_thread(start, counter.clone())),
            Sequencer::prepare(new_waker_thread(start, counter.clone())),
        ]);

        if done {
            break;
        }
    }
}
