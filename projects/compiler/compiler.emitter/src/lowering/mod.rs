use std::path::{Path, PathBuf};

use crate::{
    EmitterError, Result,
    emitter_backend_wasi::{WasmBinaryBackendInput, WasmBinaryModule},
};
use compiler_target::{ArtifactPurpose, HostProjectionBoundary, TargetBackendFamily};
#[cfg(feature = "ovm-lane")]
use data_binary_odin::OdinModuleData;

#[cfg(feature = "ovm-lane")]
use crate::OvmBackendInput;
use crate::{DriverBackendInput, FragmentSubmission};

#[doc(hidden)]
pub mod backends;
#[doc(hidden)]
pub mod features;
pub(crate) mod shared;

#[cfg(feature = "ovm-lane")]
use self::backends::{clr, jvm, ovm, wasm};
#[cfg(not(feature = "ovm-lane"))]
use self::backends::{clr, jvm, wasm};

pub(crate) fn testing_lower_fragment_to_wasm_mir_module(submission: &FragmentSubmission, export_name: &str) -> Result<WasmBinaryModule> {
    backends::wasm::mir::lower_fragment_mir_to_wasm_module(submission, export_name).map(|(module, _)| module)
}

pub(crate) fn testing_jvm_type_descriptor(
    layouts: &compiler_types::AggregateLayoutPlan,
    ty: &crate::backend_plan_views::SemanticType,
) -> Result<crate::emitter_backend_jvm::JvmTypeDescriptor> {
    jvm::emit::jvm_type_descriptor(layouts, ty)
}

pub(crate) fn testing_msil_type(
    layouts: &compiler_types::AggregateLayoutPlan,
    ty: &crate::backend_plan_views::SemanticType,
) -> Result<crate::emitter_backend_clr::MsilType> {
    clr::emit::msil_type(layouts, ty)
}

#[cfg(feature = "ovm-lane")]
pub(crate) fn testing_lower_fragment_to_ovm_module(submission: &FragmentSubmission) -> Result<OdinModuleData> {
    ovm::lower_fragment_to_ovm_module(submission)
}

pub(crate) fn testing_lower_fragment_to_jvm_class(submission: &FragmentSubmission) -> Result<crate::emitter_backend_jvm::JvmClassFile> {
    jvm::emit::lower_fragment_to_jvm_class(submission).map(|lowered| lowered.class_file)
}

pub(crate) fn testing_lower_fragment_to_jvm_managed_surface(
    submission: &FragmentSubmission,
) -> Result<(
    crate::emitter_backend_jvm::JvmClassFile,
    Vec<crate::emitter_backend_jvm::JvmClassFile>,
    Option<crate::ManagedAbiContract>,
    Option<String>,
)> {
    let lowered = jvm::emit::lower_fragment_to_jvm_class(submission)?;
    Ok((lowered.class_file, lowered.companion_classes, lowered.managed_abi, lowered.entry_launcher))
}

pub(crate) fn testing_lower_fragment_to_clr_module(submission: &FragmentSubmission) -> Result<crate::emitter_backend_clr::MsilModule> {
    clr::emit::lower_fragment_to_clr_module(submission).map(|lowered| lowered.module)
}

pub(crate) fn testing_lower_fragment_to_clr_managed_surface(
    submission: &FragmentSubmission,
) -> Result<(crate::emitter_backend_clr::MsilModule, Option<crate::ManagedAbiContract>)> {
    let lowered = clr::emit::lower_fragment_to_clr_module(submission)?;
    Ok((lowered.module, lowered.managed_abi))
}

pub(crate) fn testing_lower_fragment_to_machine_image(
    submission: &FragmentSubmission,
    target: compiler_target::abstractions::CanonicalTarget,
) -> Result<backends::machine::X64LoweredProgram> {
    backends::machine::lower_fragment_to_machine_image(submission, target)
}

pub(crate) fn testing_lower_fragment_to_machine_extension(
    submission: &FragmentSubmission,
    target: &backends::machine::X64ProductTarget,
    module_name: &str,
) -> Result<backends::machine::X64LoweredExtension> {
    backends::machine::lower_fragment_to_machine_extension(submission, target, module_name)
}

pub(crate) fn testing_decode_wasm_uleb128(bytes: &[u8], pos: &mut usize) -> u32 {
    wasm::decode_uleb128(bytes, pos)
}

pub(crate) fn testing_lower_fragment_to_wasm_module(
    submission: &FragmentSubmission,
    host_boundary: HostProjectionBoundary,
) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    wasm::lower_fragment_to_wasm_module(submission, host_boundary)
}

pub(crate) fn testing_mir_lowering_context(submission: &FragmentSubmission) -> Result<shared::executable::ExecutableLoweringContext<'_>> {
    shared::executable::ExecutableLoweringContext::new(submission, shared::physical_layout::LayoutPolicy::WASM32)
}

pub(crate) const TESTING_WASM_GC_ANYREF: u8 = wasm::WASM_GC_ANYREF;

