#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

pub mod transitional;

mod artifacts;
pub mod assembly;
#[doc(hidden)]
pub mod backend;
mod driver;
#[doc(hidden)]
pub mod lowering;
pub mod target_operator;

mod error;
pub use error::{EmitterError, Result};

pub mod managed_abi;
pub use managed_abi::{MANAGED_ABI_SCHEMA_VERSION, ManagedAbiContract, ManagedAbiEntry, managed_abi_sidecar_file};

// OVM 库导出 ABI 合同 schema 归格式 crate；派生（语言语义裁决）已移入
// 语言 × OVM 适配器，emitter 只在 OVM family 写 sidecar 前做目标侧校验。
use data_binary_odin::{OvmAbiContract, OvmAbiSidecar};

use std::path::{Path, PathBuf};

use crate::{
    emitter_backend_clr::ClrBinaryBackendInput, emitter_backend_jvm::JvmBinaryBackendInput, emitter_backend_machine::MachineImageBackendInput,
    emitter_backend_wasi::WasmBinaryBackendInput,
};
pub use backend::{
    clr as emitter_backend_clr, jvm as emitter_backend_jvm, machine as emitter_backend_machine, vm as emitter_backend_vm,
    wasi as emitter_backend_wasi, wasm as emitter_backend_wasm, wasm_js_glue as emitter_backend_wasm_js_glue,
};
use compiler_target::PartitionBackendRequirement;
use compiler_target::{EntryReturnProjection, ProjectionPolicy};
use compiler_target::{
    ArtifactPurpose, BackendCapability, BackendInputKind, BackendInterpreterRegistration, BackendRegistry, BinaryTarget, CapabilityTag,
    HostProjectionBoundary, Identifier, ReferenceManagement, TargetBackendFamily, TargetFamily, TargetLane, TargetProfile, backends::CompilationOptions,
    packaging::ArtifactSet,
};

/// 已由 Compiler 绑定身份、签名和链接合同的导入。
#[derive(Debug, Clone)]
pub struct BackendImport {
    /// Canonical 导入索引。
    pub index: compiler_types::ImportIndex,
    /// 外部链接候选（声明顺序；target 裁决归各后端自己的 interop 函数）。
    pub links: Vec<compiler_types::ExternalImportLink>,
    /// 参数类型列表。
    pub parameter_types: Vec<compiler_types::SemanticType>,
    /// 返回类型。
    pub return_type: compiler_types::SemanticType,
}

/// 程序级文本载体表示模式：表示计划的正式事实。
///
/// raw/managed 的选择必须由计划生产者显式声明并冻结进目标输入，发射期与
/// ABI 合同只认这份声明；禁止由「布局注册表里没有 `utf8` 托管布局」反推
/// 程序选择了 raw——上游漏登记整类布局时，missing 会被吸收成合法表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramTextRepresentation {
    /// 程序显式选择 raw 宿主串表示；未逐槽冻结托管布局的文本槽才是合法 raw。
    Raw,
    /// 程序选择托管文本表示；没有逐值/逐槽载体事实的文本槽一律按 missing 拒收。
    Managed,
}

/// sum 的语言事实：variant 身份、discriminant 与 payload 类型由语言冻结。
///
/// tag 的物理宽度与槽位表示**不在**这份合同里——它是各 lane 自己的物理事实
/// （machine 4 字节 tag 槽、JVM `int` 字段、CLR `int32` 字段、Wasm GC `i32`
/// 字段、OVM i32 槽 0），由后端在自己的布局处冻结，不由编排层按 backend
/// family 预先取好传给语言适配器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumRepresentation {
    /// 完整的 sum 名义实例身份。
    pub nominal: compiler_types::NominalInstanceId,
    /// variant 身份到 tag/payload 合同的稀疏表。
    pub variants: std::collections::BTreeMap<compiler_types::VariantId, SumVariantRepresentation>,
}

/// 平台 sum variant 合同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SumVariantRepresentation {
    /// 语言稳定 variant 身份。
    pub id: compiler_types::VariantId,
    /// 平台 discriminant；不是语义身份。
    pub tag: u32,
    /// 已代入 payload 类型；无 payload 时为空。
    pub payload_type: Option<compiler_types::TypeId>,
}

use crate::driver::partitioning::{backend_family_for_partition, merge_partition_reports, partition_artifact_name};
use crate::lowering::write_wasm_wat_sidecar;

