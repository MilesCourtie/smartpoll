use crate::{
    algorithm,
    tests::util::{interleave_futures, yield_now},
};
use core::{future::Future, pin::Pin, sync::atomic::AtomicUsize};
extern crate alloc;
use alloc::rc::Rc;

#[test]
#[allow(dead_code)]
fn exhaustive_proof_2_wakers() {
    async fn new_task_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::task as steps;
        let counter = counter.as_ref();

        let waker_invoked = steps::was_waker_invoked(start, counter);
        yield_now().await;

        if waker_invoked {
            let permission_to_reschedule = steps::attempt_reschedule(start, counter);
            if permission_to_reschedule {
                println!("  task thread reschedules the task");
            }
        }
    }

    async fn new_waker_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::waker as steps;
        let counter = counter.as_ref();

        let should_proceed = steps::on_wake(start, counter);
        yield_now().await;

        if should_proceed {
            let should_reschedule = steps::attempt_reschedule(start, counter);
            if should_reschedule {
                println!("  waker thread reschedules the task");
            }
        }
    }

    interleave_futures(|| {
        println!("next execution");
        let start = 0;
        let counter = Rc::new(AtomicUsize::new(start));
        vec![
            Box::pin(new_task_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
            Box::pin(new_waker_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
            Box::pin(new_waker_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
        ]
    });
}
