//! `WebAssembly` 二进制后端容器入口，覆盖 `WasmJsGlue` 与 `WasiComponent` 两种宿主边界。
//!
//! 这里按 `wasm / wat / wit` 三个输出格式收口。
//! `WASM` 二进制模型与编解码由 `data-binary-wasm` 提供；组件 `WIT` 文本由本模块直接生成。

#![deny(missing_docs)]

#[doc(hidden)]
pub mod component;
mod host_imports;
mod witness_dispatch;

pub(crate) use host_imports::{expand_wasi_cli_stream_imports, wasi_import_signature};

use std::{collections::BTreeSet, path::PathBuf};

use crate::{EmitterError, Result};
use compiler_target::{
    HostProjectionBoundary,
    abstractions::{ArtifactFormat, BackendInputKind, BinaryTarget},
    backends::{BackendDescriptor, CompilationOptions, TargetCodeGenBackend},
    packaging::{ArtifactDescriptor, ArtifactSet, TargetLane},
};

use crate::backend::binding_builders::{BindingGenerationContext, generate_host_binding_artifacts};
use data_binary_wasm::{parse_export_section, parse_import_section};

pub use component::WasiPreview;
pub(crate) use component::{
    WASI_PREVIEW2_VERSION, WASI_PREVIEW3_VERSION, WitBindingBuilder, package_core_wasm_as_component, wasi_adapt_import_for_preview,
    wasi_cli_run_export_name, wasi_cli_run_export_name_for, wasi_versioned_import_module, wasi_versioned_import_module_for,
    write_component_wit_package, write_component_wit_package_for,
};
pub use data_binary_wasm::{WasmBinaryError, WasmBinaryModule, WasmCustomSection, WasmSection};
pub use witness_dispatch::{
    WasmTraitFatPointer, WitnessMethodSlot, WitnessTableLayout, materialize_witness_bytes, plan_witness_table_layout, resolve_witness_call,
};

/// Returns the physical WASM/WASI artifact stem from the configured artifact
/// name. This boundary deliberately preserves the name verbatim: project or
/// library names are not semantic evidence and must never select a special
/// bootstrap path.
#[doc(hidden)]
pub fn canonical_wasm_artifact_stem(artifact_name: &str) -> String {
    artifact_name.to_string()
}

/// Whether Node wasm glue should expose `callExport` (library) or auto-run `main` (binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WasmPackageKind {
    /// CLI / command package: auto-invoke `main` or `_start` on import.
    #[default]
    Binary,
    /// Importable library: `callExport` only, no implicit entry execution.
    Library,
}

/// `WebAssembly` 二进制后端输入（覆盖 `WasmJsGlue` 与 `WasiComponent` 宿主边界）。
#[derive(Debug, Clone)]
pub struct WasmBinaryBackendInput {
    /// `WASM` 模块。
    pub module: WasmBinaryModule,
    /// 输出目录。
    pub output_dir: PathBuf,
    /// 宿主投影边界。
    pub host_boundary: HostProjectionBoundary,
    /// 导入声明列表（`(module, field)` 对），由宿主绑定生成器消费以生成对应的 `import` 实现。
    pub imports: Vec<(String, String)>,
    /// Whether this fragment's entry matches WASI command `run: func()` (nullary).
    ///
    /// Param-taking entries stay as core wasm only; command components are reserved
    /// for nullary entries that read argv via `wasi:cli`.
    pub package_as_wasi_command: bool,
    /// WASI package-train selection (`wasip2` / `wasip3`). Ignored for JS glue.
    pub wasi_preview: WasiPreview,
    /// 显式 `[export]` 导出面（源码名 + wasm 命名合同投影后的公开名）；library 模式允许无 `main` / `_start`。
    pub library_wasm_exports: Vec<WasmLibraryExport>,
    /// Manifest-selected wasm package mode (`binary` vs `library`).
    pub wasm_package_kind: WasmPackageKind,
    /// 启动壳必须调用的冻结入口导出名（二进制包）；库包与 WASI 为 `None`。
    ///
    /// 入口名由 lowering 按宿主边界冻结，宿主绑定生成器只消费、不按 `main` /
    /// `_start` 重新猜测。
    pub host_entry_export: Option<String>,
    /// 测试包逐入口映射：逻辑 `[test]` 名 → wasm 导出名。
    ///
    /// 映射只在 lowering 由 `TargetExecutionPlan` 的冻结公开名生成；JS 启动壳按
    /// argv 查表选择导出，绝不重新推导、也不回退到第一个导出。非测试包为空。
    pub test_entry_exports: Vec<WasmTestEntryExport>,
}

/// 测试包启动壳的一条冻结入口：逻辑测试名与其 wasm 导出名。
///
/// 两者都由 `TargetExecutionPlan::export_names` / `export_public_name` 冻结，
/// 启动壳只按 argv 逻辑名查表，不按名字猜导出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmTestEntryExport {
    /// 运行合同冻结的逻辑测试名（`[test]` 声明名）。
    pub logical_name: String,
    /// 该测试在 wasm 模块里的导出名。
    pub export_name: String,
}

