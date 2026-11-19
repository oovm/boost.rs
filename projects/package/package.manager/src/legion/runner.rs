//! 目标运行时家族与外部进程启动模板。
//!
//! 这里只拥有**进程模板**（可执行文件与参数中的占位符），不执行构建产物语义。
//! 占位符在调用方按产物事实展开：`{artifact}`、`{classpath}`、`{entry}`。

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

/// 产物给出的运行契约：入口符号名与 WASI 预览版本。
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeContract<'a> {
    /// 逻辑入口符号（可为空）。
    pub logical_entry: Option<&'a str>,
    /// 物理入口文件名（可为空）。
    pub physical_entry: Option<&'a str>,
    /// 是否要求 `wasip3` 语义。
    pub wasi_p3: bool,
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
    /// 该家族在给定契约下的默认模板。
    #[must_use]
    pub fn default_template(self, contract: Option<RuntimeContract<'_>>) -> RuntimeTemplate {
        let _ = contract;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasi_template_runs_the_artifact_through_wasmtime() {
        let template = RuntimeFamily::Wasi.default_template(None);
        assert_eq!(template.command, "wasmtime");
        assert!(template.args.contains(&"{artifact}".to_string()));
    }

    #[test]
    fn native_families_execute_the_artifact_directly() {
        for family in [RuntimeFamily::Windows, RuntimeFamily::Ovm] {
            let template = family.default_template(None);
            assert_eq!(template.command, "{artifact}");
            assert!(template.args.is_empty());
        }
    }
}
