// `legion build` 命令。
//
// 这里只负责装配与落盘；目标解释、后端注册和 lowering 均交给 `emitter`。
use crate::legion::{
    cmds::error::{LegionError, LegionResult},
    workspace::{
        labeled_report_with_context,
        manifest::{ProjectArtifactKind, ProjectManifest},
        planner::{BuildPlan, BuildRequest, WorkspaceResolver, compiler_host_provider_bindings},
        source_snapshot::compile_source_snapshot,
    },
};
use compiler_emitter::DriverRunContract;
use compiler_target::{ArtifactContract, ArtifactSet, CanonicalTarget, ClrSuspendStrategy};
use compiler_types::compiler_input::CompilerSourceInput;
use language_valkyrie::compiler::emitter::pipeline::{CompilerBuildContext, compile_source_groups_to_artifacts};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::legion::{
    cache::{CompilationCache, cache_root_for, collect_build_bundle, compute_artifact_hash, store_cached_build},
    cmds::{
        project_input::resolve_project_path,
        run::{ExecutionManifest, RunContract},
    },
    script, unity_export, write_von_indented,
};

/// `legion build` 的命令参数。
#[derive(Debug, Clone)]
pub struct BuildArgs {
    /// 项目目录，默认当前目录。
    pub project_dir: PathBuf,
    /// 已冻结的解析目标：平台身份 + 产物/宿主合同。
    ///
    /// `native` / `node` / `python` 占位符必须先经统一目标解析服务
    /// （[`crate::legion::resolve_authoring_target`]）展开成完整事实再进入
    /// 构建；合同不在这里丢失，下游不猜产物形态。
    pub resolved: crate::legion::ResolvedTarget,
    /// 输出目录。
    pub output_dir: Option<PathBuf>,
    /// 输出调试用构建副产物。
    pub debug_artifacts: bool,
}

/// 执行 `legion build`。
///
/// 编译链是表达式深度驱动的递归，主线程默认栈装不下；统一在 `legion-driver` worker 上跑。
pub fn run(args: &BuildArgs) -> LegionResult<ExitCode> {
    let args = args.clone();
    crate::legion::cmds::run_with_driver_stack(move || run_inner(&args))
}

fn run_inner(args: &BuildArgs) -> LegionResult<ExitCode> {
    let project_input = resolve_project_path(&args.project_dir)?;
    let workspace = WorkspaceResolver::discover_for_project(&project_input)?;
    if workspace.is_at_workspace_root(&project_input) {
        return run_all_workspace_members(args);
    }

    let request =
        BuildRequest { project_dir: project_input, resolved: args.resolved.clone(), output_dir: args.output_dir.clone(), ..Default::default() };
    let plan = workspace.build_plan(&request)?;

    println!("workspace: {}", crate::legion::cmds::path_for_cli_log(&plan.workspace_root));
    println!("mode: workspace");
    println!("project: {}", plan.project.name);
    println!("target: {}", plan.project.build_target.target);
    if matches!(plan.project.resolved.contract, ArtifactContract::NodeApi | ArtifactContract::Cpython(_)) {
        println!("host contract: {}", plan.project.resolved.contract.as_str());
    }
    println!("output: {}", crate::legion::cmds::path_for_cli_log(&plan.output_dir));
    println!("sources: {}", plan.project.source_files.len());
    println!("host contracts: {}", plan.project.host_contracts.len());
    println!("selected host providers: {}", plan.project.selected_host_providers.len());
    let host_capabilities = {
        let mut capabilities =
            plan.project.selected_host_providers.iter().flat_map(|provider| provider.required_capabilities.iter().cloned()).collect::<Vec<_>>();
        capabilities.sort();
        capabilities.dedup();
        capabilities
    };
    if !host_capabilities.is_empty() {
        println!("host capabilities: {}", host_capabilities.join(", "));
    }

    if plan.project.source_files.is_empty() {
        return Err(LegionError::msg(report!("没有找到任何源码文件")));
    }

    if args.debug_artifacts {
        write_host_selection_spec(&plan.output_dir, &plan.project.selected_host_providers)?;
    }
    else {
        remove_host_selection_spec(&plan.output_dir)?;
    }

    let api_plan = crate::legion::api::LegionBuildPlan::from_build_plan(plan.clone());
    let compile_report =
        crate::legion::api::compile(&api_plan, crate::legion::api::CompileOptions { verbose: true, debug_artifacts: args.debug_artifacts })?;
    if !compile_report.success {
        let message = compile_report.diagnostics.first().map(|d| d.message.clone()).unwrap_or_else(|| "compile failed".into());
        return Err(LegionError::msg(message));
    }
    let report = compile_report.driver.as_ref().ok_or_else(|| LegionError::msg(report!("编译成功但缺少驱动报告")))?;
    println!("backend status: compiled");
    let target_profile = plan.project.build_target.target.to_profile(None);
    println!("host kind: {:?}", target_profile.host_kind);
    println!("publish format: {}", target_profile.artifact_policy.default_publish_format);
    if let Some(entry_symbol) = &compile_report.entry_symbol {
        println!("entry: {}", entry_symbol);
    }
    print_artifacts(&plan.output_dir, &report.artifacts);
    let manifest_source = if script::is_script_path(&plan.project.manifest_path) {
        let script_source = fs::read_to_string(&plan.project.manifest_path)
            .map_err(|e| LegionError::msg(format!("读取单脚本失败：{}: {e}", plan.project.manifest_path.display())))?;
        script::extract_embedded_manifest(&script_source, &plan.project.manifest_path)
            .map_err(|error| LegionError::from(error))?
            .ok_or_else(|| LegionError::msg(report!("单脚本 `{}` 缺少内嵌 `# ```legion` 块", plan.project.manifest_path.display())))?
    }
    else {
        fs::read_to_string(&plan.project.manifest_path)
            .map_err(|e| LegionError::msg(format!("读取项目清单失败：{}: {e}", plan.project.manifest_path.display())))?
    };
    let project_manifest = ProjectManifest::parse(&manifest_source)?;
    if let Some(plugin) = &project_manifest.build_plugin {
        unity_export::export_unity_project(&plan, &report, plugin)?;
        println!("unity export: {}", plugin.kind);
    }

    Ok(ExitCode::SUCCESS)
}