pub(crate) fn testing_append_singleton_metadata_sections(module: &mut WasmBinaryModule, submission: &FragmentSubmission) {
    wasm::mir::singleton::append_singleton_metadata_sections(module, submission);
}

pub(crate) fn testing_augment_wasm_with_singleton_accessors(
    module: &mut WasmBinaryModule,
    submission: &FragmentSubmission,
) -> std::result::Result<(), features::semantic_mir_contract::SemanticMirContractError> {
    wasm::mir::singleton::augment_wasm_with_singleton_accessors(module, submission)
}

pub(crate) fn testing_singleton_metadata_line(plan: &compiler_types::SingletonInstancePlan) -> String {
    wasm::mir::singleton::singleton_metadata_line(plan)
}

/// 将已完成语义装配的片段降低为驱动层目标输入（集成测试专用）。
///
/// 公共边界返回结构化 [`crate::FragmentLoweringError`]：Semantic MIR 合同门禁
/// 有独立变体，其余目标降低/装配原因保留细节文本。
#[doc(hidden)]
pub fn lower_fragment_to_driver_input(
    submission: &FragmentSubmission,
    backend_family: TargetBackendFamily,
    host_boundary: HostProjectionBoundary,
    output_dir: PathBuf,
    host_flavor: &str,
    artifact_purpose: ArtifactPurpose,
    wasm_package_kind: crate::emitter_backend_wasi::WasmPackageKind,
) -> std::result::Result<DriverBackendInput, crate::FragmentLoweringError> {
    features::semantic_mir_contract::validate_submission(submission).map_err(crate::FragmentLoweringError::SemanticContract)?;
    build_driver_input(submission, backend_family, host_boundary, output_dir, host_flavor, artifact_purpose, wasm_package_kind)
        .map_err(crate::FragmentLoweringError::from)
}

fn build_driver_input(
    submission: &FragmentSubmission,
    backend_family: TargetBackendFamily,
    host_boundary: HostProjectionBoundary,
    output_dir: PathBuf,
    host_flavor: &str,
    artifact_purpose: ArtifactPurpose,
    wasm_package_kind: crate::emitter_backend_wasi::WasmPackageKind,
) -> Result<DriverBackendInput> {
    match backend_family {
        TargetBackendFamily::Wasm => {
            let wasi_preview = crate::emitter_backend_wasi::WasiPreview::from_host_flavor(host_flavor);
            let (module, imports) = wasm::lower_fragment_to_wasm_module_for(submission, host_boundary, wasi_preview, wasm_package_kind)?;
            // WASI command world 由冻结的产物用途决定：只有进程镜像（`Application` /
            // `TestBundle`）打包为 command component，`Library` 打包为库组件。
            // 禁止按「是否存在函数实例」猜测 command——库片段同样有函数实例。
            let package_as_wasi_command = host_boundary == HostProjectionBoundary::WasiComponent
                && matches!(artifact_purpose, ArtifactPurpose::Application | ArtifactPurpose::TestBundle);
            // 库导出签名只从已冻结事实派生：载体来自 `freeze_wasm_export_abi`（与
            // `nyar.library_invoke` 消费的槽位同源），形参名来自 canonical 参数身份。
            // 缺函数体或参数身份不合法都是结构化失败，不写 `None` 冒充未投影。
            // 只有库用途消费导出面；应用/测试镜像不得因库专属的载体约束而失败。
            let mut library_wasm_exports = Vec::new();
            if matches!(artifact_purpose, ArtifactPurpose::Library) {
                for (instance, source_name) in submission.backend_plan.export_names() {
                    let public_name = submission.backend_plan.export_public_name(instance).expect("导出实例必有公开名").clone();
                    let view = submission
                        .backend_plan
                        .get_function(instance)
                        .ok_or_else(|| format!("Wasm 库导出 `{public_name}` 缺少 Compiler 函数体，无法冻结导出签名"))?;
                    let abi = backends::wasm::abi::freeze_wasm_export_abi(&view.function)?;
                    let parameter_names = crate::abi_structure::canonical_parameter_identities(&public_name, &view.function)?
                        .into_iter()
                        .map(|(_, name)| name.to_string())
                        .collect::<Vec<_>>();
                    library_wasm_exports.push(crate::emitter_backend_wasi::WasmLibraryExport {
                        source_name: source_name.clone(),
                        public_name,
                        parameters: abi.params.iter().map(|carrier| carrier.as_str().to_string()).collect(),
                        parameter_names,
                        returns: abi.returns.as_str().to_string(),
                    });
                }
            }
            Ok(DriverBackendInput::Wasm(WasmBinaryBackendInput {
                module,
                output_dir,
                host_boundary,
                imports,
                package_as_wasi_command,
                wasi_preview,
                library_wasm_exports,
                wasm_package_kind,
            }))
        }
        #[cfg(feature = "ovm-lane")]
        TargetBackendFamily::Ovm => {
            let (module, abi_contract) = ovm::lower_fragment_to_ovm_module_with_abi(submission)?;
            Ok(DriverBackendInput::Ovm(OvmBackendInput {
                ovm_module: Some(module),
                abi_contract,
                library_public_exports: self::backends::ovm_mir::nyar_library_public_export_names(submission),
                output_dir,
            }))
        }
        TargetBackendFamily::Jvm => {
            let lowered = jvm::emit::lower_fragment_to_jvm_class(submission)?;
            Ok(DriverBackendInput::Jvm(crate::emitter_backend_jvm::JvmBinaryBackendInput {
                class_file: lowered.class_file,
                output_dir,
                emit_class_file: false,
                companion_classes: lowered.companion_classes,
                managed_abi: lowered.managed_abi,
                entry_launcher: lowered.entry_launcher,
                run_contracts: Vec::new(),
            }))
        }
        TargetBackendFamily::Clr => {
            let lowered = clr::emit::lower_fragment_to_clr_module(submission)?;
            // 库导出面自带合成入口点，仍必须写成 `.dll`；无导出的可执行镜像继续
            // 按 `is_entry_point` 推断 `.exe`。
            let image_kind = lowered.managed_abi.as_ref().map(|_| compiler_target::backends::clr::ClrImageKind::DynamicLibrary);
            Ok(DriverBackendInput::Clr(crate::emitter_backend_clr::ClrBinaryBackendInput {
                module: lowered.module,
                output_dir,
                image_kind,
                managed_abi: lowered.managed_abi,
                run_contracts: Vec::new(),
            }))
        }
        other => Err(format!("目标 `{other:?}` 在当前构建中没有已启用的正式 lowering；拒绝切换目标或补造产物")),
    }
}

