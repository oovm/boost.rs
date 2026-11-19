//! 设备/后端标识、可执行性签名与放置决策合同。
//!
//! 调度不再按后端名或任务种类猜测放置。上层为每个算子或子图提交一份精确签名，
//! 每个后端返回接受方案或拒绝原因；设备发现、后端可用与当前可提交是三个独立状态，
//! 健康状态和实时预算都是可行性硬条件。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 稳定物理设备标识（跨后端实例复用，如同一 GPU 的 DX12 与 Vulkan 路径）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(String);

impl DeviceId {
    /// 用稳定字符串构造设备标识。
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// 返回底层字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DeviceId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for DeviceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 访问某块物理设备的后端/API 路径标识（如 `dx12`、`vulkan`、`cuda`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BackendId(String);

impl BackendId {
    /// 用稳定字符串构造后端标识。
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// 返回底层字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for BackendId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for BackendId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 设备生命周期状态；三者必须分开报告，不能用一个“可用”布尔值代替。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DeviceState {
    /// 操作系统或枚举器已看到设备，尚不能作为后端提交目标。
    Discovered,
    /// 后端已成功建立运行时对象，可以接受资源准备。
    Available,
    /// 当前可以提交任务。
    Submittable,
}

/// 设备健康状态；不健康的设备不进入可行性候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Health {
    /// 最近一次探测正常。
    Healthy,
    /// 可以提交，但已有降级证据（超时、重试、驱动重置）。
    Degraded(String),
    /// 设备丢失或驱动失败；在重新探测成功前停止派发。
    Failed(String),
}

impl Health {
    /// 是否允许作为提交目标。
    pub fn is_usable(&self) -> bool {
        matches!(self, Health::Healthy | Health::Degraded(_))
    }

    /// 不可用原因；健康时返回 `None`。
    pub fn reason(&self) -> Option<&str> {
        match self {
            Health::Healthy => None,
            Health::Degraded(reason) | Health::Failed(reason) => Some(reason),
        }
    }
}

/// 一次可执行性查询的执行单元类别。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ExecKind {
    /// 单个算子。
    #[default]
    Operator,
    /// 已连通、需要整体编译或整体拒绝的子图。
    Subgraph,
}

/// 设备与后端实例的公开记录（能力报告行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 物理设备。
    pub device: DeviceId,
    /// 访问路径。
    pub backend: BackendId,
    /// 驱动名称。
    pub driver: Option<String>,
    /// 运行时名称与版本。
    pub runtime: Option<String>,
    /// 驱动版本。
    pub driver_version: Option<String>,
    /// 生命周期状态。
    pub state: DeviceState,
    /// 健康状态。
    pub health: Health,
    /// 实时内存预算。
    pub budget: MemoryBudget,
}

impl DeviceInfo {
    /// 构造一条设备记录；驱动与运行时信息可选。
    pub fn new(device: impl Into<DeviceId>, backend: impl Into<BackendId>, state: DeviceState) -> Self {
        Self {
            device: device.into(),
            backend: backend.into(),
            driver: None,
            runtime: None,
            driver_version: None,
            state,
            health: Health::Healthy,
            budget: MemoryBudget::unlimited(),
        }
    }

    /// 驱动版本等元数据。
    pub fn with_driver(mut self, driver: impl Into<String>, version: impl Into<String>) -> Self {
        self.driver = Some(driver.into());
        self.driver_version = Some(version.into());
        self
    }

    /// 运行时名称与版本。
    pub fn with_runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = Some(runtime.into());
        self
    }

    /// 生命周期状态。
    pub fn with_state(mut self, state: DeviceState) -> Self {
        self.state = state;
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

    /// 当前是否可以作为提交目标。
    pub fn is_submittable(&self) -> bool {
        self.state == DeviceState::Submittable && self.health.is_usable()
    }
}

/// 一次可执行性查询的精确签名。
///
/// `shapes` 中的 `None` 表示该输入形状尚未确定；后端必须按自己支持的动态范围判断。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSignature {
    /// 执行单元类别。
    pub kind: ExecKind,
    /// 算子名或子图名。
    pub op: String,
    /// 输入与输出数据类型，如 `f32`、`q8_0`。
    pub dtypes: Vec<String>,
    /// 输入形状；`None` 表示动态或未知。
    pub shapes: Vec<Option<Vec<usize>>>,
    /// 数据布局，如 `contiguous`、`nchw`。
    pub layout: Option<String>,
    /// 量化规则，如 `per-tensor-int8`。
    pub quantization: Option<String>,
    /// 累加/输出精度要求，如 `f32`。
    pub precision: Option<String>,
}

