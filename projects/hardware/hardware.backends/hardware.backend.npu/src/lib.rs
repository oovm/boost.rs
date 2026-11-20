//! AMD XDNA（Ryzen AI）NPU 后端合同。
//!
//! 这里实现的是设备报告、可执行性查询与提交边界，不是一套私有运行时：
//! - XDNA 只接受固定形状、已编译的量化子图，以及白名单内的单算子；
//! - 动态 shape、未知 dtype/layout、未编译子图都在 [`Backend::supports`] 阶段被拒绝；
//! - 编译与提交由调用方附着的 [`KernelLauncher`] 完成，后端按任务身份缓存编译产物；
//! - [`NpuBackend::load_overlay`] 在装载边界只做 `xclbin2` 容器结构校验与节表登记，
//!   不调用驱动、不执行设备代码；
//! - 没有附着 XDNA 运行时或 launcher 之前，[`Backend::submit_task`] 返回明确错误，绝不静默回退 CPU。
//!
//! 真实 kernel 由后续接入的 XDNA 运行时生成；本 crate 负责让调度器在计划阶段区分
//! “已发现”“后端可用”“当前可提交”，并在提交时只编译一次。

#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use data_binary_xclbin::{SectionKind, Xclbin, XclbinError};
use hardware_capability::{
    Conversion, DeviceId, DeviceState, ExecDecision, ExecKind, ExecPlan, ExecRefusal, ExecSignature, Feature, FeatureSet, Health, MemoryBudget,
    OutputDomain,
};
use hardware_ir::{Graph, TaskKind, TaskNode};
use hardware_scheduler::{Backend, Error, Result};

/// 单段装载上限的保守默认值（64 MiB）。
pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

const DEFAULT_OPS: &[&str] = &["add", "mul", "matmul", "relu", "conv", "softmax", "layernorm", "reshape", "transpose"];
const DEFAULT_DTYPES: &[&str] = &["f16", "int8", "f32"];
const DEFAULT_LAYOUTS: &[&str] = &["contiguous", "nchw", "nhwc"];

/// NPU 设备与运行时的公开配置。
#[derive(Debug, Clone)]
pub struct NpuConfig {
    /// 物理设备标识。
    pub device: DeviceId,
    /// 驱动名称。
    pub driver: Option<String>,
    /// 驱动版本。
    pub driver_version: Option<String>,
    /// 附着的运行时名称；`None` 表示只完成设备发现。
    pub runtime: Option<String>,
    /// 生命周期状态。
    pub state: DeviceState,
    /// 健康状态。
    pub health: Health,
    /// 实时内存预算（XDNA 与系统共享内存，按设备整体上报）。
    pub budget: MemoryBudget,
    /// 允许的单算子 / 子图名。
    pub supported_ops: BTreeSet<String>,
    /// 允许的数据类型。
    pub supported_dtypes: BTreeSet<String>,
    /// 允许的布局。
    pub supported_layouts: BTreeSet<String>,
    /// 分段装载的单段上限。
    pub max_segment_bytes: u64,
}

impl NpuConfig {
    /// 只完成设备发现：状态为 [`DeviceState::Discovered`]，没有运行时。
    pub fn discovered() -> Self {
        Self {
            device: DeviceId::new("npu0"),
            driver: Some("amdxdna".into()),
            driver_version: None,
            runtime: None,
            state: DeviceState::Discovered,
            health: Health::Healthy,
            budget: MemoryBudget::unlimited(),
            supported_ops: DEFAULT_OPS.iter().map(|op| (*op).to_owned()).collect(),
            supported_dtypes: DEFAULT_DTYPES.iter().map(|dtype| (*dtype).to_owned()).collect(),
            supported_layouts: DEFAULT_LAYOUTS.iter().map(|layout| (*layout).to_owned()).collect(),
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
        }
    }

    /// 运行时已附着，设备可以提交任务。
    pub fn attached(runtime: impl Into<String>) -> Self {
        Self { runtime: Some(runtime.into()), state: DeviceState::Submittable, ..Self::discovered() }
    }

    /// 驱动版本。
    pub fn with_driver_version(mut self, version: impl Into<String>) -> Self {
        self.driver_version = Some(version.into());
        self
    }

    /// 健康状态。
    pub fn with_health(mut self, health: Health) -> Self {
        self.health = health;
        self
    }

    /// 实时预算。
    pub fn with_budget(mut self, budget: MemoryBudget) -> Self {
        self.budget = budget;
        self
    }

