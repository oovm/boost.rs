//! 结构化诊断 wire — 语言中立，不含自然语言文案。
//!
//! 稳定身份：`code + severity + args + details + span + path`。
//! 本地化由产品 catalog + `@vmz/diagnostic` 完成，不在本 crate。

use std::{collections::BTreeMap, fmt};

use crate::ids::SourceSpan;

/// 严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 硬失败。
    Error,
    /// 警告。
    Warning,
}

/// Catalog / details 中的机器可读标量（非用户文案）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticValue {
    /// 布尔。
    Bool(bool),
    /// 有符号整数。
    Int(i64),
    /// 无符号整数。
    UInt(u64),
    /// 机器标识符或已解码字面量（禁止整句自然语言）。
    Text(String),
}

impl From<bool> for DiagnosticValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for DiagnosticValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<u64> for DiagnosticValue {
    fn from(v: u64) -> Self {
        Self::UInt(v)
    }
}

impl From<u32> for DiagnosticValue {
    fn from(v: u32) -> Self {
        Self::UInt(u64::from(v))
    }
}

impl From<i32> for DiagnosticValue {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<String> for DiagnosticValue {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<&str> for DiagnosticValue {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}

impl fmt::Display for DiagnosticValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
        }
    }
}

/// 对象 / 请求路径（机器可读段，非展示句）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticPath {
    /// 路径段（如 `session`、`request`、`modulus`）。
    pub segments: Vec<String>,
}

impl DiagnosticPath {
    /// 由段列表构造。
    pub fn from_segments(segments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { segments: segments.into_iter().map(Into::into).collect() }
    }
}

