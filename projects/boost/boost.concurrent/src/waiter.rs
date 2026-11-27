//! 计数等待组。

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// 计数等待组：`add` 增加待完成数，`done` 递减，`wait` 阻塞到归零。
#[derive(Clone, Debug, Default)]
pub struct Waiter(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    pending: Mutex<usize>,
    drained: Condvar,
}

impl Waiter {
    /// 创建零待完成数的等待组。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个待完成任务。
    pub fn add(&self) {
        *lock(&self.0.pending) += 1;
    }

    /// 完成任务；归零时唤醒所有等待者。
    pub fn done(&self) {
        let mut pending = lock(&self.0.pending);
        *pending = pending.saturating_sub(1);
        if *pending == 0 {
            self.0.drained.notify_all();
        }
    }

    /// 阻塞到待完成数归零。
    pub fn wait(&self) {
        let mut pending = lock(&self.0.pending);
        while *pending > 0 {
            pending = self.0.drained.wait(pending).unwrap_or_else(|poison| poison.into_inner());
        }
    }

    /// 当前待完成数。
    pub fn pending(&self) -> usize {
        *lock(&self.0.pending)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn wait_returns_after_every_done() {
        let waiter = Waiter::new();
        for _ in 0..4 {
            waiter.add();
            let clone = waiter.clone();
            thread::spawn(move || clone.done());
        }
        waiter.wait();
        assert_eq!(waiter.pending(), 0);
    }
}