    /// 分段装载单段上限。
    pub fn with_max_segment_bytes(mut self, bytes: u64) -> Self {
        self.max_segment_bytes = bytes;
        self
    }
}

/// 一次编译的产物；后端只保存并复用，不解释二进制。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelArtifact {
    /// kernel 名（子图名或算子名）。
    pub name: String,
    /// 编译产物的字节数。
    pub bytes: u64,
    /// 生成它的运行时标识。
    pub runtime: String,
}

/// XDNA 运行时的编译与提交边界。
///
/// 实现方负责真实编译和下发；后端负责按任务身份复用产物，并在未附着时拒绝提交。
pub trait KernelLauncher {
    /// 编译一个任务对应的 kernel。
    fn compile(&mut self, task: &TaskNode) -> Result<KernelArtifact>;

    /// 提交已编译的 kernel。
    fn submit(&mut self, artifact: &KernelArtifact, task: &TaskNode) -> Result<()>;
}

/// 任务身份；同一子图/算子只编译一次。
fn kernel_key(task: &TaskNode) -> String {
    match &task.kind {
        TaskKind::Raster => "raster".into(),
        TaskKind::Dispatch => "dispatch".into(),
        TaskKind::Fill => "fill".into(),
        TaskKind::MatMul => "matmul".into(),
        TaskKind::RtTrace => "rttrace".into(),
        TaskKind::Copy => "copy".into(),
        TaskKind::Custom(name) => name.clone(),
    }
}

/// 节表条目摘要（`axlf_section_header` 的装载侧视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySection {
    /// 原始 `axlf_section_kind` 编号。
    pub kind: u32,
    /// 节名。
    pub name: String,
    /// 载荷字节数。
    pub size: u64,
}

/// 装载边界登记的 XCLBIN 设备镜像。
///
/// [`Overlay::stage`] 只完成 `xclbin2` 容器结构校验与节表登记：不解释 PDI /
/// AIE 载荷、不调用驱动、不执行任何设备代码。设备代码生成与真实装载分别由
/// `hardware.emitter` 与 XDNA 运行时接入负责。
#[derive(Debug, Clone)]
pub struct Overlay {
    bytes: Vec<u8>,
    uuid: String,
    platform: String,
    sections: Vec<OverlaySection>,
}

impl Overlay {
    /// 校验并登记一个 `xclbin2` 容器；结构不合法时返回原因，不产出半成品。
    pub fn stage(bytes: Vec<u8>) -> std::result::Result<Self, XclbinError> {
        let summary = {
            let image = Xclbin::parse(&bytes)?;
            (
                image.uuid_hex(),
                image.platform.to_owned(),
                image.sections.iter().map(|s| OverlaySection { kind: s.kind, name: s.name.to_owned(), size: s.size }).collect(),
            )
        };
        Ok(Self { bytes, uuid: summary.0, platform: summary.1, sections: summary.2 })
    }

    /// 容器原始字节。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// xclbin UUID（小写十六进制）。
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    /// `m_platformVBNV` 平台标识。
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// 节表登记。
    pub fn sections(&self) -> &[OverlaySection] {
        &self.sections
    }

    /// 是否携带设备配置镜像（PDI / 部分 PDI / AIE 分区任一）。
    pub fn has_device_config(&self) -> bool {
        self.sections.iter().any(|section| {
            matches!(SectionKind::from_raw(section.kind), Some(SectionKind::Pdi | SectionKind::BitstreamPartialPdi | SectionKind::AiePartition))
        })
    }
}

/// AMD XDNA NPU 后端。
pub struct NpuBackend {
    config: NpuConfig,
    launcher: Option<Box<dyn KernelLauncher + Send + Sync>>,
    kernels: BTreeMap<String, KernelArtifact>,
    overlay: Option<Overlay>,
}

impl NpuBackend {
    /// 用给定配置构造后端。
    pub fn new(config: NpuConfig) -> Self {
        Self { config, launcher: None, kernels: BTreeMap::new(), overlay: None }
    }

    /// 只完成设备发现的默认实例。
    pub fn discovered() -> Self {
        Self::new(NpuConfig::discovered())
    }

    /// 附着编译/提交实现；只有附着了 runtime 且提供 launcher 才能提交。
    pub fn with_launcher(mut self, launcher: impl KernelLauncher + Send + Sync + 'static) -> Self {
        self.launcher = Some(Box::new(launcher));
        self
    }

    /// 已缓存的编译产物。
    pub fn cached_kernels(&self) -> impl Iterator<Item = (&str, &KernelArtifact)> {
        self.kernels.iter().map(|(key, artifact)| (key.as_str(), artifact))
    }

