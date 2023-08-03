use crate::Task;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Tests using `Task::poll()` with the simplest possible future.
#[test]
fn test_0() {
    struct Fut;
    impl Future for Fut {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(())
        }
    }
    fn recursive_poll(task: Task) {
        task.poll(recursive_poll);
    }
    recursive_poll(Task::new(Fut));
}

/// Tests using `Task` to poll a future which doesn't immediately complete.
#[test]
fn test_1() {
    struct Fut(u8);
    impl Future for Fut {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 > 0 {
                self.0 -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
    fn recursive_poll(task: Task) {
        task.poll(recursive_poll);
    }
    recursive_poll(Task::new(Fut(2)));
}