/// canonical 原语拼写的测试载体映射（仅单测使用）。
///
/// 生产路径的类型载体只来自 canonical 身份；测试需要一个与 canonical 词汇一致的
/// 构造入口，集中在这里避免各测试模块各写一份名字表。语言表面拼写（`byte`、
/// `Utf8Text` 等）刻意不在此支持。
#[cfg(test)]
pub(crate) fn test_canonical_primitive(spelling: &str) -> compiler_types::SemanticType {
    match spelling {
        "bool" => compiler_types::SemanticType::Boolean,
        "i8" => compiler_types::SemanticType::Integer8 { signed: true },
        "u8" => compiler_types::SemanticType::Integer8 { signed: false },
        "i16" => compiler_types::SemanticType::Integer16 { signed: true },
        "u16" => compiler_types::SemanticType::Integer16 { signed: false },
        "i32" => compiler_types::SemanticType::Integer32 { signed: true },
        "u32" => compiler_types::SemanticType::Integer32 { signed: false },
        "i64" => compiler_types::SemanticType::Integer64 { signed: true },
        "u64" => compiler_types::SemanticType::Integer64 { signed: false },
        "i128" => compiler_types::SemanticType::Integer128 { signed: true },
        "u128" => compiler_types::SemanticType::Integer128 { signed: false },
        "f32" => compiler_types::SemanticType::Float32,
        "f64" => compiler_types::SemanticType::Float64,
        "char" => compiler_types::SemanticType::Character,
        "utf8" => compiler_types::SemanticType::Utf8,
        "utf16" => compiler_types::SemanticType::Utf16,
        "unit" => compiler_types::SemanticType::Unit,
        "void" => compiler_types::SemanticType::Bottom,
        other => panic!("测试只支持 canonical 原语拼写: {other}"),
    }
}
pub use assembly::{AdaptedFragmentInput, AssembledFragment, TargetInputRequest};
pub use compiler_planning::ArtifactPartition;

/// Integration-test support wrappers for crate-internal lowering and compile flows.
#[doc(hidden)]
pub mod testing {
    use crate::Result;
    use compiler_target::{PartitionBackendRequirement, backends::CompilationOptions};

    use super::{DriverCompileReport, DriverCompileRequest, LoweredBackendInput, compile_with_bundled_backends};

    /// Decode one WASM uleb128 integer (test helper for section parsing).
    pub fn decode_wasm_uleb128(bytes: &[u8], pos: &mut usize) -> u32 {
        super::lowering::testing_decode_wasm_uleb128(bytes, pos)
    }

    /// Stable Node utf8 host import count used by wasm MIR contract tests.
    pub const NODE_UTF8_IMPORT_COUNT: u8 = 12;

    /// Re-export OVM entry resolution for driver-family contract tests.
    #[cfg(feature = "ovm-lane")]
    pub fn resolve_entry_symbols(module: &data_binary_odin::OdinModuleData, library_public_exports: &[String]) -> crate::Result<Vec<String>> {
        super::driver::families::ovm::resolve_entry_symbols(module, library_public_exports, false)
    }

    /// Compile a previously lowered backend input with bundled backends.
    pub fn compile_lowered_backend_input(
        artifact_name: &str,
        requirement: PartitionBackendRequirement,
        input: LoweredBackendInput,
        generate_runtime_config: bool,
        options: &CompilationOptions,
        purpose: compiler_target::ArtifactPurpose,
    ) -> Result<DriverCompileReport> {
        compile_with_bundled_backends(DriverCompileRequest {
            artifact_name,
            requirement,
            input: input.into_driver_backend_input(),
            generate_runtime_config,
            options,
            purpose,
        })
    }
}

/// bundled backend 解释器的显式 capability 描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledBackendCapabilityDescriptor {
    /// backend family。
    pub backend_family: TargetBackendFamily,
    /// backend 注册名。
    pub backend_name: &'static str,
    /// interpreter 标识。
    pub interpreter: &'static str,
    /// 对应 lane。
    pub lane: TargetLane,
    /// 产出的 backend 输入类型。
    pub input_kind: Option<BackendInputKind>,
    /// 对应目标家族。
    pub target_family: TargetFamily,
    /// 支持的宿主边界。
    pub supported_host_boundaries: &'static [HostProjectionBoundary],
    /// 支持的产物用途。
    ///
    /// 用途是家族级交付能力：家族能否承载该用途的装载/启动方式。逐用途的
    /// 语义前提（应用必须带入口、库必须带导出面）仍由 lane 自己判定，这里只回答
    /// 「这个家族是否承载该用途」。
    pub supported_purposes: &'static [ArtifactPurpose],
    /// 家族承载该用途时仍不成立的宿主边界例外。
    ///
    /// 用途是家族级交付能力，但少数宿主边界不具备该用途所需的启动形状。例外只在
    /// 这里声明一份，驱动层与分区编排都通过 `supports_purpose_on_boundary` 消费，
    /// lane 不再各写黑名单。
    pub purpose_boundary_exceptions: &'static [(ArtifactPurpose, HostProjectionBoundary)],
    /// 默认引用管理策略。
    pub reference_management: ReferenceManagement,
    /// 可接受的动态分发路由。
    pub backend_route: BackendRoute,
}

impl BundledBackendCapabilityDescriptor {
    /// 判断当前 descriptor 是否能消费指定的规划需求。
    pub fn supports_requirement(&self, requirement: &PartitionBackendRequirement) -> bool {
        requirement.lane == self.lane
            && self.input_kind.is_none_or(|kind| kind == requirement.input_kind)
            && requirement.target.family == self.target_family
            && self.supported_host_boundaries.contains(&requirement.host_boundary)
            && requirement.reference_management == self.reference_management
    }

    /// 判断该后端能否承载指定产物用途。
    ///
    /// 产物用途与「本后端已实现什么」是同一份能力事实，只有这里声明；驱动层与
    /// 分区编排都消费它来在写出前拒绝不支持的组合，lane 不再各自维护用途黑名单。
    pub fn supports_purpose(&self, purpose: ArtifactPurpose) -> bool {
        self.supported_purposes.contains(&purpose)
    }