impl ExecSignature {
    /// 单个算子签名。
    pub fn operator(op: impl Into<String>, dtypes: Vec<String>, shapes: Vec<Option<Vec<usize>>>) -> Self {
        Self { kind: ExecKind::Operator, op: op.into(), dtypes, shapes, ..Self::default() }
    }

    /// 需要整体编译或整体拒绝的子图签名。
    pub fn subgraph(op: impl Into<String>, dtypes: Vec<String>, shapes: Vec<Option<Vec<usize>>>) -> Self {
        Self { kind: ExecKind::Subgraph, op: op.into(), dtypes, shapes, ..Self::default() }
    }

    /// 指定布局。
    pub fn with_layout(mut self, layout: impl Into<String>) -> Self {
        self.layout = Some(layout.into());
        self
    }

    /// 指定量化规则。
    pub fn with_quantization(mut self, quantization: impl Into<String>) -> Self {
        self.quantization = Some(quantization.into());
        self
    }

    /// 指定精度要求。
    pub fn with_precision(mut self, precision: impl Into<String>) -> Self {
        self.precision = Some(precision.into());
        self
    }

    /// 按元素个数估算最小数据字节数；任一形状未知时返回 `None`。
    pub fn element_count(&self) -> Option<usize> {
        let mut total = 0usize;
        for shape in &self.shapes {
            let shape = shape.as_ref()?;
            total = total.checked_add(shape.iter().copied().product::<usize>())?;
        }
        Some(total)
    }
}

/// 需要执行的缓冲区格式转换。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Conversion {
    /// 布局转换，如 `nchw -> nhwc`。
    Layout(String),
    /// 数据类型转换，如 `f16 -> f32`。
    Dtype(String),
    /// 设备域搬运，如 `host -> device`。
    Device(String),
}

impl fmt::Display for Conversion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Conversion::Layout(detail) => write!(f, "layout:{detail}"),
            Conversion::Dtype(detail) => write!(f, "dtype:{detail}"),
            Conversion::Device(detail) => write!(f, "device:{detail}"),
        }
    }
}

/// 输出数据所在的域。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OutputDomain {
    /// 设备可寻址内存。
    #[default]
    Device,
    /// 主机内存（需要回读）。
    Host,
}

/// 后端接受签名后的可执行方案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecPlan {
    /// 接受该方案的后端。
    pub backend: BackendId,
    /// 方案执行的物理设备。
    pub device: DeviceId,
    /// 临时内存需求（字节）。
    pub temp_bytes: u64,
    /// 必要搬运字节。
    pub transfer_bytes: u64,
    /// 执行前必须完成的转换。
    pub required_conversions: Vec<Conversion>,
    /// 输出域。
    pub output_domain: OutputDomain,
    /// 编译或装载代价（微秒）。
    pub compile_cost: u64,
    /// 端到端代价权重，供调度器比较（微秒）。
    pub estimated_cost: u64,
    /// 分段装载时单段的最大字节数；`None` 表示不支持分段。
    ///
    /// 支持分段的设备（如带磁盘后备的虚拟显存）可以在总足迹超过实时预算时，
    /// 只要求单段装得下；每次提交仍必须能容纳当前段的输入、输出与临时区。
    pub max_segment_bytes: Option<u64>,
}

impl ExecPlan {
    /// 零代价基础方案。
    pub fn new(backend: impl Into<BackendId>, device: impl Into<DeviceId>) -> Self {
        Self {
            backend: backend.into(),
            device: device.into(),
            temp_bytes: 0,
            transfer_bytes: 0,
            required_conversions: Vec::new(),
            output_domain: OutputDomain::Device,
            compile_cost: 0,
            estimated_cost: 0,
            max_segment_bytes: None,
        }
    }

    /// 临时内存需求。
    pub fn with_temp_bytes(mut self, bytes: u64) -> Self {
        self.temp_bytes = bytes;
        self
    }

    /// 必要搬运字节。
    pub fn with_transfer_bytes(mut self, bytes: u64) -> Self {
        self.transfer_bytes = bytes;
        self
    }

