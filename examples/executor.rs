//! This example demonstrates how to implement a simple multithreaded executor using just Smartpoll
//! and the standard library.

use smartpoll::Task;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

fn main() {
    let mut executor = Executor::new();
    let handle = executor.handle();
    handle.spawn_task(async {
        println!("hello from task 1");
    });
    handle.spawn_task(async {
        println!("hello from task 2");
    });
    handle.spawn_task(async {
        println!("hello from task 3");
    });
    executor.run_while(|| true);
}

struct Executor {
    handle: ExecutorHandle,
    workers: Vec<Option<Worker>>,
    queue_rx: mpsc::Receiver<Task>,
    num_active_tasks: Arc<AtomicUsize>,
    next_worker: usize,
}

/// handle for spawning tasks
#[derive(Clone)]
struct ExecutorHandle {
    queue_tx: mpsc::Sender<Task>,
    num_active_tasks: Arc<AtomicUsize>,
}

struct Worker {
    task_tx: mpsc::SyncSender<Task>,
    join_handle: thread::JoinHandle<()>,
}

impl Executor {
    fn new() -> Self {
        let num_workers: usize = thread::available_parallelism().unwrap().into();

        // the main work queue
        let (queue_tx, queue_rx) = mpsc::channel::<Task>();

        // function that pushes a task onto the work queue
        let reschedule_fn = {
            let queue_tx = queue_tx.clone();
            move |task| queue_tx.send(task).unwrap()
        };

        let workers = (0..num_workers)
            .map(|_| {
                let reschedule_fn = reschedule_fn.clone();

                // channel to send tasks to this worker
                let (task_tx, task_rx) = mpsc::sync_channel::<Task>(1);

                let join_handle = thread::spawn(move || {
                    // poll tasks until the channel closes
                    while let Ok(task) = task_rx.recv() {
                        task.poll(reschedule_fn.clone())
                    }
                });
                Some(Worker {
                    task_tx,
                    join_handle,
                })
            })
            .collect::<Vec<_>>();

        let num_active_tasks = Arc::new(AtomicUsize::new(0));

        let handle = ExecutorHandle {
            queue_tx,
            num_active_tasks: num_active_tasks.clone(),
        };

        Self {
            handle,
            workers,
            queue_rx,
            num_active_tasks,
            next_worker: 0,
        }
    }

    fn handle(&self) -> ExecutorHandle {
        self.handle.clone()
    }

    fn run_while(&mut self, mut condition: impl FnMut() -> bool) {
        while condition() && self.num_active_tasks.load(Ordering::SeqCst) > 0 {
            match self.queue_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(task) => {
                    // keep trying to send the task to the next free worker
                    let mut task = Some(task);
                    while let Err(mpsc::TrySendError::Full(returned_task)) = self.workers
                        [self.next_worker]
                        .as_ref()
                        .unwrap()
                        .task_tx
                        .try_send(task.take().unwrap())
                    {
                        task = Some(returned_task);
                        self.next_worker += 1;
                        if self.next_worker == self.workers.len() {
                            self.next_worker = 0;
                        }
                    }
                    {}
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("all work queue senders have disconnected");
                }
            }
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        for worker in self.workers.iter_mut() {
            let Worker {
                task_tx,
                join_handle,
            } = worker.take().unwrap();
            drop(task_tx);
            let worker_is_panicking = join_handle.join().is_err();
            if worker_is_panicking && !thread::panicking() {
                panic!("worker panicked");
            }
        }
    }
}

impl ExecutorHandle {
    fn spawn_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        let num_active_tasks = self.num_active_tasks.clone();
        num_active_tasks.fetch_add(1, Ordering::SeqCst);
        self.queue_tx
            .send(Task::new(async move {
                future.await;
                num_active_tasks.fetch_sub(1, Ordering::SeqCst);
            }))
            .unwrap();
    }
}
