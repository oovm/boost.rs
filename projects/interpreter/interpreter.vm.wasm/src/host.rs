//! 导入分类：把任意导入名归入 WASI preview1、component-model 或普通宿主模块。

use crate::{
    error::WasmError,
    wasi::{ComponentDirection, component_direction, wasi_preview1_import},
};

/// 导入种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportKind {
    /// WASI preview1 函数。
    WasiPreview1(crate::wasi::WasiPreview1Import),
    /// component-model 接口，附带方向与所属包。
    ComponentModel {
        /// 链接方向。
        direction: ComponentDirection,
        /// 所属包（`namespace:package`）。
        package: String,
    },
    /// 其他宿主模块（`module.field`）。
    Host {
        /// 模块名。
        module: String,
        /// 字段名。
        field: String,
    },
}

/// 宿主边界：记录导入分类并拒绝无法解析的名称。
#[derive(Debug, Default, Clone)]
pub struct HostBoundary {
    imports: Vec<(String, ImportKind)>,
}

impl HostBoundary {
    /// 创建一个空边界。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个导入名并返回分类。
    pub fn declare(&mut self, name: &str) -> Result<&ImportKind, WasmError> {
        let kind = classify(name)?;
        self.imports.push((name.to_owned(), kind));
        Ok(&self.imports.last().expect("just pushed").1)
    }

    /// 已登记导入数。
    pub fn len(&self) -> usize {
        self.imports.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.imports.is_empty()
    }

    /// 所有需要文件系统授权的导入名。
    pub fn filesystem_imports(&self) -> Vec<&str> {
        self.imports
            .iter()
            .filter(|(_, kind)| matches!(kind, ImportKind::WasiPreview1(import) if import.touches_filesystem()))
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// 分类一个导入名。
pub fn classify(name: &str) -> Result<ImportKind, WasmError> {
    if let Some(import) = wasi_preview1_import(name)? {
        return Ok(ImportKind::WasiPreview1(import));
    }
    if name.contains('#') {
        let (direction, package) = component_direction(name)?;
        return Ok(ImportKind::ComponentModel { direction, package: package.to_owned() });
    }
    match name.split_once('.') {
        Some((module, field)) if !module.is_empty() && !field.is_empty() => {
            Ok(ImportKind::Host { module: module.to_owned(), field: field.to_owned() })
        }
        _ => Err(WasmError::UnsupportedImport { name: name.to_owned() }),
    }
}
