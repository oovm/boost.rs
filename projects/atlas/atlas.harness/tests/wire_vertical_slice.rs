//! W4 真实纵切：云客户端与数据库配置由显式环境提供，普通输入只由作者传入，装配不触发网络。
//!
//! 本用例同时验证两件互相约束的事实：`atlas_cloud` 的短信客户端仍可独立构造调用，而注入
//! 只负责把它交给组件；装配阶段只建立本地句柄，不发送任何付费请求。数据库连接是每线程
//! 状态机且不是 `Send`，因此环境只交付可跨线程的配置描述符，连接由宿主自己打开。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use atlas_cloud::tencent::{Credentials, Region, sms::Client};
use atlas_runtime::wire::{Component as _, Environment, Scope};
use boost_net::{
    NetError,
    http::{Request, Response, Transport},
};

/// 角色 marker：审计后端与其它依赖同类型时靠它区分。
struct Primary;

trait Audit: Send + Sync {
    fn record(&self, event: &str);
}

struct RecordingAudit {
    records: AtomicUsize,
}

impl Audit for RecordingAudit {
    fn record(&self, _: &str) {
        self.records.fetch_add(1, Ordering::SeqCst);
    }
}

/// 装配阶段一旦被调用即失败：证明构造没有发起网络请求。
struct PanickingTransport;

impl Transport for PanickingTransport {
    fn send(&self, _: &Request) -> Result<Response, NetError> {
        panic!("装配阶段不得发起网络请求");
    }
}

#[derive(atlas_runtime::Component)]
#[atlas(input = NotifierInput, runtime = atlas_runtime::wire)]
struct Notifier {
    sign_name: String,
    template_id: String,

    #[wire]
    client: Arc<Client>,

    #[wire(key = Primary)]
    audit: Arc<dyn Audit>,
}

/// 数据库资源：环境交付可跨线程的配置描述符，连接由持有该线程的宿主自己打开。
///
/// `sqlite::Connection` 是每连接状态机且不是 `Send`，运行时只托管 `Send + Sync + 'static`
/// 的值，因此连接本身不得进入注入键；把描述符注入、把连接留在所有者线程才是正确分层。
#[derive(atlas_runtime::Component)]
#[atlas(input = ReportStoreInput, runtime = atlas_runtime::wire)]
struct ReportStore {
    label: String,

    #[wire]
    settings: Arc<SqliteSettings>,
}

struct SqliteSettings {
    memory: bool,
}

fn sms_client() -> Client {
    Client::new(Credentials::new("AKID-test", "secret-test"), "1400000000", Region::ap_guangzhou()).with_transport(Box::new(PanickingTransport))
}

#[test]
fn cloud_client_can_still_be_constructed_without_the_container() {
    let client = sms_client();
    let authorization = client.authorization(r#"{"PhoneNumberSet":[]}"#, 1_700_000_000);
    assert!(authorization.starts_with("TC3-HMAC-SHA256"), "客户端必须可独立签名：{authorization}");
}

#[test]
fn assembly_injects_cloud_client_without_network_and_honours_roles() {
    let audit = Arc::new(RecordingAudit { records: AtomicUsize::new(0) });
    let mut builder = Environment::builder();
    builder.register::<Arc<Client>, _>(Scope::Application, "sms-client", Vec::new(), |_| Ok(Arc::new(sms_client()))).expect("注册短信客户端");
    let audit_handle = Arc::clone(&audit);
    builder
        .register_qualified::<Arc<dyn Audit>, Primary, _>(Scope::Application, "audit", Vec::new(), move |_| {
            Ok(audit_handle.clone() as Arc<dyn Audit>)
        })
        .expect("注册审计后端");
    let environment = builder.freeze().expect("装配验证不联网，必须成功");
    environment.validate_root::<Notifier>().expect("根依赖完整");

    let notifier =
        environment.construct::<Notifier>(NotifierInput { sign_name: "示例签名".into(), template_id: "1234567".into() }).expect("构造通知组件");

    assert_eq!(notifier.sign_name, "示例签名");
    assert_eq!(notifier.template_id, "1234567");
    // 注入的客户端仍可离线计算签名；`PanickingTransport` 未被调用，因此没有网络请求。
    assert!(notifier.client.authorization("{}", 1_700_000_000).starts_with("TC3-HMAC-SHA256"));
    notifier.audit.record("constructed");
    assert_eq!(audit.records.load(Ordering::SeqCst), 1);
}

#[test]
fn injected_fields_are_not_part_of_the_author_input() {
    // `NotifierInput` 只有两个普通字段：注入字段无法由请求 DTO 填写。
    let input = NotifierInput { sign_name: "示例签名".into(), template_id: "1234567".into() };
    assert_eq!(input.sign_name, "示例签名");
    assert_eq!(Notifier::dependencies().len(), 2);
    assert!(Notifier::dependencies()[1].key().is_qualified());
    assert_eq!(Notifier::dependencies()[0].label(), "client");
    assert_eq!(Notifier::dependencies()[1].label(), "audit");
}

#[test]
fn database_settings_are_injected_and_the_host_opens_its_own_connection() {
    let mut builder = Environment::builder();
    builder
        .register::<Arc<SqliteSettings>, _>(Scope::Application, "sqlite", Vec::new(), |_| Ok(Arc::new(SqliteSettings { memory: true })))
        .expect("注册数据库配置");
    let environment = builder.freeze().expect("冻结环境");

    let store = environment.construct::<ReportStore>(ReportStoreInput { label: "日报".into() }).expect("构造报表存储");
    assert_eq!(store.label, "日报");
    assert!(store.settings.memory, "配置必须来自显式环境");

    // 连接不在注入键里，也不跨线程：持有该线程的宿主按注入的配置自己打开。
    // `sqlite::Connection` 实测不是 `Send`，因此这里只断言配置身份，不把数据库拖进 harness。
    assert!(Arc::strong_count(&store.settings) >= 2, "注入必须交付同一份配置句柄");
    assert_eq!(ReportStore::dependencies().len(), 1);
    assert_eq!(ReportStore::dependencies()[0].label(), "settings");
}

#[test]
fn missing_cloud_client_fails_without_partial_object() {
    let mut builder = Environment::builder();
    builder
        .register_qualified::<Arc<dyn Audit>, Primary, _>(Scope::Application, "audit", Vec::new(), |_| {
            Ok(Arc::new(RecordingAudit { records: AtomicUsize::new(0) }) as Arc<dyn Audit>)
        })
        .expect("注册审计后端");
    let environment = builder.freeze().expect("可选依赖之外仍可冻结");
    let error = environment
        .construct::<Notifier>(NotifierInput { sign_name: "示例签名".into(), template_id: "1234567".into() })
        .err()
        .expect("缺少云客户端必须失败");
    assert_eq!(error.kind(), atlas_runtime::wire::WireErrorKind::NotRegistered);
    assert!(error.path().contains(&"client"), "错误必须定位到注入字段：{}", error.path().join(" -> "));
}
