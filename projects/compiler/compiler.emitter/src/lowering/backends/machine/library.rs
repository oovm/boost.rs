//! 普通原生共享库降低（`artifact: "library"` + 机器码目标）。
//!
//! 复用与可执行/宿主扩展相同的执行链：计划事实 → 文本 LIR（[`super::x86_64`]）
//! → 唯一程序装配器（[`super::x64_program`]）→ 共享库容器
//! （[`super::container`]）。本模块只把**已冻结的导出面**映射成共享库符号，
//! 不合成进程入口、不绑定任何宿主模块合同。
//!
//! 共享库不接受进程入口：入口是应用交付事实，与导出面互相独立。
//! 导出名必须是合法 C 标识符，外部消费者（C/Python ctypes/其他宿主）按名装载。

use std::collections::BTreeMap;

use crate::{
    FragmentSubmission, Result,
    lowering::shared::physical_layout::{LayoutPolicy, resolve_physical_layouts},
};
use compiler_types::ItemInstanceId;

use super::{
    X64Platform, compile_shared_container, extension::verify_shared_image, is_c_identifier,
    x64_program::encode_x64_program_with_dependencies,
    x86_64::{collect_reachable_functions, is_reference_carrier, lower_function_to_text_program},
};

/// 普通共享库降低结果：完整共享库镜像与导出事实。
///
/// 库没有单一入口：交付合同是 [`Self::exports`] 的导出索引，外部消费者按
/// `LoadLibrary`/`dlopen` 的名字装载。这里不提供「首个导出」之类的入口符号，
/// 避免把任意一个导出伪装成库入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64LoweredLibrary {
    /// 共享库镜像字节（`PE32+` DLL 或 `ELF64 ET_DYN`）。
    pub executable: Vec<u8>,
    /// 对外可见导出面（源码名 + 公开名，按冻结导出面顺序）。
    pub exports: Vec<MachineLibraryExport>,
    /// 产物扩展名（含点）：`.dll` / `.so`。
    pub artifact_extension: String,
}

/// 普通共享库的一条 C ABI 导出：语言侧源码名与目标公开名。
///
/// C ABI 的命名合同就是源码名（忽略 `name:` / `case:` 注解改名）；公开名当前
/// 恒等于源码名，保留双字段是为了与宿主扩展/wasm 交付同一清单合同形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineLibraryExport {
    /// 语言侧冻结的源码声明名。
    pub source_name: String,
    /// C ABI 公开符号名（本 lane 合同下等于源码名）。
    pub public_name: String,
    /// 形参载体标签（`i32` / `i64` / `f64` / `utf8` / `i64[]` / `handle`，按形参
    /// 顺序）；来自冻结签名，供外部 C 消费者按清单装载与调用，不在消费侧重新猜测。
    pub parameters: Vec<String>,
    /// 形参声明名（按形参顺序，与 `parameters` 等长）；消费侧按名绑定用例字段。
    pub parameter_names: Vec<String>,
    /// 返回载体标签（`unit` / `bool` / `char` / `i64` / `utf8` / `i64[]` / `handle`）。
    pub returns: String,
}

/// 普通共享库 C ABI 载体：本 lane 的封闭词汇。
///
/// 载体必须与 x86_64 的物理槽一一对应：标量按精确语言类型命名（`i32` 不并入
/// `i64`），堆数组递归展开元素，引用对象/unite/元组/可空等 8 字节指针统一记
/// `handle`。没有物理槽的类型（值存储聚合、定长数组、`i128`、函数、trait 对象）
/// 结构化失败，禁止把语义类型文本写进清单冒充已定案 ABI。
#[derive(Debug, Clone, PartialEq, Eq)]
enum X64LibraryCarrier {
    Unit,
    Bool,
    Char,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Utf8,
    Utf16,
    Array(Box<Self>),
    Handle,
}

