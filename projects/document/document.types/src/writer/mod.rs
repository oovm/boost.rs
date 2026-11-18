#![doc = include_str!("readme.md")]

use std::io::{Seek, SeekFrom, Write};

/// 小端二进制写入器。
///
/// 包装任意 [`Write`] 的底层流，固定使用小端布局写入固定宽度数值；需要
/// 其它端序的调用方按自身协议直接使用标准库的 `to_be_bytes` 等能力。
#[derive(Copy, Clone, Debug)]
pub struct BinaryWriter<W> {
    writer: W,
}

impl<W: Write> Write for BinaryWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Seek> Seek for BinaryWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.writer.seek(pos)
    }
}

impl<W> BinaryWriter<W> {
    /// 创建一个新的二进制写入器
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// 获取内部写入器
    pub fn finish(self) -> W {
        self.writer
    }
}

impl<W: Write> BinaryWriter<W> {
    /// Writes one unsigned 8-bit integer.
    pub fn write_u8(&mut self, value: u8) -> std::io::Result<()> {
        self.writer.write_all(&[value])
    }

    /// Writes one unsigned 16-bit integer in little endian.
    pub fn write_u16(&mut self, value: u16) -> std::io::Result<()> {
        self.writer.write_all(&value.to_le_bytes())
    }

    /// Writes one unsigned 32-bit integer in little endian.
    pub fn write_u32(&mut self, value: u32) -> std::io::Result<()> {
        self.writer.write_all(&value.to_le_bytes())
    }

    /// Writes one signed 32-bit integer in little endian.
    pub fn write_i32(&mut self, value: i32) -> std::io::Result<()> {
        self.writer.write_all(&value.to_le_bytes())
    }

    /// Writes one unsigned 64-bit integer in little endian.
    pub fn write_u64(&mut self, value: u64) -> std::io::Result<()> {
        self.writer.write_all(&value.to_le_bytes())
    }

    /// Writes a raw byte slice without endian conversion.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }
}