/// 稳定的 Science 诊断码（wire：`SCIENCE_*`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// 非法数字字面量。
    InvalidNumber,
    /// 域错误。
    DomainError,
    /// 除以零。
    DivideByZero,
    /// 类型不匹配。
    TypeMismatch,
    /// 形状不匹配。
    ShapeMismatch,
    /// 未绑定符号。
    UnboundSymbol,
    /// 未知算子。
    UnknownOperator,
    /// 不支持的操作。
    UnsupportedOperation,
    /// 非法下标。
    InvalidIndex,
    /// 条件位置需要 typed Boolean，却收到非布尔值。
    NonBooleanCondition,
    /// 禁止精度损失时发生精度损失。
    PrecisionLoss,
    /// 赋值错误。
    AssignmentError,
    /// 不收敛。
    NonConvergent,
    /// 数值提升失败。
    PromotionFailed,
    /// 指数越界。
    ExponentOutOfRange,
    /// 当前假设下微积分表达式无定义。
    CalculusUndefined,
    /// 在该点 / 假设下导数不存在。
    DerivativeNotExist,
    /// 极限不存在。
    LimitDoesNotExist,
    /// 极限振荡。
    LimitOscillatory,
    /// 积分发散。
    IntegralDivergent,
    /// 未找到初等原函数。
    IntegralNotElementary,
    /// 积分域非法。
    IntegrationDomainInvalid,
    /// 分支选择歧义。
    BranchAmbiguous,
    /// 所需假设未消解。
    AssumptionUnresolved,
    /// 触及级数阶数 / 余项上限。
    SeriesOrderLimit,
    /// 级数余项未知。
    SeriesRemainderUnknown,
    /// ODE 类别不支持。
    OdeUnsupported,
    /// ODE 解未通过验证。
    OdeSolutionUnverified,
    /// 变换收敛域（ROC）未知。
    TransformRocUnknown,
    /// 数值结果未认证。
    NumericNotCertified,
    /// 微积分资源 / 改写上限。
    CalculusResourceLimit,
    /// 域 / 模数绑定不匹配。
    DomainMismatch,
    /// 因式分解不完整（带结构化 metadata 时仍可返回部分结果）。
    FactorIncomplete,
    /// 素性判定未决。
    PrimeTestInconclusive,
    /// 模数非法（非 `> 1` 等）。
    ModulusInvalid,
    /// 模逆不存在（与模不互素）。
    ModularInverseMissing,
    /// 同余系统无解 / 不一致。
    CongruenceInconsistent,
    /// 多项式在非域上非法除法。
    PolynomialNonFieldDivision,
    /// 多项式除零。
    PolynomialDivisionByZero,
    /// 多项式变量集不匹配。
    PolynomialVariableMismatch,
    /// 单项式序非法或与变量数不一致。
    PolynomialOrderInvalid,
    /// 多项式过大（资源）。
    PolynomialTooLarge,
    /// 单项式指数溢出（`u32` checked 失败）。
    PolynomialDegreeOverflow,
    /// Gröbner 资源上限。
    GroebnerResourceLimit,
    /// Gröbner 基未在资源限制内完成（不可作 exact witness）。
    GroebnerIncomplete,
    /// Gröbner 独立验证失败（S-pair 未约化为零等）。
    GroebnerVerificationFailed,
    /// 群对象不匹配。
    GroupMismatch,
    /// 群元素非法。
    GroupElementInvalid,
    /// 群非有限。
    GroupNotFinite,
    /// 子群不正规。
    GroupNotNormal,
    /// 群阶未知。
    GroupOrderUnknown,
    /// 同构判定未决。
    GroupIsomorphismInconclusive,
    /// 置换非法。
    PermutationInvalid,
    /// 域对象不匹配。
    FieldMismatch,
    /// 域元素非法。
    FieldElementInvalid,
    /// 有限域模多项式可约。
    FieldModulusReducible,
    /// 域扩张非法。
    FieldExtensionInvalid,
    /// 扩张不可分。
    ExtensionNotSeparable,
    /// 扩张不正规。
    ExtensionNotNormal,
    /// 伽罗瓦群不完整。
    GaloisGroupIncomplete,
    /// 伽罗瓦资源上限。
    GaloisResourceLimit,
    /// 自同构非法。
    AutomorphismInvalid,
    /// 固定域不可用。
    FixedFieldUnavailable,
    /// 数值域不匹配。
    NumericDomainMismatch,
    /// 数值 promotion 失败。
    NumericPromotionFailed,
    /// 数值精度损失。
    NumericPrecisionLoss,
    /// 数值后端不可用。
    NumericBackendUnavailable,
    /// 数值转换禁止。
    NumericConversionForbidden,
    /// 数值资源上限（limb / significand / wire 预算耗尽）。
    NumericResourceLimit,
    /// 数值计算被取消。
    NumericCancelled,
    /// 数值除以零。
    NumericDivisionByZero,
    /// 采样域非法（非有限、空区间等）。
    SamplingDomainInvalid,
    /// 采样资源上限（样本数过小/过大）。
    SamplingResourceLimit,
    /// 采样被取消。
    SamplingCancelled,
}

