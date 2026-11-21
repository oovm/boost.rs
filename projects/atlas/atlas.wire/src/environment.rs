//! 环境构建与冻结。

use std::{any::Any, collections::BTreeMap, sync::Arc};

use crate::{
    Component,
    coordinator::Coordinator,
    dependency::Dependency,
    error::{WireError, WireErrorKind},
    graph,
    key::TypeKey,
    provider::{CloneValue, Entry, ProviderEntry, RunFactory, Slot, SlotEntry},
    request::RequestScopeBuilder,
    resolver::{Resolution, Resolver},
    scope::Scope,
};

/// 冻结后的应用环境。
///
/// 冻结后不接受运行时注册、替换或隐式自动发现；测试需要另一套注册时显式建立新环境。
#[derive(Clone)]
pub struct Environment {
    inner: Arc<EnvInner>,
}

impl Environment {
    /// 新建空的构建器。
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::new()
    }

    pub(crate) fn inner(&self) -> &Arc<EnvInner> {
        &self.inner
    }

    /// 以应用级根构造组件：普通输入显式传入，其余字段从应用环境解析。
    ///
    /// 根组件自身的依赖可以在启动时用 [`Environment::validate_root`] 提前校验；未声明为根时，
    /// 首次构造同样受冻结图约束。
    pub fn construct<C: Component>(&self, input: C::Input) -> Result<C, WireError> {
        let resolution = Resolution::new(Arc::clone(&self.inner), None);
        resolution.construct::<C>(input)
    }

    /// 校验某个根组件在当前环境下的依赖是否完整且作用域合法。
    pub fn validate_root<C: Component>(&self) -> Result<(), WireError> {
        graph::validate_root_dependencies(&self.inner.entries, &C::dependencies())
    }

    /// 打开请求作用域构建器；上下文槽位必须显式提供后才能 `seal`。
    pub fn open_request(&self) -> RequestScopeBuilder<'_> {
        RequestScopeBuilder::new(self)
    }
}

/// 冻结图。
pub(crate) struct EnvInner {
    pub(crate) entries: BTreeMap<TypeKey, Entry>,
    pub(crate) slot_keys: Vec<TypeKey>,
    pub(crate) coordinator: Arc<Coordinator>,
}

/// 注册阶段的 provider 定义。
pub struct EnvironmentBuilder {
    entries: BTreeMap<TypeKey, EntryDef>,
}

enum EntryDef {
    Provider(ProviderDef),
    Slot(SlotDef),
}

struct ProviderDef {
    scope: Scope,
    label: &'static str,
    dependencies: Vec<Dependency>,
    run: Box<RunFactory>,
    clone_value: Option<Box<CloneValue>>,
}

struct SlotDef {
    label: &'static str,
}

impl Default for EnvironmentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvironmentBuilder {
    /// 新建空的构建器。
    pub fn new() -> Self {
        Self { entries: BTreeMap::new() }
    }