fn run_all_workspace_members(args: &BuildArgs) -> LegionResult<ExitCode> {
    let workspace = WorkspaceResolver::discover_for_project(&args.project_dir)?;
    let requested_target = args.resolved.clone();
    let members_dirs = workspace.legions_member_dirs();
    let mut members = Vec::with_capacity(members_dirs.len());
    // CPython 模块名由构建计划按工程名冻结一次：同一输出目录里两个成员归一到
    // 同名会互相覆盖 `.pyd`/`PyInit_`，必须在写出前拒绝并同时指名两个工程。
    // 预检只消费冻结事实，不重新归一化。
    let mut cpython_modules: Vec<(String, String)> = Vec::new();
    for project_dir in members_dirs {
        let request = BuildRequest {
            project_dir: project_dir.clone(),
            resolved: requested_target.clone(),
            output_dir: args.output_dir.clone(),
            ..Default::default()
        };
        match workspace.build_plan(&request) {
            Ok(plan) => {
                if let ArtifactContract::Cpython(config) = &plan.project.resolved.contract {
                    let module = config.module_name.clone().ok_or_else(|| {
                        LegionError::msg(report!("CPython 目标计划未冻结模块名：工程 `{}`", plan.project.name))
                    })?;
                    ensure_cpython_module_unique(&mut cpython_modules, module, &plan.project.name)?;
                }
                members.push(project_dir);
            }
            Err(crate::legion::workspace::planner::PlannerError::MissingBuildTarget { .. }) => {
                eprintln!("workspace: skipping {} (target not declared)", project_dir.display());
            }
            Err(error) => {
                eprintln!("workspace: skipping {} (planner error: {})", project_dir.display(), error);
            }
        }
    }
    if members.is_empty() {
        return Err(LegionError::msg(report!("workspace 没有可构建成员")));
    }

    println!("workspace mode: 顺序构建 {} 个成员（{}）", members.len(), crate::legion::cmds::path_for_cli_log(&workspace.root_dir));

    let mut failures = Vec::new();
    for project_dir in members {
        let member_args = BuildArgs {
            project_dir: project_dir.clone(),
            resolved: args.resolved.clone(),
            output_dir: args.output_dir.clone(),
            debug_artifacts: args.debug_artifacts,
        };
        match run_inner(&member_args) {
            Ok(code) if code == ExitCode::SUCCESS => {}
            Ok(_) => failures.push(format!("{}: exited with failure code", project_dir.display())),
            Err(error) => failures.push(format!("{}: {}", project_dir.display(), error)),
        }
    }

    if failures.is_empty() {
        println!("workspace 构建完成：全部成功");
        Ok(ExitCode::SUCCESS)
    }
    else {
        for failure in &failures {
            eprintln!("workspace build failure: {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

/// 记录一个 CPython 成员的归一化模块名；同名但不同工程即在写出前拒绝。
fn ensure_cpython_module_unique(
    modules: &mut Vec<(String, String)>,
    module: String,
    project: &str,
) -> LegionResult<()> {
    if let Some((previous_module, previous_project)) =
        modules.iter().find(|(name, owner)| name == &module && owner != project)
    {
        return Err(LegionError::msg(report!(
            "CPython 模块名碰撞：工程 `{previous_project}` 与 `{project}` 归一到同一模块名 `{module}`（此前为 `{previous_module}`），拒绝互相覆盖的扩展产物"
        )));
    }
    modules.push((module, project.to_string()));
    Ok(())
}

#[cfg(test)]
mod cpython_module_collision_tests {
    use super::ensure_cpython_module_unique;

    #[test]
    fn duplicate_module_for_different_projects_is_rejected() {
        let mut modules = vec![("leetcode_foo_bar".to_string(), "leetcode-foo-bar".to_string())];
        let error = ensure_cpython_module_unique(&mut modules, "leetcode_foo_bar".to_string(), "leetcode_foo_bar")
            .expect_err("不同工程归一到同名模块必须拒绝");
        let text = format!("{error}");
        assert!(text.contains("leetcode-foo-bar"), "{text}");
        assert!(text.contains("leetcode_foo_bar"), "{text}");
    }

    #[test]
    fn same_project_reentry_and_distinct_modules_are_allowed() {
        let mut modules = vec![
            ("leetcode_foo".to_string(), "leetcode-foo".to_string()),
            ("leetcode_foo".to_string(), "leetcode-foo".to_string()),
        ];
        ensure_cpython_module_unique(&mut modules, "leetcode_bar".to_string(), "leetcode-bar")
            .expect("不同模块名不应碰撞");
    }
}

/// 编译给定 `BuildPlan`，返回驱动编译报告。
pub fn compile_plan(plan: &BuildPlan, verbose: bool) -> LegionResult<compiler_emitter::DriverCompileReport> {
    if plan.project.source_files.is_empty() {
        return Err(LegionError::msg(report!("没有找到任何源码文件")));
    }

    let cache = CompilationCache::open(cache_root_for(&plan.workspace_root));
    // 缓存身份消费解析服务冻结的完整 identity：平台 + 宿主合同 + CPython ABI
    // 配置（扩展后缀、导入库）。同一平台上普通原生产物、Node 扩展与不同解释器
    // 的 CPython 扩展不是同一份产物。
    let canonical_triple = plan.project.resolved.identity();
    let manifest_files = plan
        .project
        .semantic_source_groups
        .iter()
        .map(|group| group.manifest_dir.join("legion.von"))
        .chain(std::iter::once(plan.project.manifest_path.clone()))
        .collect::<Vec<_>>();
    let ir_hash = compute_artifact_hash(
        &plan.project.source_files,
        &manifest_files,
        &canonical_triple,
        plan.project.build_target.wat,
        plan.project.build_target.runtime_async,
        &plan.enabled_features,
        Some(&crate::legion::workspace::planner::provider_set_digest(&plan.project.selected_host_providers)),
    )
    .map_err(|error| report!("{error}"))?;

    // 模板展开只按目标真实架构分支；Node / WASI 的宿主差异由 `[when]` 条件表达，
    // 不再用伪架构键折叠。
    let arch = compile_arch(plan);
    let source_groups = compile_source_snapshot(&plan.project.semantic_source_groups)?;
    if verbose {
        println!("frontend: semantic source groups={}", plan.project.semantic_source_groups.len());
    }
    let target_profile = plan.project.build_target.target.to_profile(None);
    let clr_suspend_strategy = ClrSuspendStrategy::from_runtime_async_flag(plan.project.build_target.runtime_async);
    // 产物用途由清单、宿主合同与测试模式显式声明；Wasm 容器形态从用途派生，
    // 不得反过来由容器枚举推导全 lane 的交付形态。
    let host_extension = matches!(plan.project.resolved.contract, ArtifactContract::NodeApi | ArtifactContract::Cpython(_));
    let artifact_purpose = if plan.project.is_test {
        // 测试构建：`[test]` 是唯一的 run 入口，不得要求 `[main]`
        // 或把测试回落成普通程序入口。
        if host_extension {
            return Err(LegionError::msg("宿主扩展产物不能作为测试构建：测试运行器没有宿主装载链"));
        }
        compiler_target::ArtifactPurpose::TestBundle
    }
    else if host_extension {
        // 宿主合同只与库交付组合：程序入口与导出面是独立事实，
        // binary + 宿主合同必须在写出前拒绝，不得改标或补造入口。
        match plan.project.artifact_kind {
            ProjectArtifactKind::Library => compiler_target::ArtifactPurpose::HostExtension,
            ProjectArtifactKind::Binary => {
                return Err(LegionError::msg("宿主扩展不接受程序入口：把产物声明为 library 并以导出面提供宿主 API"));
            }
        }
    }
    else {
        match plan.project.artifact_kind {
            ProjectArtifactKind::Library => compiler_target::ArtifactPurpose::Library,
            ProjectArtifactKind::Binary => compiler_target::ArtifactPurpose::Application,
        }
    };
    let wasm_package_kind =
        if matches!(artifact_purpose, compiler_target::ArtifactPurpose::TestBundle | compiler_target::ArtifactPurpose::Library) {
            compiler_emitter::backend::wasi::WasmPackageKind::Library
        }
        else {
            compiler_emitter::backend::wasi::WasmPackageKind::Binary
        };
    fs::create_dir_all(&plan.output_dir).map_err(|e| LegionError::msg(format!("创建输出目录失败: {}: {e}", plan.output_dir.display())))?;
    let host_bindings =
        compiler_host_provider_bindings(&plan.project.selected_host_providers).map_err(|error| LegionError::msg(report!("{error}")))?;
    let gated_out_host_providers = crate::legion::workspace::planner::compiler_gated_out_host_providers(&plan.project.gated_out_host_providers);
    // 编译计划只消费解析服务冻结的 `ResolvedTarget`：平台与合同一起进入，
    // 不在这里重新拼装或降级成单一四元组。
    let build_context = CompilerBuildContext::new(arch, plan.project.resolved.clone(), clr_suspend_strategy, wasm_package_kind)
        .with_artifact_purpose(artifact_purpose)
        .with_test_entries(plan.project.test_entries.clone())
        .with_selected_host_providers(host_bindings)
        .with_gated_out_host_providers(gated_out_host_providers);
    // 产物事务：并发序列化 → 全部产物、sidecar、evidence 与运行合同写入暂存目录
    // → 提交前完整性验证 → 受管理 generation 原子提交（可回滚）。消费者只会看到
    // 完整提交的一代；本代消失的旧受管理制品随提交退出，非受管理的用户文件不动。
    // 缓存写入失败与交付提交成功分别报告：已提交的交付不会因缓存故障被谎报失败。
    let _build_lock = acquire_build_lock(&plan.output_dir)?;
    let staging_dir = staging_dir_for(&plan.output_dir);
    let _ = fs::remove_dir_all(&staging_dir);
    fs::create_dir_all(&staging_dir).map_err(|e| LegionError::msg(format!("创建产物暂存目录失败: {}: {e}", staging_dir.display())))?;
    let outcome = (|| -> LegionResult<compiler_emitter::DriverCompileReport> {
        let artifact = match compile_source_groups_to_artifacts(
            &language_valkyrie::compiler::hir::lowering::ValkyrieCompiler::default(),
            &compiler_types::compiler_input::CompilerSourceInput::application(source_groups),
            &build_context,
            &staging_dir,
            &plan.project.name,
            plan.project.build_target.wat,
            target_profile.artifact_policy.generate_runtime_config,
        ) {
            Ok(artifact) => artifact,
            Err(error) => return Err(compile_error_with_diagnostics(error, plan)),
        };
        write_compile_evidence(&staging_dir, &artifact.evidence)?;
        for managed_abi in &artifact.driver.managed_abi {
            write_managed_abi(&staging_dir, managed_abi)?;
        }
        for ovm_abi in &artifact.driver.ovm_abi {
            write_ovm_abi(&staging_dir, ovm_abi)?;
        }
        if verbose {
            println!("compiler: canonical artifact set ready");
        }

        // Write execution manifest before collecting the artifact-set so restore can run.
        // 库产物可能没有可执行 run contract（wasm 走 `callExport`），但仍必须写入
        // 导出索引，因此库构建无条件写 manifest。合法无 manifest 的交付不写它：
        // 上一代的运行合同属于受管理制品，会随本代提交退出，不再提交后单独补删。
        let is_library = plan.project.artifact_kind == ProjectArtifactKind::Library;
        if is_library || !artifact.driver.run_contracts.is_empty() {
            write_execution_manifest(&staging_dir, plan, &artifact.driver.run_contracts, &artifact.driver.library_exports, &artifact.evidence)?;
        }

        // 提交前完整性验证：报告声明的全部产物、运行合同条目与 sidecar
        // 必须在暂存集合中齐备；缺失即整组失败，不提交部分交付。
        collect_build_bundle(&staging_dir, &artifact.driver).map_err(|error| report!("产物集合不完整（暂存目录）：{error}"))?;

        commit_staged_delivery(&staging_dir, &plan.output_dir, &canonical_triple, &ir_hash)?;
        Ok(artifact.driver)
    })();
    let driver = match outcome {
        Ok(driver) => driver,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(error);
        }
    };
    // 交付已提交；缓存只是加速路径，失败单独报告为警告，不推翻已成功的交付。
    match collect_build_bundle(&plan.output_dir, &driver).map_err(|error| report!("产物集合不完整：{error}")).and_then(|bundle| {
        store_cached_build(&cache, &plan.project.name, &canonical_triple, &ir_hash, &bundle)
            .map_err(|error| report!("产物缓存写入失败：{error}"))
    }) {
        Ok(()) => {
            if verbose {
                println!("cache: stored (artifact-set)");
            }
        }
        Err(error) => {
            eprintln!("warning: 交付已提交，但产物缓存写入失败（下次构建将重新编译）: {error}");
        }
    }

    Ok(driver)
}

/// 暂存目录：输出目录内的隐藏子目录，按进程与序号命名，避免并发构建互相覆盖。
fn staging_dir_for(output_dir: &Path) -> PathBuf {
    static STAGING_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    output_dir.join(format!(".nyar-staging-{}-{seq}", std::process::id()))
}

/// 受管理制品的 generation 身份文件：记录最近一次成功提交的整代文件清单与逐文件摘要。
///
/// 消费者只应把这份清单列出的文件当作上一代交付；清单之外的文件是用户内容，
/// 提交事务不清空它们。文件本身是元数据，不在受管理清单内。
///
/// 整代一致性合同：提交点是本文件的一次原子 rename 切换；清单内每个产物携带
/// 提交时冻结的字节摘要，读者先读清单固定一代、再读产物并按摘要核对，摘要不符
/// 即说明读取跨越了切换点，必须重读清单而不是接受混合代。
pub(crate) const DELIVERY_MANIFEST_FILE: &str = ".nyar-delivery.json";

/// 并发提交事务的锁文件：只在一代提交期间存在，不是产物。
pub(crate) const BUILD_LOCK_FILE: &str = ".nyar-build.lock";

/// 整代索引中的单个受管理产物。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ManagedFile {
    /// 受管理产物文件名（相对输出目录）。
    name: String,
    /// 提交时冻结的字节摘要（sha256 小写 hex）：跨代读取的唯一可检测证据。
    sha256: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct DeliveryManifest {
    /// 身份 schema 版本（v1 起 `files` 携带逐文件摘要）。
    schema_version: u32,
    /// 本代身份（目标三元组 + IR 摘要）。
    generation: String,
    /// 整代受管理文件与提交时冻结的摘要。
    files: Vec<ManagedFile>,
}

/// 暂存产物的字节摘要（sha256 小写 hex）。
fn sha256_of_file(path: &Path) -> LegionResult<String> {
    let bytes = fs::read(path).map_err(|e| LegionError::msg(format!("读取暂存产物做摘要失败: {}: {e}", path.display())))?;
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 并发序列化：同一输出目录同时只允许一个提交事务。
#[derive(Debug)]
struct BuildLock {
    path: PathBuf,
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_build_lock(output_dir: &Path) -> LegionResult<BuildLock> {
    let path = output_dir.join(BUILD_LOCK_FILE);
    match fs::File::create_new(&path) {
        Ok(_) => Ok(BuildLock { path }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(LegionError::msg(format!("输出目录正被另一个构建占用，等待其完成后重试: {}", output_dir.display())))
        }
        Err(error) => Err(LegionError::msg(format!("创建构建锁失败 {}: {error}", path.display()))),
    }
}

fn read_delivery_manifest(output_dir: &Path) -> DeliveryManifest {
    // 读不到的清单按"无受管理上一代"处理：首次提交或被中断的旧格式交付。
    let Some(bytes) = fs::read(output_dir.join(DELIVERY_MANIFEST_FILE)).ok() else {
        return DeliveryManifest::default();
    };
    if let Ok(manifest) = serde_json::from_slice::<DeliveryManifest>(&bytes) {
        return manifest;
    }
    // v0 历史格式：`files` 是纯文件名数组，没有逐文件摘要；按无摘要读入，
    // 保证升级后的 retire 集合仍能识别上一代受管理文件。
    #[derive(serde::Deserialize)]
    struct LegacyManifest {
        #[serde(default)]
        generation: String,
        #[serde(default)]
        files: Vec<String>,
    }
    match serde_json::from_slice::<LegacyManifest>(&bytes) {
        Ok(legacy) => DeliveryManifest {
            schema_version: 0,
            generation: legacy.generation,
            files: legacy.files.into_iter().map(|name| ManagedFile { name, sha256: String::new() }).collect(),
        },
        Err(_) => DeliveryManifest::default(),
    }
}

/// 提交整组暂存产物为新一代受管理交付，可回滚。
///
/// 顺序：预校验暂存集合并冻结逐文件摘要 → 把将被覆盖与退出的上一代受管理文件
/// 移入备份目录 → 移入本代文件 → **一次原子 rename 切换整代索引** → 清理备份。
/// 提交点是 generation 清单的一次切换：清单携带每个产物的字节摘要，读者按
/// 「先读清单、再读产物、核对摘要」固定整代，切换窗口内的跨代读取由摘要不符
/// 显式暴露，不会被当成合法交付。任何一步失败都把备份移回，输出目录恢复上一代
/// 完整状态；消费者不会看到半代交付，也不会因本代缺失而残留旧受管理制品
/// （运行合同退出同样由删除集覆盖，不再有提交后补删）。
fn commit_staged_delivery(staging_dir: &Path, output_dir: &Path, triple: &str, ir_hash: &str) -> LegionResult<()> {
    // 预校验：只允许普通文件，先把本代集合与字节摘要固定下来。
    let mut staged: Vec<(PathBuf, String, String)> = Vec::new();
    for entry in fs::read_dir(staging_dir).map_err(|e| LegionError::msg(format!("枚举暂存目录失败: {}: {e}", staging_dir.display())))? {
        let entry = entry.map_err(|e| LegionError::msg(format!("读取暂存目录项失败: {e}")))?;
        let file_type = entry.file_type().map_err(|e| LegionError::msg(format!("暂存目录项类型不可读: {}: {e}", entry.path().display())))?;
        if !file_type.is_file() {
            return Err(LegionError::msg(format!("产物暂存目录不应包含子目录: {}", entry.path().display())));
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string)
        else {
            return Err(LegionError::msg(format!("暂存产物文件名不是合法 UTF-8: {}", entry.path().display())));
        };
        if name == DELIVERY_MANIFEST_FILE || name.starts_with(".nyar-") {
            return Err(LegionError::msg(format!("暂存目录不应包含事务元数据: {name}")));
        }
        let digest = sha256_of_file(&entry.path())?;
        staged.push((entry.path(), name, digest));
    }
    let staged_names: Vec<&str> = staged.iter().map(|(_, name, _)| name.as_str()).collect();

    let previous = read_delivery_manifest(output_dir);
    static BACKUP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let backup_seq = BACKUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let backup_dir = output_dir.join(format!(".nyar-backup-{}-{backup_seq}", std::process::id()));
    fs::create_dir_all(&backup_dir).map_err(|e| LegionError::msg(format!("创建提交备份目录失败: {}: {e}", backup_dir.display())))?;

    // 回滚记录按对读者的影响分三类，保证替换项不留「文件缺失」窗口：
    // - created：本代新建、原目录不存在的文件，回滚时删除。
    // - overwritten：本代覆盖旧文件的文件，旧字节先按副本留在备份目录，原位文件保持在场，
    //   回滚时把备份 rename 回原位（同卷 rename 覆盖是原子的，读者不会看到半写文件）。
    // - retired：上一代受管理而本代退出的文件，整体移入备份目录，回滚时移回原位。
    let mut created: Vec<PathBuf> = Vec::new();
    let mut overwritten: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut retired: Vec<(PathBuf, PathBuf)> = Vec::new();
    // 返回 Some(备份目录) 表示恢复未全部成功：必须保留备份作为唯一恢复副本，禁止删除。
    let rollback = |created: &[PathBuf], overwritten: &[(PathBuf, PathBuf)], retired: &[(PathBuf, PathBuf)]| -> Option<PathBuf> {
        for to in created.iter().rev() {
            let _ = fs::remove_file(to);
        }
        let mut restore_failed = false;
        for (backup, original) in overwritten.iter().rev() {
            // 同卷 rename 原子覆盖：回滚不把备份逐字节写回原位，否则读者能在
            // 写入中途看到半旧半新的产物。
            if fs::rename(backup, original).is_err() {
                restore_failed = true;
            }
        }
        for (backup, original) in retired.iter().rev() {
            if fs::rename(backup, original).is_err() {
                restore_failed = true;
            }
        }
        if restore_failed {
            Some(backup_dir.clone())
        }
        else {
            let _ = fs::remove_dir_all(&backup_dir);
            None
        }
    };
    let report_rollback = |kept: Option<PathBuf>, base: String| -> LegionError {
        match kept {
            Some(dir) => LegionError::msg(format!("{base}；回滚未能恢复全部上一代文件，恢复副本保留于 {}，请人工介入", dir.display())),
            None => LegionError::msg(base),
        }
    };

    // 1) 备份将被覆盖的旧文件（留在原位，读者仍见完整旧代），并把退出项整体移入备份。
    let retire: Vec<&str> = previous.files.iter().map(|file| file.name.as_str()).filter(|name| !staged_names.contains(name)).collect();
    for name in &staged_names {
        let existing = output_dir.join(name);
        if existing.is_file() {
            let backup = backup_dir.join(name);
            if let Err(error) = fs::copy(&existing, &backup) {
                let kept = rollback(&created, &overwritten, &retired);
                return Err(report_rollback(kept, format!("备份旧产物失败 {}: {error}", existing.display())));
            }
            overwritten.push((backup, existing));
        }
    }
    for name in &retire {
        let existing = output_dir.join(name);
        if existing.is_file() {
            let backup = backup_dir.join(name);
            if let Err(error) = fs::rename(&existing, &backup) {
                let kept = rollback(&created, &overwritten, &retired);
                return Err(report_rollback(kept, format!("移出退出产物失败 {}: {error}", existing.display())));
            }
            retired.push((backup, existing));
        }
    }

    // 2) 移入本代：同卷 rename 原子覆盖，替换项全程在场，不再出现「先移走再移入」的缺失窗口。
    for (from, name, _) in &staged {
        let to = output_dir.join(name);
        let overwrites = overwritten.iter().any(|(_, original)| original == &to);
        if let Err(error) = fs::rename(from, &to) {
            let kept = rollback(&created, &overwritten, &retired);
            return Err(report_rollback(kept, format!("提交产物失败: {} -> {}: {error}", from.display(), to.display())));
        }
        if !overwrites {
            created.push(to);
        }
    }

    // 3) 切换整代索引：这是唯一的提交点。清单先写临时文件，再一次 rename 落到
    //    正式路径——读者要么看到完整的上一代清单，要么看到完整的本代清单，
    //    不会读到半份索引。清单自身是元数据，不进入受管理文件集合。
    let manifest = DeliveryManifest {
        schema_version: 1,
        generation: format!("{triple}-{ir_hash}"),
        files: staged.iter().map(|(_, name, digest)| ManagedFile { name: name.clone(), sha256: digest.clone() }).collect(),
    };
    let manifest_bytes = match serde_json::to_vec_pretty(&manifest) {
        Ok(bytes) => bytes,
        Err(error) => {
            let kept = rollback(&created, &overwritten, &retired);
            return Err(report_rollback(kept, format!("序列化 generation 清单失败: {error}")));
        }
    };
    let manifest_path = output_dir.join(DELIVERY_MANIFEST_FILE);
    let manifest_tmp = output_dir.join(format!("{DELIVERY_MANIFEST_FILE}.tmp"));
    if let Err(error) = fs::write(&manifest_tmp, &manifest_bytes) {
        let _ = fs::remove_file(&manifest_tmp);
        let kept = rollback(&created, &overwritten, &retired);
        return Err(report_rollback(kept, format!("写入 generation 清单临时文件失败: {error}")));
    }
    if let Err(error) = fs::rename(&manifest_tmp, &manifest_path) {
        let _ = fs::remove_file(&manifest_tmp);
        let kept = rollback(&created, &overwritten, &retired);
        return Err(report_rollback(kept, format!("切换 generation 清单失败: {error}")));
    }

    // 4) 提交成功：清理备份副本与暂存目录。
    let _ = fs::remove_dir_all(&backup_dir);
    fs::remove_dir_all(staging_dir).map_err(|e| LegionError::msg(format!("清理暂存目录失败: {}: {e}", staging_dir.display())))?;
    Ok(())
}

#[cfg(test)]
mod artifact_transaction_tests {
    use super::*;

    /// 提交整组暂存产物：新文件进入交付、同名旧文件被替换、generation 清单落盘。
    #[test]
    fn commit_moves_whole_staged_set_and_replaces_old_delivery() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path().join("dist");
        fs::create_dir_all(&output).expect("output dir");
        fs::write(output.join("stale-only.txt"), b"user file").expect("write user file");
        fs::write(output.join("app.wasm"), b"old bytes").expect("write old artifact");

        let staging = staging_dir_for(&output);
        fs::create_dir_all(&staging).expect("staging dir");
        fs::write(staging.join("app.wasm"), b"new bytes").expect("write new artifact");
        fs::write(staging.join("compile-evidence.txt"), b"evidence").expect("write sidecar");

        commit_staged_delivery(&staging, &output, "wasm32-unknown-unknown", "ir1").expect("提交整组暂存产物");

        assert_eq!(fs::read(output.join("app.wasm")).expect("read artifact"), b"new bytes", "同名旧产物必须被整组替换");
        assert!(output.join("compile-evidence.txt").is_file(), "sidecar 与产物同批提交");
        assert!(output.join("stale-only.txt").is_file(), "非受管理的用户文件不被事务清空");
        assert!(!staging.exists(), "提交成功后暂存目录必须清空");
        let manifest = read_delivery_manifest(&output);
        assert_eq!(manifest.generation, "wasm32-unknown-unknown-ir1");
        let app = manifest.files.iter().find(|file| file.name == "app.wasm").unwrap_or_else(|| panic!("清单必须列出 app.wasm: {manifest:?}"));
        assert_eq!(app.sha256, sha256_of_file(&output.join("app.wasm")).expect("digest"), "清单摘要必须冻结提交时的字节");
        assert!(!manifest.files.iter().any(|file| file.name == DELIVERY_MANIFEST_FILE), "清单自身是元数据，不进入受管理集合: {manifest:?}");
    }

    /// 上一代受管理而本代消失的制品（如运行合同）随提交退出；用户文件保留。
    #[test]
    fn commit_retires_previous_managed_files() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path().join("dist");
        fs::create_dir_all(&output).expect("output dir");
        // 手工布置上一代：manifest 声明 app.wasm 与 run-contracts.txt 受管理。
        fs::write(output.join("app.wasm"), b"g1").expect("write g1");
        fs::write(output.join("run-contracts.txt"), b"g1 contracts").expect("write g1 contracts");
        fs::write(output.join("user-notes.md"), b"keep me").expect("write user file");
        // 用 v0 历史格式布置上一代，同时覆盖 legacy 解析路径。
        let previous = serde_json::json!({
            "schema_version": 0,
            "generation": "g1",
            "files": ["app.wasm", "run-contracts.txt", DELIVERY_MANIFEST_FILE],
        });
        fs::write(output.join(DELIVERY_MANIFEST_FILE), serde_json::to_vec(&previous).expect("serialize")).expect("write manifest");

        let staging = staging_dir_for(&output);
        fs::create_dir_all(&staging).expect("staging dir");
        fs::write(staging.join("app.wasm"), b"g2").expect("write g2");
        // 本代没有 run-contracts.txt：上一代受管理副本必须退出。

        commit_staged_delivery(&staging, &output, "wasm32-unknown-unknown", "ir2").expect("提交第二代交付");

        assert_eq!(fs::read(output.join("app.wasm")).expect("read g2"), b"g2");
        assert!(!output.join("run-contracts.txt").exists(), "本代消失的受管理运行合同必须随提交退出");
        assert!(output.join("user-notes.md").is_file(), "用户文件不清空");
    }

    /// 提交中途失败必须回滚到上一代完整状态，不留半代交付。
    #[test]
    fn commit_rolls_back_to_previous_generation_on_failure() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path().join("dist");
        fs::create_dir_all(&output).expect("output dir");
        fs::write(output.join("app.wasm"), b"g1").expect("write g1");
        // 注入失败：本代第二个目标位置是一个目录，rename 无法覆盖它。
        fs::create_dir_all(output.join("run-contracts.txt")).expect("blocking directory");
        let previous = DeliveryManifest {
            schema_version: 1,
            generation: "g1".to_string(),
            files: vec![ManagedFile { name: "app.wasm".to_string(), sha256: sha256_of_file(&output.join("app.wasm")).expect("digest") }],
        };
        fs::write(output.join(DELIVERY_MANIFEST_FILE), serde_json::to_vec(&previous).expect("serialize")).expect("write manifest");

        let staging = staging_dir_for(&output);
        fs::create_dir_all(&staging).expect("staging dir");
        fs::write(staging.join("app.wasm"), b"g2").expect("write g2");
        fs::write(staging.join("run-contracts.txt"), b"g2 contracts").expect("write g2 contracts");

        commit_staged_delivery(&staging, &output, "wasm32-unknown-unknown", "ir2").expect_err("目标被占用必须整组失败");

        assert_eq!(fs::read(output.join("app.wasm")).expect("read after rollback"), b"g1", "回滚后上一代产物完整恢复");
        assert!(output.join(DELIVERY_MANIFEST_FILE).is_file(), "generation 清单回滚保留");
        assert!(read_delivery_manifest(&output).generation == "g1", "清单仍指向上第一代");
    }

    /// 被覆盖的旧产物在回滚时按备份 rename 回原位，且成功恢复后备份目录被清理。
    #[test]
    fn commit_restores_overwritten_files_by_rename() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path().join("dist");
        fs::create_dir_all(&output).expect("output dir");
        fs::write(output.join("app.wasm"), b"g1").expect("write g1");
        fs::write(output.join("lib.wasm"), b"g1").expect("write g1 lib");
        // 注入失败：第三个目标位置是目录，rename 无法覆盖。
        fs::create_dir_all(output.join("broken.txt")).expect("blocking directory");
        let previous = DeliveryManifest {
            schema_version: 1,
            generation: "g1".to_string(),
            files: vec![
                ManagedFile { name: "app.wasm".to_string(), sha256: sha256_of_file(&output.join("app.wasm")).expect("digest") },
                ManagedFile { name: "lib.wasm".to_string(), sha256: sha256_of_file(&output.join("lib.wasm")).expect("digest") },
            ],
        };
        fs::write(output.join(DELIVERY_MANIFEST_FILE), serde_json::to_vec(&previous).expect("serialize")).expect("write manifest");

        let staging = staging_dir_for(&output);
        fs::create_dir_all(&staging).expect("staging dir");
        fs::write(staging.join("app.wasm"), b"g2").expect("write g2");
        fs::write(staging.join("lib.wasm"), b"g2").expect("write g2 lib");
        fs::write(staging.join("broken.txt"), b"g2 broken").expect("write g2 broken");

        commit_staged_delivery(&staging, &output, "wasm32-unknown-unknown", "ir2").expect_err("目标被占用必须整组失败");

        assert_eq!(fs::read(output.join("app.wasm")).expect("read after rollback"), b"g1", "被覆盖产物按备份 rename 回原位");
        assert_eq!(fs::read(output.join("lib.wasm")).expect("read after rollback"), b"g1", "被覆盖产物按备份 rename 回原位");
        assert_eq!(read_delivery_manifest(&output).generation, "g1", "清单仍指向上第一代");
        let leftovers = fs::read_dir(&output)
            .expect("read output")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".nyar-backup-"))
            .count();
        assert_eq!(leftovers, 0, "恢复全部成功后不得残留备份目录");
    }

    /// 暂存集合出现子目录说明装配违约：提交拒绝，不部分移动。
    #[test]
    fn commit_rejects_unexpected_subdirectory() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path().join("dist");
        fs::create_dir_all(&output).expect("output dir");
        let staging = staging_dir_for(&output);
        fs::create_dir_all(staging.join("nested")).expect("staging with subdirectory");

        let error = commit_staged_delivery(&staging, &output, "t", "h").expect_err("子目录必须被拒绝");
        assert!(error.to_string().contains("子目录"), "{error}");
    }

    /// 同一输出目录的提交事务串行：锁未释放前第二个构建拒绝进入。
    #[test]
    fn build_lock_serializes_concurrent_commits() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let output = root.path();
        let first = acquire_build_lock(output).expect("first lock");
        let error = acquire_build_lock(output).expect_err("second lock must be rejected while held");
        assert!(error.to_string().contains("占用"), "{error}");
        drop(first);
        acquire_build_lock(output).expect("lock reacquired after release");
    }

    /// 并发构建的暂存目录互不覆盖。
    #[test]
    fn staging_directories_are_unique() {
        let root = boost_fs::temp::create_folder().expect("temp dir");
        let first = staging_dir_for(root.path());
        let second = staging_dir_for(root.path());
        assert_ne!(first, second);
        assert!(first.file_name().expect("name").to_string_lossy().starts_with(".nyar-staging-"));
    }
}