    /// 判断该后端能否在指定宿主边界上承载产物用途。
    ///
    /// 承载能力 = 家族级用途能力 − 该边界的显式例外；例外只有 descriptor 一份。
    pub fn supports_purpose_on_boundary(&self, purpose: ArtifactPurpose, boundary: HostProjectionBoundary) -> bool {
        self.supports_purpose(purpose) && !self.purpose_boundary_exceptions.contains(&(purpose, boundary))
    }

    /// 将 descriptor 转成 backend 注册项。
    pub fn interpreter_registration(
        &self,
        fragment: Identifier,
        supported_projection_families: Vec<compiler_target::FutamuraProjectionFamily>,
        supported_targets: Vec<BinaryTarget>,
        required_capabilities: Vec<CapabilityTag>,
    ) -> BackendInterpreterRegistration {
        BackendInterpreterRegistration {
            backend_name: self.backend_name.to_string(),
            priority: 100,
            capability: BackendCapability {
                interpreter: Identifier::new(self.interpreter),
                fragment,
                lane: self.lane,
                input_kind: self.input_kind,
                supported_projection_families,
                supported_host_boundaries: self.supported_host_boundaries.to_vec(),
                supported_targets,
                required_capabilities,
                reference_management: Some(self.reference_management),
            },
        }
    }
}

const WASM_HOST_BOUNDARIES: &[HostProjectionBoundary] = &[HostProjectionBoundary::WasmJsGlue, HostProjectionBoundary::WasiComponent];
const JVM_HOST_BOUNDARIES: &[HostProjectionBoundary] = &[HostProjectionBoundary::Jvm];
const CLR_HOST_BOUNDARIES: &[HostProjectionBoundary] = &[HostProjectionBoundary::Clr];
const MACHINE_CODE_HOST_BOUNDARIES: &[HostProjectionBoundary] = &[HostProjectionBoundary::MachineCode];
#[cfg(feature = "ovm-lane")]
const VM_HOST_BOUNDARIES: &[HostProjectionBoundary] = &[HostProjectionBoundary::Vm];

/// managed 家族（`CLR`/`JVM`/`OVM`）承载的用途：应用、库与测试包。
///
/// 宿主扩展不在其中：这些产物没有宿主模块装载合同，无法给出装载方式。
const MANAGED_PURPOSES: &[ArtifactPurpose] =
    &[ArtifactPurpose::Application, ArtifactPurpose::Library, ArtifactPurpose::TestBundle];
/// wasm 家族承载的用途：应用、库与测试包。
///
/// 测试包只在 JS-glue 边界成立：模块按 `[test]` 冻结公开名逐个导出，启动壳按
/// argv 选择导出并逐测试产出运行合同。`WasiComponent` 没有该启动形状，因此以
/// descriptor 的边界例外表达，而非在 lane 里另写黑名单。
const WASM_PURPOSES: &[ArtifactPurpose] =
    &[ArtifactPurpose::Application, ArtifactPurpose::Library, ArtifactPurpose::TestBundle];
/// wasm 家族承载测试包时仍不成立的宿主边界。
const WASM_PURPOSE_BOUNDARY_EXCEPTIONS: &[(ArtifactPurpose, HostProjectionBoundary)] =
    &[(ArtifactPurpose::TestBundle, HostProjectionBoundary::WasiComponent)];
/// 机器码家族承载应用、库与宿主扩展；测试包要求逐 `[test]` 独立运行合同，
/// 而原生交付只有单一进程入口，因此不在支持面内。
const MACHINE_PURPOSES: &[ArtifactPurpose] =
    &[ArtifactPurpose::Application, ArtifactPurpose::Library, ArtifactPurpose::HostExtension];
/// 未登记 bundled driver 编译器的家族不承载任何用途。
const NO_PURPOSES: &[ArtifactPurpose] = &[];