impl X64LibraryCarrier {
    /// 由冻结语义类型与计划事实派生载体；不支持即结构化失败。
    fn of(ty: &crate::backend_plan_views::SemanticType, plan: &crate::TargetExecutionPlan) -> Result<Self> {
        use crate::backend_plan_views::SemanticType;
        Ok(match ty {
            SemanticType::Unit => Self::Unit,
            SemanticType::Boolean => Self::Bool,
            SemanticType::Character => Self::Char,
            SemanticType::Integer8 { signed: true } => Self::I8,
            SemanticType::Integer8 { signed: false } => Self::U8,
            SemanticType::Integer16 { signed: true } => Self::I16,
            SemanticType::Integer16 { signed: false } => Self::U16,
            SemanticType::Integer32 { signed: true } => Self::I32,
            SemanticType::Integer32 { signed: false } => Self::U32,
            SemanticType::Integer64 { signed: true } => Self::I64,
            SemanticType::Integer64 { signed: false } => Self::U64,
            SemanticType::Float32 => Self::F32,
            SemanticType::Float64 => Self::F64,
            SemanticType::Utf8 => Self::Utf8,
            SemanticType::Utf16 => Self::Utf16,
            SemanticType::Array(element) => Self::Array(Box::new(Self::of(element, plan)?)),
            other if is_reference_carrier(other, plan) => Self::Handle,
            other => {
                return Err(format!("`{other}` 没有 x86_64 槽载体：值存储聚合、定长数组、`i128` 与函数类型都不能跨越 C ABI 库边界"))
            }
        })
    }

    /// 清单标签：跨仓消费的稳定字符串，不是调试文本。
    fn tag(&self) -> String {
        match self {
            Self::Unit => "unit".to_string(),
            Self::Bool => "bool".to_string(),
            Self::Char => "char".to_string(),
            Self::I8 => "i8".to_string(),
            Self::U8 => "u8".to_string(),
            Self::I16 => "i16".to_string(),
            Self::U16 => "u16".to_string(),
            Self::I32 => "i32".to_string(),
            Self::U32 => "u32".to_string(),
            Self::I64 => "i64".to_string(),
            Self::U64 => "u64".to_string(),
            Self::F32 => "f32".to_string(),
            Self::F64 => "f64".to_string(),
            Self::Utf8 => "utf8".to_string(),
            Self::Utf16 => "utf16".to_string(),
            Self::Array(element) => format!("{}[]", element.tag()),
            Self::Handle => "handle".to_string(),
        }
    }
}

