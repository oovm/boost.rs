use crate::{EmitterError, Result, bundled_backend_capability_descriptor};
use compiler_target::{BackendCandidate, BackendSelector, PartitionBackendRequirement, TargetBackendFamily};

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
    family: TargetBackendFamily,
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
    DriverCompilerRegistration {
        name: "clr-binary",
        family: TargetBackendFamily::Clr,
        priority: 100,
        supports: clr::supports_requirement,
        compiler: &CLR_COMPILER,
    },
    DriverCompilerRegistration {
        name: "jvm-jar",
        family: TargetBackendFamily::Jvm,
        priority: 100,
        supports: jvm::supports_requirement,
        compiler: &JVM_COMPILER,
    },
    DriverCompilerRegistration {
        name: "machine-code",
        family: TargetBackendFamily::MachineCode,
        priority: 100,
        supports: machine::supports_requirement,
        compiler: &MACHINE_COMPILER,
    },
    DriverCompilerRegistration {
        name: "wasm-binary",
        family: TargetBackendFamily::Wasm,
        priority: 100,
        supports: wasm::supports_requirement,
        compiler: &WASM_COMPILER,
    },
    DriverCompilerRegistration { name: "ovm", family: TargetBackendFamily::Ovm, priority: 100, supports: ovm::supports_requirement, compiler: &OVM_COMPILER },
];

#[cfg(not(feature = "ovm-lane"))]
static DRIVER_COMPILERS: [DriverCompilerRegistration; 4] = [
    DriverCompilerRegistration {
        name: "clr-binary",
        family: TargetBackendFamily::Clr,
        priority: 100,
        supports: clr::supports_requirement,
        compiler: &CLR_COMPILER,
    },
    DriverCompilerRegistration {
        name: "jvm-jar",
        family: TargetBackendFamily::Jvm,
        priority: 100,
        supports: jvm::supports_requirement,
        compiler: &JVM_COMPILER,
    },
    DriverCompilerRegistration {
        name: "machine-code",
        family: TargetBackendFamily::MachineCode,
        priority: 100,
        supports: machine::supports_requirement,
        compiler: &MACHINE_COMPILER,
    },
    DriverCompilerRegistration {
        name: "wasm-binary",
        family: TargetBackendFamily::Wasm,
        priority: 100,
        supports: wasm::supports_requirement,
        compiler: &WASM_COMPILER,
    },
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
            matched_compilers.push(registration);
        }
    }

    let Some(selected) = selector.select(&request.requirement)
    else {
        return Err(format!("`emitter` 找不到满足需求的 backend 编译器：{:?}", request.requirement));
    };
    let Some(registration) = matched_compilers.into_iter().find(|registration| registration.name == selected.name)
    else {
        return Err(format!("选中的 backend `{}` 没有关联 driver compiler", selected.name));
    };
    // 用途承载能力只有 descriptor 一处声明；这里在 lane 写出任何产物之前统一
    // 裁决，lane 不再各自维护用途黑名单，新增用途时也不会漏掉某个 lane。
    let purpose_supported = bundled_backend_capability_descriptor(registration.family)
        .is_some_and(|descriptor| descriptor.supports_purpose(request.purpose, request.requirement.host_boundary));
    if !purpose_supported {
        return Err(format!(
            "后端 `{}`（家族 `{:?}`）不承载产物用途 `{:?}`（宿主边界 `{:?}`）；拒绝写出无法装载或无法逐入口调用的产物",
            registration.name, registration.family, request.purpose, request.requirement.host_boundary
        ));
    }
    let report = registration.compiler.compile(request)?;
    validate_export_surface(purpose, &report)?;
    validate_run_contract_surface(&report)?;
    Ok(report)
}

/// 本次报告真实产出的产物文件名集合。
///
/// 跨字段校验（导出索引、运行合同）只按文件名比对：各 lane 的产物路径有的用
/// 相对名、有的用完整路径，但同一产物在清单与产物集合中的文件名必须一致。
fn produced_artifact_file_names(report: &DriverCompileReport) -> std::collections::BTreeSet<String> {
    report
        .artifacts
        .artifacts
        .iter()
        .filter_map(|artifact| std::path::Path::new(&artifact.path).file_name().map(|name| name.to_string_lossy().into_owned()))
        .collect()
}