/// 返回 bundled backend family 对应的显式 capability 描述。
pub fn bundled_backend_capability_descriptor(backend_family: TargetBackendFamily) -> Option<BundledBackendCapabilityDescriptor> {
    match backend_family {
        TargetBackendFamily::Jvm => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "jvm-jar",
            interpreter: "jvm.classfile",
            lane: TargetLane::Jvm,
            input_kind: Some(BackendInputKind::JvmClassFile),
            target_family: TargetFamily::Jvm,
            supported_host_boundaries: JVM_HOST_BOUNDARIES,
            supported_purposes: MANAGED_PURPOSES,
            purpose_boundary_exceptions: &[],

            reference_management: ReferenceManagement::HostGc,
            backend_route: BackendRoute::StaticOnly,
        }),
        TargetBackendFamily::Clr => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "clr-binary",
            interpreter: "clr.msil",
            lane: TargetLane::Clr,
            input_kind: Some(BackendInputKind::MsilText),
            target_family: TargetFamily::Clr,
            supported_host_boundaries: CLR_HOST_BOUNDARIES,
            supported_purposes: MANAGED_PURPOSES,
            purpose_boundary_exceptions: &[],

            reference_management: ReferenceManagement::HostGc,
            backend_route: BackendRoute::StaticOnly,
        }),
        TargetBackendFamily::MachineCode => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "machine-code",
            interpreter: "machine.image",
            lane: TargetLane::MachineCode,
            input_kind: Some(BackendInputKind::MachineImage),
            target_family: TargetFamily::MachineCode,
            supported_host_boundaries: MACHINE_CODE_HOST_BOUNDARIES,
            supported_purposes: MACHINE_PURPOSES,
            purpose_boundary_exceptions: &[],

            reference_management: ReferenceManagement::PerceusRc,
            backend_route: BackendRoute::StaticOnly,
        }),
        TargetBackendFamily::Wasm => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "wasm-binary",
            interpreter: "wasm.module",
            lane: TargetLane::Wasm,
            input_kind: Some(BackendInputKind::WasmModule),
            target_family: TargetFamily::Wasm,
            supported_host_boundaries: WASM_HOST_BOUNDARIES,
            supported_purposes: WASM_PURPOSES,
            purpose_boundary_exceptions: WASM_PURPOSE_BOUNDARY_EXCEPTIONS,

            reference_management: ReferenceManagement::HostGc,
            backend_route: BackendRoute::WitnessCapable,
        }),
        TargetBackendFamily::Gpu => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "gpu-spirv",
            interpreter: "gpu.spirv",
            lane: TargetLane::Gpu,
            input_kind: Some(BackendInputKind::SpirvModule),
            target_family: TargetFamily::Gpu,
            supported_host_boundaries: MACHINE_CODE_HOST_BOUNDARIES,
            supported_purposes: NO_PURPOSES,
            purpose_boundary_exceptions: &[],

            reference_management: ReferenceManagement::PerceusRc,
            backend_route: BackendRoute::StaticOnly,
        }),
        #[cfg(feature = "ovm-lane")]
        TargetBackendFamily::Ovm => Some(BundledBackendCapabilityDescriptor {
            backend_family,
            backend_name: "ovm",
            interpreter: "ovm.vm",
            lane: TargetLane::Vm,
            input_kind: Some(BackendInputKind::Ovm),
            target_family: TargetFamily::Ovm,
            supported_host_boundaries: VM_HOST_BOUNDARIES,
            supported_purposes: MANAGED_PURPOSES,
            purpose_boundary_exceptions: &[],

            reference_management: ReferenceManagement::HostGc,
            backend_route: BackendRoute::Full,
        }),
        #[cfg(not(feature = "ovm-lane"))]
        TargetBackendFamily::Ovm => None,
        TargetBackendFamily::Unknown => None,
    }
}

/// 基于 Canonical fragment 生成 bundled backend registry。
pub fn bundled_backend_registry_from_canonical(
    fragments: &std::collections::BTreeMap<compiler_types::Identifier, compiler_types::CanonicalFragment>,
    target_profile: &TargetProfile,
    projection_policy: &ProjectionPolicy,
) -> BackendRegistry {
    let mut registry = BackendRegistry::default();
    let binary_target: BinaryTarget = target_profile.canonical_target.into();
    if target_profile.backend_family == TargetBackendFamily::Gpu {
        for fragment in fragments.values() {
            compiler_target::backends::gpu::register_gpu_backends(
                &mut registry,
                fragment.id.clone(),
                vec![projection_policy.family],
                vec![binary_target.clone()],
                fragment.required_capabilities.clone(),
            );
        }
        return registry;
    }
    let Some(descriptor) = bundled_backend_capability_descriptor(target_profile.backend_family)
    else {
        return registry;
    };
    for fragment in fragments.values() {
        registry.register(descriptor.interpreter_registration(
            fragment.id.clone(),
            vec![projection_policy.family],
            vec![binary_target.clone()],
            fragment.required_capabilities.clone(),
        ));
    }
    registry
}

/// 后端路由可接受的动态分发能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendRoute {
    /// 仅接受静态分发。
    StaticOnly,
    /// 接受 witness 分发，不接受 effect handler。
    WitnessCapable,
    /// 接受全部已建模分发。
    Full,
}

/// backend boundary 可接受的分发形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendDispatchKind {
    /// 静态分发。
    Static,
    /// witness 分发。
    Witness,
    /// effect handler 分发。
    EffectHandler,
}

/// 驱动层在 backend boundary 前校验的输入形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendInputShape {
    /// 是否仍然包含未消解的开放 row 证据。
    pub contains_open_row_evidence: bool,
    /// 是否仍然包含未消解的名义检查。
    pub contains_unresolved_nominal_checks: bool,
}

/// backend boundary 校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendBoundaryError {
    /// backend 输入仍包含开放 row 证据。
    OpenRowEvidence,
    /// backend 输入仍包含未消解名义检查。
    UnresolvedNominalCheck,
    /// 当前路由不支持 trait witness 分发。
    UnsupportedTraitDispatch {
        /// 不支持的分发路由。
        route: BackendRoute,
    },
    /// 当前路由不支持 effect handler 分发。
    UnsupportedEffectDispatch {
        /// 不支持的分发路由。
        route: BackendRoute,
    },
}

/// 校验 backend 输入形状是否已经满足后端边界。
pub fn validate_backend_input(shape: BackendInputShape) -> std::result::Result<(), BackendBoundaryError> {
    if shape.contains_open_row_evidence {
        return Err(BackendBoundaryError::OpenRowEvidence);
    }
    if shape.contains_unresolved_nominal_checks {
        return Err(BackendBoundaryError::UnresolvedNominalCheck);
    }
    Ok(())
}

