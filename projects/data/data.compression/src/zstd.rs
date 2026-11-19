//! Zstandard 帧（MongoDB `OP_COMPRESSED` compressor id 3）。
//!
//! 解码委托给纯 Rust 实现的 `ruzstd`（无 native 链接，符合本仓纯 Rust 依赖约束）。
//! 帧封套声明的 `uncompressed_size` 必须与 `ruzstd` 实际产出一致，否则拒绝；
//! zstd 流自带的 content_size 是可选字段，不能替代封套校验。
//! 本层不提供编码：`ruzstd` 0.9 的压缩路径仍是 `todo!()`，服务端回写压缩帧
//! 由宿主原语（如 `node:zlib` 的 zstd）生成，解码期望值同样来自独立实现。
//! 测试中的压缩帧由 Node `node:zlib`（libzstd C 实现）独立生成并交叉核对，
//! 见同目录 `zstd_vectors.rs`。

use crate::CompressionError;
use ruzstd::decoding::StreamingDecoder;
use std::io::Read;

/// 解码 zstd 流；`announced` 来自帧封套，必须与实际产出一致。
pub fn decode(input: &[u8], announced: usize) -> Result<Vec<u8>, CompressionError> {
    // `StreamingDecoder::new` 解析帧头，`read_to_end` 逐块解压；二者的错误都映射为
    // `Invalid`（`ruzstd` 不区分截断与非法，封套层已先校验帧长）。
    let mut decoder = StreamingDecoder::new(input).map_err(|_| CompressionError::Invalid)?;
    // 初始容量封顶，避免封套谎报的巨大 `announced` 直接预分配过多内存。
    let mut output = Vec::with_capacity(announced.min(1 << 20));
    decoder.read_to_end(&mut output).map_err(|_| CompressionError::Invalid)?;
    if output.len() != announced {
        return Err(CompressionError::LengthMismatch { announced, actual: output.len() });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("zstd_vectors.rs");

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex")).collect()
    }

    #[test]
    fn decodes_frames_produced_by_independent_libzstd() {
        for (name, original_hex, compressed_hex) in KNOWN {
            let original = hex_bytes(original_hex);
            let compressed = hex_bytes(compressed_hex);
            let decoded = decode(&compressed, original.len()).unwrap_or_else(|err| panic!("decode {name}: {err:?}"));
            assert_eq!(decoded, original, "mismatch for {name}");
        }
    }

    #[test]
    fn decodes_large_zero_filled_frames() {
        for (len, compressed_hex) in ZEROES {
            let original = vec![0u8; *len];
            let compressed = hex_bytes(compressed_hex);
            let decoded = decode(&compressed, original.len()).expect("decode");
            assert_eq!(decoded.len(), original.len());
            assert!(decoded.iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn rejects_wrong_announced_length() {
        let (_, original_hex, compressed_hex) = &KNOWN[1]; // "hello"
        let original = hex_bytes(original_hex);
        let compressed = hex_bytes(compressed_hex);
        let wrong = original.len() + 1;
        assert!(matches!(
            decode(&compressed, wrong),
            Err(CompressionError::LengthMismatch { announced, actual }) if announced == wrong && actual == original.len()
        ));
    }

    #[test]
    fn rejects_truncated_or_malformed_stream() {
        // 魔数截断。
        assert_eq!(decode(&[0x28, 0xb5], 0), Err(CompressionError::Invalid));
        // 非 zstd 魔数。
        assert_eq!(decode(&[0x00, 0x00, 0x00, 0x00], 0), Err(CompressionError::Invalid));
    }
}
