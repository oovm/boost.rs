use std::{fs, path::Path};

use crate::{
    DriverBackendInput, DriverCompileReport, DriverCompileRequest, DriverLibraryExport, DriverRunContract, OvmAbiSidecar, Result,
    bundled_backend_capability_descriptor, emitter_backend_vm::emit_ovm_module, ovm_abi::OvmAbiContract,
};
use compiler_target::{
    ArtifactDescriptor, ArtifactFormat, ArtifactKind, ArtifactSet, EntryReturnProjection, HostProjectionBoundary, PartitionBackendRequirement,
    TargetBackendFamily, TargetLane,
};
use data_binary_odin::{OdinExportKind, OdinModuleData, decode_module};

use super::BundledBackendCompiler;

pub(super) struct OvmFamilyCompiler;

pub(super) fn supports_requirement(requirement: &PartitionBackendRequirement) -> bool {
    bundled_backend_capability_descriptor(TargetBackendFamily::Ovm).is_some_and(|descriptor| descriptor.supports_requirement(requirement))
}

impl BundledBackendCompiler for OvmFamilyCompiler {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
        let DriverBackendInput::Ovm(input) = request.input
        else {
            return Err("`ovm` 家族请求必须携带 `OvmBackendInput`".to_string());
        };
        // 用途承载能力在驱动层按 descriptor 统一裁决（见 families::compile），
        // 这里只按已裁决的用途决定容器与导出面。

        let mut artifacts = ArtifactSet::default();
        let mut entry_symbol = None;
        let mut run_contracts = Vec::new();
        let mut library_exports = Vec::new();
        let mut ovm_abi = Vec::new();

        fs::create_dir_all(&input.output_dir).map_err(|e| format!("创建输出目录失败：{}: {e}", input.output_dir.display()))?;

        if let Some(module) = input.ovm_module.as_ref() {
            let library_surface = request.purpose == compiler_target::ArtifactPurpose::Library;
            let entries = resolve_entry_symbols(module, &input.library_public_exports, library_surface)?;
            reject_legacy_output(&input.output_dir, request.artifact_name)?;
            let odin_name = format!("{}.odin", request.artifact_name);
            let odin_path = input.output_dir.join(&odin_name);
            emit_ovm_module(module, &odin_path)?;
            // sidecar 冻结产物**整体**哈希：布局表、常量池与代码段都参与 ABI 解释，
            // 挑字段计算迟早漏掉参与解释的内容。
            let odin_bytes =
                fs::read(&odin_path).map_err(|error| format!("读取刚写出的 `.odin` 产物失败：{}: {error}", odin_path.display()))?;
            artifacts.push(ArtifactDescriptor {
                name: odin_name.clone(),
                path: format!("{}.odin", request.artifact_name),
                // 装载方式由用途决定：库交付按导出名被宿主调用，不是直接启动的程序。
                kind: if library_surface { ArtifactKind::Library } else { ArtifactKind::Executable },
                format: ArtifactFormat::RawBinary,
                target: request.options.target.clone(),
                lane: TargetLane::Vm,
            });

            // 应用交付的唯一入口就是产物身份；库交付的身份是导出索引，测试包的
            // 身份是逐测试运行合同——后两者都不得拿清单里的第一个导出冒充「入口」。
            let has_single_process_entry =
                !library_surface && request.purpose != compiler_target::ArtifactPurpose::TestBundle;
            entry_symbol = entry_symbol_for(has_single_process_entry, &entries);
            if library_surface {
                // 库导出面必须携带冻结的 ABI 合同；缺失即失败闭合，绝不写出
                // 无合同的导出索引让宿主按未校验的布局解释内存。
                let contract =
                    input.abi_contract.as_ref().ok_or_else(|| "`artifact: library` 缺少 OVM ABI 合同；拒绝产出无合同的库导出面".to_string())?;
                library_exports = entries
                    .iter()
                    .map(|entry| library_export_from_contract(entry, request.artifact_name, contract))
                    .collect::<Result<Vec<_>>>()?;
            }
            // sidecar 与 `.odin` 同目录落盘，每个物理产物各持一份；宿主读取时先按整份产物重算哈希。
            if let Some(contract) = input.abi_contract.as_ref() {
                ovm_abi.push(OvmAbiSidecar::new(contract.clone(), odin_name.clone(), &odin_bytes));
            }
            for entry in entries {
                run_contracts.push(ovm_run_contract(request.artifact_name, &entry));
            }
        }

