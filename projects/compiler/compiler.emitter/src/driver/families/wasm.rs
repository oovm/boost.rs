use crate::{
    DriverBackendInput, DriverCompileReport, DriverCompileRequest, DriverLibraryExport, DriverRunContract, Result,
    bundled_backend_capability_descriptor, emitter_backend_wasi::WasmBinaryBackend,
};
use compiler_target::{HostProjectionBoundary, PartitionBackendRequirement, TargetBackendFamily, backends::TargetCodeGenBackend};

use super::BundledBackendCompiler;

/// `WASM` 家族后端编译器，统一调度 `WasmJsGlue` 与 `WasiComponent` 两种宿主边界。
pub(super) struct WasmFamilyCompiler;

/// 判断给定的后端需求是否可由 `WASM` 家族满足。
pub(super) fn supports_requirement(requirement: &PartitionBackendRequirement) -> bool {
    bundled_backend_capability_descriptor(TargetBackendFamily::Wasm).is_some_and(|descriptor| descriptor.supports_requirement(requirement))
}

impl BundledBackendCompiler for WasmFamilyCompiler {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
        let DriverBackendInput::Wasm(input) = request.input
        else {
            return Err(format!("`WASM` 家族请求必须携带 `WasmBinaryBackendInput`"));
        };
        let expected_boundary = input.host_boundary;
        if request.requirement.host_boundary != expected_boundary {
            return Err(format!(
                "`WASM` 后端输入宿主与规划需求不一致：input={:?}, requirement={:?}",
                expected_boundary, request.requirement.host_boundary
            ));
        }
        // 未实现的用途组合必须在写出前拒绝：JS glue 的宿主交付身份是库导出面，
        // `WasiComponent` 的 command 世界只有 `_start`，两者都无法表达宿主扩展
        // 装载或逐 `[test]` 调用；先写产物再报错只会留下无法按名调用的文件。
        if request.purpose == compiler_target::ArtifactPurpose::HostExtension {
            return Err("`wasm` lane 不支持宿主扩展用途：JS glue 的交付身份是库导出面；请改用 `artifact: library`".to_string());
        }
        if request.purpose == compiler_target::ArtifactPurpose::TestBundle && expected_boundary == HostProjectionBoundary::WasiComponent {
            return Err(
                "`WasiComponent` 不支持测试包用途：command 世界只有 `_start`，无法按 `[test]` 名字逐条调用；拒绝写出只能整体启动的组件"
                    .to_string(),
            );
        }
        let backend = WasmBinaryBackend::new();
        backend.validate(&input).map_err(|error| error.to_string())?;
        let package_as_wasi_command = input.package_as_wasi_command;
        let wasi_preview = input.wasi_preview;
        let library_surface = request.purpose == compiler_target::ArtifactPurpose::Library;
        // `artifact: library`：导出通过 glue 的 `callExport` 调用，不产生可执行
        // run contract；同时记录每个导出的物理产物与 ABI digest。
        //
        // digest 与导出面在移动输入前取自冻结事实；物理产物必须在编码完成后从
        // 真实产物集合读取，不能按 `.wasm` 后缀手拼——`WasiComponent` 的库模块
        // 写的是 `.core.wasm`，手拼会指向不存在的文件。
        let library_spec = if library_surface {
            if input.library_wasm_exports.is_empty() {
                return Err("`artifact: library` 的 `wasm` 产物没有导出面".to_string());
            }
            Some((required_library_invoke_digest(&input.module)?, input.library_wasm_exports.clone()))
        }
        else {
            None
        };
        let mut artifacts = backend.compile(input, request.options).map_err(|error| error.to_string())?;
        let library_exports = match &library_spec {
            Some((abi_digest, exports)) => {
                let physical_artifact = artifacts
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.name == wasm_physical_artifact_name(request.artifact_name))
                    .map(|artifact| artifact.path.clone())
                    .ok_or_else(|| "`artifact: library` 的 `wasm` 后端没有写出与产物基名一致的模块产物".to_string())?;
                exports.iter().map(|export| project_library_export(export, &physical_artifact, abi_digest)).collect()
            }
            None => Vec::new(),
        };
        if library_surface {
            // 装载方式由用途决定：库交付的产物身份是宿主按导出名调用的模块。
            // 只有模块产物本身是库；同次写出的 `.mjs` 启动壳与组件绑定文件仍按
            // 各自真实角色标注，不能整批改标成 Library 误导下游打包器。
            let module_artifact_name = wasm_physical_artifact_name(request.artifact_name);
            for artifact in &mut artifacts.artifacts {
                if artifact.name == module_artifact_name {
                    artifact.kind = compiler_target::ArtifactKind::Library;
                }
            }
        }
        let run_contracts = if library_surface {
            Vec::new()
        }
        else {
            match request.requirement.host_boundary {
                HostProjectionBoundary::WasiComponent if !package_as_wasi_command => Vec::new(),
                boundary => vec![wasm_run_contract(request.artifact_name, boundary, wasi_preview)],
            }
        };
        Ok(DriverCompileReport { artifacts, entry_symbol: None, run_contracts, managed_abi: Vec::new(), ovm_abi: Vec::new(), library_exports })
    }
}

