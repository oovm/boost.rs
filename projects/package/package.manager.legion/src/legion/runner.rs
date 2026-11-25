//! 目标运行时家族与外部进程启动模板。
//!
//! 这里只拥有**进程模板**（可执行文件与参数中的占位符），不执行构建产物语义。
//! 占位符在调用方按产物事实展开：`{artifact}`、`{classpath}`、`{entry}`。
//!
//! 进程模板只有一个事实源：Compiler 冻结的 [`RunContract::launch`]。家族默认模板
//! 只用于「合同没有冻结启动 argv」的旧产物或无合同覆盖路径；两处都不得按入口名、
//! 产物扩展名或目标家族再猜一次调用方式。

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::legion::cmds::error::{LegionError, LegionResult};

/// 运行产物时使用的目标运行时家族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFamily {
    /// .NET CLR（`dotnet exec`）。
    Clr,
    /// JVM（`java -jar`）。
    Jvm,
    /// Node（`node`）。
    Node,
    /// 原生 Windows 可执行文件。
    Windows,
    /// WASI（`wasmtime run`）。
    Wasi,
    /// 自有 OVM 可执行产物。
    Ovm,
}

/// 一条产物运行合同。
///
/// `launch` 是 Compiler 冻结的启动 argv（第 0 项是可执行文件，其余是参数），
/// 只含 `{artifact}` 占位符；入口符号等 lane 专属事实在编译期已写成具体值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContract {
    /// 逻辑入口符号名。
    pub logical_entry: String,
    /// 物理入口（产物中的文件名或符号）。
    pub physical_entry: String,
    /// 冻结的宿主边界；消费者据此选择宿主，不再派生返回值观测方式。
    /// 旧 manifest 缺少该字段时按 [`RunHostBoundary::Unknown`] 读取，新鲜度校验
    /// 必须判 stale，不得静默放行。
    #[serde(default)]
    pub boundary: RunHostBoundary,
    /// 入口包装冻结的返回值观测投影（结果通道 + 值宽 + 解码规则）。
    /// 由生成入口包装的目标后端逐条写入，Legion 只消费；缺失按
    /// [`RunEntryProjection::Unknown`] 读取并判 stale。
    #[serde(default)]
    pub return_projection: RunEntryProjection,
    /// 冻结的启动 argv；旧 manifest 缺少该字段时为空。
    #[serde(default)]
    pub launch: Vec<String>,
    /// 校验入口可用的命令模板。
    pub validate: String,
}

/// 运行合同冻结的宿主边界在 VON 清单里的传输形态。
///
/// VON 能序列化 unit variant，但不能反序列化枚举，因此清单字段使用本枚举的显式
/// 词表。词表与 [`compiler_target::HostProjectionBoundary`] 一一对应：新增边界
/// 会因转换 `match` 不穷尽而在编译期暴露，不会静默漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunHostBoundary {
    /// 旧清单缺失或词表未知：fail-closed，不得据此启动或观测返回值。
    #[default]
    Unknown,
    /// `.NET` 宿主边界。
    Clr,
    /// `JVM` 宿主边界。
    Jvm,
    /// `WASM + JS glue` 宿主边界。
    WasmJsGlue,
    /// `WASI component model` 宿主边界。
    WasiComponent,
    /// 机器码宿主边界。
    MachineCode,
    /// 字节码虚拟机宿主边界。
    Vm,
}

impl From<compiler_target::HostProjectionBoundary> for RunHostBoundary {
    fn from(value: compiler_target::HostProjectionBoundary) -> Self {
        match value {
            compiler_target::HostProjectionBoundary::Clr => Self::Clr,
            compiler_target::HostProjectionBoundary::Jvm => Self::Jvm,
            compiler_target::HostProjectionBoundary::WasmJsGlue => Self::WasmJsGlue,
            compiler_target::HostProjectionBoundary::WasiComponent => Self::WasiComponent,
            compiler_target::HostProjectionBoundary::MachineCode => Self::MachineCode,
            compiler_target::HostProjectionBoundary::Vm => Self::Vm,
        }
    }
}

impl TryFrom<RunHostBoundary> for compiler_target::HostProjectionBoundary {
    type Error = &'static str;

