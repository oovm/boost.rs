//! Boost 家族的确定性作用域并行。
//!
//! 分块由共享游标动态领取，[`map`] 的结果按输入下标回填并排序，因此输出
//! 顺序与串行一致；[`for_each_range`] 在块边界检查 [`Cancel`]，让长任务
//! 在读下一块内存之前就退出。借用调用方数据，不需要 `'static`，也不使用
//! `unsafe`。

#![doc = include_str!("readme.md")]
#![deny(missing_docs)]

use std::{
    ops::Range,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
};

pub use boost_concurrent::Cancel;

/// 返回实际使用的工作线程数，语义同 [`boost_concurrent::threads`]。
pub fn threads(requested: usize) -> usize {
    boost_concurrent::threads(requested)
}

/// 并行遍历切片元素；副作用顺序不作保证。
pub fn for_each<T, F>(items: &[T], threads: usize, body: F)
where
    T: Sync,
    F: Fn(&T) + Sync,
{
    if items.is_empty() {
        return;
    }
    let workers = worker_count(items.len(), threads);
    let chunk = chunk_size(items.len(), workers);
    let cursor = AtomicUsize::new(0);
    let run = |_worker: usize| {
        loop {
            let start = cursor.fetch_add(chunk, Ordering::Relaxed);
            if start >= items.len() {
                break;
            }
            for item in &items[start..(start + chunk).min(items.len())] {
                body(item);
            }
        }
    };
    dispatch(workers, &run);
}

/// 并行映射，返回与 `items` 同序的结果。
pub fn map<T, R, F>(items: &[T], threads: usize, body: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = worker_count(items.len(), threads);
    let chunk = chunk_size(items.len(), workers);
    let cursor = AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(items.len()));
    let run = |_worker: usize| {
        let mut local: Vec<(usize, R)> = Vec::new();
        loop {
            let start = cursor.fetch_add(chunk, Ordering::Relaxed);
            if start >= items.len() {
                break;
            }
            for index in start..(start + chunk).min(items.len()) {
                local.push((index, body(index, &items[index])));
            }
        }
        if !local.is_empty() {
            lock(&collected).extend(local);
        }
    };
    dispatch(workers, &run);
    let mut pairs = collected.into_inner().unwrap_or_else(|poison| poison.into_inner());
    pairs.sort_unstable_by_key(|(index, _)| *index);
    pairs.into_iter().map(|(_, value)| value).collect()
}

/// 按下标区间并行遍历，每块拿到连续区间；取消后不再领取新块。
pub fn for_each_range<F>(len: usize, threads: usize, cancel: &Cancel, body: F)
where
    F: Fn(Range<usize>, &Cancel) + Sync,
{
    if len == 0 {
        return;
    }
    let workers = worker_count(len, threads);
    let chunk = chunk_size(len, workers);
    let cursor = AtomicUsize::new(0);
    let run = |_worker: usize| {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let start = cursor.fetch_add(chunk, Ordering::Relaxed);
            if start >= len {
                break;
            }
            body(start..(start + chunk).min(len), cancel);
        }
    };
    dispatch(workers, &run);
}

fn worker_count(len: usize, threads: usize) -> usize {
    boost_concurrent::threads(threads).min(len.max(1))
}

fn chunk_size(len: usize, workers: usize) -> usize {
    len.div_ceil(workers.max(1) * 8).max(1)
}

fn dispatch(workers: usize, body: &(dyn Fn(usize) + Sync)) {
    if workers <= 1 {
        body(0);
        return;
    }
    std::thread::scope(|scope| {
        for worker in 0..workers {
            scope.spawn(move || body(worker));
        }
    });
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
    fn map_keeps_input_order() {
        let items: Vec<usize> = (0..1_000).collect();
        let doubled = map(&items, 4, |index, item| index * 1_000 + item);
        assert_eq!(doubled.len(), items.len());
        for (index, value) in doubled.iter().enumerate() {
            assert_eq!(*value, index * 1_000 + index);
        }
        assert_eq!(map::<usize, usize, _>(&[], 4, |_, item| *item), Vec::<usize>::new());
    }

    #[test]
    fn for_each_visits_every_item_once() {
        let items: Vec<usize> = (0..10_000).collect();
        let seen = Arc::new(AtomicUsize::new(0));
        for_each(&items, 4, |_| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(seen.load(Ordering::Relaxed), items.len());
    }

    #[test]
    fn for_each_range_stops_after_cancel() {
        let cancel = Cancel::new();
        let touched = AtomicUsize::new(0);
        for_each_range(1_000_000, 4, &cancel, |range, token| {
            if range.start >= 1_000 {
                token.cancel();
            }
            touched.fetch_add(range.len(), Ordering::Relaxed);
        });
        assert!(cancel.is_cancelled());
        // 取消后最多再跑完已被认领的块，远小于全部区间。
        assert!(touched.load(Ordering::Relaxed) < 1_000_000);
    }

    #[test]
    fn single_thread_path_matches_parallel_output() {
        let items: Vec<u64> = (0..257).map(|value| value * 3).collect();
        let serial = map(&items, 1, |_, item| *item + 1);
        let parallel = map(&items, 8, |_, item| *item + 1);
        assert_eq!(serial, parallel);
    }
}