        Ok(DriverCompileReport { artifacts, entry_symbol, run_contracts, managed_abi: Vec::new(), ovm_abi, library_exports })
    }
}

/// 把单个入口的冻结合同投影为驱动层库导出清单项。
///
/// 签名事实（形参名、形参载体、返回载体）只来自 `contract`：源码名/模块导出
/// 不参与签名推导。入口在合同里没有对应导出时失败闭合，绝不写 `None` 或空签名。
fn library_export_from_contract(entry: &str, artifact: &str, contract: &OvmAbiContract) -> Result<DriverLibraryExport> {
    let export = contract.export(entry).ok_or_else(|| {
        format!("`artifact: library` 的 OVM ABI 合同缺少导出 `{entry}` 的签名事实；拒绝产出无签名清单")
    })?;
    Ok(DriverLibraryExport {
        logical_name: entry.to_string(),
        // 两个身份轴各自取自合同冻结事实，不互相复制。
        source_name: export.source_name.clone(),
        physical_artifact: format!("{artifact}.odin"),
        invocation: "ovm run --entry".to_string(),
        abi_digest: contract.digest.clone(),
        abi: "ovm".to_string(),
        parameters: Some(export.params.iter().map(|param| param.carrier.clone()).collect()),
        parameter_names: Some(export.params.iter().map(|param| param.name.clone()).collect()),
        returns: Some(export.returns_carrier.clone()),
    })
}

/// 只有「单一进程入口」交付才取清单第一个导出作为入口身份。
///
/// 库交付的入口身份由导出索引承载，测试包的身份由逐测试运行合同承载，两者都
/// 没有单一进程入口。
fn entry_symbol_for(has_single_process_entry: bool, entries: &[String]) -> Option<String> {
    if has_single_process_entry { entries.first().cloned() } else { None }
}

#[doc(hidden)]
pub fn resolve_entry_symbols(module: &OdinModuleData, library_public_exports: &[String], library_mode: bool) -> Result<Vec<String>> {    if library_mode && library_public_exports.is_empty() {
        return Err("`artifact: library` 要求至少一个已解析的 `[export]`；拒绝回落 `main`".to_string());
    }
    let entries = if library_public_exports.is_empty() { vec!["main".to_owned()] } else { library_public_exports.to_vec() };
    for entry in &entries {
        let matches: Vec<_> =
            module.exports.iter().filter(|export| export.kind == OdinExportKind::Function && export.symbol_name == *entry).collect();
        if matches.len() != 1 {
            return Err(format!("Nyar 入口 `{entry}` 必须对应唯一明确函数导出"));
        }
        let index = usize::try_from(matches[0].function_index).map_err(|_| format!("Nyar 入口 `{entry}` 的函数索引无效"))?;
        if module.functions.get(index).is_none() {
            return Err(format!("Nyar 入口 `{entry}` 的函数索引越界"));
        }
    }
    Ok(entries)
}

/// 拒绝覆盖格式不兼容的既有 `.odin` 产物。
///
/// 不以扩展名判定旧产物：`.nyar` 已是正式语义程序集身份，与 `.odin` 执行字节码互不覆盖，
/// 输出目录里同时存在程序集不影响本次重建。
fn reject_legacy_output(output_dir: &Path, artifact_name: &str) -> Result<()> {
    let target = output_dir.join(format!("{artifact_name}.odin"));
    if target.is_file() {
        let bytes = fs::read(&target).map_err(|error| format!("读取既有 `.odin` 产物失败：{}: {error}", target.display()))?;
        decode_module(&bytes)
            .map_err(|error| format!("拒绝覆盖格式不兼容的既有 `.odin` 产物 `{}`：{error}；请删除后重建", target.display()))?;
    }
    Ok(())
}

