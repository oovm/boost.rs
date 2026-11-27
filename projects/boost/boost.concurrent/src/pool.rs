//! 固定线程执行器。

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
};

use crate::threads;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 固定线程数的任务执行器。
///
/// 任务按提交流程排队，任意工作线程取到即执行；[`close`](Self::close) 关闭
/// 入口后等待已入队任务与工作线程全部结束。任务恐慌被隔离在工作线程内。
pub struct Pool {
    sender: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    size: usize,
}

impl Pool {
    /// 创建线程池；`requested` 语义同 [`crate::threads`]。
    pub fn new(requested: usize) -> Self {
        let size = threads(requested);
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = (0..size)
            .map(|index| {
                let receiver = Arc::clone(&receiver);
                thread::Builder::new()
                    .name(format!("boost-pool-{index}"))
                    .spawn(move || {
                        loop {
                            let job = lock(&receiver).recv();
                            match job {
                                Ok(job) => {
                                    let _ = catch_unwind(AssertUnwindSafe(job));
                                }
                                Err(_) => break,
                            }
                        }
                    })
                    .expect("boost-pool: spawn worker")
            })
            .collect();
        Self { sender: Some(sender), workers, size }
    }

    /// 工作线程数。
    pub fn threads(&self) -> usize {
        self.size
    }

    /// 提交一个任务；入口已关闭时返回 `false`，任务不会被调用。
    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) -> bool {
        match &self.sender {
            Some(sender) => sender.send(Box::new(job)).is_ok(),
            None => false,
        }
    }

    /// 入口是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.sender.is_none()
    }

    /// 关闭入口，排空已入队任务并回收工作线程。
    pub fn close(&mut self) {
        self.sender = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Pool").field("threads", &self.size).field("closed", &self.is_closed()).finish()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn every_job_runs_before_close_returns() {
        let mut pool = Pool::new(4);
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let counter = Arc::clone(&counter);
            assert!(pool.spawn(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }));
        }
        pool.close();
        assert_eq!(counter.load(Ordering::Relaxed), 64);
        assert!(pool.is_closed());
        assert!(!pool.spawn(|| {}));
    }

    #[test]
    fn panicking_job_does_not_kill_the_pool() {
        let mut pool = Pool::new(2);
        let counter = Arc::new(AtomicUsize::new(0));
        pool.spawn(|| panic!("intentional"));
        let counter_clone = Arc::clone(&counter);
        assert!(pool.spawn(move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        }));
        pool.close();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
}