/// 校验指定路由是否支持当前分发形态。
pub fn validate_dispatch_for_route(route: BackendRoute, dispatch: BackendDispatchKind) -> std::result::Result<(), BackendBoundaryError> {
    match (route, dispatch) {
        (_, BackendDispatchKind::Static) => Ok(()),
        (BackendRoute::WitnessCapable | BackendRoute::Full, BackendDispatchKind::Witness) => Ok(()),
        (BackendRoute::Full, BackendDispatchKind::EffectHandler) => Ok(()),
        (_, BackendDispatchKind::Witness) => Err(BackendBoundaryError::UnsupportedTraitDispatch { route }),
        (_, BackendDispatchKind::EffectHandler) => Err(BackendBoundaryError::UnsupportedEffectDispatch { route }),
    }
}

/// 从片段能力标签推断 backend 分发形态。
/// 驱动层可消费的已规划分区视图。
pub trait PlannedArtifactPartitionsView {
    /// 返回主分区名。
    fn primary_partition_name(&self) -> Option<String>;

    /// 返回分区数量。
    fn partition_count(&self) -> usize;

    /// 返回指定分区。
    fn partition(&self, partition_index: usize) -> Option<&ArtifactPartition>;

    /// 返回指定分区已经完成规划的后端需求。
    fn backend_requirement(&self, partition_index: usize) -> Option<PartitionBackendRequirement>;
}

/// 前端提交给驱动层的最小构建 bundle 协议。
pub trait FrontendBuildBundle {
    /// 返回驱动层可消费的分区计划视图。
    fn planned_partitions(&self) -> &dyn PlannedArtifactPartitionsView;

    /// 为指定分区提交前端已完成语义闭合的装配结果。
    ///
    /// 前端必须显式声明接入能力：复用共享语义子集，或由自己的语言→平台适配器
    /// 准备目标输入。编排层不要求所有语言先转成同一份公共 MIR。
    fn assemble_fragment_for_partition(&self, partition_index: usize) -> Result<AssembledFragment<'_>>;

    /// 返回目标 profile 的 host flavor，供 Emitter 选择目标私有编码。
    fn target_host_flavor(&self) -> Option<String>;

    /// 产物用途：由语言侧显式声明的交付事实（应用 / 库 / 测试包 / 宿主扩展）。
    ///
    /// 各 lane 必须消费本事实决定入口、导出面与容器形态；禁止由 Wasm 打包枚举
    /// 或“有无导出”反推用途。
    fn artifact_purpose(&self) -> compiler_target::ArtifactPurpose {
        compiler_target::ArtifactPurpose::Application
    }

    /// Wasm glue package mode from manifest (`binary` runs `main`, `library` exposes `callExport`).
    ///
    /// 只描述 Wasm lane 的容器形态；全目标用途以 [`Self::artifact_purpose`] 为准。
    fn wasm_package_kind(&self) -> emitter_backend_wasi::WasmPackageKind {
        emitter_backend_wasi::WasmPackageKind::Binary
    }

    /// 已冻结的解析目标：平台身份与产物/宿主合同必须一起进入 lane。
    ///
    /// 编排层与适配器只消费这一份事实；不得先取四元组、再由下游猜产物是
    /// 可执行文件、Node 扩展还是 Python 扩展。
    fn resolved_target(&self) -> compiler_target::ResolvedTarget;

    /// 测试构建的 `[test]` 入口名；空表示普通构建。
    ///
    /// 名字来自前端语义闭包 seed 的显式 `[test]` 声明身份，Emitter 据此为每个测试
    /// 生成独立 run contract（`logical_entry` 必须等于该名字），runner 按名字 invoke。
    /// 禁止用函数名猜测或字符串拼接伪造入口身份。
    fn test_entry_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// First-class suspend backend input (ovm / native continuation runtime).
#[derive(Debug, Clone)]
pub struct OvmBackendInput {
    /// Optional OVM module payload to emit as `.odin`.
    pub ovm_module: Option<data_binary_odin::OdinModuleData>,
    /// 库导出 ABI 合同（含稳定 digest）；无导出面时为 `None`。
    pub abi_contract: Option<OvmAbiContract>,
    /// 库模式公开导出名（与 wasm `export_names` 对齐），用于 run-contract 入口。
    pub library_public_exports: Vec<String>,
    /// Output directory.
    pub output_dir: PathBuf,
}

/// 驱动层接收的目标专用输入（集成测试专用）。
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum DriverBackendInput {
    /// `CLR` 二进制输入。
    Clr(ClrBinaryBackendInput),
    /// `JVM` 二进制输入。
    Jvm(JvmBinaryBackendInput),
    /// `WASM/WASI` 二进制输入。
    Wasm(WasmBinaryBackendInput),
    /// 机器码镜像输入（`PE` / `ELF`，由真实四元组与产物合同决定）。
    Machine(MachineImageBackendInput),
    /// `ovm` first-class suspend 输入。
    Ovm(OvmBackendInput),
}

/// 前端已经完成的分区级 backend 输入。
#[derive(Debug, Clone)]
pub struct LoweredBackendInput {
    input: DriverBackendInput,
    entry_artifact_name: Option<String>,
}

impl LoweredBackendInput {
    pub(crate) fn new(input: DriverBackendInput) -> Self {
        Self { input, entry_artifact_name: None }
    }

