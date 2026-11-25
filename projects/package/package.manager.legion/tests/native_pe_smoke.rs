//! 机器码 lane 的最小发射检查：真实 `.v` 工程经正式产品构建产出可直接启动的 PE。
//!
//! 只在 Windows 构建宿主上运行；Linux 同源用例在 `valkyrie.rs` 的正式 CLI 验收里承载。

use std::{fs, path::PathBuf, process::Command};

use package_manager_legion::{
    legion::resolve_platform_target,
    product::{BuildOptions, BuildOutcome, build_project},
};

const PRIMITIVES: &str = concat!(
    "namespace std.primitive;\n",
    "[primitive(\"std::primitive::i32\")] structure i32 { }\n",
    "[primitive(\"std::primitive::i64\")] structure i64 { }\n",
);

const SOURCE: &str = r#"namespace app;

micro choose(flag: bool) -> bool {
    return flag
}

[main]
micro main() -> i64 {
    if app::choose(true) {
        return 42
    }
    return 7
}
"#;

const MANIFEST_TEMPLATE: &str = r#"{
    name: "app",
    build: [
        { target: "%TARGET%" }
    ]
}
"#;

fn write_project(root: &std::path::Path, manifest_target: &str, source: &str) -> PathBuf {
    let source_dir = root.join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        root.join("legions.von"),
        r#"{
    name: "native-smoke",
    members: []
}
"#,
    )
    .unwrap();
    fs::write(root.join("legion.von"), MANIFEST_TEMPLATE.replace("%TARGET%", manifest_target)).unwrap();
    fs::write(root.join("source").join("main.v"), source).unwrap();
    fs::write(source_dir.join("core_primitive.v"), PRIMITIVES).unwrap();
    root.to_path_buf()
}

fn build(prefix: &str, source: &str, target: &str) -> (BuildOutcome, PathBuf) {
    build_for(prefix, source, target, target)
}

fn build_for(prefix: &str, source: &str, target: &str, manifest_target: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("legion-native-pe-{prefix}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project_dir = write_project(&root, manifest_target, source);
    let output_dir = project_dir.join("dist").join("native");
    let outcome = build_project(&BuildOptions {
        project_dir,
        target: Some(target.to_string()),
        output_dir: Some(output_dir),
    });
    (outcome, root)
}

fn executable_of(outcome: &BuildOutcome) -> PathBuf {
    outcome
        .artifacts
        .iter()
        .map(PathBuf::from)
        .find(|path| path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("exe")))
        .unwrap_or_else(|| panic!("没有产出 `.exe` 产物: {:?}", outcome.artifacts))
}

fn assert_runnable_pe(executable: &std::path::Path) -> i32 {
    let bytes = fs::read(executable).expect("read PE artifact");
    assert_eq!(&bytes[..2], b"MZ", "产物必须是 PE 文件");
    let pe_offset = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe_offset..pe_offset + 4], b"PE\0\0", "缺少 PE 签名");
    let machine = u16::from_le_bytes(bytes[pe_offset + 4..pe_offset + 6].try_into().unwrap());
    assert_eq!(machine, 0x8664, "PE 架构必须是 AMD64");
    let optional = pe_offset + 24;
    let entry = u32::from_le_bytes(bytes[optional + 16..optional + 20].try_into().unwrap());
    assert_ne!(entry, 0, "PE AddressOfEntryPoint 必须指向冻结入口");

    Command::new(executable).status().expect("启动 PE 产物").code().expect("PE 进程有退出码")
}

