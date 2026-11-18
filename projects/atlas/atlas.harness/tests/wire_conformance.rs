//! `atlas.wire` 详细符合性：角色、作用域、失败传播、并发与关闭。
//!
//! 上游 `atlas-wire` 只保留基本冒烟测试；这里覆盖反例与并发时序。

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use atlas::wire::{Component, Dependency, Environment, Resolver, Scope, WireError, WireErrorKind};

trait Store: Send + Sync {
    fn id(&self) -> u32;
}

struct MemoryStore(u32);

impl Store for MemoryStore {
    fn id(&self) -> u32 {
        self.0
    }
}

struct Primary;

struct Replica;

struct Slow;

struct Broken;

struct Widget;

struct Bridge;

struct Session {
    identity: RequestIdentity,
}

#[derive(Clone)]
struct RequestIdentity(u64);

struct NeedsSlow {
    slow: Arc<Slow>,
}

impl Component for NeedsSlow {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<Slow>>("slow", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { slow: resolver.resolve::<Arc<Slow>>(None)? })
    }
}

struct NeedsBroken;

impl Component for NeedsBroken {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<Broken>>("broken", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        resolver.resolve::<Arc<Broken>>(None)?;
        Err(WireError::new(WireErrorKind::TypeContract, "不应到达"))
    }
}

struct RequestRoot {
    session: Arc<Session>,
}

impl Component for RequestRoot {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<Session>>("session", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { session: resolver.resolve::<Arc<Session>>(None)? })
    }
}

fn store_environment() -> Environment {
    let mut builder = Environment::builder();
    builder
        .register_qualified::<Arc<dyn Store>, Primary, _>(Scope::Application, "primary", Vec::new(), |_| {
            Ok(Arc::new(MemoryStore(1)) as Arc<dyn Store>)
        })
        .expect("注册主库");
    builder
        .register_qualified::<Arc<dyn Store>, Replica, _>(Scope::Application, "replica", Vec::new(), |_| {
            Ok(Arc::new(MemoryStore(2)) as Arc<dyn Store>)
        })
        .expect("注册备库");
    builder.freeze().expect("冻结环境")
}

#[test]
fn same_type_resolves_per_role() {
    let environment = store_environment();
    assert_eq!(environment.construct::<PrimaryRoot>(()).expect("构造主库根").store.id(), 1);
    assert_eq!(environment.construct::<ReplicaRoot>(()).expect("构造备库根").store.id(), 2);
}

/// 用两个不同组件固定各自的角色键。
struct PrimaryRoot {
    store: Arc<dyn Store>,
}

impl Component for PrimaryRoot {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of_qualified::<Arc<dyn Store>, Primary>("store", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { store: resolver.resolve::<Arc<dyn Store>>(Some(std::any::TypeId::of::<Primary>()))? })
    }
}

struct ReplicaRoot {
    store: Arc<dyn Store>,
}

impl Component for ReplicaRoot {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of_qualified::<Arc<dyn Store>, Replica>("store", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { store: resolver.resolve::<Arc<dyn Store>>(Some(std::any::TypeId::of::<Replica>()))? })
    }
}

#[test]
fn same_type_keyed_roles_stay_separate() {
    let environment = store_environment();
    assert_eq!(environment.construct::<PrimaryRoot>(()).expect("主库").store.id(), 1);
    assert_eq!(environment.construct::<ReplicaRoot>(()).expect("备库").store.id(), 2);
}

#[test]
fn application_scope_cannot_reach_request_provider() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Widget>, _>(Scope::Application, "widget", vec![Dependency::of::<Arc<Session>>("session", false)], |_| {
            Ok(Arc::new(Widget))
        })
        .expect("注册应用级组件");
    builder
        .register::<Arc<Session>, _>(Scope::Request, "session", Vec::new(), |_| {
            Err(WireError::new(WireErrorKind::ProviderFailed, "需要请求上下文"))
        })
        .expect("注册请求级会话");
    let error = builder.freeze().err().expect("应用级依赖请求级必须冻结失败");
    assert_eq!(error.kind(), WireErrorKind::ScopeViolation);
}

