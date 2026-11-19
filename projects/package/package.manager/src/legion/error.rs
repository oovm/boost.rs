//! Legion 操作错误（无 wrap 链，结构化事实进变体字段）。

use std::fmt::{Display, Formatter};

use crate::legion::workspace::{manifest::ManifestError, planner::PlannerError};
use data_text_core::TextError;

/// `legion::*` 装配 API 的 `Result` 别名。
pub type LegionResult<T> = std::result::Result<T, LegionError>;

/// Legion 操作失败。
#[derive(Debug)]
pub enum LegionError {
    /// Workspace / build plan。
    Planner(PlannerError),
    /// `legion.von` / `legions.von`。
    Manifest(ManifestError),
    /// 裸 I/O（无额外上下文时用 `?` 即可）。
    Io(std::io::Error),
    /// Oak/VON 解析，带操作上下文与展示标签。
    Oak {
        /// 操作上下文说明。
        context: String,
        /// VON/Oak 片段标签。
        label: String,
        /// 底层 Oak 解析错误。
        error: TextError,
    },
    /// 发射器驱动失败（待 emitter 结构化 `E` 后收窄）。
    Emitter {
        /// 用户可见说明。
        message: String,
    },
    /// 命令级一句说明（无更细枚举时）。
    Message(String),
}

impl LegionError {
    /// 用户可见短句（仅当没有更合适变体时）。
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub(crate) fn oak(context: impl Into<String>, label: impl Into<String>, error: TextError) -> Self {
        Self::Oak { context: context.into(), label: label.into(), error }
    }

    pub(crate) fn emitter(message: impl Into<String>) -> Self {
        Self::Emitter { message: message.into() }
    }
}

impl Display for LegionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Planner(error) => Display::fmt(error, f),
            Self::Manifest(error) => Display::fmt(error, f),
            Self::Io(error) => Display::fmt(error, f),
            Self::Oak { context, label, error } => {
                if let Some(offset) = error.source_offset() {
                    write!(f, "{context}: {label} [{offset}..{}]: {error}", offset.saturating_add(1))
                }
                else {
                    write!(f, "{context}: {label}: {error}")
                }
            }
            Self::Emitter { message } => f.write_str(message),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LegionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Planner(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Oak { error, .. } => Some(error),
            Self::Emitter { .. } | Self::Message(_) => None,
        }
    }
}

impl From<PlannerError> for LegionError {
    fn from(value: PlannerError) -> Self {
        Self::Planner(value)
    }
}

impl From<ManifestError> for LegionError {
    fn from(value: ManifestError) -> Self {
        Self::Manifest(value)
    }
}

impl From<std::io::Error> for LegionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<compiler_emitter::EmitterError> for LegionError {
    fn from(value: compiler_emitter::EmitterError) -> Self {
        Self::emitter(value.to_string())
    }
}

impl From<compiler_valkyrie::compiler::emitter::pipeline::PipelineError> for LegionError {
    fn from(value: compiler_valkyrie::compiler::emitter::pipeline::PipelineError) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<crate::asgard::error::AsgardError> for LegionError {
    fn from(value: crate::asgard::error::AsgardError) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<crate::ManagerError> for LegionError {
    fn from(value: crate::ManagerError) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<package_registry::RegistryError> for LegionError {
    fn from(value: package_registry::RegistryError) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<serde_json::Error> for LegionError {
    fn from(value: serde_json::Error) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<std::path::StripPrefixError> for LegionError {
    fn from(value: std::path::StripPrefixError) -> Self {
        Self::msg(value.to_string())
    }
}

impl From<crate::legion::workspace::script::ScriptError> for LegionError {
    fn from(value: crate::legion::workspace::script::ScriptError) -> Self {
        Self::msg(value.to_string())
    }
}
