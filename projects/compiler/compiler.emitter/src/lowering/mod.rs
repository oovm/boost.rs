use std::path::{Path, PathBuf};

use crate::{Result, emitter_backend_wasi::WasmBinaryModule};
use compiler_target::{ArtifactPurpose, HostProjectionBoundary, TargetBackendFamily};
use crate::{DriverBackendInput, FragmentSubmission};

#[doc(hidden)]
pub mod backends;
#[doc(hidden)]
pub mod features;
pub(crate) mod shared;

use self::backends::wasm;

pub(crate) fn testing_decode_wasm_uleb128(bytes: &[u8], pos: &mut usize) -> u32 {
    wasm::decode_uleb128(bytes, pos)
}

pub(crate) fn testing_mir_lowering_context(submission: &FragmentSubmission) -> Result<shared::executable::ExecutableLoweringContext<'_>> {
    shared::executable::ExecutableLoweringContext::new(submission, shared::physical_layout::LayoutPolicy::WASM32)
}

pub(crate) const TESTING_WASM_GC_ANYREF: u8 = wasm::WASM_GC_ANYREF;

pub(crate) fn testing_singleton_metadata_line(plan: &compiler_types::SingletonInstancePlan) -> String {
    let fact = wasm::input::WasmSingletonFact {
        namespace: plan.namespace.clone(),
        name: plan.name.clone(),
        instance_field: plan.instance_field.clone(),
        is_lazy: plan.is_lazy,
        constructor_symbol: plan.constructor_symbol.clone(),
        finalizer_symbol: plan.finalizer_symbol.clone(),
        layout_size: None,
        layout_align: None,
        accessor_method: plan.accessor_method().to_string(),
        qualified_name: plan.qualified_name(),
        supports_unload: plan.supports_unload(),
    };
    wasm::mir::singleton::singleton_metadata_line(&fact)
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
    _submission: &FragmentSubmission,
    backend_family: TargetBackendFamily,
    _host_boundary: HostProjectionBoundary,
    _output_dir: PathBuf,
    _host_flavor: &str,
    _artifact_purpose: ArtifactPurpose,
    _wasm_package_kind: crate::emitter_backend_wasi::WasmPackageKind,
) -> Result<DriverBackendInput> {
    match backend_family {
        // WASM lane 已切到语言侧直出目标私有输入（`language.valkyrie` 的
        // `pipeline/wasm/`）：submission 公共 IR 解释阶段已删除，Wasm 家族不得
        // 再经本路径静默产出产物，显式失败闭合。
        TargetBackendFamily::Wasm => Err(format!(
            "WASM lane 已由语言侧直出目标私有输入，submission 公共路径不再承载 Wasm 家族；拒绝经 `lower_fragment_to_driver_input` 产出 WASM 产物"
        )),
        // OVM lane 已切到语言侧直出目标私有输入（`language.valkyrie` 的
        // `pipeline/ovm/`）：submission 公共 IR 解释阶段已删除，Ovm 家族不得
        // 再经本路径静默产出产物，显式失败闭合。
        TargetBackendFamily::Ovm => Err(format!(
            "OVM lane 已由语言侧直出目标私有输入，submission 公共路径不再承载 Ovm 家族；拒绝经 `lower_fragment_to_driver_input` 产出 OVM 产物"
        )),
        // JVM lane 已切到语言侧直出目标私有输入（`language.valkyrie` 的
        // `pipeline/jvm/`）：submission 公共 IR 解释阶段已删除，Jvm 家族不得
        // 再经本路径静默产出产物，显式失败闭合。
        TargetBackendFamily::Jvm => Err(format!(
            "JVM lane 已由语言侧直出目标私有输入，submission 公共路径不再承载 Jvm 家族；拒绝经 `lower_fragment_to_driver_input` 产出 JVM 产物"
        )),
        // CLR lane 已切到语言侧直出目标私有输入（`language.valkyrie` 的
        // `pipeline/clr/`）：submission 公共 IR 解释阶段已删除，Clr 家族不得
        // 再经本路径静默产出产物，显式失败闭合。
        TargetBackendFamily::Clr => Err(format!(
            "CLR lane 已由语言侧直出目标私有输入，submission 公共路径不再承载 Clr 家族；拒绝经 `lower_fragment_to_driver_input` 产出 CLR 产物"
        )),
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

pub fn sanitize_symbol(name: &str) -> String {
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
pub fn sanitize_operation_symbol(operation: &compiler_target::QualifiedName) -> String {
    let dotted = operation.parts().iter().map(|part| part.as_str()).collect::<Vec<_>>().join(".");
    sanitize_symbol(&dotted)
}

/// JVM メソッド名用サニタイズ：`QualifiedName` パーツを `__` でジョインする。
///
/// `demo::main` → `["demo", "main"]` → join `__` → `"demo__main"`
/// `sanitize_operation_symbol`（`.` join → `demo_main`）とは区別する。
pub fn sanitize_jvm_method_symbol(operation: &compiler_target::QualifiedName) -> String {
    let underscored = operation.parts().iter().map(|part| part.as_str()).collect::<Vec<_>>().join("__");
    sanitize_symbol(&underscored)
}
