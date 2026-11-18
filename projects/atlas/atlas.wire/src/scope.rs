//! 生命周期分类。

/// provider 的生命周期。
///
/// 首版只支持三类；异步初始化、lazy 注入、热替换都不在此列。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// 每个环境一份已发布结果，不是每进程全局单例。
    Application,
    /// 每个请求作用域一份；作用域显式关联应用环境。
    Request,
    /// 每次解析构造一份，生命周期交给返回值的所有权。
    Transient,
}

impl Scope {
    /// 稳定的诊断标签。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Request => "request",
            Self::Transient => "transient",
        }
    }

    /// 是否是缓存作用域（应用级或请求级）。
    pub const fn is_cached(self) -> bool {
        !matches!(self, Self::Transient)
    }
}
