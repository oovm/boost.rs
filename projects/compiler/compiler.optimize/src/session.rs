//! 优化会话与目标投影计划：把 `E-Graph` 机制组装成可消费的优化请求/结果。
//!
//! 语言提交程序边界、可用能力与等价理论；本模块只负责饱和、代表元抽取与投影计划记录。

use crate::{
    CapabilityTag, ConstantInterpreter, EGraphHost, Identifier, QualifiedName, RewriteEquation, RewritePhase, Term, TermRewrite,
    builtin_core_term_rewrites, filter_equations, filter_term_rewrites,
};
use compiler_types::ReferenceManagement;

/// `Object Algebraic` 结构化项即中立底座的 `E-Graph` 项。
pub type AlgebraicTerm = Term;

/// 单个 `Object Algebraic` 语义维度的清单。
///
/// 这里记录的是“这个程序对外暴露了哪些语义视图”，
/// 而不是重新发明一个闭合节点池。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectAlgebraicDimension {
    /// 维度名称。
    pub name: Identifier,
    /// 该维度对外提供的语义操作或解释入口。
    pub exported_operations: Vec<QualifiedName>,
    /// 激活该维度所需的能力标签。
    pub required_capabilities: Vec<CapabilityTag>,
    /// 当前维度内操作共同呈现出的引用对象管理提示。
    pub reference_management_hint: Option<ReferenceManagement>,
}

/// `Object Algebraic` 程序边界。
///
/// 当前阶段只保留组合边界和维度清单，
/// 明确拒绝把 `Object Algebraic` 简化为统一节点枚举。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectAlgebraicProgram {
    /// 模块名。
    pub module_name: QualifiedName,
    /// 稳定导出符号。
    pub exports: Vec<QualifiedName>,
    /// 已注册的语义维度。
    pub dimensions: Vec<ObjectAlgebraicDimension>,
    /// 可直接进入 `E-Graph` 的结构化项。
    pub structured_terms: Vec<AlgebraicTerm>,
}

impl ObjectAlgebraicProgram {
    /// 注册一个语义维度。
    pub fn register_dimension(&mut self, dimension: ObjectAlgebraicDimension) {
        self.dimensions.push(dimension);
    }
}

/// `Object Algebraic` builder 的最小公共接口。
pub trait ObjectAlgebraicBuilder {
    /// 返回逻辑模块名。
    fn module_name(&self) -> &QualifiedName;

    /// 返回稳定导出符号。
    fn exports(&self) -> &[QualifiedName];

    /// 返回已声明的语义维度。
    fn dimensions(&self) -> &[ObjectAlgebraicDimension];
}

impl ObjectAlgebraicBuilder for ObjectAlgebraicProgram {
    fn module_name(&self) -> &QualifiedName {
        &self.module_name
    }

    fn exports(&self) -> &[QualifiedName] {
        &self.exports
    }

    fn dimensions(&self) -> &[ObjectAlgebraicDimension] {
        &self.dimensions
    }
}

/// `Object Algebraic` 解释器边界。
pub trait ObjectAlgebraicInterpreter {
    /// 返回解释器名。
    fn interpreter_name(&self) -> &Identifier;

    /// 返回解释器支持的能力。
    fn supported_capabilities(&self) -> &[CapabilityTag];
}

/// 单条等价重写规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteRule {
    /// 规则名。
    pub name: Identifier,
    /// 所属阶段。
    pub phase: RewritePhase,
    /// 触发规则所需能力。
    pub required_capabilities: Vec<CapabilityTag>,
    /// 允许投影到的目标家族。
    pub allowed_projection_families: Vec<FutamuraProjectionFamily>,
}

/// 一组重写规则。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RewriteTheory {
    /// 已注册规则。
    pub rules: Vec<RewriteRule>,
    /// 已注册等式。
    pub equations: Vec<RewriteEquation>,
    /// 结构化项级重写。
    pub term_rewrites: Vec<TermRewrite>,
}

impl RewriteTheory {
    /// 注册一条重写规则。
    pub fn register(&mut self, rule: RewriteRule) {
        self.rules.push(rule);
    }

    /// 注册一条等式。
    pub fn equate(&mut self, equation: RewriteEquation) {
        self.equations.push(equation);
    }

    /// 统计某阶段的规则数量。
    pub fn count_in_phase(&self, phase: RewritePhase) -> usize {
        self.rules.iter().filter(|rule| rule.phase == phase).count()
            + self.equations.iter().filter(|equation| equation.phase == phase).count()
            + self.term_rewrites.iter().filter(|rewrite| rewrite.phase == phase).count()
    }

    /// 注册一条结构化项级重写。
    pub fn register_term_rewrite(&mut self, rewrite: TermRewrite) {
        self.term_rewrites.push(rewrite);
    }
}

/// 供片段提交携带的理论 bundle。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TheoryBundle {
    /// 共享理论。
    pub shared: RewriteTheory,
    /// 当前片段私有理论。
    pub fragment: RewriteTheory,
}