/// 编译失败时取出本编译单元暂存的全部可恢复诊断，渲染成多诊断报告。
///
/// 没有暂存诊断（例如规划、后端或装配阶段的失败）保持原有单错误文本不变；
/// 有暂存诊断时以完整报告作为错误消息：同一编译单元的全部独立错误一次列出，
/// 按 `(文件, span)` 稳定排序、同一根因归并、超出上限给汇总。
/// 源码原文按诊断的 `source.id`（相对组 manifest 目录的逻辑路径）懒读磁盘，
/// 与 `compile_source_snapshot` 同样做 BOM 归一化，保证 span → 行列映射一致。
fn compile_error_with_diagnostics(error: language_valkyrie::compiler::emitter::pipeline::PipelineError, plan: &BuildPlan) -> LegionError {
    let pending = language_valkyrie::compiler::diagnostics::pending::take_pending_diagnostics();
    if pending.is_empty() {
        return LegionError::from(error);
    }
    // 最终失败事实不得被多诊断报告吞掉：暂存汇已含同 code 条目说明它已被记录，
    // 否则在报告末尾补一行最终失败（rustc 在 aborting 行后同型给出最终失败）。
    let fatal_code = match &error {
        language_valkyrie::compiler::emitter::pipeline::PipelineError::Hir(hir) => Some(hir.code()),
        _ => None,
    };
    let fatal_text = error.to_string();
    let represented = match fatal_code {
        Some(code) => pending.diagnostics.iter().any(|diagnostic| diagnostic.row.code == code),
        None => pending.diagnostics.iter().any(|diagnostic| diagnostic.message == fatal_text),
    };
    let options = language_valkyrie::compiler::diagnostics::report::DiagnosticRenderOptions::from_environment();
    let mut report = language_valkyrie::compiler::diagnostics::report::render_compile_diagnostics(
        &pending,
        &|id| {
            plan.project.semantic_source_groups.iter().find_map(|group| {
                let content = std::fs::read_to_string(group.manifest_dir.join(id)).ok()?;
                Some(content.strip_prefix('\u{FEFF}').map(str::to_string).unwrap_or(content))
            })
        },
        &options,
    );
    if !represented {
        report.push_str(&format!("error: {fatal_text}\n"));
    }
    if report.trim().is_empty() { LegionError::from(error) } else { LegionError::msg(report.trim_end().to_string()) }
}

