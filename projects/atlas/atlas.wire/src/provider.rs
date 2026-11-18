//! provider 的擦除形式、缓存槽与上下文槽位。

use std::{
    any::Any,
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

use crate::{
    coordinator::Coordinator,
    dependency::Dependency,
    error::{WireError, WireErrorKind},
    key::TypeKey,
    resolver::Resolver,
    scope::Scope,
};

/// 擦除后的工厂：读取受限 resolver，返回装箱值。
///
/// 共享作用域把返回值转成 `Arc` 缓存；transient 直接交付装箱值。两种作用域共用同一签名，
/// 以便生成代码与手写 provider 走完全相同的路径。
pub(crate) type RunFactory = dyn Fn(&mut Resolver<'_>) -> Result<Box<dyn Any + Send + Sync>, WireError> + Send + Sync;

/// 从缓存值复制一份交付。共享作用域要求输出类型 `Clone`，由注册函数保证。
pub(crate) type CloneValue = dyn Fn(&(dyn Any + Send + Sync)) -> Result<Box<dyn Any + Send + Sync>, WireError> + Send + Sync;

/// 已注册 provider。
pub(crate) struct ProviderEntry {
    pub(crate) scope: Scope,
    pub(crate) label: &'static str,
    pub(crate) dependencies: Vec<Dependency>,
    pub(crate) run: Box<RunFactory>,
    pub(crate) clone_value: Option<Box<CloneValue>>,
    pub(crate) slot: Arc<Slot>,
}

/// 冻结图中的一项。
#[derive(Clone)]
pub(crate) enum Entry {
    Provider(Arc<ProviderEntry>),
    Slot(SlotEntry),
}

impl Entry {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Provider(provider) => provider.label,
            Self::Slot(slot) => slot.label,
        }
    }
}

/// 请求作用域必须提供的上下文槽位声明。
#[derive(Clone)]
pub(crate) struct SlotEntry {
    pub(crate) label: &'static str,
}

enum SlotState {
    Empty,
    Resolving,
    Ready(Arc<dyn Any + Send + Sync>),
    Failed(WireError),
}

/// 单个键的缓存槽。
///
/// 同一键的并发解析只运行一次工厂：后来者阻塞在 condvar 上，醒来后拿到同一次结果。
/// 失败同样被缓存，避免一次失败后等待者接连重试工厂。
pub(crate) struct Slot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

impl Slot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(SlotState::Empty), ready: Condvar::new() })
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn get_or_init<F>(&self, key: TypeKey, coordinator: &Coordinator, init: F) -> Result<Arc<dyn Any + Send + Sync>, WireError>
    where
        F: FnOnce() -> Result<Arc<dyn Any + Send + Sync>, WireError>,
    {
        let mut state = self.lock();
        loop {
            enum Step {
                Ready(Arc<dyn Any + Send + Sync>),
                Failed(WireError),
                Wait,
                Init,
            }
            let step = match &*state {
                SlotState::Ready(value) => Step::Ready(Arc::clone(value)),
                SlotState::Failed(error) => Step::Failed(error.clone()),
                SlotState::Resolving => Step::Wait,
                SlotState::Empty => Step::Init,
            };
            match step {
                Step::Ready(value) => return Ok(value),
                Step::Failed(error) => return Err(error),
                Step::Wait => {
                    coordinator.begin_wait(key)?;
                    let guard = WaitGuard { coordinator };
                    state = self.ready.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
                    drop(guard);
                }
                Step::Init => {
                    *state = SlotState::Resolving;
                    coordinator.enter(key);
                    drop(state);
                    let guard = InitGuard { slot: self, key, coordinator, armed: true };
                    let outcome = init();
                    return guard.finish(outcome);
                }
            }
        }
    }
}

struct WaitGuard<'a> {
    coordinator: &'a Coordinator,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.coordinator.end_wait();
    }
}

struct InitGuard<'a> {
    slot: &'a Slot,
    key: TypeKey,
    coordinator: &'a Coordinator,
    armed: bool,
}

impl InitGuard<'_> {
    fn finish(mut self, outcome: Result<Arc<dyn Any + Send + Sync>, WireError>) -> Result<Arc<dyn Any + Send + Sync>, WireError> {
        self.armed = false;
        match &outcome {
            Ok(value) => *self.slot.lock() = SlotState::Ready(Arc::clone(value)),
            Err(error) => *self.slot.lock() = SlotState::Failed(error.clone()),
        }
        self.coordinator.leave(self.key);
        self.slot.ready.notify_all();
        outcome
    }
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // 工厂 panic 时不得把槽位留在 Resolving，否则等待者永久挂起。
        *self.slot.lock() = SlotState::Failed(WireError::new(WireErrorKind::ProviderFailed, "provider 初始化被 panic 中断，所属作用域不重试"));
        self.coordinator.leave(self.key);
        self.slot.ready.notify_all();
    }
}

/// 请求上下文槽位的值：按值复制交付。
pub(crate) trait ContextValue: Send + Sync {
    fn clone_boxed(&self) -> Result<Box<dyn Any + Send + Sync>, WireError>;
}

impl<T: Clone + Send + Sync + 'static> ContextValue for T {
    fn clone_boxed(&self) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        Ok(Box::new(self.clone()))
    }
}