#[cfg(windows)]
#[test]
fn builds_runnable_windows_pe_with_expected_exit_code() {
    let (outcome, root) = build("explicit", SOURCE, "x86_64-pc-windows-msvc");
    assert!(outcome.ok, "legion build 失败: {:?}", outcome.error);
    let executable = executable_of(&outcome);
    let manifest = fs::read_to_string(executable.parent().unwrap().join("run-contracts.txt")).expect("native build must emit run contracts");
    assert!(manifest.contains("main"), "运行清单必须登记冻结入口: {manifest}");
    assert!(
        manifest.contains(executable.file_name().unwrap().to_str().unwrap()),
        "运行清单必须指向实际 PE 产物: {manifest}"
    );
    assert_eq!(assert_runnable_pe(&executable), 42, "退出码必须来自冻结入口的返回值");
    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn native_placeholder_resolves_like_explicit_windows_target() {
    let native = resolve_platform_target("native").expect("native");
    let explicit = resolve_platform_target("x86_64-pc-windows-msvc").expect("explicit windows");
    assert_eq!(native.platform, explicit.platform, "`native` 必须展开成与显式目标相同的平台描述");
    assert_eq!(native.contract, explicit.contract, "`native` 必须与显式机器目标携带同一产物合同");

    let (outcome, root) = build("native", SOURCE, "native");
    assert!(outcome.ok, "legion build --target native 失败: {:?}", outcome.error);
    let executable = executable_of(&outcome);
    assert_eq!(assert_runnable_pe(&executable), 42, "`native` 与显式目标必须产出等价行为");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn linux_target_emits_sysv_elf_from_the_same_source() {
    let (outcome, root) = build("linux", SOURCE, "x86_64-unknown-linux-gnu");
    assert!(outcome.ok, "linux legion build 失败: {:?}", outcome.error);
    let executable = PathBuf::from(outcome.artifacts.first().expect("linux artifact"));
    let bytes = fs::read(&executable).expect("read ELF artifact");
    assert_eq!(&bytes[..4], b"\x7fELF", "产物必须是 ELF");
    assert_eq!(bytes[4], 2, "ELF 必须是 64 位");
    assert_eq!(u16::from_le_bytes(bytes[18..20].try_into().unwrap()), 62, "ELF 架构必须是 x86-64");
    let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    assert_ne!(entry, 0, "ELF e_entry 必须指向冻结入口");
    // 真实装载：Linux x86-64 上由内核把镜像跳到冻结入口，进程以返回值退出。
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&executable).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("chmod");
        let status = std::process::Command::new(&executable).status().expect("load and run ELF");
        assert_eq!(status.code(), Some(42), "ELF 真实执行必须返回冻结入口值");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn same_source_builds_both_native_image_and_jvm_lane() {
    // 同一份源码分别走 x86-64 进程镜像与 JVM 两条正式产物路径。两条路径必须各自
    // 完成验证并产出自己的产物：JVM lane 不借用机器码结果，机器码 lane 也不依赖
    // JVM 的 class 输出。
    let (native, native_root) = build("same-source-native", SOURCE, "native");
    assert!(native.ok, "native lane build 失败: {:?}", native.error);
    assert!(
        native.artifacts.iter().any(|artifact| artifact.ends_with(".exe") || artifact.ends_with(".elf")),
        "native lane 必须产出机器码镜像: {:?}",
        native.artifacts
    );

    let (jvm, jvm_root) = build("same-source-jvm", SOURCE, "jvm");
    assert!(jvm.ok, "jvm lane build 失败: {:?}", jvm.error);
    assert!(!jvm.artifacts.is_empty(), "jvm lane 必须产出自己的产物");
    assert!(
        !jvm.artifacts.iter().any(|artifact| artifact.ends_with(".exe") || artifact.ends_with(".elf")),
        "jvm lane 不得借用机器码镜像: {:?}",
        jvm.artifacts
    );
    // JVM 产物必须自己通过容器验证：真实 JAR（ZIP 头）且内含片段类，不是空壳。
    let jar = jvm
        .artifacts
        .iter()
        .find(|artifact| artifact.ends_with(".jar"))
        .unwrap_or_else(|| panic!("jvm lane 必须产出 JAR 容器: {:?}", jvm.artifacts));
    let jar_bytes = fs::read(jar).expect("read JAR artifact");
    assert_eq!(&jar_bytes[..4], b"PK\x03\x04", "JVM 产物必须是真实 JAR 容器");
    assert!(jar_bytes.windows(4).any(|window| window == b"nyar"), "JAR 必须包含本片段自己的类");

    let _ = fs::remove_dir_all(&native_root);
    let _ = fs::remove_dir_all(&jvm_root);
}

#[test]
fn binary_without_frozen_entry_fails_at_entry_stage() {
    let (outcome, root) = build(
        "missing-entry",
        r#"namespace app;

micro helper(x: i64) -> i64 {
    return x
}
"#,
        "x86_64-pc-windows-msvc",
    );
    assert!(!outcome.ok, "缺少冻结入口的 binary 不得成功: {:?}", outcome.artifacts);
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("入口") || error.contains("entry"),
        "错误必须来自入口阶段并指名入口身份，实际: {error}"
    );
    let _ = fs::remove_dir_all(&root);
}