/// 运行合同的共享后置条件。
///
/// 各 lane 自己构造 `run_contracts`，但「合同指向的物理入口必须是本次真实产出的
/// 产物文件」以及「入口身份字段不得用空串冒充」只在这里统一判定。否则某个 lane
/// 改了产物命名之后，合同会继续指向不存在的文件，问题从构建期漂移到运行期。
/// 多测试入口的逐条合同同样经过这里：每条合同的物理入口都必须真实存在。
fn validate_run_contract_surface(report: &DriverCompileReport) -> Result<()> {
    if let Some(symbol) = &report.entry_symbol {
        if symbol.trim().is_empty() {
            return Err("入口符号是空串；缺失入口身份必须用 `None` 表达，不得用空串冒充".to_string());
        }
    }
    if report.run_contracts.is_empty() {
        return Ok(());
    }
    let produced = produced_artifact_file_names(report);
    for contract in &report.run_contracts {
        if contract.logical_entry.trim().is_empty() {
            return Err(format!("运行合同缺少逻辑入口名（物理入口 `{}`）", contract.physical_entry));
        }
        if contract.physical_entry.trim().is_empty() {
            return Err(format!("运行合同 `{}` 缺少物理入口", contract.logical_entry));
        }
        if contract.launch.is_empty() {
            return Err(format!("运行合同 `{}` 缺少启动 argv", contract.logical_entry));
        }
        if contract.launch.iter().any(|part| part.trim().is_empty()) {
            return Err(format!("运行合同 `{}` 的启动 argv 含空项", contract.logical_entry));
        }
        if contract.validate.trim().is_empty() {
            return Err(format!("运行合同 `{}` 缺少校验命令", contract.logical_entry));
        }
        let named = std::path::Path::new(&contract.physical_entry).file_name().map(|name| name.to_string_lossy().into_owned());
        if !matches!(named.as_deref(), Some(name) if produced.contains(name)) {
            return Err(format!(
                "运行合同 `{}` 指向物理入口 `{}`，但本次产物集合没有同名文件",
                contract.logical_entry, contract.physical_entry
            ));
        }
    }
    Ok(())
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
    // 导出索引必须指向本次真实产出的产物文件。只校验「非空字符串」会让清单在产物
    // 改名或换 lane 之后继续指向不存在的文件而无人发现；这里按文件名判定，因为各
    // lane 的 `physical_artifact` 有的是相对名、有的是完整路径。
    let produced = produced_artifact_file_names(report);
    for export in &report.library_exports {
        let named = std::path::Path::new(&export.physical_artifact).file_name().map(|name| name.to_string_lossy().into_owned());
        if !matches!(named.as_deref(), Some(name) if produced.contains(name)) {
            return Err(format!(
                "库导出清单条目 `{}` 指向物理产物 `{}`，但本次产物集合没有同名文件",
                export.logical_name, export.physical_artifact
            ));
        }
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

#[cfg(test)]
mod run_contract_surface_tests {
    use super::*;
    use crate::DriverRunContract;

    fn contract(entry: &str, physical: &str) -> DriverRunContract {
        DriverRunContract {
            logical_entry: entry.to_string(),
            physical_entry: physical.to_string(),
            boundary: compiler_target::HostProjectionBoundary::WasmJsGlue,
            launch: vec!["node".to_string(), "{artifact}".to_string()],
            validate: "node --check".to_string(),
        }
    }

    /// 产物集合里真实存在同名文件时，运行合同必须通过。
    fn report_with_artifact(path: &str, contracts: Vec<DriverRunContract>) -> DriverCompileReport {
        let artifact = compiler_target::packaging::ArtifactDescriptor {
            name: "app".to_string(),
            path: path.to_string(),
            kind: compiler_target::ArtifactKind::Executable,
            format: compiler_target::ArtifactFormat::RawBinary,
            target: compiler_target::BinaryTarget::new(
                compiler_target::TargetFamily::Wasm,
                compiler_target::BinaryArch::Any,
                compiler_target::BinaryFlavor::Mixed,
            ),
            lane: compiler_target::TargetLane::Wasm,
        };
        let mut report = DriverCompileReport::default();
        report.artifacts.artifacts.push(artifact);
        report.run_contracts = contracts;
        report
    }

    #[test]
    fn contract_pointing_to_produced_artifact_is_accepted() {
        validate_run_contract_surface(&report_with_artifact("dist/app.mjs", vec![contract("main", "app.mjs")]))
            .expect("合同指向真实产物时不应失败");
    }

    #[test]
    fn contract_pointing_to_unknown_artifact_is_rejected() {
        let error = validate_run_contract_surface(&report_with_artifact("dist/app.mjs", vec![contract("main", "missing.mjs")]))
            .expect_err("合同指向不存在产物必须失败");
        assert!(error.contains("没有同名文件"), "{error}");
        assert!(error.contains("missing.mjs"), "{error}");
    }

    #[test]
    fn contract_with_blank_fields_is_rejected() {
        let error = validate_run_contract_surface(&report_with_artifact("dist/app.mjs", vec![contract("main", "  ")]))
            .expect_err("空物理入口必须失败");
        assert!(error.contains("缺少物理入口"), "{error}");
    }

    #[test]
    fn contract_without_launch_argv_is_rejected() {
        let mut entry = contract("main", "app.mjs");
        entry.launch = Vec::new();
        let error = validate_run_contract_surface(&report_with_artifact("dist/app.mjs", vec![entry]))
            .expect_err("缺少启动 argv 必须失败");
        assert!(error.contains("缺少启动 argv"), "{error}");
    }

    #[test]
    fn empty_entry_symbol_is_rejected() {
        let mut report = report_with_artifact("dist/app.mjs", Vec::new());
        report.entry_symbol = Some("  ".to_string());
        let error = validate_run_contract_surface(&report).expect_err("空入口符号必须失败");
        assert!(error.contains("入口符号是空串"), "{error}");
    }
}

/// 用途承载能力必须来自 descriptor 这一处声明，并且每个已注册后端都能查到。
#[cfg(test)]
mod purpose_capability_tests {
    use super::*;
    use crate::bundled_backend_capability_descriptor;
    use compiler_target::{ArtifactPurpose, HostProjectionBoundary};

    fn descriptor(family: TargetBackendFamily) -> crate::BundledBackendCapabilityDescriptor {
        bundled_backend_capability_descriptor(family).expect("已注册的后端家族必须有 descriptor")
    }

    #[test]
    fn every_registered_compiler_family_declares_a_descriptor() {
        for registration in DRIVER_COMPILERS.iter() {
            assert_eq!(descriptor(registration.family).backend_name, registration.name);
        }
    }

    #[test]
    fn managed_families_carry_app_library_and_tests_but_not_host_extensions() {
        for family in [TargetBackendFamily::Clr, TargetBackendFamily::Jvm] {
            let descriptor = descriptor(family);
            for purpose in [ArtifactPurpose::Application, ArtifactPurpose::Library, ArtifactPurpose::TestBundle] {
                assert!(descriptor.supports_purpose(purpose, descriptor.supported_host_boundaries[0]), "{family:?} 应承载 {purpose:?}");
            }
            assert!(!descriptor.supports_purpose(ArtifactPurpose::HostExtension, descriptor.supported_host_boundaries[0]));
        }
    }

    #[test]
    fn machine_family_carries_host_extensions_but_not_test_bundles() {
        let descriptor = descriptor(TargetBackendFamily::MachineCode);
        assert!(descriptor.supports_purpose(ArtifactPurpose::HostExtension, HostProjectionBoundary::MachineCode));
        assert!(!descriptor.supports_purpose(ArtifactPurpose::TestBundle, HostProjectionBoundary::MachineCode));
    }

    #[test]
    fn wasm_test_bundle_support_depends_on_the_host_boundary() {
        let descriptor = descriptor(TargetBackendFamily::Wasm);
        assert!(descriptor.supports_purpose(ArtifactPurpose::TestBundle, HostProjectionBoundary::WasmJsGlue));
        assert!(!descriptor.supports_purpose(ArtifactPurpose::TestBundle, HostProjectionBoundary::WasiComponent));
        assert!(!descriptor.supports_purpose(ArtifactPurpose::HostExtension, HostProjectionBoundary::WasmJsGlue));
    }
}
