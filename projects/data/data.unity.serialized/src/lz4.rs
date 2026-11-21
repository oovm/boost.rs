//! LZ4 block 解压。
//!
//! 只实现解压侧，按 LZ4 block 规范处理 token、字面量、匹配长度与重叠复制。
//! 输出长度必须与调用方给出的期望值完全一致。

use crate::UnityError;

const MIN_MATCH: usize = 4;

/// 解压一个 LZ4 block。
pub(crate) fn decompress_block(input: &[u8], expected: usize) -> Result<Vec<u8>, UnityError> {
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(64 * 1024 * 1024));
    let mut pos = 0usize;

    while pos < input.len() {
        let token = input[pos];
        pos += 1;

        let mut literal_len = (token >> 4) as usize;
        if literal_len == 15 {
            loop {
                let byte = read_len_byte(input, &mut pos)?;
                literal_len = literal_len.checked_add(byte as usize).ok_or_else(|| UnityError::Parse("lz4 literal length overflow".into()))?;
                if byte != 255 {
                    break;
                }
            }
        }

        let literal_end = pos
            .checked_add(literal_len)
            .filter(|&end| end <= input.len())
            .ok_or_else(|| UnityError::Parse("lz4 literal runs past input".into()))?;
        out.extend_from_slice(&input[pos..literal_end]);
        pos = literal_end;
        if out.len() > expected {
            return Err(UnityError::Parse("lz4 output exceeds expected size".into()));
        }

        // 最后一个序列只有字面量，输入到此结束。
        if pos == input.len() {
            break;
        }

        if pos + 2 > input.len() {
            return Err(UnityError::Parse("lz4 match offset is truncated".into()));
        }
        let offset = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
        pos += 2;
        if offset == 0 || offset > out.len() {
            return Err(UnityError::Parse(format!("invalid lz4 match offset {offset}")));
        }

        let mut match_len = (token & 0x0F) as usize;
        if match_len == 15 {
            loop {
                let byte = read_len_byte(input, &mut pos)?;
                match_len = match_len.checked_add(byte as usize).ok_or_else(|| UnityError::Parse("lz4 match length overflow".into()))?;
                if byte != 255 {
                    break;
                }
            }
        }
        match_len += MIN_MATCH;

        if out.len().saturating_add(match_len) > expected {
            return Err(UnityError::Parse("lz4 output exceeds expected size".into()));
        }
        let start = out.len() - offset;
        for index in 0..match_len {
            let byte = out[start + index];
            out.push(byte);
        }
    }

    if out.len() != expected {
        return Err(UnityError::Parse(format!("lz4 output length {} does not match expected {expected}", out.len())));
    }
    Ok(out)
}

fn read_len_byte(input: &[u8], pos: &mut usize) -> Result<u8, UnityError> {
    let byte = *input.get(*pos).ok_or_else(|| UnityError::Parse("lz4 length is truncated".into()))?;
    *pos += 1;
    Ok(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompresses_literal_only_block() {
        // token: 5 个字面量，0 个匹配；随后 5 字节，输入结束。
        let mut block = vec![0x50];
        block.extend_from_slice(b"hello");
        assert_eq!(decompress_block(&block, 5).unwrap(), b"hello");
    }

    #[test]
    fn decompresses_long_literal_block() {
        let payload = b"0123456789abcde";
        let mut block = vec![0xF0, 0x00];
        block.extend_from_slice(payload);
        assert_eq!(decompress_block(&block, payload.len()).unwrap(), payload);
    }

    #[test]
    fn decompresses_match_and_overlap() {
        // 1 个字面量 'a'，匹配长度 2 + 4 = 6，偏移 1 => "a" + "aaaaaa"。
        let block = [0x12, b'a', 0x01, 0x00];
        assert_eq!(decompress_block(&block, 7).unwrap(), b"aaaaaaa");
    }

    #[test]
    fn rejects_bad_blocks() {
        assert!(decompress_block(&[0x50, b'h'], 5).is_err());
        assert!(decompress_block(&[0x10, b'a', 0x00, 0x00], 7).is_err());
        assert!(decompress_block(&[0x50], 5).is_err());
        assert!(decompress_block(&[0x10, b'a', 0x05, 0x00], 9).is_err());
    }
}