    /// 直接提交 `CLR` backend 输入。
    pub fn clr(input: ClrBinaryBackendInput) -> Self {
        Self::new(DriverBackendInput::Clr(input))
    }

    /// 直接提交 `JVM` backend 输入。
    pub fn jvm(input: JvmBinaryBackendInput) -> Self {
        Self::new(DriverBackendInput::Jvm(input))
    }

    /// 直接提交 `WASM/WASI` backend 输入。
    pub fn wasm(input: WasmBinaryBackendInput) -> Self {
        Self::new(DriverBackendInput::Wasm(input))
    }

    /// 直接提交机器码 backend 输入。
    pub fn machine(input: MachineImageBackendInput) -> Self {
        Self::new(DriverBackendInput::Machine(input))
    }

    /// 覆盖产物基名（默认是工程名）。
    ///
    /// CPython 扩展的工程名与可导入模块名是两个事实：文件名、`PyInit_<module>`、
    /// 执行清单与宿主 `import` 必须共用归一化后的模块名。
    pub fn with_artifact_base_name(mut self, artifact_base_name: Option<String>) -> Self {
        if artifact_base_name.is_some() {
            self.entry_artifact_name = artifact_base_name;
        }
        self
    }

    /// 直接提交 `ovm` backend 输入。
    pub fn ovm(input: OvmBackendInput) -> Self {
        Self::new(DriverBackendInput::Ovm(input))
    }

    pub(crate) fn into_driver_backend_input(self) -> DriverBackendInput {
        self.input
    }

    pub(crate) fn entry_artifact_name(&self) -> Option<&str> {
        self.entry_artifact_name.as_deref()
    }
}

/// 驱动层编译请求。
#[derive(Debug)]
pub(crate) struct DriverCompileRequest<'a> {
    /// 逻辑产物名。
    pub artifact_name: &'a str,
    /// 已经完成规划的后端需求。
    pub requirement: PartitionBackendRequirement,
    /// 目标专用输入。
    pub input: DriverBackendInput,
    /// 是否生成 runtime config。
    pub generate_runtime_config: bool,
    /// 通用编译选项。
    pub options: &'a CompilationOptions,
    /// 产物用途：语言侧声明的交付事实，lane 据此冻结入口、导出面与装载方式。
    pub purpose: compiler_target::ArtifactPurpose,
}

/// 驱动层运行契约。
///
/// 消费侧只按本结构冻结的事实启动产物：`boundary` 给出宿主边界身份，
/// `launch` 给出可执行 argv，`validate` 给出人类/CI 可读的校验命令行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverRunContract {
    /// 逻辑入口名。
    pub logical_entry: String,
    /// 物理入口文件。
    pub physical_entry: String,
    /// 产物唯一的宿主边界身份；消费侧据此选择宿主，不按命令名或扩展名猜测。
    pub boundary: HostProjectionBoundary,
    /// 本条合同的入口返回值观测投影：由生成入口包装的后端按其实际包装策略冻结。
    ///
    /// 结果通道、值宽与解码规则是包装的事实，不是宿主边界的推论——同一边界上
    /// 不同包装可以选择不同通道，消费侧（Legion、e2e）只读本字段，不再派生。
    pub return_projection: EntryReturnProjection,
    /// 冻结的启动 argv：第 0 项是可执行文件，其余是参数。
    ///
    /// 只允许 `{artifact}` 占位符（由消费侧展开为解析后的物理入口路径）；
    /// 入口符号等 lane 专属事实必须在此写入具体值，不再留给消费侧猜。
    pub launch: Vec<String>,
    /// 校验命令。
    pub validate: String,
}

/// 驱动层库导出索引条目。
///
/// 只由 `artifact: library` 的构建产出，描述可被宿主按名调用的导出；
/// `abi_digest` 在 lane 尚无 ABI 载体合同时为空串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverLibraryExport {
    /// 逻辑导出名（目标命名合同投影后的公开名）。
    pub logical_name: String,
    /// 语言侧冻结的源码声明名；改名由各目标所有者的命名合同决定。
    pub source_name: String,
    /// 物理产物（文件名）。
    pub physical_artifact: String,
    /// 调用方式。
    pub invocation: String,
    /// ABI digest。
    pub abi_digest: String,
    /// 目标 ABI 合同标签（`node-api` / `cpython` / `c-abi` / `wasm` / `ovm` / `jvm-managed` / `clr-managed`）。
    pub abi: String,
    /// 形参载体标签（按形参顺序），词汇表由各 lane 的冻结 ABI 决定：
    /// `c-abi` 用 `i64` / `i64[]` / `utf8` 等，`wasm` 用 `i32` / `i64` / `f32` /
    /// `f64` / `handle`，托管 lane 用 `void` / `bool` / `char` / `i8`…`u64` /
    /// `f32` / `f64` / `string` 及其 `T[]` 数组形式，`ovm` 用其 carrier 名。
    /// 托管 lane 的这些标签是入口 ABI 事实，与能否用 argv 编组无关。
    ///
    /// `Some([])` 是已冻结的零形参签名，`None` 是该 lane 不提供签名事实；
    /// 两者必须可区分，消费侧不得把 `None` 当零形参编组。
    pub parameters: Option<Vec<String>>,
    /// 形参声明名（按形参顺序，与 `parameters` 等长）；`None` 表示签名不可用。
    /// 消费侧按名绑定用例字段，禁止依赖 JSON 对象键序。
    pub parameter_names: Option<Vec<String>>,
    /// 返回载体标签，词汇表与 [`Self::parameters`] 同源；无返回值用各 lane 的
    /// void 标签（如 `unit` / `void`）。`None` 表示该 lane 不提供签名事实。
    pub returns: Option<String>,
}