impl TheoryBundle {
    /// 合并共享理论与片段私有理论。
    pub fn merged(&self) -> RewriteTheory {
        let mut merged = self.shared.clone();
        for rule in &self.fragment.rules {
            if !merged.rules.contains(rule) {
                merged.rules.push(rule.clone());
            }
        }
        for rewrite in &self.shared.term_rewrites {
            if !merged.term_rewrites.iter().any(|existing| existing.name == rewrite.name) {
                merged.term_rewrites.push(rewrite.clone());
            }
        }
        for equation in &self.fragment.equations {
            if !merged.equations.contains(equation) {
                merged.equations.push(equation.clone());
            }
        }
        for rewrite in &self.fragment.term_rewrites {
            if !merged.term_rewrites.iter().any(|existing| existing.name == rewrite.name) {
                merged.term_rewrites.push(rewrite.clone());
            }
        }
        merged
    }
}

/// `E-Graph` 会话快照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EGraphSnapshot {
    /// 等价类数量。
    pub equivalence_class_count: usize,
    /// 维度操作数量。
    pub operation_count: usize,
    /// 被真正吸收进 `E-Graph` 的等式数量。
    pub applied_equation_count: usize,
    /// 被应用的模式重写数量。
    pub applied_rewrite_count: usize,
    /// 常量折叠次数。
    pub constant_fold_count: usize,
    /// 饱和迭代轮次。
    pub iteration_count: usize,
    /// 被管理的 `E-Node` 数量。
    pub enode_count: usize,
    /// 是否达到当前理论下的饱和。
    pub saturated: bool,
}

/// `Futamura projection` 目标家族。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FutamuraProjectionFamily {
    /// `futa_clr`
    Clr,
    /// `futa_jvm`
    Jvm,
    /// `futa_wasm`
    Wasm,
    /// `futa_machine_code`
    MachineCode,
    /// `futa_vm`
    Ovm,
    /// `futa_gpu`
    Gpu,
}

impl FutamuraProjectionFamily {
    /// 返回稳定家族名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clr => "futa_clr",
            Self::Jvm => "futa_jvm",
            Self::Wasm => "futa_wasm",
            Self::MachineCode => "futa_machine_code",
            Self::Ovm => "futa_vm",
            Self::Gpu => "futa_gpu",
        }
    }
}

/// 目标宿主边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HostProjectionBoundary {
    /// `.NET` 宿主边界。
    Clr,
    /// `JVM` 宿主边界。
    Jvm,
    /// `WASM + JS glue` 宿主边界。
    WasmJsGlue,
    /// `WASI component model` 宿主边界。
    WasiComponent,
    /// 机器码宿主边界：OS 直接装载 `PE` / `ELF`，宿主包装由产物合同决定。
    MachineCode,
    /// 字节码虚拟机宿主边界。
    Vm,
}

/// 入口返回值在宿主上的可观测投影（结果通道 + 值宽 + 解码规则）。
///
/// 该事实由**实际生成入口包装的目标后端**逐条冻结进运行合同（`DriverRunContract`），
/// Legion 与运行器只消费合同里的投影，不得按 target 名字或宿主边界另行推导：
/// 同一宿主边界上不同入口包装完全可能选择不同通道（例如 JVM launcher 当前打印
/// stdout 只是该包装的选择，不能推广为 JVM 所有入口的结果通道）。
/// 合同缺失该事实时 fail-closed，不得假定默认通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryReturnProjection {
    /// 入口包装把 i32 返回值以十进制行打印到 stdout，自身以 0 退出。
    StdoutInteger,
    /// 入口返回值就是进程退出码（按宿主约定截断到低 8 位）。
    ProcessExitCode,
    /// 宿主在进程内调用入口，返回值是 VM 内的 i32 值。
    InProcessValue,
    /// 入口没有返回值可观测。
    NoReturnValue,
}

/// `Futamura projection` 选择策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionPolicy {
    /// 目标投影家族。
    pub family: FutamuraProjectionFamily,
    /// 宿主边界。
    pub host_boundary: HostProjectionBoundary,
    /// 引用对象管理策略。
    pub reference_management: ReferenceManagement,
    /// 是否优先缩小产物体积。
    pub prefer_small_artifacts: bool,
    /// 是否保留显式 effect 边界。
    pub preserve_effect_boundaries: bool,
}

/// 单次目标投影计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionPlan {
    /// 选中的目标家族。
    pub family: FutamuraProjectionFamily,
    /// 选中的宿主边界。
    pub host_boundary: HostProjectionBoundary,
    /// 选中的引用对象管理策略。
    pub reference_management: ReferenceManagement,
    /// 投影后仍需要保留的能力。
    pub preserved_capabilities: Vec<CapabilityTag>,
}