    /// 注册应用级或请求级 provider。
    ///
    /// `T` 必须是可复制交付的共享输出，标准写法是输出 `Arc<U>`；按值常量注册要求 `Clone`。
    /// transient 必须使用 [`EnvironmentBuilder::register_transient`]。
    pub fn register<T, F>(&mut self, scope: Scope, label: &'static str, dependencies: Vec<Dependency>, factory: F) -> Result<(), WireError>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        if !scope.is_cached() {
            return Err(WireError::new(WireErrorKind::ScopeViolation, "transient provider 必须使用 `register_transient`"));
        }
        self.insert_shared(TypeKey::of::<T>(), scope, label, dependencies, factory)
    }

    /// 注册带角色 marker 的共享 provider；同类型不同角色用不同 `Q` 区分。
    pub fn register_qualified<T, Q, F>(
        &mut self,
        scope: Scope,
        label: &'static str,
        dependencies: Vec<Dependency>,
        factory: F,
    ) -> Result<(), WireError>
    where
        T: Clone + Send + Sync + 'static,
        Q: 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        if !scope.is_cached() {
            return Err(WireError::new(WireErrorKind::ScopeViolation, "transient provider 必须使用 `register_transient_qualified`"));
        }
        self.insert_shared(TypeKey::qualified::<T, Q>(), scope, label, dependencies, factory)
    }

    /// 以组件形式注册共享 provider，输出 `Arc<C>`。
    ///
    /// 依赖直接取 [`Component::dependencies`]，构造交给 [`Component::construct`]，因此组件不必
    /// 再手写一份 provider 闭包。`input` 每次解析都会调用一次来生成普通输入，调用方不需要为
    /// 输入类型实现 `Clone`。
    pub fn register_component<C>(
        &mut self,
        scope: Scope,
        label: &'static str,
        input: impl Fn() -> C::Input + Send + Sync + 'static,
    ) -> Result<(), WireError>
    where
        C: Component,
    {
        self.register::<Arc<C>, _>(scope, label, C::dependencies(), move |resolver| C::construct(input(), resolver).map(Arc::new))
    }

    /// 注册 transient provider：每次解析都运行工厂，不要求 `Clone`。
    pub fn register_transient<T, F>(&mut self, label: &'static str, dependencies: Vec<Dependency>, factory: F) -> Result<(), WireError>
    where
        T: Send + Sync + 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        self.insert_transient(TypeKey::of::<T>(), label, dependencies, factory)
    }

    /// 注册带角色 marker 的 transient provider。
    pub fn register_transient_qualified<T, Q, F>(
        &mut self,
        label: &'static str,
        dependencies: Vec<Dependency>,
        factory: F,
    ) -> Result<(), WireError>
    where
        T: Send + Sync + 'static,
        Q: 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        self.insert_transient(TypeKey::qualified::<T, Q>(), label, dependencies, factory)
    }

    /// 声明请求上下文槽位。
    ///
    /// 槽位进入冻结图，可作为请求级 provider 的依赖；具体值必须在打开请求作用域时显式提供。
    pub fn declare_slot<T: 'static>(&mut self, label: &'static str) -> Result<(), WireError> {
        let key = TypeKey::of::<T>();
        if self.entries.contains_key(&key) {
            return Err(WireError::new(WireErrorKind::DuplicateKey, format!("键已被注册，不能重复声明槽位 `{label}`")));
        }
        self.entries.insert(key, EntryDef::Slot(SlotDef { label }));
        Ok(())
    }

    /// 校验依赖图并冻结环境。
    ///
    /// 校验缺失的非可选依赖、重复键、直接与间接环，以及「应用级不得依赖请求级」的作用域
    /// 约束；任何一项失败都不返回可用环境。
    pub fn freeze(self) -> Result<Environment, WireError> {
        let mut entries = BTreeMap::new();
        let mut slot_keys = Vec::new();
        for (key, definition) in self.entries {
            let entry = match definition {
                EntryDef::Provider(definition) => Entry::Provider(Arc::new(ProviderEntry {
                    scope: definition.scope,
                    label: definition.label,
                    dependencies: definition.dependencies,
                    run: definition.run,
                    clone_value: definition.clone_value,
                    slot: Slot::new(),
                })),
                EntryDef::Slot(definition) => {
                    slot_keys.push(key);
                    Entry::Slot(SlotEntry { label: definition.label })
                }
            };
            entries.insert(key, entry);
        }
        graph::validate_graph(&entries)?;
        Ok(Environment { inner: Arc::new(EnvInner { entries, slot_keys, coordinator: Arc::new(Coordinator::new()) }) })
    }

    fn insert_shared<T, F>(
        &mut self,
        key: TypeKey,
        scope: Scope,
        label: &'static str,
        dependencies: Vec<Dependency>,
        factory: F,
    ) -> Result<(), WireError>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        let run: Box<RunFactory> = Box::new(move |resolver| factory(resolver).map(|value| Box::new(value) as Box<dyn Any + Send + Sync>));
        let clone_value: Box<CloneValue> = Box::new(|value: &(dyn Any + Send + Sync)| {
            value
                .downcast_ref::<T>()
                .map(|value| Box::new(value.clone()) as Box<dyn Any + Send + Sync>)
                .ok_or_else(|| WireError::new(WireErrorKind::TypeContract, format!("缓存实例类型与键不一致：`{}`", std::any::type_name::<T>())))
        });
        self.insert_provider(key, ProviderDef { scope, label, dependencies, run, clone_value: Some(clone_value) })
    }

    fn insert_transient<T, F>(&mut self, key: TypeKey, label: &'static str, dependencies: Vec<Dependency>, factory: F) -> Result<(), WireError>
    where
        T: Send + Sync + 'static,
        F: Fn(&mut Resolver<'_>) -> Result<T, WireError> + Send + Sync + 'static,
    {
        let run: Box<RunFactory> = Box::new(move |resolver| factory(resolver).map(|value| Box::new(value) as Box<dyn Any + Send + Sync>));
        self.insert_provider(key, ProviderDef { scope: Scope::Transient, label, dependencies, run, clone_value: None })
    }

    fn insert_provider(&mut self, key: TypeKey, definition: ProviderDef) -> Result<(), WireError> {
        if self.entries.contains_key(&key) {
            return Err(WireError::new(WireErrorKind::DuplicateKey, format!("`{}` 的键已被注册", definition.label)));
        }
        self.entries.insert(key, EntryDef::Provider(definition));
        Ok(())
    }
}