    fn try_from(value: RunHostBoundary) -> std::result::Result<Self, Self::Error> {
        match value {
            RunHostBoundary::Unknown => Err("run contract is missing a frozen host boundary"),
            RunHostBoundary::Clr => Ok(Self::Clr),
            RunHostBoundary::Jvm => Ok(Self::Jvm),
            RunHostBoundary::WasmJsGlue => Ok(Self::WasmJsGlue),
            RunHostBoundary::WasiComponent => Ok(Self::WasiComponent),
            RunHostBoundary::MachineCode => Ok(Self::MachineCode),
            RunHostBoundary::Vm => Ok(Self::Vm),
        }
    }
}

impl<'de> Deserialize<'de> for RunHostBoundary {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BoundaryVisitor;

        impl<'de> serde::de::Visitor<'de> for BoundaryVisitor {
            type Value = RunHostBoundary;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("'unknown' / 'clr' / 'jvm' / 'wasm_js_glue' / 'wasi_component' / 'machine_code' / 'vm'")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                // 归一化大小写与分隔符：清单词表、Rust variant 名都能读取。
                match value.to_ascii_lowercase().replace('_', "").as_str() {
                    "unknown" => Ok(RunHostBoundary::Unknown),
                    "clr" => Ok(RunHostBoundary::Clr),
                    "jvm" => Ok(RunHostBoundary::Jvm),
                    "wasmjsglue" => Ok(RunHostBoundary::WasmJsGlue),
                    "wasicomponent" => Ok(RunHostBoundary::WasiComponent),
                    "machinecode" => Ok(RunHostBoundary::MachineCode),
                    "vm" => Ok(RunHostBoundary::Vm),
                    other => Err(E::custom(format!("unsupported host boundary '{other}'"))),
                }
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(BoundaryVisitor)
    }
}

/// 运行合同冻结的入口返回值观测投影在 VON 清单里的传输形态。
///
/// 与 [`RunHostBoundary`] 同一词表模式：投影事实由生成入口包装的目标后端逐条
/// 冻结，Legion 只消费；缺失/未知一律 `Unknown` 并 fail-closed。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunEntryProjection {
    /// 旧清单缺失或词表未知：不得据此观测入口返回值。
    #[default]
    Unknown,
    /// 入口包装把 i32 返回值以十进制行打印到 stdout，自身以 0 退出。
    StdoutInteger,
    /// 入口返回值就是进程退出码（低 8 位截断）。
    ProcessExitCode,
    /// 宿主在进程内调用入口，返回值是 VM 内的 i32 值。
    InProcessValue,
    /// 入口没有返回值可观测。
    NoReturnValue,
}

impl From<compiler_target::EntryReturnProjection> for RunEntryProjection {
    fn from(value: compiler_target::EntryReturnProjection) -> Self {
        match value {
            compiler_target::EntryReturnProjection::StdoutInteger => Self::StdoutInteger,
            compiler_target::EntryReturnProjection::ProcessExitCode => Self::ProcessExitCode,
            compiler_target::EntryReturnProjection::InProcessValue => Self::InProcessValue,
            compiler_target::EntryReturnProjection::NoReturnValue => Self::NoReturnValue,
        }
    }
}

impl TryFrom<RunEntryProjection> for compiler_target::EntryReturnProjection {
    type Error = &'static str;

    fn try_from(value: RunEntryProjection) -> std::result::Result<Self, Self::Error> {
        match value {
            RunEntryProjection::Unknown => Err("run contract is missing a frozen entry return projection"),
            RunEntryProjection::StdoutInteger => Ok(Self::StdoutInteger),
            RunEntryProjection::ProcessExitCode => Ok(Self::ProcessExitCode),
            RunEntryProjection::InProcessValue => Ok(Self::InProcessValue),
            RunEntryProjection::NoReturnValue => Ok(Self::NoReturnValue),
        }
    }
}

impl<'de> Deserialize<'de> for RunEntryProjection {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ProjectionVisitor;

        impl<'de> serde::de::Visitor<'de> for ProjectionVisitor {
            type Value = RunEntryProjection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("'unknown' / 'stdout_integer' / 'process_exit_code' / 'in_process_value' / 'no_return_value'")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.to_ascii_lowercase().replace('_', "").as_str() {
                    "unknown" => Ok(RunEntryProjection::Unknown),
                    "stdoutinteger" => Ok(RunEntryProjection::StdoutInteger),
                    "processexitcode" => Ok(RunEntryProjection::ProcessExitCode),
                    "inprocessvalue" => Ok(RunEntryProjection::InProcessValue),
                    "noreturnvalue" => Ok(RunEntryProjection::NoReturnValue),
                    other => Err(E::custom(format!("unsupported entry return projection '{other}'"))),
                }
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(ProjectionVisitor)
    }
}