/// 把已冻结的目标计划降低为普通共享库。
pub fn lower_fragment_to_machine_library(submission: &FragmentSubmission, platform: X64Platform) -> Result<X64LoweredLibrary> {
    super::validate_machine_submission(submission)?;
    if let Some(entry) = submission.backend_plan.entry_operation() {
        return Err(format!(
            "普通共享库不接受进程入口（`{entry:?}`）：入口是应用交付事实，库交付只公开导出面，target={}",
            platform.name()
        ));
    }

    let reachable = collect_reachable_functions(submission);
    if reachable.is_empty() {
        return Err(format!("普通共享库没有可达函数：冻结导出面为空，拒绝产出空库，target={}", platform.name()));
    }

    // 导出名必须是 C 标识符：外部消费者按名 `GetProcAddress` / `dlsym` 装载。
    // C ABI 命名合同 = 源码名，注解改名在本 lane 被忽略。
    let mut names: BTreeMap<ItemInstanceId, String> = BTreeMap::new();
    let mut exports = Vec::new();
    for (instance, export_name) in submission.backend_plan.export_names() {
        if !is_c_identifier(export_name) {
            return Err(format!(
                "普通共享库导出名 `{export_name}` 不是合法 C 标识符：外部消费者按名装载，target={}",
                platform.name()
            ));
        }
        if names.insert(*instance, export_name.clone()).is_some() {
            return Err(format!("普通共享库导出名 `{export_name}` 重复：不得用同名导出覆盖", ));
        }
        // 载体签名只从冻结函数类型派生：缺函数体或载体不在本 lane 封闭词汇内
        // 都是结构化失败，不写空标签或用语义类型文本冒充已定案 ABI。参数名是
        // 绑定用例字段的唯一依据，缺失同样失败。
        let view = submission.backend_plan.get_function(instance).ok_or_else(|| {
            format!("普通共享库：导出 `{export_name}` 缺少冻结函数体，无法派生 C ABI 载体签名，target={}", platform.name())
        })?;
        let names = crate::abi_structure::canonical_parameter_identities(export_name, &view.function)?
            .into_iter()
            .map(|(_, name)| name.to_string())
            .collect::<Vec<_>>();
        let parameters = view
            .function
            .param_types
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let carrier = X64LibraryCarrier::of(ty, &submission.backend_plan)
                    .map_err(|reason| format!("普通共享库导出 `{export_name}` 形参 {index} 类型 `{ty}` 没有 C ABI 载体: {reason}"))?;
                if carrier == X64LibraryCarrier::Unit {
                    return Err(format!("普通共享库导出 `{export_name}` 形参 {index} 类型为 `unit`：`unit` 只允许出现在返回位"));
                }
                Ok(carrier.tag())
            })
            .collect::<Result<Vec<_>>>()?;
        let returns = X64LibraryCarrier::of(&view.function.return_type, &submission.backend_plan)
            .map_err(|reason| format!("普通共享库导出 `{export_name}` 返回类型 `{}` 没有 C ABI 载体: {reason}", view.function.return_type))?
            .tag();
        exports.push(MachineLibraryExport {
            source_name: export_name.clone(),
            public_name: export_name.clone(),
            parameters,
            parameter_names: names,
            returns,
        });
    }
    if exports.is_empty() {
        return Err(format!(
            "普通共享库必须至少公开一个 `[export]` 函数：无导出面拒绝产出库产物，target={}",
            platform.name()
        ));
    }

    // 内部闭包函数取确定性的私有符号名，不占用公开导出名。
    for (index, instance) in reachable.iter().enumerate() {
        if names.contains_key(instance) {
            continue;
        }
        names.insert(*instance, format!("__nyar_library_internal_{index}"));
    }

    // 与可执行/扩展 lane 使用同一物理布局策略：库对象的字段偏移不得另算一套。
    let layouts = resolve_physical_layouts(submission.backend_plan.aggregate_layouts(), LayoutPolicy::X64)
        .map_err(|reason| format!("普通共享库降低：物理布局解析失败：{reason}"))?;
    let mut functions = Vec::with_capacity(reachable.len());
    for instance in &reachable {
        let view = submission
            .backend_plan
            .get_function(instance)
            .ok_or_else(|| format!("普通共享库降低：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lower_function_to_text_program(
            instance,
            submission,
            &view.function,
            &reachable,
            &names,
            platform,
            &layouts,
        )?);
    }

    // 普通库不绑定宿主模块：没有导入面、没有宿主依赖、没有文本数据段。
    let image = encode_x64_program_with_dependencies(&functions, &[], platform.abi(), &[], &[], &[])?;
    let executable = compile_shared_container(platform, &image)?;
    verify_shared_image(&executable, platform)?;

    let artifact_extension = if platform.is_windows() { ".dll".to_string() } else { ".so".to_string() };
    Ok(X64LoweredLibrary { executable, exports, artifact_extension })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend_plan_views::SemanticType;

    fn tag_of(ty: &SemanticType) -> Result<String> {
        X64LibraryCarrier::of(ty, &crate::TargetExecutionPlan::default()).map(|carrier| carrier.tag())
    }

    fn primitive(spelling: &str) -> SemanticType {
        crate::test_canonical_primitive(spelling)
    }

    /// 标量按精确语言类型命名：`i32` 不得并入 `i64`，浮点不得并入整数。
    #[test]
    fn scalar_carriers_use_exact_language_names() {
        for spelling in ["bool", "char", "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "f32", "f64", "utf8", "utf16"] {
            assert_eq!(tag_of(&primitive(spelling)), Ok(spelling.to_string()), "`{spelling}` 必须按自身命名");
        }
        assert_eq!(tag_of(&SemanticType::Unit), Ok("unit".to_string()));
    }

    /// 数组载体按结构递归派生，不得退化为 `Display` 文本（`[i32]` 不是 `[i32]`）。
    #[test]
    fn array_carriers_are_structural() {
        let i64_array = SemanticType::Array(Box::new(primitive("i64")));
        assert_eq!(tag_of(&i64_array), Ok("i64[]".to_string()));
        let i32_array = SemanticType::Array(Box::new(primitive("i32")));
        assert_eq!(tag_of(&i32_array), Ok("i32[]".to_string()), "窄整型数组标签必须与 `i64[]` 同构");
        let nested = SemanticType::Array(Box::new(i64_array));
        assert_eq!(tag_of(&nested), Ok("i64[][]".to_string()));
        let unsupported = SemanticType::Array(Box::new(primitive("i128")));
        assert!(tag_of(&unsupported).is_err(), "元素无载体时数组也必须失败闭合");
    }

    /// 没有 x86_64 物理槽的类型必须在清单派生处失败，不得写入语义类型文本。
    #[test]
    fn carriers_without_x86_64_slot_fail_closed() {
        let fixed = SemanticType::FixedArray { element: Box::new(primitive("i64")), length: 4 };
        let named_without_layout = SemanticType::Nominal {
            layout: 0u32,
            spelling: compiler_types::Identifier::new("app::Record"),
        };
        for ty in [primitive("i128"), primitive("void"), fixed, named_without_layout] {
            let error = tag_of(&ty).expect_err("无槽载体必须结构化失败");
            assert!(error.contains("没有 x86_64 槽载体"), "`{ty:?}` 的错误必须说明缺载体，got: {error}");
        }
    }
}
