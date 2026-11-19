//! `package.resolve` 冒烟测试：清单 / 工作区 / lock 往返、版本约束与依赖求解。

use std::{collections::BTreeMap, sync::Arc};

use package_registry::{MockRegistry, Package};
use package_resolve::{
    DependencyResolver, DependencySpec, LockFile, PackageManifest, ProjectLayout, ProjectMode, WorkspaceManifest, satisfies_version_constraint,
};

#[test]
fn manifest_and_lock_round_trip_through_von() {
    let mut manifest = PackageManifest::parse("{\n    name: \"demo\",\n    version: \"1.2.3\",\n    description: \"demo package\",\n}\n")
        .expect("manifest parses");
    manifest.dependencies.insert("left".to_string(), DependencySpec::Version("^1.0".to_string()));
    manifest.dev_dependencies.insert(
        "right".to_string(),
        DependencySpec::Detailed { version: Some("~2.1".to_string()), path: None, registry: Some("npm".to_string()) },
    );

    let directory = std::env::temp_dir().join(format!("package-resolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp dir");
    manifest.save(&directory, ProjectLayout::neutral()).expect("manifest saved");
    assert!(PackageManifest::exists(&directory, ProjectLayout::neutral()));

    let reloaded_manifest = PackageManifest::load(&directory, ProjectLayout::neutral()).expect("manifest reloaded");
    assert_eq!(reloaded_manifest.name, "demo");
    assert_eq!(reloaded_manifest.version, "1.2.3");
    assert_eq!(reloaded_manifest.description.as_deref(), Some("demo package"));
    assert_eq!(reloaded_manifest.dependencies.get("left"), Some(&DependencySpec::Version("^1.0".to_string())));

    let mut lock = LockFile::load(&directory, ProjectLayout::neutral()).expect("lock loaded");
    let package = Package { name: "left".to_string(), version: "1.4.0".to_string(), ..Package::default() };
    lock.add_package(&package, "npm", "https://registry.npmjs.org");
    lock.save().expect("lock saved");
    let reloaded = LockFile::load(&directory, ProjectLayout::neutral()).expect("lock reloaded");
    assert_eq!(reloaded.packages.len(), 1);
    assert_eq!(reloaded.packages["left@1.4.0"].install_path, "npm/left@1.4.0");
    assert!(reloaded.detect_drift(&[("left".to_string(), "9.9.9".to_string())]).len() == 1);

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn project_mode_discovers_workspace_members() {
    let root = std::env::temp_dir().join(format!("package-resolve-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let layout = ProjectLayout::neutral();

    std::fs::create_dir_all(root.join("crates/alpha")).expect("alpha dir");
    std::fs::create_dir_all(root.join("crates/beta")).expect("beta dir");
    std::fs::write(root.join(layout.workspace_manifest), "{\n    name: \"ws\",\n    members: [\"crates/alpha\", \"crates/beta\"],\n}\n")
        .expect("workspace manifest");
    std::fs::write(root.join("crates/alpha").join(layout.package_manifest), "{\n    name: \"alpha\",\n    version: \"0.1.0\",\n}\n")
        .expect("alpha manifest");
    std::fs::write(root.join("crates/beta").join(layout.package_manifest), "{\n    name: \"beta\",\n    version: \"0.1.0\",\n}\n")
        .expect("beta manifest");

    let mode = ProjectMode::discover(&root, layout).expect("mode discovered");
    let ProjectMode::Workspace { workspace, .. } = &mode
    else {
        panic!("expected workspace mode");
    };
    let members = workspace.member_packages(&root, layout).expect("members");
    assert!(members.contains_key("alpha"));
    assert!(members.contains_key("beta"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn version_constraints_cover_caret_tilde_wildcard_and_yearly() {
    assert!(satisfies_version_constraint("1.5.0", "^1.2"));
    assert!(!satisfies_version_constraint("2.0.0", "^1.2"));
    assert!(satisfies_version_constraint("1.2.9", "~1.2.1"));
    assert!(!satisfies_version_constraint("1.3.0", "~1.2.1"));
    assert!(satisfies_version_constraint("1.2.7", "1.2.*"));
    assert!(satisfies_version_constraint("2026.3.1.4", "2026.*"));
    assert!(!satisfies_version_constraint("1.0.0", ">=2.0"));
    assert!(satisfies_version_constraint("1.0.0", "*"));
}

#[test]
fn resolver_walks_transitive_dependencies_once() {
    let registry = Arc::new(MockRegistry::new("npm", "https://example.invalid"));
    registry.insert_package(Package {
        name: "root".to_string(),
        version: "1.0.0".to_string(),
        dependency_versions: BTreeMap::from([("child".to_string(), "2.0.0".to_string())]),
        ..Package::default()
    });
    registry.insert_package(Package { name: "child".to_string(), version: "2.0.0".to_string(), ..Package::default() });

    let mut registries: std::collections::HashMap<String, Arc<dyn package_registry::Registry>> = std::collections::HashMap::new();
    registries.insert("npm".to_string(), registry);
    let resolver = DependencyResolver::new(registries);

    let mut dependencies = BTreeMap::new();
    dependencies.insert("root".to_string(), DependencySpec::Version("latest".to_string()));
    let forest = resolver.resolve_all(&dependencies, "npm").expect("resolved");
    assert_eq!(forest.len(), 1);
    assert_eq!(forest[0].name, "root");
    assert_eq!(forest[0].children.len(), 1);
    assert_eq!(forest[0].children[0].name, "child");
}