/// 编译期架构键：目标真实架构，不再折叠伪架构。
///
/// `<% match arch %>` 模板分支只认架构轴；宿主差异（Node / WASI）由
/// `[when(target.specification == ...)]` 文件与声明条件表达。WASI 目标恒为
/// `wasm32`，伪键 `"wasi"` 只会让装配层与需求闭包对同一目标持有不同架构事实。
pub(crate) fn compile_arch(plan: &BuildPlan) -> &'static str {
    plan.project.build_target.target.arch.as_str()
}

/// 将 managed ABI 合同落盘为确定性 pretty JSON。
///
/// 每个物理产物一份 sidecar，文件名由产物名唯一派生（`app.jar` →
/// `app.jar.managed-abi.json`）；多分区构建不得互相覆盖。
fn write_managed_abi(output_dir: &Path, contract: &compiler_emitter::ManagedAbiContract) -> LegionResult<()> {
    let output_path = output_dir.join(compiler_emitter::managed_abi_sidecar_file(&contract.artifact));
    let content = serde_json::to_string_pretty(contract)
        .map_err(|error| LegionError::msg(format!("序列化 managed ABI 契约失败 {}: {error}", output_path.display())))?;
    fs::create_dir_all(output_dir).map_err(|e| LegionError::msg(format!("创建输出目录失败: {}: {e}", output_dir.display())))?;
    fs::write(&output_path, format!("{content}\n"))
        .map_err(|e| LegionError::msg(format!("写入 managed ABI 契约失败 {}: {e}", output_path.display())))?;
    Ok(())
}