#[test]
fn application_scope_cannot_reach_request_through_transient() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Widget>, _>(Scope::Application, "widget", vec![Dependency::of::<Arc<Bridge>>("bridge", false)], |_| {
            Ok(Arc::new(Widget))
        })
        .expect("注册应用级组件");
    builder
        .register_transient::<Arc<Bridge>, _>("bridge", vec![Dependency::of::<Arc<Session>>("session", false)], |_| Ok(Arc::new(Bridge)))
        .expect("注册 transient 桥");
    builder
        .register::<Arc<Session>, _>(Scope::Request, "session", Vec::new(), |_| {
            Err(WireError::new(WireErrorKind::ProviderFailed, "需要请求上下文"))
        })
        .expect("注册请求级会话");
    let error = builder.freeze().err().expect("经 transient 间接依赖请求级必须冻结失败");
    assert_eq!(error.kind(), WireErrorKind::ScopeViolation);
}

#[test]
fn request_provider_requires_request_scope() {
    let mut builder = Environment::builder();
    builder.declare_slot::<RequestIdentity>("identity").expect("声明身份槽位");
    builder
        .register::<Arc<Session>, _>(Scope::Request, "session", vec![Dependency::of::<RequestIdentity>("identity", false)], |resolver| {
            Ok(Arc::new(Session { identity: resolver.resolve::<RequestIdentity>(None)? }))
        })
        .expect("注册会话");
    let environment = builder.freeze().expect("冻结环境");
    let error = environment.construct::<RequestRoot>(()).err().expect("应用根不能解析请求级依赖");
    assert_eq!(error.kind(), WireErrorKind::ScopeViolation);
}

#[test]
fn request_scope_requires_declared_slots() {
    let mut builder = Environment::builder();
    builder.declare_slot::<RequestIdentity>("identity").expect("声明身份槽位");
    builder
        .register::<Arc<Session>, _>(Scope::Request, "session", vec![Dependency::of::<RequestIdentity>("identity", false)], |resolver| {
            Ok(Arc::new(Session { identity: resolver.resolve::<RequestIdentity>(None)? }))
        })
        .expect("注册会话");
    let environment = builder.freeze().expect("冻结环境");
    let error = environment.open_request().seal().err().expect("缺少槽位必须拒绝");
    assert_eq!(error.kind(), WireErrorKind::NotRegistered);
}

#[test]
fn request_scope_provides_context_and_serves_request_providers() {
    let mut builder = Environment::builder();
    builder.declare_slot::<RequestIdentity>("identity").expect("声明身份槽位");
    builder
        .register::<Arc<Session>, _>(Scope::Request, "session", vec![Dependency::of::<RequestIdentity>("identity", false)], |resolver| {
            Ok(Arc::new(Session { identity: resolver.resolve::<RequestIdentity>(None)? }))
        })
        .expect("注册会话");
    let environment = builder.freeze().expect("冻结环境");

    let mut scope = environment.open_request();
    scope.provide(RequestIdentity(7)).expect("提供身份");
    let scope = scope.seal().expect("冻结请求作用域");
    let root = scope.construct::<RequestRoot>(()).expect("请求内构造成功");
    assert_eq!(root.session.identity.0, 7);

    scope.close();
    assert!(scope.is_closed());
    let error = scope.construct::<RequestRoot>(()).err().expect("关闭后拒绝解析");
    assert_eq!(error.kind(), WireErrorKind::ScopeClosed);
}

#[test]
fn undeclared_dependency_is_rejected() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn Store>, _>(Scope::Application, "store", Vec::new(), |_| Ok(Arc::new(MemoryStore(1)) as Arc<dyn Store>))
        .expect("注册仓库");
    builder
        .register_transient::<Arc<Widget>, _>("widget", Vec::new(), |resolver| {
            // 声明里没有 `Arc<dyn Store>`，读取必须失败。
            resolver.resolve::<Arc<dyn Store>>(None)?;
            Ok(Arc::new(Widget))
        })
        .expect("注册 transient");
    let environment = builder.freeze().expect("冻结环境");
    let error = environment.construct::<WidgetRoot>(()).err().expect("访问未声明依赖必须失败");
    assert_eq!(error.kind(), WireErrorKind::UndeclaredDependency);
}

