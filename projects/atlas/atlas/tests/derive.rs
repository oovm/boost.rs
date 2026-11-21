//! `Component` derive 与运行时合同的联合验收。

use std::sync::Arc;

use atlas::wire::{Component as _, Environment, Scope, WireError, WireErrorKind};

trait OrderRepository: Send + Sync {
    fn region(&self) -> &'static str;
}

struct Orders;

impl OrderRepository for Orders {
    fn region(&self) -> &'static str {
        "orders"
    }
}

struct ModelClient {
    role: &'static str,
}

struct Metrics;

/// 角色 marker：同类型多实例用类型区分，不按字段名猜测。
struct Primary;

#[derive(atlas::Component)]
#[atlas(input = OrderServiceInput, runtime = atlas::wire)]
struct OrderService {
    region: String,
    max_items: usize,

    #[wire]
    repository: Arc<dyn OrderRepository + Send + Sync>,

    #[wire(key = Primary)]
    model: Arc<ModelClient>,

    #[wire(optional)]
    metrics: Option<Arc<Metrics>>,
}

/// 直用包路径：`runtime = atlas_wire` 与门面等价。
#[derive(atlas::Component)]
#[atlas(input = SnapshotInput, runtime = atlas_wire)]
struct Snapshot {
    label: String,
    #[wire(key = Primary)]
    model: Arc<ModelClient>,
}

#[test]
fn derive_generates_input_without_wire_fields() {
    let dependencies = OrderService::dependencies();
    assert_eq!(dependencies.len(), 3);
    assert_eq!(dependencies[0].label(), "repository");
    assert!(!dependencies[0].optional());
    assert_eq!(dependencies[1].label(), "model");
    assert!(dependencies[1].key().is_qualified(), "`key = Primary` 必须生成限定键");
    assert_eq!(dependencies[2].label(), "metrics");
    assert!(dependencies[2].optional());
}

#[test]
fn constructs_through_facade_runtime_path() {
    let environment = environment();
    let service =
        environment.construct::<OrderService>(OrderServiceInput { region: "ap-guangzhou".into(), max_items: 100 }).expect("构造订单服务");
    assert_eq!(service.region, "ap-guangzhou");
    assert_eq!(service.max_items, 100);
    assert_eq!(service.repository.region(), "orders");
    assert_eq!(service.model.role, "primary");
    assert!(service.metrics.is_none());
}

#[test]
fn constructs_through_direct_runtime_path() {
    let environment = environment();
    let snapshot = environment.construct::<Snapshot>(SnapshotInput { label: "daily".into() }).expect("构造快照");
    assert_eq!(snapshot.label, "daily");
    assert_eq!(snapshot.model.role, "primary");
}

#[test]
fn role_is_matched_by_marker_not_field_name() {
    // 只注册无角色键时，带 `key = Primary` 的字段必须失败，而不是复用同类型实例。
    let mut builder = Environment::builder();
    builder
        .register::<Arc<ModelClient>, _>(Scope::Application, "model", Vec::new(), |_| Ok(Arc::new(ModelClient { role: "plain" })))
        .expect("注册无角色模型");
    builder
        .register::<Arc<dyn OrderRepository + Send + Sync>, _>(Scope::Application, "repository", Vec::new(), |_| {
            Ok(Arc::new(Orders) as Arc<dyn OrderRepository + Send + Sync>)
        })
        .expect("注册仓库");
    let environment = builder.freeze().expect("冻结环境");
    let error = environment
        .construct::<OrderService>(OrderServiceInput { region: "ap-guangzhou".into(), max_items: 100 })
        .err()
        .expect("缺少角色实例必须失败");
    assert_eq!(error.kind(), WireErrorKind::NotRegistered);
}

#[test]
fn optional_provider_failure_is_not_swallowed() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn OrderRepository + Send + Sync>, _>(Scope::Application, "repository", Vec::new(), |_| {
            Ok(Arc::new(Orders) as Arc<dyn OrderRepository + Send + Sync>)
        })
        .expect("注册仓库");
    builder
        .register_qualified::<Arc<ModelClient>, Primary, _>(Scope::Application, "model", Vec::new(), |_| {
            Ok(Arc::new(ModelClient { role: "primary" }))
        })
        .expect("注册角色模型");
    builder
        .register::<Arc<Metrics>, _>(Scope::Application, "metrics", Vec::new(), |_| {
            Err(WireError::new(WireErrorKind::ProviderFailed, "指标后端不可用"))
        })
        .expect("注册指标后端");
    let environment = builder.freeze().expect("可选依赖允许注册失败型 provider");
    let error = environment
        .construct::<OrderService>(OrderServiceInput { region: "ap-guangzhou".into(), max_items: 100 })
        .err()
        .expect("可选 provider 已注册但构造失败必须报错");
    assert_eq!(error.kind(), WireErrorKind::ProviderFailed);
}

/// 直接以组件身份注册的服务集合：依赖与构造都来自 derive。
#[derive(atlas::Component)]
#[atlas(input = ServiceRegistryInput, runtime = atlas::wire)]
struct ServiceRegistry {
    #[wire(key = Primary)]
    model: Arc<ModelClient>,

    #[wire]
    orders: Arc<OrderService>,
}

#[test]
fn derived_component_registers_as_provider() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn OrderRepository + Send + Sync>, _>(Scope::Application, "repository", Vec::new(), |_| {
            Ok(Arc::new(Orders) as Arc<dyn OrderRepository + Send + Sync>)
        })
        .expect("注册仓库");
    builder
        .register_qualified::<Arc<ModelClient>, Primary, _>(Scope::Application, "model", Vec::new(), |_| {
            Ok(Arc::new(ModelClient { role: "primary" }))
        })
        .expect("注册角色模型");
    builder
        .register_component::<OrderService>(Scope::Application, "orders", || OrderServiceInput { region: "ap-guangzhou".into(), max_items: 10 })
        .expect("注册组件");
    let environment = builder.freeze().expect("冻结环境");

    let registry = environment.construct::<ServiceRegistry>(ServiceRegistryInput).expect("构造注册表");
    assert_eq!(registry.orders.region, "ap-guangzhou");
    assert_eq!(registry.orders.max_items, 10);
    assert_eq!(registry.orders.repository.region(), "orders");
    assert_eq!(registry.model.role, "primary");
    assert!(registry.orders.metrics.is_none());
}

#[test]
fn derived_component_rejects_duplicate_registration() {
    let mut builder = Environment::builder();
    builder
        .register_component::<OrderService>(Scope::Application, "orders", || OrderServiceInput { region: "ap-guangzhou".into(), max_items: 10 })
        .expect("首次注册");
    let error = builder
        .register_component::<OrderService>(Scope::Application, "orders", || OrderServiceInput { region: "ap-shanghai".into(), max_items: 20 })
        .expect_err("重复注册必须失败");
    assert_eq!(error.kind(), WireErrorKind::DuplicateKey);
}

fn environment() -> Environment {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<dyn OrderRepository + Send + Sync>, _>(Scope::Application, "repository", Vec::new(), |_| {
            Ok(Arc::new(Orders) as Arc<dyn OrderRepository + Send + Sync>)
        })
        .expect("注册仓库");
    builder
        .register_qualified::<Arc<ModelClient>, Primary, _>(Scope::Application, "model", Vec::new(), |_| {
            Ok(Arc::new(ModelClient { role: "primary" }))
        })
        .expect("注册角色模型");
    builder.freeze().expect("冻结环境")
}