/// 将 OVM ABI sidecar 落盘为确定性 pretty JSON：合同 + `.odin` 产物整体哈希。
///
/// 每个物理产物一份 sidecar，文件名由产物名唯一派生（`app.odin` →
/// `app.odin.ovm-abi.json`）；宿主读取后必须先用实际 `.odin` 字节重算哈希，
/// 再按合同解释，不一致 fail-closed。
fn write_ovm_abi(output_dir: &Path, sidecar: &data_binary_odin::OvmAbiSidecar) -> LegionResult<()> {
    let output_path = output_dir.join(data_binary_odin::ovm_abi_sidecar_file(&sidecar.artifact));
    let content = serde_json::to_string_pretty(sidecar)
        .map_err(|error| LegionError::msg(format!("序列化 OVM ABI sidecar 失败 {}: {error}", output_path.display())))?;
    fs::create_dir_all(output_dir).map_err(|e| LegionError::msg(format!("创建输出目录失败: {}: {e}", output_dir.display())))?;
    fs::write(&output_path, format!("{content}\n"))
        .map_err(|e| LegionError::msg(format!("写入 OVM ABI sidecar 失败 {}: {e}", output_path.display())))?;
    Ok(())
}

fn write_compile_evidence(
    output_dir: &Path,
    evidence: &language_valkyrie::compiler::emitter::pipeline::CompilerCompileEvidence,
) -> LegionResult<()> {
    let output_path = output_dir.join("compile-evidence.txt");
    let content = crate::legion::write_von_indented(evidence).map_err(|error| {
        labeled_report_with_context(
            data_text_core::TextError::format_error(error.to_string()),
            format!("序列化 compile evidence 失败 {}", output_path.display()),
            "VON 序列化失败位置",
        )
    })?;
    fs::create_dir_all(output_dir).map_err(|e| LegionError::msg(format!("创建输出目录失败: {}: {e}", output_dir.display())))?;
    fs::write(&output_path, content).map_err(|e| LegionError::msg(format!("写入 compile evidence 失败: {}: {e}", output_path.display())))?;
    Ok(())
}