/// 驱动层编译结果。
#[derive(Debug, Default)]
pub struct DriverCompileReport {
    /// 产物集合。
    pub artifacts: ArtifactSet,
    /// 入口符号。
    pub entry_symbol: Option<String>,
    /// 运行契约列表。
    pub run_contracts: Vec<DriverRunContract>,
    /// 每个物理产物各自的 managed 入口 ABI 契约（JVM/CLR）。
    ///
    /// 多分区构建里每个分区拥有自己的产物与合同，由 `library_exports` 的
    /// `physical_artifact`/`abi_digest` 显式关联；不得任取一份或无条件拼接。
    pub managed_abi: Vec<ManagedAbiContract>,
    /// 每个物理产物各自的 OVM 库导出 sidecar（合同 + `.odin` 整体哈希）。
    ///
    /// 多分区构建里每个分区拥有自己的产物与合同，由 `library_exports` 的
    /// `physical_artifact`/`abi_digest` 显式关联；不得任取一份或无条件拼接。
    pub ovm_abi: Vec<OvmAbiSidecar>,
    /// 库导出索引（仅 `artifact: library`）。
    pub library_exports: Vec<DriverLibraryExport>,
}

struct DriverPartitionCompileRequest<'a> {
    bundle: &'a dyn FrontendBuildBundle,
    planned_partitions: &'a dyn PlannedArtifactPartitionsView,
    output_dir: &'a Path,
    project_name: &'a str,
    emit_wat_sidecar: bool,
    generate_runtime_config: bool,
}

impl<'a> DriverPartitionCompileRequest<'a> {
    pub fn from_frontend_bundle(
        bundle: &'a dyn FrontendBuildBundle,
        output_dir: &'a Path,
        project_name: &'a str,
        emit_wat_sidecar: bool,
        generate_runtime_config: bool,
    ) -> Self {
        Self { bundle, planned_partitions: bundle.planned_partitions(), output_dir, project_name, emit_wat_sidecar, generate_runtime_config }
    }
}

/// 使用 bundled backend 执行目标编译。
pub(crate) fn compile_with_bundled_backends(request: DriverCompileRequest<'_>) -> Result<DriverCompileReport> {
    driver::compile(request)
}

/// 使用 bundled backend 直接编译前端构建 bundle。
pub fn compile_frontend_bundle_with_bundled_backends(
    bundle: &dyn FrontendBuildBundle,
    output_dir: &Path,
    project_name: &str,
    emit_wat_sidecar: bool,
    generate_runtime_config: bool,
) -> Result<DriverCompileReport> {
    compile_partitions_with_bundled_backends(DriverPartitionCompileRequest::from_frontend_bundle(
        bundle,
        output_dir,
        project_name,
        emit_wat_sidecar,
        generate_runtime_config,
    ))
}

