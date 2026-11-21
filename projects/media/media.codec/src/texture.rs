//! 块压缩纹理解码。
//!
//! 只提供把 BC1/BC7 块解码为 RGBA8 的入口；文件容器、mipmap 选择和资源命名
//! 由调用方决定。PNG 输出复用 [`crate::write_png`]。

/// 把 BC1（DXT1）块解码为 RGBA8。
///
/// `input` 必须至少包含 `width * height / 16` 个 8 字节块；不足时返回空缓冲。
pub fn decode_bc1(input: &[u8], width: u32, height: u32) -> Vec<u8> {
    decode(input, width, height, ctt_etcpak::decode::decode_bc1_into)
}

/// 把 BC7 块解码为 RGBA8。
///
/// `input` 必须至少包含 `width * height / 16` 个 16 字节块；不足时返回空缓冲。
pub fn decode_bc7(input: &[u8], width: u32, height: u32) -> Vec<u8> {
    decode(input, width, height, ctt_etcpak::decode::decode_bc7_into)
}

fn decode(
    input: &[u8],
    width: u32,
    height: u32,
    decode_into: fn(&[u8], u32, u32, &mut [u8]),
) -> Vec<u8> {
    let Some(length) = width.checked_mul(height).and_then(|pixels| pixels.checked_mul(4))
    else {
        return Vec::new();
    };
    let mut rgba = vec![0u8; length as usize];
    decode_into(input, width, height, &mut rgba);
    rgba
}

#[cfg(test)]
mod tests {
    use super::decode_bc1;

    #[test]
    fn bc1_constant_white_block_decodes_to_opaque_rgba() {
        // color0 = 0xffff (white), color1 = 0x0000, all indices select color0.
        let mut block = vec![0xff, 0xff, 0x00, 0x00];
        block.extend_from_slice(&[0u8; 4]);
        let rgba = decode_bc1(&block, 4, 4);
        assert_eq!(rgba.len(), 64);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
    }
}
