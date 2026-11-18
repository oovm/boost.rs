//! `package.manager` 冒烟测试：安装、锁文件与离线重装。

use std::{collections::HashMap, path::Path, sync::Arc};

use package_manager::{ManagerServices, PackageManager, ProjectLayout};
use package_registry::{MockRegistry, Package, Registry};

/// 串行执行并隔离用户数据目录：缓存与 auth 都落在用例自己的临时根下。
fn with_home<T>(root: &Path, f: impl FnOnce(ProjectLayout) -> T) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    // SAFETY: 由进程内互斥保护，测试串行设置该变量。
    unsafe {
        std::env::set_var("PACKAGE_HOME", &home);
    }
    f(ProjectLayout::neutral())
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("package-manager-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("project root");
    root
}

fn mock_registries() -> HashMap<String, Arc<dyn Registry>> {
    let registry = Arc::new(MockRegistry::new("npm", "https://example.invalid"));
    registry.insert_package(Package { name: "demo".to_string(), version: "1.2.0".to_string(), ..Package::default() });
    let mut registries: HashMap<String, Arc<dyn Registry>> = HashMap::new();
    registries.insert("npm".to_string(), registry);
    registries
}

#[test]
fn installs_a_package_and_records_the_lockfile() {
    let root = temp_root("install");
    with_home(&root, |layout| {
        std::fs::write(root.join(layout.package_manifest), "{\n    name: \"app\",\n    version: \"0.1.0\",\n}\n").expect("manifest");

        let mut manager = PackageManager::open_with_registries(&root, mock_registries(), layout).expect("open");
        let info = manager.install_one("demo", "1.2.0", "npm", false).expect("installed");
        assert_eq!(info.version, "1.2.0");
        assert!(manager.base_directory().join("vendors/npm/demo@1.2.0").is_dir());

        let lock = package_resolve::LockFile::load(&root, layout).expect("lock");
        assert!(lock.packages.contains_key("demo@1.2.0"));
    });
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn offline_install_uses_the_cache() {
    let root = temp_root("offline");
    with_home(&root, |layout| {
        std::fs::write(
            root.join(layout.package_manifest),
            "{\n    name: \"app\",\n    version: \"0.1.0\",\n    dependencies: {\n        \"demo\": \"1.2.0\",\n    },\n}\n",
        )
        .expect("manifest");

        let mut manager = PackageManager::open_with_registries(&root, mock_registries(), layout).expect("open");
        manager.install_dependencies(false, "npm").expect("online install");

        let _ = std::fs::remove_dir_all(root.join("vendors"));
        let mut offline = PackageManager::open_with_registries(&root, mock_registries(), layout).expect("reopen");
        offline.offline = true;
        let installed = offline.install_dependencies(false, "npm").expect("offline install");
        assert_eq!(installed.len(), 1);
        assert!(offline.base_directory().join("vendors/npm/demo@1.2.0").is_dir());
    });
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_services_expose_no_credentials() {
    let services = ManagerServices::default();
    assert!(services.credentials.discover("npm", None, None).is_none());
}
