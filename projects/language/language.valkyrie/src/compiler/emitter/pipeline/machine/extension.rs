//! 机器码 lane 宿主扩展入口：宿主边界合同 → 目标私有扩展输入。
//!
//! 宿主扩展的交付合同是「已冻结导出面 → 类型化宿主方法」：公开名投影由
//! `compiler_target` 宿主合同（[`HostExportNaming`]）完成，语言侧只提交源码名与
//! 注解事实；碰撞拒绝、载体闭合与镜像符号在这里完成；编组包装体、编码与容器
//! 仍由 emitter 的装配服务完成。

use std::collections::BTreeMap;

use compiler_emitter::{
    Result,
    emitter_backend_machine::{
        HostMethod, HostModule, HostParameterKind, HostValueKind, LayoutPolicy, MachineHostExport, MachineTextCarrier, X64ExtensionProgramInput,
        X64Platform, X64ProductTarget, is_c_identifier, resolve_physical_layouts, sanitize_symbol,
    },
};
use compiler_target::{HostExportNameFact, HostExportNaming, ResolvedTarget};
use compiler_types::{ItemInstanceId, SemanticType};

use super::super::lane::LaneContext;
use super::lowerer;

/// 一条已投影的宿主导出：实例身份 + 源码名 + 宿主合同公开名 + 类型化宿主方法。
///
/// 镜像符号（产物）与执行清单（`MachineHostExport`）都从这一份导出表派生，
/// 不再各自重新投影公开名。
#[derive(Debug)]
pub(crate) struct ProjectedHostExport {
    /// 可调用实例身份。
    pub(crate) instance: ItemInstanceId,
    /// 冻结源码声明名。
    pub(crate) source_name: String,
    /// 宿主合同投影后的公开名。
    pub(crate) public_name: String,
    /// 类型化宿主方法（镜像符号与载体签名）。
    pub(crate) method: HostMethod,
}

/// 一条导出的宿主方法投影输入：实例身份、源码名、注解与冻结签名。
pub(crate) struct HostMethodFacts {
    /// 可调用实例身份。
    pub(crate) instance: ItemInstanceId,
    /// 冻结源码名。
    pub(crate) source_name: String,
    /// `[export(name=...)]` 注解名事实；是否生效由宿主命名合同决定。
    pub(crate) annotation: Option<String>,
    /// 冻结形参类型；`None` 表示缺少函数体。
    pub(crate) param_types: Option<Vec<SemanticType>>,
    /// 冻结返回类型。
    pub(crate) return_type: Option<SemanticType>,
}