/// wasm library 交付的一条导出：语言侧源码名、目标公开名与冻结的载体签名。
///
/// wasm 的命名合同是「有 `name:` / `case:` 注解用注解名，否则源码名」；
/// 清单与宿主调用面记录公开名，源码名保留用于跨目标追溯。签名事实全部来自
/// 已冻结的 Wasm 导出 ABI 载体合同（`freeze_wasm_export_abi`）与 canonical
/// 参数身份，不按类型名/字符串表重新推导。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmLibraryExport {
    /// 语言侧冻结的源码声明名。
    pub source_name: String,
    /// wasm 命名合同投影后的公开导出名。
    pub public_name: String,
    /// 按声明顺序排列的形参载体标签（`i32` / `i64` / `f32` / `f64` / `handle`）。
    ///
    /// 空向量表示已冻结的零形参，不是签名缺失。
    pub parameters: Vec<String>,
    /// 形参声明名（按声明顺序，与 `parameters` 等长）。
    pub parameter_names: Vec<String>,
    /// 返回载体标签（`i32` / `i64` / `f32` / `f64` / `handle` / `void`）。
    ///
    /// `void` 表示该导出没有返回值。
    pub returns: String,
}

/// `WebAssembly` 二进制后端（覆盖 `WasmJsGlue` 与 `WasiComponent` 宿主边界）。
pub struct WasmBinaryBackend {
    descriptor: BackendDescriptor,
}

impl WasmBinaryBackend {
    /// 创建一个新的 `WebAssembly` 二进制后端。
    pub fn new() -> Self {
        Self {
            descriptor: BackendDescriptor {
                name: "wasm-binary".to_string(),
                input_kind: BackendInputKind::WasmModule,
                supported_targets: vec![BinaryTarget::new(
                    compiler_target::TargetFamily::Wasm,
                    compiler_target::BinaryArch::Any,
                    compiler_target::BinaryFlavor::MachineCode,
                )],
            },
        }
    }
}

impl Default for WasmBinaryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmBinaryBackend {
    fn validate_artifact(&self, input: &WasmBinaryBackendInput) -> Result<()> {
        match input.host_boundary {
            HostProjectionBoundary::WasmJsGlue | HostProjectionBoundary::WasiComponent => {}
            other => {
                return Err(format!("`WASM` 后端只支持 `WasmJsGlue/WasiComponent`，实际得到 {:?}", other));
            }
        };

        // This is intentionally a physical gate, separate from Semantic MIR
        // validation: malformed section bytes or a stale glue-import list must
        // be rejected before any artifact is written.
        let bytes = input.module.to_bytes().map_err(|error| format!("WASM 预发射编码失败：{error}"))?;
        let decoded = WasmBinaryModule::from_bytes(&bytes).map_err(|error| format!("WASM 预发射结构验证失败：{error}"))?;
        let actual_imports: BTreeSet<_> = parse_import_section(&decoded).into_iter().map(|item| (item.module, item.field)).collect();
        for expected in &input.imports {
            let physical = wasi_core_import_name(expected, input.wasi_preview);
            if !actual_imports.contains(&physical) {
                return Err(format!("WASM glue import metadata declares `{}.{}`, but core module does not import it", expected.0, expected.1));
            }
        }
        let exports: BTreeSet<_> = parse_export_section(&decoded).into_iter().map(|item| item.name).collect();
        // 测试包没有单一 `main`：每个 `[test]` 的导出名由 lowering 冻结。这里按
        // 冻结事实逐个校验，缺任何一个都失败闭合，不回退到 `main` 或第一个导出。
        if input.host_boundary == HostProjectionBoundary::WasmJsGlue && !input.test_entry_exports.is_empty() {
            for entry in &input.test_entry_exports {
                if !exports.contains(&entry.export_name) {
                    return Err(format!(
                        "WASM pre-emission verifier: test entry `{}` maps to absent export `{}`",
                        entry.logical_name, entry.export_name
                    ));
                }
            }
            return Ok(());
        }
        let required_entry = match input.host_boundary {
            HostProjectionBoundary::WasmJsGlue => "main",
            HostProjectionBoundary::WasiComponent if input.package_as_wasi_command => "_start",
            HostProjectionBoundary::WasiComponent => return Ok(()),
            _ => unreachable!(),
        };
        if !exports.contains(required_entry) {
            if !input.library_wasm_exports.is_empty() && input.library_wasm_exports.iter().all(|export| exports.contains(&export.public_name)) {
                return Ok(());
            }
            return Err(format!("WASM pre-emission verifier: required `{required_entry}` function export is absent"));
        }
        Ok(())
    }

