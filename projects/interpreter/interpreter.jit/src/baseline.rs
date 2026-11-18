//! 基线标量 JIT 驱动：格式匹配交给适配器，通用调度与元数据组装在此。

use crate::{
    JitCompileRequest, JitCompiledArtifact, JitCompiler, JitError, baseline_scalar_assumptions, build_baseline_deopt_map,
    build_conservative_stack_maps,
};

/// 格式适配器合同：把一条已验证函数的字节码视图匹配为标量程序 blob。
///
/// 匹配规则属于具体执行格式，由目标 VM 实现并注入；本 crate 只提供通用调度、
/// stack map / deopt 组装与假设挂载，不被任何字节码格式绑死。
pub trait ScalarProgramMatcher {
    /// 匹配成功返回标量程序 blob 字节，形态不支持时返回 `None`。
    fn match_scalar_program(&self, request: &JitCompileRequest) -> Option<Vec<u8>>;
}

/// 识别极简标量模式并附带机器码 blob 的 JIT 后端（无原生可执行页）。
#[derive(Debug)]
pub struct BaselineScalarJit<M: ScalarProgramMatcher> {
    matcher: M,
}

impl<M: ScalarProgramMatcher> BaselineScalarJit<M> {
    /// 以给定格式适配器构造后端。
    pub fn new(matcher: M) -> Self {
        Self { matcher }
    }

    /// 借出内部格式适配器。
    pub fn matcher(&self) -> &M {
        &self.matcher
    }
}

impl<M: ScalarProgramMatcher + Default> Default for BaselineScalarJit<M> {
    fn default() -> Self {
        Self::new(M::default())
    }
}

impl<M: ScalarProgramMatcher> JitCompiler for BaselineScalarJit<M> {
    fn enabled(&self) -> bool {
        true
    }

    fn compile_function(&mut self, request: &JitCompileRequest) -> Result<JitCompiledArtifact, JitError> {
        let _ = request.function_code()?;
        let machine_code = self.matcher.match_scalar_program(request).ok_or(JitError::Unsupported)?;
        let maps = build_conservative_stack_maps(request.function_index, request.function.local_count, &request.function.safepoint_indices);
        let deopt = build_baseline_deopt_map(request.function_index, request.function.local_count, &request.function.safepoint_indices);
        Ok(JitCompiledArtifact::with_machine_code(maps, deopt, machine_code).with_assumptions(baseline_scalar_assumptions(request.module_version)))
    }
}