fn compile_partitions_with_bundled_backends(request: DriverPartitionCompileRequest<'_>) -> Result<DriverCompileReport> {
    let primary_partition_name = request.planned_partitions.primary_partition_name();
    let partition_count = request.planned_partitions.partition_count();
    let test_entry_names = request.bundle.test_entry_names();
    // 产物用途是语言侧声明的交付事实；用途与 `[test]` 入口面必须一致，
    // 不允许由 Wasm 打包枚举或导出数量反推用途。
    let purpose = request.bundle.artifact_purpose();
    match purpose {
        compiler_target::ArtifactPurpose::TestBundle if test_entry_names.is_empty() => {
            return Err("`artifact_purpose = test` 的构建必须携带 `[test]` 入口面；拒绝无测试入口的测试包".to_string());
        }
        compiler_target::ArtifactPurpose::Application
        | compiler_target::ArtifactPurpose::Library
        | compiler_target::ArtifactPurpose::HostExtension
            if !test_entry_names.is_empty() =>
        {
            return Err(format!("产物用途 `{purpose:?}` 不得携带 `[test]` 入口面；测试交付必须声明为 test 用途"));
        }
        compiler_target::ArtifactPurpose::HostExtension
            if !matches!(
                request.bundle.resolved_target().contract,
                compiler_target::ArtifactContract::NodeApi | compiler_target::ArtifactContract::Cpython(_)
            ) =>
        {
            return Err("宿主扩展用途必须携带已冻结的宿主合同（Node-API / CPython）；拒绝无合同的扩展交付".to_string());
        }
        _ => {}
    }
    let mut reports = Vec::new();

    for partition_index in 0..partition_count {
        let partition =
            request.planned_partitions.partition(partition_index).ok_or_else(|| format!("分区索引 `{partition_index}` 超出范围"))?;
        let fragment = request.bundle.assemble_fragment_for_partition(partition_index)?;
        let host_flavor = request.bundle.target_host_flavor().ok_or_else(|| format!("分区计划缺少目标 host flavor"))?;
        let backend_family = backend_family_for_partition(partition);
        // 用途承载能力只有一处声明：descriptor 的 `supported_purposes` 与边界例外表。
        // 这里在语言→目标适配与写出之前拒绝不支持的组合，避免先产出无法装载或
        // 无法按 `[test]` 名字逐条调用的文件。
        let purpose_supported = bundled_backend_capability_descriptor(backend_family)
            .is_some_and(|descriptor| descriptor.supports_purpose_on_boundary(purpose, partition.host_boundary));
        if !purpose_supported {
            return Err(format!(
                "分区 `{}` 的后端家族 `{backend_family:?}` 不承载产物用途 `{purpose:?}`（宿主边界 `{:?}`）；拒绝降级成平台产物",
                partition.name, partition.host_boundary
            ));
        }
        let lowered_input = assembly::prepare_assembled_fragment(
            fragment,
            &TargetInputRequest {
                backend_family,
                host_boundary: partition.host_boundary,
                output_dir: request.output_dir,
                host_flavor: &host_flavor,
                artifact_purpose: purpose,
                wasm_package_kind: request.bundle.wasm_package_kind(),
                project_name: request.project_name,
                resolved: request.bundle.resolved_target(),
            },
        )?;
        let artifact_base_name = lowered_input.entry_artifact_name().unwrap_or(request.project_name);
        let artifact_name = partition_artifact_name(artifact_base_name, partition, partition_count);
        let options = CompilationOptions {
            target: partition.binary_target.clone(),
            artifact_name: artifact_name.clone(),
            emit_debug_symbols: false,
            optimize: false,
        };
        let driver_input = lowered_input.into_driver_backend_input();

        if request.emit_wat_sidecar {
            let _ = write_wasm_wat_sidecar(request.output_dir, &artifact_name, &driver_input);
        }

        let requirement =
            request.planned_partitions.backend_requirement(partition_index).ok_or_else(|| format!("分区 `{}` 缺少后端需求", partition.name))?;
        let report = compile_with_bundled_backends(DriverCompileRequest {
            artifact_name: &artifact_name,
            requirement,
            input: driver_input,
            generate_runtime_config: request.generate_runtime_config,
            options: &options,
            purpose,
        })?;
        reports.push((partition.name.clone(), report));
    }

    let mut report = merge_partition_reports(reports, primary_partition_name.as_deref())?;
    if purpose == compiler_target::ArtifactPurpose::TestBundle {
        emit_test_run_contracts(&mut report, request.output_dir, &test_entry_names)?;
    }
    Ok(report)
}

/// 校验测试包的逐入口运行合同。
///
/// 每个测试必须有各自可执行、且物理产物真实存在的入口合同，否则会被静默跳过
/// 或假通过。合同由各后端按冻结事实逐条给出：managed 家族来自逐入口 ABI，
/// wasm JS-glue 来自 lowering 冻结的「逻辑测试名 → wasm 导出名」映射。emitter
/// 只验证覆盖关系并收敛到测试集合，不再合成 launcher、不按名字猜导出。
fn emit_test_run_contracts(report: &mut DriverCompileReport, output_dir: &Path, test_entry_names: &[String]) -> Result<()> {
    // 前端冻结的每个 `[test]` 必须由后端给出的逐入口运行合同唯一覆盖。这里只
    // 校验覆盖关系与物理产物是否真实存在，不合成启动器、不按名字猜导出：wasm
    // 家族按 descriptor 冻结的测试入口映射逐条给出合同，managed 家族同理。
    for test_name in test_entry_names {
        let matches: Vec<&DriverRunContract> =
            report.run_contracts.iter().filter(|contract| &contract.logical_entry == test_name).collect();
        let [contract] = matches.as_slice()
        else {
            return Err(format!("测试入口 `{test_name}` 必须由后端提供唯一执行合同，实际 {} 条；拒绝静默跳过测试入口", matches.len()));
        };
        let artifact_present = Path::new(&contract.physical_entry).is_file() || output_dir.join(&contract.physical_entry).is_file();
        if !artifact_present {
            return Err(format!("测试入口 `{test_name}` 的合同指向不存在的产物 `{}`", contract.physical_entry));
        }
    }
    report.run_contracts.retain(|contract| test_entry_names.contains(&contract.logical_entry));
    Ok(())
}

// ---------------------------------------------------------------------------
// Integration-test-only re-exports. Not part of the public API contract.
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub use backend::wasi::component::{
    build_component_wit_document, build_component_wit_document_for, build_dependency_documents, build_dependency_documents_for,
    wasi_adapt_import_for_preview, wasi_versioned_import_module_for,
};
#[doc(hidden)]
pub use backend::wasi::{WasiPreview, canonical_wasm_artifact_stem as wasm_physical_artifact_name};
#[doc(hidden)]
pub use backend::wasm_js_glue::{build_node_launcher, collect_import_bindings, try_build_node_launcher};
#[doc(hidden)]
pub use driver::families::driver_compiler_names;
#[cfg(feature = "ovm-lane")]
#[doc(hidden)]
pub use driver::families::ovm::resolve_entry_symbols;