    /// 追加一次转换。
    pub fn with_conversion(mut self, conversion: Conversion) -> Self {
        self.required_conversions.push(conversion);
        self
    }

    /// 输出域。
    pub fn with_output_domain(mut self, domain: OutputDomain) -> Self {
        self.output_domain = domain;
        self
    }

    /// 编译或装载代价。
    pub fn with_compile_cost(mut self, micros: u64) -> Self {
        self.compile_cost = micros;
        self
    }

    /// 端到端代价权重。
    pub fn with_estimated_cost(mut self, micros: u64) -> Self {
        self.estimated_cost = micros;
        self
    }

    /// 方案需要的内存足迹（临时区 + 搬运缓冲）。
    pub fn footprint_bytes(&self) -> u64 {
        self.temp_bytes.saturating_add(self.transfer_bytes)
    }

    /// 声明分段装载能力；`max_segment_bytes` 为单段上限。
    pub fn with_max_segment_bytes(mut self, max_segment_bytes: u64) -> Self {
        self.max_segment_bytes = Some(max_segment_bytes);
        self
    }

    /// 实时预算是否接受该方案。
    ///
    /// 优先要求整个足迹装得下；否则只有声明了分段装载且单段装得下才接受。
    pub fn fits_budget(&self, budget: &MemoryBudget) -> bool {
        if budget.fits(self.footprint_bytes()) {
            return true;
        }
        match self.max_segment_bytes {
            Some(segment) if segment > 0 => budget.fits(segment),
            _ => false,
        }
    }
}

/// 后端拒绝执行签名的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecRefusal {
    /// 不支持的算子或子图。
    UnsupportedOp(String),
    /// 形状或动态范围不受支持。
    UnsupportedShape {
        /// 具体说明。
        detail: String,
    },
    /// 数据类型或精度不受支持。
    UnsupportedPrecision(String),
    /// 布局或量化规则不受支持。
    UnsupportedLayout(String),
    /// 实时预算不足。
    BudgetExceeded {
        /// 方案需要的内存。
        required_bytes: u64,
        /// 当前可用内存。
        available_bytes: u64,
    },
    /// 设备不健康或尚未进入可提交状态。
    Unhealthy(String),
}

impl fmt::Display for ExecRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecRefusal::UnsupportedOp(op) => write!(f, "unsupported op `{op}`"),
            ExecRefusal::UnsupportedShape { detail } => write!(f, "unsupported shape: {detail}"),
            ExecRefusal::UnsupportedPrecision(detail) => write!(f, "unsupported precision: {detail}"),
            ExecRefusal::UnsupportedLayout(detail) => write!(f, "unsupported layout: {detail}"),
            ExecRefusal::BudgetExceeded { required_bytes, available_bytes } => {
                write!(f, "budget exceeded: need {required_bytes} bytes, have {available_bytes}")
            }
            ExecRefusal::Unhealthy(detail) => write!(f, "device unavailable: {detail}"),
        }
    }
}

/// 可执行性查询结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecDecision {
    /// 接受，并给出执行方案。
    Accept(ExecPlan),
    /// 拒绝，并给出可判别原因。
    Refuse(ExecRefusal),
}

impl ExecDecision {
    /// 是否接受。
    pub fn is_accept(&self) -> bool {
        matches!(self, ExecDecision::Accept(_))
    }

    /// 接受方案。
    pub fn plan(&self) -> Option<&ExecPlan> {
        match self {
            ExecDecision::Accept(plan) => Some(plan),
            ExecDecision::Refuse(_) => None,
        }
    }

    /// 拒绝原因。
    pub fn refusal(&self) -> Option<&ExecRefusal> {
        match self {
            ExecDecision::Accept(_) => None,
            ExecDecision::Refuse(reason) => Some(reason),
        }
    }
}

/// 实时内存预算；容量、已用与预留分开记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    /// 总容量（字节）。
    pub capacity_bytes: u64,
    /// 已使用（字节）。
    pub used_bytes: u64,
    /// 为其它驻留数据预留（字节）。
    pub reserve_bytes: u64,
}

impl MemoryBudget {
    /// 构造预算。
    pub fn new(capacity_bytes: u64, used_bytes: u64, reserve_bytes: u64) -> Self {
        Self { capacity_bytes, used_bytes, reserve_bytes }
    }