pub(super) fn print_artifacts(output_dir: &Path, artifacts: &ArtifactSet) {
    for artifact in &artifacts.artifacts {
        let artifact_path = output_dir.join(&artifact.path);
        if !artifact_path.is_file() {
            println!("artifact: missing {}", artifact.path);
            continue;
        }
        println!("artifact: {}", artifact_path.display());
    }
}

fn write_execution_manifest(
    output_dir: &Path,
    plan: &crate::legion::workspace::planner::BuildPlan,
    specs: &[DriverRunContract],
    library_exports: &[compiler_emitter::DriverLibraryExport],
    evidence: &language_valkyrie::compiler::emitter::pipeline::CompilerCompileEvidence,
) -> LegionResult<()> {
    let contracts = specs
        .iter()
        .map(|spec| RunContract {
            logical_entry: spec.logical_entry.clone(),
            physical_entry: spec.physical_entry.clone(),
            boundary: spec.boundary.into(),
            return_projection: spec.return_projection.into(),
            launch: spec.launch.clone(),
            validate: spec.validate.clone(),
        })
        .collect::<Vec<_>>();
    // 依赖模块名的宿主（CPython）在清单里写目标计划已冻结的模块名——与
    // 文件名/`PyInit_` 同源同值；清单写出端不再重新归一化。
    let cpython_module = match &plan.project.resolved.contract {
        ArtifactContract::Cpython(config) => Some(config.module_name.clone().ok_or_else(|| {
            LegionError::msg(report!("CPython 目标计划未冻结模块名：工程 `{}`", plan.project.name))
        })?),
        _ => None,
    };
    let exports = library_exports
        .iter()
        .map(|export| crate::legion::cmds::run::LibraryExportIndexEntry {
            logical_name: export.logical_name.clone(),
            source_name: export.source_name.clone(),
            physical_artifact: export.physical_artifact.clone(),
            invocation: export.invocation.clone(),
            abi_digest: export.abi_digest.clone(),
            abi: export.abi.clone(),
            signature_available: export.parameters.is_some() && export.returns.is_some(),
            parameters: export.parameters.clone().unwrap_or_default(),
            parameter_names: export.parameter_names.clone().unwrap_or_default(),
            returns: export.returns.clone().unwrap_or_default(),
            module_name: cpython_module.clone().unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    ExecutionManifest::from_build_plan(plan, output_dir, &contracts, &exports, evidence)?.write_to_output_dir(output_dir)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HostSelectionEntry {
    contract: String,
    provider: String,
    source_file: String,
    line: usize,
    required_capabilities: Vec<String>,
    foreign_bindings: Vec<String>,
}

fn write_host_selection_spec(output_dir: &Path, providers: &[crate::legion::workspace::planner::PlannedHostProvider]) -> LegionResult<()> {
    let output_path = output_dir.join("host-selection.txt");
    let entries: Vec<HostSelectionEntry> = providers
        .iter()
        .map(|item| HostSelectionEntry {
            contract: item.contract.clone(),
            provider: item.symbol.clone(),
            source_file: item.source_file.display().to_string(),
            line: item.line,
            required_capabilities: item.required_capabilities.clone(),
            foreign_bindings: item.foreign_bindings.clone(),
        })
        .collect();
    let content = write_von_indented(&entries).map_err(|error| {
        labeled_report_with_context(
            data_text_core::TextError::format_error(error.to_string()),
            format!("序列化 host 选择结果失败 {}", output_path.display()),
            "VON 序列化失败位置",
        )
    })?;
    fs::create_dir_all(output_dir).map_err(|e| LegionError::msg(format!("创建输出目录失败 {}: {e}", output_dir.display())))?;
    fs::write(&output_path, content).map_err(|e| LegionError::msg(format!("写入 host 选择结果失败 {}: {e}", output_path.display())))?;
    Ok(())
}

fn remove_host_selection_spec(output_dir: &Path) -> LegionResult<()> {
    let output_path = output_dir.join("host-selection.txt");
    if output_path.exists() {
        fs::remove_file(&output_path).map_err(|e| LegionError::msg(format!("删除 host 选择结果失败 {}: {e}", output_path.display())))?;
    }
    Ok(())
}
