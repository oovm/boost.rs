use crate::{
    DriverBackendInput, DriverCompileReport, DriverCompileRequest, DriverLibraryExport, DriverRunContract, Result,
    bundled_backend_capability_descriptor, emitter_backend_clr::ClrBinaryBackend,
};
use compiler_target::{
    BackendInputKind, HostProjectionBoundary, PartitionBackendRequirement, TargetBackendFamily, TargetFamily, TargetLane,
    backends::TargetCodeGenBackend,
};

use super::BundledBackendCompiler;

/// `CLR` 家族后端编译器：唯一宿主边界是 [`HostProjectionBoundary::Clr`]。
pub(super) struct ClrFamilyCompiler;

/// 判断给定的后端需求是否可由 `CLR` 家族满足。
pub(super) fn supports_requirement(requirement: &PartitionBackendRequirement) -> bool {
    bundled_backend_capability_descriptor(TargetBackendFamily::Clr).is_some_and(|descriptor| descriptor.supports_requirement(requirement))
}

/// 把 managed ABI 入口投影为驱动层库导出清单条目。
///
/// 签名事实（形参载体、形参声明名、返回载体）只来自 `entry`，不得用 `None`
/// 掩盖已冻结的签名；零形参必须投影为 `Some([])`。
fn library_export_from_entry(entry: &crate::managed_abi::ManagedAbiEntry, artifact: &str, contract_digest: &str) -> DriverLibraryExport {
    DriverLibraryExport {
        logical_name: entry.logical_entry.clone(),
        // clr lane 没有注解改名合同，公开名就是源码名。
        source_name: entry.logical_entry.clone(),
        physical_artifact: artifact.to_string(),
        invocation: "dotnet".to_string(),
        abi_digest: contract_digest.to_string(),
        abi: "clr-managed".to_string(),
        parameters: Some(entry.parameter_types.clone()),
        parameter_names: Some(entry.parameter_names.clone()),
        returns: Some(entry.return_type.clone()),
    }
}

