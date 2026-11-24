use crate::{EmitterError, Result};
use compiler_target::{BackendCandidate, BackendSelector, PartitionBackendRequirement};

use crate::{DriverCompileReport, DriverCompileRequest};

mod clr;
mod jvm;
mod machine;
#[cfg(feature = "ovm-lane")]
#[doc(hidden)]
pub mod ovm;
mod wasm;

trait BundledBackendCompiler: Sync {
    fn compile(&self, request: DriverCompileRequest<'_>) -> Result<DriverCompileReport>;
}

struct DriverCompilerRegistration {
    name: &'static str,
    priority: u16,
    supports: fn(&PartitionBackendRequirement) -> bool,
    compiler: &'static dyn BundledBackendCompiler,
}

impl DriverCompilerRegistration {
    fn candidate(&self, requirement: &PartitionBackendRequirement) -> Option<BackendCandidate> {
        (self.supports)(requirement).then(|| BackendCandidate {
            name: self.name.to_string(),
            requirement: requirement.clone(),
            priority: self.priority,
        })
    }
}

static CLR_COMPILER: clr::ClrFamilyCompiler = clr::ClrFamilyCompiler;
static JVM_COMPILER: jvm::JvmFamilyCompiler = jvm::JvmFamilyCompiler;
static MACHINE_COMPILER: machine::MachineFamilyCompiler = machine::MachineFamilyCompiler;
static WASM_COMPILER: wasm::WasmFamilyCompiler = wasm::WasmFamilyCompiler;
#[cfg(feature = "ovm-lane")]
static OVM_COMPILER: ovm::OvmFamilyCompiler = ovm::OvmFamilyCompiler;

#[cfg(feature = "ovm-lane")]
static DRIVER_COMPILERS: [DriverCompilerRegistration; 5] = [
    DriverCompilerRegistration { name: "clr-binary", priority: 100, supports: clr::supports_requirement, compiler: &CLR_COMPILER },
    DriverCompilerRegistration { name: "jvm-jar", priority: 100, supports: jvm::supports_requirement, compiler: &JVM_COMPILER },
    DriverCompilerRegistration { name: "machine-code", priority: 100, supports: machine::supports_requirement, compiler: &MACHINE_COMPILER },
    DriverCompilerRegistration { name: "wasm-binary", priority: 100, supports: wasm::supports_requirement, compiler: &WASM_COMPILER },
    DriverCompilerRegistration { name: "ovm", priority: 100, supports: ovm::supports_requirement, compiler: &OVM_COMPILER },
];

#[cfg(not(feature = "ovm-lane"))]
static DRIVER_COMPILERS: [DriverCompilerRegistration; 4] = [
    DriverCompilerRegistration { name: "clr-binary", priority: 100, supports: clr::supports_requirement, compiler: &CLR_COMPILER },
    DriverCompilerRegistration { name: "jvm-jar", priority: 100, supports: jvm::supports_requirement, compiler: &JVM_COMPILER },
    DriverCompilerRegistration { name: "machine-code", priority: 100, supports: machine::supports_requirement, compiler: &MACHINE_COMPILER },
    DriverCompilerRegistration { name: "wasm-binary", priority: 100, supports: wasm::supports_requirement, compiler: &WASM_COMPILER },
];

/// 返回 bundled driver 编译器注册名（集成测试专用）。
#[doc(hidden)]
pub fn driver_compiler_names() -> Vec<&'static str> {
    DRIVER_COMPILERS.iter().map(|registration| registration.name).collect()
}

pub(crate) fn compile(request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
    let purpose = request.purpose;
    let mut selector = BackendSelector::default();
    let mut matched_compilers = Vec::new();
    for registration in DRIVER_COMPILERS.iter() {
        if let Some(candidate) = registration.candidate(&request.requirement) {
            selector.register(candidate);
            matched_compilers.push((registration.name, registration.compiler));
        }
    }

    let Some(selected) = selector.select(&request.requirement)
    else {
        return Err(format!("`emitter` 找不到满足需求的 backend 编译器：{:?}", request.requirement));
    };
    let Some((_, compiler)) = matched_compilers.into_iter().find(|(name, _)| *name == selected.name)
    else {
        return Err(format!("选中的 backend `{}` 没有关联 driver compiler", selected.name));
    };
    let report = compiler.compile(request)?;
    validate_export_surface(purpose, &report)?;
    Ok(report)
}

