//! 普通原生共享库降低（`artifact: "library"` + 机器码目标）。
//!
//! 复用与可执行/宿主扩展相同的执行链：计划事实 → 文本 LIR（[`super::x86_64`]）
//! → 唯一程序装配器（[`super::x64_program`]）→ 共享库容器
//! （[`super::container`]）。本模块只把**已冻结的导出面**映射成共享库符号，
//! 不合成进程入口、不绑定任何宿主模块合同。
//!
//! 共享库不接受进程入口：入口是应用交付事实，与导出面互相独立。
//! 导出名必须是合法 C 标识符，外部消费者（C/Python ctypes/其他宿主）按名装载。

use crate::Result;

use super::{
    X64Platform, X64ProgramFunction, compile_shared_container, extension::verify_shared_image,
    x64_program::encode_x64_program_with_dependencies,
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

/// 普通共享库的目标私有输入：已降低的 LIR 程序 + 冻结导出描述符。
///
/// 装配服务（编码 + 共享库容器 + 自检）只接受这个形状，不再接触跨目标的
/// `FragmentSubmission` / `ExecutableFunction`。
pub struct X64LibraryProgramInput {
    platform: X64Platform,
    functions: Vec<X64ProgramFunction>,
    exports: Vec<MachineLibraryExport>,
}

impl X64LibraryProgramInput {
    /// 语言适配器构造目标私有共享库输入：容器与调用约定由平台身份一次冻结。
    pub fn new(platform: X64Platform, functions: Vec<X64ProgramFunction>, exports: Vec<MachineLibraryExport>) -> Self {
        Self { platform, functions, exports }
    }

    /// 目标平台身份（容器与调用约定一次冻结）。
    pub fn platform(&self) -> X64Platform {
        self.platform
    }
}

/// 目标私有共享库输入 → 共享库镜像（编码 + 容器 + 自检）。
pub(crate) fn assemble_x64_library_image(input: &X64LibraryProgramInput) -> Result<X64LoweredLibrary> {
    // 普通库不绑定宿主模块：没有导入面、没有宿主依赖、没有文本数据段。
    let image = encode_x64_program_with_dependencies(&input.functions, &[], input.platform.abi(), &[], &[], &[])?;
    let executable = compile_shared_container(input.platform, &image)?;
    verify_shared_image(&executable, input.platform)?;

    let artifact_extension = if input.platform.is_windows() { ".dll".to_string() } else { ".so".to_string() };
    Ok(X64LoweredLibrary { executable, exports: input.exports.clone(), artifact_extension })
}