impl BundledBackendCompiler for ClrFamilyCompiler {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
        let DriverBackendInput::Clr(mut input) = request.input
        else {
            return Err(format!("`CLR` 家族请求必须携带 `ClrBinaryBackendInput`"));
        };
        let requirement = &request.requirement;
        if requirement.lane != TargetLane::Clr {
            return Err(format!("`CLR` 后端需求 lane 不一致：requirement={:?}", requirement.lane));
        }
        if requirement.input_kind != BackendInputKind::MsilText {
            return Err(format!("`CLR` 后端需求输入种类不一致：requirement={:?}", requirement.input_kind));
        }
        if requirement.target.family != TargetFamily::Clr {
            return Err(format!("`CLR` 后端需求目标家族不一致：requirement={:?}", requirement.target.family));
        }
        if requirement.host_boundary != HostProjectionBoundary::Clr {
            return Err(format!("`CLR` 后端输入宿主与规划需求不一致：requirement={:?}", requirement.host_boundary));
        }
        let has_entry_point = input.module.global_methods.iter().any(|method| method.is_entry_point);
        let provided_contracts = std::mem::take(&mut input.run_contracts);
        let mut managed_abi = input.managed_abi.take();
        let library_surface = request.purpose == compiler_target::ArtifactPurpose::Library;
        // 镜像口味由用途决定：库交付固定 `.dll`；应用交付必须有冻结程序入口，
        // 不得由“有无导出/入口点”反推出库镜像；测试包按导出入口合同（有 ABI 即 dll）。
        if library_surface {
            input.image_kind = Some(compiler_target::backends::clr::ClrImageKind::DynamicLibrary);
        }
        else if managed_abi.is_some() && input.image_kind.is_none() {
            input.image_kind = Some(compiler_target::backends::clr::ClrImageKind::DynamicLibrary);
        }
        else if input.image_kind.is_none() && request.purpose == compiler_target::ArtifactPurpose::Application && !has_entry_point {
            return Err("`artifact: application` 的 `clr` 产物没有冻结程序入口；拒绝静默改发库镜像".to_string());
        }
        let image_kind = input.image_kind.unwrap_or_else(|| compiler_target::backends::clr::ClrImageKind::infer(has_entry_point));
        let artifact_file = format!("{}.{}", request.artifact_name, image_kind.file_extension());
        if let Some(contract) = managed_abi.as_mut() {
            // 产物文件名由驱动层决定；契约摘要必须覆盖最终文件名。
            contract.set_artifact(artifact_file.clone());
        }
        let backend = ClrBinaryBackend::new();
        backend.validate(&input).map_err(|error| error.to_string())?;
        // 库产物必须有导出面：缺失 managed ABI 入口时在写出前结构化失败，
        // 不得回退 `main`、静默返回空运行契约，也不得先写产物再判用途。
        if library_surface {
            match managed_abi.as_ref() {
                None => return Err("`artifact: library` 的 `clr` 产物缺少 managed ABI 契约；拒绝回退 `main`".to_string()),
                Some(contract) if contract.entries.is_empty() => {
                    return Err("`artifact: library` 的 `clr` managed ABI 契约为空；拒绝产出空运行契约".to_string());
                }
                Some(_) => {}
            }
        }
        let artifacts = backend.compile(input, request.options).map_err(|error| error.to_string())?;
        // managed ABI 入口逐个提供 argv 分发合同；无导出时才回退到单一 `main` 入口。
        let run_contracts = if !provided_contracts.is_empty() {
            provided_contracts
        }
        else if let Some(contract) = &managed_abi {
            contract
                .entries
                .iter()
                .map(|entry| DriverRunContract {
                    logical_entry: entry.logical_entry.clone(),
                    physical_entry: artifact_file.clone(),
                    invocation: "dotnet".to_string(),
                    validate: format!("dotnet exec {} {}", artifact_file, entry.logical_entry),
                })
                .collect()
        }
        else if has_entry_point {
            vec![DriverRunContract {
                logical_entry: "main".to_string(),
                physical_entry: artifact_file.clone(),
                invocation: "dotnet".to_string(),
                validate: format!("dotnet exec {}", artifact_file),
            }]
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
                        .map(|entry| library_export_from_entry(entry, &artifact_file, &contract.contract_digest))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_abi::ManagedAbiEntry;

    fn entry(parameter_types: &[&str], parameter_names: &[&str]) -> ManagedAbiEntry {
        ManagedAbiEntry {
            logical_entry: "sum".to_string(),
            owner: "_GlobalMethods".to_string(),
            method: "sum".to_string(),
            signature: "(int64,int64) int64".to_string(),
            parameter_types: parameter_types.iter().map(|ty| (*ty).to_string()).collect(),
            parameter_names: parameter_names.iter().map(|name| (*name).to_string()).collect(),
            return_type: "i64".to_string(),
            launcher_class: "managed_entry".to_string(),
        }
    }

    #[test]
    fn projects_managed_signature_into_library_export() {
        let export = library_export_from_entry(&entry(&["i64", "i64"], &["left", "right"]), "app.dll", "sha256-test");
        assert_eq!(export.parameters, Some(vec!["i64".to_string(), "i64".to_string()]));
        assert_eq!(export.parameter_names, Some(vec!["left".to_string(), "right".to_string()]));
        assert_eq!(export.returns, Some("i64".to_string()));
        assert_eq!(export.physical_artifact, "app.dll");
        assert_eq!(export.abi, "clr-managed");
        assert_eq!(export.abi_digest, "sha256-test");
    }

    #[test]
    fn projects_zero_parameter_signature_as_some_empty() {
        let export = library_export_from_entry(&entry(&[], &[]), "app.dll", "sha256-test");
        assert_eq!(export.parameters, Some(Vec::new()));
        assert_eq!(export.parameter_names, Some(Vec::new()));
    }
}
