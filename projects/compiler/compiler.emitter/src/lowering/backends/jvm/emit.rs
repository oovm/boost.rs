//! JVM 目标私有输入与装配原语。
//!
//! 公共 IR 解释阶段已拆除：字节码发射与计划事实结算（方法命名、ABI 契约、
//! launcher、布局/sum companion 类）全部由语言侧 JVM lane 直读 MIR 完成。
//! 架构门 D4 后，源语言类型 → descriptor 的裁决移入语言 × 目标适配器
//! （`language.valkyrie` 的 `pipeline/jvm/carrier.rs`）；本模块只保留 JVM
//! 目标原语（槽位种类与宽度、布局 companion 类命名）与装配服务：
//! [`JvmProgramInput`] 是语言侧一次结算的目标私有输入，[`assemble_jvm_class`]
//! 只接受这个形状，做 companion 载体唯一性自检并封装主类，不再接触任何
//! 跨目标共享程序载荷，也不接收 `SemanticType`。

use std::collections::BTreeSet;

use crate::{
    Result,
    managed_abi::ManagedAbiContract,
    target_operator::TargetOperandType,
    transitional::binary::class::{JvmClassFile, JvmInstruction, JvmMethodSignature, JvmTypeDescriptor},
};
use compiler_types::AggregateLayout;

/// JVM 槽位种类：决定 `iload`/`lload`/`dload` 等指令与宽度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmSlotKind {
    /// `int` / `boolean` / `byte` / `char` / `short`，占 1 槽。
    Int,
    /// `long`，占 2 槽。
    Long,
    /// `float`，占 1 槽。
    Float,
    /// `double`，占 2 槽。
    Double,
    /// 引用，占 1 槽。
    Reference,
}

impl JvmSlotKind {
    pub fn of(descriptor: &JvmTypeDescriptor) -> Result<Self> {
        Ok(match descriptor {
            JvmTypeDescriptor::Void => return Err("`void` 不能作为局部变量或操作数载体".to_string()),
            JvmTypeDescriptor::Long => Self::Long,
            JvmTypeDescriptor::Float => Self::Float,
            JvmTypeDescriptor::Double => Self::Double,
            JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) => Self::Reference,
            JvmTypeDescriptor::Byte
            | JvmTypeDescriptor::Char
            | JvmTypeDescriptor::Int
            | JvmTypeDescriptor::Short
            | JvmTypeDescriptor::Boolean => Self::Int,
        })
    }

    pub fn of_operand_type(operand_type: TargetOperandType) -> Self {
        match operand_type {
            TargetOperandType::I32 => Self::Int,
            TargetOperandType::I64 => Self::Long,
            TargetOperandType::F32 => Self::Float,
            TargetOperandType::F64 => Self::Double,
        }
    }

    pub fn slot_width(self) -> u16 {
        match self {
            Self::Long | Self::Double => 2,
            Self::Int | Self::Float | Self::Reference => 1,
        }
    }

    pub fn load(self, slot: u16) -> JvmInstruction {
        match self {
            Self::Int => JvmInstruction::ILoad(slot),
            Self::Long => JvmInstruction::LLoad(slot),
            Self::Float => JvmInstruction::FLoad(slot),
            Self::Double => JvmInstruction::DLoad(slot),
            Self::Reference => JvmInstruction::ALoad(slot),
        }
    }

    pub fn store(self, slot: u16) -> JvmInstruction {
        match self {
            Self::Int => JvmInstruction::IStore(slot),
            Self::Long => JvmInstruction::LStore(slot),
            Self::Float => JvmInstruction::FStore(slot),
            Self::Double => JvmInstruction::DStore(slot),
            Self::Reference => JvmInstruction::AStore(slot),
        }
    }
}

/// JVM lowering 结果：主类、launcher companion 类与 managed ABI 契约。
#[derive(Debug, Clone)]
pub struct JvmLoweredFragment {
    /// 片段主类。
    pub class_file: JvmClassFile,
    /// 每个导出入口独立的 argv launcher 类。
    pub companion_classes: Vec<JvmClassFile>,
    /// 导出入口 ABI 契约；没有可导出入口时为 `None`。
    pub managed_abi: Option<ManagedAbiContract>,
    /// 程序入口（`[main]`）的 argv launcher 内部类名。
    ///
    /// 入口身份来自冻结的计划，而不是扫描主类里是否恰好有名为 `main` 的方法。
    pub entry_launcher: Option<String>,
}

