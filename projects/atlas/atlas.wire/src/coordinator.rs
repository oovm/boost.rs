//! 跨线程解析协调：只在有证据时判定环，不依赖超时。
//!
//! 单个线程内的递归由解析栈直接发现。两个线程互相等待对方的 provider 时，线程本地栈看不
//! 到对方，因此本模块把「谁正在初始化哪个键」和「谁阻塞在哪个键上」放进同一份全局状态，
//! 让每个即将阻塞的线程沿 owner → waiting 链回溯，命中自身的解析链就立即返回环错误，而不
//! 是死锁后依赖超时。

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    thread::{self, ThreadId},
};

use crate::{
    error::{WireError, WireErrorKind},
    key::TypeKey,
};

#[derive(Default)]
struct State {
    owners: HashMap<TypeKey, ThreadId>,
    waiting: HashMap<ThreadId, TypeKey>,
}

/// 环境内共享的解析协调器。
pub(crate) struct Coordinator {
    state: Mutex<State>,
}

impl Coordinator {
    pub(crate) fn new() -> Self {
        Self { state: Mutex::new(State::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 记录当前线程开始初始化 `key`。
    pub(crate) fn enter(&self, key: TypeKey) {
        self.lock().owners.insert(key, thread::current().id());
    }

    /// 记录当前线程结束初始化 `key`。
    pub(crate) fn leave(&self, key: TypeKey) {
        let mut state = self.lock();
        state.owners.remove(&key);
        let thread = thread::current().id();
        if state.waiting.get(&thread) == Some(&key) {
            state.waiting.remove(&thread);
        }
    }

    /// 登记当前线程即将阻塞在 `key` 上，并检测是否会形成环。
    ///
    /// 返回 `Err` 时调用方不得阻塞，且本方法已经撤销登记。
    pub(crate) fn begin_wait(&self, key: TypeKey) -> Result<(), WireError> {
        let thread = thread::current().id();
        let mut state = self.lock();
        if state.owners.get(&key) == Some(&thread) {
            return Err(WireError::new(WireErrorKind::Cycle, "同一线程重复请求正在初始化的 provider"));
        }
        let previous = state.waiting.insert(thread, key);
        let mut cursor = state.owners.get(&key).copied();
        let limit = state.owners.len() + state.waiting.len() + 1;
        let mut steps = 0usize;
        while let Some(owner) = cursor {
            if owner == thread {
                state.waiting.remove(&thread);
                return Err(WireError::new(WireErrorKind::Cycle, "多个线程互相等待对方的 provider，装配图存在环"));
            }
            let Some(next) = state.waiting.get(&owner).copied()
            else {
                break;
            };
            if next == key {
                state.waiting.remove(&thread);
                return Err(WireError::new(WireErrorKind::Cycle, "多个线程互相等待对方的 provider，装配图存在环"));
            }
            cursor = state.owners.get(&next).copied();
            steps += 1;
            if steps > limit {
                break;
            }
        }
        if previous.is_some() {
            // 线程只允许有一个等待目标；恢复旧登记，避免覆盖别的解析帧。
            state.waiting.remove(&thread);
            return Err(WireError::new(WireErrorKind::Cycle, "同一线程重复登记等待目标"));
        }
        Ok(())
    }

    /// 撤销当前线程的等待登记。
    pub(crate) fn end_wait(&self) {
        self.lock().waiting.remove(&thread::current().id());
    }
}