    /// 不设上限（仅供未探测的占位路径使用）。
    pub fn unlimited() -> Self {
        Self { capacity_bytes: u64::MAX, used_bytes: 0, reserve_bytes: 0 }
    }

    /// 当前可分配的字节数。
    pub fn available_bytes(&self) -> u64 {
        self.capacity_bytes.saturating_sub(self.used_bytes).saturating_sub(self.reserve_bytes)
    }

    /// 是否装得下 `bytes`。
    pub fn fits(&self, bytes: u64) -> bool {
        bytes <= self.available_bytes()
    }

    /// 分配 `bytes` 后剩余的可分配字节数；不足时返回 `None`。
    pub fn remaining_after(&self, bytes: u64) -> Option<u64> {
        self.available_bytes().checked_sub(bytes)
    }
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self::unlimited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_accounts_used_and_reserve() {
        let budget = MemoryBudget::new(1024, 256, 256);
        assert_eq!(budget.available_bytes(), 512);
        assert!(budget.fits(512));
        assert!(!budget.fits(513));
        assert_eq!(budget.remaining_after(512), Some(0));
        assert_eq!(budget.remaining_after(1024), None);
        assert_eq!(MemoryBudget::unlimited().available_bytes(), u64::MAX);
    }

    #[test]
    fn device_record_separates_states() {
        let discovered = DeviceInfo::new("gpu0", "vulkan", DeviceState::Discovered);
        assert!(!discovered.is_submittable());
        let available = DeviceInfo::new("gpu0", "dx12", DeviceState::Available).with_driver("amdxc64", "32.0.21030.2001");
        assert!(!available.is_submittable());
        let submittable = available.clone().with_state(DeviceState::Submittable).with_budget(MemoryBudget::new(512, 0, 0));
        assert_eq!(submittable.state, DeviceState::Submittable);
        assert!(submittable.is_submittable());
        assert!(!submittable.with_health(Health::Failed("device removed".into())).is_submittable());
        assert_eq!(Health::Degraded("timeout".into()).reason(), Some("timeout"));
    }

    #[test]
    fn decision_carries_plan_or_reason() {
        let plan = ExecPlan::new("dx12", "gpu0")
            .with_temp_bytes(64)
            .with_transfer_bytes(16)
            .with_conversion(Conversion::Layout("nchw -> nhwc".into()))
            .with_estimated_cost(40);
        assert_eq!(plan.footprint_bytes(), 80);
        let accepted = ExecDecision::Accept(plan.clone());
        assert!(accepted.is_accept());
        assert_eq!(accepted.plan(), Some(&plan));
        assert_eq!(accepted.refusal(), None);

        let refused = ExecDecision::Refuse(ExecRefusal::BudgetExceeded { required_bytes: 2048, available_bytes: 1024 });
        assert!(!refused.is_accept());
        assert_eq!(refused.refusal().map(ToString::to_string).as_deref(), Some("budget exceeded: need 2048 bytes, have 1024"));
    }

    #[test]
    fn segmentation_only_relaxes_the_whole_footprint_check() {
        let budget = MemoryBudget::new(512, 0, 0);
        let flat = ExecPlan::new("cpu", "cpu0").with_temp_bytes(1024);
        assert!(!flat.fits_budget(&budget));

        let segmented = ExecPlan::new("npu", "npu0").with_temp_bytes(1024).with_max_segment_bytes(256);
        assert!(segmented.fits_budget(&budget));

        let too_wide = ExecPlan::new("npu", "npu0").with_temp_bytes(1024).with_max_segment_bytes(1024);
        assert!(!too_wide.fits_budget(&budget));
    }

    #[test]
    fn signature_reports_element_count_and_metadata() {
        let signature = ExecSignature::operator("matmul", vec!["f32".into()], vec![Some(vec![2, 3]), Some(vec![3, 4])])
            .with_layout("contiguous")
            .with_precision("f32");
        assert_eq!(signature.kind, ExecKind::Operator);
        assert_eq!(signature.element_count(), Some(18));
        assert_eq!(signature.layout.as_deref(), Some("contiguous"));
        assert_eq!(ExecSignature::operator("x", vec![], vec![None]).element_count(), None);
        assert_eq!(ExecSignature::subgraph("mlp", vec![], vec![]).kind, ExecKind::Subgraph);
    }
}
