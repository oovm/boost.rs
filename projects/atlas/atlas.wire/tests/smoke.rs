//! 运行时的基本冒烟测试；详细符合性用例在 `atlas-harness`。

use std::sync::Arc;

use atlas_wire::{Component, Dependency, Environment, Resolver, Scope, WireError, WireErrorKind};

trait Repository: Send + Sync {
    fn name(&self) -> &'static str;
}

struct Orders;

impl Repository for Orders {
    fn name(&self) -> &'static str {
        "orders"
    }
}

struct Service {
    region: String,
    repository: Arc<dyn Repository>,
    cache: Option<Arc<Cache>>,
}

impl Component for Service {
    type Input = String;

    fn dependencies() -> Vec<Dependency> {
        vec![Dependency::of::<Arc<dyn Repository>>("repository", false), Dependency::of::<Arc<Cache>>("cache", true)]
    }

    fn construct(region: Self::Input, resolver: &mut Resolver<'_>) -> Result<Self, WireError> {
        Ok(Self { region, repository: resolver.resolve::<Arc<dyn Repository>>(None)?, cache: resolver.resolve_optional::<Arc<Cache>>(None)? })
    }
}

struct Cache;

fn environment() -> Environment {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn Repository>, _>(Scope::Application, "repository", Vec::new(), |_| Ok(Arc::new(Orders) as Arc<dyn Repository>))
        .expect("注册仓库");
    builder.freeze().expect("冻结环境")
}

#[test]
fn constructs_with_injected_fields() {
    let service = environment().construct::<Service>("ap-guangzhou".into()).expect("构造组件");
    assert_eq!(service.region, "ap-guangzhou");
    assert_eq!(service.repository.name(), "orders");
    assert!(service.cache.is_none(), "未注册的可选依赖必须是 None");
}

#[test]
fn missing_required_dependency_is_reported() {
    let environment = Environment::builder().freeze().expect("空环境可以冻结");
    let error = environment.construct::<Service>("ap-guangzhou".into()).err().expect("缺少仓库必须失败");
    assert_eq!(error.kind(), WireErrorKind::NotRegistered);
}

#[test]
fn duplicate_registration_is_rejected() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn Repository>, _>(Scope::Application, "repository", Vec::new(), |_| Ok(Arc::new(Orders) as Arc<dyn Repository>))
        .expect("首次注册");
    let error = builder
        .register::<Arc<dyn Repository>, _>(Scope::Application, "repository", Vec::new(), |_| Ok(Arc::new(Orders) as Arc<dyn Repository>))
        .expect_err("重复注册必须失败");
    assert_eq!(error.kind(), WireErrorKind::DuplicateKey);
}
