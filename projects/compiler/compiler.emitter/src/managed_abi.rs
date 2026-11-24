//! Managed run 入口的 ABI 契约。
//!
//! 库产物（例如 `.jar`）没有零参 `main`，但仍需要机器可校验的执行入口。
//! 本模块只描述已经由目标 lowering 冻结的入口事实：逻辑入口名、主类中的
//! 静态方法、JVM 方法描述符，以及可选的 argv launcher 类；不补造未降低的入口。
//!
//! 入口 ABI 事实与 argv 包装能力是两件事：前者由语言冻结签名决定，后者只覆盖
//! 宿主命令行能无损解析的标量子集。数组、引用等签名仍然是合法的库入口事实，
//! 只是没有 argv launcher；`launcher_class = None` 表示「该入口不经 argv 编组」，
//! 不表示签名未知。禁止为了补齐 launcher 而发明 argv 语法或默认值。
//!
//! `contract_digest` 覆盖 lane、产物文件名、调用约定与按 `logical_entry`
//! 排序后的全部入口（含形参类型、形参声明名与 launcher 事实），任何入口事实
//! 变化都会改变摘要。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// managed ABI 契约 schema 版本。
///
/// 版本 2 起入口携带 `parameter_names`：消费侧按形参声明名绑定实参，禁止按
/// 位置猜测；该事实同时进入合同摘要，因此与版本 1 的摘要不可互认。
///
/// 版本 3 起 `launcher_class` 是 `Option`：入口事实与 argv 包装能力分离。
/// `None` 是「签名不可由 argv 无损表达」的显式事实，消费侧必须按静态方法绑定
/// 实参，不得据此推断入口不存在。
pub const MANAGED_ABI_SCHEMA_VERSION: u32 = 3;

/// managed ABI sidecar 文件名：每个物理产物一份，`app.jar` → `app.jar.managed-abi.json`。
///
/// 产物名缺失时保留兼容的 `managed-abi.json`，但多产物场景必须由驱动层先冻结
/// `artifact`，否则文件名无法唯一归属。
pub fn managed_abi_sidecar_file(artifact: &str) -> String {
    if artifact.is_empty() { "managed-abi.json".to_string() } else { format!("{artifact}.managed-abi.json") }
}

/// managed 入口 ABI 契约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedAbiContract {
    /// schema 版本，固定为 [`MANAGED_ABI_SCHEMA_VERSION`]。
    pub schema_version: u32,
    /// 目标 lane，`jvm` 或 `clr`。
    pub lane: String,
    /// 产物文件名（仅文件名，不含目录）。
    pub artifact: String,
    /// 调用约定名。
    pub calling_convention: String,
    /// 已冻结的入口列表，按 `logical_entry` 排序。
    pub entries: Vec<ManagedAbiEntry>,
    /// 对小写十六进制 sha256 摘要。
    pub contract_digest: String,
}

impl ManagedAbiContract {
    /// 构造契约并计算摘要；`entries` 会按 `logical_entry` 稳定排序。
    ///
    /// `calling_convention` 由入口事实派生：存在 argv launcher 时声明
    /// `static-argv-launcher`，全部入口都只能静态绑定实参时声明 `static-method`。
    /// 它描述契约实际提供的能力，不按 lane 或产物名猜测。
    pub fn new(lane: impl Into<String>, artifact: impl Into<String>, mut entries: Vec<ManagedAbiEntry>) -> Self {
        entries.sort_by(|left, right| left.logical_entry.cmp(&right.logical_entry));
        let calling_convention = if entries.iter().any(|entry| entry.launcher_class.is_some()) {
            "static-argv-launcher"
        }
        else {
            "static-method"
        };
        let mut contract = Self {
            schema_version: MANAGED_ABI_SCHEMA_VERSION,
            lane: lane.into(),
            artifact: artifact.into(),
            calling_convention: calling_convention.to_string(),
            entries,
            contract_digest: String::new(),
        };
        contract.refresh_digest();
        contract
    }

    /// 设置产物文件名并重新计算摘要。
    pub fn set_artifact(&mut self, artifact: impl Into<String>) {
        self.artifact = artifact.into();
        self.refresh_digest();
    }

    /// 重新计算 [`Self::contract_digest`]。
    pub fn refresh_digest(&mut self) {
        self.contract_digest = self.compute_digest();
    }

