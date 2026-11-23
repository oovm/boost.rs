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
    x86_64::{collect_reachable_functions, lower_function_to_text_program},
};

/// 普通共享库降低结果：完整共享库镜像与导出事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64LoweredLibrary {
    /// 共享库镜像字节（`PE32+` DLL 或 `ELF64 ET_DYN`）。
    pub executable: Vec<u8>,
    /// 首个导出符号（用于记录产物入口身份；库没有进程入口）。
    pub entry_symbol: Option<String>,
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
    /// 形参载体标签（`i64` / `i64[]` / `utf8`，按形参顺序）；来自冻结签名，
    /// 供外部 C 消费者按清单装载与调用，不在消费侧重新猜测。
    pub parameters: Vec<String>,
    /// 返回载体标签（`i64` / `bool` / `utf8` / `i64[]` / `unit`）。
    pub returns: String,
}

/// 语义类型 → 清单载体标签；未闭合载体保留语义类型文本，避免伪装成已定案 ABI。
fn library_type_tag(ty: &crate::backend_plan_views::SemanticType) -> String {
    use crate::backend_plan_views::SemanticType;
    match ty {
        SemanticType::Integer64 { signed: true } => "i64".to_string(),
        SemanticType::Boolean => "bool".to_string(),
        SemanticType::Utf8 => "utf8".to_string(),
        SemanticType::Unit => "unit".to_string(),
        SemanticType::Array(element) if matches!(element.as_ref(), SemanticType::Integer64 { signed: true }) => "i64[]".to_string(),
        other => other.to_string(),
    }
}

/// 把已冻结的目标计划降低为普通共享库。
pub fn lower_fragment_to_machine_library(submission: &FragmentSubmission, platform: X64Platform) -> Result<X64LoweredLibrary> {
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
        // 载体签名来自冻结函数类型；缺函数体时留空标签，消费侧按未知处理，
        // 不用默认值伪造已定案签名。
        let (parameters, returns) = submission
            .backend_plan
            .get_function(instance)
            .map(|view| {
                (
                    view.function.param_types.iter().map(library_type_tag).collect(),
                    library_type_tag(&view.function.return_type),
                )
            })
            .unwrap_or_default();
        exports.push(MachineLibraryExport {
            source_name: export_name.clone(),
            public_name: export_name.clone(),
            parameters,
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
    let entry_symbol = exports.first().map(|export| export.public_name.clone());
    Ok(X64LoweredLibrary { executable, entry_symbol, exports, artifact_extension })
}
