//! `LC_ENCRYPTION_INFO` / `LC_ENCRYPTION_INFO_64` 的加密信息。

/// 一条加密信息加载命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachEncryptionInfo {
    /// `cryptoff`。
    pub crypt_offset: u32,
    /// `cryptsize`。
    pub crypt_size: u32,
    /// `cryptid`；`0` 表示未加密。
    pub crypt_id: u32,
    /// 是否来自 `LC_ENCRYPTION_INFO_64`。
    pub is_64: bool,
}

impl MachEncryptionInfo {
    /// 镜像是否加密。
    pub fn encrypted(&self) -> bool {
        self.crypt_id != 0
    }
}
