//! API 请求类型（无 argv）。

use crate::legion::ResolvedTarget;
use std::path::PathBuf;

/// `plan()` 输入。
#[derive(Debug, Clone)]
pub struct PlanRequest {
    /// 项目目录、包根或单脚本路径。
    pub project_dir: PathBuf,
    /// 已冻结的解析目标：平台身份 + 产物/宿主合同。
    ///
    /// `native` / `node` / `python` 占位符必须先经统一目标解析服务展开；
    /// 后续阶段只消费这一份事实，不再按作者面拼写二次分派。
    pub resolved: ResolvedTarget,
    /// 可选产物目录（默认由 planner 决定）。
    pub output_dir: Option<PathBuf>,
}

/// `compile()` 选项。
#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    /// 是否打印编译进度类日志（CLI 用）。
    pub verbose: bool,
    /// 是否写出 host 选择调试副产物。
    pub debug_artifacts: bool,
}