    fn compile_artifact(&self, input: WasmBinaryBackendInput, options: &CompilationOptions) -> Result<ArtifactSet> {
        std::fs::create_dir_all(&input.output_dir).map_err(|e| format!("创建输出目录失败：{}: {e}", input.output_dir.display()))?;

        let artifact_stem = canonical_wasm_artifact_stem(&options.artifact_name);
        let wasm_bytes = input.module.to_bytes().map_err(|error| format!("WASM 写入失败：{error}"))?;
        match input.host_boundary {
            HostProjectionBoundary::WasmJsGlue => {
                let wasm_path = input.output_dir.join(format!("{artifact_stem}.wasm"));
                std::fs::write(&wasm_path, wasm_bytes).map_err(|e| format!("写入 WASM 文件失败：{}: {e}", wasm_path.display()))?;
            }
            HostProjectionBoundary::WasiComponent => {
                let core_wasm_path = input.output_dir.join(format!("{artifact_stem}.core.wasm"));
                std::fs::write(&core_wasm_path, wasm_bytes)
                    .map_err(|e| format!("写入 core WASM 文件失败：{}: {e}", core_wasm_path.display()))?;
                // WASI command world requires nullary `run`. Param-taking entries
                // emit core only; a separate nullary entry that reads wasi:cli argv
                // becomes the `.wasi` command component.
                if input.package_as_wasi_command {
                    let component_path = input.output_dir.join(format!("{artifact_stem}.wasi"));
                    let wit_package_path =
                        write_component_wit_package_for(&input.output_dir, &artifact_stem, &input.imports, input.wasi_preview)?;
                    package_core_wasm_as_component(&core_wasm_path, &wit_package_path, &component_path)?;
                }
            }
            other => {
                return Err(format!("`WASM` 后端不支持的 host boundary：{:?}", other));
            }
        }

        let mut artifacts = ArtifactSet::default();
        let core_path = if matches!(input.host_boundary, HostProjectionBoundary::WasmJsGlue) {
            format!("{artifact_stem}.wasm")
        }
        else {
            format!("{artifact_stem}.core.wasm")
        };
        artifacts.push(ArtifactDescriptor {
            name: artifact_stem.clone(),
            path: core_path,
            kind: compiler_target::ArtifactKind::Executable,
            format: ArtifactFormat::RawBinary,
            target: options.target.clone(),
            lane: TargetLane::Wasm,
        });
        if matches!(input.host_boundary, HostProjectionBoundary::WasiComponent) && input.package_as_wasi_command {
            artifacts.push(ArtifactDescriptor {
                name: format!("{}.component", artifact_stem),
                path: format!("{artifact_stem}.wasi"),
                kind: compiler_target::ArtifactKind::Executable,
                format: ArtifactFormat::RawBinary,
                target: options.target.clone(),
                lane: TargetLane::Wasm,
            });
            artifacts.push(ArtifactDescriptor {
                name: format!("{}.component", artifact_stem),
                path: format!("{artifact_stem}.component-wit/component.wit"),
                kind: compiler_target::ArtifactKind::AssemblyListing,
                format: ArtifactFormat::RawBinary,
                target: options.target.clone(),
                lane: TargetLane::Wasm,
            });
        }

        let binding_artifacts = generate_host_binding_artifacts(
            input.host_boundary,
            BindingGenerationContext {
                artifact_name: &artifact_stem,
                output_dir: &input.output_dir,
                target: &options.target,
                imports: &input.imports,
                wasi_preview: input.wasi_preview,
                wasm_package_kind: input.wasm_package_kind,
                host_entry_export: input.host_entry_export.as_deref(),
                test_entry_exports: &input.test_entry_exports,
            },
        )?;
        for artifact in binding_artifacts {
            artifacts.push(artifact);
        }

        Ok(artifacts)
    }
}

impl TargetCodeGenBackend for WasmBinaryBackend {
    type Input = WasmBinaryBackendInput;

    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn validate(&self, input: &Self::Input) -> compiler_target::backends::BackendResult<()> {
        self.validate_artifact(input).map_err(|error| compiler_target::backends::BackendError::for_backend(&self.descriptor.name, error))
    }

    fn compile(&self, input: Self::Input, options: &CompilationOptions) -> compiler_target::backends::BackendResult<ArtifactSet> {
        self.compile_artifact(input, options)
            .map_err(|error| compiler_target::backends::BackendError::for_backend(&self.descriptor.name, error))
    }
}

fn wasi_core_import_name(import: &(String, String), preview: WasiPreview) -> (String, String) {
    if preview == WasiPreview::Preview2 {
        let bare = import.0.split('@').next().unwrap_or(import.0.as_str());
        let module = match bare {
            "wasi:clocks/monotonic-clock" => "cm32p2|wasi:clocks/monotonic-clock@0.2",
            "wasi:io/streams" => "cm32p2|wasi:io/streams@0.2",
            _ => return import.clone(),
        };
        return (module.to_string(), import.1.clone());
    }
    import.clone()
}
