#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

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
    AlgebraicTerm, EGraphSnapshot, EntryReturnProjection, FutamuraProjectionFamily, HostProjectionBoundary, ObjectAlgebraicBuilder,
    ObjectAlgebraicDimension, ObjectAlgebraicInterpreter, ObjectAlgebraicProgram, OptimizationRequest, OptimizationResult,
    OptimizationSession, ProjectionPlan, ProjectionPolicy, ReferenceManagement, RewriteEquation, RewritePhase, RewriteRule,
    RewriteTheory, TermRewrite, TheoryBundle,
};
pub use compiler_types::{
    AggregateLayoutPlan, CapabilityTag, ExternalCallArgument, ExternalCallEdge, ExternalImportLink, FlagsLayout, FunctionType, Identifier,
    InternalCallEdge, NamePath, QualifiedName, RuntimeRequirement, SemanticType, SingletonInstancePlan, SumTypeLayout, SymbolIdentity,
    WitnessCallEdge, WitnessMethodSlotSubmission, WitnessObject, WitnessSubmission,
};