pub(crate) fn write_wasm_wat_sidecar(output_dir: &Path, artifact_name: &str, input: &DriverBackendInput) -> Result<Option<PathBuf>> {
    let DriverBackendInput::Wasm(input) = input
    else {
        return Ok(None);
    };

    std::fs::create_dir_all(output_dir).map_err(|e| format!("failed to create WAT sidecar directory: {}: {e}", output_dir.display()))?;

    let sidecar_path = output_dir.join(format!("{artifact_name}.wat"));
    let wat_text = render_wasm_module_as_wat(&input.module);
    std::fs::write(&sidecar_path, wat_text).map_err(|e| format!("failed to write WAT sidecar: {}: {e}", sidecar_path.display()))?;
    Ok(Some(sidecar_path))
}

fn render_wasm_module_as_wat(module: &WasmBinaryModule) -> String {
    let mut output = String::from("(module");
    for section in &module.sections {
        output.push('\n');
        output.push_str("  ;; ");
        if section.id == 0 {
            let name = section.name.as_deref().unwrap_or("");
            output.push_str("custom section ");
            output.push_str(&format_quoted_text(name));
            if section.bytes.is_empty() {
                output.push_str(": empty");
            }
            else {
                output.push_str(": ");
                output.push_str(&format_wasm_payload(&section.bytes));
            }
        }
        else {
            output.push_str(&format!("section id {}: {} bytes", section.id, section.bytes.len()));
        }
    }
    output.push('\n');
    output.push(')');
    output
}

fn format_wasm_payload(bytes: &[u8]) -> String {
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" ");
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.is_empty() && text.chars().all(|ch| !ch.is_control()) => {
            format!("utf8 {} | hex [{}]", format_quoted_text(text), hex)
        }
        _ => format!("hex [{}]", hex),
    }
}

fn format_quoted_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

pub(crate) fn sanitize_symbol(name: &str) -> String {
    // Non-alphanumeric chars must stay distinguishable: `infix ==` and `infix !=`
    // must not both collapse to `infix___` (duplicate MethodDef → BadImage / PEVerify).
    // Keep `_` as-is so already-sanitized / snake_case symbols are stable.
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            sanitized.push(ch);
        }
        else if ch == '.' {
            sanitized.push('_');
        }
        else {
            sanitized.push_str(&format!("_x{:02x}", u32::from(ch)));
        }
    }
    if sanitized.is_empty() { "module".to_string() } else { sanitized }
}

/// Sanitize a resolved operation for MSIL MethodDef / Call names.
///
/// Joins [`QualifiedName`] parts with `.` (same shape as MIR instance symbols /
/// [`NamePath`] Display) before flattening, so `Option::is_none` and
/// `Option.is_none` both become `Option_is_none` — never `Option__is_none`.
pub(crate) fn sanitize_operation_symbol(operation: &compiler_target::QualifiedName) -> String {
    let dotted = operation.parts().iter().map(|part| part.as_str()).collect::<Vec<_>>().join(".");
    sanitize_symbol(&dotted)
}

/// JVM メソッド名用サニタイズ：`QualifiedName` パーツを `__` でジョインする。
///
/// `demo::main` → `["demo", "main"]` → join `__` → `"demo__main"`
/// `sanitize_operation_symbol`（`.` join → `demo_main`）とは区別する。
pub(crate) fn sanitize_jvm_method_symbol(operation: &compiler_target::QualifiedName) -> String {
    let underscored = operation.parts().iter().map(|part| part.as_str()).collect::<Vec<_>>().join("__");
    sanitize_symbol(&underscored)
}
