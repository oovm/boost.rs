//! 机器码 lane C ABI 共享库入口：导出面合同 → 目标私有共享库输入。
//!
//! 普通共享库没有进程入口：交付合同是冻结导出面。导出名、形参/返回载体与
//! canonical 参数名在这里闭合，编码与容器仍由 emitter 的装配服务完成。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::{
    Result,
    emitter_backend_machine::{
        LayoutPolicy, MachineLibraryExport, X64LibraryProgramInput, X64ProductTarget, is_c_identifier, is_reference_carrier,
        resolve_physical_layouts,
    },
};
use compiler_target::ResolvedTarget;
use compiler_types::{ItemInstanceId, SemanticType};

use super::super::lane::{FunctionView, LaneContext};
use super::lowerer;

/// x86-64 C ABI 库载体清单：一个物理槽一个稳定标签。
///
/// 载体必须与 x86_64 的物理槽一一对应：标量按精确语言类型命名（`i32` 不并入
/// `i64`），堆数组递归展开元素，引用对象/unite/元组/可空等 8 字节指针统一记
/// `handle`。没有物理槽的类型（值存储聚合、定长数组、`i128`、函数、trait 对象）
/// 结构化失败，禁止把语义类型文本写进清单冒充已定案 ABI。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum X64LibraryCarrier {
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
    /// 由冻结语义类型与布局/sum 表派生载体；不支持即结构化失败。
    pub(crate) fn of(
        ty: &SemanticType,
        aggregate_layouts: &compiler_types::AggregateLayoutPlan,
        sum_layouts: &BTreeMap<SemanticType, compiler_types::SumTypeLayout>,
    ) -> Result<Self> {
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
            SemanticType::Array(element) => Self::Array(Box::new(Self::of(element, aggregate_layouts, sum_layouts)?)),
            other if is_reference_carrier(other, aggregate_layouts, sum_layouts) => Self::Handle,
            other => {
                return Err(format!("`{other}` 没有 x86_64 槽载体：值存储聚合、定长数组、`i128` 与函数类型都不能跨越 C ABI 库边界"))
            }
        })
    }

    /// 清单标签：跨仓消费的稳定字符串，不是调试文本。
    pub(crate) fn tag(&self) -> String {
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

/// canonical 参数身份校验：数量、连续、非空、去重（旧 `canonical_parameter_identities`
/// 的 MIR 等价——MIR 的 `parameter_names` 按位置即 canonical 身份）。
pub(crate) fn canonical_parameter_names(label: &str, view: &FunctionView) -> Result<Vec<String>> {
    let identities: Vec<(usize, &str)> = view.parameter_names.iter().enumerate().map(|(index, name)| (index, name.as_str())).collect();
    if identities.len() != view.param_types.len() {
        return Err(format!(
            "库导出 `{label}` 的入口参数身份数量与参数类型数量不一致: parameter_origins={} param_types={}",
            identities.len(),
            view.param_types.len()
        ));
    }
    let mut identities = identities;
    identities.sort_by_key(|(index, _)| *index);
    let mut seen = BTreeSet::new();
    let mut names = Vec::with_capacity(identities.len());
    for (position, (index, name)) in identities.into_iter().enumerate() {
        if index != position {
            return Err(format!("库导出 `{label}` 参数下标不连续: position={position} index={index}"));
        }
        if name.trim().is_empty() {
            return Err(format!("库导出 `{label}` 参数 {index} 缺少 canonical 参数名"));
        }
        if !seen.insert(name) {
            return Err(format!("库导出 `{label}` 参数 {index} 的参数名 `{name}` 重复"));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// 导出名合同：C 标识符 + 不重名（`platform_name` 只进诊断文案）。
pub(crate) fn validate_library_export_names<'a>(
    export_names: impl Iterator<Item = (&'a ItemInstanceId, &'a String)>,
    platform_name: &str,
) -> Result<Vec<(&'a ItemInstanceId, &'a String)>> {
    let mut names: BTreeMap<&ItemInstanceId, &String> = BTreeMap::new();
    let mut ordered = Vec::new();
    for (instance, export_name) in export_names {
        if !is_c_identifier(export_name) {
            return Err(format!(
                "普通共享库导出名 `{export_name}` 不是合法 C 标识符：外部消费者按名装载，target={platform_name}"
            ));
        }
        if names.insert(instance, export_name).is_some() {
            return Err(format!("普通共享库导出名 `{export_name}` 重复：不得用同名导出覆盖"));
        }
        ordered.push((instance, export_name));
    }
    Ok(ordered)
}

/// 冻结导出面 → 目标私有共享库输入（不编码、不写容器）。
pub(crate) fn build_library_input(ctx: &LaneContext, resolved: &ResolvedTarget) -> Result<X64LibraryProgramInput> {
    let product = X64ProductTarget::from_resolved(resolved).map_err(|error| format!("普通共享库目标不合法：{error}"))?;
    let platform = product.platform();
    if let Some(entry) = ctx.fragment().entry_operation {
        return Err(format!(
            "普通共享库不接受进程入口（`{entry:?}`）：入口是应用交付事实，库交付只公开导出面，target={}",
            platform.name()
        ));
    }

    let reachable = ctx.reachable();
    if reachable.is_empty() {
        return Err(format!("普通共享库没有可达函数：冻结导出面为空，拒绝产出空库，target={}", platform.name()));
    }

    // 导出名必须是 C 标识符：外部消费者按名 `GetProcAddress` / `dlsym` 装载。
    // C ABI 命名合同 = 源码名，注解改名在本 lane 被忽略。
    let mut names: BTreeMap<ItemInstanceId, String> = BTreeMap::new();
    let mut exports = Vec::new();
    for (instance, export_name) in validate_library_export_names(ctx.fragment().export_names.iter(), &platform.name())? {
        // 导出实例的镜像符号名就是公开导出名：容器把符号表整体写成导出目录，
        // 这里漏登记会让导出函数编成内部私有名，外部消费者按名装载直接失败。
        names.insert(*instance, export_name.clone());
        // 载体签名只从冻结函数类型派生：缺函数体或载体不在本 lane 封闭词汇内
        // 都是结构化失败，不写空标签或用语义类型文本冒充已定案 ABI。参数名是
        // 绑定用例字段的唯一依据，缺失同样失败。
        let view = ctx.view(instance).ok_or_else(|| {
            format!("普通共享库：导出 `{export_name}` 缺少冻结函数体，无法派生 C ABI 载体签名，target={}", platform.name())
        })?;
        let parameter_names = canonical_parameter_names(export_name, view)?;
        let parameters = view
            .param_types
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let carrier = X64LibraryCarrier::of(ty, &ctx.linked().aggregate_layouts, ctx.sum_layout_by_semantic_type())
                    .map_err(|reason| format!("普通共享库导出 `{export_name}` 形参 {index} 类型 `{ty}` 没有 C ABI 载体: {reason}"))?;
                if carrier == X64LibraryCarrier::Unit {
                    return Err(format!("普通共享库导出 `{export_name}` 形参 {index} 类型为 `unit`：`unit` 只允许出现在返回位"));
                }
                Ok(carrier.tag())
            })
            .collect::<Result<Vec<_>>>()?;
        let returns = X64LibraryCarrier::of(&view.return_type, &ctx.linked().aggregate_layouts, ctx.sum_layout_by_semantic_type())
            .map_err(|reason| format!("普通共享库导出 `{export_name}` 返回类型 `{}` 没有 C ABI 载体: {reason}", view.return_type))?
            .tag();
        exports.push(MachineLibraryExport {
            source_name: export_name.clone(),
            public_name: export_name.clone(),
            parameters,
            parameter_names,
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
    let layouts = resolve_physical_layouts(&ctx.linked().aggregate_layouts, LayoutPolicy::X64)
        .map_err(|reason| format!("普通共享库降低：物理布局解析失败：{reason}"))?;
    let mut functions = Vec::with_capacity(reachable.len());
    for instance in reachable {
        ctx.function(instance).ok_or_else(|| format!("普通共享库降低：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lowerer::lower_function(ctx, instance, reachable, &names, platform, &layouts)?);
    }

    Ok(X64LibraryProgramInput::new(platform, functions, exports))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_tables() -> (compiler_types::AggregateLayoutPlan, BTreeMap<SemanticType, compiler_types::SumTypeLayout>) {
        (compiler_types::AggregateLayoutPlan::default(), BTreeMap::new())
    }

    fn tag_of(ty: &SemanticType) -> Result<String> {
        let (aggregate_layouts, sum_layouts) = empty_tables();
        X64LibraryCarrier::of(ty, &aggregate_layouts, &sum_layouts).map(|carrier| carrier.tag())
    }

    /// 标量按精确语言类型命名：`i32` 不得并入 `i64`，浮点不得并入整数。
    #[test]
    fn scalar_carriers_use_exact_language_names() {
        let cases = [
            (SemanticType::Boolean, "bool"),
            (SemanticType::Character, "char"),
            (SemanticType::Integer8 { signed: true }, "i8"),
            (SemanticType::Integer8 { signed: false }, "u8"),
            (SemanticType::Integer16 { signed: true }, "i16"),
            (SemanticType::Integer16 { signed: false }, "u16"),
            (SemanticType::Integer32 { signed: true }, "i32"),
            (SemanticType::Integer32 { signed: false }, "u32"),
            (SemanticType::Integer64 { signed: true }, "i64"),
            (SemanticType::Integer64 { signed: false }, "u64"),
            (SemanticType::Float32, "f32"),
            (SemanticType::Float64, "f64"),
            (SemanticType::Utf8, "utf8"),
            (SemanticType::Utf16, "utf16"),
            (SemanticType::Unit, "unit"),
        ];
        for (ty, expected) in cases {
            assert_eq!(tag_of(&ty), Ok(expected.to_string()), "`{ty}` 必须按自身命名");
        }
    }

    /// 数组载体按结构递归派生，不得退化为 `Display` 文本。
    #[test]
    fn array_carriers_are_structural() {
        let i64_array = SemanticType::Array(Box::new(SemanticType::Integer64 { signed: true }));
        assert_eq!(tag_of(&i64_array), Ok("i64[]".to_string()));
        let i32_array = SemanticType::Array(Box::new(SemanticType::Integer32 { signed: true }));
        assert_eq!(tag_of(&i32_array), Ok("i32[]".to_string()), "窄整型数组标签必须与 `i64[]` 同构");
        let nested = SemanticType::Array(Box::new(i64_array));
        assert_eq!(tag_of(&nested), Ok("i64[][]".to_string()));
        let unsupported = SemanticType::Array(Box::new(SemanticType::Integer128 { signed: true }));
        assert!(tag_of(&unsupported).is_err(), "元素无载体时数组也必须失败闭合");
    }

    /// 没有 x86_64 物理槽的类型必须在清单派生处失败，不得写入语义类型文本。
    #[test]
    fn carriers_without_x86_64_slot_fail_closed() {
        let fixed = SemanticType::FixedArray { element: Box::new(SemanticType::Integer64 { signed: true }), length: 4 };
        let named_without_layout = SemanticType::Nominal { layout: 0u32, spelling: compiler_types::Identifier::new("app::Record") };
        for ty in [SemanticType::Integer128 { signed: true }, SemanticType::Bottom, fixed, named_without_layout] {
            let error = tag_of(&ty).expect_err("无槽载体必须结构化失败");
            assert!(error.contains("没有 x86_64 槽载体"), "`{ty:?}` 的错误必须说明缺载体，got: {error}");
        }
    }

    /// 导出名合同：非法 C 标识符与重名都在写出前拒绝；合法名保持源码名即公开名。
    #[test]
    fn library_export_names_validate_c_identifiers() {
        let first = ItemInstanceId::from_index(1).expect("测试实例身份");
        let second = ItemInstanceId::from_index(2).expect("测试实例身份");
        let mut export_names: BTreeMap<ItemInstanceId, String> = BTreeMap::new();
        export_names.insert(first, "foo_bar".to_string());
        export_names.insert(second, "fooBar".to_string());
        let validated = validate_library_export_names(export_names.iter(), "linux-x64").expect("两个合法 C 标识符必须通过");
        assert_eq!(validated.len(), 2, "投影不碰撞时普通库必须保留两个导出");
        export_names.insert(ItemInstanceId::from_index(3).expect("测试实例身份"), "move zeroes".to_string());
        let error = validate_library_export_names(export_names.iter(), "linux-x64").expect_err("空格不是合法 C 标识符");
        assert!(error.contains("不是合法 C 标识符"), "错误必须指名 C 标识符合同，got: {error}");
    }
}
