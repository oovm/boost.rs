#![doc = include_str!("readme.md")]

pub use self::token::Token;
use std::io::{Read, Seek, SeekFrom};

mod token;

/// 小端二进制读取器。
///
/// 包装任意 [`Read`]（以及可选 [`Seek`]）的底层流，并在读取固定宽度数值时
/// 维护自身的字节位置。端序固定为小端，不引入通用端序框架；需要大端或
/// 其它布局的调用方按自身协议直接使用标准库的 `from_be_bytes` 等能力。
#[derive(Debug)]
pub struct BinaryReader<R> {
    reader: R,
    position: u64,
}

impl<R: Read> Read for BinaryReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl<R> BinaryReader<R> {
    /// 创建新的二进制读取器
    ///
    /// # Arguments
    /// * `reader` - 要读取的数据
    ///
    /// # Returns
    /// 返回新的 BinaryReader 实例
    pub fn new(reader: R) -> Self {
        Self { reader, position: 0 }
    }

    /// 获取当前读取位置
    ///
    /// # Returns
    /// 返回当前的字节偏移位置
    pub fn get_position(&self) -> u64 {
        self.position
    }

    /// 设置读取位置
    ///
    /// 注意：如果底层reader不支持Seek操作，此函数将返回错误
    ///
    /// # Arguments
    /// * `pos` - 新的读取位置
    ///
    /// # Returns
    /// 返回操作结果
    pub fn set_position(&mut self, position: u64) -> crate::Result<u64>
    where
        R: Seek,
    {
        self.reader.seek(SeekFrom::Start(position))?;
        self.position = position;
        Ok(position)
    }
    /// Returns the wrapped reader and drops position tracking state.
    pub fn finish(self) -> R {
        self.reader
    }
}

impl<R: Read> BinaryReader<R> {
    /// 读取 u8
    ///
    /// # Returns
    /// 返回读取的 u8 值或 IO 错误
    pub fn read_u8(&mut self) -> std::io::Result<u8> {
        let value = self.read_array::<1>()?[0];
        Ok(value)
    }

    /// 读取 u16
    ///
    /// # Returns
    /// 返回读取的 u16 值或 IO 错误
    pub fn read_u16(&mut self) -> std::io::Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    /// 读取 i16
    ///
    /// # Returns
    /// 返回读取的 i16 值或 IO 错误
    pub fn read_i16(&mut self) -> std::io::Result<i16> {
        Ok(i16::from_le_bytes(self.read_array::<2>()?))
    }

    /// 读取 u32
    ///
    /// # Returns
    /// 返回读取的 u32 值或 IO 错误
    pub fn read_u32(&mut self) -> std::io::Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    /// 读取 u64
    ///
    /// # Returns
    /// 返回读取的 u64 值或 IO 错误
    pub fn read_u64(&mut self) -> std::io::Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    /// 读取指定长度的字节数组
    ///
    /// # Arguments
    /// * `len` - 要读取的字节数
    ///
    /// # Returns
    /// 返回读取的字节数组或 IO 错误
    pub fn read_bytes(&mut self, len: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf)?;
        self.position += len as u64;
        Ok(buf)
    }

    /// 读取固定长度的字节数组
    ///
    /// # Returns
    /// 返回读取的字节数组或 IO 错误
    pub fn read_array<const N: usize>(&mut self) -> std::io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.reader.read_exact(&mut buf)?;
        self.position += N as u64;
        Ok(buf)
    }

    /// 跳过指定数量的字节
    ///
    /// 注意：如果底层reader不支持Seek操作，此函数将返回错误
    ///
    /// # Arguments
    /// * `count` - 要跳过的字节数
    ///
    /// # Returns
    /// 返回操作结果
    pub fn skip(&mut self, count: u64) -> std::io::Result<u64>
    where
        R: Seek,
    {
        let new_pos = self.reader.seek(SeekFrom::Current(count as i64))?;
        self.position = new_pos;
        Ok(new_pos)
    }
}

impl<R: Seek> BinaryReader<R> {
    /// Seeks the underlying reader and leaves [`BinaryReader::position`] unchanged.
    pub fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.reader.seek(pos)
    }
}
