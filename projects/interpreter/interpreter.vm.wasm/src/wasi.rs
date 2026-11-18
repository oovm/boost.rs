//! WASI preview1 与 component-model 的宿主边界合同。
//!
//! 本模块只做**导入名解析、能力分类与方向判定**，不实现任何 syscall、不读写宿主资源。
//! 真正的 I/O 由宿主在能力授权后提供，底座不持有平台句柄。

use crate::error::WasmError;

/// WASI preview1 的导入模块名。
pub const WASI_PREVIEW1_MODULE: &str = "wasi_snapshot_preview1";

/// component-model 的版本后缀分隔符（`@0.2.0`）。
pub const COMPONENT_VERSION_SEPARATOR: char = '@';

/// WASI preview1 的导入函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WasiPreview1Import {
    /// `args_get`
    ArgsGet,
    /// `args_sizes_get`
    ArgsSizesGet,
    /// `environ_get`
    EnvironGet,
    /// `environ_sizes_get`
    EnvironSizesGet,
    /// `clock_res_get`
    ClockResGet,
    /// `clock_time_get`
    ClockTimeGet,
    /// `fd_write`
    FdWrite,
    /// `fd_read`
    FdRead,
    /// `fd_close`
    FdClose,
    /// `fd_seek`
    FdSeek,
    /// `fd_fdstat_get`
    FdFdstatGet,
    /// `fd_filestat_get`
    FdFilestatGet,
    /// `fd_prestat_get`
    FdPrestatGet,
    /// `fd_prestat_dir_name`
    FdPrestatDirName,
    /// `path_open`
    PathOpen,
    /// `poll_oneoff`
    PollOneoff,
    /// `random_get`
    RandomGet,
    /// `sched_yield`
    SchedYield,
    /// `proc_exit`
    ProcExit,
}

impl WasiPreview1Import {
    /// 规范中的导入名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArgsGet => "args_get",
            Self::ArgsSizesGet => "args_sizes_get",
            Self::EnvironGet => "environ_get",
            Self::EnvironSizesGet => "environ_sizes_get",
            Self::ClockResGet => "clock_res_get",
            Self::ClockTimeGet => "clock_time_get",
            Self::FdWrite => "fd_write",
            Self::FdRead => "fd_read",
            Self::FdClose => "fd_close",
            Self::FdSeek => "fd_seek",
            Self::FdFdstatGet => "fd_fdstat_get",
            Self::FdFilestatGet => "fd_filestat_get",
            Self::FdPrestatGet => "fd_prestat_get",
            Self::FdPrestatDirName => "fd_prestat_dir_name",
            Self::PathOpen => "path_open",
            Self::PollOneoff => "poll_oneoff",
            Self::RandomGet => "random_get",
            Self::SchedYield => "sched_yield",
            Self::ProcExit => "proc_exit",
        }
    }

    /// 该导入是否触碰文件系统命名空间（需要显式路径授权）。
    pub fn touches_filesystem(self) -> bool {
        matches!(
            self,
            Self::FdWrite
                | Self::FdRead
                | Self::FdClose
                | Self::FdSeek
                | Self::FdFdstatGet
                | Self::FdFilestatGet
                | Self::FdPrestatGet
                | Self::FdPrestatDirName
                | Self::PathOpen
        )
    }
}

/// 解析 WASI preview1 的导入名（`wasi_snapshot_preview1.<field>`）。
///
/// 非该模块的导入返回 `None`；模块名匹配但字段未知返回 `UnsupportedImport`。
pub fn wasi_preview1_import(name: &str) -> Result<Option<WasiPreview1Import>, WasmError> {
    let Some(field) = name.strip_prefix(WASI_PREVIEW1_MODULE).and_then(|rest| rest.strip_prefix('.')) else {
        return Ok(None);
    };
    let import = match field {
        "args_get" => WasiPreview1Import::ArgsGet,
        "args_sizes_get" => WasiPreview1Import::ArgsSizesGet,
        "environ_get" => WasiPreview1Import::EnvironGet,
        "environ_sizes_get" => WasiPreview1Import::EnvironSizesGet,
        "clock_res_get" => WasiPreview1Import::ClockResGet,
        "clock_time_get" => WasiPreview1Import::ClockTimeGet,
        "fd_write" => WasiPreview1Import::FdWrite,
        "fd_read" => WasiPreview1Import::FdRead,
        "fd_close" => WasiPreview1Import::FdClose,
        "fd_seek" => WasiPreview1Import::FdSeek,
        "fd_fdstat_get" => WasiPreview1Import::FdFdstatGet,
        "fd_filestat_get" => WasiPreview1Import::FdFilestatGet,
        "fd_prestat_get" => WasiPreview1Import::FdPrestatGet,
        "fd_prestat_dir_name" => WasiPreview1Import::FdPrestatDirName,
        "path_open" => WasiPreview1Import::PathOpen,
        "poll_oneoff" => WasiPreview1Import::PollOneoff,
        "random_get" => WasiPreview1Import::RandomGet,
        "sched_yield" => WasiPreview1Import::SchedYield,
        "proc_exit" => WasiPreview1Import::ProcExit,
        other => return Err(WasmError::UnsupportedImport { name: format!("{WASI_PREVIEW1_MODULE}.{other}") }),
    };
    Ok(Some(import))
}

/// 导入名是否属于 WASI preview1。
pub fn is_wasi_preview1_import(name: &str) -> bool {
    matches!(wasi_preview1_import(name), Ok(Some(_)))
}

/// component-model 的链接方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentDirection {
    /// 组件向宿主请求的导入。
    Import,
    /// 组件对外提供的导出。
    Export,
}

/// 判定一个 component-model 接口名（`namespace:package/interface@version`）的可用性并返回其所属包。
///
/// 只校验结构，不解析版本语义，也不建立实例。
pub fn component_direction(name: &str) -> Result<(ComponentDirection, &str), WasmError> {
    let (head, direction) = match name.strip_suffix("#import") {
        Some(head) => (head, ComponentDirection::Import),
        None => match name.strip_suffix("#export") {
            Some(head) => (head, ComponentDirection::Export),
            None => return Err(WasmError::UnsupportedImport { name: name.to_owned() }),
        },
    };
    let (package, interface) = head.split_once('/').ok_or_else(|| WasmError::UnsupportedImport { name: name.to_owned() })?;
    let package = package.split(COMPONENT_VERSION_SEPARATOR).next().unwrap_or(package);
    if package.split_once(':').is_none() || interface.is_empty() {
        return Err(WasmError::UnsupportedImport { name: name.to_owned() });
    }
    Ok((direction, package))
}
