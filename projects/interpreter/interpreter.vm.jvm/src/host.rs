//! native 方法宿主边界。
//!
//! JVM 的 native 方法由宿主注册后由解释器调用；本模块只拥有注册表与调用协议，
//! 不实现任何具体 native 库，也不定义产品或平台函数名。

use std::collections::BTreeMap;

use crate::error::JvmError;

/// native 方法的宿主实现：接收参数槽并按返回宽度写回。
///
/// 返回 `Some(value)` 表示该 native 返回一个单槽整数，`None` 表示 `void`。
pub type NativeBinding = fn(&[i32]) -> Option<i32>;

/// native 方法注册表。
#[derive(Debug, Default, Clone)]
pub struct NativeRegistry {
    bindings: BTreeMap<String, NativeBinding>,
}

impl NativeRegistry {
    /// 创建一个空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 native 方法；同名重复注册覆盖旧实现。
    pub fn register(&mut self, name: impl Into<String>, binding: NativeBinding) {
        self.bindings.insert(name.into(), binding);
    }

    /// 查找 native 方法。
    pub fn get(&self, name: &str) -> Result<NativeBinding, JvmError> {
        self.bindings.get(name).copied().ok_or_else(|| JvmError::UnboundNative { name: name.to_owned() })
    }

    /// 已注册方法数。
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// 是否为空注册表。
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}
