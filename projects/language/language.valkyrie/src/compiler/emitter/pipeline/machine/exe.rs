//! 机器码 lane 进程镜像入口：用途/合同门 → 四元组分派 → 目标私有输入。
//!
//! 与旧 emitter 入口逐段对应：先校验产物用途与合同，再按规范目标四元组冻结
//! 调用约定与容器，最后把 MIR 闭包降成 [`X64ProgramInput`]。未实现的四元组
//! 失败闭合，指名具体四元组。

use std::collections::BTreeMap;

use compiler_emitter::{
    Result,
    emitter_backend_machine::{LayoutPolicy, X64CallConv, X64Platform, X64ProgramInput, is_exit_code_type, resolve_physical_layouts},
};
use compiler_target::abstractions::{CanonicalAbi, CanonicalArch, CanonicalSpecification};
use compiler_target::ResolvedTarget;
use compiler_types::ItemInstanceId;

use super::super::lane::{FunctionView, LaneContext};
use super::lowerer;

/// 进程可执行用途（`Application` / `TestBundle`）的目标私有输入。
///
/// 只接受进程可执行用途与普通原生合同：库交付与宿主扩展分别由库 lane 与
/// 宿主扩展降低承载，在这里结构化拒绝，不把进程镜像改标成其他产物。入口身份
/// 必须已由片段冻结；缺失时由 lowering 报错，不回填默认名。
pub(crate) fn build_program_input(ctx: &LaneContext, resolved: &ResolvedTarget, purpose: compiler_target::ArtifactPurpose) -> Result<X64ProgramInput> {
    match purpose {
        compiler_target::ArtifactPurpose::Application | compiler_target::ArtifactPurpose::TestBundle => {}
        compiler_target::ArtifactPurpose::Library => {
            return Err("机器码库交付尚未接线：拒绝把进程镜像改标为库产物".to_string());
        }
        compiler_target::ArtifactPurpose::HostExtension => {
            return Err("宿主扩展必须走宿主扩展降低：进程镜像不接受宿主扩展用途".to_string());
        }
    }
    match &resolved.contract {
        compiler_target::ArtifactContract::Native => {}
        compiler_target::ArtifactContract::Platform => {
            return Err(format!("机器码进程镜像不接受平台合同 `{}`：请使用 `native` 或显式机器目标", resolved.identity()));
        }
        compiler_target::ArtifactContract::NodeApi | compiler_target::ArtifactContract::Cpython(_) => {
            return Err(format!(
                "机器码进程镜像不接受 `{}` 宿主合同：宿主扩展必须走宿主扩展降低",
                resolved.contract.as_str()
            ));
        }
    }
    build_machine_program_input(ctx, resolved.platform)
}

/// 目标四元组 → 目标私有 x86-64 输入。
///
/// 四元组是唯一分派依据；`TargetFamily::MachineCode` 只说明产物形态，不能用来选
/// 编码、容器或 ABI。未实现的目标四元组必须失败闭合，指名具体四元组。
fn build_machine_program_input(ctx: &LaneContext, target: compiler_target::CanonicalTarget) -> Result<X64ProgramInput> {
    match target.arch {
        CanonicalArch::X86_64 => match target.specification {
            CanonicalSpecification::Windows => match target.abi {
                // x86-64 Windows 的 MSVC 与 GNU 目标共用 Win64 调用约定与 PE 容器。
                None | Some(CanonicalAbi::Msvc) | Some(CanonicalAbi::Gnu) => build_x86_64_program_input(ctx, X64CallConv::Win64, true),
                Some(other) => Err(format!(
                    "机器码后端不支持 `x86_64` + `windows` + `{}` 的 ABI：只闭合 Win64 调用约定（target={}）",
                    abi_name(other),
                    target.as_canonical_str()
                )),
            },
            // x86-64 Linux 只有 SysV64 调用约定 + ELF 容器；不借用 Win64 编码。
            CanonicalSpecification::Linux => match target.abi {
                None | Some(CanonicalAbi::Gnu) | Some(CanonicalAbi::SystemV) => build_x86_64_program_input(ctx, X64CallConv::SystemV64, false),
                Some(other) => Err(format!(
                    "机器码后端不支持 `x86_64` + `linux` + `{}` 的 ABI：只闭合 SysV64 调用约定（target={}）",
                    abi_name(other),
                    target.as_canonical_str()
                )),
            },
            other => Err(format!(
                "机器码后端没有 `x86_64` + `{}` 的容器实现：拒绝借用其他平台的镜像格式（target={}）",
                specification_name(other),
                target.as_canonical_str()
            )),
        },
        other => Err(format!("机器码后端尚未实现架构 `{}` 的机器码降低（target={}）", other.as_str(), target.as_canonical_str())),
    }
}