/// 一条外部进程模板。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTemplate {
    /// 可执行文件名或绝对路径。
    pub command: String,
    /// 参数列表（含占位符）。
    pub args: Vec<String>,
}

impl RuntimeFamily {
    /// 该家族的默认模板，只在合同没有冻结启动 argv 时使用。
    #[must_use]
    pub fn default_template(self) -> RuntimeTemplate {
        match self {
            Self::Clr => RuntimeTemplate { command: "dotnet".into(), args: vec!["exec".into(), "{artifact}".into()] },
            Self::Jvm => RuntimeTemplate { command: "java".into(), args: vec!["-jar".into(), "{artifact}".into()] },
            Self::Node => RuntimeTemplate { command: "node".into(), args: vec!["{artifact}".into()] },
            Self::Windows => RuntimeTemplate { command: "{artifact}".into(), args: Vec::new() },
            Self::Wasi => RuntimeTemplate {
                command: "wasmtime".into(),
                args: vec!["run".into(), "-W".into(), "gc".into(), "-W".into(), "max-memory-size=16777216".into(), "{artifact}".into()],
            },
            Self::Ovm => RuntimeTemplate { command: "{artifact}".into(), args: Vec::new() },
        }
    }

    /// 按合同决定进程模板：合同冻结的 argv 优先，缺失时才退回家族默认模板。
    #[must_use]
    pub fn resolve_template(self, contract: Option<&RunContract>) -> RuntimeTemplate {
        contract.and_then(|contract| template_from_launch(&contract.launch)).unwrap_or_else(|| self.default_template())
    }
}

/// 合同冻结 argv → 进程模板；argv 为空或可执行文件为空时返回 `None`。
fn template_from_launch(launch: &[String]) -> Option<RuntimeTemplate> {
    let (command, args) = launch.split_first()?;
    if command.trim().is_empty() {
        return None;
    }
    Some(RuntimeTemplate { command: command.clone(), args: args.to_vec() })
}

/// 构造展开启动模板所需的占位符表。
///
/// `{artifact}` 是解析后的产物路径；`{classpath}` 对 `jar` 产物是自身路径，
/// 其它产物是输出目录；`{entry}` 按家族取合同入口（`JVM`/`OVM` 取逻辑入口，
/// 其它家族取物理入口）。
pub fn launch_placeholders(
    output_dir: &Path,
    artifact: &Path,
    family: RuntimeFamily,
    contract: Option<&RunContract>,
) -> LegionResult<BTreeMap<&'static str, String>> {
    let mut values = BTreeMap::new();
    let artifact_text = strip_verbatim_prefix(artifact.to_string_lossy().as_ref()).to_owned();
    values.insert("artifact", artifact_text.clone());

    let classpath = if artifact.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jar")) {
        artifact_text
    }
    else {
        strip_verbatim_prefix(output_dir.to_string_lossy().as_ref()).to_owned()
    };
    values.insert("classpath", classpath);

    // `{entry}` 是调用符号。`OVM` / `JVM` 把模块文件放在 `{artifact}`（或 classpath）
    // 里，入口是合同里的逻辑入口，不能复用 `physical_entry` 文件名。
    let entry = match contract {
        Some(contract) if matches!(family, RuntimeFamily::Jvm | RuntimeFamily::Ovm) && !contract.logical_entry.is_empty() => {
            contract.logical_entry.clone()
        }
        Some(contract) if !contract.physical_entry.is_empty() => contract.physical_entry.clone(),
        Some(contract) if !contract.logical_entry.is_empty() => contract.logical_entry.clone(),
        _ if family == RuntimeFamily::Jvm => artifact
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| LegionError::msg(format!("cannot infer JVM entry from {}", artifact.display())))?,
        _ => artifact.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default().to_string(),
    };
    values.insert("entry", entry);
    Ok(values)
}

/// 展开模板中的全部占位符。
#[must_use]
pub fn expand_placeholders(template: &str, placeholders: &BTreeMap<&'static str, String>) -> String {
    placeholders.iter().fold(template.to_string(), |current, (key, value)| current.replace(&format!("{{{key}}}"), value))
}

/// 逐项展开参数列表中的占位符。
#[must_use]
pub fn expand_args(args: &[String], placeholders: &BTreeMap<&'static str, String>) -> Vec<String> {
    args.iter().map(|value| expand_placeholders(value, placeholders)).collect()
}

