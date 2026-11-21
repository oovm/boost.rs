//! 有界字节读取器。
//!
//! 所有读取都检查剩余长度，越界返回 [`UnityError::Parse`]；多字节字段按当前
//! 端序解释，端序由文件头决定。

use crate::UnityError;

/// 基于切片的位置指针，支持按端序读取。
#[derive(Debug, Clone)]
pub(crate) struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
    little: bool,
}

impl<'a> ByteReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, little: true }
    }

    pub(crate) fn set_little(&mut self, little: bool) {
        self.little = little;
    }

    pub(crate) fn little(&self) -> bool {
        self.little
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8, UnityError> {
        let byte = *self.data.get(self.pos).ok_or_else(|| UnityError::Parse("unexpected end of serialized file".into()))?;
        self.pos += 1;
        Ok(byte)
    }

    pub(crate) fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], UnityError> {
        let end = self.pos.checked_add(len).ok_or_else(|| UnityError::Parse("byte range overflow".into()))?;
        let slice = self.data.get(self.pos..end).ok_or_else(|| UnityError::Parse("unexpected end of serialized file".into()))?;
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn read_u16(&mut self) -> Result<u16, UnityError> {
        let bytes: [u8; 2] = self.read_bytes(2)?.try_into().expect("length checked");
        Ok(if self.little { u16::from_le_bytes(bytes) } else { u16::from_be_bytes(bytes) })
    }

    pub(crate) fn read_i16(&mut self) -> Result<i16, UnityError> {
        Ok(self.read_u16()? as i16)
    }

    pub(crate) fn read_u32(&mut self) -> Result<u32, UnityError> {
        let bytes: [u8; 4] = self.read_bytes(4)?.try_into().expect("length checked");
        Ok(if self.little { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) })
    }

    pub(crate) fn read_i32(&mut self) -> Result<i32, UnityError> {
        Ok(self.read_u32()? as i32)
    }

    pub(crate) fn read_u64(&mut self) -> Result<u64, UnityError> {
        let bytes: [u8; 8] = self.read_bytes(8)?.try_into().expect("length checked");
        Ok(if self.little { u64::from_le_bytes(bytes) } else { u64::from_be_bytes(bytes) })
    }

    pub(crate) fn read_i64(&mut self) -> Result<i64, UnityError> {
        Ok(self.read_u64()? as i64)
    }

    pub(crate) fn read_f32(&mut self) -> Result<f32, UnityError> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    pub(crate) fn read_f64(&mut self) -> Result<f64, UnityError> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    pub(crate) fn read_u32_be(&mut self) -> Result<u32, UnityError> {
        let bytes: [u8; 4] = self.read_bytes(4)?.try_into().expect("length checked");
        Ok(u32::from_be_bytes(bytes))
    }

    pub(crate) fn read_i64_be(&mut self) -> Result<i64, UnityError> {
        let bytes: [u8; 8] = self.read_bytes(8)?.try_into().expect("length checked");
        Ok(i64::from_be_bytes(bytes))
    }

    /// 读取以 NUL 结束的 UTF-8 字符串。
    pub(crate) fn read_cstr(&mut self) -> Result<String, UnityError> {
        let start = self.pos;
        let tail = self.data.get(start..).ok_or_else(|| UnityError::Parse("unexpected end of serialized file".into()))?;
        let len = tail.iter().position(|&byte| byte == 0).ok_or_else(|| UnityError::Parse("unterminated string in serialized file".into()))?;
        self.pos = start + len + 1;
        String::from_utf8(tail[..len].to_vec()).map_err(|error| UnityError::Parse(format!("invalid utf8 string: {error}")))
    }

    /// 前进到 `align` 的整数倍位置。
    pub(crate) fn align(&mut self, align: usize) -> Result<(), UnityError> {
        if align == 0 || !align.is_power_of_two() {
            return Err(UnityError::Parse(format!("invalid alignment {align}")));
        }
        let remainder = self.pos % align;
        if remainder != 0 {
            let new_pos = self.pos.checked_add(align - remainder).ok_or_else(|| UnityError::Parse("alignment overflow".into()))?;
            if new_pos > self.data.len() {
                return Err(UnityError::Parse("alignment runs past end of serialized file".into()));
            }
            self.pos = new_pos;
        }
        Ok(())
    }

    pub(crate) fn seek(&mut self, pos: usize) -> Result<(), UnityError> {
        if pos > self.data.len() {
            return Err(UnityError::Parse("seek past end of serialized file".into()));
        }
        self.pos = pos;
        Ok(())
    }
}

/// 读取一个非负计数，并用最小条目大小与剩余字节做上界检查。
pub(crate) fn read_count(reader: &mut ByteReader<'_>, min_item_bytes: usize, what: &str) -> Result<usize, UnityError> {
    const MAX_ITEMS: usize = 16_000_000;

    let raw = reader.read_i32()?;
    if raw < 0 {
        return Err(UnityError::Parse(format!("negative {what}: {raw}")));
    }
    let count = raw as usize;
    if count > MAX_ITEMS {
        return Err(UnityError::Parse(format!("unreasonable {what}: {count}")));
    }
    if min_item_bytes > 0 && count.saturating_mul(min_item_bytes) > reader.remaining() {
        return Err(UnityError::Parse(format!("{what} {count} exceeds remaining bytes {}", reader.remaining())));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_endiannesses() {
        let mut reader = ByteReader::new(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(reader.read_u32().unwrap(), 0x0403_0201);

        let mut reader = ByteReader::new(&[0x01, 0x02, 0x03, 0x04]);
        reader.set_little(false);
        assert_eq!(reader.read_u32().unwrap(), 0x0102_0304);
    }

    #[test]
    fn rejects_truncated_and_negative_counts() {
        let mut reader = ByteReader::new(&[0x01]);
        assert!(reader.read_u32().is_err());

        let mut reader = ByteReader::new(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(read_count(&mut reader, 4, "items").is_err());
    }
}
