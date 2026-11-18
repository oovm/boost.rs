//! 注入失败的分层错误。

use std::{error::Error, fmt, sync::Arc};

/// 注入失败的分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WireErrorKind {
    /// 依赖键不在冻结图中。
    NotRegistered,
    /// 同一个键被重复注册，或同一请求槽位被重复提供。
    DuplicateKey,
    /// 解析发生在不允许的作用域，例如应用级依赖触达请求级键。
    ScopeViolation,
    /// 依赖图或运行时解析出现环。
    Cycle,
    /// provider 或组件读取了未声明的依赖。
    UndeclaredDependency,
    /// provider 工厂失败，或初始化被 panic 打断。
    ProviderFailed,
    /// 请求作用域已经关闭。
    ScopeClosed,
    /// 运行时类型合同不一致。
    TypeContract,
}

impl WireErrorKind {
    /// 稳定的诊断标签，用于日志与断言。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRegistered => "not-registered",
            Self::DuplicateKey => "duplicate-key",
            Self::ScopeViolation => "scope-violation",
            Self::Cycle => "cycle",
            Self::UndeclaredDependency => "undeclared-dependency",
            Self::ProviderFailed => "provider-failed",
            Self::ScopeClosed => "scope-closed",
            Self::TypeContract => "type-contract",
        }
    }
}

#[derive(Clone, Debug)]
struct Repr {
    kind: WireErrorKind,
    message: String,
    path: Vec<&'static str>,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

/// 装配失败。
///
/// 错误可克隆：父 provider 的工厂失败时，所属作用域缓存同一份错误并把它交给所有等待者，
/// 后续解析不再执行工厂。
#[derive(Clone)]
pub struct WireError {
    repr: Arc<Repr>,
}

impl WireError {
    /// 从分类与说明构造错误。
    pub fn new(kind: WireErrorKind, message: impl Into<String>) -> Self {
        Self { repr: Arc::new(Repr { kind, message: message.into(), path: Vec::new(), source: None }) }
    }

    /// 记录依赖路径。已经带有路径的错误保留原路径，避免外层覆盖内层定位。
    pub fn with_path(mut self, path: &[&'static str]) -> Self {
        if !path.is_empty() && self.repr.path.is_empty() {
            let mut repr = (*self.repr).clone();
            repr.path = path.to_vec();
            self.repr = Arc::new(repr);
        }
        self
    }

    /// 追加一段诊断路径。
    pub fn with_step(mut self, step: &'static str) -> Self {
        let mut repr = (*self.repr).clone();
        repr.path.push(step);
        self.repr = Arc::new(repr);
        self
    }

    /// 附上底层错误。提供方负责不把凭证或配置值写进说明。
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        let mut repr = (*self.repr).clone();
        repr.source = Some(Arc::new(source));
        self.repr = Arc::new(repr);
        self
    }

    /// 错误分类。
    pub fn kind(&self) -> WireErrorKind {
        self.repr.kind
    }

    /// 依赖路径，从根组件到失败位置。
    pub fn path(&self) -> &[&'static str] {
        &self.repr.path
    }

    /// 人类可读的说明。
    pub fn message(&self) -> &str {
        &self.repr.message
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.repr.kind.as_str(), self.repr.message)?;
        if !self.repr.path.is_empty() {
            write!(formatter, "（依赖路径：{}）", self.repr.path.join(" -> "))?;
        }
        if let Some(source) = &self.repr.source {
            write!(formatter, "：{source}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireError")
            .field("kind", &self.repr.kind)
            .field("message", &self.repr.message)
            .field("path", &self.repr.path)
            .finish_non_exhaustive()
    }
}

impl Error for WireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.repr.source.as_ref().map(|source| &**source as &(dyn Error + 'static))
    }
}
