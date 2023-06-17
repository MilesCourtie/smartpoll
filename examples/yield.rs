//! This example demonstrates using `smartpoll` to run a future that sometimes yields control to the
//! executor.

use smartpoll::Task;
use core::{future::Future, pin::Pin, task::Context, task::Poll};

fn main() {
    let task = Task::new(async {
        println!("1");
        yield_now().await;
        println!("2");
        async {
            println!("3");
            yield_now().await;
            println!("4");
        }
        .await;
        println!("5");
    });
    task.poll(recursive_poll);
}

fn recursive_poll(task: Task) {
    println!("rescheduling...");
    task.poll(recursive_poll);
}

/// Returns a future which yields control to the executor once.
fn yield_now() -> Yield {
    Yield(false)
}
struct Yield(bool);
impl Future for Yield {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}
