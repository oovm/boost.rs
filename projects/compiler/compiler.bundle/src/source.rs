//! 只读源码端口：打包算法按已解析身份取模块源码、JSON 文本与资源字节，
//! 不直接触碰文件系统。
//!
//! 图、链接、分包与发射只消费「锁定后的输入快照」：身份由 `resolve`
//! 给出，字节由调用方注入的 [`SourceReader`] 提供。宿主可以是文件系统
//! （[`StdSourceReader`]）、内存快照、归档或远程缓存，算法本身不感知。
//!
//! 路径的 canonicalize/存在性判定仍属身份解析（`resolve` / `graph`），
//! 不由本端口承担。

use std::path::Path;

/// 源码与资源字节的只读端口。
pub trait SourceReader {
    /// 读取整段字节。
    ///
    /// `NotFound` 与其它 IO 错误由调用方按各自语义处理（入口缺失是图级
    /// 诊断，依赖读取失败是 `read_failed`）。
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// 读取 UTF-8 文本；非 UTF-8 按 `InvalidData` 报错，不做有损替换。
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }
}

/// 直接以本地文件系统为后端的端口实现。
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSourceReader;

impl SourceReader for StdSourceReader {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}
