//! 请求作用域。

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    Component,
    environment::Environment,
    error::{WireError, WireErrorKind},
    key::TypeKey,
    provider::{ContextValue, Entry, Slot},
    resolver::Resolution,
};

/// 请求作用域构建器。
///
/// 打开作用域时按声明逐项提供上下文槽位；缺失、重复或未声明的槽位都会拒绝。
pub struct RequestScopeBuilder<'a> {
    environment: &'a Environment,
    provided: BTreeMap<TypeKey, Box<dyn ContextValue>>,
}

impl<'a> RequestScopeBuilder<'a> {
    pub(crate) fn new(environment: &'a Environment) -> Self {
        Self { environment, provided: BTreeMap::new() }
    }

    /// 为已声明的上下文槽位提供值。
    pub fn provide<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Result<&mut Self, WireError> {
        let key = TypeKey::of::<T>();
        match self.environment.inner().entries.get(&key) {
            Some(Entry::Slot(slot)) => {
                if self.provided.insert(key, Box::new(value)).is_some() {
                    return Err(WireError::new(WireErrorKind::DuplicateKey, format!("上下文槽位 `{}` 被重复提供", slot.label)));
                }
                Ok(self)
            }
            Some(Entry::Provider(provider)) => {
                Err(WireError::new(WireErrorKind::ScopeViolation, format!("`{}` 是 provider，不是可提供的上下文槽位", provider.label)))
            }
            None => Err(WireError::new(WireErrorKind::NotRegistered, format!("上下文槽位 `{}` 未声明", std::any::type_name::<T>()))),
        }
    }

    /// 校验并冻结请求作用域。
    pub fn seal(self) -> Result<RequestScope<'a>, WireError> {
        for key in &self.environment.inner().slot_keys {
            if !self.provided.contains_key(key) {
                let label = self.environment.inner().entries.get(key).map(Entry::label).unwrap_or("<未注册>");
                return Err(WireError::new(WireErrorKind::NotRegistered, format!("请求上下文槽位 `{label}` 没有提供值")));
            }
        }
        Ok(RequestScope {
            environment: self.environment,
            inner: Arc::new(RequestInner { slots: self.provided, cache: Mutex::new(BTreeMap::new()), closed: AtomicBool::new(false) }),
        })
    }
}

/// 已冻结的请求作用域。
pub struct RequestScope<'a> {
    environment: &'a Environment,
    inner: Arc<RequestInner>,
}

impl RequestScope<'_> {
    /// 在请求作用域内构造组件：应用级、请求级与 transient 依赖都可用。
    pub fn construct<C: Component>(&self, input: C::Input) -> Result<C, WireError> {
        if self.inner.closed() {
            return Err(WireError::new(WireErrorKind::ScopeClosed, "请求作用域已关闭，拒绝新解析"));
        }
        let resolution = Resolution::new(Arc::clone(self.environment.inner()), Some(Arc::clone(&self.inner)));
        resolution.construct::<C>(input)
    }

    /// 关闭作用域并释放请求级缓存。关闭后的解析返回 [`WireErrorKind::ScopeClosed`]。
    ///
    /// 作用域代表缓存与解析范围，不能保证被业务带出的 `Arc` 同时销毁；事务、任务与连接的
    /// 显式完成仍由资源 owner 负责。
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// 作用域是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.inner.closed()
    }
}

/// 请求级缓存与上下文槽位值。
pub(crate) struct RequestInner {
    slots: BTreeMap<TypeKey, Box<dyn ContextValue>>,
    cache: Mutex<BTreeMap<TypeKey, Arc<Slot>>>,
    closed: AtomicBool,
}

impl RequestInner {
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn slot(&self, key: TypeKey) -> Arc<Slot> {
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(cache.entry(key).or_insert_with(Slot::new))
    }

    pub(crate) fn context(&self, key: TypeKey) -> Option<Result<Box<dyn Any + Send + Sync>, WireError>> {
        self.slots.get(&key).map(|value| value.as_ref().clone_boxed())
    }
}
