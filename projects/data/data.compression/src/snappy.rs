//! Snappy block 格式（MongoDB `OP_COMPRESSED` compressor id 1）。
//!
//! 解码器支持全部 tag（literal 与 1/2/4 字节 offset copy），并强制校验流自报长度、
//! 帧声明长度与实际产出三者一致；编码器产出合法的 literal-only 流（与既有
//! 驱动可解码的形态一致，比率 1.0），避免复刻 libsnappy 的匹配启发式。
//! 期望值来源：`mongod 8.0.9` 真实应答捕获、仓内 TS 实现（经 mongod 验证）与
//! 官方驱动 e2e 交叉核对，不自产期望值。

use crate::CompressionError;

/// 单个 literal 元素的最大字节数（与既有实现同口径，16 MiB）。
const MAX_LITERAL: usize = 0x1000_000;

/// 编码为 literal-only 的 Snappy 流：varint 长度前缀 + literal 元素序列。
pub fn encode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + 8);
    write_varint(&mut out, input.len() as u64);
    let mut offset = 0;
    while offset < input.len() {
        let length = (input.len() - offset).min(MAX_LITERAL);
        write_literal_header(&mut out, length);
        out.extend_from_slice(&input[offset..offset + length]);
        offset += length;
    }
    out
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_literal_header(out: &mut Vec<u8>, length: usize) {
    let encoded = (length - 1) as u64;
    if encoded < 60 {
        out.push((encoded as u8) << 2);
        return;
    }
    let extra_bytes = if encoded <= 0xff {
        1
    }
    else if encoded <= 0xffff {
        2
    }
    else if encoded <= 0xff_ffff {
        3
    }
    else {
        4
    };
    out.push(((59 + extra_bytes) as u8) << 2);
    for index in 0..extra_bytes {
        out.push(((encoded >> (8 * index)) & 0xff) as u8);
    }
}