struct WidgetRoot;

impl Component for WidgetRoot {
    type Input = ();

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<Widget>>("widget", false)]
    }

    fn construct(_: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        resolver.resolve::<Arc<Widget>>(None)?;
        Ok(Self)
    }
}

#[test]
fn provider_failure_is_cached_per_scope() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Broken>, _>(Scope::Application, "broken", Vec::new(), |_| {
            RUNS.fetch_add(1, Ordering::SeqCst);
            Err(WireError::new(WireErrorKind::ProviderFailed, "后端不可用"))
        })
        .expect("注册失败型 provider");
    let environment = builder.freeze().expect("冻结环境");

    for _ in 0..2 {
        let error = environment.construct::<NeedsBroken>(()).err().expect("首次与后续都必须失败");
        assert_eq!(error.kind(), WireErrorKind::ProviderFailed);
    }
    assert_eq!(RUNS.load(Ordering::SeqCst), 1, "失败结果必须被作用域缓存，等待者不重复执行工厂");
}

#[test]
fn shared_provider_initializes_once_under_concurrency() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Slow>, _>(Scope::Application, "slow", Vec::new(), |_| {
            RUNS.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(30));
            Ok(Arc::new(Slow))
        })
        .expect("注册慢 provider");
    let environment = Arc::new(builder.freeze().expect("冻结环境"));

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let environment = Arc::clone(&environment);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            environment.construct::<NeedsSlow>(()).map(|root| Arc::strong_count(&root.slow))
        }));
    }
    for handle in handles {
        let count = handle.join().expect("线程未 panic").expect("并发构造成功");
        assert!(count >= 1);
    }
    assert_eq!(RUNS.load(Ordering::SeqCst), 1, "同一键并发解析只运行一次工厂");
}

#[test]
fn indirect_cycle_fails_freeze() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Widget>, _>(Scope::Application, "widget", vec![Dependency::of::<Arc<Bridge>>("bridge", false)], |_| {
            Ok(Arc::new(Widget))
        })
        .expect("注册组件");
    builder
        .register::<Arc<Bridge>, _>(Scope::Application, "bridge", vec![Dependency::of::<Arc<Widget>>("widget", false)], |_| {
            Ok(Arc::new(Bridge))
        })
        .expect("注册桥");
    let error = builder.freeze().err().expect("间接环必须冻结失败");
    assert_eq!(error.kind(), WireErrorKind::Cycle);
}

#[test]
fn missing_dependency_is_reported_at_freeze_with_path() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Widget>, _>(Scope::Application, "widget", vec![Dependency::of::<Arc<Bridge>>("bridge", false)], |_| {
            Ok(Arc::new(Widget))
        })
        .expect("注册组件");
    let error = builder.freeze().err().expect("缺失依赖必须冻结失败");
    assert_eq!(error.kind(), WireErrorKind::NotRegistered);
    assert!(error.path().contains(&"widget"), "诊断必须带 provider 路径：{}", error.path().join(" -> "));
}

#[test]
fn failing_dependency_returns_error_without_partial_object() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<Broken>, _>(Scope::Application, "broken", Vec::new(), |_| {
            Err(WireError::new(WireErrorKind::ProviderFailed, "后端不可用"))
        })
        .expect("注册失败型 provider");
    let environment = builder.freeze().expect("冻结环境");
    let error = environment.construct::<NeedsBroken>(()).err().expect("构造不得返回半成品");
    assert_eq!(error.kind(), WireErrorKind::ProviderFailed);
    assert!(error.path().contains(&"broken"), "错误必须定位到失败字段：{}", error.path().join(" -> "));
}