/// 去掉 Windows verbatim 前缀（`\\?\`），交给外部进程的路径不带该前缀。
#[must_use]
pub fn strip_verbatim_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(logical: &str, physical: &str, launch: &[&str]) -> RunContract {
        RunContract {
            logical_entry: logical.to_string(),
            physical_entry: physical.to_string(),
            // 模板解析用例不消费宿主边界；这里保持缺失事实，避免伪造观测投影。
            boundary: RunHostBoundary::Unknown,
            return_projection: RunEntryProjection::Unknown,
            launch: launch.iter().map(|part| (*part).to_string()).collect(),
            validate: String::new(),
        }
    }

    #[test]
    fn wasi_template_runs_the_artifact_through_wasmtime() {
        let template = RuntimeFamily::Wasi.default_template();
        assert_eq!(template.command, "wasmtime");
        assert!(template.args.contains(&"{artifact}".to_string()));
    }

    #[test]
    fn native_families_execute_the_artifact_directly() {
        for family in [RuntimeFamily::Windows, RuntimeFamily::Ovm] {
            let template = family.default_template();
            assert_eq!(template.command, "{artifact}");
            assert!(template.args.is_empty());
        }
    }

    #[test]
    fn frozen_launch_wins_over_family_template() {
        let contract = contract("sum", "app.jar", &["java", "-cp", "{artifact}", "nyar.invoke.sum"]);
        let template = RuntimeFamily::Jvm.resolve_template(Some(&contract));
        assert_eq!(template.command, "java");
        assert_eq!(template.args, vec!["-cp", "{artifact}", "nyar.invoke.sum"]);
    }

    #[test]
    fn missing_launch_falls_back_to_family_template() {
        let contract = contract("sum", "app.jar", &[]);
        let template = RuntimeFamily::Jvm.resolve_template(Some(&contract));
        assert_eq!(template, RuntimeFamily::Jvm.default_template());
    }

    #[test]
    fn entry_placeholder_prefers_logical_entry_for_managed_families() {
        let contract = contract("sum", "app.jar", &[]);
        let placeholders = launch_placeholders(Path::new("dist"), Path::new("dist/app.jar"), RuntimeFamily::Jvm, Some(&contract))
            .expect("占位符必须可构造");
        assert_eq!(placeholders.get("entry").map(String::as_str), Some("sum"));
        assert_eq!(placeholders.get("classpath").map(String::as_str), Some("dist/app.jar"));
    }

    #[test]
    fn entry_placeholder_prefers_physical_entry_for_native_families() {
        let contract = contract("main", "app.mjs", &[]);
        let placeholders = launch_placeholders(Path::new("dist"), Path::new("dist/app.mjs"), RuntimeFamily::Node, Some(&contract))
            .expect("占位符必须可构造");
        assert_eq!(placeholders.get("entry").map(String::as_str), Some("app.mjs"));
    }

    #[test]
    fn run_host_boundary_round_trips_and_fails_closed() {
        let frozen = RunContract {
            boundary: RunHostBoundary::WasmJsGlue,
            return_projection: RunEntryProjection::ProcessExitCode,
            ..contract("main", "app.wasm", &["node", "{artifact}"])
        };
        let text = crate::legion::write_von_indented(&frozen).expect("清单必须可序列化");
        assert!(text.contains("boundary=wasm_js_glue"), "{text}");
        assert!(text.contains("return_projection=process_exit_code"), "{text}");
        let parsed: RunContract = crate::legion::parse_von(&text).expect("清单必须可反序列化");
        assert_eq!(parsed.host_boundary(), Some(compiler_target::HostProjectionBoundary::WasmJsGlue));
        assert_eq!(parsed.entry_return_projection(), Some(compiler_target::EntryReturnProjection::ProcessExitCode));

        // 旧清单缺少边界与投影字段：按 `Unknown` 读取，消费必须 fail-closed。
        let legacy = crate::legion::write_von_indented(&contract("main", "app.jar", &["java"])).expect("清单必须可序列化");
        let parsed: RunContract = crate::legion::parse_von(&legacy).expect("旧清单必须可读取");
        assert_eq!(parsed.host_boundary(), None);
        assert_eq!(parsed.entry_return_projection(), None);

        // 未知词表不得静默降级成任何真实边界或通道。
        let unknown = text.replace("wasm_js_glue", "bogus_boundary");
        assert!(crate::legion::parse_von::<RunContract>(&unknown).is_err());
        let unknown = text.replace("process_exit_code", "bogus_channel");
        assert!(crate::legion::parse_von::<RunContract>(&unknown).is_err());
    }
}
