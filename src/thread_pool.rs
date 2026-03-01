use crossbeam_deque::{Injector, Worker};
use std::{
    iter,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Parker {
    tokens: Mutex<usize>,
    condvar: Condvar,
    max_tokens: usize,
}

impl Parker {
    fn new(max_tokens: usize) -> Self {
        Self {
            tokens: Mutex::new(0),
            condvar: Condvar::new(),
            max_tokens,
        }
    }

    fn wait(&self) {
        let mut tokens = self.tokens.lock().unwrap();
        while *tokens == 0 {
            tokens = self.condvar.wait(tokens).unwrap();
        }
        *tokens -= 1;
    }

    fn notify_one(&self) {
        let mut tokens = self.tokens.lock().unwrap();
        if *tokens < self.max_tokens {
            *tokens += 1;
            self.condvar.notify_one();
        }
    }
}

pub struct ThreadPool {
    injector: Arc<Injector<Job>>,
    parker: Arc<Parker>,
    pending: Arc<AtomicUsize>,
    queue_size: usize,
}

impl ThreadPool {
    pub fn new(size: usize, queue_size: usize) -> Self {
        let injector = Arc::new(Injector::<Job>::new());
        let parker = Arc::new(Parker::new(size));
        let pending = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::with_capacity(size);
        let mut stealers = Vec::with_capacity(size);

        for _ in 0..size {
            let worker = Worker::new_fifo();
            stealers.push(worker.stealer());
            workers.push(worker);
        }

        for worker in workers {
            let injector = Arc::clone(&injector);
            let parker = Arc::clone(&parker);
            let stealers = stealers.clone();
            let pending = Arc::clone(&pending);

            thread::spawn(move || {
                loop {
                    let task = worker.pop().or_else(|| {
                        iter::repeat_with(|| {
                            injector
                                .steal_batch_and_pop(&worker)
                                .or_else(|| stealers.iter().map(|s| s.steal()).collect())
                        })
                        .find(|s| !s.is_retry())
                        .and_then(|s| s.success())
                    });

                    match task {
                        Some(task) => {
                            pending.fetch_sub(1, Ordering::SeqCst);
                            let _ = catch_unwind(AssertUnwindSafe(|| {
                                task();
                            }));
                        }
                        None => {
                            let mut spun = false;
                            for _ in 0..64 {
                                if pending.load(Ordering::Relaxed) > 0 {
                                    spun = true;
                                    break;
                                }
                                std::hint::spin_loop();
                            }
                            if !spun {
                                parker.wait();
                            }
                        }
                    }
                }
            });
        }

        Self {
            injector,
            parker,
            pending,
            queue_size,
        }
    }

    pub fn execute<F>(&self, f: F) -> Result<(), std::sync::mpsc::TrySendError<Job>>
    where
        F: FnOnce() + Send + 'static,
    {
        if self.pending.load(Ordering::Relaxed) >= self.queue_size {
            return Err(std::sync::mpsc::TrySendError::Full(Box::new(f)));
        }

        self.pending.fetch_add(1, Ordering::SeqCst);
        self.injector.push(Box::new(f));
        self.parker.notify_one();
        Ok(())
    }
}
