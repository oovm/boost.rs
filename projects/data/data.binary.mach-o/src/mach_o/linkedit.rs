//! 加载命令中的 `linkedit_data_command` 载荷位置。

/// `LC_FUNCTION_STARTS`、`LC_DYLD_EXPORTS_TRIE`、`LC_DYLD_CHAINED_FIXUPS` 或
/// `LC_CODE_SIGNATURE` 指向的文件区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachLinkeditData {
    /// `dataoff`。
    pub data_offset: u32,
    /// `datasize`。
    pub data_size: u32,
}

impl MachLinkeditData {
    /// 是否没有载荷。
    pub fn is_empty(&self) -> bool {
        self.data_size == 0
    }
}