fn wasm_run_contract(
    artifact_name: &str,
    boundary: HostProjectionBoundary,
    wasi_preview: crate::emitter_backend_wasi::WasiPreview,
) -> DriverRunContract {
    let physical_name = wasm_physical_artifact_name(artifact_name);
    match boundary {
        HostProjectionBoundary::WasmJsGlue => DriverRunContract {
            logical_entry: "main".to_string(),
            physical_entry: format!("{}.mjs", physical_name),
            boundary: HostProjectionBoundary::WasmJsGlue,
            launch: vec!["node".to_string(), "{artifact}".to_string()],
            validate: format!("node {}.mjs", physical_name),
        },
        HostProjectionBoundary::WasiComponent => {
            let p3_flag = match wasi_preview {
                crate::emitter_backend_wasi::WasiPreview::Preview3 => " -S p3",
                crate::emitter_backend_wasi::WasiPreview::Preview2 => "",
            };
            // 运行契约只包含“在宿主上跑起来”所必需的选项：`-W gc` 是 wasm-gc
            // 语义前提，`-W max-memory-size` 是产品内存上限。`wmemcheck` 是
            // 诊断开关（stock wasmtime 默认未编译该特性），不能进入默认运行路径，
            // 否则声明的契约在标准宿主上直接失败。
            let mut launch = vec![
                "wasmtime".to_string(),
                "run".to_string(),
                "-W".to_string(),
                "gc".to_string(),
                "-W".to_string(),
                "max-memory-size=16777216".to_string(),
            ];
            if wasi_preview == crate::emitter_backend_wasi::WasiPreview::Preview3 {
                launch.push("-S".to_string());
                launch.push("p3".to_string());
            }
            launch.push("{artifact}".to_string());
            DriverRunContract {
                logical_entry: "_start".to_string(),
                physical_entry: format!("{}.wasi", physical_name),
                boundary: HostProjectionBoundary::WasiComponent,
                launch,
                validate: format!("wasmtime run -W gc -W max-memory-size=16777216{p3_flag} {}.wasi", physical_name),
            }
        }
        other => unreachable!("unexpected wasm host boundary: {:?}", other),
    }
}

/// 从 `.wasm` 的 `nyar.library_invoke` 自定义段读取 ABI digest。
///
/// 该段由 lowering 在库模式下写入；本函数只读取已冻结事实，不重新推导。
fn library_invoke_digest(module: &crate::emitter_backend_wasi::WasmBinaryModule) -> Option<String> {
    let section = module.sections.iter().find(|section| section.name.as_deref() == Some("nyar.library_invoke"))?;
    let payload: serde_json::Value = serde_json::from_slice(&section.bytes).ok()?;
    payload.get("abi_digest")?.as_str().map(str::to_string)
}

/// 库交付清单要求真实的 ABI digest。
///
/// digest 是宿主校验 `nyar.library_invoke` 版本与形状的事实；段缺失、JSON 无效或
/// 字段缺失时不得以空串冒充已冻结指纹——空串会被消费侧当成“没有约束”而跳过校验。
fn required_library_invoke_digest(module: &crate::emitter_backend_wasi::WasmBinaryModule) -> Result<String> {
    library_invoke_digest(module).ok_or_else(|| {
        "`artifact: library` 的 `wasm` 模块缺少可解析的 `nyar.library_invoke` ABI digest：库导出清单不得以空 digest 冒充已冻结 ABI".to_string()
    })
}

