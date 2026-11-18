//! 受限解析器与解析上下文。

use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use crate::{
    Component,
    dependency::Dependency,
    environment::EnvInner,
    error::{WireError, WireErrorKind},
    key::TypeKey,
    provider::{Entry, ProviderEntry, Slot},
    request::RequestInner,
    scope::Scope,
};

/// 传给组件构造与 provider 工厂的受限解析器。
///
/// 在 provider 工厂与组件构造内部，它只允许读取该 provider 或组件声明过的键；读取未声明的
/// 键立即返回 [`WireErrorKind::UndeclaredDependency`]，缺失键的错误会带上字段标签。
pub struct Resolver<'a> {
    resolution: &'a mut Resolution,
    declared: Option<&'a [Dependency]>,
}

impl<'a> Resolver<'a> {
    /// 解析精确类型 `T`，可带角色限定符。
    ///
    /// 键是 `T` 本身：`resolve::<Arc<Model>>` 不会降级查找 `Model`。
    pub fn resolve<T: Send + Sync + 'static>(&mut self, qualifier: Option<TypeId>) -> Result<T, WireError> {
        let key = TypeKey::from_parts(TypeId::of::<T>(), qualifier);
        let boxed = self.resolve_boxed(key, false)?.ok_or_else(|| WireError::new(WireErrorKind::TypeContract, "必需依赖返回了空值"))?;
        boxed
            .downcast::<T>()
            .map(|value| *value)
            .map_err(|_| WireError::new(WireErrorKind::TypeContract, format!("缓存实例类型与请求类型 `{}` 不一致", std::any::type_name::<T>())))
    }

    /// 解析可选依赖：只有「未注册」得到 `None`。
    ///
    /// provider 已注册但构造失败、作用域非法或发生循环仍然返回错误。
    pub fn resolve_optional<T: Send + Sync + 'static>(&mut self, qualifier: Option<TypeId>) -> Result<Option<T>, WireError> {
        let key = TypeKey::from_parts(TypeId::of::<T>(), qualifier);
        match self.resolve_boxed(key, true)? {
            Some(boxed) => boxed.downcast::<T>().map(|value| Some(*value)).map_err(|_| {
                WireError::new(WireErrorKind::TypeContract, format!("缓存实例类型与请求类型 `{}` 不一致", std::any::type_name::<T>()))
            }),
            None => Ok(None),
        }
    }

    fn resolve_boxed(&mut self, key: TypeKey, optional: bool) -> Result<Option<Box<dyn Any + Send + Sync>>, WireError> {
        let mut label = None;
        if let Some(declared) = self.declared {
            let Some(dependency) = declared.iter().find(|dependency| dependency.key() == key)
            else {
                let provider = self.resolution.entry_label(key);
                return Err(self.resolution.error(WireErrorKind::UndeclaredDependency, format!("provider 读取了未声明的依赖 `{provider}`")));
            };
            label = Some(dependency.label());
        }
        self.resolution.resolve_key(key, optional, label)
    }
}

/// 一次根构造的解析上下文。
pub(crate) struct Resolution {
    env: Arc<EnvInner>,
    request: Option<Arc<RequestInner>>,
    stack: Vec<TypeKey>,
    path: Vec<&'static str>,
}

impl Resolution {
    pub(crate) fn new(env: Arc<EnvInner>, request: Option<Arc<RequestInner>>) -> Self {
        Self { env, request, stack: Vec::new(), path: Vec::new() }
    }

    pub(crate) fn construct<C: Component>(mut self, input: C::Input) -> Result<C, WireError> {
        let declared = C::dependencies();
        let mut resolver = Resolver { resolution: &mut self, declared: Some(declared.as_slice()) };
        C::construct(input, &mut resolver)
    }