/// MIR 闭包事实 → 目标私有 LIR 程序（不编码、不写容器）。
///
/// 这是语言语义最后一次以函数级形状出现的地方；返回值之后的装配服务只认
/// [`X64ProgramInput`]。
fn build_x86_64_program_input(ctx: &LaneContext, abi: X64CallConv, windows: bool) -> Result<X64ProgramInput> {
    let container = if windows { "PE" } else { "ELF" };
    let reachable = ctx.reachable();
    if reachable.is_empty() {
        return Err(format!("x86_64 lowering 没有可达函数：片段既无导出也无入口操作，拒绝凭空生成 {container}"));
    }

    let entry = ctx
        .fragment()
        .entry_operation
        .ok_or_else(|| format!("x86_64 lowering 缺少入口操作：{container} 入口点必须来自计划冻结的 `entry_operation`"))?;
    let entry_view = ctx.view(&entry).ok_or_else(|| {
        format!("x86_64 lowering：入口操作 `{entry:?}` 没有函数体，不能作为 {container} 入口")
    })?;
    validate_entry(ctx, &entry, entry_view, container)?;

    let names = exe_symbol_names(reachable);
    // 容器与调用约定由规范目标一次冻结；数组运行时按同一平台身份选择分配/释放实现。
    let platform = if windows { X64Platform::windows_x64() } else { X64Platform::linux_x64() };
    if platform.abi() != abi {
        return Err(format!(
            "x86_64 lowering：容器 {container} 的冻结 ABI `{}` 与调用约定 `{}` 不一致",
            platform.abi().name(),
            abi.name()
        ));
    }
    let mut functions = Vec::with_capacity(reachable.len());
    // 物理布局按规范平台的引用/文本槽策略一次解析；对象字段偏移只来自这里。
    let layouts = resolve_physical_layouts(&ctx.linked().aggregate_layouts, LayoutPolicy::X64)
        .map_err(|reason| format!("x86_64 lowering：物理布局解析失败：{reason}"))?;
    for instance in reachable {
        ctx.function(instance).ok_or_else(|| format!("x86_64 lowering：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lowerer::lower_function(ctx, instance, reachable, &names, platform, &layouts)?);
    }

    let entry_name = names.get(&entry).expect("入口实例在可达函数符号表内").clone();
    Ok(X64ProgramInput::new(platform, functions, entry_name))
}

/// exe 镜像内的符号名：实例身份是唯一权威，名字不参与语义，也不跨产物复用。
fn exe_symbol_names(reachable: &[ItemInstanceId]) -> BTreeMap<ItemInstanceId, String> {
    reachable.iter().map(|instance| (*instance, format!("__nyar_fn_{}", instance.index()))).collect()
}

/// 入口函数必须是空形参、返回整数/布尔/字符或 unit。
///
/// 两个容器都以 `rax` 作为进程退出码（PE loader 读取返回值，ELF 入口桩把它传给
/// `exit`）；loader 不提供有意义的形参，因此入口只接受可直接作为退出码的标量返回。
fn validate_entry(ctx: &LaneContext, entry: &ItemInstanceId, view: &FunctionView, container: &str) -> Result<()> {
    let function = ctx
        .function(entry)
        .ok_or_else(|| format!("x86_64 lowering：入口函数 `{}` 缺少入口块", view.symbol))?;
    let entry_block = function
        .blocks
        .get(&function.entry)
        .ok_or_else(|| format!("x86_64 lowering：入口函数 `{}` 缺少入口块", view.symbol))?;
    if !entry_block.parameters.is_empty() || !view.param_types.is_empty() {
        return Err(format!(
            "x86_64 lowering：{container} 入口函数 `{}` 必须无参数（loader 不传递形参），实际 {} 个",
            view.symbol,
            view.param_types.len()
        ));
    }
    if !is_exit_code_type(&view.return_type) {
        return Err(format!(
            "x86_64 lowering：{container} 入口函数 `{}` 的返回类型 `{}` 不能作为进程退出码（只接受整数/布尔/字符/unit）",
            view.symbol, view.return_type
        ));
    }
    Ok(())
}

fn specification_name(specification: CanonicalSpecification) -> &'static str {
    match specification {
        CanonicalSpecification::Unknown => "unknown",
        CanonicalSpecification::Browser => "browser",
        CanonicalSpecification::Wasi => "wasi",
        CanonicalSpecification::Windows => "windows",
        CanonicalSpecification::Linux => "linux",
        CanonicalSpecification::MacOs => "macos",
        CanonicalSpecification::Android => "android",
        CanonicalSpecification::Ios => "ios",
    }
}

fn abi_name(abi: CanonicalAbi) -> &'static str {
    match abi {
        CanonicalAbi::Managed => "managed",
        CanonicalAbi::Clr => "clr",
        CanonicalAbi::Jvm => "jvm",
        CanonicalAbi::WebAssembly => "wasm",
        CanonicalAbi::Wasi => "wasi",
        CanonicalAbi::WasiP3 => "wasip3",
        CanonicalAbi::Msvc => "msvc",
        CanonicalAbi::Gnu => "gnu",
        CanonicalAbi::SystemV => "systemv",
        CanonicalAbi::Aapcs64 => "aapcs64",
    }
}