/// 解码 Snappy 流；`announced` 来自帧封套，必须与流自报长度和实际产出一致。
pub fn decode(input: &[u8], announced: usize) -> Result<Vec<u8>, CompressionError> {
    let mut cursor = 0;
    let mut stream_length: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *input.get(cursor).ok_or(CompressionError::Truncated)?;
        cursor += 1;
        stream_length |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(CompressionError::Invalid);
        }
    }
    if stream_length != announced as u64 {
        return Err(CompressionError::LengthMismatch { announced, actual: stream_length as usize });
    }

    let mut output = Vec::with_capacity(announced);
    while cursor < input.len() {
        let tag = input[cursor];
        cursor += 1;
        match tag & 0x03 {
            0 => {
                let mut length = (tag >> 2) as usize + 1;
                if length > 60 {
                    let extra = length - 60;
                    let bytes = input.get(cursor..cursor + extra).ok_or(CompressionError::Truncated)?;
                    cursor += extra;
                    let mut value: u64 = 0;
                    for (index, &byte) in bytes.iter().enumerate() {
                        value |= (byte as u64) << (8 * index);
                    }
                    length = value.checked_add(1).ok_or(CompressionError::Invalid)? as usize;
                }
                let data = input.get(cursor..cursor + length).ok_or(CompressionError::Truncated)?;
                cursor += length;
                if output.len() + length > announced {
                    return Err(CompressionError::LengthMismatch { announced, actual: output.len() + length });
                }
                output.extend_from_slice(data);
            }
            1 | 2 | 3 => {
                let (length, offset) = if tag & 0x03 == 1 {
                    let bytes = input.get(cursor..cursor + 1).ok_or(CompressionError::Truncated)?;
                    cursor += 1;
                    ((((tag >> 2) & 0x07) as usize) + 4, (((tag >> 5) as usize) << 8) | bytes[0] as usize)
                }
                else if tag & 0x03 == 2 {
                    let bytes = input.get(cursor..cursor + 2).ok_or(CompressionError::Truncated)?;
                    cursor += 2;
                    (((tag >> 2) as usize) + 1, u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
                }
                else {
                    let bytes = input.get(cursor..cursor + 4).ok_or(CompressionError::Truncated)?;
                    cursor += 4;
                    (((tag >> 2) as usize) + 1, u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
                };
                if offset == 0 || offset > output.len() {
                    return Err(CompressionError::Invalid);
                }
                if output.len() + length > announced {
                    return Err(CompressionError::LengthMismatch { announced, actual: output.len() + length });
                }
                for _ in 0..length {
                    let source = output.len() - offset;
                    let byte = output[source];
                    output.push(byte);
                }
            }
            _ => unreachable!("two-bit tag always matches one of the arms above"),
        }
    }

    if output.len() != announced {
        return Err(CompressionError::LengthMismatch { announced, actual: output.len() });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex")).collect()
    }

    #[test]
    fn decodes_a_real_mongod_reply() {
        // `mongod 8.0.9` 对 `ping` 的 snappy 压缩应答（仓内既有测试捕获的真实流）。
        let stream = hex_bytes("16000001014011000000016f6b00000000000000f03f00");
        let body = hex_bytes("000000000011000000016f6b00000000000000f03f00");
        assert_eq!(decode(&stream, body.len()).unwrap(), body);
        assert!(matches!(decode(&stream, 21), Err(CompressionError::LengthMismatch { announced: 21, .. })));
    }

    #[test]
    fn round_trips_literal_only_streams() {
        for payload in [
            vec![0u8; 0],
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            vec![7u8; 60],
            vec![7u8; 61],
            vec![9u8; 200],
            vec![5u8; 0x1000_00 + 9],
            (0..1000u32).map(|value| value as u8).collect(),
        ] {
            let encoded = encode(&payload);
            assert_eq!(decode(&encoded, payload.len()).unwrap(), payload, "round trip at len {}", payload.len());
        }
    }

    #[test]
    fn decodes_streams_produced_by_independent_encoders() {
        // 前五条流由独立 JS 编码器构造（含 1/2 字节 offset copy 与 60/61 长度头）。
        let vectors: &[(&str, &str)] = &[
            (
                "327874686520717569636b2062726f776e20666f78206a756d7073206f766572204a1f00",
                "74686520717569636b2062726f776e20666f78206a756d7073206f7665722074686520717569636b2062726f776e20666f78",
            ),
            ("340061ca0100", "61616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161"),
            (
                "460078fe01000540",
                "78787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
            ),
            ("1d146d6f6e676f205a0600", "6d6f6e676f206d6f6e676f206d6f6e676f206d6f6e676f206d6f6e676f"),
            ("180c0102030411040c05060708010c0108", "010203040102030401020304050607080102030405060708"),
        ];
        for (stream, expected) in vectors {
            let expected = hex_bytes(expected);
            let decoded = decode(&hex_bytes(stream), expected.len()).expect("decode");
            assert_eq!(decoded, expected);
        }

        // 以下两条按 Snappy 格式规范手工构造，与本解码器无共同来源：
        // 第一条覆盖 copy1 长度 11 与 copy2 长度 64 的链式重叠拷贝（250 个 0x09）；
        // 第二条覆盖 copy4 四字节 offset。
        let stream = hex_bytes("fa0100091d01fe0100fe0100fe0100b60100");
        assert_eq!(decode(&stream, 250).unwrap(), vec![0x09_u8; 250]);
        let stream = hex_bytes("0a1001020304051305000000");
        assert_eq!(decode(&stream, 10).unwrap(), hex_bytes("01020304050102030405"));
    }

    #[test]
    fn rejects_malformed_streams() {
        // varint 截断。
        assert_eq!(decode(&[0x80], 1), Err(CompressionError::Truncated));
        // copy 偏移为 0，越出已解码前缀。
        assert_eq!(decode(&[0x01, 0x00, b'a', 0x01, 0x00], 1), Err(CompressionError::Invalid));
        // literal 越出流尾。
        assert_eq!(decode(&[0x04, 0x0c, b'a'], 4), Err(CompressionError::Truncated));
        // 产出越出声明长度。
        assert_eq!(
            decode(&[0x04, 0x08, b'a', b'b', b'c', 0x08, b'd', b'e', b'f'], 4),
            Err(CompressionError::LengthMismatch { announced: 4, actual: 6 })
        );
    }
}