fn ovm_run_contract(artifact_name: &str, entry: &str) -> DriverRunContract {
    DriverRunContract {
        logical_entry: entry.to_string(),
        physical_entry: format!("{artifact_name}.odin"),
        boundary: HostProjectionBoundary::Vm,
        // OVM 宿主在进程内调用入口，返回值是 VM 内的 i32 值，不经退出码/stdout 通道。
        return_projection: EntryReturnProjection::InProcessValue,
        launch: vec![
            "ovm".to_string(),
            "run".to_string(),
            "{artifact}".to_string(),
            "--entry".to_string(),
            entry.to_string(),
        ],
        validate: format!("ovm run {artifact_name}.odin --entry {entry}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ovm_abi::{OVM_ABI_SCHEMA_VERSION, OvmAbiExport, OvmAbiParam, OvmAbiType};

    fn contract_with(exports: Vec<OvmAbiExport>) -> OvmAbiContract {
        OvmAbiContract { schema_version: OVM_ABI_SCHEMA_VERSION, lane: "ovm".to_string(), exports, digest: "sha256-test".to_string() }
    }

    fn export(logical_name: &str, params: Vec<OvmAbiParam>, returns: OvmAbiType, returns_carrier: &str) -> OvmAbiExport {
        OvmAbiExport {
            logical_name: logical_name.to_string(),
            source_name: logical_name.to_string(),
            params,
            returns,
            returns_carrier: returns_carrier.to_string(),
        }
    }

    #[test]
    fn only_single_process_entry_deliveries_claim_first_export_as_entry() {
        let entries = vec!["greet".to_string(), "tick".to_string()];
        assert_eq!(entry_symbol_for(false, &entries), None, "库/测试包交付不得拿第一个导出冒充入口");
        assert_eq!(entry_symbol_for(true, &entries), Some("greet".to_string()), "应用交付保留唯一入口身份");
    }

    #[test]
    fn library_export_projects_signature_from_contract() {
        let contract = contract_with(vec![export(
            "greet",
            vec![
                OvmAbiParam {
                    index: 0,
                    name: "name".to_string(),
                    ty: OvmAbiType::Text { representation: crate::ovm_abi::OvmTextRepresentation::Raw },
                    carrier: "text".to_string(),
                },
                OvmAbiParam { index: 1, name: "count".to_string(), ty: OvmAbiType::I64, carrier: "i64".to_string() },
            ],
            OvmAbiType::Bool,
            "i32",
        )]);

        let projected = library_export_from_contract("greet", "app", &contract).expect("合同内有导出即可投影");

        assert_eq!(projected.logical_name, "greet");
        assert_eq!(projected.source_name, "greet");
        assert_eq!(projected.physical_artifact, "app.odin");
        assert_eq!(projected.abi, "ovm");
        assert_eq!(projected.abi_digest, "sha256-test");
        assert_eq!(projected.parameters, Some(vec!["text".to_string(), "i64".to_string()]));
        assert_eq!(projected.parameter_names, Some(vec!["name".to_string(), "count".to_string()]));
        assert_eq!(projected.returns, Some("i32".to_string()));
    }

    #[test]
    fn zero_parameter_export_is_some_empty_not_none() {
        let contract = contract_with(vec![export("tick", Vec::new(), OvmAbiType::Unit, "void")]);

        let projected = library_export_from_contract("tick", "app", &contract).expect("零形参导出必须在合同中");

        assert_eq!(projected.parameters, Some(Vec::new()));
        assert_eq!(projected.parameter_names, Some(Vec::new()));
        assert_eq!(projected.returns, Some("void".to_string()));
    }

    #[test]
    fn missing_export_fails_closed_instead_of_empty_signature() {
        let contract = contract_with(vec![export("greet", Vec::new(), OvmAbiType::Unit, "void")]);

        let error = library_export_from_contract("absent", "app", &contract).expect_err("合同缺少导出时必须拒绝产出清单");

        assert!(error.contains("缺少导出 `absent`"), "must name the missing export: {error}");
        assert!(error.contains("拒绝产出无签名清单"), "must state refusal: {error}");
    }
}
