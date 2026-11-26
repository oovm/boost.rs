#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

pub mod abstractions;
pub mod backends;
pub mod lanes;
pub mod packaging;
pub mod selection;
pub mod target_profile;
pub mod target_resolution;

pub use self::{
    abstractions::{
        ArtifactFormat, ArtifactKind, ArtifactPurpose, BackendInputKind, BinaryArch, BinaryFlavor, BinaryTarget, ByteOrder, CanonicalAbi,
        CanonicalArch, CanonicalSpecification, CanonicalTarget, CanonicalTargetParseError, CanonicalVendor, ObjectKind, PartitionBackendRequirement, TargetFamily,
    },
    backends::{
        BackendCapability, BackendDescriptor, BackendInterpreterRegistration, BackendInterpreterSelection, BackendRegistry, CompilationOptions,
        TargetCodeGenBackend,
        clr::{ClrImageKind, ClrSuspendStrategy, FrontendBuildContext},
        projection_family_for_backend, projection_policy_for_target_profile,
        vm::VmSuspendStrategy, SuspendConsumptionModel, suspend_consumption_model, suspend_consumption_model_for_lane,
    },
    lanes::{LaneLoweringResult, TargetLoweringLane, TargetLoweringLaneDescriptor},
    packaging::{ArtifactDescriptor, ArtifactSet, OutputSpec, TargetLane},
    selection::{BackendCandidate, BackendSelector},
    target_profile::{
        ArtifactPolicy, EntryPolicy, PublishFormat, RunnerFamily, RunnerSelector, TargetBackendFamily, TargetHostKind, TargetMode,
        TargetProfile, WrapStrategy,
    },
    target_resolution::{
        ArtifactContract, CpythonHostConfig, HostExportName, HostExportNameCollision, HostExportNameFact, HostExportNaming, NodeHostProbe,
        PythonHostProbe, ResolvedTarget, TargetRequest, TargetResolutionError, TargetResolver, cpython_module_name, probe_build_host,
    },
};
pub use compiler_types::{
    AggregateLayoutPlan, CapabilityTag, EntryReturnProjection, ExternalCallArgument, ExternalCallEdge, ExternalImportLink, FlagsLayout,
    FunctionType, FutamuraProjectionFamily, HostProjectionBoundary, Identifier, InternalCallEdge, NamePath, ProjectionPlan, ProjectionPolicy,
    QualifiedName, ReferenceManagement, RuntimeRequirement, SemanticType, SingletonInstancePlan, SumTypeLayout, SymbolIdentity,
    WitnessCallEdge, WitnessMethodSlotSubmission, WitnessObject, WitnessSubmission,
};
