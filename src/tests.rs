use crate::Task;
use core::{
    future::Future,
    hint::black_box,
    pin::Pin,
    task::{Context, Poll},
};

mod common {
    use crate::Task;
    use core::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// A simple helper for asserting execution order using an atomic counter.
    /// Uses `SeqCst` memory ordering.
    #[derive(Clone)]
    pub struct ExecutionOrder(Arc<AtomicUsize>);
    impl ExecutionOrder {
        pub fn new() -> Self {
            Self(Arc::new(AtomicUsize::new(0)))
        }
        pub fn assert(&self, val: usize) {
            assert!(
                self.0
                    .compare_exchange(val, val + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok(),
                "out of order: {val}"
            );
        }
    }

    /// A `Future` type which can be used to yield control to the executor once.
    /// The first time it is polled it will invoke the waker and then return `Poll::Pending`.
    /// The second time it is polled it will return `Poll::Ready(())`.
    pub struct Yield(bool);
    impl Yield {
        pub fn new() -> Self {
            Self(false)
        }
    }
    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// The simplest possible reschedule callback that can be passed to `Task::poll()`.
    /// When invoked, it polls the task immediately using itself as the reschedule callback.
    pub fn recursive_poll(task: Task) {
        task.poll(recursive_poll);
    }
}

/// Test that `Task`s can be used to poll a future that returns both `Pending` and `Ready`, and
/// contains another future that also does so.
/// This also tests whether creating, invoking and dropping a `SmartWaker` works correctly and
/// does not panic.
#[test]
fn basic_usage() {
    use common::*;
    let order = ExecutionOrder::new();

    let task = {
        let order = order.clone();
        Task::new(async move {
            order.assert(1);
            Yield::new().await;
            order.assert(2);
            let fut = async {
                order.assert(4);
                Yield::new().await;
                order.assert(5);
            };
            order.assert(3);
            fut.await;
            order.assert(6);
        })
    };

    order.assert(0);
    task.poll(recursive_poll);
    order.assert(7);
}

/// Test that cloning a `SmartWaker` does not panic.
#[test]
fn clone_waker() {
    use common::*;

    struct F;
    impl Future for F {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let waker = cx.waker().clone();
            black_box(waker); // act as if the waker is used in some way
            Poll::Ready(())
        }
    }

    Task::new(F).poll(recursive_poll);
}