impl DiagnosticCode {
    /// Wire 字符串（`SCIENCE_*`，稳定 identity）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidNumber => "SCIENCE_INVALID_NUMBER",
            Self::DomainError => "SCIENCE_DOMAIN_ERROR",
            Self::DivideByZero => "SCIENCE_DIVIDE_BY_ZERO",
            Self::TypeMismatch => "SCIENCE_TYPE_MISMATCH",
            Self::ShapeMismatch => "SCIENCE_SHAPE_MISMATCH",
            Self::UnboundSymbol => "SCIENCE_UNBOUND_SYMBOL",
            Self::UnknownOperator => "SCIENCE_UNKNOWN_OPERATOR",
            Self::UnsupportedOperation => "SCIENCE_UNSUPPORTED_OPERATION",
            Self::InvalidIndex => "SCIENCE_INVALID_INDEX",
            Self::NonBooleanCondition => "SCIENCE_NON_BOOLEAN_CONDITION",
            Self::PrecisionLoss => "SCIENCE_PRECISION_LOSS",
            Self::AssignmentError => "SCIENCE_ASSIGNMENT_ERROR",
            Self::NonConvergent => "SCIENCE_NON_CONVERGENT",
            Self::PromotionFailed => "SCIENCE_PROMOTION_FAILED",
            Self::ExponentOutOfRange => "SCIENCE_EXPONENT_OUT_OF_RANGE",
            Self::CalculusUndefined => "SCIENCE_CALCULUS_UNDEFINED",
            Self::DerivativeNotExist => "SCIENCE_DERIVATIVE_NOT_EXIST",
            Self::LimitDoesNotExist => "SCIENCE_LIMIT_DOES_NOT_EXIST",
            Self::LimitOscillatory => "SCIENCE_LIMIT_OSCILLATORY",
            Self::IntegralDivergent => "SCIENCE_INTEGRAL_DIVERGENT",
            Self::IntegralNotElementary => "SCIENCE_INTEGRAL_NOT_ELEMENTARY",
            Self::IntegrationDomainInvalid => "SCIENCE_INTEGRATION_DOMAIN_INVALID",
            Self::BranchAmbiguous => "SCIENCE_BRANCH_AMBIGUOUS",
            Self::AssumptionUnresolved => "SCIENCE_ASSUMPTION_UNRESOLVED",
            Self::SeriesOrderLimit => "SCIENCE_SERIES_ORDER_LIMIT",
            Self::SeriesRemainderUnknown => "SCIENCE_SERIES_REMAINDER_UNKNOWN",
            Self::OdeUnsupported => "SCIENCE_ODE_UNSUPPORTED",
            Self::OdeSolutionUnverified => "SCIENCE_ODE_SOLUTION_UNVERIFIED",
            Self::TransformRocUnknown => "SCIENCE_TRANSFORM_ROC_UNKNOWN",
            Self::NumericNotCertified => "SCIENCE_NUMERIC_NOT_CERTIFIED",
            Self::CalculusResourceLimit => "SCIENCE_CALCULUS_RESOURCE_LIMIT",
            Self::DomainMismatch => "SCIENCE_DOMAIN_MISMATCH",
            Self::FactorIncomplete => "SCIENCE_FACTOR_INCOMPLETE",
            Self::PrimeTestInconclusive => "SCIENCE_PRIME_TEST_INCONCLUSIVE",
            Self::ModulusInvalid => "SCIENCE_MODULUS_INVALID",
            Self::ModularInverseMissing => "SCIENCE_MODULAR_INVERSE_MISSING",
            Self::CongruenceInconsistent => "SCIENCE_CONGRUENCE_INCONSISTENT",
            Self::PolynomialNonFieldDivision => "SCIENCE_POLYNOMIAL_NON_FIELD_DIVISION",
            Self::PolynomialDivisionByZero => "SCIENCE_POLYNOMIAL_DIVISION_BY_ZERO",
            Self::PolynomialVariableMismatch => "SCIENCE_POLYNOMIAL_VARIABLE_MISMATCH",
            Self::PolynomialOrderInvalid => "SCIENCE_POLYNOMIAL_ORDER_INVALID",
            Self::PolynomialTooLarge => "SCIENCE_POLYNOMIAL_TOO_LARGE",
            Self::PolynomialDegreeOverflow => "SCIENCE_POLYNOMIAL_DEGREE_OVERFLOW",
            Self::GroebnerResourceLimit => "SCIENCE_GROEBNER_RESOURCE_LIMIT",
            Self::GroebnerIncomplete => "SCIENCE_GROEBNER_INCOMPLETE",
            Self::GroebnerVerificationFailed => "SCIENCE_GROEBNER_VERIFICATION_FAILED",
            Self::GroupMismatch => "SCIENCE_GROUP_MISMATCH",
            Self::GroupElementInvalid => "SCIENCE_GROUP_ELEMENT_INVALID",
            Self::GroupNotFinite => "SCIENCE_GROUP_NOT_FINITE",
            Self::GroupNotNormal => "SCIENCE_GROUP_NOT_NORMAL",
            Self::GroupOrderUnknown => "SCIENCE_GROUP_ORDER_UNKNOWN",
            Self::GroupIsomorphismInconclusive => "SCIENCE_GROUP_ISOMORPHISM_INCONCLUSIVE",
            Self::PermutationInvalid => "SCIENCE_PERMUTATION_INVALID",
            Self::FieldMismatch => "SCIENCE_FIELD_MISMATCH",
            Self::FieldElementInvalid => "SCIENCE_FIELD_ELEMENT_INVALID",
            Self::FieldModulusReducible => "SCIENCE_FIELD_MODULUS_REDUCIBLE",
            Self::FieldExtensionInvalid => "SCIENCE_FIELD_EXTENSION_INVALID",
            Self::ExtensionNotSeparable => "SCIENCE_EXTENSION_NOT_SEPARABLE",
            Self::ExtensionNotNormal => "SCIENCE_EXTENSION_NOT_NORMAL",
            Self::GaloisGroupIncomplete => "SCIENCE_GALOIS_GROUP_INCOMPLETE",
            Self::GaloisResourceLimit => "SCIENCE_GALOIS_RESOURCE_LIMIT",
            Self::AutomorphismInvalid => "SCIENCE_AUTOMORPHISM_INVALID",
            Self::FixedFieldUnavailable => "SCIENCE_FIXED_FIELD_UNAVAILABLE",
            Self::NumericDomainMismatch => "SCIENCE_NUMERIC_DOMAIN_MISMATCH",
            Self::NumericPromotionFailed => "SCIENCE_NUMERIC_PROMOTION_FAILED",
            Self::NumericPrecisionLoss => "SCIENCE_NUMERIC_PRECISION_LOSS",
            Self::NumericBackendUnavailable => "SCIENCE_NUMERIC_BACKEND_UNAVAILABLE",
            Self::NumericConversionForbidden => "SCIENCE_NUMERIC_CONVERSION_FORBIDDEN",
            Self::NumericResourceLimit => "SCIENCE_NUMERIC_RESOURCE_LIMIT",
            Self::NumericCancelled => "SCIENCE_NUMERIC_CANCELLED",
            Self::NumericDivisionByZero => "SCIENCE_NUMERIC_DIVISION_BY_ZERO",
            Self::SamplingDomainInvalid => "SCIENCE_SAMPLING_DOMAIN_INVALID",
            Self::SamplingResourceLimit => "SCIENCE_SAMPLING_RESOURCE_LIMIT",
            Self::SamplingCancelled => "SCIENCE_SAMPLING_CANCELLED",
        }
    }
}

