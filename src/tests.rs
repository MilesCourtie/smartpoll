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
    use core::task::Waker;
    use loom::{
        sync::mpsc::{channel, Sender},
        thread,
    };

    struct Fut {
        polls_remaining: u8,
        waker_tx: Sender<Waker>,
    }
    impl Fut {
        fn new(polls_remaining: u8, waker_tx: Sender<Waker>) -> Self {
            Self {
                polls_remaining,
                waker_tx,
            }
        }
    }
    impl Future for Fut {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.polls_remaining > 0 {
                self.polls_remaining -= 1;
                self.waker_tx
                    .send(cx.waker().clone())
                    .expect("channel closed");
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }

    fn test_body() {
        let (task_tx, task_rx) = channel();

        let reschedule_fn = {
            let task_tx = task_tx.clone();
            move |task| {
                task_tx.send(task).expect("channel disconnected");
            }
        };

        let (waker_tx, waker_rx) = channel::<Waker>();

        let _waker_thread = thread::spawn(move || {
            while let Ok(waker) = waker_rx.recv() {
                waker.wake();
            }
        });

        let task = Task::new(Fut::new(3, waker_tx));

        task_tx.send(task).expect("channel disconnected");

        loop {
            match task_rx.recv() {
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