/// 将 lowering 冻结的 wasm 库导出投影为驱动层清单条目。
///
/// 签名（载体标签 / 形参名 / 返回标签）已在 lowering 由冻结 ABI 定案，这里只做
/// 纯映射：`Some([])` 表示已冻结的零形参，绝不回落为 `None`。
fn project_library_export(
    export: &crate::emitter_backend_wasi::WasmLibraryExport,
    physical_artifact: &str,
    abi_digest: &str,
) -> DriverLibraryExport {
    DriverLibraryExport {
        logical_name: export.public_name.clone(),
        source_name: export.source_name.clone(),
        physical_artifact: physical_artifact.to_string(),
        invocation: "wasm callExport".to_string(),
        abi_digest: abi_digest.to_string(),
        abi: "wasm".to_string(),
        parameters: Some(export.parameters.clone()),
        parameter_names: Some(export.parameter_names.clone()),
        returns: Some(export.returns.clone()),
    }
}

/// 将逻辑产物名规范化为 `WASM` 物理产物名。
///
/// 通用规则：直接使用 `artifact_name` 作为物理产物名，不对任何项目名做特殊分支。
/// 此函数是 `DriverRunContract` 引用物理文件名的唯一来源，必须与
/// `backend::wasi::canonical_wasm_artifact_stem` 产出相同的结果。
fn wasm_physical_artifact_name(artifact_name: &str) -> String {
    artifact_name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(parameters: &[&str], parameter_names: &[&str], returns: &str) -> crate::emitter_backend_wasi::WasmLibraryExport {
        crate::emitter_backend_wasi::WasmLibraryExport {
            source_name: "two_sum".to_string(),
            public_name: "twoSum".to_string(),
            parameters: parameters.iter().map(|carrier| carrier.to_string()).collect(),
            parameter_names: parameter_names.iter().map(|name| name.to_string()).collect(),
            returns: returns.to_string(),
        }
    }

    /// 冻结签名必须原样投影为 `Some(...)`，不得回落为 `None`。
    #[test]
    fn library_export_projects_frozen_signature() {
        let projected = project_library_export(&export(&["i32", "handle"], &["nums", "target"], "i64"), "app.wasm", "digest");
        assert_eq!(projected.logical_name, "twoSum");
        assert_eq!(projected.source_name, "two_sum");
        assert_eq!(projected.parameters, Some(vec!["i32".to_string(), "handle".to_string()]));
        assert_eq!(projected.parameter_names, Some(vec!["nums".to_string(), "target".to_string()]));
        assert_eq!(projected.returns, Some("i64".to_string()));
        assert_eq!(projected.physical_artifact, "app.wasm");
        assert_eq!(projected.abi_digest, "digest");
    }

    /// 零形参是已冻结事实 `Some([])`，不是签名缺失 `None`。
    #[test]
    fn zero_parameter_export_stays_frozen_empty_vector() {
        let projected = project_library_export(&export(&[], &[], "void"), "lib.core.wasm", "");
        assert_eq!(projected.parameters, Some(Vec::new()));
        assert_eq!(projected.parameter_names, Some(Vec::new()));
        assert_eq!(projected.returns, Some("void".to_string()));
    }

    /// 库清单的 ABI digest 必须是真实事实：段缺失、字段缺失都失败闭合。
    #[test]
    fn missing_library_invoke_digest_fails_closed() {
        let module = crate::emitter_backend_wasi::WasmBinaryModule::new();
        let error = required_library_invoke_digest(&module).expect_err("缺失 `nyar.library_invoke` 不得默认空 digest");
        assert!(error.contains("nyar.library_invoke"), "{error}");

        let mut without_digest = crate::emitter_backend_wasi::WasmBinaryModule::new();
        without_digest.push_custom_section("nyar.library_invoke", br#"{"version":0}"#.to_vec());
        assert!(required_library_invoke_digest(&without_digest).is_err(), "缺 `abi_digest` 字段不得默认空 digest");
    }

    /// 合法段读出的 digest 原样投影，不做任何改写。
    #[test]
    fn library_invoke_digest_is_projected_verbatim() {
        let mut module = crate::emitter_backend_wasi::WasmBinaryModule::new();
        module.push_custom_section("nyar.library_invoke", br#"{"version":0,"abi_digest":"sha256-abc"}"#.to_vec());
        assert_eq!(required_library_invoke_digest(&module), Ok("sha256-abc".to_string()));
    }
}