/// 单个导出 → 类型化宿主方法事实；未闭合的载体在写出前拒绝。
pub(crate) fn host_method_of(target: &X64ProductTarget, export_name: &str, param_types: &[SemanticType], return_type: &SemanticType) -> Result<HostMethod> {
    let limit = target.method_parameter_limit();
    if param_types.len() > limit {
        return Err(format!(
            "宿主扩展方法 `{export_name}` 有 {} 个形参，超出 `{}` 的整数实参上限 {limit}",
            param_types.len(),
            target.name()
        ));
    }
    // 实参载体：完整 `i64`、宿主序列投影的 `[i64]` 堆数组，或 `utf8` 文本。
    // 数组按本 lane 的冻结表示逐元素编组，集合语义仍留在 std。
    let mut parameter_kinds = Vec::with_capacity(param_types.len());
    for parameter in param_types {
        parameter_kinds.push(match parameter {
            SemanticType::Integer64 { signed: true } => HostParameterKind::Int64,
            SemanticType::Array(element) if matches!(element.as_ref(), SemanticType::Integer64 { signed: true }) => HostParameterKind::Int64Array,
            SemanticType::Utf8 => HostParameterKind::Utf8,
            other => {
                return Err(format!(
                    "宿主边界只闭合完整 `i64`、`[i64]` 与 `utf8` 实参：`{export_name}` 的形参类型 `{other}` 没有无损宿主载体，拒绝写出"
                ));
            }
        });
    }
    // CPython 边界的 `PyArg_ParseTuple` 闭合 64 位整数与 UTF-8 字符串：数组投影
    // 只在 Node-API 的序列编组里实现，不能把同一个导出悄悄降级成不完整的宿主载体。
    if target.host_module() == Some(HostModule::Python)
        && parameter_kinds.iter().any(|kind| matches!(kind, HostParameterKind::Int64Array))
    {
        return Err(format!("CPython 宿主边界只闭合 `i64` 与 `utf8` 实参：`{export_name}` 的导出面需要 Node-API 序列编组，拒绝写出"));
    }
    // 返回与实参共用同一符号合同：`u64` 的 `2^63..2^64-1` 段没有有符号
    // `i64` 载体可无损承载，在 BigInt 载体接入前与窄整数一样写出前拒绝。
    let returns = match return_type {
        SemanticType::Integer64 { signed: true } => HostValueKind::Int64,
        SemanticType::Boolean => HostValueKind::Bool,
        SemanticType::Utf8 => HostValueKind::Utf8,
        SemanticType::Array(element) if matches!(element.as_ref(), SemanticType::Integer64 { signed: true }) => {
            // 序列返回只在 Node-API 侧物化；CPython 的列表载体尚未定案，显式拒绝。
            if target.host_module() == Some(HostModule::Python) {
                return Err(format!("CPython 宿主边界尚未闭合 `[i64]` 返回：`{export_name}` 的序列载体只在 Node-API 侧实现，拒绝写出"));
            }
            HostValueKind::Int64Array
        }
        SemanticType::Unit => {
            if target.host_module() == Some(HostModule::Python) {
                return Err(format!("CPython 宿主边界尚未闭合 `unit` 返回：`{export_name}` 拒绝写出"));
            }
            HostValueKind::Unit
        }
        other => {
            return Err(format!(
                "宿主边界尚未闭合返回载体 `{other}`：`{export_name}` 只接受有符号 `i64`、`bool`、`utf8`、`[i64]` 或 `unit` 返回（其余嵌套序列与无符号 `u64` 尚无无损载体），拒绝写出"
            ));
        }
    };
    Ok(HostMethod {
        name: export_name.to_string(),
        symbol: sanitize_symbol(export_name),
        parameters: parameter_kinds,
        returns,
    })
}

/// 从冻结导出面构造宿主合同的最终导出表 + 类型化宿主方法事实。
///
/// 语言侧只提交源码名 + 注解事实；公开名由 [`HostExportNaming`] 一次投影
/// （Node 注解优先、CPython snake_case 忽略注解）。投影后两个不同源码名得到
/// 同一公开名（如 `foo_bar` 与 `fooBar` 都落到 `fooBar`）在发射前由合同拒绝，
/// 诊断同时列出两处源码名与实例身份。
pub(crate) fn project_host_methods(
    facts: &[HostMethodFacts],
    target: &X64ProductTarget,
    naming: HostExportNaming,
    reachable: &[ItemInstanceId],
) -> Result<Vec<ProjectedHostExport>> {
    let name_facts = facts
        .iter()
        .map(|fact| HostExportNameFact { identity: fact.instance, source_name: fact.source_name.clone(), annotation: fact.annotation.clone() })
        .collect::<Vec<_>>();
    let table = naming.project(&name_facts).map_err(|collision| format!("{collision}：拒绝产出互相覆盖的导出面"))?;
    let mut exports = Vec::with_capacity(table.len());
    for entry in table {
        if !reachable.contains(&entry.identity) {
            return Err(format!("宿主扩展：导出实例 `{}`（`{}`）不在可达函数集中", entry.identity, entry.public_name));
        }
        let fact = facts.iter().find(|fact| fact.instance == entry.identity).expect("投影表条目来自导出事实");
        let (param_types, return_type) = match (&fact.param_types, &fact.return_type) {
            (Some(params), Some(ret)) => (params.clone(), ret.clone()),
            _ => {
                return Err(format!("宿主扩展：导出实例 `{}`（`{}`）缺少函数体", entry.identity, entry.public_name));
            }
        };
        let method = host_method_of(target, &entry.public_name, &param_types, &return_type)?;
        exports.push(ProjectedHostExport { instance: entry.identity, source_name: entry.source_name, public_name: entry.public_name, method });
    }
    exports.sort_by(|left, right| left.method.name.cmp(&right.method.name));
    Ok(exports)
}