    /// 只读配置。
    pub fn config(&self) -> &NpuConfig {
        &self.config
    }

    /// 是否已附着 XDNA 运行时。
    pub fn runtime_attached(&self) -> bool {
        self.config.runtime.is_some()
    }

    /// 装载（登记）一个 XCLBIN overlay：只做容器结构校验与节表登记，
    /// 不调用驱动、不执行设备代码。校验失败时保持原装载状态不变。
    pub fn load_overlay(&mut self, bytes: Vec<u8>) -> Result<()> {
        let overlay = Overlay::stage(bytes).map_err(|e| Error::Backend("npu".into(), format!("XCLBIN 容器校验失败：{e}")))?;
        self.overlay = Some(overlay);
        Ok(())
    }

    /// 当前登记的 overlay；未装载时为 `None`。
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }
}

impl Backend for NpuBackend {
    fn name(&self) -> &str {
        "npu"
    }

    fn features(&self) -> FeatureSet {
        FeatureSet::new().with(Feature::NpuCompiledSubgraph).with(Feature::SegmentedLoading)
    }

    fn device(&self) -> DeviceId {
        self.config.device.clone()
    }

    fn state(&self) -> DeviceState {
        self.config.state
    }

    fn health(&self) -> Health {
        self.config.health.clone()
    }

    fn budget(&self) -> MemoryBudget {
        self.config.budget
    }

    fn supports(&self, signature: &ExecSignature) -> ExecDecision {
        if !self.config.health.is_usable() {
            let reason = self.config.health.reason().unwrap_or("unhealthy").to_owned();
            return ExecDecision::Refuse(ExecRefusal::Unhealthy(reason));
        }
        if self.config.state != DeviceState::Submittable {
            return ExecDecision::Refuse(ExecRefusal::Unhealthy(format!("device state is {:?}", self.config.state)));
        }
        if signature.dtypes.is_empty() {
            return ExecDecision::Refuse(ExecRefusal::UnsupportedPrecision("signature does not declare a dtype".into()));
        }
        for dtype in &signature.dtypes {
            if !self.config.supported_dtypes.contains(dtype) {
                return ExecDecision::Refuse(ExecRefusal::UnsupportedPrecision(dtype.clone()));
            }
        }
        if let Some(layout) = &signature.layout {
            if !self.config.supported_layouts.contains(layout) {
                return ExecDecision::Refuse(ExecRefusal::UnsupportedLayout(layout.clone()));
            }
        }
        match signature.kind {
            ExecKind::Subgraph => {
                if signature.shapes.iter().any(Option::is_none) {
                    return ExecDecision::Refuse(ExecRefusal::UnsupportedShape { detail: "compiled subgraphs need fixed input shapes".into() });
                }
                if signature.quantization.is_none() && !self.config.supported_ops.contains(&signature.op) {
                    return ExecDecision::Refuse(ExecRefusal::UnsupportedOp(format!("`{}` is not a compiled subgraph", signature.op)));
                }
            }
            ExecKind::Operator => {
                if !self.config.supported_ops.contains(&signature.op) {
                    return ExecDecision::Refuse(ExecRefusal::UnsupportedOp(signature.op.clone()));
                }
            }
        }

        let elements = signature.element_count().unwrap_or(0) as u64;
        let bytes = elements.saturating_mul(4);
        let compile_cost: u64 = if signature.kind == ExecKind::Subgraph { 900 } else { 60 };
        let estimated_cost = compile_cost.saturating_add(bytes / 1024);
        let mut plan = ExecPlan::new("npu", self.config.device.clone())
            .with_temp_bytes(bytes)
            .with_transfer_bytes(bytes)
            .with_output_domain(OutputDomain::Device)
            .with_compile_cost(compile_cost)
            .with_estimated_cost(estimated_cost)
            .with_max_segment_bytes(self.config.max_segment_bytes);
        if signature.layout.as_deref() == Some("nchw") {
            plan = plan.with_conversion(Conversion::Layout("nchw -> nhwc".into()));
        }
        ExecDecision::Accept(plan)
    }

