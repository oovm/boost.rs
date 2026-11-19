//! 模块事实层：模块记录类型与事实来源合同。
//!
//! 具体语言适配层（例如 JS/TS 的 `data.text` 前端）实现 [`ModuleFactSource`]，把源码解析成
//! [`record::ModuleRecord`]；本 crate 不引入任何语法前端，也不做正则识别。

pub mod record;

pub use record::{
    AssetFact, ExportFact, ImportBinding, ImportFact, ImportKind, ImportedName, ModuleFlags, ModuleRecord, RequireFact, ScopeBindingFact,
    ScopeIncompleteReason, StatementEffect, StatementFact,
};

use crate::diag::PackDiagnostic;

/// 源码 → 模块事实的来源合同。
pub trait ModuleFactSource {
    /// 解析 `source`；`label` 是诊断用的来源身份（可为空）。
    ///
    /// 失败时返回结构化诊断，调用方据此产出「事实不完整」节点，不猜测语义。
    fn parse_module(&self, label: Option<&str>, source: &str) -> Result<ModuleRecord, Vec<PackDiagnostic>>;
}