    /// 生成摘要的规范文本；入口已按 `logical_entry` 排序。
    pub fn canonical_digest_input(&self) -> String {
        let mut lines = Vec::with_capacity(self.entries.len() + 3);
        lines.push(format!("lane={}", self.lane));
        lines.push(format!("artifact={}", self.artifact));
        lines.push(format!("calling_convention={}", self.calling_convention));
        for entry in &self.entries {
            lines.push(format!(
                "{}|{}|{}|{}|{}|{}|{}|{}",
                entry.logical_entry,
                entry.owner,
                entry.method,
                entry.signature,
                entry.parameter_types.join(","),
                entry.parameter_names.join(","),
                entry.return_type,
                // 无 launcher 是已冻结事实；用显式哨兵参与摘要，禁止与空名字混淆。
                entry.launcher_class.as_deref().unwrap_or("-"),
            ));
        }
        lines.join("\n")
    }

    fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_digest_input().as_bytes());
        hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// 单个 managed 入口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedAbiEntry {
    /// 逻辑入口名（导出名）。
    pub logical_entry: String,
    /// 主类内部名（JVM 形态带 `/`）。
    pub owner: String,
    /// 主类中的静态方法名。
    pub method: String,
    /// managed 方法描述符，例如 `(J)J`。
    pub signature: String,
    /// 形参的 managed 标量类型名。
    pub parameter_types: Vec<String>,
    /// 形参的声明名，按声明顺序排列且与 [`Self::parameter_types`] 等长。
    ///
    /// 名字来自语言冻结签名（`ValueOrigin::Parameter`），消费侧按名绑定实参，
    /// 禁止按顺序猜测；缺失或非法时 lowering 结构化失败，不写空名字伪造。
    pub parameter_names: Vec<String>,
    /// 返回值的 managed 标量类型名。
    pub return_type: String,
    /// argv launcher 类内部名。
    ///
    /// `None` 表示该入口的签名不能由 argv 无损编组（数组、引用、字符/字符串等），
    /// 入口本身仍然是合法的库 ABI 事实，宿主应按 `owner`/`method`/`signature`
    /// 静态绑定实参。该字段不参与「入口是否存在」的判定。
    pub launcher_class: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sidecar 文件名必须由物理产物唯一派生，多分区不得互相覆盖。
    #[test]
    fn sidecar_file_is_derived_from_the_physical_artifact() {
        assert_eq!(managed_abi_sidecar_file("app.jar"), "app.jar.managed-abi.json");
        assert_eq!(managed_abi_sidecar_file("server.dll"), "server.dll.managed-abi.json");
        assert_eq!(managed_abi_sidecar_file(""), "managed-abi.json");
    }

    fn entry_with_names(names: &[&str]) -> ManagedAbiEntry {
        ManagedAbiEntry {
            logical_entry: "sum".to_string(),
            owner: "nyar/App".to_string(),
            method: "sum".to_string(),
            signature: "(JJ)J".to_string(),
            parameter_types: vec!["i64".to_string(), "i64".to_string()],
            parameter_names: names.iter().map(|name| (*name).to_string()).collect(),
            return_type: "i64".to_string(),
            launcher_class: Some("nyar/invoke/sum".to_string()),
        }
    }

    /// 形参声明名进入摘要：同名类型不同参数名必须产生不同合同摘要。
    #[test]
    fn digest_input_covers_parameter_names() {
        assert_eq!(MANAGED_ABI_SCHEMA_VERSION, 3);
        let left = ManagedAbiContract::new("jvm", "app.jar", vec![entry_with_names(&["left", "right"])]);
        let right = ManagedAbiContract::new("jvm", "app.jar", vec![entry_with_names(&["first", "second"])]);
        assert!(left.canonical_digest_input().contains("i64,i64|left,right|i64"), "got: {}", left.canonical_digest_input());
        assert_ne!(left.contract_digest, right.contract_digest, "形参名变化必须改变合同摘要");
    }

    /// 无 launcher 是显式已冻结事实：进入摘要、与有 launcher 的入口不可互认，
    /// 且不得写成空字符串冒充。
    #[test]
    fn digest_input_covers_launcher_presence() {
        assert_eq!(MANAGED_ABI_SCHEMA_VERSION, 3);
        let with_launcher = ManagedAbiContract::new("jvm", "app.jar", vec![entry_with_names(&["left", "right"])]);
        let mut entry = entry_with_names(&["left", "right"]);
        entry.launcher_class = None;
        let without_launcher = ManagedAbiContract::new("jvm", "app.jar", vec![entry]);
        assert!(without_launcher.canonical_digest_input().ends_with("|-"), "got: {}", without_launcher.canonical_digest_input());
        assert_ne!(with_launcher.contract_digest, without_launcher.contract_digest, "launcher 事实变化必须改变合同摘要");
        assert_eq!(with_launcher.calling_convention, "static-argv-launcher");
        assert_eq!(without_launcher.calling_convention, "static-method");
    }
}