/// 冻结导出面 → 方法事实（真实链接路径），交宿主合同一次投影并闭合载体。
fn frozen_host_methods(
    ctx: &LaneContext,
    target: &X64ProductTarget,
    naming: HostExportNaming,
    reachable: &[ItemInstanceId],
) -> Result<Vec<ProjectedHostExport>> {
    let facts = ctx
        .fragment()
        .export_names
        .iter()
        .map(|(instance, source_name)| {
            let view = ctx.view(instance);
            HostMethodFacts {
                instance: *instance,
                source_name: source_name.clone(),
                annotation: ctx.fragment().export_annotations.get(instance).cloned(),
                param_types: view.map(|view| view.param_types.clone()),
                return_type: view.map(|view| view.return_type.clone()),
            }
        })
        .collect::<Vec<_>>();
    project_host_methods(&facts, target, naming, reachable)
}

/// 镜像符号名表：导出函数取最终导出表里公开名规约的符号，内部函数按实例身份。
fn extension_symbol_names(
    ctx: &LaneContext,
    reachable: &[ItemInstanceId],
    exports: &[ProjectedHostExport],
) -> Result<BTreeMap<ItemInstanceId, String>> {
    let mut names = BTreeMap::new();
    let mut used: BTreeMap<&str, &str> = BTreeMap::new();
    for export in exports {
        if let Some(previous) = used.insert(export.method.symbol.as_str(), export.public_name.as_str()) {
            return Err(format!("宿主扩展导出名 `{}` 与 `{previous}` 规约成同一镜像符号：拒绝产出互相覆盖的导出面", export.public_name));
        }
    }
    for instance in reachable {
        let name = match exports.iter().find(|export| export.instance == *instance) {
            Some(export) => export.method.symbol.clone(),
            None => format!("__nyar_fn_{}", instance.index()),
        };
        if names.insert(*instance, name).is_some() {
            return Err(format!("宿主扩展：可达操作 `{instance:?}` 被重复登记"));
        }
    }
    Ok(names)
}

/// Windows 宿主导入模块名；Linux 的 `CallImport` 模块名必须为空。
fn host_import_library(target: &X64ProductTarget, platform: X64Platform) -> Result<String> {
    let Some(module) = target.host_module() else {
        return Err("目标不是宿主扩展合同".to_string());
    };
    if !platform.is_windows() {
        return Ok(String::new());
    }
    match module {
        HostModule::Node => Ok("node.exe".to_string()),
        HostModule::Python => target
            .cpython_import_library()
            .map(str::to_string)
            .ok_or_else(|| "Windows CPython 扩展必须提供宿主导入 DLL 名（例如 `python312.dll`）：版本相关名不能由编译器猜".to_string()),
    }
}

/// CPython 的模块名由构建计划冻结一次：文件名、`PyInit_`、清单与 `import`
/// 共用这一个事实，任何消费端不得再按工程名推导。
pub(crate) fn frozen_python_module_name(target: &X64ProductTarget, host: HostModule) -> Result<Option<String>> {
    Ok(match host {
        HostModule::Python => Some(
            target
                .cpython_module_name()
                .ok_or("CPython 目标计划未冻结模块名：构建计划必须经 `freeze_cpython_module_name` 装配后再 lowering")?
                .to_string(),
        ),
        HostModule::Node => None,
    })
}

