//! 面向宿主的 Valkyrie 结构化产品 API。
//!
//! 这些类型只描述规划与编译执行的选项和结果，
//! 不依赖任何宿主绑定。复合语言的测试与 Asgard 静态交付入口在 `package.manager.asgard::product`。

use std::{path::PathBuf, thread};

use crate::legion::{
    CanonicalTarget, CompileOptions, LegionError, PlanRequest, compile_project, plan, resolve_platform_only_target,
};

/// 栈大小为产品编译线程预留的深度，避免深递归 lowering 打穿默认线程栈。
const WORKER_STACK_SIZE: usize = 64 * 1024 * 1024;

/// 单个语义源码组摘要。
#[derive(Debug, Clone)]
pub struct SemanticGroupItem {
    /// 语义源码组对应的依赖键。
    pub dependency_key: String,
    /// 该组包含的源码文件数。
    pub file_count: u32,
}

/// 规划入参。
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// 项目目录。
    pub project_dir: PathBuf,
    /// 编译目标平台字符串，缺省为 `clr`。
    pub target: Option<String>,
}

/// 规划结果。
#[derive(Debug, Clone)]
pub struct PlanOutcome {
    /// 是否成功。
    pub ok: bool,
    /// 失败时的错误描述。
    pub error: Option<String>,
    /// 项目名。
    pub project: Option<String>,
    /// 编译目标平台字符串。
    pub target: Option<String>,
    /// workspace 根目录路径。
    pub workspace_root: Option<String>,
    /// 产物输出目录路径。
    pub output_dir: Option<String>,
    /// 参与构建的源码文件数。
    pub source_count: u32,
    /// 语义源码组数量。
    pub semantic_group_count: u32,
    /// 直接依赖数量。
    pub dependency_count: u32,
    /// 各语义源码组摘要。
    pub groups: Vec<SemanticGroupItem>,
}

/// 构建入参。
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// 项目目录。
    pub project_dir: PathBuf,
    /// 编译目标平台字符串，缺省为 `clr`。
    pub target: Option<String>,
    /// 产物输出目录，缺省由规划决定。
    pub output_dir: Option<PathBuf>,
}

/// 构建结果。
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    /// 是否成功。
    pub ok: bool,
    /// 失败时的错误描述。
    pub error: Option<String>,
    /// 项目名。
    pub project: Option<String>,
    /// 编译目标平台字符串。
    pub target: Option<String>,
    /// 产物输出目录路径。
    pub output_dir: Option<String>,
    /// 落盘产物路径列表。
    pub artifacts: Vec<String>,
    /// 产物入口符号名（可能为空）。
    pub entry_symbol: Option<String>,
}

/// 解析 workspace 并输出 build plan（不进入编译）。
#[must_use]
pub fn plan_project(options: &PlanOptions) -> PlanOutcome {
    let target = match parse_target(options.target.as_deref()) {
        Ok(target) => target,
        Err(error) => return PlanOutcome::failed(error),
    };
    match plan(PlanRequest { project_dir: options.project_dir.clone(), target, output_dir: None }) {
        Ok(build_plan) => {
            let compilable = build_plan.is_compilable();
            PlanOutcome {
                ok: compilable,
                error: if compilable { None } else { Some("no source files or plan diagnostics contain errors".into()) },
                project: Some(build_plan.project),
                target: Some(build_plan.target),
                workspace_root: Some(build_plan.workspace_root),
                output_dir: Some(build_plan.output_dir),
                source_count: build_plan.source_count.min(u32::MAX as usize) as u32,
                semantic_group_count: build_plan.semantic_group_count.min(u32::MAX as usize) as u32,
                dependency_count: build_plan.dependency_count.min(u32::MAX as usize) as u32,
                groups: build_plan
                    .groups
                    .into_iter()
                    .map(|group| SemanticGroupItem {
                        dependency_key: group.dependency_key,
                        file_count: group.file_count.min(u32::MAX as usize) as u32,
                    })
                    .collect(),
            }
        }
        Err(report) => PlanOutcome::failed(format!("{report:?}")),
    }
}

/// 编译项目并落盘产物。
#[must_use]
pub fn build_project(options: &BuildOptions) -> BuildOutcome {
    let target = match parse_target(options.target.as_deref()) {
        Ok(target) => target,
        Err(error) => return BuildOutcome::failed(error),
    };
    let project_dir = options.project_dir.clone();
    let output_dir = options.output_dir.clone();
    let worker = thread::Builder::new()
        .name("legion-build".into())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || compile_project(project_dir, target, output_dir, CompileOptions::default()))
        .expect("failed to spawn legion build thread");
    match worker.join().expect("legion build thread panicked") {
        Ok(report) => {
            if !report.success {
                let message =
                    report.diagnostics.first().map(|diagnostic| diagnostic.message.clone()).unwrap_or_else(|| "compile failed".into());
                return BuildOutcome {
                    ok: false,
                    error: Some(message),
                    project: Some(report.project),
                    target: Some(report.target),
                    output_dir: Some(report.output_dir),
                    artifacts: report.artifacts,
                    entry_symbol: report.entry_symbol,
                };
            }
            BuildOutcome {
                ok: true,
                error: None,
                project: Some(report.project),
                target: Some(report.target),
                output_dir: Some(report.output_dir),
                artifacts: report.artifacts,
                entry_symbol: report.entry_symbol,
            }
        }
        Err(report) => BuildOutcome::failed(format!("{report:?}")),
    }
}

fn parse_target(target: Option<&str>) -> Result<CanonicalTarget, String> {
    let raw = target.unwrap_or("clr");
    resolve_platform_only_target(raw).map(|resolved| resolved.platform).map_err(|error| target_error_message(&error))
}

fn target_error_message(error: &LegionError) -> String {
    error.to_string()
}

impl PlanOutcome {
    fn failed(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
            project: None,
            target: None,
            workspace_root: None,
            output_dir: None,
            source_count: 0,
            semantic_group_count: 0,
            dependency_count: 0,
            groups: Vec::new(),
        }
    }
}

impl BuildOutcome {
    fn failed(error: String) -> Self {
        Self { ok: false, error: Some(error), project: None, target: None, output_dir: None, artifacts: Vec::new(), entry_symbol: None }
    }
}