/// JVM 目标私有输入：一次冻结的方法计划、已降低方法体与 companion 载体。
///
/// 计划事实（可达导出面、方法命名与 descriptor、布局/sum 载体、managed ABI、
/// 入口 launcher）由语言侧 JVM lane 一次结算；装配服务 [`assemble_jvm_class`]
/// 只接受这个形状，不再接触跨目标的共享程序载荷。
pub struct JvmProgramInput {
    internal_name: String,
    methods: Vec<JvmMethodSignature>,
    companion_classes: Vec<JvmClassFile>,
    managed_abi: Option<ManagedAbiContract>,
    entry_launcher: Option<String>,
}

impl JvmProgramInput {
    /// 结算完成的 JVM 目标私有输入：主类内部名、已降低方法体与 companion 载体。
    pub fn new(
        internal_name: String,
        methods: Vec<JvmMethodSignature>,
        companion_classes: Vec<JvmClassFile>,
        managed_abi: Option<ManagedAbiContract>,
        entry_launcher: Option<String>,
    ) -> Self {
        Self { internal_name, methods, companion_classes, managed_abi, entry_launcher }
    }

    /// 主类内部名（由模块名与片段身份一次冻结）。
    pub fn internal_name(&self) -> &str {
        &self.internal_name
    }

    /// 已降低的方法体（按可达导出面的稳定顺序）。
    pub fn methods(&self) -> &[JvmMethodSignature] {
        &self.methods
    }
}

/// JVM 目标私有输入 → 片段产物：只做 companion 载体唯一性自检并封装主类。
///
/// 签名里没有跨目标共享程序：类编码（`JvmClassFile::to_bytes`）与 JAR 容器由
/// [`crate::backend::jvm`] 在同一份目标私有输入上完成。
pub fn assemble_jvm_class(input: &JvmProgramInput) -> Result<JvmLoweredFragment> {
    let mut companion_names = BTreeSet::new();
    for companion in &input.companion_classes {
        if !companion_names.insert(companion.internal_name.clone()) {
            return Err(format!(
                "JVM 最小 lowering：companion 类内部名 `{}` 冲突：布局类、sum 类与 launcher 类必须各有唯一载体",
                companion.internal_name
            ));
        }
    }
    let mut class_file = JvmClassFile::new(input.internal_name().to_string());
    class_file.methods = input.methods().to_vec();
    Ok(JvmLoweredFragment {
        class_file,
        companion_classes: input.companion_classes.clone(),
        managed_abi: input.managed_abi.clone(),
        entry_launcher: input.entry_launcher.clone(),
    })
}

/// 布局 companion 类的稳定内部名：`nyar/layout/<sanitized(name)>$<layout id>`。
///
/// 同一 layout id 稳定、不同布局不撞名；名字只用于 companion 类，不是语义身份。
pub fn jvm_layout_class_name(layout: &AggregateLayout) -> String {
    format!("nyar/layout/{}${}", crate::lowering::sanitize_symbol(&layout.name), layout.id)
}

#[cfg(test)]
mod layout_naming_tests {
    use super::*;
    use compiler_types::StorageKind;

    /// 两个拼写相同的 nominals 必须是两个载体：身份由 layout 记录结算，拼写不参与。
    #[test]
    fn same_spelling_nominals_keep_distinct_carriers() {
        let left = AggregateLayout {
            id: 11,
            name: "Point".to_string(),
            namespace: "app::geometry".to_string(),
            key: None,
            storage: StorageKind::Reference,
            fields: Vec::new(),
        };
        let right = AggregateLayout {
            id: 12,
            name: "Point".to_string(),
            namespace: "app::physics".to_string(),
            key: None,
            storage: StorageKind::Reference,
            fields: Vec::new(),
        };
        assert_ne!(jvm_layout_class_name(&left), jvm_layout_class_name(&right), "同名 nominal 不得折叠到同一载体");
    }
}