/// 冻结导出面 → 目标私有宿主扩展输入（不编组宿主包装体、不编码、不写容器）。
pub(crate) fn build_extension_input(ctx: &LaneContext, resolved: &ResolvedTarget) -> Result<X64ExtensionProgramInput> {
    let product = X64ProductTarget::from_resolved(resolved).map_err(|error| format!("宿主扩展目标不合法：{error}"))?;
    let (platform, host) = match &product {
        X64ProductTarget::Extension { platform, module, .. } => (*platform, *module),
        X64ProductTarget::Library(_) => return Err("目标不是宿主扩展合同；普通共享库交付尚未接线".to_string()),
    };
    let python_module = frozen_python_module_name(&product, host)?;

    // 宿主扩展不接受进程入口：入口是应用交付事实，与导出面互相独立。
    if let Some(entry) = ctx.fragment().entry_operation {
        return Err(format!("宿主扩展不接受进程入口（`{entry:?}`）；导出面与入口是两个独立事实，应用交付请改用 native 可执行目标"));
    }

    let reachable = ctx.reachable();
    if reachable.is_empty() {
        return Err(format!("宿主扩展 `{}` 没有可达函数：冻结导出面为空，拒绝产出无导出的扩展", product.name()));
    }

    // 命名策略来自宿主合同：语言侧只提交源码名 + 注解事实，不在此投影。
    let naming = resolved
        .contract
        .export_naming()
        .ok_or_else(|| "宿主扩展缺少宿主导出命名合同：Node/Python 公开名必须由 `compiler_target` 宿主合同投影".to_string())?;

    // 冻结导出面 → 宿主合同一次投影的最终导出表 + 类型化宿主边界事实。
    let exports = frozen_host_methods(ctx, &product, naming, reachable)?;
    if exports.is_empty() {
        return Err(format!("宿主扩展 `{}` 的导出面为空：宿主扩展必须至少公开一个已冻结的导出函数", product.name()));
    }
    let methods: Vec<HostMethod> = exports.iter().map(|export| export.method.clone()).collect();

    // 符号名：导出函数取最终导出表里公开名规约的符号，内部闭包函数取实例身份名。
    let names = extension_symbol_names(ctx, reachable, &exports)?;
    // 与可执行 lane 使用同一物理布局策略：宿主扩展的对象字段偏移不得另算一套。
    let layouts = resolve_physical_layouts(&ctx.linked().aggregate_layouts, LayoutPolicy::X64)
        .map_err(|reason| format!("宿主扩展降低：物理布局解析失败：{reason}"))?;
    let mut functions = Vec::with_capacity(reachable.len() + methods.len() + 2);
    for instance in reachable {
        ctx.function(instance).ok_or_else(|| format!("宿主扩展降低：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lowerer::lower_function(ctx, instance, reachable, &names, platform, &layouts)?);
    }

    let host_library = host_import_library(&product, platform)?;
    // `ELF` 没有 IAT：宿主库以 `DT_NEEDED` 显式声明（名字来自冻结的宿主 ABI 配置）。
    let host_dependencies: Vec<String> = match product.cpython_import_library() {
        Some(library) if !platform.is_windows() && matches!(product.host_module(), Some(HostModule::Python)) => vec![library.to_string()],
        _ => Vec::new(),
    };
    if let Some(module) = &python_module {
        if !is_c_identifier(module) {
            return Err(format!("CPython 扩展模块名 `{module}` 不是合法 C 标识符：`PyInit_<module>` 必须能在 C 中声明"));
        }
    }
    // 扩展后缀优先取解析服务冻结/探测的宿主 ABI 事实；无配置时才按平台规范名
    // 回落（`.pyd` / `.so`），产物身份差异由 ResolvedTarget::identity 承载。
    let artifact_extension = match host {
        HostModule::Node => ".node".to_string(),
        HostModule::Python => product
            .cpython_module_suffix()
            .map(|suffix| if suffix.starts_with('.') { suffix.to_string() } else { format!(".{suffix}") })
            .unwrap_or_else(|| if platform.is_windows() { ".pyd".to_string() } else { ".so".to_string() }),
    };

    // 导出表单一来源：清单条目（`MachineHostExport`）与镜像方法从同一份投影表
    // 派生，不再各自重新投影公开名。清单按冻结顺序（与 methods 一致按公开名
    // 排序）携带源码名 + 公开名 + 载体签名 + 形参声明名：签名只来自上面已闭合的
    // `HostMethod`，参数名来自入口参数的 canonical 身份，消费侧不在清单之外重新
    // 推导或依赖 JSON 键序。
    // 每个宿主导出都必须同时拿到源码名与形参声明名：清单里 `parameters` 与
    // `parameter_names` 按合同等长，缺身份时结构化失败，不写空名冒充无参数。
    let mut host_exports = Vec::with_capacity(exports.len());
    for export in &exports {
        let view = ctx
            .view(&export.instance)
            .ok_or_else(|| format!("宿主扩展导出 `{}` 缺少冻结函数体，无法派生形参声明名", export.public_name))?;
        let parameter_names = super::library::canonical_parameter_names(&export.source_name, view)?;
        let parameters: Vec<String> = export.method.parameters.iter().map(|kind| kind.tag().to_string()).collect();
        if parameter_names.len() != parameters.len() {
            return Err(format!(
                "宿主扩展导出 `{}` 的形参声明名 {} 个与载体 {} 个不一致",
                export.public_name,
                parameter_names.len(),
                parameters.len()
            ));
        }
        host_exports.push(MachineHostExport {
            source_name: export.source_name.clone(),
            public_name: export.public_name.clone(),
            parameters,
            parameter_names,
            returns: export.method.returns.tag().to_string(),
        });
    }

    // 文本载体描述按需提交：只有导出面实际编组 utf8 时，语言侧结算的布局事实
    // 才进入桥输入；不含 utf8 载体的扩展不携带，桥也不默认构造或读回。
    let text_carrier = if methods.iter().any(HostMethod::uses_text_carrier) {
        let layout = crate::compiler::valkyrie_mir::UTF8_TEXT_CARRIER_LAYOUT;
        Some(MachineTextCarrier { repr_field_byte_offset: layout.repr_field_byte_offset, object_field_slots: layout.object_field_slots })
    } else {
        None
    };

    Ok(X64ExtensionProgramInput::new(
        platform,
        host,
        host_library,
        host_dependencies,
        functions,
        methods,
        host_exports,
        python_module,
        artifact_extension,
        text_carrier,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler_emitter::emitter_backend_machine::X64Platform;

    fn instance(index: u32) -> ItemInstanceId {
        ItemInstanceId::from_index(index).expect("测试实例身份")
    }

    fn i64_ty() -> SemanticType {
        SemanticType::Integer64 { signed: true }
    }

    fn facts(
        entries: &[(&str, Option<&str>, Vec<SemanticType>, SemanticType)],
    ) -> Vec<HostMethodFacts> {
        entries
            .iter()
            .enumerate()
            .map(|(index, (source, annotation, params, ret))| HostMethodFacts {
                instance: instance(index as u32 + 1),
                source_name: (*source).to_string(),
                annotation: annotation.map(|value| value.to_string()),
                param_types: Some(params.clone()),
                return_type: Some(ret.clone()),
            })
            .collect()
    }

    fn reachable_of(facts: &[HostMethodFacts]) -> Vec<ItemInstanceId> {
        facts.iter().map(|fact| fact.instance).collect()
    }

    fn node_target(platform: X64Platform) -> X64ProductTarget {
        X64ProductTarget::extension(platform, HostModule::Node)
    }

    /// Node 命名合同（宿主合同策略）：无注解时 snake_case 源码名自动 camelCase 投影。
    #[test]
    fn node_extension_projects_snake_case_source_name() {
        let target = node_target(X64Platform::linux_x64());
        let facts = facts(&[("move_zeroes", None, vec![i64_ty()], i64_ty())]);
        let exports =
            project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts)).expect("Node 导出面必须闭合");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].method.name, "moveZeroes", "Node 命名合同 = snake_case 源码名自动 camelCase 投影");
        assert_eq!(exports[0].source_name, "move_zeroes", "导出表必须保留源码名事实");
    }

    /// Node 命名合同：显式 `[export(name=...)]` 注解名优先于自动投影。
    #[test]
    fn node_extension_honors_explicit_export_annotation() {
        let target = node_target(X64Platform::linux_x64());
        let facts = facts(&[("move_zeroes", Some("customRun"), vec![i64_ty()], i64_ty())]);
        let exports =
            project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts)).expect("Node 注解导出面必须闭合");
        assert_eq!(exports[0].public_name, "customRun", "Node 命名合同 = 注解名优先");
    }

    /// CPython 命名合同：camelCase 源码名自动 snake_case 投影，显式注解改名被忽略。
    #[test]
    fn cpython_projects_camel_case_source_name_to_snake_case_and_ignores_annotation() {
        let target = X64ProductTarget::cpython_extension(X64Platform::linux_x64(), "math_utils");
        let facts = facts(&[("twoSum", Some("customRun"), vec![i64_ty()], i64_ty())]);
        let exports = project_host_methods(&facts, &target, HostExportNaming::CpythonSnakeCase, &reachable_of(&facts))
            .expect("CPython 导出面必须闭合");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].method.name, "two_sum", "CPython 命名合同 = camelCase 源码名自动 snake_case 投影");
    }

    /// 投影碰撞（`two_sum` 与 `twoSum` 同落 `two_sum`）在写出前拒绝，不静默覆盖。
    #[test]
    fn cpython_export_projection_collision_is_rejected_before_image() {
        let target = X64ProductTarget::cpython_extension(X64Platform::linux_x64(), "math_utils");
        let facts = facts(&[("two_sum", None, vec![i64_ty()], i64_ty()), ("twoSum", None, vec![i64_ty()], i64_ty())]);
        let error =
            project_host_methods(&facts, &target, HostExportNaming::CpythonSnakeCase, &reachable_of(&facts)).expect_err("投影碰撞必须拒绝");
        assert!(error.contains("two_sum") && error.contains("twoSum"), "错误必须同时列出两处源码名，got: {error}");
    }

    /// Node camelCase 投影碰撞（`foo_bar` 与 `fooBar` 同落 `fooBar`）在写出前拒绝。
    #[test]
    fn node_projection_collision_is_rejected_with_both_source_names() {
        let target = node_target(X64Platform::linux_x64());
        let facts = facts(&[("foo_bar", None, vec![i64_ty()], i64_ty()), ("fooBar", None, vec![i64_ty()], i64_ty())]);
        let error = project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts))
            .expect_err("投影碰撞必须写出前拒绝");
        assert!(error.contains("foo_bar") && error.contains("fooBar"), "诊断应同时列出两处源码名，got: {error}");
    }

    /// CPython 的模块名是构建计划已冻结事实：未冻结时 lowering 拒绝，不接收工程名。
    #[test]
    fn cpython_extension_rejects_unfrozen_module_name() {
        // 未冻结 `Cpython(module)` 的 CPython 目标：lowering 只消费冻结事实。
        let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Python);
        let error = frozen_python_module_name(&target, HostModule::Python).expect_err("未冻结模块名必须拒绝");
        assert!(error.contains("未冻结"), "错误必须指名模块名未冻结，got: {error}");
    }

    /// Node-API 整数参数窗口固定：混合 int/float 签名超出合同，写出前拒绝。
    #[test]
    fn node_extension_rejects_mixed_int_float_parameter() {
        let target = node_target(X64Platform::linux_x64());
        let facts = facts(&[("mix_args", None, vec![i64_ty(), SemanticType::Float64], i64_ty())]);
        let error = project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts))
            .expect_err("混合 int/float 签名必须拒绝");
        assert!(error.contains("没有无损宿主载体"), "错误必须指名宿主载体合同，got: {error}");
    }

    /// Windows Node 扩展同样拒绝混合 int/float 签名（Win64 与 SysV64 共用合同）。
    #[test]
    fn windows_node_extension_rejects_mixed_int_float_parameter() {
        let target = node_target(X64Platform::windows_x64());
        let facts = facts(&[("mix_args", None, vec![i64_ty(), SemanticType::Float64], i64_ty())]);
        let error = project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts))
            .expect_err("混合 int/float 签名必须拒绝");
        assert!(error.contains("没有无损宿主载体"), "错误必须指名宿主载体合同，got: {error}");
    }

    /// 导出表单一来源：镜像方法名、公开名与源码名同行携带，符号由公开名规约。
    #[test]
    fn projected_export_carries_symbol_with_public_name() {
        let target = node_target(X64Platform::linux_x64());
        let facts = facts(&[("move_zeroes", None, vec![i64_ty()], i64_ty())]);
        let exports = project_host_methods(&facts, &target, HostExportNaming::NodeAnnotationFirst, &reachable_of(&facts)).expect("导出面必须闭合");
        assert_eq!(exports[0].method.symbol, sanitize_symbol(&exports[0].public_name), "镜像符号必须从同一公开名派生");
        assert_eq!(exports[0].instance, instance(1), "导出表条目保留调用方身份");
    }
}