/// 一次优化请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationRequest {
    /// 输入的 `Object Algebraic` 程序。
    pub program: ObjectAlgebraicProgram,
    /// 当前可用能力。
    pub capabilities: Vec<CapabilityTag>,
    /// 本轮使用的等价理论。
    pub rewrite_theory: RewriteTheory,
    /// 目标投影策略。
    pub projection_policy: ProjectionPolicy,
}

/// 优化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationResult {
    /// 选择后的 `Object Algebraic` 程序。
    pub program: ObjectAlgebraicProgram,
    /// 本轮 `E-Graph` 快照。
    pub egraph: EGraphSnapshot,
    /// 目标投影计划。
    pub projection: ProjectionPlan,
    /// 被采用的规则名。
    pub applied_rules: Vec<Identifier>,
}

/// 优化会话。
#[derive(Debug, Default)]
pub struct OptimizationSession;

impl OptimizationSession {
    /// 委托中立底座执行 `E-Graph` 饱和、常量折叠与代表元抽取。
    pub fn optimize(&self, request: OptimizationRequest) -> OptimizationResult {
        let theory = merge_with_builtin_core(request.rewrite_theory);
        let applied_rules = theory
            .rules
            .iter()
            .filter(|rule| {
                rule.allowed_projection_families.is_empty() || rule.allowed_projection_families.contains(&request.projection_policy.family)
            })
            .filter(|rule| {
                rule.required_capabilities.iter().all(|capability| request.capabilities.iter().any(|provided| provided == capability))
            })
            .map(|rule| rule.name.clone())
            .collect::<Vec<_>>();

        let mut egraph = EGraphHost::default();
        for export in &request.program.exports {
            egraph.add_flat_symbol(export.clone());
        }
        for dimension in &request.program.dimensions {
            for operation in &dimension.exported_operations {
                egraph.add_flat_symbol(operation.clone());
            }
        }
        let structured_roots = request.program.structured_terms.iter().map(|term| egraph.add_term(term)).collect::<Vec<_>>();

        let folding: &dyn ConstantInterpreter = &crate::CoreConstantInterpreter;
        let phases = [RewritePhase::Normalize, RewritePhase::Saturate, RewritePhase::PreProjection];
        let mut stats = egraph.stats().clone();
        for phase in phases {
            let rewrites = filter_term_rewrites(&theory.term_rewrites, phase, &request.capabilities);
            let equations = filter_equations(&theory.equations, phase, &request.capabilities);
            stats = egraph.saturate(&rewrites, &equations, Some(folding));
        }

        let mut program = request.program;
        program.exports = canonicalize_flat(&egraph, &program.exports);
        for dimension in &mut program.dimensions {
            dimension.exported_operations = canonicalize_flat(&egraph, &dimension.exported_operations);
        }
        program.structured_terms = structured_roots.iter().map(|root| egraph.extract_term(*root)).collect();

        let operation_count = program.dimensions.iter().map(|dimension| dimension.exported_operations.len()).sum();
        let preserved_capabilities = request
            .capabilities
            .iter()
            .filter(|capability| request.projection_policy.preserve_effect_boundaries || capability.as_str() != "suspend")
            .cloned()
            .collect();

        OptimizationResult {
            program,
            egraph: EGraphSnapshot {
                equivalence_class_count: stats.equivalence_class_count,
                operation_count,
                applied_equation_count: stats.applied_equation_count,
                applied_rewrite_count: stats.applied_rewrite_count,
                constant_fold_count: stats.constant_fold_count,
                iteration_count: stats.iteration_count,
                enode_count: stats.enode_count,
                saturated: stats.saturated,
            },
            projection: ProjectionPlan {
                family: request.projection_policy.family,
                host_boundary: request.projection_policy.host_boundary,
                reference_management: request.projection_policy.reference_management,
                preserved_capabilities,
            },
            applied_rules,
        }
    }
}

fn merge_with_builtin_core(mut theory: RewriteTheory) -> RewriteTheory {
    for rewrite in builtin_core_term_rewrites() {
        if !theory.term_rewrites.iter().any(|existing| existing.name == rewrite.name) {
            theory.term_rewrites.push(rewrite);
        }
    }
    theory
}

fn term_to_flat_name(term: &Term) -> QualifiedName {
    match term {
        Term::Literal(value) => crate::core_lit_name(*value),
        Term::Symbol(name) => name.clone(),
        Term::Apply { operator, .. } => operator.clone(),
    }
}

fn extract_flat(host: &EGraphHost, name: &QualifiedName) -> QualifiedName {
    let Some(class) = host.class_of_symbol(name)
    else {
        return name.clone();
    };
    term_to_flat_name(&host.extract_term(class))
}

fn canonicalize_flat(host: &EGraphHost, operations: &[QualifiedName]) -> Vec<QualifiedName> {
    let mut canonical = Vec::new();
    for operation in operations {
        let representative = extract_flat(host, operation);
        if !canonical.contains(&representative) {
            canonical.push(representative);
        }
    }
    canonical
}
