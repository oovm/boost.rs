use crate::{
    DriverBackendInput, DriverCompileReport, DriverCompileRequest, DriverLibraryExport, DriverRunContract, Result,
    bundled_backend_capability_descriptor, emitter_backend_jvm::JvmBinaryBackend,
};
use compiler_optimize::EntryReturnProjection;
use compiler_target::{
    BackendInputKind, HostProjectionBoundary, PartitionBackendRequirement, TargetBackendFamily, TargetFamily, TargetLane,
    backends::TargetCodeGenBackend,
};

use super::BundledBackendCompiler;

/// `JVM` 家族后端编译器：唯一宿主边界是 [`HostProjectionBoundary::Jvm`]。
pub(super) struct JvmFamilyCompiler;

/// 判断给定的后端需求是否可由 `JVM` 家族满足。
pub(super) fn supports_requirement(requirement: &PartitionBackendRequirement) -> bool {
    bundled_backend_capability_descriptor(TargetBackendFamily::Jvm).is_some_and(|descriptor| descriptor.supports_requirement(requirement))
}

impl BundledBackendCompiler for JvmFamilyCompiler {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
        let DriverBackendInput::Jvm(mut input) = request.input
        else {
            return Err(format!("`JVM` 家族请求必须携带 `JvmBinaryBackendInput`"));
        };
        let requirement = &request.requirement;
        if requirement.lane != TargetLane::Jvm {
            return Err(format!("`JVM` 后端需求 lane 不一致：requirement={:?}", requirement.lane));
        }
        if requirement.input_kind != BackendInputKind::JvmClassFile {
            return Err(format!("`JVM` 后端需求输入种类不一致：requirement={:?}", requirement.input_kind));
        }
        if requirement.target.family != TargetFamily::Jvm {
            return Err(format!("`JVM` 后端需求目标家族不一致：requirement={:?}", requirement.target.family));
        }
        if requirement.host_boundary != HostProjectionBoundary::Jvm {
            return Err(format!("`JVM` 后端输入宿主与规划需求不一致：requirement={:?}", requirement.host_boundary));
        }
        // 用途承载能力在驱动层按 descriptor 统一裁决（见 families::compile）；这里只
        // 处理 jvm 用途自身的语义前提：应用必须有冻结入口 launcher，库必须有导出面。
        let entry_launcher = input.entry_launcher.clone();
        let provided_contracts = std::mem::take(&mut input.run_contracts);
        // 可执行产物必须消费 lowering 冻结的入口 launcher；入口无法包装为
        // argv launcher 时明确失败，禁止后端按方法名扫描补造第二入口权威。
        if request.purpose == compiler_target::ArtifactPurpose::Application && entry_launcher.is_none() && provided_contracts.is_empty() {
            return Err("`Application` 的 `jvm` 产物缺少冻结入口 launcher：程序入口无法编组 argv，拒绝产出可执行 jar".to_string());
        }
        let mut managed_abi = input.managed_abi.take();
        if let Some(contract) = managed_abi.as_mut() {
            // 产物文件名由驱动层决定；契约摘要必须覆盖最终文件名。
            contract.set_artifact(format!("{}.jar", request.artifact_name));
        }
        let backend = JvmBinaryBackend::new();
        backend.validate(&input).map_err(|error| error.to_string())?;
        let library_surface = request.purpose == compiler_target::ArtifactPurpose::Library;
        // 库产物必须有导出面：缺失 managed ABI 入口时在写出前结构化失败，
        // 不得回退 `main`、静默返回空运行契约，也不得先写产物再判用途。
        if library_surface {
            match managed_abi.as_ref() {
                None => return Err("`artifact: library` 的 `jvm` 产物缺少 managed ABI 契约；拒绝回退 `main`".to_string()),
                Some(contract) if contract.entries.is_empty() => {
                    return Err("`artifact: library` 的 `jvm` managed ABI 契约为空；拒绝产出空运行契约".to_string());
                }
                Some(_) => {}
            }
        }
        let mut artifacts = backend.compile(input, request.options).map_err(|error| error.to_string())?;
        if library_surface {
            // 装载方式由用途决定：库 jar 按导出面被宿主调用。同次导出的 `.class`
            // 是装配清单件而非库交付本身，不能整批改标成 Library 误导打包器。
            for artifact in &mut artifacts.artifacts {
                if artifact.path.ends_with(".jar") {
                    artifact.kind = compiler_target::ArtifactKind::Library;
                }
            }
        }
        // 运行合同与 JAR manifest 消费同一个冻结入口 launcher；没有程序入口的
        // 库产物由每个 managed ABI 入口各自持有可执行的 argv launcher 与 run contract。
        let run_contracts = if !provided_contracts.is_empty() {
            provided_contracts
        }
        else if let Some(launcher) = &entry_launcher {
            // 入口身份来自 lowering 冻结的计划；launcher 是明确的 argv 分发类。
            vec![DriverRunContract {
                logical_entry: "main".to_string(),
                physical_entry: format!("{}.jar", request.artifact_name),
                boundary: HostProjectionBoundary::Jvm,
                // 冻结入口 launcher 包装把 main 的 i32 返回值以十进制行打印到
                // stdout 并以 0 退出；这是该包装的通道选择，不是 JVM 边界的推论。
                return_projection: EntryReturnProjection::StdoutInteger,
                launch: vec!["java".to_string(), "-jar".to_string(), "{artifact}".to_string()],
                validate: format!("java -cp {}.jar {}", request.artifact_name, launcher.replace('/', ".")),
            }]
        }
        else if let Some(contract) = &managed_abi {
            contract
                .entries
                .iter()
                .filter_map(|entry| {
                    // 无 argv launcher 的入口仍是合法库入口；它不产生命令行运行合同。
                    let launcher = entry.launcher_class.as_deref()?;
                    Some(DriverRunContract {
                        logical_entry: entry.logical_entry.clone(),
                        physical_entry: format!("{}.jar", request.artifact_name),
                        boundary: HostProjectionBoundary::Jvm,
                        // 逐入口 launcher 与主入口 launcher 同一包装策略：返回值走 stdout。
                        return_projection: EntryReturnProjection::StdoutInteger,
                        // 逐入口启动的 argv 是 launcher 类：`java -jar` 只会启动
                        // manifest 里的 Main-Class，无法按测试名选择入口。
                        launch: vec![
                            "java".to_string(),
                            "-cp".to_string(),
                            "{artifact}".to_string(),
                            launcher.replace('/', "."),
                        ],
                        validate: format!("java -cp {}.jar {}", request.artifact_name, launcher.replace('/', ".")),
                    })
                })
                .collect()
        }
        else {
            Vec::new()
        };
        let library_exports = if library_surface {
            managed_abi
                .as_ref()
                .map(|contract| {
                    contract
                        .entries
                        .iter()
                        .map(|entry| DriverLibraryExport {
                            logical_name: entry.logical_entry.clone(),
                            // 两个身份轴各自取自契约冻结事实，不互相复制。
                            source_name: entry.source_name.clone(),
                            physical_artifact: format!("{}.jar", request.artifact_name),
                            invocation: "java".to_string(),
                            abi_digest: contract.contract_digest.clone(),
                            abi: "jvm-managed".to_string(),
                            parameters: Some(entry.parameter_types.clone()),
                            parameter_names: Some(entry.parameter_names.clone()),
                            returns: Some(entry.return_type.clone()),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        else {
            Vec::new()
        };
        Ok(DriverCompileReport {
            artifacts,
            entry_symbol: None,
            run_contracts,
            managed_abi: managed_abi.into_iter().collect(),
            ovm_abi: Vec::new(),
            library_exports,
        })
    }
}
