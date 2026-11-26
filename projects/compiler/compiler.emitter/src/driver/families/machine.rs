//! `machine-code` 家族后端编译器：唯一宿主边界是 [`HostProjectionBoundary::MachineCode`]。
//!
//! 输入是语言 × 目标适配器已经编码好的原生镜像；本家族只负责按冻结的目标四元组
//! 选择容器并写出产物，不重新生成机器码，也不解释语言语义。

use crate::{
    DriverBackendInput, DriverCompileReport, DriverCompileRequest, DriverRunContract, Result, bundled_backend_capability_descriptor,
    emitter_backend_machine::MachineImageBackend,
};
use compiler_target::{
    BackendInputKind, CanonicalSpecification, EntryReturnProjection, HostProjectionBoundary, PartitionBackendRequirement, TargetBackendFamily,
    TargetFamily, TargetLane, backends::TargetCodeGenBackend,
};

/// 由冻结的导出 ABI 事实派生稳定 digest，与 wasm/托管 lane 同语义：同一签名恒得
/// 同一 digest，任一形参载体、形参名、返回载体或 ABI 标签变化都会改变它。
/// 机器库此前留空串，等于告诉消费侧「没有 ABI 约束」，必须携带真实指纹。
fn machine_export_abi_digest(
    abi: &str,
    logical_name: &str,
    parameters: &[String],
    parameter_names: &[String],
    returns: &str,
) -> String {
    let canonical = format!(
        "abi={abi}\u{1f}export={logical_name}\u{1f}returns={returns}\u{1f}parameters={parameters:?}\u{1f}parameter_names={parameter_names:?}"
    );
    format!("sha256-{}", compiler_types::combined_content_hash(&[&canonical]))
}

use super::BundledBackendCompiler;

/// `machine-code` 家族后端编译器。
pub(super) struct MachineFamilyCompiler;

/// 判断给定的后端需求是否可由 `machine-code` 家族满足。
pub(super) fn supports_requirement(requirement: &PartitionBackendRequirement) -> bool {
    bundled_backend_capability_descriptor(TargetBackendFamily::MachineCode)
        .is_some_and(|descriptor| descriptor.supports_requirement(requirement))
}

