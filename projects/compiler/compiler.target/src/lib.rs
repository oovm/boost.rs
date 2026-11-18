#![deny(missing_docs)]

//! 语言无关的目标解析、产物规划与后端注册合同。
//!
//! # 分层
//!
//! - [`abstractions`]：目标、产物种类与二进制形态的中立描述。
//! - [`target_profile`] / [`target_resolution`]：目标画像、宿主探测与解析结果。
//! - [`backends`]：后端描述、注册表与目标编码 trait；具体编码由各目标实现。
//! - [`planning`] / [`lanes`] / [`packaging`] / [`selection`]：分区计划、降低通道、
//!   产物集合与后端选择。
//! - 语言语义、函数体与目标私有 LIR 不在本 crate；它们由语言前端与目标发射器拥有。

pub mod abstractions;
pub mod backends;
pub mod lanes;
pub mod packaging;
pub mod planning;
pub mod selection;
pub mod target_profile;
pub mod target_resolution;

pub use self::{
    abstractions::{
        ArtifactFormat, ArtifactKind, ArtifactPurpose, BackendInputKind, BinaryArch, BinaryFlavor, BinaryTarget, ByteOrder, CanonicalAbi,
        CanonicalArch, CanonicalSpecification, CanonicalTarget, CanonicalTargetParseError, CanonicalVendor, ObjectKind, TargetFamily,
    },
    backends::{
        BackendCapability, BackendDescriptor, BackendInterpreterRegistration, BackendInterpreterSelection, BackendRegistry, CompilationOptions,
        TargetCodeGenBackend,
        clr::{ClrImageKind, ClrSuspendStrategy, FrontendBuildContext},
        projection_family_for_backend, projection_policy_for_target_profile,
        vm::VmSuspendStrategy,
    },
    lanes::{LaneLoweringResult, TargetLoweringLane, TargetLoweringLaneDescriptor},
    packaging::{ArtifactDescriptor, ArtifactSet, OutputSpec, TargetLane},
    planning::{
        ArtifactPartition, ArtifactPartitionPlan, ControlFlowPayload, FragmentOptimizationView, PartitionBackendRequirement, PlanningError,
        SemanticFragment, SuspendConsumptionModel, SuspendContinuationArtifact, SuspendDispatchCase, SuspendFunctionArtifact,
        SuspendRuntimeFunctionArtifact, SuspendRuntimePayload, SuspendStateArtifact, SuspendWitnessBinding, builtin_graphic_manifest,
        builtin_neural_manifest, suspend_consumption_model, suspend_consumption_model_for_lane,
    },
    selection::{BackendCandidate, BackendSelector},
    target_profile::{
        ArtifactPolicy, EntryPolicy, PublishFormat, RunnerFamily, RunnerSelector, TargetBackendFamily, TargetHostKind, TargetMode,
        TargetProfile, WrapStrategy,
    },
    target_resolution::{
        ArtifactContract, CpythonHostConfig, NodeHostProbe, PythonHostProbe, ResolvedTarget, TargetRequest, TargetResolutionError,
        TargetResolver, probe_build_host,
    },
};
pub use compiler_analysis::{EntryContract, ExportContract, FunctionAnalysis, ImportContract, ProgramFacts};
pub use compiler_optimize::{
    AlgebraicTerm, EGraphSnapshot, FutamuraProjectionFamily, HostProjectionBoundary, ObjectAlgebraicBuilder, ObjectAlgebraicDimension,
    ObjectAlgebraicInterpreter, ObjectAlgebraicProgram, OptimizationRequest, OptimizationResult, OptimizationSession, ProjectionPlan,
    ProjectionPolicy, ReferenceManagement, RewriteEquation, RewritePhase, RewriteRule, RewriteTheory, TermRewrite, TheoryBundle,
};
pub use compiler_types::{
    AggregateLayoutPlan, CapabilityTag, ExternalCallArgument, ExternalCallEdge, ExternalImportLink, FlagsLayout, FunctionType, Identifier,
    InternalCallEdge, NamePath, QualifiedName, RuntimeRequirement, SemanticType, SingletonInstancePlan, SumTypeLayout, SymbolIdentity,
    WitnessCallEdge, WitnessMethodSlotSubmission, WitnessObject, WitnessSubmission,
};
