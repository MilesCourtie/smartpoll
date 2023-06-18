//! This example demonstrates the simplest possible usage of Smartpoll.
//! A future that prints "hello world!" is wrapped in a [`Task`] and polled using [`Task::poll`].
//!
//! Polling a [`Task`] requires providing a callback that will be invoked when the task is ready
//! to be rescheduled. The function `recursive_poll` is used, which re-polls the task immediately
//! using itself as the callback.
//!
//! Note that the rescheduling callback may be invoked directly by a waker, so polling the future
//! directly within the callback is generally not advisable as it prevents your executor from
//! controlling which threads the future runs on.

use smartpoll::Task;

fn main() {
    let task = Task::new(async {
        println!("hello world!");
    });
    task.poll(recursive_poll);
}

fn recursive_poll(task: Task) {
    task.poll(recursive_poll);
}
