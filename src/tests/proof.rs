use crate::{
    algorithm,
    tests::util::{interleave_futures, yield_now},
};
use core::{future::Future, pin::Pin, sync::atomic::AtomicUsize};
extern crate alloc;
use alloc::rc::Rc;

#[test]
#[allow(dead_code)]
fn permute_steps() {
    async fn new_task_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::task as steps;
        let counter = counter.as_ref();

        let waker_invoked = steps::was_waker_invoked(start, counter);
        yield_now().await;

        if waker_invoked {
            let permission_to_reschedule = steps::attempt_reschedule(start, counter);
            if permission_to_reschedule {
                println!("task thread has permission to reschedule");
            }
        }
        println!("task thread complete");
    }

    async fn new_waker_thread(start: usize, counter: Rc<AtomicUsize>) {
        use algorithm::waker as steps;
        let counter = counter.as_ref();

        let (first_waker, poll_completed) = steps::on_wake(start, counter);
        yield_now().await;

        if first_waker && poll_completed {
            let permission_to_reschedule = steps::attempt_reschedule(start, counter);
            if permission_to_reschedule {
                println!("waker thread has permission to reschedule");
            }
        }
        println!("waker thread complete");
    }

    interleave_futures(|| {
        let start = 0;
        let counter = Rc::new(AtomicUsize::new(start));
        vec![
            Box::pin(new_task_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
            Box::pin(new_waker_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
            Box::pin(new_waker_thread(start, counter.clone())) as Pin<Box<dyn Future<Output = ()>>>,
        ]
    });
}