/// 结构化诊断（无自然语言 `message` / `detail: String`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 稳定诊断码。
    pub code: DiagnosticCode,
    /// 严重级别。
    pub severity: Severity,
    /// Catalog 插值参数。
    pub args: BTreeMap<String, DiagnosticValue>,
    /// 机器可读上下文（domain、operation、limits 等）。
    pub details: BTreeMap<String, DiagnosticValue>,
    /// 源码偏移（若有）。
    pub span: Option<SourceSpan>,
    /// 对象 / 请求路径（若有）。
    pub path: Option<DiagnosticPath>,
}

impl Diagnostic {
    /// 错误级诊断（无文案）。
    pub fn new(code: DiagnosticCode) -> Self {
        Self { code, severity: Severity::Error, args: BTreeMap::new(), details: BTreeMap::new(), span: None, path: None }
    }

    /// 警告级诊断。
    pub fn warning(code: DiagnosticCode) -> Self {
        Self { severity: Severity::Warning, ..Self::new(code) }
    }

    /// 追加 catalog 插值参数。
    pub fn arg(mut self, key: impl Into<String>, value: impl Into<DiagnosticValue>) -> Self {
        self.args.insert(key.into(), value.into());
        self
    }

    /// 追加机器可读 detail 字段。
    pub fn detail(mut self, key: impl Into<String>, value: impl Into<DiagnosticValue>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    /// 绑定源码 span。
    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    /// 绑定诊断路径。
    pub fn with_path(mut self, path: DiagnosticPath) -> Self {
        self.path = Some(path);
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 仅供调试/日志：输出 wire code。本地化文案属于产品目录。
        write!(f, "{}", self.code.as_str())?;
        if !self.args.is_empty() {
            write!(f, " args={{")?;
            for (i, (k, v)) in self.args.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{k}={v}")?;
            }
            write!(f, "}}")?;
        }
        if !self.details.is_empty() {
            write!(f, " details={{")?;
            for (i, (k, v)) in self.details.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{k}={v}")?;
            }
            write!(f, "}}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// Result 别名。
pub type Result<T> = std::result::Result<T, Diagnostic>;
