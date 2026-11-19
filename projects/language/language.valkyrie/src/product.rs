//! 面向宿主的 Valkyrie 结构化产品 API。
//!
//! 这些类型只描述规划、编译、测试与 Asgard 静态交付的选项和结果，
//! 不依赖任何宿主绑定。调用方（Node-API、CLI、harness）负责参数搬运。

use std::{path::PathBuf, process::ExitCode, thread};

use crate::legion::{
    CanonicalTarget, CompileOptions, LegionError, PlanRequest, cmds::test::TestArgs, compile_project, plan, resolve_platform_only_target,
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

/// 测试结果。
#[derive(Debug, Clone)]
pub struct TestOutcome {
    /// 是否全部通过。
    pub ok: bool,
    /// 失败说明。
    pub error: Option<String>,
    /// 通过数。
    pub passed: u32,
    /// 失败数。
    pub failed: u32,
    /// 跳过数。
    pub skipped: u32,
}

/// Asgard 静态构建入参。
#[derive(Debug, Clone)]
pub struct StaticBuildOptions {
    /// 项目根目录。
    pub project_dir: PathBuf,
    /// 入口根组件文件路径（相对 `project_dir`）。
    pub entry: PathBuf,
    /// 输出目录；缺省由 Asgard 决定。
    pub output_dir: Option<PathBuf>,
    /// 页面标题；缺省为根组件名。
    pub title: Option<String>,
    /// 产物基址（目录 URL）；缺省 `/`。
    pub base: Option<String>,
    /// 显式资源目录（相对 `project_dir`）。
    pub assets_dir: Option<PathBuf>,
}

/// Asgard 静态构建结果。
#[derive(Debug, Clone)]
pub struct StaticBuildOutcome {
    /// 是否成功。
    pub ok: bool,
    /// 失败时的错误描述。
    pub error: Option<String>,
    /// 实际写出产物的目录。
    pub output_dir: Option<String>,
    /// 产物 `index.html` 的完整路径。
    pub page_path: Option<String>,
    /// 根组件名。
    pub component_name: Option<String>,
}

/// Asgard 托管入参。
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// 产物根目录。
    pub root: PathBuf,
    /// 部署前缀（目录 URL）；缺省 `/`。
    pub base: Option<String>,
    /// 监听主机；缺省 `127.0.0.1`。
    pub host: Option<String>,
    /// 监听端口；缺省 `3000`，`0` 表示系统分配。
    pub port: Option<u16>,
}

/// Asgard 托管结果；成功时服务线程已启动，本调用阻塞到进程退出。
#[derive(Debug, Clone)]
pub struct ServeOutcome {
    /// 是否成功启动。
    pub ok: bool,
    /// 失败时的错误描述。
    pub error: Option<String>,
    /// 实际监听地址（`host:port`）。
    pub address: Option<String>,
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
                let message = report.diagnostics.first().map(|diagnostic| diagnostic.message.clone()).unwrap_or_else(|| "compile failed".into());
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

/// 运行 `legion test`。
#[must_use]
pub fn test_project(project_dir: PathBuf, target: Option<String>) -> TestOutcome {
    let target_label = target.unwrap_or_else(|| "wasm-node".into());
    let parsed_target = match resolve_platform_only_target(&target_label) {
        Ok(target) => target.platform.to_string(),
        Err(error) => return TestOutcome::failed(error.to_string()),
    };
    let worker = thread::Builder::new()
        .name("legion-test".into())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || {
            crate::legion::cmds::test::run(&TestArgs {
                project_dir,
                filter: None,
                target: Some(parsed_target),
                runner: Vec::new(),
                verbose: false,
                standalone: false,
            })
        })
        .expect("failed to spawn legion test thread");
    match worker.join().expect("legion test thread panicked") {
        Ok(code) => {
            let success = code == ExitCode::SUCCESS;
            TestOutcome {
                ok: success,
                error: if success { None } else { Some("legion test reported failures".into()) },
                passed: if success { 1 } else { 0 },
                failed: if success { 0 } else { 1 },
                skipped: 0,
            }
        }
        Err(error) => TestOutcome::failed(error.to_string()),
    }
}

/// 构建 Asgard 静态站点并返回结构化结果。
#[must_use]
pub fn build_static_site(options: StaticBuildOptions) -> StaticBuildOutcome {
    let worker = thread::Builder::new()
        .name("asgard-static-build".into())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || {
            crate::asgard::cli::require_project_admission(&options.project_dir)?;
            crate::asgard::static_site::build_static_site(&crate::asgard::static_site::StaticSiteOptions {
                project_dir: options.project_dir,
                entry: options.entry,
                output_dir: options.output_dir,
                title: options.title,
                base: options.base.unwrap_or_else(|| "/".to_string()),
                assets_dir: options.assets_dir,
            })
        })
        .expect("failed to spawn asgard static build thread");
    match worker.join().expect("asgard static build thread panicked") {
        Ok(report) => StaticBuildOutcome {
            ok: true,
            error: None,
            output_dir: Some(report.output_dir.display().to_string()),
            page_path: Some(report.page_path.display().to_string()),
            component_name: Some(report.component_name),
        },
        Err(error) => StaticBuildOutcome { ok: false, error: Some(error.to_string()), output_dir: None, page_path: None, component_name: None },
    }
}

/// 托管 Asgard 静态产物；本调用阻塞直到服务线程结束。
#[must_use]
pub fn serve_static(options: ServeOptions) -> ServeOutcome {
    let serve_options = crate::asgard::serve::ServeOptions {
        root: options.root,
        base: options.base.unwrap_or_else(|| "/".to_string()),
        host: options.host.unwrap_or_else(|| "127.0.0.1".to_string()),
        port: options.port.unwrap_or(3000),
    };
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let handle = match crate::asgard::serve::serve_static(&serve_options, move |address| {
        let _ = ready_sender.send(address);
    }) {
        Ok(handle) => handle,
        Err(error) => return ServeOutcome { ok: false, error: Some(error.to_string()), address: None },
    };
    let address = ready_receiver.recv().map(|address| address.to_string()).unwrap_or_default();
    let _ = handle.thread.join();
    ServeOutcome { ok: true, error: None, address: Some(address) }
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
        Self {
            ok: false,
            error: Some(error),
            project: None,
            target: None,
            output_dir: None,
            artifacts: Vec::new(),
            entry_symbol: None,
        }
    }
}

impl TestOutcome {
    fn failed(error: String) -> Self {
        Self { ok: false, error: Some(error), passed: 0, failed: 1, skipped: 0 }
    }
}