    fn entry_label(&self, key: TypeKey) -> &'static str {
        self.env.entries.get(&key).map(Entry::label).unwrap_or("<未注册>")
    }

    pub(crate) fn error(&self, kind: WireErrorKind, message: impl Into<String>) -> WireError {
        WireError::new(kind, message).with_path(&self.path)
    }

    fn resolve_key(
        &mut self,
        key: TypeKey,
        optional: bool,
        label: Option<&'static str>,
    ) -> Result<Option<Box<dyn Any + Send + Sync>>, WireError> {
        let Some(entry) = self.env.entries.get(&key).cloned()
        else {
            if optional {
                return Ok(None);
            }
            let mut error =
                self.error(WireErrorKind::NotRegistered, format!("依赖 `{}` 没有对应 provider 或上下文槽位", label.unwrap_or("<未声明>")));
            if let Some(label) = label {
                error = error.with_step(label);
            }
            return Err(error);
        };
        match entry {
            Entry::Slot(_) => self.resolve_context(key).map(Some),
            Entry::Provider(provider) => self.resolve_provider(key, &provider).map(Some),
        }
    }

    fn resolve_context(&mut self, key: TypeKey) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        let Some(request) = self.request.clone()
        else {
            return Err(self.error(WireErrorKind::ScopeViolation, format!("上下文槽位 `{}` 只能在请求作用域内解析", self.entry_label(key))));
        };
        if request.closed() {
            return Err(self.error(WireErrorKind::ScopeClosed, "请求作用域已关闭，拒绝新解析"));
        }
        match request.context(key) {
            Some(value) => value,
            None => Err(self.error(WireErrorKind::NotRegistered, format!("上下文槽位 `{}` 未提供值", self.entry_label(key)))),
        }
    }

    fn resolve_provider(&mut self, key: TypeKey, provider: &Arc<ProviderEntry>) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        match provider.scope {
            Scope::Transient => self.run_factory(key, provider, None),
            Scope::Application => {
                let slot = Arc::clone(&provider.slot);
                self.run_factory(key, provider, Some(slot))
            }
            Scope::Request => {
                let Some(request) = self.request.clone()
                else {
                    return Err(
                        self.error(WireErrorKind::ScopeViolation, format!("`{}` 是请求级 provider，当前解析不在请求作用域内", provider.label))
                    );
                };
                if request.closed() {
                    return Err(self.error(WireErrorKind::ScopeClosed, format!("请求作用域已关闭，拒绝解析 `{}`", provider.label)));
                }
                let slot = request.slot(key);
                self.run_factory(key, provider, Some(slot))
            }
        }
    }

    fn run_factory(
        &mut self,
        key: TypeKey,
        provider: &ProviderEntry,
        slot: Option<Arc<Slot>>,
    ) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        if self.stack.contains(&key) {
            return Err(self.error(WireErrorKind::Cycle, format!("依赖 `{}` 在当前解析链上重复出现", provider.label)));
        }
        self.stack.push(key);
        self.path.push(provider.label);
        let outcome = self.run_provider_body(key, provider, slot).map_err(|error| error.with_path(&self.path));
        self.path.pop();
        self.stack.pop();
        outcome
    }

    fn run_provider_body(
        &mut self,
        key: TypeKey,
        provider: &ProviderEntry,
        slot: Option<Arc<Slot>>,
    ) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        let Some(slot) = slot
        else {
            return self.invoke(provider);
        };
        let coordinator = Arc::clone(&self.env.coordinator);
        let cached = slot.get_or_init(key, &coordinator, || self.invoke(provider).map(Arc::from))?;
        match &provider.clone_value {
            Some(clone_value) => clone_value(cached.as_ref()),
            None => Err(self.error(WireErrorKind::TypeContract, format!("共享 provider `{}` 缺少复制策略", provider.label))),
        }
    }

    fn invoke(&mut self, provider: &ProviderEntry) -> Result<Box<dyn Any + Send + Sync>, WireError> {
        let declared = provider.dependencies.clone();
        let mut resolver = Resolver { resolution: self, declared: Some(declared.as_slice()) };
        (provider.run)(&mut resolver)
    }
}
