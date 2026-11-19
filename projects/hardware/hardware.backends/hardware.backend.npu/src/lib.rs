//! AMD XDNA（Ryzen AI）NPU 后端合同。
//!
//! 这里实现的是设备报告、可执行性查询与提交边界，不是一套私有运行时：
//! - XDNA 只接受固定形状、已编译的量化子图，以及白名单内的单算子；
//! - 动态 shape、未知 dtype/layout、未编译子图都在 [`Backend::supports`] 阶段被拒绝；
//! - 没有附着 XDNA 运行时之前，[`Backend::submit_task`] 返回明确错误，绝不静默回退 CPU。
//!
//! 真实执行由后续接入的 XDNA 运行时（编译 + 提交）完成；本 crate 负责让调度器
//! 在计划阶段就能区分“已发现”“后端可用”“当前可提交”。

#![warn(missing_docs)]

use std::collections::BTreeSet;

use hardware_capability::{
    Conversion, DeviceId, DeviceState, ExecDecision, ExecKind, ExecPlan, ExecRefusal, ExecSignature, Feature, FeatureSet, Health, MemoryBudget,
    OutputDomain,
};
use hardware_ir::{Graph, TaskNode};
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

/// AMD XDNA NPU 后端。
pub struct NpuBackend {
    config: NpuConfig,
}

impl NpuBackend {
    /// 用给定配置构造后端。
    pub fn new(config: NpuConfig) -> Self {
        Self { config }
    }

    /// 只完成设备发现的默认实例。
    pub fn discovered() -> Self {
        Self::new(NpuConfig::discovered())
    }

    /// 只读配置。
    pub fn config(&self) -> &NpuConfig {
        &self.config
    }

    /// 是否已附着 XDNA 运行时。
    pub fn runtime_attached(&self) -> bool {
        self.config.runtime.is_some()
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
        match &self.config.runtime {
            None => Err(Error::Backend("npu".into(), "no XDNA runtime attached; refusing to submit".into())),
            Some(runtime) => Err(Error::Backend("npu".into(), format!("runtime `{runtime}` has no compiled kernel for task {}", task.id.0))),
        }
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

    #[test]
    fn submit_fails_loudly_instead_of_falling_back() {
        let graph =
            Graph { resources: hardware_ir::ResourceGraph { nodes: vec![] }, tasks: hardware_ir::TaskGraph { nodes: vec![], edges: vec![] } };
        let task = TaskNode {
            id: hardware_ir::TaskId(1),
            kind: hardware_ir::TaskKind::Custom("mlp".into()),
            shader: hardware_ir::ShaderId(1),
            params: Default::default(),
            dispatch: Default::default(),
            objective: hardware_ir::Objective::MaxThroughput,
            priority: hardware_ir::Priority::Batch,
        };
        let mut backend = NpuBackend::discovered();
        let error = backend.submit_task(&graph, &task).expect_err("no runtime");
        assert!(error.to_string().contains("no XDNA runtime attached"), "{error}");

        let mut backend = NpuBackend::new(NpuConfig::attached("onnxruntime-xdna"));
        let error = backend.submit_task(&graph, &task).expect_err("no compiled kernel");
        assert!(error.to_string().contains("no compiled kernel"), "{error}");
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
}
