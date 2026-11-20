//! 面向宿主的 AWSL + Valkyrie 复合语言结构化产品 API。
//!
//! 这些类型只描述测试与 Asgard 静态交付的选项和结果，不依赖任何宿主绑定。
//! 调用方（Node-API、CLI、harness）负责参数搬运。纯 Valkyrie 规划/编译入口仍由
//! `package.manager.legion::product` 提供。

use std::{path::PathBuf, process::ExitCode, thread};

use package_manager_legion::legion::resolve_platform_only_target;

use crate::{
    cli, serve, static_site,
    cmds::test::TestArgs,
};

/// 栈大小为产品线程预留的深度，避免深递归 lowering 打穿默认线程栈。
const WORKER_STACK_SIZE: usize = 64 * 1024 * 1024;

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

impl TestOutcome {
    fn failed(error: String) -> Self {
        Self { ok: false, error: Some(error), passed: 0, failed: 1, skipped: 0 }
    }
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

/// 运行 `legion test`（复合语言产品入口，含 HTML 报告渲染）。
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
            crate::cmds::test::run(&TestArgs {
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
            cli::require_project_admission(&options.project_dir)?;
            static_site::build_static_site(&static_site::StaticSiteOptions {
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
    let serve_options = serve::ServeOptions {
        root: options.root,
        base: options.base.unwrap_or_else(|| "/".to_string()),
        host: options.host.unwrap_or_else(|| "127.0.0.1".to_string()),
        port: options.port.unwrap_or(3000),
    };
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let handle = match serve::serve_static(&serve_options, move |address| {
        let _ = ready_sender.send(address);
    }) {
        Ok(handle) => handle,
        Err(error) => return ServeOutcome { ok: false, error: Some(error.to_string()), address: None },
    };
    let address = ready_receiver.recv().map(|address| address.to_string()).unwrap_or_default();
    let _ = handle.thread.join();
    ServeOutcome { ok: true, error: None, address: Some(address) }
}