    fn submit_task(&mut self, _graph: &Graph, task: &TaskNode) -> Result<()> {
        let runtime = match &self.config.runtime {
            None => return Err(Error::Backend("npu".into(), "no XDNA runtime attached; refusing to submit".into())),
            Some(runtime) => runtime.clone(),
        };
        let launcher = match self.launcher.as_mut() {
            Some(launcher) => launcher,
            None => {
                return Err(Error::Backend("npu".into(), format!("runtime `{runtime}` has no kernel launcher attached; refusing to submit")));
            }
        };
        let key = kernel_key(task);
        let artifact = match self.kernels.get(&key) {
            Some(artifact) => artifact.clone(),
            None => {
                let artifact = launcher.compile(task)?;
                self.kernels.insert(key, artifact.clone());
                artifact
            }
        };
        launcher.submit(&artifact, task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matmul() -> ExecSignature {
        ExecSignature::operator("matmul", vec!["f16".into()], vec![Some(vec![2, 3]), Some(vec![3, 4])])
    }

    #[test]
    fn discovered_device_reports_without_being_submittable() {
        let backend = NpuBackend::discovered();
        assert_eq!(backend.name(), "npu");
        assert_eq!(backend.state(), DeviceState::Discovered);
        assert!(!backend.runtime_attached());
        assert!(backend.features().has(Feature::NpuCompiledSubgraph));
        assert!(backend.features().has(Feature::SegmentedLoading));
        match backend.supports(&matmul()) {
            ExecDecision::Refuse(ExecRefusal::Unhealthy(reason)) => assert!(reason.contains("Discovered")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn attached_device_accepts_fixed_shape_subgraphs_with_segmentation() {
        let backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna").with_driver_version("32.0.203.242"));
        let signature = ExecSignature::subgraph("mlp", vec!["int8".into()], vec![Some(vec![1, 256])])
            .with_quantization("per-tensor-int8")
            .with_layout("nhwc");
        let plan = backend.supports(&signature).plan().cloned().expect("accepted");
        assert_eq!(plan.backend.as_str(), "npu");
        assert_eq!(plan.device.as_str(), "npu0");
        assert!(plan.compile_cost > 0);
        assert!(plan.estimated_cost >= plan.compile_cost);
        assert_eq!(plan.max_segment_bytes, Some(DEFAULT_MAX_SEGMENT_BYTES));
        assert!(plan.required_conversions.is_empty());
    }

    #[test]
    fn rejects_unsupported_shapes_dtypes_ops_and_layouts() {
        let backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna"));

        let dynamic = ExecSignature::subgraph("mlp", vec!["f16".into()], vec![None]).with_quantization("per-tensor-int8");
        assert!(matches!(backend.supports(&dynamic), ExecDecision::Refuse(ExecRefusal::UnsupportedShape { .. })));

        let f64 = ExecSignature::operator("matmul", vec!["f64".into()], vec![Some(vec![2, 2]), Some(vec![2, 2])]);
        assert!(matches!(backend.supports(&f64), ExecDecision::Refuse(ExecRefusal::UnsupportedPrecision(_))));

        let unknown = ExecSignature::operator("fft", vec!["f16".into()], vec![Some(vec![4])]);
        assert!(matches!(backend.supports(&unknown), ExecDecision::Refuse(ExecRefusal::UnsupportedOp(_))));

        let blocked = ExecSignature::operator("matmul", vec!["f16".into()], vec![Some(vec![2, 2])]).with_layout("blocked");
        assert!(matches!(backend.supports(&blocked), ExecDecision::Refuse(ExecRefusal::UnsupportedLayout(_))));

        let no_dtype = ExecSignature::operator("matmul", Vec::new(), Vec::new());
        assert!(matches!(backend.supports(&no_dtype), ExecDecision::Refuse(ExecRefusal::UnsupportedPrecision(_))));
    }

    #[test]
    fn unhealthy_device_is_not_a_candidate() {
        let backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna").with_health(Health::Failed("device removed".into())));
        match backend.supports(&matmul()) {
            ExecDecision::Refuse(ExecRefusal::Unhealthy(reason)) => assert_eq!(reason, "device removed"),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn empty_graph() -> Graph {
        Graph { resources: hardware_ir::ResourceGraph { nodes: vec![] }, tasks: hardware_ir::TaskGraph { nodes: vec![], edges: vec![] } }
    }

    fn task(id: u64, kind: hardware_ir::TaskKind) -> TaskNode {
        TaskNode {
            id: hardware_ir::TaskId(id),
            kind,
            shader: hardware_ir::ShaderId(1),
            params: Default::default(),
            dispatch: Default::default(),
            objective: hardware_ir::Objective::MaxThroughput,
            priority: hardware_ir::Priority::Batch,
        }
    }

    /// 记录编译与提交次数；编译产物是确定性的，不含任何真实计算。
    #[derive(Default)]
    struct RecordingLauncher {
        compiled: Vec<String>,
        submitted: Vec<String>,
    }

    impl KernelLauncher for RecordingLauncher {
        fn compile(&mut self, task: &TaskNode) -> Result<KernelArtifact> {
            let name = kernel_key(task);
            self.compiled.push(name.clone());
            Ok(KernelArtifact { name, bytes: 4096, runtime: "onnxruntime-xdna".into() })
        }

        fn submit(&mut self, artifact: &KernelArtifact, _task: &TaskNode) -> Result<()> {
            self.submitted.push(artifact.name.clone());
            Ok(())
        }
    }

    #[test]
    fn submit_fails_loudly_instead_of_falling_back() {
        let graph = empty_graph();
        let task = task(1, hardware_ir::TaskKind::Custom("mlp".into()));
        let mut backend = NpuBackend::discovered();
        let error = backend.submit_task(&graph, &task).expect_err("no runtime");
        assert!(error.to_string().contains("no XDNA runtime attached"), "{error}");

        let mut backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna"));
        let error = backend.submit_task(&graph, &task).expect_err("no launcher");
        assert!(error.to_string().contains("no kernel launcher attached"), "{error}");
    }

    #[test]
    fn attached_runtime_compiles_once_and_reuses_the_cached_kernel() {
        let mut backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna")).with_launcher(RecordingLauncher::default());
        let graph = empty_graph();
        let first = task(1, hardware_ir::TaskKind::Custom("mlp".into()));
        let second = task(2, hardware_ir::TaskKind::Custom("mlp".into()));
        let matmul = task(3, hardware_ir::TaskKind::MatMul);

        backend.submit_task(&graph, &first).expect("compile and submit");
        backend.submit_task(&graph, &second).expect("cache hit and submit");
        backend.submit_task(&graph, &matmul).expect("different kernel");

        let cached: Vec<&str> = backend.cached_kernels().map(|(key, _)| key).collect();
        assert_eq!(cached, vec!["matmul", "mlp"], "one artifact per task identity");
        assert_eq!(backend.cached_kernels().next().map(|(_, artifact)| artifact.bytes), Some(4096));
    }

    #[test]
    fn segmented_plan_covers_a_model_larger_than_the_budget() {
        let budget = MemoryBudget::new(256 * 1024, 0, 0);
        let backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna").with_budget(budget).with_max_segment_bytes(64 * 1024));
        let signature =
            ExecSignature::subgraph("mlp", vec!["int8".into()], vec![Some(vec![1, 1024 * 1024])]).with_quantization("per-tensor-int8");
        let plan = backend.supports(&signature).plan().cloned().expect("accepted");
        assert!(plan.footprint_bytes() > budget.available_bytes());
        assert!(plan.fits_budget(&budget));
    }

    fn authored_container() -> Vec<u8> {
        data_binary_xclbin::XclbinBuilder::new()
            .platform("xilinx_v1_ipu_0_0")
            .uuid([0x11; 16])
            .add_section(data_binary_xclbin::SectionKind::Pdi, "pdi", vec![0xA5u8; 16])
            .add_section(data_binary_xclbin::SectionKind::AieMetadata, "aie_metadata", vec![1u8; 8])
            .build()
    }

    #[test]
    fn load_overlay_validates_the_container_and_registers_the_inventory() {
        let mut backend = NpuBackend::discovered();
        assert!(backend.overlay().is_none());
        backend.load_overlay(authored_container()).expect("valid container");
        let overlay = backend.overlay().expect("staged overlay");
        assert_eq!(overlay.platform(), "xilinx_v1_ipu_0_0");
        assert_eq!(overlay.uuid().len(), 32);
        assert_eq!(overlay.sections().len(), 2);
        assert_eq!(overlay.sections()[0].name, "pdi");
        assert!(overlay.has_device_config());
    }

    #[test]
    fn load_overlay_rejects_non_container_bytes_without_staging() {
        let mut backend = NpuBackend::discovered();
        let error = backend.load_overlay(b"definitely not an xclbin image".to_vec()).expect_err("garbage");
        assert!(error.to_string().contains("容器校验失败"), "{error}");
        assert!(backend.overlay().is_none());
    }

    #[test]
    fn overlay_without_device_config_is_still_a_valid_container() {
        let bytes = data_binary_xclbin::XclbinBuilder::new()
            .add_section(data_binary_xclbin::SectionKind::MemTopology, "mem_topology", vec![1u8; 8])
            .build();
        let overlay = Overlay::stage(bytes).expect("valid");
        assert!(!overlay.has_device_config());
    }
}
