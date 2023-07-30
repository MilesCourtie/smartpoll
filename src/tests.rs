#![allow(dead_code)]

use crate::Task;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Tests using `Task::poll()` with the simplest possible future.
#[cfg(not(loom))]
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
#[cfg(not(loom))]
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

/// Tests that when a single waker is used, a task is never rescheduled until after poll() returns.
#[cfg(loom)]
#[test]
fn test_2() {
    use loom::{sync::mpsc::channel, thread};

    struct Fut {
        polls_remaining: u8,
    }
    impl Fut {
        fn new(polls_remaining: u8) -> Self {
            Self { polls_remaining }
        }
    }
    impl Future for Fut {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let output = if self.polls_remaining > 0 {
                self.polls_remaining -= 1;
                let waker = cx.waker().clone();
                thread::spawn(move || {
                    waker.wake();
                });
                Poll::Pending
            } else {
                Poll::Ready(())
            };
            output
        }
    }

    fn test_body() {
        let (tx, rx) = channel();

        let reschedule_fn = {
            let tx = tx.clone();
            move |task| {
                tx.send(task).expect("channel disconnected");
            }
        };

        let task = Task::new(Fut::new(1));
        tx.send(task).expect("channel disconnected");

        loop {
            match rx.recv() {
                Ok(task) => {
                    if task.poll(reschedule_fn.clone()) {
                        break;
                    }
                }
                Err(_) => {
                    panic!("channel disconnected");
                }
            }
        }
    }

    loom::model(test_body);
}
