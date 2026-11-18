//! 依赖描述。

use crate::key::TypeKey;

/// 一条直接依赖：键、是否可选、字段诊断标签。
///
/// 组件与普通 provider 使用同一种描述；`freeze` 与运行时都只按这份描述判定，不读取工厂
/// 函数体，也不接受闭包临时读取未声明的键。
#[derive(Clone, Copy, Debug)]
pub struct Dependency {
    key: TypeKey,
    optional: bool,
    label: &'static str,
}

impl Dependency {
    /// 从已有的键、可选标记与诊断标签构造。
    pub const fn new(key: TypeKey, optional: bool, label: &'static str) -> Self {
        Self { key, optional, label }
    }

    /// 以精确类型 `T` 构造依赖。
    pub fn of<T: 'static>(label: &'static str, optional: bool) -> Self {
        Self::new(TypeKey::of::<T>(), optional, label)
    }

    /// 以精确类型 `T` 与角色 marker `Q` 构造依赖。
    pub fn of_qualified<T: 'static, Q: 'static>(label: &'static str, optional: bool) -> Self {
        Self::new(TypeKey::qualified::<T, Q>(), optional, label)
    }

    /// 依赖键。
    pub fn key(&self) -> TypeKey {
        self.key
    }

    /// 是否可选。可选只把「未注册」映射为 `None`，构造失败与作用域违规仍然报错。
    pub fn optional(&self) -> bool {
        self.optional
    }

    /// 字段或参数的诊断标签。
    pub fn label(&self) -> &'static str {
        self.label
    }
}