impl BundledBackendCompiler for MachineFamilyCompiler {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
        let DriverBackendInput::Machine(input) = request.input
        else {
            return Err(format!("`machine-code` 家族请求必须携带 `MachineImageBackendInput`"));
        };
        let requirement = &request.requirement;
        if requirement.lane != TargetLane::MachineCode {
            return Err(format!("`machine-code` 后端需求 lane 不一致：requirement={:?}", requirement.lane));
        }
        if requirement.input_kind != BackendInputKind::MachineImage {
            return Err(format!("`machine-code` 后端需求输入种类不一致：requirement={:?}", requirement.input_kind));
        }
        if requirement.target.family != TargetFamily::MachineCode {
            return Err(format!("`machine-code` 后端需求目标家族不一致：requirement={:?}", requirement.target.family));
        }
        if requirement.host_boundary != HostProjectionBoundary::MachineCode {
            return Err(format!("`machine-code` 后端输入宿主与规划需求不一致：requirement={:?}", requirement.host_boundary));
        }
        let backend = MachineImageBackend::new();
        backend.validate(&input).map_err(|error| error.to_string())?;
        let entry_symbol = input.entry_symbol.clone();
        // 用途决定容器：输入必须已携带与用途一致的产物事实（扩展名、导出面），
        // 包装层不得把可执行镜像改标为库或扩展。
        match request.purpose {
            compiler_target::ArtifactPurpose::Library => {
                // 普通共享库：导出面就是交付合同；镜像必须是共享容器（扩展名门
                // 已保证），没有进程入口，也不得携带宿主模块绑定。
                if input.artifact_extension.is_none() {
                    return Err("native 共享库输入缺少产物扩展名（`.dll`/`.so`）；拒绝写出不具名的共享库交付".to_string());
                }
                if input.host_exports.is_empty() {
                    return Err("native 共享库缺少冻结的导出面；拒绝产出无导出的库交付".to_string());
                }
                if input.host_module.is_some() {
                    return Err("native 共享库不得绑定宿主模块；宿主交付请改用扩展用途".to_string());
                }
            }
            compiler_target::ArtifactPurpose::HostExtension => {
                if input.artifact_extension.is_none() {
                    return Err("native 宿主扩展输入缺少产物扩展名（`.node`/`.pyd`/`.so`）；拒绝写出不具名的共享库".to_string());
                }
                if input.host_exports.is_empty() {
                    return Err("native 宿主扩展缺少冻结的宿主导出面；拒绝产出无导出的扩展交付".to_string());
                }
                if input.host_module.is_none() {
                    return Err("native 宿主扩展缺少冻结的宿主合同；拒绝按扩展名猜装载方式".to_string());
                }
            }
            // 测试包不在本家族的 descriptor 支持面内，驱动层会在调用本 lane 之前
            // 拒绝，因此这里没有测试包分支。
            compiler_target::ArtifactPurpose::Application if !input.host_exports.is_empty() || input.host_module.is_some() => {
                return Err("native 可执行交付不得携带宿主扩展导出面或宿主合同；用途与导出面必须一致".to_string());
            }
            _ => {}
        }
        let artifacts = backend.compile(input.clone(), request.options).map_err(|error| error.to_string())?;
        // 宿主扩展没有进程入口：不产 CLI 运行合同，交付事实是宿主装载入口符号
        // 与按名调用的导出索引（合法无 CLI，区别于合同缺失）。
        if request.purpose == compiler_target::ArtifactPurpose::HostExtension {
            let physical_artifact = artifacts
                .artifacts
                .first()
                .map(|artifact| artifact.path.clone())
                .ok_or_else(|| "native 宿主扩展没有产出共享库镜像".to_string())?;
            let invocation = match input.host_module {
                Some(crate::emitter_backend_machine::HostModule::Node) => "node require",
                Some(crate::emitter_backend_machine::HostModule::Python) => "python import",
                None => return Err("native 宿主扩展缺少冻结的宿主合同；拒绝按扩展名猜装载方式".to_string()),
            };
            let abi = match input.host_module {
                Some(crate::emitter_backend_machine::HostModule::Node) => "node-api",
                Some(crate::emitter_backend_machine::HostModule::Python) => "cpython",
                None => return Err("native 宿主扩展缺少冻结的宿主合同；拒绝按扩展名猜装载方式".to_string()),
            };
            let library_exports = input
                .host_exports
                .iter()
                .map(|export| crate::DriverLibraryExport {
                    logical_name: export.public_name.clone(),
                    source_name: export.source_name.clone(),
                    physical_artifact: physical_artifact.clone(),
                    invocation: invocation.to_string(),
                    abi_digest: machine_export_abi_digest(abi, &export.public_name, &export.parameters, &export.parameter_names, &export.returns),
                    abi: abi.to_string(),
                    parameters: Some(export.parameters.clone()),
                    parameter_names: Some(export.parameter_names.clone()),
                    returns: Some(export.returns.clone()),
                })
                .collect();
            return Ok(DriverCompileReport {
                artifacts,
                // 宿主扩展的身份是宿主装载入口符号；缺失即输入事实错误。
                entry_symbol: Some(entry_symbol.ok_or_else(|| "native 宿主扩展缺少装载入口符号".to_string())?),
                run_contracts: Vec::new(),
                managed_abi: Vec::new(),
                ovm_abi: Vec::new(),
                library_exports,
            });
        }
        // 普通共享库同样没有进程入口：交付事实是按系统加载器调用的 C ABI 导出索引
        // （`LoadLibrary`/`dlopen` 按名取符号），运行合同为空是用途语义而非缺失。
        if request.purpose == compiler_target::ArtifactPurpose::Library {
            let physical_artifact = artifacts
                .artifacts
                .first()
                .map(|artifact| artifact.path.clone())
                .ok_or_else(|| "native 共享库没有产出共享镜像".to_string())?;
            // 装载方式由冻结的平台规格事实决定，与机器后端选容器同源；
            // 不得从产物后缀猜加载器，缺平台事实即失败闭合。
            let invocation = match requirement.target.specification {
                Some(CanonicalSpecification::Windows) => "LoadLibraryW + GetProcAddress",
                Some(CanonicalSpecification::Linux | CanonicalSpecification::MacOs | CanonicalSpecification::Android | CanonicalSpecification::Ios) => {
                    "dlopen + dlsym"
                }
                other => {
                    return Err(format!(
                        "native 共享库缺少可判定加载器的平台规格事实（specification={other:?}）；拒绝按产物后缀猜装载方式"
                    ));
                }
            };
            let library_exports = input
                .host_exports
                .iter()
                .map(|export| crate::DriverLibraryExport {
                    logical_name: export.public_name.clone(),
                    source_name: export.source_name.clone(),
                    physical_artifact: physical_artifact.clone(),
                    invocation: invocation.to_string(),
                    abi_digest: machine_export_abi_digest("c-abi", &export.public_name, &export.parameters, &export.parameter_names, &export.returns),
                    abi: "c-abi".to_string(),
                    parameters: Some(export.parameters.clone()),
                    parameter_names: Some(export.parameter_names.clone()),
                    returns: Some(export.returns.clone()),
                })
                .collect();
            return Ok(DriverCompileReport {
                artifacts,
                // 普通共享库没有单一入口：交付合同由 `library_exports` 的导出索引
                // 承载，不得拿某个导出冒充入口。
                entry_symbol: None,
                run_contracts: Vec::new(),
                managed_abi: Vec::new(),
                ovm_abi: Vec::new(),
                library_exports,
            });
        }
        // 原生可执行文件就是宿主入口：运行契约直接指向产物本身，不拼接命令前缀，
        // 逻辑入口取适配器冻结的入口符号；缺失是输入事实错误，不写空入口。
        let entry_symbol = entry_symbol.ok_or_else(|| "原生可执行交付缺少冻结的入口身份：计划未给出入口操作的 ABI 名".to_string())?;
        let run_contracts = artifacts
            .artifacts
            .first()
            .map(|artifact| DriverRunContract {
                logical_entry: entry_symbol.clone(),
                physical_entry: artifact.path.clone(),
                boundary: HostProjectionBoundary::MachineCode,
                // 原生入口包装把入口返回值交付为进程退出码（低 8 位截断）。
                return_projection: EntryReturnProjection::ProcessExitCode,
                // 原生可执行文件自己就是宿主入口：OS 直接启动该文件，无解释器前缀。
                launch: vec![artifact.path.clone()],
                validate: artifact.path.clone(),
            })
            .into_iter()
            .collect();
        Ok(DriverCompileReport {
            artifacts,
            entry_symbol: None,
            run_contracts,
            managed_abi: Vec::new(),
            ovm_abi: Vec::new(),
            library_exports: Vec::new(),
        })
    }
}