/// 库/扩展导出面的共享后置条件。
///
/// 各 lane 自己构造 `library_exports`，但「哪种用途带导出面、导出签名必须自洽」
/// 只在这里统一判定，避免 lane 各自放宽或漂移。非库用途携带导出面、库用途没有
/// 导出面、以及签名缺项/长度不一致都在写出之后、返回驱动层之前失败闭合。
fn validate_export_surface(purpose: compiler_target::ArtifactPurpose, report: &DriverCompileReport) -> Result<()> {
    let carries_exports = matches!(
        purpose,
        compiler_target::ArtifactPurpose::Library | compiler_target::ArtifactPurpose::HostExtension
    );
    if !carries_exports {
        if !report.library_exports.is_empty() {
            return Err(format!(
                "非库用途 `{purpose:?}` 却产出了 {} 条库导出清单；用途与导出面必须一致",
                report.library_exports.len()
            ));
        }
        return Ok(());
    }
    if report.library_exports.is_empty() {
        return Err(format!("`{purpose:?}` 没有产出任何库导出清单条目；拒绝让产物在没有导出面时静默通过"));
    }
    for export in &report.library_exports {
        if export.logical_name.trim().is_empty() {
            return Err(format!("库导出清单条目缺少公开名（源码名 `{}`）", export.source_name));
        }
        if export.source_name.trim().is_empty() {
            return Err(format!("库导出清单条目 `{}` 缺少源码声明名", export.logical_name));
        }
        if export.physical_artifact.trim().is_empty() {
            return Err(format!("库导出清单条目 `{}` 缺少物理产物名", export.logical_name));
        }
        if export.invocation.trim().is_empty() {
            return Err(format!("库导出清单条目 `{}` 缺少调用方式", export.logical_name));
        }
        if export.abi.trim().is_empty() {
            return Err(format!("库导出清单条目 `{}` 缺少 ABI 合同标签", export.logical_name));
        }
        let (Some(parameters), Some(parameter_names), Some(_)) = (&export.parameters, &export.parameter_names, &export.returns)
        else {
            return Err(format!(
                "库导出清单条目 `{}` 缺少冻结签名事实：`parameters`/`parameter_names`/`returns` 必须同时给出",
                export.logical_name
            ));
        };
        if parameters.len() != parameter_names.len() {
            return Err(format!(
                "库导出清单条目 `{}` 的形参载体数 {} 与形参名数 {} 不一致",
                export.logical_name,
                parameters.len(),
                parameter_names.len()
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for name in parameter_names {
            if name.trim().is_empty() {
                return Err(format!("库导出清单条目 `{}` 存在空形参名", export.logical_name));
            }
            if !seen.insert(name) {
                return Err(format!("库导出清单条目 `{}` 的形参名 `{name}` 重复", export.logical_name));
            }
        }
    }
    if report.artifacts.artifacts.is_empty() {
        return Err(format!("`{purpose:?}` 声明了导出面却没有产出任何物理产物"));
    }
    Ok(())
}

#[cfg(test)]
mod export_surface_tests {
    use super::*;
    use crate::DriverLibraryExport;
    use compiler_target::ArtifactPurpose;

    fn export(name: &str) -> DriverLibraryExport {
        DriverLibraryExport {
            logical_name: name.to_string(),
            source_name: name.to_string(),
            physical_artifact: "lib.wasm".to_string(),
            invocation: "wasm callExport".to_string(),
            abi_digest: String::new(),
            abi: "wasm".to_string(),
            parameters: Some(vec!["i32".to_string()]),
            parameter_names: Some(vec!["value".to_string()]),
            returns: Some("i32".to_string()),
        }
    }

    fn report(exports: Vec<DriverLibraryExport>) -> DriverCompileReport {
        DriverCompileReport { library_exports: exports, ..DriverCompileReport::default() }
    }

    #[test]
    fn application_carrying_exports_is_rejected() {
        let error = validate_export_surface(ArtifactPurpose::Application, &report(vec![export("run")]))
            .expect_err("非库用途不得携带导出面");
        assert!(error.contains("用途与导出面必须一致"), "{error}");
    }

    #[test]
    fn test_bundle_carrying_exports_is_rejected() {
        let error = validate_export_surface(ArtifactPurpose::TestBundle, &report(vec![export("t")]))
            .expect_err("测试包不属于库导出面");
        assert!(error.contains("用途与导出面必须一致"), "{error}");
    }

    #[test]
    fn library_without_exports_is_rejected() {
        let error = validate_export_surface(ArtifactPurpose::Library, &report(Vec::new())).expect_err("库用途必须有导出面");
        assert!(error.contains("没有产出任何库导出清单条目"), "{error}");
    }

    #[test]
    fn library_export_without_signature_is_rejected() {
        let mut entry = export("run");
        entry.parameter_names = None;
        let error = validate_export_surface(ArtifactPurpose::Library, &report(vec![entry])).expect_err("缺少签名事实必须失败");
        assert!(error.contains("缺少冻结签名事实"), "{error}");
    }

    #[test]
    fn library_export_with_parameter_name_count_mismatch_is_rejected() {
        let mut entry = export("run");
        entry.parameters = Some(vec!["i32".to_string(), "i64".to_string()]);
        let error = validate_export_surface(ArtifactPurpose::Library, &report(vec![entry])).expect_err("载体数与形参名数必须一致");
        assert!(error.contains("不一致"), "{error}");
    }

    #[test]
    fn library_export_with_blank_or_duplicate_parameter_name_is_rejected() {
        let mut blank = export("run");
        blank.parameter_names = Some(vec!["  ".to_string()]);
        let error = validate_export_surface(ArtifactPurpose::Library, &report(vec![blank])).expect_err("空形参名必须失败");
        assert!(error.contains("空形参名"), "{error}");

        let mut duplicate = export("run");
        duplicate.parameters = Some(vec!["i32".to_string(), "i64".to_string()]);
        duplicate.parameter_names = Some(vec!["value".to_string(), "value".to_string()]);
        let error = validate_export_surface(ArtifactPurpose::Library, &report(vec![duplicate])).expect_err("重复形参名必须失败");
        assert!(error.contains("重复"), "{error}");
    }
}
