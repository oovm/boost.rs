//! 依赖键。

use std::{any::TypeId, fmt};

/// 依赖键：**完整字段类型**加可选的角色限定符。
///
/// 默认键只由类型身份决定：`Arc<ModelClient>` 的键就是 `Arc<ModelClient>`，不会降级查找
/// `ModelClient` 再自动包装。类型别名按 Rust 的实际类型身份处理，类型名字符串只用于诊断。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeKey {
    value: TypeId,
    qualifier: Option<TypeId>,
}

impl TypeKey {
    /// 以精确类型 `T` 构造键。
    pub fn of<T: 'static>() -> Self {
        Self { value: TypeId::of::<T>(), qualifier: None }
    }

    /// 以精确类型 `T` 与角色 marker `Q` 构造键。
    pub fn qualified<T: 'static, Q: 'static>() -> Self {
        Self { value: TypeId::of::<T>(), qualifier: Some(TypeId::of::<Q>()) }
    }

    /// 运行时拼装：值类型身份加可选限定符身份。
    pub fn from_parts(value: TypeId, qualifier: Option<TypeId>) -> Self {
        Self { value, qualifier }
    }

    /// 值类型身份。
    pub fn value_id(self) -> TypeId {
        self.value
    }

    /// 角色限定符身份；未限定时为 `None`。
    pub fn qualifier_id(self) -> Option<TypeId> {
        self.qualifier
    }

    /// 是否带角色限定符。
    pub fn is_qualified(self) -> bool {
        self.qualifier.is_some()
    }
}

impl fmt::Debug for TypeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.qualifier {
            Some(qualifier) => write!(formatter, "TypeKey({:?}, {:?})", self.value, qualifier),
            None => write!(formatter, "TypeKey({:?})", self.value),
        }
    }
}
