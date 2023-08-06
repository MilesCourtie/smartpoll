use crate::algorithm;
use core::{future::Future, pin::pin, sync::atomic::AtomicUsize, task::Context};

mod util;
use util::{noop_waker, yield_now};

#[test]
fn permute_steps() {
    let new_task_thread = |start, counter| async move {
        use algorithm::task as steps;

        let waker_invoked = steps::was_waker_invoked(start, counter);
        yield_now().await;

        if waker_invoked {
            let permission_to_reschedule = steps::attempt_reschedule(start, counter);
            if permission_to_reschedule {
                println!("task thread has permission to reschedule");
            }
        }
        println!("task thread complete");
    };

    let new_waker_thread = |start, counter| async move {
        use algorithm::waker as steps;

        let (first_waker, poll_completed) = steps::on_wake(start, counter);
        yield_now().await;

        if first_waker && poll_completed {
            let permission_to_reschedule = steps::attempt_reschedule(start, counter);
            if permission_to_reschedule {
                println!("waker thread has permission to reschedule");
            }
        }
        println!("waker thread complete");
    };

    let start = 0;
    let counter = AtomicUsize::new(start);

    let mut task_thread = pin!(new_task_thread(start, &counter));
    let mut waker_thread_a = pin!(new_waker_thread(start, &counter));
    let mut waker_thread_b = pin!(new_waker_thread(start, &counter));

    let waker = noop_waker();
    let mut _schedule_task_thread = {
        let f = &mut task_thread;
        let mut cx = Context::from_waker(&waker);
        move || f.as_mut().poll(&mut cx).is_ready()
    };
    let mut _schedule_waker_thread_a = {
        let f = &mut waker_thread_a;
        let mut cx = Context::from_waker(&waker);
        move || f.as_mut().poll(&mut cx).is_ready()
    };
    let mut _schedule_waker_thread_b = {
        let f = &mut waker_thread_b;
        let mut cx = Context::from_waker(&waker);
        move || f.as_mut().poll(&mut cx).is_ready()
    };

    // TODO simulate all possible executions of the three threads

    println!("done");
}
